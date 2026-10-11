//! The Claude capital allocator, run through one persistent omp session (replaces
//! `tools/claude_allocator.py` and `CAPITAL_AI_ADVISOR_CMD`). Claude reads the allocation context,
//! may pull fleet status / balances / scorecards through host tools, and delivers its proposal
//! with `propose_allocation`, which validates it against the refactor JSON schema. The proposal is
//! only advice: [`crate::advisor::guard_proposal`] clamps it into the deterministic bounds.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bb_binance::BinanceClient;
use bb_omp::{
    AgentSession, CancellationToken, HostTool, HostToolHandler, HostToolOutcome, OmpError,
    PromptStatus, SessionConfig, Transport, TurnOutcome,
};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};

use crate::advisor::{
    AllocationContext, FileProposalProvider, FirstAvailable, GuardedAdvisor, ProposalProvider,
    allocation_schema, extract_allocation_proposal, guard_proposal, validate_proposal,
};
use crate::config::{AdvisorConfig, ClaudeAllocatorConfig};
use crate::error::Result;
use crate::status::{collect_snapshot, render_json};
use crate::wallet::fetch_wallet;

/// Standing instructions (the refactor `SYSTEM_PROMPT`, adapted to tools and memory).
pub const SYSTEM_PROMPT: &str = "You are a capital allocation advisor for a live spot trading bot fleet.\n\
You do not submit exchange orders and you do not choose entries or exits.\n\
Your only job is to propose module-level capital limits.\n\
You remember earlier allocation rounds through this conversation; keep your reasoning consistent with them \
unless the evidence changed.\n\
Deliver every proposal by calling propose_allocation exactly once with a JSON object that matches its schema. \
If it returns an error, fix the proposal and call it again. Never include markdown in the proposal.";

/// Compaction instructions for the persistent context.
pub const COMPACT_INSTRUCTIONS: &str = "You are compacting the context of a capital allocation advisor. Preserve: \
the latest accepted allocation per module with its reason, modules you paused or reduced and why, trends in \
module scorecards across rounds. Drop raw tool payloads and superseded proposals.";

/// Per-turn allocation rules (the refactor `build_prompt` rules).
pub const RULES: &str = "Rules:\n\
- Only include modules listed in module_ids.\n\
- Keep max_usdt at or below the fallback allocation unless the scorecard has enough evidence to justify growth.\n\
- If trades are below 5, prefer the fallback allocation or a reduced allocation.\n\
- If realized PnL is negative, win rate is weak, drawdown is high, or risk breaches exist, reduce or pause that module.\n\
- If there is no scorecard for a module, do not invent performance evidence.\n\
- The proposal is not an order. It only controls maximum budget per module.\n\
- Use short machine-readable reasons such as ai_no_evidence, ai_reduce_drawdown, ai_pause_losses, \
ai_keep_fallback, or ai_scale_winner.\n\
- The bot clamps every proposal to the configured per-module cap and the allocatable pool; modules you omit \
receive no budget.";

fn object(properties: Value) -> Value {
    json!({"type": "object", "properties": properties, "required": [], "additionalProperties": false})
}

/// Host tools registered with omp.
pub fn tool_definitions() -> Vec<HostTool> {
    vec![
        HostTool::new(
            "get_fleet_status",
            "Fleet status",
            "Every bot of the fleet: activity (last write, staleness), state summary, last audit row, lifetime and 24h trades and realised P&L.",
            object(json!({})),
        ),
        HostTool::new(
            "get_scorecards",
            "Scorecards",
            "This round's allocation context: config (reserve, min/max per module), scoring policy, total equity, module_ids, per-module scorecards and the deterministic fallback plan.",
            object(json!({})),
        ),
        HostTool::new(
            "get_balances",
            "Balances",
            "Live Binance wallet: spot USDT free, USD-M futures USDT free, valued spot holdings and total USDT value.",
            object(json!({})),
        ),
        HostTool::new(
            "propose_allocation",
            "Propose allocation",
            "Submit this round's proposal: {\"allocations\": [{module_id, status: active|reduced|paused, max_usdt >= 0, reason}]}. Validated against the schema and module_ids; returns the guarded plan the bot will apply. Call exactly once per round (again only after an error).",
            allocation_schema(),
        ),
    ]
}

/// Read-only data behind `get_fleet_status` and `get_balances`.
#[async_trait]
pub trait FleetSource: Send + Sync {
    async fn fleet_status(&self) -> std::result::Result<Value, String>;
    async fn balances(&self) -> std::result::Result<Value, String>;
}

/// Files under the data dir plus the Binance wallet (when credentials exist).
pub struct LiveFleet {
    pub client: Option<BinanceClient>,
    pub data_dir: PathBuf,
}

#[async_trait]
impl FleetSource for LiveFleet {
    async fn fleet_status(&self) -> std::result::Result<Value, String> {
        let snapshot = collect_snapshot(self.client.as_ref(), &self.data_dir).await;
        Ok(render_json(&snapshot)["bots"].clone())
    }

    async fn balances(&self) -> std::result::Result<Value, String> {
        let client = self
            .client
            .as_ref()
            .ok_or("no Binance credentials configured")?;
        serde_json::to_value(fetch_wallet(client).await).map_err(|e| e.to_string())
    }
}

#[derive(Default)]
struct TurnState {
    context: Option<AllocationContext>,
    proposal: Option<Value>,
}

/// The allocator's host tools.
pub struct AllocatorTools {
    fleet: Arc<dyn FleetSource>,
    turn: parking_lot::Mutex<TurnState>,
}

impl AllocatorTools {
    pub fn new(fleet: Arc<dyn FleetSource>) -> Self {
        Self {
            fleet,
            turn: parking_lot::Mutex::new(TurnState::default()),
        }
    }

    async fn dispatch(&self, name: &str, arguments: Value) -> std::result::Result<Value, String> {
        match name {
            "get_fleet_status" => self.fleet.fleet_status().await,
            "get_balances" => self.fleet.balances().await,
            "get_scorecards" => self
                .turn
                .lock()
                .context
                .as_ref()
                .map(AllocationContext::to_json)
                .ok_or_else(|| "no allocation round is active".to_owned()),
            "propose_allocation" => {
                let mut turn = self.turn.lock();
                let context = turn
                    .context
                    .as_ref()
                    .ok_or("no allocation round is active")?;
                validate_proposal(&arguments, &context.module_ids)?;
                let guarded =
                    guard_proposal(Some(&arguments), &context.fallback_plan, &context.config);
                turn.proposal = Some(arguments);
                Ok(json!({"accepted": true, "guarded_plan": guarded}))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}

#[async_trait]
impl HostToolHandler for AllocatorTools {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> HostToolOutcome {
        match self.dispatch(name, arguments).await {
            Ok(value) => HostToolOutcome::text(value.to_string()),
            Err(message) => HostToolOutcome::error(message),
        }
    }
}

/// One tool call of a round.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolTrace {
    pub name: String,
    pub args: Value,
    pub is_error: bool,
    pub ms: u64,
    pub result: String,
}

/// What one advisory round produced.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdviceReport {
    /// `completed`, `aborted`, `error`, `timeout`, `crashed` or `unavailable`.
    pub status: String,
    pub error: Option<String>,
    pub session_id: String,
    pub resumed: bool,
    pub tool_calls: Vec<ToolTrace>,
    pub final_text: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub cost_usd: Decimal,
    /// Schema-valid proposal (from `propose_allocation`, else recovered from the final text).
    pub proposal: Option<Value>,
}

impl AdviceReport {
    fn failed(status: &str, error: String) -> Self {
        Self {
            status: status.into(),
            error: Some(error),
            session_id: String::new(),
            resumed: false,
            tool_calls: Vec::new(),
            final_text: None,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: Decimal::ZERO,
            proposal: None,
        }
    }

    fn from_outcome(status: &str, outcome: &TurnOutcome, error: Option<String>) -> Self {
        Self {
            status: status.into(),
            error: error.or_else(|| outcome.error.as_ref().map(|e| e.message.clone())),
            session_id: String::new(),
            resumed: false,
            tool_calls: outcome
                .tool_calls
                .iter()
                .map(|call| ToolTrace {
                    name: call.name.clone(),
                    args: call.arguments.clone(),
                    is_error: call.is_error,
                    ms: u64::try_from(call.duration.as_millis()).unwrap_or(u64::MAX),
                    result: call.result_text.chars().take(400).collect(),
                })
                .collect(),
            final_text: outcome.final_text.clone(),
            input_tokens: outcome.usage.input_tokens,
            output_tokens: outcome.usage.output_tokens,
            cost_usd: outcome.usage.cost_usd,
            proposal: None,
        }
    }
}

struct Live {
    session: AgentSession,
    session_id: String,
    resumed: bool,
}

/// The persistent omp allocator session (started lazily, resumed from `CLAUDE_ALLOCATOR_SESSION_DIR`).
pub struct ClaudeAllocator {
    config: ClaudeAllocatorConfig,
    tools: Arc<AllocatorTools>,
    live: tokio::sync::Mutex<Option<Live>>,
}

impl ClaudeAllocator {
    pub fn new(config: ClaudeAllocatorConfig, fleet: Arc<dyn FleetSource>) -> Self {
        Self {
            config,
            tools: Arc::new(AllocatorTools::new(fleet)),
            live: tokio::sync::Mutex::new(None),
        }
    }

    /// `omp --mode rpc --no-tools ... --model --system-prompt --session-dir`.
    pub fn session_config(config: &ClaudeAllocatorConfig) -> SessionConfig {
        SessionConfig {
            program: config.command.clone().into(),
            model: Some(config.model.clone()),
            system_prompt: Some(SYSTEM_PROMPT.into()),
            session_dir: Some(config.session_dir.clone()),
            isolated: true,
            ..SessionConfig::default()
        }
    }

    /// Attach over an existing transport (tests, custom launchers).
    pub async fn connect(
        config: ClaudeAllocatorConfig,
        fleet: Arc<dyn FleetSource>,
        transport: Transport,
    ) -> Result<Self> {
        let allocator = Self::new(config, fleet);
        let session =
            AgentSession::connect(transport, Self::session_config(&allocator.config)).await?;
        let live = allocator.attach(session).await?;
        *allocator.live.lock().await = Some(live);
        Ok(allocator)
    }

    async fn attach(&self, mut session: AgentSession) -> Result<Live> {
        session
            .set_host_tools(tool_definitions(), self.tools.clone())
            .await?;
        let opened = session.open_session(&self.config.session_dir).await?;
        if opened.cancelled {
            return Err(crate::error::CapitalError::invalid(
                "omp cancelled open_session",
            ));
        }
        tracing::info!(
            session_id = ?opened.session_id,
            resumed = opened.resumed,
            dir = %self.config.session_dir.display(),
            "omp allocator session ready"
        );
        Ok(Live {
            session,
            session_id: opened.session_id.unwrap_or_default(),
            resumed: opened.resumed,
        })
    }

    async fn start(&self) -> Result<Live> {
        std::fs::create_dir_all(&self.config.session_dir)?;
        let session = AgentSession::start(Self::session_config(&self.config)).await?;
        self.attach(session).await
    }

    /// One advisory round; never fails (errors are reported in the result).
    pub async fn advise(&self, context: &AllocationContext) -> AdviceReport {
        let mut guard = self.live.lock().await;
        if guard.is_none() {
            match self.start().await {
                Ok(live) => *guard = Some(live),
                Err(error) => {
                    return AdviceReport::failed(
                        "unavailable",
                        format!("omp allocator unavailable: {error}"),
                    );
                }
            }
        }
        let Some(live) = guard.as_mut() else {
            return AdviceReport::failed("unavailable", "omp allocator unavailable".into());
        };
        {
            let mut turn = self.tools.turn.lock();
            turn.context = Some(context.clone());
            turn.proposal = None;
        }
        let result = live
            .session
            .prompt(&round_prompt(context), self.config.timeout)
            .await;
        let tool_proposal = {
            let mut turn = self.tools.turn.lock();
            turn.context = None;
            turn.proposal.take()
        };
        let mut report = match result {
            Ok(outcome) => {
                let status = match outcome.status {
                    PromptStatus::Completed => "completed",
                    PromptStatus::Aborted => "aborted",
                    PromptStatus::Error => "error",
                };
                AdviceReport::from_outcome(status, &outcome, None)
            }
            Err(OmpError::PromptTimeout { after, outcome }) => AdviceReport::from_outcome(
                "timeout",
                &outcome,
                Some(format!(
                    "Claude timed out after {}s; turn aborted",
                    after.as_secs()
                )),
            ),
            Err(error) => {
                let crashed =
                    error.is_disconnect() || matches!(error, OmpError::ProcessDied { .. });
                let report = AdviceReport::failed(
                    if crashed { "crashed" } else { "error" },
                    error.to_string(),
                );
                if crashed {
                    // The next round starts a fresh process on the same session dir.
                    *guard = None;
                }
                return report;
            }
        };
        report.session_id.clone_from(&live.session_id);
        report.resumed = live.resumed;
        report.proposal = tool_proposal.or_else(|| {
            report
                .final_text
                .as_deref()
                .and_then(extract_allocation_proposal)
                .filter(|p| validate_proposal(p, &context.module_ids).is_ok())
        });
        maybe_compact(&live.session, self.config.compact_at_percent).await;
        report
    }

    /// Close omp cleanly.
    pub async fn shutdown(&self) {
        if let Some(live) = self.live.lock().await.take()
            && let Err(error) = live.session.shutdown().await
        {
            tracing::warn!(%error, "omp allocator shutdown failed");
        }
    }
}

async fn maybe_compact(session: &AgentSession, threshold: f64) {
    let percent = match session.state().await {
        Ok(state) => state.context_usage.and_then(|usage| usage.percent),
        Err(error) => {
            tracing::warn!(%error, "get_state failed; skipping compaction check");
            return;
        }
    };
    if percent.is_some_and(|p| p >= threshold)
        && let Err(error) = session.compact(Some(COMPACT_INSTRUCTIONS)).await
    {
        tracing::warn!(%error, "allocator compaction failed");
    }
}

#[async_trait]
impl ProposalProvider for ClaudeAllocator {
    async fn proposal(&self, context: &AllocationContext, timeout: Duration) -> Option<Value> {
        match tokio::time::timeout(timeout, self.advise(context)).await {
            Ok(report) => {
                if report.proposal.is_none() {
                    tracing::warn!(status = %report.status, error = ?report.error, "Claude allocator gave no proposal; using fallback");
                }
                report.proposal
            }
            Err(_) => {
                tracing::warn!(
                    timeout_s = timeout.as_secs_f64(),
                    "Claude allocator exceeded the advisor timeout; using fallback"
                );
                None
            }
        }
    }
}

/// The advisor a pool runtime attaches: `None` unless `CAPITAL_AI_ADVISOR_ENABLED`; tries
/// `CAPITAL_AI_ADVISOR_FILE` first, then the Claude allocator, the whole call bounded by
/// `CAPITAL_AI_ADVISOR_TIMEOUT_SECONDS`.
pub fn guarded_advisor_from_env(fleet: Arc<dyn FleetSource>) -> Result<Option<GuardedAdvisor>> {
    let advisor = AdvisorConfig::from_env()?;
    if !advisor.enabled {
        return Ok(None);
    }
    let mut providers: Vec<Arc<dyn ProposalProvider>> = Vec::new();
    if let Some(path) = &advisor.proposal_file {
        providers.push(Arc::new(FileProposalProvider::new(path)));
    }
    providers.push(Arc::new(ClaudeAllocator::new(
        ClaudeAllocatorConfig::from_env()?,
        fleet,
    )));
    Ok(Some(GuardedAdvisor::new(
        Arc::new(FirstAvailable(providers)),
        advisor.timeout,
    )))
}

/// The user turn of one round.
pub fn round_prompt(context: &AllocationContext) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Allocation round — {}",
        bb_core::time::iso(bb_core::time::now())
    );
    out.push_str("Review this allocation context and return a guarded proposal through propose_allocation.\n\n");
    out.push_str(RULES);
    out.push_str("\n\nAllocation context:\n");
    out.push_str(&serde_json::to_string_pretty(&context.to_json()).unwrap_or_default());
    out.push_str(
        "\n\nDeeper data: get_fleet_status (bot activity, ledger P&L), get_balances (wallet), get_scorecards (this context).\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bb_omp::codec::{FrameLimits, FrameReader};
    use rust_decimal_macros::dec;
    use serde_json::Map;
    use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

    use super::*;
    use crate::allocator::{AllocationUnit, Scoring, build_plan};
    use crate::config::CapitalPoolConfig;
    use crate::scorecard::ModuleScorecard;

    struct Server {
        reader: FrameReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Server {
        fn pair() -> (Self, Transport) {
            let (client, server) = tokio::io::duplex(1 << 20);
            let (client_read, client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            (
                Self {
                    reader: FrameReader::new(server_read, FrameLimits::default()),
                    writer: server_write,
                },
                Transport::new(client_read, client_write),
            )
        }

        async fn send(&mut self, frame: Value) {
            let mut line = frame.to_string();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        async fn expect(&mut self, kind: &str) -> (String, Map<String, Value>) {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.reader.next_frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(frame["type"], kind, "{frame:?}");
            (frame["id"].as_str().unwrap_or_default().to_owned(), frame)
        }

        async fn respond(&mut self, id: &str, command: &str, data: Value) {
            self.send(json!({"id": id, "type": "response", "command": command, "success": true, "data": data}))
                .await;
        }

        async fn boot(&mut self) -> String {
            self.send(
                json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1, 2],
                             "maxFrameBytes": 1_048_576, "maxReassembledFrameBytes": 67_108_864}),
            )
            .await;
            let (id, _) = self.expect("negotiate_protocol").await;
            self.respond(&id, "negotiate_protocol", json!({"protocolVersion": 2}))
                .await;
            let (id, frame) = self.expect("set_event_filter").await;
            self.respond(&id, "set_event_filter", json!({"events": frame["events"]}))
                .await;
            let (id, frame) = self.expect("set_host_tools").await;
            let names: Vec<Value> = frame["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].clone())
                .collect();
            assert!(
                names.contains(&json!("propose_allocation"))
                    && names.contains(&json!("get_fleet_status"))
            );
            self.respond(&id, "set_host_tools", json!({"toolNames": names}))
                .await;
            let (id, frame) = self.expect("open_session").await;
            let dir = frame["sessionDir"].as_str().unwrap().to_owned();
            self.respond(
                &id,
                "open_session",
                json!({"cancelled": false, "resumed": true, "sessionId": "alloc"}),
            )
            .await;
            dir
        }

        /// Claude calls `tool` with `args`; returns the host result frame.
        async fn tool_call(&mut self, call_id: &str, tool: &str, args: Value) -> Value {
            self.send(json!({"type": "tool_execution_start", "toolCallId": call_id, "toolName": tool, "args": args}))
                .await;
            self.send(json!({"type": "host_tool_call", "id": format!("h-{call_id}"), "toolCallId": call_id,
                             "toolName": tool, "arguments": args}))
                .await;
            let (_, result) = self.expect("host_tool_result").await;
            let is_error = result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.send(
                json!({"type": "tool_execution_end", "toolCallId": call_id, "toolName": tool,
                             "result": result["result"], "isError": is_error}),
            )
            .await;
            Value::Object(result)
        }
    }

    struct StaticFleet;

    #[async_trait]
    impl FleetSource for StaticFleet {
        async fn fleet_status(&self) -> std::result::Result<Value, String> {
            Ok(json!([{"name": "trend_filter", "trades_lifetime": 3}]))
        }
        async fn balances(&self) -> std::result::Result<Value, String> {
            Err("no Binance credentials configured".into())
        }
    }

    fn config(dir: &std::path::Path) -> ClaudeAllocatorConfig {
        ClaudeAllocatorConfig {
            command: "omp".into(),
            model: "haiku".into(),
            timeout: Duration::from_secs(10),
            session_dir: dir.join("session"),
            compact_at_percent: 70.0,
        }
    }

    fn context() -> AllocationContext {
        let config = CapitalPoolConfig {
            enabled: true,
            reserve_pct: dec!(40),
            min_module_allocation_usdt: Decimal::ZERO,
            max_module_allocation_usdt: Some(dec!(25)),
        };
        let units = vec![
            AllocationUnit::new(ModuleScorecard::new("trend_filter")),
            AllocationUnit::new(ModuleScorecard::new("funding_arb")),
        ];
        let fallback_plan = build_plan(&config, dec!(100), &units, Scoring::Scorecard);
        AllocationContext {
            config,
            scoring: Scoring::Scorecard,
            total_equity_usdt: dec!(100),
            module_ids: vec!["trend_filter".into(), "funding_arb".into()],
            scorecards: BTreeMap::new(),
            fallback_plan,
        }
    }

    #[tokio::test]
    async fn round_uses_tools_validates_and_returns_the_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let session_dir = cfg.session_dir.clone();
        let (mut server, transport) = Server::pair();
        let script = tokio::spawn(async move {
            let dir = server.boot().await;
            let (pid, frame) = server.expect("prompt").await;
            let message = frame["message"].as_str().unwrap().to_owned();
            server.respond(&pid, "prompt", Value::Null).await;
            let status = server.tool_call("t1", "get_fleet_status", json!({})).await;
            let balances = server.tool_call("t2", "get_balances", json!({})).await;
            let bad = server
                .tool_call(
                    "t3",
                    "propose_allocation",
                    json!({"allocations": [
                    {"module_id": "ghost", "status": "active", "max_usdt": 5, "reason": "x"}]}),
                )
                .await;
            let good = server
                .tool_call("t4", "propose_allocation", json!({"allocations": [
                    {"module_id": "trend_filter", "status": "active", "max_usdt": 500, "reason": "ai_scale_winner"},
                    {"module_id": "funding_arb", "status": "paused", "max_usdt": 0, "reason": "ai_no_evidence"}]}))
                .await;
            server
                .send(json!({"type": "message_end", "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "Proposed."}], "stopReason": "stop",
                    "usage": {"input": 900, "output": 40, "cacheRead": 0, "cacheWrite": 0, "cost": {"total": 0.002}}}}))
                .await;
            server
                .send(json!({"type": "prompt_result", "id": pid, "agentInvoked": true, "status": "completed", "sessionSettled": true}))
                .await;
            let (id, _) = server.expect("get_state").await;
            server
                .respond(&id, "get_state", json!({"contextUsage": {"percent": 12.0}}))
                .await;
            (dir, message, status, balances, bad, good)
        });
        let allocator = ClaudeAllocator::connect(cfg, Arc::new(StaticFleet), transport)
            .await
            .unwrap();
        let ctx = context();
        let report = allocator.advise(&ctx).await;
        let (dir_seen, message, status, balances, bad, good) = script.await.unwrap();

        assert_eq!(dir_seen, session_dir.to_string_lossy());
        assert!(
            message.contains("The proposal is not an order") && message.contains("fallback_plan")
        );
        assert!(status["result"].to_string().contains("trend_filter"));
        assert_eq!(balances["isError"], true);
        assert_eq!(bad["isError"], true);
        assert!(
            bad["result"]
                .to_string()
                .contains("not one of the listed module_ids")
        );
        assert!(good["result"].to_string().contains("guarded_plan"));

        assert_eq!(report.status, "completed");
        assert_eq!(report.session_id, "alloc");
        assert!(report.resumed);
        assert_eq!(report.tool_calls.len(), 4);
        assert_eq!(report.cost_usd.to_string(), "0.002");
        let proposal = report.proposal.unwrap();
        let plan = guard_proposal(Some(&proposal), &ctx.fallback_plan, &ctx.config);
        // Raised far above the fallback but clamped to the 25 USDT cap; the pause sticks.
        assert_eq!(
            plan.allocation_for("trend_filter").unwrap().max_usdt,
            dec!(25)
        );
        assert!(
            plan.allocation_for("funding_arb")
                .unwrap()
                .max_usdt
                .is_zero()
        );
    }

    #[test]
    fn system_prompt_and_tools_keep_the_no_order_contract() {
        assert!(SYSTEM_PROMPT.contains("do not submit exchange orders"));
        assert!(RULES.contains("The proposal is not an order"));
        let tools = tool_definitions();
        let propose = tools
            .iter()
            .find(|t| t.name == "propose_allocation")
            .unwrap();
        assert_eq!(propose.parameters, allocation_schema());
    }
}

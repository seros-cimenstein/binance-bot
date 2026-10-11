//! omp host tools — the agent's hands. Arguments are typed and validated; every failure is
//! returned as an `is_error` tool result (never a panic).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bb_omp::{CancellationToken, HostTool, HostToolHandler, HostToolOutcome};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::context::decimal_json;
use crate::cycle::{Runtime, route_summary};
use crate::journal::MAX_ENTRY_CHARS;
use crate::memory::value_decimal;
use crate::universe::{candidate_assets, quote_routes};

const KLINE_INTERVALS: [&str; 14] = [
    "1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "6h", "8h", "12h", "1d", "3d", "1w",
];
const MAX_EVIDENCE_ASSETS: usize = 12;
const MAX_ROUTES: usize = 50;

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

/// Tool catalogue registered with omp.
pub fn tool_definitions() -> Vec<HostTool> {
    let assets = json!({"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_EVIDENCE_ASSETS});
    vec![
        HostTool::new(
            "get_portfolio",
            "Portfolio",
            "Live balances (free/locked), indicative USDT values, portfolio value, current holding and open trade outcomes.",
            object(json!({}), &[]),
        ),
        HostTool::new(
            "list_routes",
            "List routes",
            "Fresh route quotes from current free balances. Filter by source and/or destination asset; limit 1-50 (default 10). Returns route_id, legs, friction bps, estimated USDT output, liquidity tier, spread, pick score.",
            object(
                json!({
                    "source": {"type": "string", "description": "asset to spend, e.g. USDT"},
                    "destination": {"type": "string", "description": "asset to receive, e.g. SOL"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_ROUTES}
                }),
                &[],
            ),
        ),
        HostTool::new(
            "get_market_evidence",
            "Market evidence",
            "Evidence packets per asset: returns over 5m/15m/1h/4h/24h/7d in bps, hourly volatility, 24h volume change. Up to 12 assets.",
            object(json!({"assets": assets}), &["assets"]),
        ),
        HostTool::new(
            "get_klines",
            "Klines",
            "Raw Binance Spot candles [open_time_ms, open, high, low, close, volume] for a symbol (e.g. SOL/USDT), interval (1m..1w) and limit 1-500.",
            object(
                json!({
                    "symbol": {"type": "string"},
                    "interval": {"type": "string", "enum": KLINE_INTERVALS},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500}
                }),
                &["symbol", "interval"],
            ),
        ),
        HostTool::new(
            "get_social_evidence",
            "Social evidence",
            "Social-signal summaries (mentions, bullish/bearish balance, manipulation-risk score) per asset; status disabled when collection is off.",
            object(
                json!({"assets": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 20}}),
                &["assets"],
            ),
        ),
        HostTool::new(
            "get_trade_history",
            "Trade history",
            "Outcomes of submitted routes from the audit log (entry/exit/current USDT value, pnl bps, status). limit 1-50 (default 8).",
            object(
                json!({"limit": {"type": "integer", "minimum": 1, "maximum": 50}}),
                &[],
            ),
        ),
        HostTool::new(
            "execute_route",
            "Execute route",
            "The only order path. route_id must come from this cycle's quotes. Optional spend_amount (in source asset units) scales the position; default spends the whole free source balance. Plan-only mode records a plan; live mode places real Spot market orders and returns per-leg results.",
            object(
                json!({
                    "route_id": {"type": "string"},
                    "reason": {"type": "string", "description": "your thesis: signal, magnitude, horizon, invalidation"},
                    "spend_amount": {"type": "number", "exclusiveMinimum": 0}
                }),
                &["route_id", "reason"],
            ),
        ),
        HostTool::new(
            "journal_write",
            "Journal write",
            "Append a durable note (theses, invalidation levels, lessons). Survives compaction and restarts; the latest notes are shown every cycle.",
            object(
                json!({"text": {"type": "string", "maxLength": MAX_ENTRY_CHARS}}),
                &["text"],
            ),
        ),
        HostTool::new(
            "journal_read",
            "Journal read",
            "Read the latest journal notes (limit 1-50, default 10).",
            object(
                json!({"limit": {"type": "integer", "minimum": 1, "maximum": 50}}),
                &[],
            ),
        ),
        HostTool::new(
            "end_cycle",
            "End cycle",
            "Finish the cycle: action hold or traded, with a short summary of what you did and why. Call exactly once per cycle.",
            object(
                json!({
                    "action": {"type": "string", "enum": ["hold", "traded"]},
                    "summary": {"type": "string"}
                }),
                &["action", "summary"],
            ),
        ),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRoutesArgs {
    source: Option<String>,
    destination: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetsArgs {
    assets: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KlinesArgs {
    symbol: String,
    interval: String,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitArgs {
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteArgs {
    route_id: String,
    reason: String,
    spend_amount: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalWriteArgs {
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndCycleArgs {
    action: String,
    summary: String,
}

type ToolResult = Result<Value, String>;

fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    let arguments = if arguments.is_null() {
        json!({})
    } else {
        arguments
    };
    serde_json::from_value(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

fn limit(value: Option<usize>, default: usize, max: usize) -> Result<usize, String> {
    match value.unwrap_or(default) {
        0 => Err("limit must be at least 1".into()),
        n if n > max => Err(format!("limit must be at most {max}")),
        n => Ok(n),
    }
}

fn normalized_assets(assets: &[String], max: usize) -> Result<Vec<String>, String> {
    if assets.is_empty() || assets.len() > max {
        return Err(format!("assets must list 1-{max} assets"));
    }
    let mut out: Vec<String> = Vec::new();
    for asset in assets {
        let asset = asset.trim().to_uppercase();
        if asset.is_empty() || !asset.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(format!("invalid asset {asset:?}"));
        }
        if !out.contains(&asset) {
            out.push(asset);
        }
    }
    Ok(out)
}

fn asset_arg(value: Option<String>) -> Result<Option<String>, String> {
    value
        .map(|asset| normalized_assets(&[asset], 1).map(|mut v| v.remove(0)))
        .transpose()
}

/// Host tool handler bound to the trader runtime.
#[derive(Debug)]
pub struct TraderTools {
    runtime: Arc<Runtime>,
}

impl TraderTools {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    /// Dispatch one call (also used directly by tests).
    pub async fn dispatch(&self, name: &str, arguments: Value) -> ToolResult {
        match name {
            "get_portfolio" => {
                parse::<NoArgs>(arguments)?;
                self.get_portfolio().await
            }
            "list_routes" => self.list_routes(parse(arguments)?).await,
            "get_market_evidence" => self.get_market_evidence(parse(arguments)?).await,
            "get_klines" => self.get_klines(parse(arguments)?).await,
            "get_social_evidence" => self.get_social_evidence(parse(arguments)?),
            "get_trade_history" => self.get_trade_history(parse(arguments)?).await,
            "execute_route" => self.execute_route(parse(arguments)?).await,
            "journal_write" => self.journal_write(parse(arguments)?).await,
            "journal_read" => self.journal_read(parse(arguments)?),
            "end_cycle" => self.end_cycle(parse(arguments)?).await,
            other => Err(format!("unknown tool {other}")),
        }
    }

    async fn get_portfolio(&self) -> ToolResult {
        let runtime = &self.runtime;
        let mut guard = runtime.cycle.lock().await;
        let state = guard.as_mut().ok_or("no active cycle")?;
        state.balances = runtime
            .balances()
            .await
            .map_err(|e| format!("balance read failed: {e}"))?;
        let view = Arc::clone(&state.view);
        let balances = &state.balances;
        let rows: Vec<Value> = balances
            .total
            .keys()
            .map(|asset| {
                let free = balances.free(asset);
                let locked = balances.used.get(asset).copied().unwrap_or_default();
                json!({
                    "asset": asset,
                    "free": decimal_json(free),
                    "locked": decimal_json(locked),
                    "free_value_usdt": view.usdt_value(asset, free).map(|v| decimal_json(v.round_dp(4))),
                    "total_value_usdt": view.usdt_value(asset, free + locked).map(|v| decimal_json(v.round_dp(4))),
                })
            })
            .collect();
        let memory = runtime.trade_memory(balances, &view, runtime.config.trade_memory_limit);
        let open: Vec<_> = memory
            .recent_outcomes
            .iter()
            .filter(|outcome| outcome.status == "open")
            .collect();
        Ok(json!({
            "balances": rows,
            "portfolio_value_usdt": decimal_json(balances.free_value_usdt(&view).round_dp(4)),
            "total_value_usdt": decimal_json(balances.total_value_usdt(&view).round_dp(4)),
            "current_holding": memory.current_holding,
            "open_outcomes": open,
            "mode": runtime.mode.as_str(),
        }))
    }

    async fn list_routes(&self, args: ListRoutesArgs) -> ToolResult {
        let limit = limit(args.limit, 10, MAX_ROUTES)?;
        let source = asset_arg(args.source)?;
        let destination = asset_arg(args.destination)?;
        let runtime = &self.runtime;
        let config = &runtime.config;
        for asset in source.iter().chain(destination.iter()) {
            if let Some(reason) = config.scope.asset_block_reason(asset) {
                return Err(reason);
            }
        }
        let mut guard = runtime.cycle.lock().await;
        let state = guard.as_mut().ok_or("no active cycle")?;
        if state.view.age_seconds() > config.route_stale_seconds {
            state.view = runtime
                .load_view()
                .await
                .map_err(|e| format!("market refresh failed: {e}"))?;
        }
        state.balances = runtime
            .balances()
            .await
            .map_err(|e| format!("balance read failed: {e}"))?;
        let destinations = match &destination {
            Some(asset) => vec![asset.clone()],
            None => candidate_assets(&state.view, &state.balances.free, config),
        };
        let sources = source.map(|asset| vec![asset]);
        let mut routes = quote_routes(
            &state.view,
            config,
            &state.balances.free,
            sources.as_deref(),
            &destinations,
        )
        .await
        .map_err(|e| format!("route quote failed: {e}"))?;
        routes.truncate(limit);
        let summaries: Vec<Value> = routes.iter().map(route_summary).collect();
        for route in routes {
            state.quotes.insert(route.route_id.clone(), route);
        }
        Ok(json!({"routes": summaries, "count": summaries.len()}))
    }

    async fn get_market_evidence(&self, args: AssetsArgs) -> ToolResult {
        let assets = normalized_assets(&args.assets, MAX_EVIDENCE_ASSETS)?;
        let view = {
            let guard = self.runtime.cycle.lock().await;
            Arc::clone(&guard.as_ref().ok_or("no active cycle")?.view)
        };
        let mut out = BTreeMap::new();
        for asset in assets {
            let evidence = self
                .runtime
                .evidence
                .asset(&asset, &view.markets, false)
                .await;
            out.insert(asset, evidence);
        }
        Ok(json!(out))
    }

    async fn get_klines(&self, args: KlinesArgs) -> ToolResult {
        let interval = args.interval.trim();
        if !KLINE_INTERVALS.contains(&interval) {
            return Err(format!(
                "interval must be one of {}",
                KLINE_INTERVALS.join(", ")
            ));
        }
        let limit = args.limit.unwrap_or(100);
        if !(1..=500).contains(&limit) {
            return Err("limit must be between 1 and 500".into());
        }
        let symbol = args.symbol.trim().to_uppercase();
        if symbol.is_empty() {
            return Err("symbol must not be blank".into());
        }
        let klines = self
            .runtime
            .client
            .klines(&symbol, interval, None, None, Some(limit))
            .await
            .map_err(|e| format!("klines fetch failed: {e}"))?;
        let rows: Vec<Value> = klines
            .iter()
            .map(|k| {
                json!([
                    k.open_time,
                    decimal_json(k.open),
                    decimal_json(k.high),
                    decimal_json(k.low),
                    decimal_json(k.close),
                    decimal_json(k.volume)
                ])
            })
            .collect();
        Ok(
            json!({"symbol": symbol, "interval": interval, "columns": ["open_time_ms", "open", "high", "low", "close", "volume"], "rows": rows}),
        )
    }

    fn get_social_evidence(&self, args: AssetsArgs) -> ToolResult {
        let assets = normalized_assets(&args.assets, 20)?;
        match self.runtime.social.as_ref().filter(|s| s.enabled) {
            None => Ok(json!(
                assets
                    .into_iter()
                    .map(|a| (a, json!({"status": "disabled"})))
                    .collect::<BTreeMap<_, _>>()
            )),
            Some(settings) => bb_social::load_evidence(settings, &assets)
                .map(|evidence| json!(evidence))
                .map_err(|e| format!("social evidence failed: {e}")),
        }
    }

    async fn get_trade_history(&self, args: LimitArgs) -> ToolResult {
        let limit = limit(args.limit, 8, 50)?;
        let guard = self.runtime.cycle.lock().await;
        let state = guard.as_ref().ok_or("no active cycle")?;
        Ok(json!(self.runtime.trade_memory(
            &state.balances,
            &state.view,
            limit
        )))
    }

    async fn execute_route(&self, args: ExecuteArgs) -> ToolResult {
        let route_id = args.route_id.trim();
        if route_id.is_empty() {
            return Err("route_id must not be blank".into());
        }
        let reason = args.reason.trim();
        if reason.is_empty() {
            return Err("reason must state your thesis".into());
        }
        let spend: Option<Decimal> = match &args.spend_amount {
            None | Some(Value::Null) => None,
            Some(value) => match value_decimal(Some(value)) {
                Some(amount) if amount > Decimal::ZERO => Some(amount),
                _ => return Err("spend_amount must be a positive number".into()),
            },
        };
        self.runtime.execute_route(route_id, reason, spend).await
    }

    async fn journal_write(&self, args: JournalWriteArgs) -> ToolResult {
        let text = args.text.trim();
        if text.is_empty() {
            return Err("text must not be blank".into());
        }
        if text.chars().count() > MAX_ENTRY_CHARS {
            return Err(format!("text must be at most {MAX_ENTRY_CHARS} characters"));
        }
        let cycle_id = {
            let guard = self.runtime.cycle.lock().await;
            guard
                .as_ref()
                .map(|s| s.cycle_id.clone())
                .unwrap_or_default()
        };
        let entry = self
            .runtime
            .journal
            .write(&cycle_id, text)
            .map_err(|e| format!("journal write failed: {e}"))?;
        Ok(json!({"recorded": true, "timestamp": entry.timestamp}))
    }

    fn journal_read(&self, args: LimitArgs) -> ToolResult {
        let limit = limit(args.limit, 10, 50)?;
        let entries = self
            .runtime
            .journal
            .read(limit)
            .map_err(|e| format!("journal read failed: {e}"))?;
        Ok(json!({"entries": entries}))
    }

    async fn end_cycle(&self, args: EndCycleArgs) -> ToolResult {
        let action = args.action.trim().to_lowercase();
        if action != "hold" && action != "traded" {
            return Err("action must be hold or traded".into());
        }
        let summary = args.summary.trim();
        if summary.is_empty() {
            return Err("summary must not be blank".into());
        }
        let mut guard = self.runtime.cycle.lock().await;
        let state = guard.as_mut().ok_or("no active cycle")?;
        state.end_cycle = Some(crate::audit::EndCycle {
            action,
            summary: summary.to_owned(),
        });
        Ok(json!({"recorded": true, "note": "cycle closed; stop calling tools and reply briefly"}))
    }
}

#[async_trait]
impl HostToolHandler for TraderTools {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> HostToolOutcome {
        // Cancellation is deliberately ignored: an execute_route in flight must finish every
        // leg it started (never abandon a half route); omp drops the late result.
        match self.dispatch(name, arguments).await {
            Ok(value) => HostToolOutcome::text(value.to_string()),
            Err(message) => HostToolOutcome::error(message),
        }
    }
}

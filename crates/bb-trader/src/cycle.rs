//! Cycle runtime: the per-cycle market snapshot shared with the host tools, route
//! planning/execution with audit rows, and the [`Trader`] loop that drives the omp agent.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bb_binance::BinanceClient;
use bb_omp::CancellationToken;
use bb_pool::CommissionService;
use bb_social::SocialSettings;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::agent::{TraderAgent, cycle_prompt};
use crate::alerts::{Alerter, BRAIN_UNAVAILABLE};
use crate::audit::{
    AgentRecord, AttemptEntry, AuditLog, AuditRow, Decision, EndCycle, RowStatus, RowType,
    ToolCallEntry, UsageEntry, failed_route_halt,
};
use crate::config::TraderConfig;
use crate::context::{DecisionContext, decimal_json, execution_invariants, route_scope};
use crate::error::{Result, TraderError};
use crate::evidence::{EvidenceService, route_evidence};
use crate::executor::{Executor, LegResult};
use crate::journal::{AgentState, ExecutionSummary, Journal, LastCycle};
use crate::memory::{TradeMemory, TradeRecord};
use crate::tools::TraderTools;
use crate::universe::{
    Balances, MarketView, RouteCandidate, build_route_candidates, quote_candidate,
};

/// 1m bars read per coin for the algorithm's 15m/1h returns: 61 closed bars plus the open one.
const ALGO_PRICE_MINUTES: u32 = 62;

/// The exit-plan watcher saves its state every few seconds; older state means it is down.
const EXIT_WATCHER_STALE_MS: i64 = 60_000;

/// What one cycle turn produced.
#[derive(Debug, Clone, Default)]
pub struct TurnReport {
    /// `completed`, `aborted`, `error`, `timeout` or `crashed`.
    pub status: String,
    pub error: Option<String>,
    pub final_text: Option<String>,
    /// Every tool call of the cycle (failed attempts first).
    pub tool_calls: Vec<ToolCallEntry>,
    /// Usage summed over every attempt.
    pub usage: UsageEntry,
    /// `provider/model-id` that produced the final outcome (empty = the configured model).
    pub model: String,
    /// Thinking level of `model` (empty = the configured level).
    pub thinking: String,
    /// Failures that moved this cycle to another model (compactions and turn attempts).
    pub attempts: Vec<AttemptEntry>,
    /// `model` is not the primary.
    pub fallback_active: bool,
    /// `Some(true)`: the turn failed on every configured model (no orders were placed);
    /// `Some(false)`: a model completed the turn; `None`: neither (shutdown, aborted, or a
    /// retry refused because the failed attempt called `execute_route`).
    pub all_models_failed: Option<bool>,
}

/// Whether `execute_route` may place orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleMode {
    /// `execute_route` records a plan; no order endpoint is ever called.
    PlanOnly,
    /// Real Spot market orders (requires the live arming flags).
    Live,
}

impl CycleMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlanOnly => "plan-only",
            Self::Live => "live",
        }
    }
}

/// Everything one cycle knows; replaced at the start of every cycle.
#[derive(Debug)]
pub struct CycleState {
    pub cycle_id: String,
    pub session_id: String,
    pub view: Arc<MarketView>,
    pub balances: Balances,
    pub context: DecisionContext,
    pub context_hash: String,
    /// Every route quoted this cycle (prompt table + `list_routes`), by route id.
    pub quotes: HashMap<String, RouteCandidate>,
    pub executions: Vec<ExecutionSummary>,
    pub end_cycle: Option<EndCycle>,
    /// Set when a live route failed: no further routes this cycle.
    pub halted: Option<String>,
    /// Route ids whose Telegram alert was delivered (alerts follow the durable write).
    pub telegram_sent_routes: Vec<String>,
    /// Algo cycles (Phase 51): the rule's verdict, recorded on the cycle summary.
    pub algo: Option<Value>,
}

/// Market snapshot built for a cycle (or `--evidence-only`).
#[derive(Debug)]
pub struct Snapshot {
    pub context: DecisionContext,
    pub view: Arc<MarketView>,
    pub balances: Balances,
    /// The personas' point-in-time market view (`None` when the breadth read failed or
    /// market evidence is off); the rule-based decider reads it.
    pub step: Option<crate::book::Snapshot>,
}

/// Shared by the cycle loop and the host tools.
#[derive(Debug)]
pub struct Runtime {
    pub config: Arc<TraderConfig>,
    pub mode: CycleMode,
    pub client: BinanceClient,
    pub audit: AuditLog,
    pub alerter: Alerter,
    pub journal: Journal,
    pub evidence: EvidenceService,
    pub social: Option<SocialSettings>,
    /// Live GCP2 reader (`None` unless `BOT_SPOT_GCP2_ENABLED`).
    pub gcp2: Option<crate::gcp2::Gcp2LiveReader>,
    pub(crate) cycle: Mutex<Option<CycleState>>,
    shutting_down: AtomicBool,
    /// Final-order poll interval (1 s live).
    pub poll_interval: Duration,
    /// Recorded on every cycle summary (`run_config`); set once by the CLI.
    pub run_config: std::sync::OnceLock<Value>,
}

impl Runtime {
    pub fn new(
        config: Arc<TraderConfig>,
        mode: CycleMode,
        client: BinanceClient,
        alerter: Alerter,
        social: Option<SocialSettings>,
    ) -> Self {
        let evidence = EvidenceService::new(
            bb_binance::history::HistoryFetcher::spot(client.clone()),
            config.market_evidence_lookback_hours,
            config.market_evidence_max_symbols,
        );
        Self {
            audit: AuditLog::new(&config.audit_path),
            journal: Journal::new(&config.journal_path),
            evidence,
            mode,
            client,
            alerter,
            social,
            gcp2: config.gcp2_enabled.then(|| {
                crate::gcp2::Gcp2LiveReader::new(&config.gcp2_base_url, crate::gcp2::LIVE_TIMEOUT)
            }),
            cycle: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            poll_interval: Duration::from_secs(1),
            run_config: std::sync::OnceLock::new(),
            config,
        }
    }

    /// Refuse new routes (graceful shutdown); in-flight routes finish.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    pub fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub(crate) fn new_fees(&self) -> Arc<CommissionService> {
        Arc::new(CommissionService::new(
            Arc::new(self.client.clone()),
            self.config.fee_cache_seconds,
        ))
    }

    pub(crate) async fn load_view(&self) -> Result<Arc<MarketView>> {
        Ok(Arc::new(
            MarketView::load(&self.client, self.new_fees(), &self.config).await?,
        ))
    }

    pub(crate) async fn balances(&self) -> Result<Balances> {
        Ok(Balances::from_account(&self.client.account().await?))
    }

    pub(crate) fn trade_memory(
        &self,
        balances: &Balances,
        view: &MarketView,
        limit: usize,
    ) -> TradeMemory {
        TradeMemory::from_rows(
            &self.audit.rows(),
            &balances.free,
            |asset, amount| view.usdt_value(asset, amount),
            limit,
        )
    }

    /// The bot's record from the whole audit log, fees in other coins valued at the view's
    /// prices, plus how the price moved around its entries (candles fetched once per entry,
    /// cached next to the audit). `None` when the log cannot be read.
    pub(crate) async fn trade_record(&self, balances: &Balances, view: &MarketView) -> Option<TradeRecord> {
        let rows = match crate::exit_replay::read_audit(&self.config.audit_path) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "audit unreadable; the cycle prompt has no record section");
                return None;
            }
        };
        let mark = |asset: &str| {
            Some(crate::trades::Mark {
                free: balances.free(asset),
                price_usdt: view.usdt_value(asset, Decimal::ONE)?,
            })
        };
        let mut record = TradeRecord::from_rows(&rows, mark, bb_core::time::now());
        let (trips, _) = crate::exit_replay::round_trips(&rows, Some("USDT"), None);
        record.entry_timing = crate::entry_timing::summary(
            &self.client,
            &crate::entry_timing::cache_path(&self.config.audit_path),
            &trips,
            bb_core::time::now_ms(),
        )
        .await;
        Some(record)
    }

    /// Catalog, balances, candidate routes, evidence, social evidence, trade memory, GCP2,
    /// the schizo state, the long-term value voice and the /biz/ feed.
    pub async fn build_snapshot(&self) -> Result<Snapshot> {
        let balances = self.balances().await?;
        let view = self.load_view().await?;
        let mut candidates = build_route_candidates(&view, &self.config, &balances.free).await?;
        let trade_memory = self.trade_memory(&balances, &view, self.config.trade_memory_limit);
        let trade_record = self.trade_record(&balances, &view).await;
        self.evidence.clear().await;
        let breadth = if self.config.market_evidence_enabled {
            for candidate in &mut candidates {
                let source = self
                    .evidence
                    .asset(&candidate.route.source_asset, &view.markets, true)
                    .await;
                let destination = self
                    .evidence
                    .asset(&candidate.route.destination_asset, &view.markets, true)
                    .await;
                candidate.market_evidence = Some(route_evidence(
                    candidate.immediate_friction_bps,
                    source,
                    destination,
                ));
            }
            Some(crate::market_mood::fetch(Some(&self.client)).await)
        } else {
            None
        };
        if let Some(settings) = self.social.as_ref().filter(|s| s.enabled)
            && !candidates.is_empty()
        {
            let evidence = bb_social::load_evidence(
                settings,
                candidates.iter().map(|c| c.route.destination_asset.clone()),
            )?;
            for candidate in &mut candidates {
                candidate.social_signal_evidence = Some(
                    evidence
                        .get(&candidate.route.destination_asset)
                        .map_or_else(|| json!({"status": "empty"}), |e| json!(e)),
                );
            }
        }
        let gcp2 = match &self.gcp2 {
            Some(reader) => Some(reader.now(crate::audit::utc_timestamp()).await),
            None => None,
        };
        let portfolio_value_usdt = balances.free_value_usdt(&view);
        // The personas' point-in-time view, shared by the schizo voices and the MDP voice.
        let t = bb_core::time::now_ms();
        let latest = gcp2.as_ref().and_then(|g| g.live()).map(|live| live.latest.clone());
        let snap = breadth.as_ref().and_then(|mood| {
            crate::book::Snapshot::from_breadth(t, mood, latest.clone())
        });
        let schizo = match &gcp2 {
            Some(_) => self.schizo_state(t, latest.as_ref(), snap.as_ref(), &balances, &view, portfolio_value_usdt),
            None => None,
        };
        let mdp = self.config.mdp_enabled.then(|| {
            self.mdp_voice(trade_memory.current_holding.as_ref(), snap.as_ref())
        });
        let biz = self.biz_feed(&view);
        let step = if self.config.decider == crate::config::Decider::Algo {
            Some(self.algo_step(&view, &balances, portfolio_value_usdt, &biz).await)
        } else {
            None
        };
        let biz = Some(biz);
        let market_mood = breadth.filter(|mood| mood.score.is_some());
        let context = DecisionContext {
            timestamp: crate::audit::utc_timestamp(),
            prompt_version: self.config.prompt_version.clone(),
            market_pick_mode: self.config.market_pick_mode.as_str(),
            portfolio_value_usdt,
            balances: balances.clone(),
            candidate_routes: candidates,
            execution_invariants: execution_invariants(&self.config),
            route_scope: route_scope(&self.config),
            trade_memory,
            trade_record,
            market_mood,
            gcp2,
            schizo,
            mdp,
            biz,
        };
        Ok(Snapshot {
            context,
            view,
            balances,
            step,
        })
    }

    /// The algorithm's market view (operator decision 2026-10-29, "Feed coins + own
    /// prices"): the coins it may buy are the /biz/ feed coins with an eligible USDT market
    /// (stablecoins left out), and it reads 1m prices for those and for every held coin.
    /// Without a fresh feed it may only hold or sell. A coin whose price read fails is left
    /// out (logged), so it is neither bought nor scored as held.
    async fn algo_step(
        &self,
        view: &MarketView,
        balances: &Balances,
        equity: Decimal,
        biz: &crate::biz::BizNow,
    ) -> crate::book::Snapshot {
        use futures::StreamExt;
        use rust_decimal::prelude::ToPrimitive;
        let t = bb_core::time::now_ms();
        let usdt_market = |asset: &str| {
            view.markets
                .get(&format!("{asset}/USDT"))
                .filter(|market| market.eligible)
                .is_some()
        };
        let candidates: Vec<String> = match biz {
            crate::biz::BizNow::Fresh(feed) => feed
                .coins
                .keys()
                .filter(|asset| {
                    asset.as_str() != "USDT"
                        && !crate::evidence::is_low_upside(asset)
                        && usdt_market(asset)
                })
                .cloned()
                .collect(),
            crate::biz::BizNow::Unavailable { .. } => Vec::new(),
        };
        let held = crate::book::live_book(
            t,
            balances,
            equity,
            |asset, amount| view.usdt_value(asset, amount),
            |asset| view.markets.get(&format!("{asset}/USDT")).map(|m| m.min_notional),
        );
        let mut assets: Vec<String> = candidates.clone();
        assets.extend(held.holdings.keys().filter(|a| usdt_market(a)).cloned());
        assets.sort();
        assets.dedup();
        let client = &self.client;
        let coins = futures::stream::iter(assets)
            .map(|asset| async move {
                let symbol = format!("{asset}/USDT");
                match client.klines(&symbol, "1m", None, None, Some(ALGO_PRICE_MINUTES)).await {
                    Ok(klines) => {
                        let bars: Vec<(i64, f64)> = klines
                            .iter()
                            .filter(|k| k.close_time.is_some_and(|close| close < t))
                            .filter_map(|k| Some((k.open_time, k.close.to_f64()?)))
                            .collect();
                        crate::book::coin_view_from_minutes(&bars).map(|view| (asset, view))
                    }
                    Err(error) => {
                        tracing::warn!(%error, %symbol, "price read failed; coin left out this cycle");
                        None
                    }
                }
            })
            .buffer_unordered(8)
            .filter_map(|coin| async move { coin })
            .collect()
            .await;
        crate::book::Snapshot::live(t, coins, candidates)
    }

    /// The schizo state for this cycle (saved by [`Self::finish_cycle`]); `None`, with an
    /// error log, when the persisted memory cannot be read.
    fn schizo_state(
        &self,
        t: i64,
        latest: Option<&crate::gcp2::Gcp2View>,
        snap: Option<&crate::book::Snapshot>,
        balances: &Balances,
        view: &MarketView,
        equity: Decimal,
    ) -> Option<crate::schizo::SchizoState> {
        let path = crate::schizo::state_path(&self.config.state_path);
        let memory = match crate::schizo::SchizoMemory::load(&path) {
            Ok(memory) => memory,
            Err(error) => {
                tracing::error!(%error, path = %path.display(), "schizo memory unreadable; no schizo state this cycle");
                return None;
            }
        };
        let book = crate::book::live_book(
            t,
            balances,
            equity,
            |asset, amount| view.usdt_value(asset, amount),
            |asset| view.markets.get(&format!("{asset}/USDT")).map(|m| m.min_notional),
        );
        match crate::schizo::SchizoState::next(
            &memory,
            latest,
            snap,
            &book,
            crate::audit::utc_timestamp(),
        ) {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::error!(%error, "schizo state failed; none this cycle");
                None
            }
        }
    }

    /// The long-term value voice; `Unavailable`, with a warning, when the model file cannot
    /// be read.
    fn mdp_voice(
        &self,
        holding: Option<&crate::memory::CurrentHolding>,
        snap: Option<&crate::book::Snapshot>,
    ) -> crate::mdp::MdpNow {
        match crate::mdp::Model::load(&self.config.mdp_model_path) {
            Ok(model) => crate::mdp::MdpNow::Ready(Box::new(crate::mdp::voice(&model, holding, snap, chrono::Utc::now()))),
            Err(error) => {
                tracing::warn!(%error, "MDP voice unavailable this cycle");
                crate::mdp::MdpNow::Unavailable {
                    reason: error.to_string(),
                }
            }
        }
    }

    /// The /biz/ feed for this cycle, keeping the coins with an eligible market in the view.
    fn biz_feed(&self, view: &MarketView) -> crate::biz::BizNow {
        let listed: std::collections::BTreeSet<&str> = view
            .markets
            .values()
            .filter(|market| market.eligible)
            .map(|market| market.base.as_str())
            .collect();
        crate::biz::read(
            &self.config.biz_feed_path,
            bb_core::time::now_ms(),
            self.config.biz_max_age_minutes,
            |coin| listed.contains(coin),
        )
    }

    /// The rule-based decider's verdict on this snapshot; `Err` with the reason when it
    /// cannot decide (no market view this cycle).
    pub(crate) fn algo_verdict(
        &self,
        snapshot: &Snapshot,
        rule: &crate::decider::Rule,
    ) -> std::result::Result<crate::decider::Verdict, String> {
        use rust_decimal::prelude::ToPrimitive;
        let Some(step) = &snapshot.step else {
            return Err("no market view this cycle".into());
        };
        let view = &snapshot.view;
        let mut book = crate::book::live_book(
            step.t,
            &snapshot.balances,
            snapshot.context.portfolio_value_usdt,
            |asset, amount| view.usdt_value(asset, amount),
            |asset| view.markets.get(&format!("{asset}/USDT")).map(|m| m.min_notional),
        );
        // The open position's entry (price, time) from trade memory, for the value term.
        if let Some(open) = &snapshot.context.trade_memory.current_holding
            && open.status == "open"
            && let (Some(asset), Some(amount), Some(entry_value), Some(entered_at)) = (
                open.asset.as_deref(),
                open.amount,
                open.entry_value_usdt,
                open.entered_at.as_deref(),
            )
            && let Some(holding) = book.holdings.get_mut(asset)
            && amount > Decimal::ZERO
            && let (Some(price), Ok(at)) = (
                (entry_value / amount).to_f64(),
                chrono::DateTime::parse_from_rfc3339(entered_at),
            )
        {
            holding.entry = Some((price, at.timestamp_millis()));
        }
        let biz = match &snapshot.context.biz {
            Some(crate::biz::BizNow::Fresh(feed)) => Some(&feed.coins),
            _ => None,
        };
        let model = if self.config.mdp_enabled {
            crate::mdp::Model::load(&self.config.mdp_model_path)
                .map_err(|error| tracing::warn!(%error, "MDP model unavailable; the decider's value term is 0"))
                .ok()
        } else {
            None
        };
        Ok(crate::decider::decide(
            &crate::decider::Inputs {
                snap: step,
                book: &book,
                biz,
                mdp: model.as_ref(),
            },
            rule,
        ))
    }

    /// Quote `source` → `destination` for the whole free `source` balance into the active
    /// cycle's quotes and return its route id (the algo's way to a route `execute_route`
    /// accepts).
    async fn quote_for_cycle(
        &self,
        source: &str,
        destination: &str,
    ) -> std::result::Result<String, String> {
        let mut guard = self.cycle.lock().await;
        let state = guard.as_mut().ok_or("no active cycle")?;
        let amount = state.balances.free(source);
        if amount <= Decimal::ZERO {
            return Err(format!("no free {source} balance"));
        }
        let bnb_discount = state.balances.free("BNB") > Decimal::ZERO;
        let candidate = quote_candidate(
            &state.view,
            &self.config,
            source,
            destination,
            amount,
            bnb_discount,
        )
        .await
        .map_err(|e| format!("quote failed: {e}"))?
        .ok_or_else(|| format!("no executable route from {source} to {destination} for {amount}"))?;
        let route_id = candidate.route_id.clone();
        state.quotes.insert(route_id.clone(), candidate);
        Ok(route_id)
    }

    /// Run the verdict's orders through `execute_route` (the only order path, every hard
    /// safety check included), stopping at the first failure, and close the cycle's
    /// statement. Returns the turn report the cycle summary is written from.
    pub(crate) async fn run_algo_turn(
        &self,
        verdict: std::result::Result<crate::decider::Verdict, String>,
        rule: &crate::decider::Rule,
        shutdown: &CancellationToken,
    ) -> TurnReport {
        use crate::decider::Action;
        let mut report = TurnReport {
            status: "completed".into(),
            model: ALGO_MODEL.into(),
            thinking: "off".into(),
            ..TurnReport::default()
        };
        let mut traded = 0usize;
        let mut failure = None;
        let (summary, record) = match &verdict {
            Err(reason) => (format!("rule-based decider could not decide: {reason}; hold"), json!({"rule": rule.weights.to_string(), "error": reason})),
            Ok(verdict) => {
                for action in &verdict.actions {
                    if shutdown.is_cancelled() || self.shutting_down() {
                        failure = Some("shutdown: the remaining orders were not sent".to_owned());
                        break;
                    }
                    let (source, destination, reason) = match action {
                        Action::Sell { asset, reason } => (asset.as_str(), EXIT_QUOTE, reason.as_str()),
                        Action::Buy { asset, reason } => (EXIT_QUOTE, asset.as_str(), reason.as_str()),
                    };
                    let started = std::time::Instant::now();
                    let result = match self.quote_for_cycle(source, destination).await {
                        Ok(route_id) => self.execute_route(&route_id, reason, None).await,
                        Err(message) => Err(message),
                    };
                    report.tool_calls.push(crate::audit::ToolCallEntry {
                        name: "execute_route".into(),
                        args: json!({"source": source, "destination": destination, "reason": reason}),
                        is_error: result.is_err(),
                        ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    });
                    match result {
                        Ok(_) => traded += 1,
                        Err(message) => {
                            // A failed sell leaves the coin held: never buy a second one.
                            failure = Some(format!("{source}->{destination}: {message}"));
                            break;
                        }
                    }
                }
                let pair = |p: &(String, f64)| json!({"asset": p.0, "score": p.1});
                let orders: Vec<Value> = verdict
                    .actions
                    .iter()
                    .map(|a| match a {
                        Action::Sell { asset, reason } => json!({"side": "sell", "asset": asset, "reason": reason}),
                        Action::Buy { asset, reason } => json!({"side": "buy", "asset": asset, "reason": reason}),
                    })
                    .collect();
                let said: Vec<&str> = verdict
                    .actions
                    .iter()
                    .map(|a| match a {
                        Action::Sell { reason, .. } | Action::Buy { reason, .. } => reason.as_str(),
                    })
                    .collect();
                let summary = if said.is_empty() {
                    let best = verdict.best.as_ref().map_or_else(
                        || "nothing scored".to_owned(),
                        |(asset, score)| format!("best {asset} score {score:+.3} < buy level {:+}", rule.weights.buy_above),
                    );
                    let held = verdict
                        .held
                        .iter()
                        .map(|(asset, score)| format!("{asset} score {score:+.3} ≥ sell level {:+}", rule.weights.sell_below))
                        .collect::<Vec<_>>();
                    if held.is_empty() {
                        format!("rule-based decider: hold in cash ({best})")
                    } else {
                        format!("rule-based decider: keep {}", held.join(", "))
                    }
                } else {
                    format!("rule-based decider: {}", said.join("; "))
                };
                let record = json!({
                    "rule": rule.weights.to_string(),
                    "min_posters": rule.min_posters,
                    "best": verdict.best.as_ref().map(pair),
                    "held": verdict.held.iter().map(pair).collect::<Vec<_>>(),
                    "orders": orders,
                });
                (summary, record)
            }
        };
        report.error = failure;
        let mut guard = self.cycle.lock().await;
        if let Some(state) = guard.as_mut() {
            state.algo = Some(record);
            state.end_cycle = Some(EndCycle {
                action: if traded > 0 { "traded" } else { "hold" }.into(),
                summary,
            });
        }
        report
    }

    /// Install the snapshot as the active cycle.
    pub async fn begin_cycle(
        &self,
        cycle_id: &str,
        session_id: &str,
        snapshot: Snapshot,
    ) -> DecisionContext {
        let quotes = snapshot
            .context
            .candidate_routes
            .iter()
            .map(|c| (c.route_id.clone(), c.clone()))
            .collect();
        let context = snapshot.context.clone();
        *self.cycle.lock().await = Some(CycleState {
            cycle_id: cycle_id.to_owned(),
            session_id: session_id.to_owned(),
            context_hash: snapshot.context.context_hash(),
            view: snapshot.view,
            balances: snapshot.balances,
            context: snapshot.context,
            quotes,
            executions: Vec::new(),
            end_cycle: None,
            halted: None,
            telegram_sent_routes: Vec::new(),
            algo: None,
        });
        context
    }

    /// Close the active cycle and write its summary row (waits for an in-flight route).
    pub async fn finish_cycle(
        &self,
        report: &TurnReport,
    ) -> Result<(AuditRow, Vec<ExecutionSummary>)> {
        let state = self.cycle.lock().await.take();
        let Some(state) = state else {
            return Err(TraderError::config("no active cycle to finish"));
        };
        let status = summary_status(&state.executions);
        let end = state.end_cycle.clone();
        let reason = match (&end, &report.error) {
            (Some(end), _) => end.summary.clone(),
            (None, Some(error)) => format!("{error}; no further orders this cycle"),
            (None, None) => format!(
                "The bot ended the turn without end_cycle: {}",
                report.final_text.clone().unwrap_or_default()
            ),
        };
        let action = end.as_ref().map_or("hold", |e| e.action.as_str());
        let mut row = AuditRow::new(
            &self.config,
            RowType::CycleSummary,
            status,
            Decision::action(action, reason),
        );
        row.context_hash = state.context_hash.clone();
        row.candidate_route_count = state.context.candidate_routes.len();
        row.candidate_routes = state.context.candidate_contexts();
        row.cycle_id = state.cycle_id.clone();
        row.session_id = state.session_id.clone();
        row.error = report.error.clone().unwrap_or_default();
        row.agent = Some(AgentRecord {
            model: if report.model.is_empty() {
                self.config.model.clone()
            } else {
                report.model.clone()
            },
            thinking: if report.thinking.is_empty() {
                self.config.thinking.as_str().into()
            } else {
                report.thinking.clone()
            },
            turn_status: report.status.clone(),
            tool_calls: report.tool_calls.clone(),
            usage: report.usage.clone(),
            end_cycle: end,
            attempts: report.attempts.clone(),
            fallback_active: report.fallback_active,
        });
        row.telegram_sent_routes = Some(state.telegram_sent_routes.clone());
        row.run_config = self.run_config.get().cloned();
        row.schizo = match &state.context.schizo {
            Some(schizo) => Some(serde_json::to_value(schizo)?),
            None => None,
        };
        row.mdp = match &state.context.mdp {
            Some(mdp) => Some(serde_json::to_value(mdp)?),
            None => None,
        };
        row.biz = match &state.context.biz {
            Some(biz) => Some(serde_json::to_value(biz)?),
            None => None,
        };
        row.algo = state.algo.clone();
        let route_alerts = state.executions.len();
        row.telegram_sent = route_alerts > 0 && state.telegram_sent_routes.len() == route_alerts;
        // Durable first; the cycle's own alert (only when no route alerted) follows the write.
        self.audit.append(&row)?;
        // The mood the audit row records is the one the next cycle starts from.
        if let Some(schizo) = &state.context.schizo {
            let path = crate::schizo::state_path(&self.config.state_path);
            if let Err(error) = schizo.memory.save(&path) {
                tracing::error!(%error, path = %path.display(), "schizo memory not saved; the next cycle starts from the previous one");
            }
        }
        if route_alerts == 0 || report.error.is_some() {
            let sent = self.alerter.send(&row).await;
            row.telegram_sent = sent && (route_alerts == 0 || row.telegram_sent);
        }
        Ok((row, state.executions))
    }

    /// Record a cycle that never asked the bot (halt, cost cap, build failure).
    pub async fn record_skipped(
        &self,
        cycle_id: &str,
        status: RowStatus,
        reason: &str,
        error: &str,
    ) -> Result<AuditRow> {
        let mut row = AuditRow::new(
            &self.config,
            RowType::CycleSummary,
            status,
            Decision::hold(reason),
        );
        row.cycle_id = cycle_id.to_owned();
        row.error = error.to_owned();
        row.agent = Some(AgentRecord {
            model: self.config.model.clone(),
            thinking: self.config.thinking.as_str().into(),
            turn_status: "skipped".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
            end_cycle: None,
            attempts: Vec::new(),
            fallback_active: false,
        });
        row.run_config = self.run_config.get().cloned();
        // Durable first: the halt / cost-cap record must survive a crash during the alert.
        self.audit.append(&row)?;
        row.telegram_sent = self.alerter.send(&row).await;
        Ok(row)
    }

    /// Durably append a route row, then alert. The row is on disk (fsync'd) before any
    /// Telegram call, so a crash during the alert never loses a failed-route halt marker.
    /// Delivery is recorded on the cycle summary (`telegram_sent_routes`).
    async fn write_route_row(&self, state: &mut CycleState, mut row: AuditRow) -> Result<AuditRow> {
        row.context_hash = state.context_hash.clone();
        row.candidate_route_count = state.context.candidate_routes.len();
        row.cycle_id = state.cycle_id.clone();
        row.session_id = state.session_id.clone();
        self.audit.append(&row)?;
        state.executions.push(ExecutionSummary {
            route_id: row.decision.route_id.clone(),
            status: row.status.as_str().into(),
            error: row.error.clone(),
        });
        row.telegram_sent = self.alerter.send(&row).await;
        if row.telegram_sent {
            state
                .telegram_sent_routes
                .push(row.decision.route_id.clone());
        }
        Ok(row)
    }

    /// Audit a hard-safety refusal of a requested route and return the reason.
    async fn refuse(
        &self,
        state: &mut CycleState,
        candidate: &RouteCandidate,
        reason: &str,
        spend: Option<Decimal>,
        message: String,
    ) -> String {
        let status = match self.mode {
            CycleMode::PlanOnly => RowStatus::Blocked,
            CycleMode::Live => RowStatus::Failed,
        };
        let mut row = AuditRow::new(
            &self.config,
            RowType::Route,
            status,
            Decision::rebalance(candidate, reason, spend),
        );
        row.selected_route = candidate.as_context();
        row.error = message.clone();
        if let Err(error) = self.write_route_row(state, row).await {
            tracing::error!(%error, "failed to audit refused route");
        }
        message
    }

    /// The only order path. Plan-only: records `planned`. Live: runs the executor.
    pub async fn execute_route(
        &self,
        route_id: &str,
        reason: &str,
        spend: Option<Decimal>,
    ) -> std::result::Result<Value, String> {
        let mut guard = self.cycle.lock().await;
        let state = guard.as_mut().ok_or("no active cycle")?;
        let Some(quoted) = state.quotes.get(route_id).cloned() else {
            return Err(format!(
                "route_id {route_id} was not quoted this cycle; call list_routes for current route ids"
            ));
        };
        if self.shutting_down() {
            return Err("trader is shutting down; no new routes are accepted".into());
        }
        if let Some(halt) = &state.halted {
            return Err(format!("route execution is halted for this cycle: {halt}"));
        }
        if bb_core::fsutil::stop_requested(&self.config.stop_path) {
            let message = format!(
                "Bot Spot emergency stop is active: {}",
                self.config.stop_path.display()
            );
            return Err(self.refuse(state, &quoted, reason, spend, message).await);
        }
        let candidate = match self.refresh_quote(state, &quoted, spend).await {
            Ok(candidate) => candidate,
            Err(message) => return Err(message),
        };
        if let Some(scope) = self.config.scope.route_block_reason(&candidate.route) {
            return Err(self.refuse(state, &candidate, reason, spend, scope).await);
        }
        if candidate.route.legs.len() > self.config.max_route_legs {
            let message = "selected route exceeds configured max_route_legs".to_owned();
            return Err(self.refuse(state, &candidate, reason, spend, message).await);
        }
        match self.mode {
            CycleMode::PlanOnly => self.plan_route(state, &candidate, reason, spend).await,
            CycleMode::Live => self.live_route(state, &candidate, reason, spend).await,
        }
    }

    /// Re-quote when a spend amount is given or the quote / market view is stale.
    async fn refresh_quote(
        &self,
        state: &mut CycleState,
        quoted: &RouteCandidate,
        spend: Option<Decimal>,
    ) -> std::result::Result<RouteCandidate, String> {
        let stale = self.config.route_stale_seconds;
        let quote_age =
            (bb_core::time::now() - quoted.route.generated_at).num_milliseconds() as f64 / 1_000.0;
        let mut requote = spend.is_some() || quote_age > stale;
        if (requote || self.mode == CycleMode::Live) && state.view.age_seconds() > stale {
            state.view = self
                .load_view()
                .await
                .map_err(|e| format!("market refresh failed: {e}"))?;
            requote = true;
        }
        if !requote {
            return Ok(quoted.clone());
        }
        state.balances = self
            .balances()
            .await
            .map_err(|e| format!("balance read failed: {e}"))?;
        let source = &quoted.route.source_asset;
        let free = state.balances.free(source);
        let amount = match spend {
            Some(spend) if spend > free => {
                return Err(format!(
                    "spend_amount {spend} exceeds free {source} balance {free}"
                ));
            }
            Some(spend) => spend,
            None => free,
        };
        if amount <= Decimal::ZERO {
            return Err(format!("no free {source} balance"));
        }
        let fresh = quote_candidate(
            &state.view,
            &self.config,
            source,
            &quoted.route.destination_asset,
            amount,
            state.balances.free("BNB") > Decimal::ZERO,
        )
        .await
        .map_err(|e| format!("re-quote failed: {e}"))?
        .ok_or_else(|| {
            format!(
                "no executable route from {source} to {} for {amount}",
                quoted.route.destination_asset
            )
        })?;
        let mut fresh = RouteCandidate {
            market_evidence: quoted.market_evidence.clone(),
            social_signal_evidence: quoted.social_signal_evidence.clone(),
            ..fresh
        };
        if let Some(evidence) = &mut fresh.market_evidence {
            evidence.route_friction_bps = fresh.immediate_friction_bps.unwrap_or(0.0).max(0.0);
        }
        state.quotes.insert(fresh.route_id.clone(), fresh.clone());
        if fresh.route_id != quoted.route_id {
            return Err(format!(
                "route path changed since it was quoted: {} is now {} ({}); review it before executing",
                quoted.route_id,
                fresh.route_id,
                fresh.legs_label()
            ));
        }
        Ok(fresh)
    }

    async fn plan_route(
        &self,
        state: &mut CycleState,
        candidate: &RouteCandidate,
        reason: &str,
        spend: Option<Decimal>,
    ) -> std::result::Result<Value, String> {
        let mut row = AuditRow::new(
            &self.config,
            RowType::Route,
            RowStatus::Planned,
            Decision::rebalance(candidate, reason, spend),
        );
        row.selected_route = candidate.as_context();
        self.write_route_row(state, row)
            .await
            .map_err(|e| format!("planned route could not be audited: {e}"))?;
        Ok(json!({
            "status": "planned",
            "note": "plan-only cycle: route recorded, no order was placed",
            "route": route_summary(candidate),
        }))
    }

    async fn live_route(
        &self,
        state: &mut CycleState,
        candidate: &RouteCandidate,
        reason: &str,
        spend: Option<Decimal>,
    ) -> std::result::Result<Value, String> {
        if let Err(error) = self.config.check_live_arming() {
            return Err(self
                .refuse(state, candidate, reason, spend, error.to_string())
                .await);
        }
        if let Some(halt) = failed_route_halt(&self.audit.rows()) {
            return Err(self.refuse(state, candidate, reason, spend, halt).await);
        }
        let executor = Executor {
            exchange: &self.client,
            fees: &state.view.fees,
            config: &self.config,
            poll_interval: self.poll_interval,
        };
        let outcome = executor
            .execute(&candidate.route, &state.view.markets, spend)
            .await;
        let decision = Decision::rebalance(candidate, reason, spend);
        let (row, result) = match outcome {
            Ok(legs) => {
                let mut row =
                    AuditRow::new(&self.config, RowType::Route, RowStatus::Submitted, decision);
                row.submission_attempted = true;
                let result = Ok(
                    json!({"status": "submitted", "route": route_summary(candidate), "legs": leg_summaries(&legs)}),
                );
                row.submitted_legs = legs;
                (row, result)
            }
            Err(failure) => {
                let mut row =
                    AuditRow::new(&self.config, RowType::Route, RowStatus::Failed, decision);
                row.submission_attempted = failure.submission_attempted;
                row.error = failure.message.clone();
                if failure.submission_attempted {
                    state.halted = Some(failure.message.clone());
                }
                let result = Err(json!({
                    "status": "failed",
                    "error": failure.message,
                    "submission_attempted": failure.submission_attempted,
                    "legs": leg_summaries(&failure.legs),
                    "note": if failure.submission_attempted {
                        "live trading is halted until the operator reviews this route"
                    } else {
                        "no order was placed"
                    },
                })
                .to_string());
                row.submitted_legs = failure.legs;
                (row, result)
            }
        };
        let mut row = row;
        row.selected_route = candidate.as_context();
        if let Err(error) = self.write_route_row(state, row).await {
            tracing::error!(%error, "CRITICAL: executed route could not be audited");
            state.halted = Some(format!("audit write failed: {error}"));
        }
        // Balances moved: force fresh reads for later tools.
        if let Ok(balances) = self.balances().await {
            state.balances = balances;
        }
        result
    }

    /// Sell `share` (0..=1] of the free `asset` balance into USDT because an exit plan
    /// fired. Same hard safety as `execute_route`: live arming, the emergency stop file,
    /// the failed-route halt, operator scope, `order/test` per leg, exchange filters and
    /// fill reconciliation. Holds the cycle lock, so it never overlaps a bot route.
    pub async fn exit_sell(
        &self,
        asset: &str,
        share: Decimal,
        reason: &str,
    ) -> std::result::Result<Value, ExitSellError> {
        let _cycle = self.cycle.lock().await;
        if self.mode != CycleMode::Live {
            return Err(ExitSellError::refused(
                "plan-only mode: exit plans do not sell",
            ));
        }
        if self.shutting_down() {
            return Err(ExitSellError::refused("trader is shutting down"));
        }
        if bb_core::fsutil::stop_requested(&self.config.stop_path) {
            return Err(ExitSellError::refused(format!(
                "Bot Spot emergency stop is active: {}",
                self.config.stop_path.display()
            )));
        }
        self.config
            .check_live_arming()
            .map_err(|e| ExitSellError::refused(e.to_string()))?;
        if let Some(halt) = failed_route_halt(&self.audit.rows()) {
            return Err(ExitSellError::refused(halt));
        }
        let view = self
            .load_view()
            .await
            .map_err(|e| ExitSellError::refused(format!("market refresh failed: {e}")))?;
        let balances = self
            .balances()
            .await
            .map_err(|e| ExitSellError::refused(format!("balance read failed: {e}")))?;
        self.sell_into_usdt(&view, &balances, asset, share, reason)
            .await
    }

    /// The order half of [`Self::exit_sell`] on a given market view and balances. Every
    /// executor run is one durable route row (`cycle_id` `exit-plan`) before its alert.
    pub(crate) async fn sell_into_usdt(
        &self,
        view: &MarketView,
        balances: &Balances,
        asset: &str,
        share: Decimal,
        reason: &str,
    ) -> std::result::Result<Value, ExitSellError> {
        if share <= Decimal::ZERO || share > Decimal::ONE {
            return Err(ExitSellError::refused(format!(
                "exit share must be in (0, 1], got {share}"
            )));
        }
        let free = balances.free(asset);
        let amount = if share == Decimal::ONE {
            free
        } else {
            free * share
        };
        if amount <= Decimal::ZERO {
            return Err(ExitSellError::refused(format!("no free {asset} balance")));
        }
        let bnb_discount = balances.free("BNB") > Decimal::ZERO;
        let candidate = quote_candidate(view, &self.config, asset, EXIT_QUOTE, amount, bnb_discount)
            .await
            .map_err(|e| ExitSellError::refused(format!("quote failed: {e}")))?
            .ok_or_else(|| {
                ExitSellError::refused(format!(
                    "no executable route from {asset} to {EXIT_QUOTE} for {amount}"
                ))
            })?;
        let executor = Executor {
            exchange: &self.client,
            fees: &view.fees,
            config: &self.config,
            poll_interval: self.poll_interval,
        };
        let outcome = executor
            .execute(&candidate.route, &view.markets, Some(amount))
            .await;
        let decision = Decision::rebalance(&candidate, reason, Some(amount));
        let (mut row, result) = match outcome {
            Ok(legs) => {
                let mut row =
                    AuditRow::new(&self.config, RowType::Route, RowStatus::Submitted, decision);
                row.submission_attempted = true;
                let result = Ok(json!({
                    "status": "submitted",
                    "route": route_summary(&candidate),
                    "legs": leg_summaries(&legs),
                }));
                row.submitted_legs = legs;
                (row, result)
            }
            Err(failure) => {
                let mut row =
                    AuditRow::new(&self.config, RowType::Route, RowStatus::Failed, decision);
                row.submission_attempted = failure.submission_attempted;
                row.error = failure.message.clone();
                let result = Err(ExitSellError {
                    message: failure.message,
                    submission_attempted: failure.submission_attempted,
                });
                row.submitted_legs = failure.legs;
                (row, result)
            }
        };
        row.selected_route = candidate.as_context();
        row.cycle_id = EXIT_CYCLE_ID.into();
        if let Err(error) = self.audit.append(&row) {
            tracing::error!(%error, "CRITICAL: exit-plan sell could not be audited");
            return Err(ExitSellError {
                message: format!("exit-plan sell not audited: {error}"),
                submission_attempted: true,
            });
        }
        row.telegram_sent = self.alerter.send(&row).await;
        result
    }
}

/// Quote asset exit-plan sells go to.
pub const EXIT_QUOTE: &str = "USDT";
/// `cycle_id` of exit-plan route rows (they run between cycles).
pub const EXIT_CYCLE_ID: &str = "exit-plan";
/// `agent.model` of algo cycle summaries (Phase 51): no LLM answered.
pub const ALGO_MODEL: &str = "rule-based decider";
/// `session_id` of algo cycles (no omp session exists).
pub const ALGO_SESSION_ID: &str = "algo";

/// Why an exit-plan sell did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitSellError {
    pub message: String,
    /// An order may exist: live trading is halted until the operator reviews the route.
    pub submission_attempted: bool,
}

impl ExitSellError {
    fn refused(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            submission_attempted: false,
        }
    }
}

impl std::fmt::Display for ExitSellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Worst route status of the cycle (`held` when nothing was attempted).
fn summary_status(executions: &[ExecutionSummary]) -> RowStatus {
    let rank = |status: &str| match status {
        "failed" => 4,
        "submitted" => 3,
        "planned" => 2,
        "blocked" => 1,
        _ => 0,
    };
    match executions
        .iter()
        .map(|e| e.status.as_str())
        .max_by_key(|s| rank(s))
    {
        Some("failed") => RowStatus::Failed,
        Some("submitted") => RowStatus::Submitted,
        Some("planned") => RowStatus::Planned,
        Some("blocked") => RowStatus::Blocked,
        _ => RowStatus::Held,
    }
}

/// Compact route description for tool results.
pub fn route_summary(candidate: &RouteCandidate) -> Value {
    json!({
        "route_id": candidate.route_id,
        "source": candidate.route.source_asset,
        "destination": candidate.route.destination_asset,
        "input_amount": decimal_json(candidate.route.input_amount),
        "output_amount": decimal_json(candidate.route.output_amount),
        "source_value_usdt": candidate.source_value_usdt.map(decimal_json),
        "estimated_output_usdt": candidate.estimated_output_usdt.map(decimal_json),
        "friction_bps": candidate.immediate_friction_bps.map(|f| (f * 100.0).round() / 100.0),
        "expected_cost_bps": candidate.expected_cost_bps.map(|f| (f * 100.0).round() / 100.0),
        "liquidity_tier": candidate.liquidity_tier,
        "spread_bps": candidate.spread_bps.map(decimal_json),
        "pick_score": if candidate.market_pick_score.is_finite() { json!((candidate.market_pick_score * 100.0).round() / 100.0) } else { Value::Null },
        "legs": candidate.route.legs.iter().map(|leg| json!({
            "symbol": leg.symbol,
            "side": leg.side.as_str(),
            "input": decimal_json(leg.input_amount),
            "net_output": decimal_json(leg.net_output_amount),
            "fee_rate": decimal_json(leg.fee_rate),
        })).collect::<Vec<_>>(),
    })
}

fn leg_summaries(legs: &[LegResult]) -> Vec<Value> {
    legs.iter()
        .map(|leg| {
            json!({
                "symbol": leg.symbol,
                "side": leg.side,
                "order_amount": decimal_json(leg.order_amount),
                "status": leg.status,
                "filled": leg.final_order.get("filled"),
                "cost": leg.final_order.get("cost"),
                "source_free_after": decimal_json(leg.source_free_after),
                "destination_free_after": decimal_json(leg.destination_free_after),
            })
        })
        .collect()
}

/// Result of one trader cycle.
#[derive(Debug, Clone)]
pub struct CycleOutcome {
    pub summary: AuditRow,
    /// A live route failed or the cycle could not run (non-zero exit for one-shot runs).
    pub failed: bool,
}

/// The agentic trader: runtime + persistent omp session.
pub struct Trader {
    runtime: Arc<Runtime>,
    agent: Option<TraderAgent>,
}

impl Trader {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            agent: None,
        }
    }

    /// Use an already-attached agent (tests, custom launchers).
    pub fn with_agent(runtime: Arc<Runtime>, agent: TraderAgent) -> Self {
        Self {
            runtime,
            agent: Some(agent),
        }
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    async fn agent(&mut self) -> Result<&mut TraderAgent> {
        if self.agent.is_none() {
            let tools = Arc::new(TraderTools::new(Arc::clone(&self.runtime)));
            self.agent = Some(TraderAgent::start(&self.runtime.config, tools).await?);
        }
        self.agent
            .as_mut()
            .ok_or_else(|| TraderError::config("agent unavailable"))
    }

    /// One full cycle. Hard-safety refusals that precede any LLM call are errors.
    pub async fn run_cycle(&mut self, shutdown: &CancellationToken) -> Result<CycleOutcome> {
        let runtime = Arc::clone(&self.runtime);
        let config = Arc::clone(&runtime.config);
        if bb_core::fsutil::stop_requested(&config.stop_path) {
            return Err(TraderError::safety(format!(
                "Bot Spot emergency stop is active: {}",
                config.stop_path.display()
            )));
        }
        if runtime.mode == CycleMode::Live {
            config.check_live_arming()?;
        }
        let cycle_id = uuid::Uuid::now_v7().to_string();
        let mut state = AgentState::load(&config.state_path);
        let today = bb_core::time::utc_date(bb_core::time::now());

        if runtime.mode == CycleMode::Live
            && let Some(halt) = failed_route_halt(&runtime.audit.rows())
        {
            let summary = runtime
                .record_skipped(&cycle_id, RowStatus::Held, &halt, "")
                .await?;
            return Ok(CycleOutcome {
                summary,
                failed: false,
            });
        }
        if config.decider == crate::config::Decider::Algo {
            return self.run_algo_cycle(&cycle_id, state, shutdown).await;
        }
        let spent = state.cost_on(&today);
        if config.max_daily_llm_cost_usd > Decimal::ZERO && spent >= config.max_daily_llm_cost_usd {
            let reason = format!(
                "daily LLM cost cap reached ({spent} / {} USD); the model not asked until the next UTC day",
                config.max_daily_llm_cost_usd
            );
            let summary = runtime
                .record_skipped(&cycle_id, RowStatus::Held, &reason, "")
                .await?;
            return Ok(CycleOutcome {
                summary,
                failed: false,
            });
        }

        let failure_status = match runtime.mode {
            CycleMode::PlanOnly => RowStatus::Blocked,
            CycleMode::Live => RowStatus::Failed,
        };
        let session_id = match self.agent().await {
            Ok(agent) => agent.session_id.clone(),
            Err(error) => {
                let message = format!("omp agent start failed: {error}");
                runtime
                    .record_skipped(&cycle_id, failure_status, &message, &message)
                    .await?;
                return Err(error);
            }
        };
        let snapshot = match runtime.build_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let message = error.to_string();
                runtime
                    .record_skipped(&cycle_id, failure_status, &message, &message)
                    .await?;
                return Err(error);
            }
        };
        let context = runtime.begin_cycle(&cycle_id, &session_id, snapshot).await;
        let journal = runtime
            .journal
            .read(config.journal_prompt_entries)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "journal unreadable");
                Vec::new()
            });
        let mut prompt = cycle_prompt(
            &config,
            runtime.mode,
            state.cycle_count + 1,
            &cycle_id,
            &context,
            state.last_cycle.as_ref(),
            &journal,
        );
        prompt.push_str(&crate::exit_book::prompt_section(
            &config.exit_paths,
            bb_core::time::now_ms(),
            EXIT_WATCHER_STALE_MS,
            config.exit.live,
            context.trade_record.as_ref().map(|record| &record.by_plan),
        ));
        let agent = self.agent().await?;
        agent.prepare_cycle().await;
        let report = agent.run_turn(&prompt, shutdown).await;
        let notices = agent.take_notices();
        if shutdown.is_cancelled() {
            runtime.begin_shutdown();
        }
        let (summary, executions) = runtime.finish_cycle(&report).await?;

        state.cycle_count += 1;
        state.add_cost(&today, report.usage.cost_usd);
        state.session_id = Some(session_id);
        state.last_cycle = Some(LastCycle {
            cycle_id: cycle_id.clone(),
            finished_at: summary.timestamp.clone(),
            status: summary.status.as_str().into(),
            turn_status: report.status.clone(),
            end_cycle: summary.agent.as_ref().and_then(|a| a.end_cycle.clone()),
            executions,
            error: summary.error.clone(),
        });
        let brain_down =
            state.record_brain(report.all_models_failed, config.brain_down_alert_cycles);
        if let Err(error) = state.save(&config.state_path) {
            tracing::warn!(%error, "agent state not saved");
        }
        // Observability only, after the durable audit row: model switches and brain outages.
        for notice in &notices {
            runtime.alerter.send_notice(notice).await;
        }
        if brain_down {
            tracing::error!(
                cycles = state.brain_down_cycles,
                "trader brain unavailable on every model"
            );
            runtime
                .alerter
                .send_notice(&format!(
                    "{BRAIN_UNAVAILABLE} for {} consecutive cycles; no orders are being placed. Last error: {}",
                    state.brain_down_cycles,
                    summary.error
                ))
                .await;
        }
        let failed = summary.status == RowStatus::Failed && runtime.mode == CycleMode::Live;
        Ok(CycleOutcome { summary, failed })
    }

    /// One cycle decided by the rule-based decider (`BOT_SPOT_DECIDER=algo`): no omp session
    /// is started and no LLM cost accrues. The snapshot, the hard safety of `execute_route`,
    /// the route rows and the cycle summary are the LLM cycle's.
    async fn run_algo_cycle(
        &self,
        cycle_id: &str,
        mut state: AgentState,
        shutdown: &CancellationToken,
    ) -> Result<CycleOutcome> {
        let runtime = Arc::clone(&self.runtime);
        let config = Arc::clone(&runtime.config);
        let rule = config
            .algo_rule
            .ok_or_else(|| TraderError::config("BOT_SPOT_DECIDER=algo without BOT_SPOT_ALGO_WEIGHTS"))?;
        let snapshot = match runtime.build_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let status = match runtime.mode {
                    CycleMode::PlanOnly => RowStatus::Blocked,
                    CycleMode::Live => RowStatus::Failed,
                };
                let message = error.to_string();
                runtime
                    .record_skipped(cycle_id, status, &message, &message)
                    .await?;
                return Err(error);
            }
        };
        let verdict = runtime.algo_verdict(&snapshot, &rule);
        runtime.begin_cycle(cycle_id, ALGO_SESSION_ID, snapshot).await;
        let report = runtime.run_algo_turn(verdict, &rule, shutdown).await;
        if shutdown.is_cancelled() {
            runtime.begin_shutdown();
        }
        let (summary, executions) = runtime.finish_cycle(&report).await?;
        state.cycle_count += 1;
        state.last_cycle = Some(LastCycle {
            cycle_id: cycle_id.to_owned(),
            finished_at: summary.timestamp.clone(),
            status: summary.status.as_str().into(),
            turn_status: report.status.clone(),
            end_cycle: summary.agent.as_ref().and_then(|a| a.end_cycle.clone()),
            executions,
            error: summary.error.clone(),
        });
        if let Err(error) = state.save(&config.state_path) {
            tracing::warn!(%error, "agent state not saved");
        }
        let failed = summary.status == RowStatus::Failed && runtime.mode == CycleMode::Live;
        Ok(CycleOutcome { summary, failed })
    }

    /// Close the omp session cleanly.
    pub async fn close(self) {
        if let Some(agent) = self.agent {
            agent.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bb_omp::HostToolHandler;
    use rust_decimal_macros::dec;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::tools::TraderTools;
    use crate::universe::tests::{config, view};

    fn trader_config(dir: &Path, extra: &[(&str, &str)]) -> TraderConfig {
        let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let (audit, journal, state, stop) = (
            p("audit.jsonl"),
            p("journal.jsonl"),
            p("state.json"),
            p("stop"),
        );
        let (plans, watch_state, watch_log) = (
            p("exit-plans.json"),
            p("exit-watch-state.json"),
            p("exit-watch.jsonl"),
        );
        let mut pairs = vec![
            ("BOT_SPOT_AUDIT_PATH", audit.as_str()),
            ("BOT_SPOT_JOURNAL_PATH", journal.as_str()),
            ("BOT_SPOT_STATE_PATH", state.as_str()),
            ("BOT_SPOT_STOP_PATH", stop.as_str()),
            ("BOT_SPOT_EXIT_PLANS_PATH", plans.as_str()),
            ("BOT_SPOT_EXIT_WATCH_STATE_PATH", watch_state.as_str()),
            ("BOT_SPOT_EXIT_WATCH_LOG_PATH", watch_log.as_str()),
            ("BOT_SPOT_CANDIDATE_LIMIT", "0"),
        ];
        pairs.extend_from_slice(extra);
        config(&pairs)
    }

    async fn runtime(server: &MockServer, config: TraderConfig, mode: CycleMode) -> Arc<Runtime> {
        runtime_with(server, config, mode, Alerter::new(None, true, 0)).await
    }

    async fn runtime_with(
        server: &MockServer,
        config: TraderConfig,
        mode: CycleMode,
        alerter: Alerter,
    ) -> Arc<Runtime> {
        let client = BinanceClient::new(
            bb_binance::BinanceConfig::new(false)
                .with_credentials(bb_binance::Credentials::new("key", "secret").unwrap())
                .with_spot_rest_url(server.uri())
                .with_retries(0, Duration::from_millis(1), Duration::from_millis(1)),
        )
        .unwrap();
        let config = Arc::new(config);
        let runtime = Arc::new(Runtime::new(
            Arc::clone(&config),
            mode,
            client,
            alerter,
            None,
        ));
        let view = Arc::new(view(&config));
        let balances = Balances {
            free: [("USDT".to_owned(), dec!(100))].into(),
            used: Default::default(),
            total: [("USDT".to_owned(), dec!(100))].into(),
        };
        let candidates = build_route_candidates(&view, &config, &balances.free)
            .await
            .unwrap();
        let context = DecisionContext {
            timestamp: crate::audit::utc_timestamp(),
            prompt_version: config.prompt_version.clone(),
            market_pick_mode: config.market_pick_mode.as_str(),
            portfolio_value_usdt: dec!(100),
            balances: balances.clone(),
            candidate_routes: candidates,
            execution_invariants: execution_invariants(&config),
            route_scope: route_scope(&config),
            trade_memory: runtime.trade_memory(&balances, &view, 8),
            trade_record: None,
            market_mood: None,
            gcp2: None,
            schizo: None,
            mdp: None,
            biz: None,
        };
        runtime
            .begin_cycle(
                "cycle-1",
                "session-1",
                Snapshot {
                    context,
                    view,
                    balances,
                    step: None,
                },
            )
            .await;
        runtime
    }

    async fn first_route(runtime: &Runtime, destination: &str) -> String {
        let guard = runtime.cycle.lock().await;
        guard
            .as_ref()
            .unwrap()
            .context
            .candidate_routes
            .iter()
            .find(|c| c.route.destination_asset == destination)
            .unwrap()
            .route_id
            .clone()
    }

    fn audit_rows(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn order_calls(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path().starts_with("/api/v3/order"))
            .count()
    }

    #[tokio::test]
    async fn plan_only_execute_route_records_planned_and_never_calls_order_endpoints() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"serverTime": bb_core::time::now_ms()})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "canTrade": true,
                "balances": [{"asset": "USDT", "free": "100", "locked": "0"}]
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, trader_config(dir.path(), &[]), CycleMode::PlanOnly).await;
        let route_id = first_route(&runtime, "SOL").await;

        let plan = runtime
            .execute_route(&route_id, "SOL relative strength breakout", None)
            .await
            .unwrap();
        assert_eq!(plan["status"], "planned");
        assert_eq!(plan["route"]["legs"].as_array().unwrap().len(), 2);
        // Scaled plan: re-quoted for the spend amount (reads balances, still no order endpoint).
        let scaled = runtime
            .execute_route(&route_id, "half size", Some(dec!(50)))
            .await
            .unwrap();
        assert_eq!(scaled["route"]["input_amount"], json!(50));
        assert_eq!(
            order_calls(&server).await,
            0,
            "plan-only never touches /api/v3/order*"
        );

        let report = TurnReport {
            status: "completed".into(),
            ..TurnReport::default()
        };
        let (summary, executions) = runtime.finish_cycle(&report).await.unwrap();
        assert_eq!(summary.status, RowStatus::Planned);
        assert_eq!(executions.len(), 2);
        let rows = audit_rows(dir.path());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["status"], "planned");
        assert_eq!(rows[0]["row_type"], "route");
        assert_eq!(rows[0]["submission_attempted"], false);
        assert_eq!(rows[0]["decision"]["action"], "rebalance");
        assert_eq!(rows[0]["decision"]["spend_mode"], "all_available");
        assert_eq!(rows[0]["selected_route"]["route_id"], json!(route_id));
        assert_eq!(rows[0]["cycle_id"], "cycle-1");
        assert_eq!(rows[0]["session_id"], "session-1");
        assert_eq!(
            rows[0]["audit_path"],
            json!(dir.path().join("audit.jsonl").display().to_string())
        );
        assert_eq!(rows[1]["decision"]["spend_mode"], "amount");
        assert_eq!(rows[1]["decision"]["spend_amount"], json!(50));
        assert_eq!(rows[2]["row_type"], "cycle_summary");
        assert_eq!(
            rows[2]["candidate_route_count"],
            rows[2]["candidate_routes"].as_array().unwrap().len()
        );
    }

    #[tokio::test]
    async fn the_algo_turn_sends_its_orders_through_execute_route_and_stops_at_a_failure() {
        use crate::decider::{Action, Verdict};
        let server = MockServer::start().await;
        let algo = [
            ("BOT_SPOT_DECIDER", "algo"),
            ("BOT_SPOT_ALGO_WEIGHTS", "0,0,1,0,0.5,0"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let config = trader_config(dir.path(), &algo);
        let rule = config.algo_rule.unwrap();
        let first = runtime(&server, config, CycleMode::PlanOnly).await;
        let buy = Verdict {
            actions: vec![Action::Buy {
                asset: "SOL".into(),
                reason: "SOL score +0.600 ≥ buy level +0.5".into(),
            }],
            best: Some(("SOL".into(), 0.6)),
            held: Vec::new(),
        };
        let report = first
            .run_algo_turn(Ok(buy), &rule, &CancellationToken::new())
            .await;
        assert_eq!((report.error.as_deref(), report.model.as_str()), (None, ALGO_MODEL));
        let (summary, _) = first.finish_cycle(&report).await.unwrap();
        assert_eq!(summary.status, RowStatus::Planned);
        assert_eq!(order_calls(&server).await, 0);
        let rows = audit_rows(dir.path());
        assert_eq!(rows[0]["decision"]["destination_asset"], "SOL");
        assert_eq!(rows[0]["decision"]["reason"], "SOL score +0.600 ≥ buy level +0.5");
        assert_eq!(rows[0]["decider"], "algo");
        assert_eq!(rows[1]["agent"]["end_cycle"]["action"], "traded");
        assert_eq!(rows[1]["algo"]["best"]["asset"], "SOL");
        assert_eq!(rows[1]["agent"]["model"], ALGO_MODEL);
        assert_eq!(rows[1]["session_id"], "session-1");

        // A sell that cannot be quoted stops the turn: no buy of a second coin.
        let dir = tempfile::tempdir().unwrap();
        let second = runtime(&server, trader_config(dir.path(), &algo), CycleMode::PlanOnly).await;
        let rotate = Verdict {
            actions: vec![
                Action::Sell {
                    asset: "BTC".into(),
                    reason: "BTC score −0.5".into(),
                },
                Action::Buy {
                    asset: "SOL".into(),
                    reason: "SOL score +0.6".into(),
                },
            ],
            best: Some(("SOL".into(), 0.6)),
            held: vec![("BTC".into(), -0.5)],
        };
        let report = second
            .run_algo_turn(Ok(rotate), &rule, &CancellationToken::new())
            .await;
        assert!(report.error.as_deref().unwrap().contains("no free BTC balance"), "{report:?}");
        assert_eq!(report.tool_calls.len(), 1);
        let (summary, _) = second.finish_cycle(&report).await.unwrap();
        assert_eq!(summary.status, RowStatus::Held);
        assert_eq!(audit_rows(dir.path()).len(), 1);
    }

    #[tokio::test]
    async fn the_algo_view_is_the_feed_coins_plus_held_coins_without_a_breadth_read() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        let now = bb_core::time::now_ms();
        let newest = now - now.rem_euclid(60_000) - 60_000;
        // 61 closed bars rising 0.1 per minute from 100, plus the still-open minute.
        let klines: Vec<Value> = (0..=61)
            .map(|i| {
                let open = newest - (60 - i) * 60_000;
                let close = 100.0 + 0.1 * i as f64;
                json!([open, "1", "1", "1", close.to_string(), "1", open + 59_999, "1", 1, "1", "1", "0"])
            })
            .collect();
        for symbol in ["ETHUSDT", "BTCUSDT"] {
            Mock::given(method("GET"))
                .and(path("/api/v3/klines"))
                .and(query_param("symbol", symbol))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!(klines)))
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let algo = [("BOT_SPOT_DECIDER", "algo"), ("BOT_SPOT_ALGO_WEIGHTS", "0,0,1,0,0.5,0")];
        let config = trader_config(dir.path(), &algo);
        let view = view(&config);
        let runtime = runtime(&server, config, CycleMode::PlanOnly).await;
        let balances = Balances {
            free: [("USDT".to_owned(), dec!(10)), ("BTC".to_owned(), dec!(0.01))].into(),
            ..Balances::default()
        };
        let window = crate::biz::Window { mentions: 5, ..Default::default() };
        let windows = crate::biz::CoinWindows { h6: window, h24: window, ..Default::default() };
        let feed = crate::biz::BizFeed {
            as_of: "now".into(),
            age_minutes: 1,
            model: String::new(),
            prompt_version: String::new(),
            // SOL has no USDT market and USDC is cash-like: neither may be bought.
            coins: [("ETH", windows), ("SOL", windows), ("USDC", windows)]
                .map(|(a, w)| (a.to_owned(), w))
                .into(),
            dropped: Vec::new(),
        };
        let step = runtime
            .algo_step(&view, &balances, dec!(610), &crate::biz::BizNow::Fresh(Box::new(feed)))
            .await;
        assert_eq!(step.liquid_top, vec!["ETH"]);
        assert_eq!(step.coins.keys().collect::<Vec<_>>(), vec!["BTC", "ETH"]);
        let eth = step.coin("ETH").unwrap();
        assert!((eth.ret_1h.unwrap() - (106.0 / 100.0 - 1.0) * 100.0).abs() < 1e-9, "{eth:?}");
        assert_eq!(step.score, None);
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests.iter().all(|r| r.url.path() == "/api/v3/klines"), "no breadth or ticker read");

        // Without a fresh feed only the held coin is read and nothing is a buy candidate.
        let unavailable = crate::biz::BizNow::Unavailable { reason: "stale".into() };
        let step = runtime.algo_step(&view, &balances, dec!(610), &unavailable).await;
        assert!(step.liquid_top.is_empty());
        assert_eq!(step.coins.keys().collect::<Vec<_>>(), vec!["BTC"]);
    }

    #[tokio::test]
    async fn live_route_refused_by_stop_file_is_audited_without_orders() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let config = trader_config(
            dir.path(),
            &[
                ("BOT_SPOT_ENABLED", "true"),
                ("BOT_SPOT_DYNAMIC_SCOPE", "all_eligible_spot"),
                ("USE_TESTNET", "false"),
                ("DRY_RUN", "false"),
                ("CONFIRM_LIVE_TRADING", "yes"),
            ],
        );
        let runtime = runtime(&server, config, CycleMode::Live).await;
        std::fs::write(dir.path().join("stop"), "").unwrap();
        let route_id = first_route(&runtime, "SOL").await;
        let error = runtime.execute_route(&route_id, "x", None).await.unwrap_err();
        assert!(error.contains("emergency stop"), "{error}");
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
        let rows = audit_rows(dir.path());
        assert_eq!(rows[0]["status"], "failed");
        assert_eq!(rows[0]["submission_attempted"], false);
    }

    #[tokio::test]
    async fn execute_route_refuses_unquoted_routes_and_routes_outside_a_cycle() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, trader_config(dir.path(), &[]), CycleMode::PlanOnly).await;
        let error = runtime.execute_route("A->B:1", "x", None).await.unwrap_err();
        assert!(error.contains("was not quoted this cycle"), "{error}");
        let route_id = first_route(&runtime, "SOL").await;
        runtime.finish_cycle(&TurnReport::default()).await.unwrap();
        let error = runtime.execute_route(&route_id, "x", None).await.unwrap_err();
        assert!(error.contains("no active cycle"), "{error}");
        assert!(server.received_requests().await.unwrap_or_default().is_empty());
        assert_eq!(audit_rows(dir.path()).len(), 1, "only the cycle summary");
    }

    #[tokio::test]
    async fn tool_argument_validation_returns_errors() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, trader_config(dir.path(), &[]), CycleMode::PlanOnly).await;
        let tools = TraderTools::new(Arc::clone(&runtime));
        let cases = [
            (
                "execute_route",
                json!({"reason": "x"}),
                "missing field `route_id`",
            ),
            (
                "execute_route",
                json!({"route_id": "A->B:1", "reason": "x", "extra": 1}),
                "unknown field",
            ),
            (
                "execute_route",
                json!({"route_id": " ", "reason": "x"}),
                "route_id must not be blank",
            ),
            (
                "execute_route",
                json!({"route_id": "A->B:1", "reason": " "}),
                "reason must state",
            ),
            (
                "execute_route",
                json!({"route_id": "A->B:1", "reason": "x", "spend_amount": -1}),
                "positive number",
            ),
            (
                "execute_route",
                json!({"route_id": "A->B:1", "reason": "x", "spend_amount": "lots"}),
                "positive number",
            ),
            ("list_routes", json!({"limit": 0}), "at least 1"),
            ("list_routes", json!({"limit": 51}), "at most 50"),
            (
                "list_routes",
                json!({"destination": "EUR"}),
                "blocked as fiat",
            ),
            ("get_market_evidence", json!({"assets": []}), "1-12 assets"),
            (
                "get_market_evidence",
                json!({"assets": ["B@D"]}),
                "invalid asset",
            ),
            (
                "get_klines",
                json!({"symbol": "BTC/USDT", "interval": "7m"}),
                "interval must be one of",
            ),
            (
                "get_klines",
                json!({"symbol": "BTC/USDT", "interval": "1h", "limit": 501}),
                "between 1 and 500",
            ),
            ("get_trade_history", json!({"limit": 99}), "at most 50"),
            ("journal_write", json!({"text": "  "}), "must not be blank"),
            (
                "end_cycle",
                json!({"action": "yolo", "summary": "x"}),
                "hold or traded",
            ),
            (
                "end_cycle",
                json!({"action": "hold", "summary": ""}),
                "summary must not be blank",
            ),
            ("get_portfolio", json!({"x": 1}), "unknown field"),
            ("transfer_funds", json!({}), "unknown tool"),
        ];
        for (name, args, expected) in cases {
            let error = tools.dispatch(name, args.clone()).await.unwrap_err();
            assert!(error.contains(expected), "{name} {args}: {error}");
        }
        let outcome = tools
            .call("transfer_funds", Value::Null, CancellationToken::new())
            .await;
        assert!(outcome.is_error);
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );

        // Social disabled, journal round trip, trade history and end_cycle succeed offline.
        let social = tools
            .dispatch("get_social_evidence", json!({"assets": ["sol"]}))
            .await
            .unwrap();
        assert_eq!(social["SOL"]["status"], "disabled");
        tools
            .dispatch("journal_write", json!({"text": "SOL thesis: breakout"}))
            .await
            .unwrap();
        let journal = tools.dispatch("journal_read", json!({})).await.unwrap();
        assert_eq!(journal["entries"][0]["text"], "SOL thesis: breakout");
        assert_eq!(journal["entries"][0]["cycle_id"], "cycle-1");
        let history = tools
            .dispatch("get_trade_history", Value::Null)
            .await
            .unwrap();
        assert_eq!(history["status"], "empty");

        runtime.finish_cycle(&TurnReport::default()).await.unwrap();
        let error = tools
            .dispatch("end_cycle", json!({"action": "hold", "summary": "x"}))
            .await
            .unwrap_err();
        assert!(error.contains("no active cycle"));
    }

    #[tokio::test]
    async fn set_exit_plan_follows_the_open_position_and_clears() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, trader_config(dir.path(), &[]), CycleMode::PlanOnly).await;
        let tools = TraderTools::new(Arc::clone(&runtime));
        let config = &runtime.config;

        let error = tools
            .dispatch("set_exit_plan", json!({"asset": "ena", "trail_pct": 2}))
            .await
            .unwrap_err();
        assert!(error.contains("audit unreadable"), "{error}");

        let buy = json!({
            "row_type": "route",
            "status": "submitted",
            "selected_route": {"legs": [{"fee_rate": 0.001}]},
            "submitted_legs": [{
                "symbol": "ENA/USDT",
                "side": "buy",
                "final_order": {"average": 0.1983, "filled": 75.11, "timestamp": 1_791_990_000_000_i64}
            }]
        });
        // A position bought on a non-USDT market, in the older row schema (no row_type).
        let bnb_buy = json!({
            "status": "submitted",
            "selected_route": {"legs": [{"fee_rate": 0.001}], "source_value_usdt": 12.5},
            "submitted_legs": [{
                "symbol": "SOL/BNB",
                "side": "buy",
                "final_order": {"average": 0.25, "filled": 0.05, "timestamp": 1_791_990_100_000_i64}
            }]
        });
        std::fs::write(&config.audit_path, format!("{buy}\n{bnb_buy}\n")).unwrap();
        let error = tools
            .dispatch("set_exit_plan", json!({"asset": "DOGE", "trail_pct": 2}))
            .await
            .unwrap_err();
        assert!(error.contains("no open DOGE position"), "{error}");
        let sol = tools
            .dispatch("set_exit_plan", json!({"asset": "sol", "trail_pct": 2}))
            .await
            .unwrap();
        assert_eq!(sol["symbol"], "SOL/BNB");
        let error = tools
            .dispatch("set_exit_plan", json!({"asset": "ENA"}))
            .await
            .unwrap_err();
        assert!(error.contains("at least one rule"), "{error}");

        let set = tools
            .dispatch(
                "set_exit_plan",
                json!({"asset": "ena", "atr_multiple": 3, "atr_timeframe": "15m", "deadline_minutes": 240}),
            )
            .await
            .unwrap();
        assert_eq!(set["plan"], "atr=3:15m,deadline=240");
        assert_eq!(set["paper_only"], true);
        let book = crate::exit_book::PlanBook::load(&config.exit_paths.plans).unwrap();
        let entry = &book.plans["ENA"];
        assert_eq!(
            (entry.symbol.as_str(), entry.entry_ms, entry.entry_price),
            ("ENA/USDT", 1_791_990_000_000, Decimal::new(1983, 4))
        );

        let cleared = tools
            .dispatch("set_exit_plan", json!({"asset": "ENA", "clear": true}))
            .await
            .unwrap();
        assert_eq!(cleared["cleared"], true);
        let book = crate::exit_book::PlanBook::load(&config.exit_paths.plans).unwrap();
        assert_eq!(book.plans.keys().collect::<Vec<_>>(), ["SOL"]);
    }

    /// Alert sink that snapshots the audit file at the moment it is invoked.
    #[derive(Debug)]
    struct AuditSnooper {
        audit: std::path::PathBuf,
        seen: parking_lot::Mutex<Vec<Vec<Value>>>,
    }

    #[async_trait::async_trait]
    impl crate::alerts::AlertSink for AuditSnooper {
        async fn send(&self, _html: &str) -> bool {
            let rows = std::fs::read_to_string(&self.audit)
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            self.seen.lock().push(rows);
            true
        }
    }

    #[tokio::test]
    async fn failed_live_route_is_durable_before_the_alert_is_sent() {
        let server = MockServer::start().await;
        let account = json!({"canTrade": true, "balances": [{"asset": "USDT", "free": "100", "locked": "0"}]});
        for (verb, route, response) in [
            (
                "GET",
                "/api/v3/time",
                ResponseTemplate::new(200)
                    .set_body_json(json!({"serverTime": bb_core::time::now_ms()})),
            ),
            (
                "GET",
                "/api/v3/account",
                ResponseTemplate::new(200).set_body_json(account),
            ),
            (
                "POST",
                "/api/v3/order/test",
                ResponseTemplate::new(200).set_body_json(json!({})),
            ),
            // Ambiguous 5xx on the live order: an order may exist, so the route counts as attempted.
            (
                "POST",
                "/api/v3/order",
                ResponseTemplate::new(503).set_body_string("unavailable"),
            ),
        ] {
            Mock::given(method(verb))
                .and(path(route))
                .respond_with(response)
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = trader_config(
            dir.path(),
            &[
                ("BOT_SPOT_ENABLED", "true"),
                ("BOT_SPOT_DYNAMIC_SCOPE", "all_eligible_spot"),
                ("USE_TESTNET", "false"),
                ("DRY_RUN", "false"),
                ("CONFIRM_LIVE_TRADING", "yes"),
            ],
        );
        let snooper = Arc::new(AuditSnooper {
            audit: dir.path().join("audit.jsonl"),
            seen: parking_lot::Mutex::new(Vec::new()),
        });
        let alerter = Alerter::new(
            Some(Arc::clone(&snooper) as Arc<dyn crate::alerts::AlertSink>),
            true,
            0,
        );
        let runtime = runtime_with(&server, config, CycleMode::Live, alerter).await;
        let route_id = first_route(&runtime, "SOL").await;

        let error = runtime
            .execute_route(&route_id, "test", None)
            .await
            .unwrap_err();
        assert!(error.contains("order failed for ETH/USDT"), "{error}");
        let seen = snooper.seen.lock().clone();
        assert_eq!(seen.len(), 1, "one alert for the failed route");
        let on_disk = seen[0]
            .last()
            .expect("failed row already on disk when the alert ran");
        assert_eq!(on_disk["status"], "failed");
        assert_eq!(on_disk["submission_attempted"], true);
        assert_eq!(on_disk["telegram_sent"], false);
        assert_eq!(
            on_disk["audit_path"],
            json!(dir.path().join("audit.jsonl").display().to_string())
        );
        assert!(
            failed_route_halt(&runtime.audit.rows()).is_some(),
            "halt marker is durable"
        );

        let (summary, _) = runtime.finish_cycle(&TurnReport::default()).await.unwrap();
        assert_eq!(summary.status, RowStatus::Failed);
        let rows = audit_rows(dir.path());
        assert_eq!(
            rows.last().unwrap()["telegram_sent_routes"],
            json!([route_id])
        );
        assert_eq!(rows.last().unwrap()["telegram_sent"], true);
        assert_eq!(
            snooper.seen.lock().len(),
            1,
            "no extra cycle alert after a route alert"
        );
    }

    fn live_pairs() -> [(&'static str, &'static str); 5] {
        [
            ("BOT_SPOT_ENABLED", "true"),
            ("BOT_SPOT_DYNAMIC_SCOPE", "all_eligible_spot"),
            ("USE_TESTNET", "false"),
            ("DRY_RUN", "false"),
            ("CONFIRM_LIVE_TRADING", "yes"),
        ]
    }

    #[tokio::test]
    async fn exit_sell_refuses_without_orders_in_plan_only_or_under_the_stop_file() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let plan_only = runtime(
            &server,
            trader_config(dir.path(), &live_pairs()),
            CycleMode::PlanOnly,
        )
        .await;
        let error = plan_only
            .exit_sell("ETH", Decimal::ONE, "test")
            .await
            .unwrap_err();
        assert!(error.message.contains("plan-only"), "{error}");
        assert!(!error.submission_attempted);

        let live = runtime(
            &server,
            trader_config(dir.path(), &live_pairs()),
            CycleMode::Live,
        )
        .await;
        std::fs::write(dir.path().join("stop"), "").unwrap();
        let error = live
            .exit_sell("ETH", Decimal::ONE, "test")
            .await
            .unwrap_err();
        assert!(error.message.contains("emergency stop"), "{error}");
        assert_eq!(order_calls(&server).await, 0);
        assert!(audit_rows(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn ambiguous_exit_sell_is_audited_and_halts_further_exit_sells() {
        let server = MockServer::start().await;
        let account = json!({"canTrade": true, "balances": [{"asset": "ETH", "free": "0.01", "locked": "0"}]});
        for (verb, route, response) in [
            (
                "GET",
                "/api/v3/time",
                ResponseTemplate::new(200)
                    .set_body_json(json!({"serverTime": bb_core::time::now_ms()})),
            ),
            (
                "GET",
                "/api/v3/account",
                ResponseTemplate::new(200).set_body_json(account),
            ),
            (
                "POST",
                "/api/v3/order/test",
                ResponseTemplate::new(200).set_body_json(json!({})),
            ),
            (
                "POST",
                "/api/v3/order",
                ResponseTemplate::new(503).set_body_string("unavailable"),
            ),
        ] {
            Mock::given(method(verb))
                .and(path(route))
                .respond_with(response)
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = trader_config(dir.path(), &live_pairs());
        let market_view = view(&config);
        let runtime = runtime(&server, config, CycleMode::Live).await;
        let balances = Balances {
            free: [("ETH".to_owned(), dec!(0.01))].into(),
            used: Default::default(),
            total: [("ETH".to_owned(), dec!(0.01))].into(),
        };

        let error = runtime
            .sell_into_usdt(
                &market_view,
                &balances,
                "ETH",
                Decimal::ONE,
                "exit plan test fired",
            )
            .await
            .unwrap_err();
        assert!(error.submission_attempted, "{error}");
        let rows = audit_rows(dir.path());
        let row = rows.last().unwrap();
        assert_eq!(row["status"], "failed");
        assert_eq!(row["cycle_id"], EXIT_CYCLE_ID);
        assert_eq!(row["decision"]["source_asset"], "ETH");
        assert_eq!(row["decision"]["destination_asset"], "USDT");
        assert_eq!(row["decision"]["reason"], "exit plan test fired");

        // The failed-route halt now stops every further exit sell before any order.
        let calls = order_calls(&server).await;
        let error = runtime
            .exit_sell("ETH", Decimal::ONE, "again")
            .await
            .unwrap_err();
        assert!(error.message.contains("review"), "{error}");
        assert_eq!(order_calls(&server).await, calls);
    }

    #[tokio::test]
    async fn cycle_summary_records_the_producing_model_and_failed_attempts() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, trader_config(dir.path(), &[]), CycleMode::PlanOnly).await;
        let attempt = |model: &str, status: &str| crate::audit::AttemptEntry {
            model: model.into(),
            status: status.into(),
            error: "429 usage limit reached".into(),
        };
        let report = TurnReport {
            status: "error".into(),
            error: Some(format!(
                "{BRAIN_UNAVAILABLE} (no orders placed): anthropic/claude-haiku-5-5 error: 429; openai-codex/gpt-6.1-sol timeout: The bot timed out after 300s"
            )),
            model: "openai-codex/gpt-6.1-sol".into(),
            thinking: "low".into(),
            attempts: vec![attempt("anthropic/claude-haiku-5-5", "error")],
            fallback_active: true,
            all_models_failed: Some(true),
            ..TurnReport::default()
        };
        let (summary, _) = runtime.finish_cycle(&report).await.unwrap();
        assert_eq!(summary.status, RowStatus::Held);
        assert!(crate::alerts::eli5(&summary).contains("unavailable on every configured model"));
        runtime
            .record_skipped("cycle-2", RowStatus::Held, "halt", "")
            .await
            .unwrap();
        let rows = audit_rows(dir.path());
        let agent = &rows[0]["agent"];
        assert_eq!(agent["model"], "openai-codex/gpt-6.1-sol");
        assert_eq!(agent["thinking"], "low");
        assert_eq!(agent["fallback_active"], true);
        assert_eq!(agent["attempts"][0]["model"], "anthropic/claude-haiku-5-5");
        assert_eq!(agent["attempts"][0]["status"], "error");
        assert!(
            rows[0]["decision"]["reason"]
                .as_str()
                .unwrap()
                .starts_with(BRAIN_UNAVAILABLE)
        );
        // Rows without a failover keep the previous shape.
        let skipped = rows[1]["agent"].as_object().unwrap();
        assert_eq!(skipped["model"], "anthropic/claude-haiku-5-5");
        assert!(!skipped.contains_key("attempts") && !skipped.contains_key("fallback_active"));
    }

    #[tokio::test]
    async fn cycle_summary_records_the_schizo_state_and_saves_its_memory() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(&server, trader_config(dir.path(), &[]), CycleMode::PlanOnly).await;
        let zones = crate::gcp2::parse_color_zones(
            &json!({"colorzones": [
                {"label": "Normal", "range_start": 0, "range_end": 140},
                {"label": "High", "range_start": 140, "range_end": 10000}
            ]}),
            "t".into(),
        )
        .unwrap();
        let view = crate::gcp2::Gcp2View::from_reading(
            crate::gcp2::Gcp2Reading {
                end_s: 1_792_487_579,
                units: 1_691_705,
            },
            &zones,
        );
        let book = crate::book::Book {
            t: 0,
            cash: 100.0,
            equity: 100.0,
            holdings: Default::default(),
        };
        let state = crate::schizo::SchizoState::next(
            &Default::default(),
            Some(&view),
            None,
            &book,
            "now".into(),
        )
        .unwrap();
        runtime.cycle.lock().await.as_mut().unwrap().context.schizo = Some(state.clone());
        runtime.finish_cycle(&TurnReport::default()).await.unwrap();
        runtime
            .record_skipped("cycle-2", RowStatus::Held, "halt", "")
            .await
            .unwrap();
        let rows = audit_rows(dir.path());
        let schizo = &rows[0]["schizo"];
        assert_eq!(schizo["mood"], json!(state.mood));
        assert_eq!(schizo["mood_step"], "initial");
        assert_eq!(schizo["gcp2"]["value"], 169.1705);
        assert_eq!(schizo["gcp2"]["zone"], "High");
        assert_eq!(schizo["dice"], json!(view.dice));
        assert!(rows[1].get("schizo").is_none());
        let saved = crate::schizo::SchizoMemory::load(&dir.path().join(crate::schizo::STATE_FILE))
            .unwrap();
        assert_eq!(saved, state.memory);
    }

    #[test]
    fn summary_status_is_the_most_significant_route_status() {
        let execution = |status: &str| ExecutionSummary {
            route_id: "r".into(),
            status: status.into(),
            error: String::new(),
        };
        assert_eq!(summary_status(&[]), RowStatus::Held);
        assert_eq!(
            summary_status(&[execution("planned"), execution("blocked")]),
            RowStatus::Planned
        );
        assert_eq!(
            summary_status(&[execution("submitted"), execution("failed")]),
            RowStatus::Failed
        );
    }
}

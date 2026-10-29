//! Cycle runtime: the per-cycle market snapshot, route
//! planning/execution with audit rows, and the [`Trader`] cycle the algorithm decides.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bb_binance::BinanceClient;
use tokio_util::sync::CancellationToken;
use bb_pool::CommissionService;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::audit::{AuditLog, AuditRow, Decision, EndCycle, RowStatus, RowType, failed_route_halt};
use crate::config::TraderConfig;
use crate::context::{DecisionContext, decimal_json, execution_invariants, route_scope};
use crate::error::{Result, TraderError};
use crate::executor::{Executor, LegResult};
use crate::state::{ExecutionSummary, LastCycle, TraderState};
use crate::memory::TradeMemory;
use crate::universe::{
    Balances, MarketView, RouteCandidate, build_route_candidates, quote_candidate,
};

/// 1m bars read per coin for the algorithm's 15m/1h returns: 61 closed bars plus the open one.
const ALGO_PRICE_MINUTES: u32 = 62;

/// What one cycle's orders produced.
#[derive(Debug, Clone, Default)]
pub struct TurnReport {
    /// `completed` (the rule's orders ran or none were asked for).
    pub status: String,
    /// The first failed order (the remaining ones were not sent), or shutdown.
    pub error: Option<String>,
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
    /// Algo cycles (Phase 51): the rule's verdict, recorded on the cycle summary.
    pub algo: Option<Value>,
}

/// Market snapshot built for a cycle (or `--evidence-only`).
#[derive(Debug)]
pub struct Snapshot {
    pub context: DecisionContext,
    pub view: Arc<MarketView>,
    pub balances: Balances,
    /// The rule's point-in-time market view (`None` when the breadth read failed or
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
    ) -> Self {
        Self {
            audit: AuditLog::new(&config.audit_path),
            mode,
            client,
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

    /// Catalog, balances, candidate routes, trade memory, the /biz/ feed and the
    /// algorithm's market view.
    pub async fn build_snapshot(&self) -> Result<Snapshot> {
        let balances = self.balances().await?;
        let view = self.load_view().await?;
        let candidates = build_route_candidates(&view, &self.config, &balances.free).await?;
        let trade_memory = self.trade_memory(&balances, &view, self.config.trade_memory_limit);
        let portfolio_value_usdt = balances.free_value_usdt(&view);
        let biz = self.biz_feed(&view);
        let step = Some(self.algo_step(&view, &balances, portfolio_value_usdt, &biz).await);
        let biz = Some(biz);
        let context = DecisionContext {
            timestamp: crate::audit::utc_timestamp(),
            rules_version: self.config.rules_version.clone(),
            market_pick_mode: self.config.market_pick_mode.as_str(),
            portfolio_value_usdt,
            balances: balances.clone(),
            candidate_routes: candidates,
            execution_invariants: execution_invariants(&self.config),
            route_scope: route_scope(&self.config),
            trade_memory,
            market_mood: None,
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
                        && !crate::assets::is_low_upside(asset)
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
        let Some(step) = &snapshot.step else {
            return Err("no market view this cycle".into());
        };
        let view = &snapshot.view;
        let book = crate::book::live_book(
            step.t,
            &snapshot.balances,
            snapshot.context.portfolio_value_usdt,
            |asset, amount| view.usdt_value(asset, amount),
            |asset| view.markets.get(&format!("{asset}/USDT")).map(|m| m.min_notional),
        );
        let biz = match &snapshot.context.biz {
            Some(crate::biz::BizNow::Fresh(feed)) => Some(&feed.coins),
            _ => None,
        };
        Ok(crate::decider::decide(
            &crate::decider::Inputs {
                snap: step,
                book: &book,
                biz,
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
            error: None,
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
                    let result = match self.quote_for_cycle(source, destination).await {
                        Ok(route_id) => self.execute_route(&route_id, reason, None).await,
                        Err(message) => Err(message),
                    };
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
            (None, None) => "the cycle ended without a verdict".to_owned(),
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
        row.run_config = self.run_config.get().cloned();
        row.biz = match &state.context.biz {
            Some(biz) => Some(serde_json::to_value(biz)?),
            None => None,
        };
        row.algo = state.algo.clone();
        self.audit.append(&row)?;
        Ok((row, state.executions))
    }

    /// Record a cycle the rule never decided (halt, snapshot failure).
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
        row.run_config = self.run_config.get().cloned();
        self.audit.append(&row)?;
        Ok(row)
    }

    /// Durably append (fsync'd) a route row, so a crash right after never loses a
    /// failed-route halt marker.
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
    /// executor run is one durable route row (`cycle_id` `exit-plan`).
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
        result
    }
}

/// Quote asset exit-plan sells go to.
pub const EXIT_QUOTE: &str = "USDT";
/// `cycle_id` of exit-plan route rows (they run between cycles).
pub const EXIT_CYCLE_ID: &str = "exit-plan";
/// `session_id` of algorithm cycles.
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

/// The trader: the runtime plus the operator's rule (the algorithm decides; no LLM).
pub struct Trader {
    runtime: Arc<Runtime>,
}

impl Trader {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    /// One full cycle decided by the algorithm ([`crate::decider`]). Hard-safety refusals
    /// before any order are errors.
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
        let mut state = TraderState::load(&config.state_path);

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
        let rule = config.algo_rule;
        let snapshot = match runtime.build_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let status = match runtime.mode {
                    CycleMode::PlanOnly => RowStatus::Blocked,
                    CycleMode::Live => RowStatus::Failed,
                };
                let message = error.to_string();
                runtime
                    .record_skipped(&cycle_id, status, &message, &message)
                    .await?;
                return Err(error);
            }
        };
        let verdict = runtime.algo_verdict(&snapshot, &rule);
        runtime.begin_cycle(&cycle_id, ALGO_SESSION_ID, snapshot).await;
        let report = runtime.run_algo_turn(verdict, &rule, shutdown).await;
        if shutdown.is_cancelled() {
            runtime.begin_shutdown();
        }
        let (summary, executions) = runtime.finish_cycle(&report).await?;
        state.cycle_count += 1;
        state.last_cycle = Some(LastCycle {
            cycle_id: cycle_id.clone(),
            finished_at: summary.timestamp.clone(),
            status: summary.status.as_str().into(),
            end_cycle: Some(EndCycle {
                action: summary.decision.action.clone(),
                summary: summary.decision.reason.clone(),
            }),
            executions,
            error: summary.error.clone(),
        });
        if let Err(error) = state.save(&config.state_path) {
            tracing::warn!(%error, "trader state not saved");
        }
        let failed = summary.status == RowStatus::Failed && runtime.mode == CycleMode::Live;
        Ok(CycleOutcome { summary, failed })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rust_decimal_macros::dec;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::universe::tests::{config, view};

    fn trader_config(dir: &Path, extra: &[(&str, &str)]) -> TraderConfig {
        let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let (audit, state, stop) = (p("audit.jsonl"), p("state.json"), p("stop"));
        let (plans, watch_state, watch_log) = (
            p("exit-plans.json"),
            p("exit-watch-state.json"),
            p("exit-watch.jsonl"),
        );
        let mut pairs = vec![
            ("BOT_SPOT_AUDIT_PATH", audit.as_str()),
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
            rules_version: config.rules_version.clone(),
            market_pick_mode: config.market_pick_mode.as_str(),
            portfolio_value_usdt: dec!(100),
            balances: balances.clone(),
            candidate_routes: candidates,
            execution_invariants: execution_invariants(&config),
            route_scope: route_scope(&config),
            trade_memory: runtime.trade_memory(&balances, &view, 8),
            market_mood: None,
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
        let algo = [("BOT_SPOT_ALGO_WEIGHTS", "0,0,1,0.5,0")];
        let dir = tempfile::tempdir().unwrap();
        let config = trader_config(dir.path(), &algo);
        let rule = config.algo_rule;
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
        assert_eq!(report.error.as_deref(), None);
        let (summary, _) = first.finish_cycle(&report).await.unwrap();
        assert_eq!(summary.status, RowStatus::Planned);
        assert_eq!(order_calls(&server).await, 0);
        let rows = audit_rows(dir.path());
        assert_eq!(rows[0]["decision"]["destination_asset"], "SOL");
        assert_eq!(rows[0]["decision"]["reason"], "SOL score +0.600 ≥ buy level +0.5");
        assert_eq!(rows[1]["decision"]["action"], "traded");
        assert_eq!(rows[1]["algo"]["best"]["asset"], "SOL");
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
        assert_eq!(order_calls(&server).await, 0);
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
        let algo = [("BOT_SPOT_ALGO_WEIGHTS", "0,0,1,0.5,0")];
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
    async fn a_failed_live_route_is_on_disk_and_halts_before_execute_route_returns() {
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
        let runtime = runtime(&server, config, CycleMode::Live).await;
        let route_id = first_route(&runtime, "SOL").await;

        let error = runtime
            .execute_route(&route_id, "test", None)
            .await
            .unwrap_err();
        assert!(error.contains("order failed for ETH/USDT"), "{error}");
        // The row is fsync'd before execute_route returns: read the file, not the runtime.
        let rows = audit_rows(dir.path());
        let on_disk = rows.last().expect("failed row on disk");
        assert_eq!(on_disk["status"], "failed");
        assert_eq!(on_disk["submission_attempted"], true);
        assert_eq!(
            on_disk["audit_path"],
            json!(dir.path().join("audit.jsonl").display().to_string())
        );
        assert!(
            failed_route_halt(&runtime.audit.rows()).is_some(),
            "halt marker is durable"
        );

        // A second route in the same cycle is refused without another order call.
        let orders = order_calls(&server).await;
        let again = runtime.execute_route(&route_id, "again", None).await.unwrap_err();
        assert!(again.contains("halted"), "{again}");
        assert_eq!(order_calls(&server).await, orders);

        let (summary, _) = runtime.finish_cycle(&TurnReport::default()).await.unwrap();
        assert_eq!(summary.status, RowStatus::Failed);
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

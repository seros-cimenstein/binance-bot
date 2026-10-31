//! The nonstop trader (operator decisions 2026-10-29, `docs/decisions.md`): one tick every
//! [`TICK`] instead of a fixed cycle.
//!
//! Each tick:
//! 1. The emergency stop file is checked first: while it exists the tick reads and decides
//!    nothing (one log line when the stop starts and when it ends).
//! 2. Slow state (the market catalog with its fees, and the wallet balances) is reloaded
//!    every [`SLOW_REFRESH_MS`] and on the tick after every executed route; a failed reload
//!    keeps the previous state.
//! 3. Per-tick inputs: the tags feed file and one all-symbols `ticker/price` request
//!    (weight 4). Each coin's minute closes come from those polls, seeded once from 1m
//!    klines, so its 15m/1h moves are the trader's own (no breadth read).
//! 4. The operator's rule ([`crate::decider`]) decides. A decision row (the cycle summary,
//!    after the orders it asks for) is written only when the decided action changes, and
//!    once an hour as a heartbeat ([`HEARTBEAT_MS`]); an unchanged tick writes nothing and
//!    sends no order. Orders go through `execute_route` with every hard-safety check.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tokio_util::sync::CancellationToken;

use crate::audit::{AuditRow, EndCycle, RowStatus, failed_route_halt};
use crate::book::{CoinView, coin_view_from_minutes, live_book};
use crate::context::{DecisionContext, execution_invariants, route_scope};
use crate::cycle::{ALGO_SESSION_ID, CycleMode, Runtime, Snapshot};
use crate::decider::{Action, Inputs, Verdict, decide};
use crate::error::{Result, TraderError};
use crate::state::{LastCycle, TraderState};
use crate::tags::TagsNow;
use crate::universe::{Balances, MarketView, build_route_candidates};

/// Price poll and decision interval (operator decision 2026-10-29: REST polling every 15 s).
pub const TICK: Duration = Duration::from_secs(15);
/// Catalog, fees and balances are reloaded this often (and after every executed route).
pub const SLOW_REFRESH_MS: i64 = 5 * 60_000;
/// An unchanged decision is still recorded this often (operator decision 2026-10-29).
pub const HEARTBEAT_MS: i64 = 60 * 60_000;
/// A 24h ticker change older than this (catalog reloads failing) is unknown, not reused.
pub const TICKER_24H_MAX_AGE_MS: i64 = 2 * SLOW_REFRESH_MS;
/// 1m klines read to seed a coin: 61 closed minutes plus the forming one.
const SEED_MINUTES: u32 = 62;
const MINUTE_MS: i64 = 60_000;
/// Minute closes kept per coin (the 1h move needs the minute 60 minutes back).
const KEEP_MINUTES: i64 = 70;
/// 1h klines read and kept per coin: the 7d move needs the close 168 hours back.
pub const HOUR_BARS: u32 = 200;
const HOUR_MS: i64 = 3_600_000;
/// The 7d move compares the newest price with the hourly close this long before it.
const WEEK_MS: i64 = 7 * 24 * HOUR_MS;
/// The decision key of a tick that could not read the market at all.
const NO_MARKET_KEY: &str = "market data unavailable";

/// Each coin's minute closes: seeded from 1m klines, then the newest ticker price of each
/// minute.
#[derive(Debug, Default)]
pub struct PriceBook {
    minutes: HashMap<String, BTreeMap<i64, f64>>,
}

impl PriceBook {
    /// Whether `asset` needs its history read from klines: it has no minute before the
    /// current one, or a gap of more than two minutes before it (missed polls).
    pub fn needs_seed(&self, asset: &str, now_ms: i64) -> bool {
        let current = floor_minute(now_ms);
        self.minutes
            .get(asset)
            .and_then(|m| m.range(..current).next_back())
            .is_none_or(|(&open, _)| open < current - 2 * MINUTE_MS)
    }

    /// Closed 1m bars `(open ms, close)`.
    pub fn seed(&mut self, asset: &str, bars: impl IntoIterator<Item = (i64, f64)>, now_ms: i64) {
        let minutes = self.minutes.entry(asset.to_owned()).or_default();
        minutes.extend(bars.into_iter().filter(|(_, close)| *close > 0.0));
        prune(minutes, now_ms);
    }

    /// The price polled at `now_ms` becomes the close of its minute.
    pub fn record(&mut self, asset: &str, now_ms: i64, price: f64) {
        if price > 0.0 {
            let minutes = self.minutes.entry(asset.to_owned()).or_default();
            minutes.insert(floor_minute(now_ms), price);
            prune(minutes, now_ms);
        }
    }

    pub fn coin_view(&self, asset: &str) -> Option<CoinView> {
        let bars: Vec<(i64, f64)> = self
            .minutes
            .get(asset)?
            .iter()
            .map(|(t, c)| (*t, *c))
            .collect();
        coin_view_from_minutes(&bars)
    }

    /// The newest minute close of `asset`.
    pub fn last_close(&self, asset: &str) -> Option<f64> {
        self.minutes.get(asset)?.values().next_back().copied()
    }
}

/// Each coin's closed 1h bars (newest [`HOUR_BARS`]), read from 1h klines at most once per
/// clock hour per coin, for the 7d move. A failed read keeps the bars it had and is tried
/// again the next hour; coins not read for two hours are forgotten.
#[derive(Debug, Default)]
pub struct HourBook {
    coins: HashMap<String, HourBars>,
}

#[derive(Debug, Default)]
struct HourBars {
    /// When the last read (successful or not) was attempted.
    read_ms: i64,
    /// Close by bar close time (open + 1h).
    closes: BTreeMap<i64, f64>,
}

impl HourBook {
    /// Whether `asset` is due a read: never read, or last read in an earlier clock hour.
    pub fn due(&self, asset: &str, now_ms: i64) -> bool {
        self.coins
            .get(asset)
            .is_none_or(|bars| floor_hour(bars.read_ms) < floor_hour(now_ms))
    }

    /// Record a read at `now_ms`: `bars` are `(open ms, close)` of closed 1h bars (`None`
    /// when the read failed). Keeps the newest [`HOUR_BARS`] bars.
    pub fn store(
        &mut self,
        asset: &str,
        bars: Option<impl IntoIterator<Item = (i64, f64)>>,
        now_ms: i64,
    ) {
        let entry = self.coins.entry(asset.to_owned()).or_default();
        entry.read_ms = now_ms;
        if let Some(bars) = bars {
            entry.closes.extend(
                bars.into_iter()
                    .filter(|(open, close)| *close > 0.0 && open + HOUR_MS <= now_ms)
                    .map(|(open, close)| (open + HOUR_MS, close)),
            );
            while entry.closes.len() > HOUR_BARS as usize {
                entry.closes.pop_first();
            }
        }
    }

    /// Drop coins not read for two hours (no longer candidates or held).
    pub fn forget_unread(&mut self, now_ms: i64) {
        self.coins.retain(|_, bars| now_ms - bars.read_ms < 2 * HOUR_MS);
    }

    /// Percent move from the newest hourly close at or before `now_ms` − 7d to `price`;
    /// `None` when that close is missing or more than an hour older than 7d.
    pub fn ret_7d(&self, asset: &str, now_ms: i64, price: f64) -> Option<f64> {
        let target = now_ms - WEEK_MS;
        let (&closed, &then) = self.coins.get(asset)?.closes.range(..=target).next_back()?;
        (target - closed < HOUR_MS && then > 0.0).then(|| (price / then - 1.0) * 100.0)
    }

    pub fn bars(&self, asset: &str) -> usize {
        self.coins.get(asset).map_or(0, |b| b.closes.len())
    }
}

fn floor_hour(ms: i64) -> i64 {
    ms - ms.rem_euclid(HOUR_MS)
}

fn floor_minute(ms: i64) -> i64 {
    ms - ms.rem_euclid(MINUTE_MS)
}

fn prune(minutes: &mut BTreeMap<i64, f64>, now_ms: i64) {
    let oldest = floor_minute(now_ms) - KEEP_MINUTES * MINUTE_MS;
    *minutes = minutes.split_off(&oldest);
}

/// Catalog, fees and balances, reloaded on the slow timer and after every executed route.
#[derive(Debug)]
struct Slow {
    view: Arc<MarketView>,
    balances: Balances,
    loaded_ms: i64,
}

/// What a tick did.
#[derive(Debug)]
pub enum Tick {
    /// The emergency stop file exists: nothing read, decided or sent.
    Stopped,
    /// The decision is the one already recorded (or the price poll failed): no row, no order.
    Unchanged,
    /// A decision row was written (after its orders, if any).
    Recorded(Box<AuditRow>),
}

/// The rule's verdict on one tick's inputs.
struct Decision {
    verdict: Verdict,
    key: String,
    tags: TagsNow,
    equity: Decimal,
    /// Every held coin and buy candidate with its score terms, highest score first
    /// (recorded on the decision row as `algo.coins`).
    coins: Vec<serde_json::Value>,
}

/// The nonstop trader: the runtime plus the state kept between ticks.
pub struct Nonstop {
    runtime: Arc<Runtime>,
    slow: Option<Slow>,
    refresh_slow: bool,
    prices: PriceBook,
    hours: HourBook,
    /// The last recorded decision key and when it was recorded.
    last: Option<(String, i64)>,
    stopped: bool,
}

impl Nonstop {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            slow: None,
            refresh_slow: false,
            prices: PriceBook::default(),
            hours: HourBook::default(),
            last: None,
            stopped: false,
        }
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    /// Tick every [`TICK`] until `shutdown`; a tick error is logged and the next tick runs.
    pub async fn run(&mut self, shutdown: &CancellationToken) {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !shutdown.is_cancelled() {
            tokio::select! {
                _ = interval.tick() => {}
                () = shutdown.cancelled() => break,
            }
            match self.tick(bb_core::time::now_ms(), false, shutdown).await {
                Ok(Tick::Recorded(row)) => match serde_json::to_string(&row) {
                    Ok(line) => println!("{line}"),
                    Err(error) => tracing::warn!(%error, "decision row not printed"),
                },
                Ok(Tick::Stopped | Tick::Unchanged) => {}
                Err(error) => tracing::error!(%error, "tick failed"),
            }
        }
    }

    /// One decision, recorded whatever the last one was (`bb bot-spot` without `--nonstop`).
    pub async fn once(&mut self, shutdown: &CancellationToken) -> Result<AuditRow> {
        match self.tick(bb_core::time::now_ms(), true, shutdown).await? {
            Tick::Recorded(row) => Ok(*row),
            Tick::Stopped => Err(TraderError::safety(format!(
                "Bot Spot emergency stop is active: {}",
                self.runtime.config.stop_path.display()
            ))),
            Tick::Unchanged => Err(TraderError::config("no Binance prices: nothing decided")),
        }
    }

    /// The decision context the next tick would record, with the routes quotable from the
    /// wallet now (`--evidence-only`; no row, no order).
    pub async fn evidence(&mut self) -> Result<DecisionContext> {
        let now_ms = bb_core::time::now_ms();
        self.refresh(now_ms).await?;
        let (tags, candidates, held) = self.inputs()?;
        self.poll_prices(now_ms, &candidates, &held).await;
        let decision = self.decide(now_ms, tags, candidates, &held)?;
        let slow = self.slow_state()?;
        let mut context = self.context(slow, &decision);
        context.candidate_routes =
            build_route_candidates(&slow.view, &self.runtime.config, &slow.balances.free).await?;
        Ok(context)
    }

    /// One tick at `now_ms` (see the module docs). `force` records the decision even when it
    /// is unchanged.
    pub async fn tick(
        &mut self,
        now_ms: i64,
        force: bool,
        shutdown: &CancellationToken,
    ) -> Result<Tick> {
        let config = Arc::clone(&self.runtime.config);
        if bb_core::fsutil::stop_requested(&config.stop_path) {
            if !self.stopped {
                tracing::warn!(path = %config.stop_path.display(), "emergency stop active: no decisions until the stop file is removed");
            }
            self.stopped = true;
            return Ok(Tick::Stopped);
        }
        if self.stopped {
            tracing::info!("emergency stop removed: deciding again");
            self.stopped = false;
        }
        if self.runtime.mode == CycleMode::Live {
            config.check_live_arming()?;
        }
        if let Err(error) = self.refresh(now_ms).await {
            // No market view at all yet: record that (on change) and give up this tick.
            if self.due(NO_MARKET_KEY, now_ms, force) {
                let status = match self.runtime.mode {
                    CycleMode::PlanOnly => RowStatus::Blocked,
                    CycleMode::Live => RowStatus::Failed,
                };
                let message = error.to_string();
                let cycle_id = uuid::Uuid::now_v7().to_string();
                let row = self
                    .runtime
                    .record_skipped(&cycle_id, status, &message, &message)
                    .await?;
                self.remember(NO_MARKET_KEY.to_owned(), now_ms, &row);
            }
            return Err(error);
        }
        let (tags, candidates, held) = self.inputs()?;
        if !self.poll_prices(now_ms, &candidates, &held).await && !force {
            return Ok(Tick::Unchanged);
        }
        let decision = self.decide(now_ms, tags, candidates, &held)?;
        if !self.due(&decision.key, now_ms, force) {
            return Ok(Tick::Unchanged);
        }
        let cycle_id = uuid::Uuid::now_v7().to_string();
        if self.runtime.mode == CycleMode::Live
            && let Some(halt) = failed_route_halt(&self.runtime.audit.rows())
        {
            let row = self
                .runtime
                .record_skipped(&cycle_id, RowStatus::Held, &halt, "")
                .await?;
            self.remember(decision.key, now_ms, &row);
            return Ok(Tick::Recorded(Box::new(row)));
        }
        let slow = self.slow_state()?;
        let snapshot = Snapshot {
            context: self.context(slow, &decision),
            view: Arc::clone(&slow.view),
            balances: slow.balances.clone(),
        };
        let rule = config.algo_rule;
        self.runtime
            .begin_cycle(&cycle_id, ALGO_SESSION_ID, snapshot)
            .await;
        let report = self
            .runtime
            .run_algo_turn(decision.verdict, &rule, decision.coins, shutdown)
            .await;
        if shutdown.is_cancelled() {
            self.runtime.begin_shutdown();
        }
        let (row, executions) = self.runtime.finish_cycle(&report).await?;
        if !executions.is_empty() {
            // Balances moved (or may have): reload them before the next decision.
            self.refresh_slow = true;
        }
        self.remember(decision.key, now_ms, &row);
        tracing::info!(
            action = %row.decision.action,
            reason = %row.decision.reason,
            used_weight_1m = ?self.runtime.client.used_weight_1m(),
            "decision recorded"
        );
        Ok(Tick::Recorded(Box::new(row)))
    }

    /// Whether a decision with `key` is written now: it differs from the last recorded one,
    /// the heartbeat is due, or `force`.
    fn due(&self, key: &str, now_ms: i64, force: bool) -> bool {
        force
            || self
                .last
                .as_ref()
                .is_none_or(|(last, at)| last != key || now_ms - at >= HEARTBEAT_MS)
    }

    fn remember(&mut self, key: String, now_ms: i64, row: &AuditRow) {
        self.last = Some((key, now_ms));
        let state_path = &self.runtime.config.state_path;
        let mut state = TraderState::load(state_path);
        state.cycle_count += 1;
        state.last_cycle = Some(LastCycle {
            cycle_id: row.cycle_id.clone(),
            finished_at: row.timestamp.clone(),
            status: row.status.as_str().into(),
            end_cycle: Some(EndCycle {
                action: row.decision.action.clone(),
                summary: row.decision.reason.clone(),
            }),
            executions: Vec::new(),
            error: row.error.clone(),
        });
        if let Err(error) = state.save(state_path) {
            tracing::warn!(%error, "trader state not saved");
        }
    }

    fn slow_state(&self) -> Result<&Slow> {
        self.slow
            .as_ref()
            .ok_or_else(|| TraderError::config("no market view loaded"))
    }

    /// Reload the slow state when it is missing, old, or an executed route moved balances.
    /// Balances and the catalog reload independently; a failed part keeps its previous value
    /// (logged) and is tried again on the slow timer. `Err` only when there is no state yet.
    async fn refresh(&mut self, now_ms: i64) -> Result<()> {
        let due = self.refresh_slow
            || self
                .slow
                .as_ref()
                .is_none_or(|slow| now_ms - slow.loaded_ms >= SLOW_REFRESH_MS);
        if !due {
            return Ok(());
        }
        let balances = self.runtime.balances().await;
        let view = self.runtime.load_view().await;
        let slow = match (balances, view, self.slow.take()) {
            (Ok(balances), Ok(view), _) => Slow {
                view,
                balances,
                loaded_ms: now_ms,
            },
            (balances, view, Some(old)) => {
                if let Err(error) = &balances {
                    tracing::warn!(%error, "balance reload failed; keeping the previous balances");
                }
                if let Err(error) = &view {
                    tracing::warn!(%error, "catalog reload failed; keeping the previous catalog");
                }
                Slow {
                    view: view.unwrap_or(old.view),
                    balances: balances.unwrap_or(old.balances),
                    loaded_ms: now_ms,
                }
            }
            (Err(error), _, None) | (_, Err(error), None) => return Err(error),
        };
        self.slow = Some(slow);
        self.refresh_slow = false;
        Ok(())
    }

    /// This tick's tags feed, the coins the rule may buy (the fresh feed's asset tags with an
    /// eligible USDT market, cash-like coins left out) and the coins it holds.
    fn inputs(&self) -> Result<(TagsNow, Vec<String>, BTreeSet<String>)> {
        let slow = self.slow_state()?;
        let tags = self.runtime.tags_feed(&slow.view);
        let usdt_market = |asset: &str| {
            slow.view
                .markets
                .get(&format!("{asset}/USDT"))
                .is_some_and(|market| market.eligible)
        };
        let candidates = match &tags {
            TagsNow::Fresh(feed) => feed
                .assets
                .keys()
                .filter(|asset| {
                    asset.as_str() != "USDT"
                        && !crate::assets::is_low_upside(asset)
                        && usdt_market(asset)
                })
                .cloned()
                .collect(),
            TagsNow::Unavailable { .. } => Vec::new(),
        };
        let held = self
            .book(slow, 0)
            .holdings
            .into_keys()
            .filter(|asset| usdt_market(asset))
            .collect();
        Ok((tags, candidates, held))
    }

    fn book(&self, slow: &Slow, t: i64) -> crate::book::Book {
        let view = &slow.view;
        live_book(
            t,
            &slow.balances,
            slow.balances.free_value_usdt(view),
            |asset, amount| view.usdt_value(asset, amount),
            |asset| {
                view.markets
                    .get(&format!("{asset}/USDT"))
                    .map(|m| m.min_notional)
            },
        )
    }

    /// Poll every price once and record the coins of interest; seed the ones without recent
    /// minutes from 1m klines. Returns false when the poll failed.
    async fn poll_prices(
        &mut self,
        now_ms: i64,
        candidates: &[String],
        held: &BTreeSet<String>,
    ) -> bool {
        let assets: BTreeSet<&String> = candidates.iter().chain(held).collect();
        let polled = match self.runtime.client.prices().await {
            Ok(tickers) => {
                let prices: HashMap<&str, f64> = tickers
                    .iter()
                    .filter_map(|t| Some((t.symbol.as_str(), t.price.to_f64()?)))
                    .collect();
                for asset in &assets {
                    if let Some(price) = prices.get(format!("{asset}USDT").as_str()) {
                        self.prices.record(asset, now_ms, *price);
                    }
                }
                true
            }
            Err(error) => {
                tracing::warn!(%error, "price poll failed; nothing decided this tick");
                false
            }
        };
        tracing::debug!(
            coins = assets.len(),
            used_weight_1m = ?self.runtime.client.used_weight_1m(),
            "price poll"
        );
        for asset in &assets {
            if !self.prices.needs_seed(asset, now_ms) {
                continue;
            }
            let symbol = format!("{asset}/USDT");
            match self
                .runtime
                .client
                .klines(&symbol, "1m", None, None, Some(SEED_MINUTES))
                .await
            {
                Ok(klines) => {
                    let bars = klines
                        .iter()
                        .filter(|k| k.close_time.is_some_and(|close| close < now_ms))
                        .filter_map(|k| Some((k.open_time, k.close.to_f64()?)));
                    self.prices.seed(asset, bars, now_ms);
                }
                Err(error) => {
                    tracing::warn!(%error, %symbol, "price history read failed; coin left out this tick");
                }
            }
        }
        // 1h klines for the 7d move: at most one read per coin per clock hour (weight 2).
        self.hours.forget_unread(now_ms);
        for asset in &assets {
            if !self.hours.due(asset, now_ms) {
                continue;
            }
            let symbol = format!("{asset}/USDT");
            let bars = match self
                .runtime
                .client
                .klines(&symbol, "1h", None, None, Some(HOUR_BARS))
                .await
            {
                Ok(klines) => Some(
                    klines
                        .iter()
                        .filter_map(|k| Some((k.open_time, k.close.to_f64()?)))
                        .collect::<Vec<_>>(),
                ),
                Err(error) => {
                    tracing::warn!(%error, %symbol, "1h history read failed; 7d move kept or unknown until the next hour");
                    None
                }
            };
            self.hours.store(asset, bars, now_ms);
        }
        polled
    }

    /// The rule's verdict on this tick's inputs.
    fn decide(
        &self,
        now_ms: i64,
        tags: TagsNow,
        candidates: Vec<String>,
        held: &BTreeSet<String>,
    ) -> Result<Decision> {
        let slow = self.slow_state()?;
        // The 24h return is the USDT market's 24h ticker change from the catalog (reloaded
        // every SLOW_REFRESH_MS): enough for a 24h horizon, no extra request.
        let ret_24h = |asset: &str| {
            slow.view
                .markets
                .get(&format!("{asset}/USDT"))
                .and_then(|market| ticker_change_pct(market, now_ms))
        };
        let coins: BTreeMap<String, CoinView> = candidates
            .iter()
            .chain(held)
            .filter_map(|asset| {
                let mut view = self.prices.coin_view(asset)?;
                view.ret_24h = ret_24h(asset);
                view.ret_7d = self
                    .prices
                    .last_close(asset)
                    .and_then(|price| self.hours.ret_7d(asset, now_ms, price));
                Some((asset.clone(), view))
            })
            .collect();
        let step = crate::book::Snapshot::live(now_ms, coins, candidates);
        let book = self.book(slow, now_ms);
        let feed = match &tags {
            TagsNow::Fresh(feed) => Some(&feed.assets),
            TagsNow::Unavailable { .. } => None,
        };
        let rule = self.runtime.config.algo_rule;
        let inputs = Inputs {
            snap: &step,
            book: &book,
            tags: feed,
        };
        let verdict = decide(&inputs, &rule);
        let mut scored: Vec<(f64, serde_json::Value)> = step
            .liquid_top
            .iter()
            .filter(|asset| !held.contains(*asset))
            .chain(held)
            .map(|asset| {
                let terms = crate::decider::terms(&inputs, asset, rule.min_posters);
                let score = terms.score(&rule.weights);
                let coin = serde_json::json!({
                    "asset": asset,
                    "held": held.contains(asset),
                    "priced": step.coin(asset).is_some(),
                    "talk": terms.talk,
                    "stance": terms.stance,
                    "momentum": terms.momentum,
                    "ret_1h_pct": step.coin(asset).and_then(|c| c.ret_1h),
                    "ret_24h_pct": step.coin(asset).and_then(|c| c.ret_24h),
                    "ret_7d_pct": step.coin(asset).and_then(|c| c.ret_7d),
                    "score": score,
                });
                (score, coin)
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(Decision {
            key: decision_key(&verdict),
            verdict,
            tags,
            equity: slow.balances.free_value_usdt(&slow.view),
            coins: scored.into_iter().map(|(_, coin)| coin).collect(),
        })
    }

    fn context(&self, slow: &Slow, decision: &Decision) -> DecisionContext {
        let config = &self.runtime.config;
        DecisionContext {
            timestamp: crate::audit::utc_timestamp(),
            rules_version: config.rules_version.clone(),
            market_pick_mode: config.market_pick_mode.as_str(),
            portfolio_value_usdt: decision.equity,
            balances: slow.balances.clone(),
            candidate_routes: Vec::new(),
            execution_invariants: execution_invariants(config),
            route_scope: route_scope(config),
            trade_memory: self.runtime.trade_memory(
                &slow.balances,
                &slow.view,
                config.trade_memory_limit,
            ),
            tags: Some(decision.tags.clone()),
        }
    }
}

/// The market's 24h ticker change in percent at `now_ms`; `None` without one or when its
/// ticker is older than [`TICKER_24H_MAX_AGE_MS`].
pub fn ticker_change_pct(market: &bb_pool::MarketSnapshot, now_ms: i64) -> Option<f64> {
    let ticker_ms = market.timestamp_ms?;
    if now_ms - ticker_ms > TICKER_24H_MAX_AGE_MS {
        return None;
    }
    market.price_change_pct_24h?.to_f64()
}

/// What the rule decided, without scores: its orders, or the coins it keeps (cash when
/// none). Rows are written when this changes.
pub fn decision_key(verdict: &Verdict) -> String {
    if verdict.actions.is_empty() {
        let kept: Vec<&str> = verdict
            .held
            .iter()
            .map(|(asset, _)| asset.as_str())
            .collect();
        return if kept.is_empty() {
            "hold cash".to_owned()
        } else {
            format!("keep {}", kept.join(" "))
        };
    }
    verdict
        .actions
        .iter()
        .map(|action| match action {
            Action::Sell { asset, .. } => format!("sell {asset}"),
            Action::Buy { asset, .. } => format!("buy {asset}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rust_decimal_macros::dec;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::TraderConfig;
    use crate::universe::tests::{config, view};

    const BUY_ETH: &str = "0,0,1,0.5,0";
    const HOLD: &str = "0,0,1,5,0";

    fn trader_config(dir: &Path, weights: &str) -> TraderConfig {
        let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let (audit, state, stop, feed) =
            (p("audit.jsonl"), p("state.json"), p("stop"), p("feed.json"));
        config(&[
            ("BOT_SPOT_AUDIT_PATH", audit.as_str()),
            ("BOT_SPOT_STATE_PATH", state.as_str()),
            ("BOT_SPOT_STOP_PATH", stop.as_str()),
            ("BOT_SPOT_TAGS_FEED_PATH", feed.as_str()),
            ("BOT_SPOT_ALGO_WEIGHTS", weights),
        ])
    }

    /// The feed names ETH; prices: ETH 3000 now, 2800 an hour ago (+7%, momentum 1), and
    /// every hourly close of the last 200 hours 2000 (7d move +50%).
    async fn market(server: &MockServer, dir: &Path, now_ms: i64, eth_held: bool) {
        let as_of = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)
            .unwrap()
            .to_rfc3339();
        std::fs::write(
            dir.join("feed.json"),
            json!({"schema": "tags-feed-v1", "as_of": as_of, "tags": {"asset:ETH": {"kind": "asset", "name": "ETH", "windows": {"6h": {"mentions": 3}}}}}).to_string(),
        )
        .unwrap();
        let minute = now_ms - now_ms.rem_euclid(60_000);
        let klines: Vec<Value> = (1..=61)
            .map(|back| {
                let open = minute - back * 60_000;
                json!([
                    open,
                    "1",
                    "1",
                    "1",
                    "2800",
                    "1",
                    open + 59_999,
                    "1",
                    1,
                    "1",
                    "1",
                    "0"
                ])
            })
            .collect();
        let balances = if eth_held {
            json!([{"asset": "ETH", "free": "0.05", "locked": "0"}])
        } else {
            json!([{"asset": "USDT", "free": "100", "locked": "0"}])
        };
        for (route, body) in [
            (
                "/api/v3/time",
                json!({"serverTime": bb_core::time::now_ms()}),
            ),
            (
                "/api/v3/ticker/price",
                json!([{"symbol": "ETHUSDT", "price": "3000"}, {"symbol": "BTCUSDT", "price": "60000"}]),
            ),
            ("/api/v3/klines", json!(klines)),
            (
                "/api/v3/account",
                json!({"canTrade": true, "balances": balances}),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(server)
                .await;
        }
        let hour = now_ms - now_ms.rem_euclid(HOUR_MS);
        let hours: Vec<Value> = (1..=i64::from(HOUR_BARS))
            .map(|back| {
                let open = hour - back * HOUR_MS;
                json!([open, "1", "1", "1", "2000", "1", open + HOUR_MS - 1, "1", 1, "1", "1", "0"])
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .and(query_param("interval", "1h"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(hours)))
            .with_priority(1)
            .mount(server)
            .await;
    }

    fn nonstop(server: &MockServer, config: TraderConfig, now_ms: i64) -> Nonstop {
        let client = bb_binance::BinanceClient::new(
            bb_binance::BinanceConfig::new(false)
                .with_credentials(bb_binance::Credentials::new("key", "secret").unwrap())
                .with_spot_rest_url(server.uri())
                .with_retries(0, Duration::from_millis(1), Duration::from_millis(1)),
        )
        .unwrap();
        let config = Arc::new(config);
        let view = Arc::new(view(&config));
        let runtime = Arc::new(Runtime::new(config, CycleMode::PlanOnly, client));
        let mut trader = Nonstop::new(runtime);
        trader.slow = Some(Slow {
            view,
            balances: Balances {
                free: [("USDT".to_owned(), dec!(100))].into(),
                used: Default::default(),
                total: [("USDT".to_owned(), dec!(100))].into(),
            },
            loaded_ms: now_ms,
        });
        trader
    }

    fn rows(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn requests(server: &MockServer, route: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == route)
            .count()
    }

    async fn kline_reads(server: &MockServer, interval: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                r.url.path() == "/api/v3/klines"
                    && r.url.query_pairs().any(|(k, v)| k == "interval" && v == interval)
            })
            .count()
    }

    #[tokio::test]
    async fn an_unchanged_decision_writes_no_row_until_the_hourly_heartbeat() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let now = bb_core::time::now_ms();
        market(&server, dir.path(), now, false).await;
        let mut trader = nonstop(&server, trader_config(dir.path(), HOLD), now);
        let token = CancellationToken::new();

        assert!(matches!(
            trader.tick(now, false, &token).await.unwrap(),
            Tick::Recorded(_)
        ));
        let first = rows(dir.path());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["row_type"], "cycle_summary");
        assert!(
            first[0]["decision"]["reason"]
                .as_str()
                .unwrap()
                .contains("hold in cash")
        );
        for k in 1..=4 {
            let tick = trader.tick(now + k * 15_000, false, &token).await.unwrap();
            assert!(matches!(tick, Tick::Unchanged), "{tick:?}");
        }
        assert_eq!(rows(dir.path()).len(), 1, "no row on an unchanged tick");
        // The heartbeat: the same decision is recorded again an hour later.
        let later = now + HEARTBEAT_MS;
        assert!(matches!(
            trader.tick(later, false, &token).await.unwrap(),
            Tick::Recorded(_)
        ));
        assert_eq!(rows(dir.path()).len(), 2);
        // One price poll per tick and the 1m klines seed once (the hour gap re-seeds it); the
        // 1h klines are read once per clock hour the ticks fall in.
        assert_eq!(requests(&server, "/api/v3/ticker/price").await, 6);
        assert_eq!(kline_reads(&server, "1m").await, 2);
        let clock_hours: BTreeSet<i64> = [0, 15_000, 30_000, 45_000, 60_000, HEARTBEAT_MS]
            .iter()
            .map(|d| floor_hour(now + d))
            .collect();
        assert_eq!(kline_reads(&server, "1h").await, clock_hours.len());
    }

    #[tokio::test]
    async fn the_stop_file_is_seen_on_the_next_tick_and_nothing_is_read_or_decided() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let now = bb_core::time::now_ms();
        market(&server, dir.path(), now, false).await;
        let mut trader = nonstop(&server, trader_config(dir.path(), BUY_ETH), now);
        let token = CancellationToken::new();
        assert!(matches!(
            trader.tick(now, false, &token).await.unwrap(),
            Tick::Recorded(_)
        ));
        let before = (
            server.received_requests().await.unwrap().len(),
            rows(dir.path()).len(),
        );

        std::fs::write(dir.path().join("stop"), "").unwrap();
        for k in 1..=3 {
            let tick = trader.tick(now + k * 15_000, true, &token).await.unwrap();
            assert!(matches!(tick, Tick::Stopped), "{tick:?}");
        }
        assert_eq!(
            (
                server.received_requests().await.unwrap().len(),
                rows(dir.path()).len()
            ),
            before,
            "a stopped tick sends no request and writes no row"
        );
        assert!(
            trader
                .once(&token)
                .await
                .unwrap_err()
                .to_string()
                .contains("emergency stop")
        );

        std::fs::remove_file(dir.path().join("stop")).unwrap();
        let tick = trader.tick(now + 60_000, false, &token).await.unwrap();
        assert!(!matches!(tick, Tick::Stopped), "{tick:?}");
    }

    #[tokio::test]
    async fn balances_are_reloaded_on_the_tick_after_an_executed_route() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let now = bb_core::time::now_ms();
        // The account now holds ETH (as after the buy).
        market(&server, dir.path(), now, true).await;
        let mut trader = nonstop(&server, trader_config(dir.path(), BUY_ETH), now);
        let token = CancellationToken::new();

        let Tick::Recorded(buy) = trader.tick(now, false, &token).await.unwrap() else {
            panic!("the first decision is recorded");
        };
        assert_eq!(buy.decision.action, "traded");
        let first = rows(dir.path());
        assert_eq!(
            (first[0]["row_type"].as_str(), first[0]["status"].as_str()),
            (Some("route"), Some("planned"))
        );
        assert_eq!(first[0]["decision"]["destination_asset"], "ETH");
        // The decision row carries every scored coin's inputs (the dashboard's Algorithm tab).
        let eth = &first[1]["algo"]["coins"][0];
        assert_eq!((eth["asset"].as_str(), eth["held"].as_bool()), (Some("ETH"), Some(false)));
        assert_eq!((eth["momentum"].as_f64(), eth["stance"].as_f64(), eth["score"].as_f64()), (Some(1.0), Some(0.0), Some(1.0)));
        assert!((eth["ret_1h_pct"].as_f64().unwrap() - (3000.0 / 2800.0 - 1.0) * 100.0).abs() < 1e-9);
        // The 24h return is ETH/USDT's 24h ticker change from the catalog.
        assert_eq!(eth["ret_24h_pct"].as_f64(), Some(-4.2));
        // The 7d return: the price now (3000) against the hourly close 7 days back (2000).
        assert!((eth["ret_7d_pct"].as_f64().unwrap() - 50.0).abs() < 1e-9);
        let accounts = requests(&server, "/api/v3/account").await;

        // The next tick reloads the balances (the catalog reload fails here and is kept) and
        // decides on them: ETH is held, so the decision changes to keeping it.
        let Tick::Recorded(keep) = trader.tick(now + 15_000, false, &token).await.unwrap() else {
            panic!("the changed decision is recorded");
        };
        assert!(requests(&server, "/api/v3/account").await > accounts);
        assert_eq!(keep.decision.action, "hold");
        assert!(
            keep.decision.reason.contains("keep ETH"),
            "{}",
            keep.decision.reason
        );
        // No reload without an executed route before the slow timer.
        let accounts = requests(&server, "/api/v3/account").await;
        assert!(matches!(
            trader.tick(now + 30_000, false, &token).await.unwrap(),
            Tick::Unchanged
        ));
        assert_eq!(requests(&server, "/api/v3/account").await, accounts);
    }

    #[test]
    fn hourly_bars_are_read_once_per_clock_hour_and_bounded() {
        let h0 = 1_792_933_200_000; // 2026-10-25T13:00Z, an hour boundary
        let bars = |newest_open: i64, n: i64, close: f64| {
            (0..n).map(move |back| (newest_open - back * HOUR_MS, close)).collect::<Vec<_>>()
        };
        let mut hours = HourBook::default();
        assert!(hours.due("ETH", h0 + 5 * 60_000));
        // 250 closed bars plus the forming one: the forming bar is dropped, 200 are kept.
        let mut read = bars(h0 - HOUR_MS, 250, 2000.0);
        read.push((h0, 9999.0));
        hours.store("ETH", Some(read), h0 + 5 * 60_000);
        assert_eq!(hours.bars("ETH"), HOUR_BARS as usize);
        // Not again within the same clock hour, again in the next one.
        assert!(!hours.due("ETH", h0 + 59 * 60_000));
        assert!(hours.due("ETH", h0 + HOUR_MS));
        // A failed read keeps the bars and still waits for the next hour.
        hours.store("ETH", None::<Vec<(i64, f64)>>, h0 + HOUR_MS + 60_000);
        assert_eq!(hours.bars("ETH"), HOUR_BARS as usize);
        assert!(!hours.due("ETH", h0 + HOUR_MS + 30 * 60_000));
        // Coins no longer read are forgotten after two hours.
        hours.store("SOL", Some(bars(h0 - HOUR_MS, 3, 1.0)), h0 + 60_000);
        hours.forget_unread(h0 + 2 * HOUR_MS + 60_000);
        assert_eq!((hours.bars("ETH"), hours.bars("SOL")), (HOUR_BARS as usize, 0));
    }

    #[test]
    fn the_7d_move_uses_the_hourly_close_a_week_back() {
        let h0 = 1_792_933_200_000;
        let mut hours = HourBook::default();
        // Hourly closes 1000 + i for the bar opened i hours after h0 - 200h.
        let start = h0 - 200 * HOUR_MS;
        hours.store("ETH", Some((0..200).map(|i| (start + i * HOUR_MS, 1000.0 + i as f64))), h0 + 60_000);
        // At h0 + 30 min the week-old close is the bar closing at h0 - 168h (opened at
        // h0 - 169h, i = 31): 1031.
        let now = h0 + 30 * 60_000;
        assert!((hours.ret_7d("ETH", now, 2062.0).unwrap() - 100.0).abs() < 1e-9);
        // Without a close within the hour before now - 7d the move is unknown.
        assert_eq!(hours.ret_7d("ETH", h0 - 40 * HOUR_MS, 2000.0), None);
        assert_eq!(hours.ret_7d("SOL", now, 1.0), None);
    }

    #[test]
    fn the_24h_change_comes_from_a_fresh_ticker_only() {
        let config = config(&[]);
        let view = view(&config);
        let eth = &view.markets["ETH/USDT"];
        let t = eth.timestamp_ms.unwrap();
        assert_eq!(ticker_change_pct(eth, t), Some(-4.2));
        assert_eq!(ticker_change_pct(eth, t + TICKER_24H_MAX_AGE_MS), Some(-4.2));
        // A catalog kept after failed reloads does not pass an old change off as current.
        assert_eq!(ticker_change_pct(eth, t + TICKER_24H_MAX_AGE_MS + 1), None);
        // A market whose ticker had no 24h open has no change.
        assert_eq!(ticker_change_pct(&view.markets["BTC/USDT"], t), None);
    }

    #[test]
    fn the_price_book_turns_polls_into_minute_closes_and_reseeds_after_a_gap() {
        let t0 = 1_792_933_200_000; // a minute boundary
        let mut book = PriceBook::default();
        assert!(book.needs_seed("SOL", t0));
        book.seed("SOL", (1..=61).map(|back| (t0 - back * 60_000, 100.0)), t0);
        book.record("SOL", t0 + 20_000, 104.0);
        book.record("SOL", t0 + 40_000, 106.0);
        assert!(!book.needs_seed("SOL", t0 + 40_000));
        let view = book.coin_view("SOL").unwrap();
        // The newest poll of the minute against the close an hour before it.
        assert!((view.ret_1h.unwrap() - 6.0).abs() < 1e-9, "{view:?}");
        assert!(
            book.needs_seed("SOL", t0 + 4 * 60_000),
            "three missed minutes"
        );
    }
}

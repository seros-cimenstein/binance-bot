//! `claude-spot-exit-watch` (Phase 45 step 2, paper trial): follow the bid of every
//! position Claude gave an exit plan, between cycles, and record what the plan would
//! have sold. It places no orders and holds no credentials.
//!
//! Each loop: read the plan book and the Claude Spot audit; start trials for new plans
//! (the peak is seeded with the highest 1-minute high since entry, then the minutes since
//! the plan was set are caught up); feed each running trial the current bid; and close a
//! trial when the audit shows Claude sold the position. A closed trial compares "Claude
//! with the plan" (the plan's sells before Claude's, the rest at Claude's real fill) with
//! "Claude alone", both net of the entry leg's fee rate (plan sells also pay slippage).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use bb_binance::history::HistoryFetcher;
use bb_binance::{BinanceClient, BinanceConfig};
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::config::DEFAULT_AUDIT_PATH;
use crate::error::{Result, TraderError};
use crate::exit_book::{
    ClosedView, DefaultPlan, ExitPaths, ExitSettings, FillView, PlanBook, PlanEntry,
    RECENT_CLOSED, WatchState, WatchView,
};
use crate::exit_plan::{
    ATR_PERIODS, AtrTimeframe, ExitFill, ExitWatch, PriceBar, average_true_range,
};
use crate::exit_replay::{ActualExit, RoundTrip, read_audit, round_trips};

const MS_PER_MINUTE: i64 = 60_000;
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
/// 1-minute bars before entry that warm up the stretch and slowdown rules.
const HISTORY_MINUTES: i64 = 60;
/// An old position is followed from at most this far back when its trial starts.
const MAX_CATCH_UP_MS: i64 = 3 * 24 * 60 * MS_PER_MINUTE;

/// Run the paper exit watcher.
#[derive(clap::Args, Debug, Clone)]
pub struct ExitWatchArgs {
    /// Keep watching every --poll-seconds until SIGTERM / Ctrl+C (default: one pass).
    #[arg(long)]
    pub daemon: bool,
    #[arg(long, default_value_t = 5)]
    pub poll_seconds: u64,
    /// Exit slippage in bps on top of the fee for the plan's simulated sells.
    #[arg(long, default_value = "5")]
    pub slippage_bps: Decimal,
    /// Print the closed trials from the watch log as a Markdown table and exit.
    #[arg(long)]
    pub summary: bool,
    /// Claude Spot audit JSONL (default `CLAUDE_SPOT_AUDIT_PATH`).
    #[arg(long)]
    pub audit_path: Option<PathBuf>,
}

/// Where the trial's prices come from.
#[async_trait::async_trait]
pub trait PriceSource {
    /// `timeframe` candles with open time in `[start_ms, end_ms]`.
    async fn bars(
        &self,
        symbol: &str,
        timeframe: AtrTimeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<PriceBar>>;
    /// Current best bid (what a market sell would get).
    async fn bid(&self, symbol: &str) -> Result<Decimal>;
}

/// Binance public market data.
#[derive(Debug, Clone)]
pub struct BinancePrices {
    client: BinanceClient,
}

impl BinancePrices {
    pub fn new(client: BinanceClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl PriceSource for BinancePrices {
    async fn bars(
        &self,
        symbol: &str,
        timeframe: AtrTimeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<PriceBar>> {
        let time = |ms: i64| {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                .ok_or_else(|| TraderError::config(format!("timestamp out of range: {ms}")))
        };
        let klines = HistoryFetcher::spot(self.client.clone())
            .fetch_klines(
                symbol,
                timeframe.as_str(),
                time(start_ms)?,
                Some(time(end_ms)?),
            )
            .await?;
        Ok(klines
            .into_iter()
            .map(|k| PriceBar {
                open_ms: k.open_time,
                close_ms: k.close_time.unwrap_or(k.open_time + timeframe.millis() - 1),
                open: k.open,
                high: k.high,
                low: k.low,
                close: k.close,
            })
            .collect())
    }

    async fn bid(&self, symbol: &str) -> Result<Decimal> {
        Ok(self.client.book_ticker(symbol).await?.bid_price)
    }
}

/// Appends watch-log rows (JSONL).
pub trait WatchLog {
    fn append(&mut self, row: Value);
}

/// The watch log file.
#[derive(Debug)]
pub struct FileLog {
    pub(crate) path: PathBuf,
}

impl WatchLog for FileLog {
    fn append(&mut self, row: Value) {
        let write = || -> std::io::Result<()> {
            if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            writeln!(file, "{row}")
        };
        if let Err(error) = write() {
            tracing::warn!(%error, path = %self.path.display(), "exit watch log not written");
        }
    }
}

impl WatchLog for Vec<Value> {
    fn append(&mut self, row: Value) {
        self.push(row);
    }
}

struct Trial {
    entry: PlanEntry,
    fee_rate: Decimal,
    watch: ExitWatch,
    fills: Vec<FillView>,
    last_bid: Option<Decimal>,
    last_ms: i64,
}

impl Trial {
    fn view(&self) -> WatchView {
        WatchView {
            key: self.entry.key(),
            spec: self.entry.spec.clone(),
            symbol: self.entry.symbol.clone(),
            entry_price: self.entry.entry_price,
            trail_pct: self.watch.trail_pct(),
            peak: self.watch.peak(),
            sell_line: self.watch.sell_line(),
            remaining: self.watch.remaining(),
            last_bid: self.last_bid,
            last_ms: self.last_ms,
            fills: self.fills.clone(),
        }
    }
}

/// A plan that fired: what a live trader sells. `share` is the part of what is still held
/// (the fill's fraction of the original position divided by what was left before it).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSignal {
    pub key: String,
    pub asset: String,
    pub spec: String,
    pub fill: ExitFill,
    pub share: Decimal,
    /// `live` (a current bid) or `catch_up` (minutes the watcher had not seen yet).
    pub source: &'static str,
}

/// The watcher: running trials plus the persisted totals.
pub struct Watcher {
    trials: BTreeMap<String, Trial>,
    state: WatchState,
    slippage: Decimal,
    default_plan: Option<DefaultPlan>,
    /// Fills already logged per key before a restart (not logged again on catch-up).
    logged_fills: BTreeMap<String, usize>,
}

fn fill_view(fill: &ExitFill) -> FillView {
    FillView {
        ts_ms: fill.ts_ms,
        price: fill.price,
        fraction: fill.fraction,
        reason: fill.reason.as_str().into(),
    }
}

impl Watcher {
    /// Resume from a saved state (finalized keys, totals, already-logged fills).
    pub fn new(
        state: WatchState,
        slippage_bps: Decimal,
        default_plan: Option<DefaultPlan>,
    ) -> Self {
        let logged_fills = state
            .active
            .values()
            .map(|v| (v.key.clone(), v.fills.len()))
            .collect();
        Self {
            trials: BTreeMap::new(),
            state: WatchState {
                active: BTreeMap::new(),
                ..state
            },
            slippage: slippage_bps / BPS,
            default_plan,
            logged_fills,
        }
    }

    pub fn state(&self) -> &WatchState {
        &self.state
    }

    /// Plans to run, by asset: Claude's plan for an open position, else the default plan
    /// for positions opened since it applies; Claude's plans for positions no longer open
    /// stay (they close or orphan their trial) unless the asset has a newer position.
    fn effective_plans(&self, book: &PlanBook, trips: &[RoundTrip]) -> BTreeMap<String, PlanEntry> {
        let mut plans = BTreeMap::new();
        for trip in trips.iter().filter(|t| t.exit.is_none()) {
            let chosen = book
                .plans
                .get(&trip.asset)
                .filter(|p| p.entry_ms == trip.entry_ms)
                .cloned()
                .or_else(|| {
                    let default = self.default_plan.filter(|d| trip.entry_ms >= d.from_ms)?;
                    Some(PlanEntry {
                        asset: trip.asset.clone(),
                        symbol: trip.symbol.clone(),
                        spec: default.plan.to_string(),
                        plan: default.plan,
                        entry_ms: trip.entry_ms,
                        entry_price: trip.entry_price,
                        set_at_ms: trip.entry_ms,
                        cycle_id: "default".into(),
                    })
                });
            if let Some(entry) = chosen {
                plans.insert(trip.asset.clone(), entry);
            }
        }
        for (asset, entry) in &book.plans {
            plans.entry(asset.clone()).or_insert_with(|| entry.clone());
        }
        plans
    }

    /// Record `fills` (from one bar) of `asset`'s trial; returns the newly logged ones.
    fn record(
        &mut self,
        asset: &str,
        fills: Vec<ExitFill>,
        source: &'static str,
        log: &mut dyn WatchLog,
    ) -> Vec<PlanSignal> {
        let Some(trial) = self.trials.get_mut(asset) else {
            return Vec::new();
        };
        let key = trial.entry.key();
        let mut held = trial.watch.remaining() + fills.iter().map(|f| f.fraction).sum::<Decimal>();
        let mut signals = Vec::new();
        for fill in fills {
            let share = if held > Decimal::ZERO {
                (fill.fraction / held).min(Decimal::ONE)
            } else {
                Decimal::ONE
            };
            held -= fill.fraction;
            let view = fill_view(&fill);
            trial.fills.push(view.clone());
            let already = self.logged_fills.get(&key).copied().unwrap_or(0);
            if trial.fills.len() > already {
                log.append(json!({
                    "event": "would_sell",
                    "key": key,
                    "asset": asset,
                    "spec": trial.entry.spec,
                    "ts_ms": view.ts_ms,
                    "price": view.price,
                    "fraction": view.fraction,
                    "reason": view.reason,
                    "source": source,
                }));
                signals.push(PlanSignal {
                    key: key.clone(),
                    asset: asset.to_owned(),
                    spec: trial.entry.spec.clone(),
                    fill,
                    share,
                    source,
                });
            }
        }
        signals
    }

    fn finalize(
        &mut self,
        key: String,
        event: &str,
        asset: &str,
        spec: &str,
        log: &mut dyn WatchLog,
    ) {
        log.append(json!({"event": event, "key": key, "asset": asset, "spec": spec}));
        self.state.finalized.insert(key);
    }

    /// Close a trial on Claude's real exit.
    fn close(&mut self, asset: &str, trip: &RoundTrip, exit: &ActualExit, log: &mut dyn WatchLog) {
        let Some(trial) = self.trials.remove(asset) else {
            return;
        };
        let fee = trial.fee_rate;
        let claude_price = exit
            .price
            .or(trial.last_bid)
            .unwrap_or(trial.entry.entry_price);
        let cost = trial.entry.entry_price * (Decimal::ONE + fee);
        let mut sold = Decimal::ZERO;
        let mut with_plan = Decimal::ZERO;
        // The plan's sells at their trigger prices, no modelled slippage.
        let mut plan_proceeds = Decimal::ZERO;
        let mut plan_fills = Vec::new();
        for fill in trial.fills.iter().filter(|f| f.ts_ms < exit.ts_ms) {
            sold += fill.fraction;
            with_plan += fill.fraction * fill.price * (Decimal::ONE - fee - self.slippage);
            plan_proceeds += fill.fraction * fill.price * (Decimal::ONE - fee);
            plan_fills.push(fill.clone());
        }
        let alone = claude_price * (Decimal::ONE - fee);
        let rest = (Decimal::ONE - sold).max(Decimal::ZERO) * alone;
        with_plan += rest;
        plan_proceeds += rest;
        let diff = (with_plan - alone) / cost * trip.entry_value_usdt;
        let pct = |proceeds: Decimal| {
            ((proceeds / cost - Decimal::ONE) * Decimal::ONE_HUNDRED).round_dp(4)
        };
        let key = trial.entry.key();
        log.append(json!({
            "event": "closed",
            "key": key,
            "asset": asset,
            "spec": trial.entry.spec,
            "symbol": trial.entry.symbol,
            "entry_ms": trial.entry.entry_ms,
            "entry_price": trial.entry.entry_price,
            "entry_value_usdt": trip.entry_value_usdt.round_dp(6),
            "claude_exit_ms": exit.ts_ms,
            "claude_exit_price": claude_price,
            "claude_exit_price_known": exit.price.is_some(),
            "plan_fills": plan_fills,
            "claude_alone_net_pct": pct(alone),
            "with_plan_net_pct": pct(with_plan),
            "plan_vs_claude_usdt": diff.round_dp(6),
        }));
        self.state.closed_trials += 1;
        self.state.plan_vs_claude_usdt += diff.round_dp(6);
        self.state.recent_closed.push(ClosedView {
            asset: asset.to_owned(),
            spec: trial.entry.spec.clone(),
            entry_ms: trial.entry.entry_ms,
            entry_price: trial.entry.entry_price,
            exit_ms: exit.ts_ms,
            exit_price: claude_price,
            plan_fills,
            fee_rate: fee,
            plan_net_pct: pct(plan_proceeds),
            exit_net_pct: pct(alone),
        });
        let excess = self.state.recent_closed.len().saturating_sub(RECENT_CLOSED);
        self.state.recent_closed.drain(..excess);
        self.state.finalized.insert(key);
    }

    async fn start(
        &mut self,
        entry: &PlanEntry,
        trip: &RoundTrip,
        prices: &dyn PriceSource,
        now_ms: i64,
        log: &mut dyn WatchLog,
    ) -> Result<Vec<PlanSignal>> {
        let catch_up_end = trip.exit.as_ref().map_or(now_ms, |e| e.ts_ms.min(now_ms));
        // An old position is followed from at most MAX_CATCH_UP_MS back.
        let history_start =
            (entry.entry_ms - HISTORY_MINUTES * MS_PER_MINUTE).max(catch_up_end - MAX_CATCH_UP_MS);
        let bars = prices
            .bars(&entry.symbol, AtrTimeframe::M1, history_start, catch_up_end)
            .await?;
        let atr = match entry.plan.atr_timeframe() {
            None => None,
            Some(tf) => {
                // One spare candle so the first range has a previous close.
                let span = tf.millis() * (ATR_PERIODS as i64 + 1);
                let candles: Vec<PriceBar> = prices
                    .bars(&entry.symbol, tf, entry.entry_ms - span, entry.entry_ms - 1)
                    .await?
                    .into_iter()
                    .filter(|b| b.close_ms < entry.entry_ms)
                    .collect();
                average_true_range(&candles)
            }
        };
        let mut watch = match ExitWatch::new(
            &entry.plan,
            entry.entry_price,
            entry.entry_ms,
            atr,
            trip.fee_rate,
        ) {
            Ok(watch) => watch,
            Err(error) => {
                log.append(json!({"event": "rejected", "key": entry.key(), "asset": entry.asset, "spec": entry.spec, "error": error}));
                self.state.finalized.insert(entry.key());
                return Ok(Vec::new());
            }
        };
        // Minute history before entry, then the highest high and minutes between entry
        // and the moment the plan was set.
        for bar in bars.iter().filter(|b| b.close_ms < entry.entry_ms) {
            watch.warm_up_before_entry(bar);
        }
        for bar in bars
            .iter()
            .filter(|b| b.open_ms >= entry.entry_ms && b.close_ms < entry.set_at_ms)
        {
            watch.warm_up_since_entry(bar);
        }
        log.append(json!({
            "event": "started",
            "key": entry.key(),
            "asset": entry.asset,
            "spec": entry.spec,
            "symbol": entry.symbol,
            "entry_price": entry.entry_price,
            "atr": atr.map(|a| a.round_dp(8)),
            "trail_pct": watch.trail_pct().map(|t| t.round_dp(4)),
            "peak": watch.peak(),
        }));
        self.trials.insert(
            entry.asset.clone(),
            Trial {
                entry: entry.clone(),
                fee_rate: trip.fee_rate,
                watch,
                fills: Vec::new(),
                last_bid: None,
                last_ms: entry.set_at_ms,
            },
        );
        // Minutes since the plan was set (complete bars only).
        let mut signals = Vec::new();
        for bar in bars
            .iter()
            .filter(|b| b.open_ms >= entry.set_at_ms && b.close_ms < catch_up_end)
        {
            let Some(trial) = self.trials.get_mut(&entry.asset) else {
                break;
            };
            let fills = trial.watch.on_bar(bar);
            trial.last_ms = bar.close_ms;
            signals.extend(self.record(&entry.asset, fills, "catch_up", log));
        }
        Ok(signals)
    }

    /// One pass over the plans, the round trips and the current bids; returns the plan
    /// sells that fired (newly logged), for a live trader to execute.
    pub async fn step(
        &mut self,
        book: &PlanBook,
        trips: &[RoundTrip],
        prices: &dyn PriceSource,
        now_ms: i64,
        log: &mut dyn WatchLog,
    ) -> Vec<PlanSignal> {
        let trip_of = |asset: &str, entry_ms: i64| {
            trips
                .iter()
                .find(|t| t.asset == asset && t.entry_ms == entry_ms)
        };
        let plans = self.effective_plans(book, trips);
        let mut signals = Vec::new();

        // Running trials: closed (by Claude or a live plan sell), or the plan was cleared
        // or replaced.
        let running: Vec<(String, String, i64)> = self
            .trials
            .iter()
            .map(|(asset, t)| (asset.clone(), t.entry.key(), t.entry.entry_ms))
            .collect();
        for (asset, key, entry_ms) in running {
            if let Some(trip) = trip_of(&asset, entry_ms)
                && let Some(exit) = &trip.exit
            {
                self.close(&asset, trip, exit, log);
                continue;
            }
            if plans.get(&asset).map(PlanEntry::key) != Some(key.clone())
                && let Some(trial) = self.trials.remove(&asset)
            {
                self.finalize(key, "cleared", &asset, &trial.entry.spec, log);
            }
        }

        // New plans.
        for entry in plans.values() {
            let key = entry.key();
            if self.state.finalized.contains(&key)
                || self
                    .trials
                    .get(&entry.asset)
                    .is_some_and(|t| t.entry.key() == key)
            {
                continue;
            }
            let Some(trip) = trip_of(&entry.asset, entry.entry_ms) else {
                self.finalize(key, "orphaned", &entry.asset, &entry.spec, log);
                continue;
            };
            match self.start(entry, trip, prices, now_ms, log).await {
                Ok(caught_up) => signals.extend(caught_up),
                Err(error) => {
                    tracing::warn!(%error, asset = %entry.asset, "exit plan trial not started; retrying next pass");
                    continue;
                }
            }
            if let Some(exit) = &trip.exit {
                self.close(&entry.asset, trip, exit, log);
                signals.retain(|s| s.key != key);
            }
        }

        // Live bids.
        let assets: Vec<String> = self.trials.keys().cloned().collect();
        for asset in assets {
            let Some(trial) = self.trials.get(&asset) else {
                continue;
            };
            if trial.watch.remaining() <= Decimal::ZERO {
                continue;
            }
            let symbol = trial.entry.symbol.clone();
            match prices.bid(&symbol).await {
                Ok(bid) if bid > Decimal::ZERO => {
                    let Some(trial) = self.trials.get_mut(&asset) else {
                        continue;
                    };
                    trial.last_bid = Some(bid);
                    trial.last_ms = now_ms;
                    let fills = trial.watch.on_bar(&PriceBar::tick(now_ms, bid));
                    signals.extend(self.record(&asset, fills, "live", log));
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, %symbol, "bid unavailable"),
            }
        }

        self.state.updated_at_ms = now_ms;
        self.state.active = self
            .trials
            .iter()
            .map(|(asset, trial)| (asset.clone(), trial.view()))
            .collect();
        signals
    }
}

/// Re-reads the audit only when the file changed.
pub(crate) struct AuditCache {
    path: PathBuf,
    stamp: Option<(u64, std::time::SystemTime)>,
    trips: Vec<RoundTrip>,
}

impl AuditCache {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            stamp: None,
            trips: Vec::new(),
        }
    }

    pub(crate) fn trips(&mut self) -> &[RoundTrip] {
        let stamp = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.len(), m.modified().ok()?)));
        if stamp != self.stamp {
            match read_audit(&self.path) {
                Ok(rows) => self.trips = round_trips(&rows, None, None).0,
                Err(error) => {
                    tracing::warn!(%error, path = %self.path.display(), "audit unreadable")
                }
            }
            self.stamp = stamp;
        }
        &self.trips
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "SIGTERM handler unavailable; only Ctrl+C stops the watcher");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Markdown table of the closed trials in the watch log.
pub fn summary_table(log_path: &Path) -> Result<String> {
    let rows: Vec<Value> = match read_audit(log_path) {
        Ok(rows) => rows,
        Err(error) if error_is_not_found(&error) => Vec::new(),
        Err(error) => return Err(error),
    };
    let closed: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("event").and_then(Value::as_str) == Some("closed"))
        .collect();
    let mut out = format!("Exit-plan paper trial: {} closed trials.\n\n", closed.len());
    out.push_str("| asset | plan | Claude alone % | with plan % | plan vs Claude |\n|---|---|---:|---:|---:|\n");
    let mut total = Decimal::ZERO;
    for row in &closed {
        let text = |k: &str| {
            row.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let num = |k: &str| crate::memory::value_decimal(row.get(k)).unwrap_or_default();
        total += num("plan_vs_claude_usdt");
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            text("asset"),
            text("spec"),
            num("claude_alone_net_pct").round_dp(2),
            num("with_plan_net_pct").round_dp(2),
            num("plan_vs_claude_usdt").round_dp(4)
        ));
    }
    out.push_str(&format!(
        "\nTotal plan vs Claude: {} USDT.\n",
        total.round_dp(4)
    ));
    Ok(out)
}

fn error_is_not_found(error: &TraderError) -> bool {
    matches!(error, TraderError::Io(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// `bb claude-spot-exit-watch`.
pub async fn run_cli(args: ExitWatchArgs) -> Result<ExitCode> {
    let paths = ExitPaths::from_env();
    if args.summary {
        print!("{}", summary_table(&paths.log)?);
        return Ok(ExitCode::SUCCESS);
    }
    let settings =
        ExitSettings::from_lookup(|name| std::env::var(name).ok()).map_err(TraderError::config)?;
    if settings.live {
        // One writer for the watch files: live exit plans run inside `claude-spot`.
        return Err(TraderError::config(
            "CLAUDE_SPOT_EXIT_PLANS_LIVE=true: exit plans run live inside claude-spot; the paper watcher is not used",
        ));
    }
    if args.slippage_bps < Decimal::ZERO {
        return Err(TraderError::config("--slippage-bps must not be negative"));
    }
    let poll = Duration::from_secs(args.poll_seconds.max(1));
    let audit_path = args.audit_path.clone().unwrap_or_else(|| {
        PathBuf::from(bb_core::env::string(
            &["CLAUDE_SPOT_AUDIT_PATH"],
            DEFAULT_AUDIT_PATH,
        ))
    });
    let prices = BinancePrices::new(BinanceClient::new(BinanceConfig::new(false))?);
    let mut audit = AuditCache::new(audit_path);
    let mut log = FileLog {
        path: paths.log.clone(),
    };
    let mut watcher = Watcher::new(
        WatchState::load(&paths.state),
        args.slippage_bps,
        settings.default_plan,
    );
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    tracing::info!(
        daemon = args.daemon,
        poll_seconds = poll.as_secs(),
        "exit-plan paper watcher started (no orders)"
    );
    loop {
        let book = PlanBook::load(&paths.plans).unwrap_or_else(|error| {
            tracing::warn!(%error, "exit plan book unreadable");
            PlanBook::default()
        });
        let trips = audit.trips().to_vec();
        watcher
            .step(&book, &trips, &prices, bb_core::time::now_ms(), &mut log)
            .await;
        if let Err(error) = watcher.state().save(&paths.state) {
            tracing::warn!(%error, "exit watch state not saved");
        }
        if !args.daemon {
            return Ok(ExitCode::SUCCESS);
        }
        tokio::select! {
            () = &mut shutdown => {
                tracing::info!("exit-plan watcher stopping");
                return Ok(ExitCode::SUCCESS);
            }
            () = tokio::time::sleep(poll) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit_plan::ExitPlan;
    use parking_lot::Mutex;
    use rust_decimal_macros::dec;

    const M: i64 = MS_PER_MINUTE;

    struct FakePrices {
        bars: Vec<PriceBar>,
        bid: Mutex<Decimal>,
    }

    #[async_trait::async_trait]
    impl PriceSource for FakePrices {
        async fn bars(
            &self,
            _symbol: &str,
            timeframe: AtrTimeframe,
            start_ms: i64,
            end_ms: i64,
        ) -> Result<Vec<PriceBar>> {
            // Only 1-minute data in these tests: other candle sizes have none.
            if timeframe != AtrTimeframe::M1 {
                return Ok(Vec::new());
            }
            Ok(self
                .bars
                .iter()
                .copied()
                .filter(|b| b.open_ms >= start_ms && b.open_ms <= end_ms)
                .collect())
        }

        async fn bid(&self, _symbol: &str) -> Result<Decimal> {
            Ok(*self.bid.lock())
        }
    }

    fn bar(minute: i64, high: Decimal, low: Decimal) -> PriceBar {
        PriceBar {
            open_ms: minute * M,
            close_ms: (minute + 1) * M - 1,
            open: low,
            high,
            low,
            close: low,
        }
    }

    fn trip(exit: Option<ActualExit>) -> RoundTrip {
        RoundTrip {
            symbol: "ENA/USDT".into(),
            asset: "ENA".into(),
            entry_ms: 100 * M,
            entry_price: dec!(1),
            quantity: dec!(10),
            fee_rate: dec!(0.001),
            entry_value_usdt: dec!(10.01),
            exit,
        }
    }

    fn book(spec: &str, set_at_ms: i64) -> PlanBook {
        let mut book = PlanBook::default();
        book.plans.insert(
            "ENA".into(),
            PlanEntry {
                asset: "ENA".into(),
                symbol: "ENA/USDT".into(),
                plan: ExitPlan::parse_spec(spec).unwrap(),
                spec: spec.into(),
                entry_ms: 100 * M,
                entry_price: dec!(1),
                set_at_ms,
                cycle_id: "c".into(),
            },
        );
        book
    }

    fn events(log: &[Value]) -> Vec<&str> {
        log.iter().filter_map(|r| r["event"].as_str()).collect()
    }

    #[tokio::test]
    async fn trial_seeds_the_peak_catches_up_follows_bids_and_closes_on_claudes_sell() {
        // Lookback swing bars, then the position rises to 1.10 before the plan is set at
        // minute 103, and dips to 1.08 in minute 103 (above the 1.067 sell line).
        let mut bars: Vec<PriceBar> = (40..100).map(|m| bar(m, dec!(1.01), dec!(1))).collect();
        bars.push(bar(100, dec!(1.05), dec!(1)));
        bars.push(bar(101, dec!(1.10), dec!(1.04)));
        bars.push(bar(102, dec!(1.09), dec!(1.08)));
        bars.push(bar(103, dec!(1.09), dec!(1.08)));
        let prices = FakePrices {
            bars,
            bid: Mutex::new(dec!(1.08)),
        };
        let book = book("trail=3", 103 * M);
        let mut watcher = Watcher::new(WatchState::default(), dec!(5), None);
        let mut log: Vec<Value> = Vec::new();

        watcher
            .step(&book, &[trip(None)], &prices, 104 * M + 5_000, &mut log)
            .await;
        let view = &watcher.state().active["ENA"];
        assert_eq!(view.peak, dec!(1.10));
        assert_eq!(view.sell_line, Some(dec!(1.0670)));
        assert!(view.fills.is_empty());
        assert_eq!(events(&log), ["started"]);

        // The bid falls through the sell line: one would-be sell of everything.
        *prices.bid.lock() = dec!(1.06);
        watcher
            .step(&book, &[trip(None)], &prices, 104 * M + 10_000, &mut log)
            .await;
        assert_eq!(events(&log), ["started", "would_sell"]);
        assert_eq!(log[1]["source"], "live");
        assert_eq!(watcher.state().active["ENA"].remaining, Decimal::ZERO);

        // Claude sells later at 1.00: the plan would have kept 1.06.
        let exit = ActualExit {
            ts_ms: 110 * M,
            price: Some(dec!(1.00)),
            partial: false,
        };
        watcher
            .step(&book, &[trip(Some(exit))], &prices, 110 * M + 1, &mut log)
            .await;
        assert_eq!(events(&log), ["started", "would_sell", "closed"]);
        let closed = &log[2];
        let cost = dec!(1.001);
        let with_plan = dec!(1.06) * (Decimal::ONE - dec!(0.001) - dec!(0.0005));
        let alone = dec!(1.00) * dec!(0.999);
        assert_eq!(
            crate::memory::value_decimal(Some(&closed["plan_vs_claude_usdt"])).unwrap(),
            ((with_plan - alone) / cost * dec!(10.01)).round_dp(6)
        );
        let state = watcher.state();
        assert!(state.active.is_empty());
        assert_eq!(state.closed_trials, 1);
        assert!(state.finalized.contains(&book.plans["ENA"].key()));
        // The prompt's record: the rule that sold, and net results after fees with the
        // plan's sell (no modelled slippage) and with the closing sale alone.
        let view = &state.recent_closed[0];
        assert_eq!(
            (view.plan_fills[0].reason.as_str(), view.plan_fills[0].price, view.exit_price),
            ("follow_peak", dec!(1.06), dec!(1.00))
        );
        let pct = |proceeds: Decimal| ((proceeds / cost - Decimal::ONE) * dec!(100)).round_dp(4);
        assert_eq!(view.plan_net_pct, pct(dec!(1.06) * dec!(0.999)));
        assert_eq!(view.exit_net_pct, pct(alone));

        // A finished trial is never restarted from the same plan.
        watcher
            .step(&book, &[trip(None)], &prices, 111 * M, &mut log)
            .await;
        assert_eq!(log.len(), 3);
    }

    #[tokio::test]
    async fn default_plan_runs_on_new_positions_and_returns_sell_signals() {
        let bars: Vec<PriceBar> = (40..105).map(|m| bar(m, dec!(1.01), dec!(1))).collect();
        let prices = FakePrices {
            bars,
            bid: Mutex::new(dec!(1.0)),
        };
        let default = DefaultPlan {
            plan: ExitPlan::parse_spec("part=5:0.5,trail=3").unwrap(),
            from_ms: 100 * M,
        };
        let mut watcher = Watcher::new(WatchState::default(), dec!(5), Some(default));
        let mut log: Vec<Value> = Vec::new();
        let empty = PlanBook::default();

        // A position opened before the default applies gets no plan.
        let mut old = trip(None);
        old.entry_ms = 99 * M;
        let signals = watcher
            .step(&empty, &[old], &prices, 105 * M, &mut log)
            .await;
        assert!(signals.is_empty());
        assert!(watcher.state().active.is_empty());

        // A new position runs the default plan.
        watcher
            .step(&empty, &[trip(None)], &prices, 105 * M, &mut log)
            .await;
        assert_eq!(watcher.state().active["ENA"].spec, "trail=3,part=5:0.5");

        // +6% fires the take-part half (share 0.5 of what is held); then a fall through
        // the 3% trail sells the rest (share 1 of what is left).
        *prices.bid.lock() = dec!(1.06);
        let first = watcher
            .step(&empty, &[trip(None)], &prices, 105 * M + 5_000, &mut log)
            .await;
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].share, first[0].source), (dec!(0.5), "live"));
        assert_eq!(first[0].asset, "ENA");
        *prices.bid.lock() = dec!(1.02);
        let second = watcher
            .step(&empty, &[trip(None)], &prices, 105 * M + 10_000, &mut log)
            .await;
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].share, Decimal::ONE);

        // Claude's own plan replaces the default for the same position.
        let mut watcher = Watcher::new(WatchState::default(), dec!(5), Some(default));
        watcher
            .step(
                &book("trail=1", 104 * M),
                &[trip(None)],
                &prices,
                105 * M,
                &mut log,
            )
            .await;
        assert_eq!(watcher.state().active["ENA"].spec, "trail=1");
    }

    #[tokio::test]
    async fn cleared_replaced_and_orphaned_plans_are_finalized() {
        let bars: Vec<PriceBar> = (40..110).map(|m| bar(m, dec!(1.01), dec!(1))).collect();
        let prices = FakePrices {
            bars,
            bid: Mutex::new(dec!(1)),
        };
        let mut watcher = Watcher::new(WatchState::default(), dec!(5), None);
        let mut log: Vec<Value> = Vec::new();
        let first = book("trail=3", 101 * M);
        watcher
            .step(&first, &[trip(None)], &prices, 102 * M, &mut log)
            .await;

        let second = book("trail=5", 102 * M);
        watcher
            .step(&second, &[trip(None)], &prices, 103 * M, &mut log)
            .await;
        assert_eq!(events(&log), ["started", "cleared", "started"]);
        assert_eq!(watcher.state().active["ENA"].spec, "trail=5");

        watcher
            .step(
                &PlanBook::default(),
                &[trip(None)],
                &prices,
                104 * M,
                &mut log,
            )
            .await;
        assert_eq!(events(&log)[3], "cleared");
        assert!(watcher.state().active.is_empty());

        // No open position behind the plan.
        let orphan = book("trail=3", 105 * M);
        watcher.step(&orphan, &[], &prices, 106 * M, &mut log).await;
        assert_eq!(events(&log)[4], "orphaned");
        assert_eq!(watcher.state().closed_trials, 0);
    }

    #[tokio::test]
    async fn restart_does_not_log_catch_up_fills_twice() {
        let mut bars: Vec<PriceBar> = (40..101).map(|m| bar(m, dec!(1.01), dec!(1))).collect();
        bars.push(bar(101, dec!(1.20), dec!(1.15)));
        bars.push(bar(102, dec!(1.10), dec!(1.05))); // through the 1.164 line
        let prices = FakePrices {
            bars,
            bid: Mutex::new(dec!(1.05)),
        };
        let book = book("trail=3", 101 * M);
        let mut first: Vec<Value> = Vec::new();
        let mut watcher = Watcher::new(WatchState::default(), dec!(5), None);
        watcher
            .step(&book, &[trip(None)], &prices, 103 * M + 1, &mut first)
            .await;
        assert_eq!(events(&first), ["started", "would_sell"]);
        assert_eq!(first[1]["source"], "catch_up");

        let mut second: Vec<Value> = Vec::new();
        let mut restarted = Watcher::new(watcher.state().clone(), dec!(5), None);
        restarted
            .step(&book, &[trip(None)], &prices, 103 * M + 2, &mut second)
            .await;
        assert_eq!(events(&second), ["started"]);
        assert_eq!(restarted.state().active["ENA"].fills.len(), 1);
    }
}

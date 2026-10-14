//! `bb x-insighter-report`: weekly paper evaluation of x-signals per venue and
//! per arm (deterministic, haiku), with the pre-registered gate verdict.
//!
//! Every threshold, friction number and exit rule below is fixed by
//! `docs/signals/x-insighter-design.md` ("Evaluation" and "Pre-registered
//! gate"). Exits are simulated from `x-ticks`, `x-traction` and the forward
//! snapshots; each decision at time `t` sees only rows with timestamp <= `t`.
//! Nothing here can place an order.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context;
use bb_binance::spot::{AccountCommission, ApiRestrictions};
use bb_binance::{BinanceClient, BinanceConfig};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::schema::{
    HaikuStatus, Resolution, Venue, XHaikuCall, XPaths, XSignal, XTick, XTraction, load_rows,
};
use crate::common::{Clock, SystemClock, atomic_write_json, load_jsonl_objects};
use crate::error::SignalsError;
use crate::jsonv::{Object, float_or_none, int_or_zero, py_str};
use crate::snapshotter::SNAPSHOT_OFFSETS_SECONDS;
use crate::v2_report::{Distribution, distribution, fmt_pct, latest_snapshots, offset_label};

pub const REPORT_SCHEMA: &str = "x-report-v1";
pub const FREEZE_DATE_ENV: &str = "X_INSIGHTER_FREEZE_DATE";

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;
/// Time stop and horizon of `peak_upper_bound`.
const POSITION_HORIZON_MS: i64 = DAY_MS;
/// Lead-time and hit-rate window.
const OUTCOME_WINDOW_MS: i64 = 6 * HOUR_MS;
/// A "move" is the first observed price >= 1.3 x `P_ref`.
const MOVE_MULTIPLE: f64 = 1.3;
const HIT_TARGET: f64 = 1.30;
const HIT_STOP: f64 = 0.85;
const EXHAUSTION_SCALE_OUT: f64 = 0.4;
const EXHAUSTION_FULL_EXIT: f64 = 0.7;
const SCALE_OUT_FRACTION: f64 = 0.5;
/// Solana rug precursor: liquidity down >= 30% from its max since signal.
const LIQUIDITY_PRECURSOR_RATIO: f64 = 0.70;
/// V2 rug proxy.
const RUG_LIQUIDITY_FLOOR_USD: f64 = 1_000.0;
const RUG_LIQUIDITY_FRACTION: f64 = 0.2;

/// Audit model notional, used as the per-trade paper size on both venues.
pub const PAPER_NOTIONAL_USD: f64 = 15.63;
const SOLANA_ROUND_TRIP_FIXED_USD: f64 = 0.50;
const SOLANA_SLIPPAGE: f64 = 0.02;
pub const BINANCE_DEFAULT_TAKER: f64 = 0.001;
const BINANCE_SLIPPAGE: f64 = 0.0005;
const BPS: f64 = 10_000.0;

const GATE_MIN_DAYS: i64 = 28;
const GATE_MIN_SIGNALS: usize = 100;
const GATE_MAX_RUG_RATE: f64 = 0.453;
const GATE_MIN_LEAD_MINUTES: f64 = 10.0;

const VENUES: [Venue; 2] = [Venue::Binance, Venue::Solana];
const SCHEDULED_EXITS: [(&str, i64); 3] = [
    ("scheduled_15m", 15 * 60),
    ("scheduled_1h", 60 * 60),
    ("scheduled_6h", 6 * 60 * 60),
];
const ARM_KEYS: [&str; 6] = [
    "rule_exits",
    "trailing_only",
    "scheduled_15m",
    "scheduled_1h",
    "scheduled_6h",
    "peak_upper_bound",
];

// ---------------------------------------------------------------------------
// Friction
// ---------------------------------------------------------------------------

/// Round-trip cost model (design doc "Friction model").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Friction {
    pub entry_cost: f64,
    pub exit_cost: f64,
    pub fixed_usd: f64,
    pub notional_usd: f64,
}

impl Friction {
    /// Audit model: $15.63 notional, $0.50 round-trip fixed, 2% slippage per leg.
    pub const fn solana() -> Self {
        Self {
            entry_cost: SOLANA_SLIPPAGE,
            exit_cost: SOLANA_SLIPPAGE,
            fixed_usd: SOLANA_ROUND_TRIP_FIXED_USD,
            notional_usd: PAPER_NOTIONAL_USD,
        }
    }

    /// Taker fee + half the detection spread + 5 bps slippage, per side.
    /// An unknown spread contributes nothing (counted in the report).
    pub fn binance(taker_fee: f64, spread_bps: Option<f64>) -> Self {
        let half_spread = spread_bps
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or(0.0)
            / 2.0
            / BPS;
        let side = taker_fee + half_spread + BINANCE_SLIPPAGE;
        Self {
            entry_cost: side,
            exit_cost: side,
            fixed_usd: 0.0,
            notional_usd: PAPER_NOTIONAL_USD,
        }
    }

    /// `r * (1 - entry_cost) * (1 - exit_cost) - 1 - fixed / notional`.
    pub fn net_return(&self, exit_to_entry_ratio: f64) -> f64 {
        let fixed = if self.notional_usd > 0.0 {
            self.fixed_usd / self.notional_usd
        } else {
            0.0
        };
        exit_to_entry_ratio * (1.0 - self.entry_cost) * (1.0 - self.exit_cost) - 1.0 - fixed
    }
}

/// Binance taker fees: 10 bps unless the report ran with verified read-only
/// credentials and fetched the per-symbol commission.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BinanceFees {
    pub per_symbol_taker: HashMap<String, f64>,
}

impl BinanceFees {
    pub fn taker(&self, symbol: Option<&str>) -> f64 {
        symbol
            .and_then(|s| self.per_symbol_taker.get(s).copied())
            .unwrap_or(BINANCE_DEFAULT_TAKER)
    }
}

fn read_only(r: &ApiRestrictions) -> bool {
    r.enable_reading
        && !r.enable_spot_and_margin_trading
        && !r.enable_withdrawals
        && !r.enable_internal_transfer
        && !r.enable_margin
        && !r.enable_futures
        && !r.permits_universal_transfer
        && !r.enable_vanilla_options
        && !r.enable_portfolio_margin_trading
}

fn commission_taker(c: &AccountCommission) -> f64 {
    let special = c
        .special_commission
        .as_ref()
        .map_or(rust_decimal::Decimal::ZERO, |s| s.taker);
    bb_core::money::to_f64(c.standard_commission.taker + c.tax_commission.taker + special)
}

/// Per-symbol taker fees when `BINANCE_API_KEY` credentials are configured
/// for mainnet and Binance reports them as read-only; else the 10 bps default.
async fn load_binance_fees(signals: &[XSignal]) -> BinanceFees {
    let mut fees = BinanceFees::default();
    let symbols: BTreeSet<&str> = signals
        .iter()
        .filter(|s| s.venue == Venue::Binance)
        .filter_map(|s| s.symbol.as_deref())
        .collect();
    if symbols.is_empty() {
        return fees;
    }
    let config = match BinanceConfig::from_env(false) {
        Ok(config) if config.credentials.is_some() && !config.testnet => config,
        Ok(_) => return fees,
        Err(err) => {
            tracing::warn!(%err, "Binance config unavailable; using the 10 bps taker default");
            return fees;
        }
    };
    let client = match BinanceClient::new(config) {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(%err, "Binance client unavailable; using the 10 bps taker default");
            return fees;
        }
    };
    match client.api_restrictions().await {
        Ok(r) if read_only(&r) => {}
        Ok(_) => {
            tracing::info!(
                "Binance credentials are not read-only; report uses the 10 bps taker default"
            );
            return fees;
        }
        Err(err) => {
            tracing::warn!(%err, "Binance apiRestrictions failed; using the 10 bps taker default");
            return fees;
        }
    }
    for symbol in symbols {
        match client.account_commission(symbol).await {
            Ok(c) => {
                fees.per_symbol_taker
                    .insert(symbol.to_owned(), commission_taker(&c));
            }
            Err(err) => tracing::warn!(%err, symbol, "Binance commission lookup failed"),
        }
    }
    fees
}

// ---------------------------------------------------------------------------
// Observed path
// ---------------------------------------------------------------------------

/// One observed row after detection: a tick, a snapshot or a traction row.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Observation {
    pub ts_ms: i64,
    /// Positive observed price, when the row has one.
    pub price: Option<f64>,
    pub liquidity: Option<f64>,
    /// Pair gone (Solana) or symbol not `TRADING` (Binance).
    pub market_gone: bool,
    /// Traction rows only.
    pub exhaustion: Option<f64>,
    /// Traction row listed at least one rug precursor.
    pub rug_precursor: bool,
}

impl Observation {
    /// The row's price; a gone market without a price is worth 0 to a holder.
    pub fn observed_price(&self) -> Option<f64> {
        self.price.or_else(|| self.market_gone.then_some(0.0))
    }

    fn from_tick(t: &XTick) -> Self {
        Self {
            ts_ms: t.ts_ms,
            price: t.price_usd.filter(|p| *p > 0.0),
            liquidity: t.liquidity_usd,
            market_gone: !t.pair_exists,
            ..Self::default()
        }
    }

    fn from_snapshot(s: &Object) -> Self {
        Self {
            ts_ms: int_or_zero(s.get("captured_at_ms")),
            price: float_or_none(s.get("price_usd")).filter(|p| *p > 0.0),
            liquidity: float_or_none(s.get("liquidity_usd")),
            market_gone: s.get("pair_exists") == Some(&Value::Bool(false)),
            ..Self::default()
        }
    }

    fn from_traction(t: &XTraction) -> Self {
        Self {
            ts_ms: t.ts_ms,
            price: t.features.price_usd.filter(|p| *p > 0.0),
            liquidity: t.features.liquidity_usd,
            market_gone: false,
            exhaustion: Some(t.exhaustion),
            rug_precursor: !t.rug_precursors.is_empty(),
        }
    }
}

fn in_window(o: &Observation, start_ms: i64, len_ms: i64) -> bool {
    o.ts_ms > start_ms && o.ts_ms <= start_ms + len_ms
}

// ---------------------------------------------------------------------------
// Exit simulation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    ExhaustionScaleOut,
    ExhaustionFull,
    RugPrecursor,
    HardStop,
    TrailingStop,
    TimeStop,
    Scheduled,
    Peak,
}

impl ExitReason {
    fn is_exhaustion(self) -> bool {
        matches!(self, Self::ExhaustionScaleOut | Self::ExhaustionFull)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExitLeg {
    pub ts_ms: i64,
    pub fraction: f64,
    pub price_ratio: f64,
    pub reason: ExitReason,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    /// Time of the final exit leg.
    pub exit_ms: i64,
    pub legs: Vec<ExitLeg>,
    /// Size-weighted exit / entry price ratio.
    pub gross_ratio: f64,
    pub net_return: f64,
}

impl Trade {
    fn from_legs(legs: Vec<ExitLeg>, friction: &Friction) -> Self {
        let gross_ratio: f64 = legs.iter().map(|l| l.fraction * l.price_ratio).sum();
        Self {
            exit_ms: legs.last().map_or(0, |l| l.ts_ms),
            net_return: friction.net_return(gross_ratio),
            gross_ratio,
            legs,
        }
    }

    pub fn net_usd(&self, friction: &Friction) -> f64 {
        self.net_return * friction.notional_usd
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SimOutcome {
    Closed(Trade),
    /// Still inside the 24h horizon with size left.
    Open,
    /// No price observed after entry (no ticks, snapshots or priced traction rows).
    NoPath,
}

impl SimOutcome {
    pub fn trade(&self) -> Option<&Trade> {
        match self {
            Self::Closed(t) => Some(t),
            Self::Open | Self::NoPath => None,
        }
    }
}

/// Which rule families an exit arm uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitRules {
    pub exhaustion: bool,
    pub rug_precursors: bool,
}

/// `rule_exits`: all rules (the gated arm).
pub const RULE_EXITS: ExitRules = ExitRules {
    exhaustion: true,
    rug_precursors: true,
};
/// `trailing_only`: trailing, hard and time stops only.
pub const TRAILING_ONLY: ExitRules = ExitRules {
    exhaustion: false,
    rug_precursors: false,
};

/// Entry state of one paper position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    pub venue: Venue,
    pub ts_ms: i64,
    pub price: f64,
    pub liquidity: Option<f64>,
}

const fn stops(venue: Venue) -> (f64, f64) {
    // (trailing below peak, hard stop below entry)
    match venue {
        Venue::Solana => (0.25, 0.30),
        Venue::Binance => (0.10, 0.12),
    }
}

/// Walk the observed path in time order and apply the exit rules. Each
/// decision uses only the rows up to its own timestamp; fills are at the price
/// of the row that triggers (the last observed price when that row has none).
/// Precedence on one row: rug precursor, hard stop, trailing stop, exhaustion.
pub fn simulate_exits(
    entry: &Entry,
    path: &[Observation],
    rules: ExitRules,
    friction: &Friction,
    now_ms: i64,
) -> SimOutcome {
    let (trailing, hard) = stops(entry.venue);
    let mut remaining = 1.0_f64;
    let mut scaled_out = false;
    let mut last_price = entry.price;
    let mut peak = entry.price;
    let mut max_liquidity = entry.liquidity.unwrap_or(0.0);
    let mut observed_any = false;
    let mut legs: Vec<ExitLeg> = Vec::new();
    let exit = |legs: &mut Vec<ExitLeg>, ts_ms: i64, fraction: f64, price: f64, reason| {
        legs.push(ExitLeg {
            ts_ms,
            fraction,
            price_ratio: price / entry.price,
            reason,
        });
    };

    for o in path
        .iter()
        .filter(|o| in_window(o, entry.ts_ms, POSITION_HORIZON_MS))
    {
        let price = o.observed_price();
        if let Some(p) = price {
            last_price = p;
            peak = peak.max(p);
            observed_any = true;
        }
        if rules.rug_precursors {
            let liquidity_drop = entry.venue == Venue::Solana
                && o.liquidity.is_some_and(|l| {
                    max_liquidity > 0.0 && l <= max_liquidity * LIQUIDITY_PRECURSOR_RATIO
                });
            if o.market_gone || o.rug_precursor || liquidity_drop {
                exit(
                    &mut legs,
                    o.ts_ms,
                    remaining,
                    last_price,
                    ExitReason::RugPrecursor,
                );
                remaining = 0.0;
                break;
            }
        }
        if let Some(l) = o.liquidity {
            max_liquidity = max_liquidity.max(l);
        }
        if price.is_some() {
            if last_price <= entry.price * (1.0 - hard) {
                exit(
                    &mut legs,
                    o.ts_ms,
                    remaining,
                    last_price,
                    ExitReason::HardStop,
                );
                remaining = 0.0;
                break;
            }
            if last_price <= peak * (1.0 - trailing) {
                exit(
                    &mut legs,
                    o.ts_ms,
                    remaining,
                    last_price,
                    ExitReason::TrailingStop,
                );
                remaining = 0.0;
                break;
            }
        }
        if rules.exhaustion
            && let Some(x) = o.exhaustion
        {
            if x >= EXHAUSTION_FULL_EXIT {
                exit(
                    &mut legs,
                    o.ts_ms,
                    remaining,
                    last_price,
                    ExitReason::ExhaustionFull,
                );
                remaining = 0.0;
                break;
            }
            if x >= EXHAUSTION_SCALE_OUT && !scaled_out {
                exit(
                    &mut legs,
                    o.ts_ms,
                    SCALE_OUT_FRACTION,
                    last_price,
                    ExitReason::ExhaustionScaleOut,
                );
                remaining -= SCALE_OUT_FRACTION;
                scaled_out = true;
            }
        }
    }

    if remaining > 0.0 {
        if now_ms < entry.ts_ms + POSITION_HORIZON_MS {
            return SimOutcome::Open;
        }
        if !observed_any {
            return SimOutcome::NoPath;
        }
        exit(
            &mut legs,
            entry.ts_ms + POSITION_HORIZON_MS,
            remaining,
            last_price,
            ExitReason::TimeStop,
        );
    }
    SimOutcome::Closed(Trade::from_legs(legs, friction))
}

/// Best observed price within the 24h horizon and the first time it was seen.
fn observed_peak(entry: &Entry, path: &[Observation]) -> Option<(f64, i64)> {
    let mut best: Option<(f64, i64)> = None;
    for o in path
        .iter()
        .filter(|o| in_window(o, entry.ts_ms, POSITION_HORIZON_MS))
    {
        if let Some(p) = o.observed_price()
            && best.is_none_or(|(b, _)| p > b)
        {
            best = Some((p, o.ts_ms));
        }
    }
    best
}

/// `peak_upper_bound`: exit in full at the best observed price within 24h.
fn peak_trade(entry: &Entry, path: &[Observation], friction: &Friction, now_ms: i64) -> SimOutcome {
    if now_ms < entry.ts_ms + POSITION_HORIZON_MS {
        return SimOutcome::Open;
    }
    observed_peak(entry, path).map_or(SimOutcome::NoPath, |(price, ts_ms)| {
        SimOutcome::Closed(Trade::from_legs(
            vec![ExitLeg {
                ts_ms,
                fraction: 1.0,
                price_ratio: price / entry.price,
                reason: ExitReason::Peak,
            }],
            friction,
        ))
    })
}

/// Scheduled full exit at the captured snapshot of one offset.
fn scheduled_trade(entry: &Entry, snapshot: Option<&Object>, friction: &Friction) -> Option<Trade> {
    let o = Observation::from_snapshot(snapshot?);
    let price = o.observed_price()?;
    Some(Trade::from_legs(
        vec![ExitLeg {
            ts_ms: o.ts_ms,
            fraction: 1.0,
            price_ratio: price / entry.price,
            reason: ExitReason::Scheduled,
        }],
        friction,
    ))
}

// ---------------------------------------------------------------------------
// Outcome metrics per signal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lead {
    /// No `P_ref` or detection price.
    Unknown,
    /// Price already >= 1.3 x `P_ref` at detection: lead 0, late.
    Late,
    Move {
        minutes: f64,
    },
    NoMove,
    /// No move yet and the 6h window is still open.
    Pending,
}

pub fn lead_time(entry: &Entry, ref_price: Option<f64>, path: &[Observation], now_ms: i64) -> Lead {
    let Some(reference) = ref_price.filter(|p| *p > 0.0) else {
        return Lead::Unknown;
    };
    let target = MOVE_MULTIPLE * reference;
    if entry.price >= target {
        return Lead::Late;
    }
    for o in path
        .iter()
        .filter(|o| in_window(o, entry.ts_ms, OUTCOME_WINDOW_MS))
    {
        if o.observed_price().is_some_and(|p| p >= target) {
            return Lead::Move {
                minutes: (o.ts_ms - entry.ts_ms) as f64 / MINUTE_MS as f64,
            };
        }
    }
    if now_ms >= entry.ts_ms + OUTCOME_WINDOW_MS {
        Lead::NoMove
    } else {
        Lead::Pending
    }
}

/// `Some(true)` when +30% from detection is reached before -15% within 6h;
/// `None` while undecided inside the window.
pub fn hit_outcome(entry: &Entry, path: &[Observation], now_ms: i64) -> Option<bool> {
    for o in path
        .iter()
        .filter(|o| in_window(o, entry.ts_ms, OUTCOME_WINDOW_MS))
    {
        let Some(p) = o.observed_price() else {
            continue;
        };
        if p >= entry.price * HIT_TARGET {
            return Some(true);
        }
        if p <= entry.price * HIT_STOP {
            return Some(false);
        }
    }
    (now_ms >= entry.ts_ms + OUTCOME_WINDOW_MS).then_some(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RugOutcome {
    NoSnapshots,
    Clean,
    /// First rug-proxy snapshot (by capture time).
    Rug {
        at_ms: i64,
    },
}

/// V2 rug proxy by any snapshot: pair gone (Binance: symbol left `TRADING`),
/// liquidity < $1k, or liquidity down > 80% from detection.
pub fn rug_outcome(detection_liquidity: Option<f64>, snapshots: &[&Object]) -> RugOutcome {
    let mut first: Option<i64> = None;
    for s in snapshots {
        let o = Observation::from_snapshot(s);
        let rug = o.market_gone
            || o.liquidity.is_some_and(|l| {
                l < RUG_LIQUIDITY_FLOOR_USD
                    || detection_liquidity
                        .is_some_and(|d| d > 0.0 && l < RUG_LIQUIDITY_FRACTION * d)
            });
        if rug && first.is_none_or(|f| o.ts_ms < f) {
            first = Some(o.ts_ms);
        }
    }
    match (first, snapshots.is_empty()) {
        (Some(at_ms), _) => RugOutcome::Rug { at_ms },
        (None, true) => RugOutcome::NoSnapshots,
        (None, false) => RugOutcome::Clean,
    }
}

fn market_f64(signal: &XSignal, key: &str) -> Option<f64> {
    float_or_none(signal.market.get(key)).filter(|v| v.is_finite())
}

fn detection_price(signal: &XSignal) -> Option<f64> {
    market_f64(signal, "price_usd")
        .or(signal.features.price_usd)
        .filter(|p| *p > 0.0)
}

fn detection_spread_bps(signal: &XSignal) -> Option<f64> {
    market_f64(signal, "spread_bps")
        .or(signal.features.spread_bps)
        .or_else(|| {
            let bid = market_f64(signal, "bid")?;
            let ask = market_f64(signal, "ask")?;
            let mid = f64::midpoint(bid, ask);
            (bid > 0.0 && ask >= bid).then(|| (ask - bid) / mid * BPS)
        })
}

/// Everything the report derives for one signal.
#[derive(Debug, Clone)]
pub struct SignalEval<'a> {
    pub signal: &'a XSignal,
    pub entry: Option<Entry>,
    pub friction: Friction,
    pub spread_known: bool,
    pub lead: Lead,
    pub hit: Option<bool>,
    pub rug: RugOutcome,
    pub path_returns: Vec<(i64, f64)>,
    pub rule_exits: SimOutcome,
    pub trailing_only: SimOutcome,
    pub scheduled: [Option<Trade>; 3],
    pub peak: SimOutcome,
    /// Best observed price / entry within 24h and its time.
    pub observed_peak: Option<(f64, i64)>,
}

impl SignalEval<'_> {
    fn arm_trade(&self, key: &str) -> Option<&Trade> {
        match key {
            "rule_exits" => self.rule_exits.trade(),
            "trailing_only" => self.trailing_only.trade(),
            "peak_upper_bound" => self.peak.trade(),
            other => SCHEDULED_EXITS
                .iter()
                .position(|(k, _)| *k == other)
                .and_then(|i| self.scheduled[i].as_ref()),
        }
    }
}

/// Raw report inputs.
#[derive(Debug, Clone, Default)]
pub struct ReportInputs {
    pub signals: Vec<XSignal>,
    pub snapshots: Vec<Object>,
    pub ticks: Vec<XTick>,
    pub traction: Vec<XTraction>,
    pub haiku: Vec<XHaikuCall>,
}

impl ReportInputs {
    pub fn load(paths: &XPaths) -> Self {
        Self {
            signals: load_rows(&paths.signals),
            snapshots: load_jsonl_objects(&paths.snapshots),
            ticks: load_rows(&paths.ticks),
            traction: load_rows(&paths.traction),
            haiku: load_rows(&paths.haiku),
        }
    }
}

fn evaluate_signals<'a>(
    inputs: &'a ReportInputs,
    fees: &BinanceFees,
    now_ms: i64,
) -> Vec<SignalEval<'a>> {
    let latest = latest_snapshots(&inputs.snapshots);
    let mut snaps_by_id: HashMap<&str, Vec<(i64, &Object)>> = HashMap::new();
    for ((id, offset), snap) in &latest {
        if py_str(snap.get("status")) != "api_error" {
            snaps_by_id
                .entry(id.as_str())
                .or_default()
                .push((*offset, *snap));
        }
    }
    let mut ticks: HashMap<&str, Vec<&XTick>> = HashMap::new();
    for t in &inputs.ticks {
        ticks.entry(t.token_key.as_str()).or_default().push(t);
    }
    let mut traction: HashMap<&str, Vec<&XTraction>> = HashMap::new();
    for t in &inputs.traction {
        traction.entry(t.token_key.as_str()).or_default().push(t);
    }

    inputs
        .signals
        .iter()
        .map(|signal| {
            let mut snaps = snaps_by_id
                .get(signal.detection_id.as_str())
                .cloned()
                .unwrap_or_default();
            snaps.sort_by_key(|(offset, _)| *offset);
            let snap_rows: Vec<&Object> = snaps.iter().map(|(_, s)| *s).collect();
            let mut path: Vec<Observation> = Vec::new();
            path.extend(
                ticks
                    .get(signal.token_key.as_str())
                    .into_iter()
                    .flatten()
                    .map(|t| Observation::from_tick(t)),
            );
            path.extend(snap_rows.iter().map(|s| Observation::from_snapshot(s)));
            path.extend(
                traction
                    .get(signal.token_key.as_str())
                    .into_iter()
                    .flatten()
                    .map(|t| Observation::from_traction(t)),
            );
            path.retain(|o| o.ts_ms > signal.detected_at_ms);
            path.sort_by_key(|o| o.ts_ms);
            evaluate_signal(signal, &path, &snaps, fees, now_ms)
        })
        .collect()
}

fn evaluate_signal<'a>(
    signal: &'a XSignal,
    path: &[Observation],
    snaps: &[(i64, &Object)],
    fees: &BinanceFees,
    now_ms: i64,
) -> SignalEval<'a> {
    let spread = detection_spread_bps(signal);
    let friction = match signal.venue {
        Venue::Solana => Friction::solana(),
        Venue::Binance => Friction::binance(fees.taker(signal.symbol.as_deref()), spread),
    };
    let liquidity = market_f64(signal, "liquidity_usd").or(signal.features.liquidity_usd);
    let snap_rows: Vec<&Object> = snaps.iter().map(|(_, s)| *s).collect();
    let rug = rug_outcome(liquidity, &snap_rows);
    let entry = detection_price(signal).map(|price| Entry {
        venue: signal.venue,
        ts_ms: signal.detected_at_ms,
        price,
        liquidity,
    });
    let Some(entry) = entry else {
        return SignalEval {
            signal,
            entry: None,
            friction,
            spread_known: spread.is_some(),
            lead: Lead::Unknown,
            hit: None,
            rug,
            path_returns: Vec::new(),
            rule_exits: SimOutcome::NoPath,
            trailing_only: SimOutcome::NoPath,
            scheduled: [None, None, None],
            peak: SimOutcome::NoPath,
            observed_peak: None,
        };
    };
    let snapshot_at = |offset: i64| snaps.iter().find(|(o, _)| *o == offset).map(|(_, s)| *s);
    let path_returns = snaps
        .iter()
        .filter_map(|(offset, s)| {
            let price = float_or_none(s.get("price_usd")).filter(|p| *p > 0.0)?;
            Some((*offset, price / entry.price - 1.0))
        })
        .collect();
    SignalEval {
        signal,
        entry: Some(entry),
        friction,
        spread_known: spread.is_some(),
        lead: lead_time(&entry, market_f64(signal, "price_ref_1h_usd"), path, now_ms),
        hit: hit_outcome(&entry, path, now_ms),
        rug,
        path_returns,
        rule_exits: simulate_exits(&entry, path, RULE_EXITS, &friction, now_ms),
        trailing_only: simulate_exits(&entry, path, TRAILING_ONLY, &friction, now_ms),
        scheduled: SCHEDULED_EXITS
            .map(|(_, offset)| scheduled_trade(&entry, snapshot_at(offset), &friction)),
        peak: peak_trade(&entry, path, &friction, now_ms),
        observed_peak: observed_peak(&entry, path).map(|(p, ts)| (p / entry.price, ts)),
    }
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// Friction-adjusted P&L of one exit arm (JSON `arms.<arm>.<exit>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmStats {
    pub trades: usize,
    pub total_net_usd: f64,
    pub mean_net_return: Option<f64>,
    pub median_net_return: Option<f64>,
    pub win_rate: Option<f64>,
    pub total_net_usd_drop_top2: f64,
}

fn median(values: &[f64]) -> Option<f64> {
    distribution(values.to_vec(), 1).median
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn rate(count: usize, total: usize) -> Option<f64> {
    (total > 0).then(|| count as f64 / total as f64)
}

/// Stats over `(net_return, net_usd)` pairs.
pub fn arm_stats(trades: &[(f64, f64)]) -> ArmStats {
    let returns: Vec<f64> = trades.iter().map(|(r, _)| *r).collect();
    let mut usd: Vec<f64> = trades.iter().map(|(_, u)| *u).collect();
    usd.sort_by(|a, b| b.total_cmp(a));
    ArmStats {
        trades: trades.len(),
        total_net_usd: usd.iter().sum(),
        mean_net_return: mean(&returns),
        median_net_return: median(&returns),
        win_rate: rate(returns.iter().filter(|r| **r > 0.0).count(), returns.len()),
        total_net_usd_drop_top2: usd.iter().skip(2).sum(),
    }
}

fn exit_stats(evals: &[&SignalEval<'_>], key: &str) -> ArmStats {
    let trades: Vec<(f64, f64)> = evals
        .iter()
        .filter_map(|e| {
            e.arm_trade(key)
                .map(|t| (t.net_return, t.net_usd(&e.friction)))
        })
        .collect();
    arm_stats(&trades)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExitQuality {
    pub rule_capture: Option<f64>,
    pub trailing_capture: Option<f64>,
    pub exhaustion_before_peak: usize,
    pub exhaustion_after_peak: usize,
    pub rug_signals_closed: usize,
    pub rug_exited_before: usize,
    pub paired_trades: usize,
    pub rule_mean: Option<f64>,
    pub trailing_mean: Option<f64>,
}

/// Realized gross gain / best gain available, summed over trades whose peak
/// was above entry.
fn capture(
    evals: &[&SignalEval<'_>],
    outcome: impl Fn(&SignalEval<'_>) -> Option<f64>,
) -> Option<f64> {
    let (mut realized, mut best) = (0.0, 0.0);
    for e in evals {
        if let (Some(gross), Some((peak, _))) = (outcome(e), e.observed_peak)
            && peak > 1.0
        {
            realized += gross - 1.0;
            best += peak - 1.0;
        }
    }
    (best > 0.0).then(|| realized / best)
}

/// `rule_exits` vs `trailing_only` mean net return on signals closed in both.
fn paired_means(evals: &[&SignalEval<'_>]) -> (usize, Option<f64>, Option<f64>) {
    let pairs: Vec<(f64, f64)> = evals
        .iter()
        .filter_map(|e| {
            Some((
                e.rule_exits.trade()?.net_return,
                e.trailing_only.trade()?.net_return,
            ))
        })
        .collect();
    let rule: Vec<f64> = pairs.iter().map(|p| p.0).collect();
    let trailing: Vec<f64> = pairs.iter().map(|p| p.1).collect();
    (pairs.len(), mean(&rule), mean(&trailing))
}

fn exit_quality(evals: &[&SignalEval<'_>]) -> ExitQuality {
    let mut q = ExitQuality {
        rule_capture: capture(evals, |e| e.rule_exits.trade().map(|t| t.gross_ratio)),
        trailing_capture: capture(evals, |e| e.trailing_only.trade().map(|t| t.gross_ratio)),
        ..ExitQuality::default()
    };
    for e in evals {
        let Some(trade) = e.rule_exits.trade() else {
            continue;
        };
        if let Some((_, peak_ms)) = e.observed_peak {
            for leg in trade.legs.iter().filter(|l| l.reason.is_exhaustion()) {
                if leg.ts_ms < peak_ms {
                    q.exhaustion_before_peak += 1;
                } else {
                    q.exhaustion_after_peak += 1;
                }
            }
        }
        if let RugOutcome::Rug { at_ms } = e.rug {
            q.rug_signals_closed += 1;
            if trade.exit_ms < at_ms {
                q.rug_exited_before += 1;
            }
        }
    }
    (q.paired_trades, q.rule_mean, q.trailing_mean) = paired_means(evals);
    q
}

/// Outcome metrics of one arm's signal set.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmSummary {
    pub signals: usize,
    pub hits: usize,
    pub hit_decided: usize,
    pub rugs: usize,
    pub rug_evaluable: usize,
    pub lead_minutes: Vec<f64>,
    pub late: usize,
    pub lead_evaluable: usize,
    pub path_returns: Vec<(i64, Distribution)>,
    pub exits: BTreeMap<String, ArmStats>,
    pub open: usize,
    pub no_path: usize,
    pub quality: ExitQuality,
}

impl ArmSummary {
    pub fn hit_rate(&self) -> Option<f64> {
        rate(self.hits, self.hit_decided)
    }
    pub fn rug_rate(&self) -> Option<f64> {
        rate(self.rugs, self.rug_evaluable)
    }
    pub fn late_share(&self) -> Option<f64> {
        rate(self.late, self.lead_evaluable)
    }
    pub fn median_lead_minutes(&self) -> Option<f64> {
        median(&self.lead_minutes)
    }
    fn exit(&self, key: &str) -> ArmStats {
        self.exits
            .get(key)
            .cloned()
            .unwrap_or_else(|| arm_stats(&[]))
    }
}

pub fn summarize(evals: &[&SignalEval<'_>]) -> ArmSummary {
    let mut s = ArmSummary {
        signals: evals.len(),
        hits: 0,
        hit_decided: 0,
        rugs: 0,
        rug_evaluable: 0,
        lead_minutes: Vec::new(),
        late: 0,
        lead_evaluable: 0,
        path_returns: Vec::new(),
        exits: ARM_KEYS
            .iter()
            .map(|k| ((*k).to_owned(), exit_stats(evals, k)))
            .collect(),
        open: 0,
        no_path: 0,
        quality: exit_quality(evals),
    };
    let mut returns: BTreeMap<i64, Vec<f64>> = SNAPSHOT_OFFSETS_SECONDS
        .iter()
        .map(|o| (*o, Vec::new()))
        .collect();
    for e in evals {
        if let Some(hit) = e.hit {
            s.hit_decided += 1;
            s.hits += usize::from(hit);
        }
        match e.rug {
            RugOutcome::Rug { .. } => {
                s.rugs += 1;
                s.rug_evaluable += 1;
            }
            RugOutcome::Clean => s.rug_evaluable += 1,
            RugOutcome::NoSnapshots => {}
        }
        match e.lead {
            Lead::Late => {
                s.late += 1;
                s.lead_evaluable += 1;
                s.lead_minutes.push(0.0);
            }
            Lead::Move { minutes } => {
                s.lead_evaluable += 1;
                s.lead_minutes.push(minutes);
            }
            Lead::NoMove => s.lead_evaluable += 1,
            Lead::Unknown | Lead::Pending => {}
        }
        match e.rule_exits {
            SimOutcome::Open => s.open += 1,
            SimOutcome::NoPath => s.no_path += 1,
            SimOutcome::Closed(_) => {}
        }
        for (offset, r) in &e.path_returns {
            if let Some(bucket) = returns.get_mut(offset) {
                bucket.push(*r);
            }
        }
    }
    s.path_returns = returns
        .into_iter()
        .map(|(offset, values)| (offset, distribution(values, 1)))
        .collect();
    s
}

fn venue_of_token(token_key: &str) -> Option<Venue> {
    VENUES.into_iter().find(|v| {
        token_key
            .strip_prefix(v.as_str())
            .is_some_and(|rest| rest.starts_with(':'))
    })
}

fn llm_spend(calls: &[XHaikuCall], venue: Venue, since_ms: i64) -> f64 {
    calls
        .iter()
        .filter(|c| c.ts_ms >= since_ms && venue_of_token(&c.token_key) == Some(venue))
        .map(|c| c.usage.cost_usd)
        .sum()
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateCriterion {
    pub name: String,
    pub value: String,
    pub threshold: String,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateVerdict {
    pub passed: bool,
    pub criteria: Vec<GateCriterion>,
}

fn criterion(name: &str, value: String, threshold: &str, passed: bool) -> GateCriterion {
    GateCriterion {
        name: name.into(),
        value,
        threshold: threshold.into(),
        passed,
    }
}

fn freeze_start_ms(freeze: NaiveDate) -> i64 {
    freeze
        .and_hms_opt(0, 0, 0)
        .map_or(i64::MAX, |dt| dt.and_utc().timestamp_millis())
}

fn fmt_usd(v: f64) -> String {
    if v < 0.0 {
        format!("-${:.2}", -v)
    } else {
        format!("${v:.2}")
    }
}

fn fmt_opt_minutes(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".into(), |m| format!("{m:.1} min"))
}

const THRESHOLDS: [(&str, &str); 5] = [
    ("days_and_signals", ">= 28d / 100"),
    ("rule_exits_pnl", "> $0 / > $0 after dropping top 2"),
    ("rug_rate", "< 45.3%"),
    ("median_lead_time", ">= 10 min"),
    (
        "rule_exits_vs_trailing",
        "rule_exits mean > trailing_only mean",
    ),
];

/// Gate verdict for one venue over signals detected on/after the freeze date.
pub fn gate_verdict(
    evals: &[SignalEval<'_>],
    calls: &[XHaikuCall],
    venue: Venue,
    freeze: Option<NaiveDate>,
    report_date: NaiveDate,
) -> GateVerdict {
    let Some(freeze) = freeze else {
        return GateVerdict {
            passed: false,
            criteria: THRESHOLDS
                .iter()
                .map(|(name, threshold)| criterion(name, "no freeze date".into(), threshold, false))
                .collect(),
        };
    };
    let since_ms = freeze_start_ms(freeze);
    let det: Vec<&SignalEval<'_>> = evals
        .iter()
        .filter(|e| {
            e.signal.venue == venue && e.signal.floor_passed && e.signal.detected_at_ms >= since_ms
        })
        .collect();
    let haiku: Vec<&SignalEval<'_>> = det.iter().copied().filter(|e| e.signal.haiku_arm).collect();
    let summary = summarize(&det);
    let days = (report_date - freeze).num_days();

    let det_pnl = summary.exit("rule_exits");
    let haiku_pnl = exit_stats(&haiku, "rule_exits");
    let spend = llm_spend(calls, venue, since_ms);
    let det_passes = det_pnl.total_net_usd > 0.0 && det_pnl.total_net_usd_drop_top2 > 0.0;
    let haiku_passes =
        haiku_pnl.total_net_usd - spend > 0.0 && haiku_pnl.total_net_usd_drop_top2 - spend > 0.0;
    let haiku_gated = !det_passes && haiku_passes;
    let pnl_value = if haiku_gated {
        format!(
            "haiku {} / {} after dropping top 2 ({} LLM spend subtracted)",
            fmt_usd(haiku_pnl.total_net_usd - spend),
            fmt_usd(haiku_pnl.total_net_usd_drop_top2 - spend),
            fmt_usd(spend)
        )
    } else {
        format!(
            "deterministic {} / {} after dropping top 2",
            fmt_usd(det_pnl.total_net_usd),
            fmt_usd(det_pnl.total_net_usd_drop_top2)
        )
    };
    let gated: &[&SignalEval<'_>] = if haiku_gated { &haiku } else { &det };
    let (_, rule_mean, trailing_mean) = paired_means(gated);
    let rug_rate = summary.rug_rate();
    let lead = summary.median_lead_minutes();

    let criteria = vec![
        criterion(
            THRESHOLDS[0].0,
            format!("{days}d / {}", det.len()),
            THRESHOLDS[0].1,
            days >= GATE_MIN_DAYS && det.len() >= GATE_MIN_SIGNALS,
        ),
        criterion(
            THRESHOLDS[1].0,
            pnl_value,
            THRESHOLDS[1].1,
            det_passes || haiku_passes,
        ),
        criterion(
            THRESHOLDS[2].0,
            fmt_pct(rug_rate),
            THRESHOLDS[2].1,
            rug_rate.is_some_and(|r| r < GATE_MAX_RUG_RATE),
        ),
        criterion(
            THRESHOLDS[3].0,
            fmt_opt_minutes(lead),
            THRESHOLDS[3].1,
            lead.is_some_and(|m| m >= GATE_MIN_LEAD_MINUTES),
        ),
        criterion(
            THRESHOLDS[4].0,
            format!(
                "{}{} vs {}",
                if haiku_gated { "haiku arm: " } else { "" },
                fmt_pct(rule_mean),
                fmt_pct(trailing_mean)
            ),
            THRESHOLDS[4].1,
            matches!((rule_mean, trailing_mean), (Some(r), Some(t)) if r > t),
        ),
    ];
    GateVerdict {
        passed: criteria.iter().all(|c| c.passed),
        criteria,
    }
}

// ---------------------------------------------------------------------------
// Report assembly
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts_ms: i64,
    pub cumulative_net_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueArms {
    pub deterministic: BTreeMap<String, ArmStats>,
    pub haiku: BTreeMap<String, ArmStats>,
}

/// `venues.<venue>` of `x-insighter-report-latest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueJson {
    pub signals: usize,
    pub floor_passed: usize,
    pub hit_rate: Option<f64>,
    pub rug_rate: Option<f64>,
    pub median_lead_time_minutes: Option<f64>,
    pub late_share: Option<f64>,
    pub llm_spend_usd: f64,
    pub arms: VenueArms,
    pub equity_curve: Vec<EquityPoint>,
    pub gate: GateVerdict,
}

/// `x-insighter-report-latest.json` (design doc shape, schema `x-report-v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportJson {
    pub schema: String,
    pub generated_at_ms: i64,
    pub date: String,
    pub markdown_path: String,
    pub freeze_date: Option<String>,
    pub venues: BTreeMap<String, VenueJson>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportConfig {
    pub report_date: NaiveDate,
    pub freeze_date: Option<NaiveDate>,
    pub now_ms: i64,
    pub fees: BinanceFees,
}

/// Everything rendered for one venue.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueReport {
    pub venue: Venue,
    pub signals: usize,
    pub floor_passed: usize,
    pub signal_status: BTreeMap<&'static str, usize>,
    pub call_status: BTreeMap<&'static str, usize>,
    pub llm_spend_usd: f64,
    pub unknown_spread: usize,
    pub deterministic: ArmSummary,
    pub haiku: ArmSummary,
    pub ablation_signals: usize,
    pub ablation_deterministic: ArmStats,
    pub ablation_haiku: ArmStats,
    pub ablation_haiku_signals: usize,
    pub by_resolution: Vec<(Resolution, usize, ArmSummary)>,
    pub equity_curve: Vec<EquityPoint>,
    pub gate: GateVerdict,
}

const fn status_name(status: HaikuStatus) -> &'static str {
    match status {
        HaikuStatus::Ok => "ok",
        HaikuStatus::CostCap => "cost_cap",
        HaikuStatus::Error => "error",
    }
}

const fn resolution_name(r: Resolution) -> &'static str {
    match r {
        Resolution::MintInPost => "mint_in_post",
        Resolution::BinanceListing => "binance_listing",
        Resolution::CashtagLink => "cashtag_link",
        Resolution::DexscreenerSearch => "dexscreener_search",
    }
}

fn status_counts(statuses: impl Iterator<Item = HaikuStatus>) -> BTreeMap<&'static str, usize> {
    let mut counts: BTreeMap<&'static str, usize> = ["ok", "cost_cap", "error"]
        .into_iter()
        .map(|k| (k, 0))
        .collect();
    for s in statuses {
        *counts.entry(status_name(s)).or_insert(0) += 1;
    }
    counts
}

fn venue_report(
    evals: &[SignalEval<'_>],
    inputs: &ReportInputs,
    venue: Venue,
    config: &ReportConfig,
) -> VenueReport {
    let all: Vec<&SignalEval<'_>> = evals.iter().filter(|e| e.signal.venue == venue).collect();
    let det: Vec<&SignalEval<'_>> = all
        .iter()
        .copied()
        .filter(|e| e.signal.floor_passed)
        .collect();
    let haiku: Vec<&SignalEval<'_>> = det.iter().copied().filter(|e| e.signal.haiku_arm).collect();
    let ok: Vec<&SignalEval<'_>> = det
        .iter()
        .copied()
        .filter(|e| e.signal.haiku_status == HaikuStatus::Ok)
        .collect();
    let ok_haiku: Vec<&SignalEval<'_>> =
        ok.iter().copied().filter(|e| e.signal.haiku_arm).collect();

    let mut curve: Vec<(i64, f64)> = det
        .iter()
        .filter_map(|e| {
            e.rule_exits
                .trade()
                .map(|t| (t.exit_ms, t.net_usd(&e.friction)))
        })
        .collect();
    curve.sort_by_key(|(ts, _)| *ts);
    let mut cumulative = 0.0;
    let equity_curve = curve
        .into_iter()
        .map(|(ts_ms, usd)| {
            cumulative += usd;
            EquityPoint {
                ts_ms,
                cumulative_net_usd: cumulative,
            }
        })
        .collect();

    let by_resolution = if venue == Venue::Solana {
        let resolutions: BTreeSet<&'static str> = all
            .iter()
            .map(|e| resolution_name(e.signal.resolution))
            .collect();
        resolutions
            .into_iter()
            .filter_map(|name| {
                let members: Vec<&SignalEval<'_>> = all
                    .iter()
                    .copied()
                    .filter(|e| resolution_name(e.signal.resolution) == name)
                    .collect();
                let resolution = members.first()?.signal.resolution;
                let floor: Vec<&SignalEval<'_>> = members
                    .iter()
                    .copied()
                    .filter(|e| e.signal.floor_passed)
                    .collect();
                Some((resolution, members.len(), summarize(&floor)))
            })
            .collect()
    } else {
        Vec::new()
    };

    VenueReport {
        venue,
        signals: all.len(),
        floor_passed: det.len(),
        signal_status: status_counts(all.iter().map(|e| e.signal.haiku_status)),
        call_status: status_counts(
            inputs
                .haiku
                .iter()
                .filter(|c| venue_of_token(&c.token_key) == Some(venue))
                .map(|c| c.status),
        ),
        llm_spend_usd: llm_spend(&inputs.haiku, venue, i64::MIN),
        unknown_spread: if venue == Venue::Binance {
            det.iter().filter(|e| !e.spread_known).count()
        } else {
            0
        },
        deterministic: summarize(&det),
        haiku: summarize(&haiku),
        ablation_signals: ok.len(),
        ablation_deterministic: exit_stats(&ok, "rule_exits"),
        ablation_haiku: exit_stats(&ok_haiku, "rule_exits"),
        ablation_haiku_signals: ok_haiku.len(),
        by_resolution,
        equity_curve,
        gate: gate_verdict(
            evals,
            &inputs.haiku,
            venue,
            config.freeze_date,
            config.report_date,
        ),
    }
}

/// Compute every venue section.
pub fn compute_report(inputs: &ReportInputs, config: &ReportConfig) -> Vec<VenueReport> {
    let evals = evaluate_signals(inputs, &config.fees, config.now_ms);
    VENUES
        .into_iter()
        .map(|venue| venue_report(&evals, inputs, venue, config))
        .collect()
}

pub fn markdown_file_name(date: &str) -> String {
    format!("x-insighter-report-{date}.md")
}

pub fn build_json(venues: &[VenueReport], config: &ReportConfig) -> ReportJson {
    let date = config.report_date.format("%Y-%m-%d").to_string();
    ReportJson {
        schema: REPORT_SCHEMA.into(),
        generated_at_ms: config.now_ms,
        markdown_path: markdown_file_name(&date),
        date,
        freeze_date: config.freeze_date.map(|d| d.format("%Y-%m-%d").to_string()),
        venues: venues
            .iter()
            .map(|v| {
                (
                    v.venue.as_str().to_owned(),
                    VenueJson {
                        signals: v.signals,
                        floor_passed: v.floor_passed,
                        hit_rate: v.deterministic.hit_rate(),
                        rug_rate: v.deterministic.rug_rate(),
                        median_lead_time_minutes: v.deterministic.median_lead_minutes(),
                        late_share: v.deterministic.late_share(),
                        llm_spend_usd: v.llm_spend_usd,
                        arms: VenueArms {
                            deterministic: v.deterministic.exits.clone(),
                            haiku: v.haiku.exits.clone(),
                        },
                        equity_curve: v.equity_curve.clone(),
                        gate: v.gate.clone(),
                    },
                )
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn venue_title(venue: Venue) -> &'static str {
    match venue {
        Venue::Binance => "Binance Spot",
        Venue::Solana => "Solana",
    }
}

fn pnl_row(arm: &str, key: &str, s: &ArmStats) -> String {
    format!(
        "| {arm} | {key} | {} | {} | {} | {} | {} | {} |",
        s.trades,
        fmt_usd(s.total_net_usd),
        fmt_pct(s.mean_net_return),
        fmt_pct(s.median_net_return),
        fmt_pct(s.win_rate),
        fmt_usd(s.total_net_usd_drop_top2),
    )
}

fn outcome_row(arm: &str, s: &ArmSummary) -> String {
    format!(
        "| {arm} | {} | {} ({}/{}) | {} ({}/{}) | {} | {} ({}/{}) | {} | {} |",
        s.signals,
        fmt_pct(s.hit_rate()),
        s.hits,
        s.hit_decided,
        fmt_pct(s.rug_rate()),
        s.rugs,
        s.rug_evaluable,
        fmt_opt_minutes(s.median_lead_minutes()),
        fmt_pct(s.late_share()),
        s.late,
        s.lead_evaluable,
        s.open,
        s.no_path,
    )
}

fn render_venue(v: &VenueReport, lines: &mut Vec<String>) {
    let det = &v.deterministic;
    lines.extend([
        format!("## {}", venue_title(v.venue)),
        String::new(),
        format!("- Signals: {}", v.signals),
        format!(
            "- Floor passed: {} ({})",
            v.floor_passed,
            fmt_pct(rate(v.floor_passed, v.signals))
        ),
        format!(
            "- Signal Haiku status: ok {}, cost_cap {}, error {}",
            v.signal_status.get("ok").copied().unwrap_or(0),
            v.signal_status.get("cost_cap").copied().unwrap_or(0),
            v.signal_status.get("error").copied().unwrap_or(0),
        ),
        format!(
            "- Haiku calls: ok {}, cost_cap {}, error {}; LLM spend {}",
            v.call_status.get("ok").copied().unwrap_or(0),
            v.call_status.get("cost_cap").copied().unwrap_or(0),
            v.call_status.get("error").copied().unwrap_or(0),
            fmt_usd(v.llm_spend_usd),
        ),
    ]);
    if v.venue == Venue::Binance {
        lines.push(format!(
            "- Floor-passing signals without a detection spread (half-spread cost 0): {}",
            v.unknown_spread
        ));
    }
    lines.extend(
        [
            "",
            "### Outcomes",
            "",
            "| Arm | Signals | Hit rate | Rug rate | Median lead | Late share | Open | No path |",
            "|---|---:|---:|---:|---:|---:|---:|---:|",
        ]
        .map(String::from),
    );
    lines.push(outcome_row("deterministic", det));
    lines.push(outcome_row("haiku", &v.haiku));

    lines.extend(
        [
            "",
            "### Path Returns (deterministic arm)",
            "",
            "| Offset | Sample | P25 | Median | P75 | P90 |",
            "|---|---:|---:|---:|---:|---:|",
        ]
        .map(String::from),
    );
    for (offset, d) in &det.path_returns {
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} |",
            offset_label(*offset),
            d.sample_size,
            fmt_pct(d.p25),
            fmt_pct(d.median),
            fmt_pct(d.p75),
            fmt_pct(d.p90),
        ));
    }

    lines.extend(
        [
            "",
            "### Friction-Adjusted Paper P&L",
            "",
            "| Arm | Exit model | Trades | Total | Mean | Median | Win rate | Total drop top 2 |",
            "|---|---|---:|---:|---:|---:|---:|---:|",
        ]
        .map(String::from),
    );
    for (arm, summary) in [("deterministic", det), ("haiku", &v.haiku)] {
        for key in ARM_KEYS {
            lines.push(pnl_row(arm, key, &summary.exit(key)));
        }
    }
    lines.push(String::new());
    lines.push("`peak_upper_bound` is an upper bound, never expected return.".into());

    let abl_d = &v.ablation_deterministic;
    let abl_h = &v.ablation_haiku;
    lines.extend([
        String::new(),
        "### Ablation (signals with an ok Haiku verdict, rule_exits)".into(),
        String::new(),
        "| Arm | Signals | Trades | Total | Mean | Win rate | LLM spend | Total after LLM spend |"
            .into(),
        "|---|---:|---:|---:|---:|---:|---:|---:|".into(),
        format!(
            "| deterministic | {} | {} | {} | {} | {} | - | {} |",
            v.ablation_signals,
            abl_d.trades,
            fmt_usd(abl_d.total_net_usd),
            fmt_pct(abl_d.mean_net_return),
            fmt_pct(abl_d.win_rate),
            fmt_usd(abl_d.total_net_usd),
        ),
        format!(
            "| haiku | {} | {} | {} | {} | {} | {} | {} |",
            v.ablation_haiku_signals,
            abl_h.trades,
            fmt_usd(abl_h.total_net_usd),
            fmt_pct(abl_h.mean_net_return),
            fmt_pct(abl_h.win_rate),
            fmt_usd(v.llm_spend_usd),
            fmt_usd(abl_h.total_net_usd - v.llm_spend_usd),
        ),
    ]);

    let q = &det.quality;
    let exhaustion_total = q.exhaustion_before_peak + q.exhaustion_after_peak;
    lines.extend([
        String::new(),
        "### Exit Quality (deterministic arm)".into(),
        String::new(),
        format!(
            "- Peak capture ratio (gross realized gain / best gain within 24h): rule_exits {}, trailing_only {}",
            fmt_pct(q.rule_capture),
            fmt_pct(q.trailing_capture)
        ),
        format!(
            "- Exhaustion exits before the observed peak: {} ({}/{}); after: {} ({}/{})",
            fmt_pct(rate(q.exhaustion_before_peak, exhaustion_total)),
            q.exhaustion_before_peak,
            exhaustion_total,
            fmt_pct(rate(q.exhaustion_after_peak, exhaustion_total)),
            q.exhaustion_after_peak,
            exhaustion_total,
        ),
        format!(
            "- Rug-proxy signals exited before the rug snapshot: {} ({}/{})",
            fmt_pct(rate(q.rug_exited_before, q.rug_signals_closed)),
            q.rug_exited_before,
            q.rug_signals_closed
        ),
        format!(
            "- Mean net return on the same {} signals: rule_exits {} vs trailing_only {}",
            q.paired_trades,
            fmt_pct(q.rule_mean),
            fmt_pct(q.trailing_mean)
        ),
    ]);

    if !v.by_resolution.is_empty() {
        lines.extend(
            [
                "",
                "### By Resolution",
                "",
                "| Resolution | Signals | Floor passed | Rug rate | Hit rate | rule_exits trades | rule_exits total | rule_exits mean |",
                "|---|---:|---:|---:|---:|---:|---:|---:|",
            ]
            .map(String::from),
        );
        for (resolution, count, s) in &v.by_resolution {
            let pnl = s.exit("rule_exits");
            lines.push(format!(
                "| {} | {count} | {} | {} | {} | {} | {} | {} |",
                resolution_name(*resolution),
                s.signals,
                fmt_pct(s.rug_rate()),
                fmt_pct(s.hit_rate()),
                pnl.trades,
                fmt_usd(pnl.total_net_usd),
                fmt_pct(pnl.mean_net_return),
            ));
        }
    }

    lines.extend([
        String::new(),
        format!(
            "### Gate: {}",
            if v.gate.passed {
                "PASSED"
            } else {
                "not passed"
            }
        ),
        String::new(),
        "| Criterion | Value | Threshold | Passed |".into(),
        "|---|---|---|---|".into(),
    ]);
    for c in &v.gate.criteria {
        lines.push(format!(
            "| {} | {} | {} | {} |",
            c.name,
            c.value,
            c.threshold,
            if c.passed { "yes" } else { "no" }
        ));
    }
    lines.push(String::new());
}

pub fn render_markdown(venues: &[VenueReport], config: &ReportConfig) -> String {
    let date = config.report_date.format("%Y-%m-%d");
    let fee_line = if config.fees.per_symbol_taker.is_empty() {
        "10 bps taker per side".to_owned()
    } else {
        format!(
            "authenticated per-symbol taker fee ({} symbols; 10 bps default otherwise)",
            config.fees.per_symbol_taker.len()
        )
    };
    let mut lines: Vec<String> = vec![
        format!("# X Insighter Report - {date}"),
        String::new(),
        "Paper only: every P&L below is a simulated exit from observed ticks, traction rows and scheduled snapshots, not a realized fill. Nothing in Phase 43 can place an order.".into(),
        String::new(),
        format!(
            "- Freeze date: {}",
            config
                .freeze_date
                .map_or_else(|| "not set (gate cannot pass)".to_owned(), |d| d.to_string())
        ),
        "- Arms: deterministic = every floor-passing signal; haiku = floor-passing signals with conviction >= 0.6, stage ignition/building, no rug red flags and coordination risk < 0.7.".into(),
        format!(
            "- Friction: Solana ${PAPER_NOTIONAL_USD:.2} notional, ${SOLANA_ROUND_TRIP_FIXED_USD:.2} round-trip fixed, 2% entry and 2% exit slippage; Binance {fee_line} + half the detection spread + 5 bps slippage per side, ${PAPER_NOTIONAL_USD:.2} notional."
        ),
        "- Exits: rule_exits = exhaustion scale-out (50% at >= 0.4, rest at >= 0.7), any rug precursor, trailing stop (Solana 25%, Binance 10% below peak), hard stop (-30% / -12%), 24h time stop; trailing_only = trailing, hard and time stops; scheduled = full exit at the +15m / +1h / +6h snapshot. Fills at the price of the triggering row; a gone market without a price fills at 0.".into(),
        "- `peak_upper_bound` is an upper bound, never expected return.".into(),
        String::new(),
    ];
    for v in venues {
        render_venue(v, &mut lines);
    }
    lines.join("\n")
}

/// Compute, render and write the markdown and the atomic latest JSON.
pub fn generate(
    inputs: &ReportInputs,
    config: &ReportConfig,
    report_dir: &Path,
) -> Result<(PathBuf, PathBuf), SignalsError> {
    let venues = compute_report(inputs, config);
    let json = build_json(&venues, config);
    let md_path = report_dir.join(&json.markdown_path);
    bb_core::fsutil::atomic_write(&md_path, render_markdown(&venues, config).as_bytes())
        .map_err(|e| SignalsError::io(&md_path, e))?;
    let json_path = report_dir.join("x-insighter-report-latest.json");
    atomic_write_json(&json_path, &json)?;
    Ok((md_path, json_path))
}

fn parse_date(raw: &str, what: &str) -> anyhow::Result<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .with_context(|| format!("{what} must be YYYY-MM-DD, got {raw:?}"))
}

/// `bb x-insighter-report [--date YYYY-MM-DD]`.
pub async fn run_cli(date: Option<String>) -> anyhow::Result<()> {
    let paths = XPaths::from_env();
    let now_ms = SystemClock.now_ms();
    let report_date = parse_date(
        &date.unwrap_or_else(|| bb_core::time::utc_date(bb_core::time::now())),
        "--date",
    )?;
    let freeze_date = bb_core::env::optional_string(&[FREEZE_DATE_ENV])
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_date(&v, FREEZE_DATE_ENV))
        .transpose()?;
    let inputs = ReportInputs::load(&paths);
    let fees = load_binance_fees(&inputs.signals).await;
    let config = ReportConfig {
        report_date,
        freeze_date,
        now_ms,
        fees,
    };
    let (md, json) = generate(&inputs, &config, &paths.report_dir)?;
    tracing::info!(markdown = %md.display(), json = %json.display(), "Wrote x-insighter report");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2027-01-15T08:00:00Z.
    const T0: i64 = 1_800_000_000_000;
    const MIN: i64 = MINUTE_MS;
    const LATER: i64 = T0 + 2 * DAY_MS;

    fn tick(min: i64, price: f64) -> Observation {
        Observation {
            ts_ms: T0 + min * MIN,
            price: Some(price),
            ..Observation::default()
        }
    }

    fn exhaustion(min: i64, value: f64, price: Option<f64>) -> Observation {
        Observation {
            ts_ms: T0 + min * MIN,
            price,
            exhaustion: Some(value),
            ..Observation::default()
        }
    }

    fn entry(venue: Venue) -> Entry {
        Entry {
            venue,
            ts_ms: T0,
            price: 1.0,
            liquidity: Some(10_000.0),
        }
    }

    fn closed(outcome: SimOutcome) -> Trade {
        match outcome {
            SimOutcome::Closed(t) => t,
            other => panic!("expected a closed trade, got {other:?}"),
        }
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    fn reasons(t: &Trade) -> Vec<ExitReason> {
        t.legs.iter().map(|l| l.reason).collect()
    }

    #[test]
    fn rows_after_the_exit_decision_do_not_change_the_result() {
        let e = entry(Venue::Solana);
        let f = Friction::solana();
        let path = vec![tick(5, 1.1), tick(10, 0.69)];
        let base = simulate_exits(&e, &path, RULE_EXITS, &f, LATER);
        let mut future = path.clone();
        future.extend([exhaustion(11, 0.95, Some(0.7)), tick(20, 5.0)]);
        assert_eq!(simulate_exits(&e, &future, RULE_EXITS, &f, LATER), base);
        let trade = closed(base);
        assert_eq!(reasons(&trade), vec![ExitReason::HardStop]);
        assert_eq!(trade.exit_ms, T0 + 10 * MIN);

        // The scale-out fill is the price known at that row, not a later spike.
        let scale = vec![exhaustion(5, 0.45, Some(1.2)), tick(6, 1.18)];
        let mut spiked = scale.clone();
        spiked.push(tick(30, 4.0));
        let a = closed(simulate_exits(&e, &scale, RULE_EXITS, &f, LATER));
        let b = closed(simulate_exits(&e, &spiked, RULE_EXITS, &f, LATER));
        assert_eq!(a.legs[0], b.legs[0]);
        approx(b.legs[0].price_ratio, 1.2);
    }

    #[test]
    fn exhaustion_scales_out_half_then_rest_with_friction() {
        let e = entry(Venue::Solana);
        let f = Friction::solana();
        let path = vec![
            tick(1, 1.1),
            exhaustion(2, 0.45, Some(1.2)),
            exhaustion(3, 0.55, Some(1.25)),
            exhaustion(4, 0.7, Some(1.5)),
        ];
        let trade = closed(simulate_exits(&e, &path, RULE_EXITS, &f, LATER));
        assert_eq!(
            reasons(&trade),
            vec![ExitReason::ExhaustionScaleOut, ExitReason::ExhaustionFull]
        );
        approx(trade.legs[0].fraction, 0.5);
        approx(trade.legs[1].fraction, 0.5);
        approx(trade.gross_ratio, 0.5 * 1.2 + 0.5 * 1.5);
        let net = 1.35 * 0.98 * 0.98 - 1.0 - 0.50 / 15.63;
        approx(trade.net_return, net);
        approx(trade.net_usd(&f), net * 15.63);

        // trailing_only ignores exhaustion and rides to the time stop.
        let base = closed(simulate_exits(&e, &path, TRAILING_ONLY, &f, LATER));
        assert_eq!(reasons(&base), vec![ExitReason::TimeStop]);
        assert_eq!(base.exit_ms, T0 + DAY_MS);
        approx(base.gross_ratio, 1.5);
    }

    #[test]
    fn stop_precedence_on_crafted_paths() {
        let f = Friction::binance(BINANCE_DEFAULT_TAKER, None);
        let bin = entry(Venue::Binance);
        // Trailing 10% below the 1.10 peak fires before the -12% hard stop.
        let t = closed(simulate_exits(
            &bin,
            &[tick(1, 1.1), tick(2, 0.98)],
            RULE_EXITS,
            &f,
            LATER,
        ));
        assert_eq!(reasons(&t), vec![ExitReason::TrailingStop]);
        approx(t.gross_ratio, 0.98);
        // One row breaching both: the hard stop wins.
        let t = closed(simulate_exits(
            &bin,
            &[tick(1, 0.87)],
            TRAILING_ONLY,
            &f,
            LATER,
        ));
        assert_eq!(reasons(&t), vec![ExitReason::HardStop]);

        // Solana trailing boundary: 1.25 * 0.75 = 0.9375 exits, 0.9376 does not.
        let sol = entry(Venue::Solana);
        let s = Friction::solana();
        let t = closed(simulate_exits(
            &sol,
            &[tick(1, 1.25), tick(2, 0.9375)],
            RULE_EXITS,
            &s,
            LATER,
        ));
        assert_eq!(reasons(&t), vec![ExitReason::TrailingStop]);
        let path = vec![tick(1, 1.25), tick(2, 0.9376), tick(25 * 60, 0.1)];
        let t = closed(simulate_exits(&sol, &path, RULE_EXITS, &s, LATER));
        assert_eq!(reasons(&t), vec![ExitReason::TimeStop]);
        approx(t.gross_ratio, 0.9376);

        // Time stop needs the 24h horizon to have passed and an observed price.
        assert_eq!(
            simulate_exits(&sol, &[tick(1, 1.0)], RULE_EXITS, &s, T0 + 23 * HOUR_MS),
            SimOutcome::Open
        );
        assert_eq!(
            simulate_exits(&sol, &[], RULE_EXITS, &s, LATER),
            SimOutcome::NoPath
        );
    }

    #[test]
    fn rug_precursors_force_full_exit_and_gone_market_fills_at_zero() {
        let e = entry(Venue::Solana);
        let f = Friction::solana();
        // Liquidity down 30% from its max since signal (12k -> 8.4k).
        let path = vec![
            Observation {
                liquidity: Some(12_000.0),
                ..tick(1, 1.2)
            },
            Observation {
                liquidity: Some(8_000.0),
                ..tick(2, 1.1)
            },
        ];
        let t = closed(simulate_exits(&e, &path, RULE_EXITS, &f, LATER));
        assert_eq!(reasons(&t), vec![ExitReason::RugPrecursor]);
        approx(t.gross_ratio, 1.1);
        let gone = [Observation {
            ts_ms: T0 + MIN,
            market_gone: true,
            ..Observation::default()
        }];
        let t = closed(simulate_exits(&e, &gone, RULE_EXITS, &f, LATER));
        approx(t.gross_ratio, 0.0);
    }

    #[test]
    fn friction_formulas_match_hand_computed_values() {
        approx(
            Friction::solana().net_return(2.0),
            2.0 * 0.9604 - 1.0 - 0.50 / 15.63,
        );
        // 10 bps taker + 20/2 bps half spread + 5 bps slippage = 25 bps per side.
        approx(
            Friction::binance(0.001, Some(20.0)).net_return(1.1),
            1.1 * 0.9975 * 0.9975 - 1.0,
        );
        approx(
            Friction::binance(0.001, Some(20.0)).net_return(1.1),
            0.094_506_875,
        );
        approx(Friction::binance(0.001, None).entry_cost, 0.0015);
        let fees = BinanceFees {
            per_symbol_taker: HashMap::from([("PEPEUSDT".to_owned(), 0.00075)]),
        };
        approx(fees.taker(Some("PEPEUSDT")), 0.00075);
        approx(fees.taker(Some("DOGEUSDT")), 0.001);
    }

    #[test]
    fn lead_time_measures_first_move_and_marks_late() {
        let e = Entry {
            price: 1.1,
            ..entry(Venue::Binance)
        };
        let path = vec![tick(5, 1.2), tick(12, 1.3), tick(13, 1.6)];
        assert_eq!(
            lead_time(&e, Some(1.0), &path, LATER),
            Lead::Move { minutes: 12.0 }
        );
        let late = Entry { price: 1.35, ..e };
        assert_eq!(lead_time(&late, Some(1.0), &path, LATER), Lead::Late);
        let after_window = vec![tick(6 * 60 + 1, 2.0)];
        assert_eq!(lead_time(&e, Some(1.0), &after_window, LATER), Lead::NoMove);
        assert_eq!(lead_time(&e, Some(1.0), &[], T0 + HOUR_MS), Lead::Pending);
        assert_eq!(lead_time(&e, None, &path, LATER), Lead::Unknown);
    }

    #[test]
    fn hit_rate_boundaries() {
        let e = entry(Venue::Solana);
        assert_eq!(hit_outcome(&e, &[tick(10, 1.3)], LATER), Some(true));
        assert_eq!(
            hit_outcome(&e, &[tick(10, 0.85), tick(20, 1.5)], LATER),
            Some(false)
        );
        assert_eq!(
            hit_outcome(&e, &[tick(10, 0.86), tick(20, 1.3)], LATER),
            Some(true)
        );
        assert_eq!(hit_outcome(&e, &[tick(10, 1.29)], LATER), Some(false));
        assert_eq!(hit_outcome(&e, &[tick(10, 1.29)], T0 + HOUR_MS), None);
        assert_eq!(hit_outcome(&e, &[tick(6 * 60, 1.3)], LATER), Some(true));
        assert_eq!(
            hit_outcome(&e, &[tick(6 * 60 + 1, 1.3)], LATER),
            Some(false)
        );
    }

    fn snap_row(captured_min: i64, pair_exists: bool, liquidity: Option<f64>) -> Object {
        json!({
            "detection_id": "d", "scheduled_offset_seconds": 300, "status": "ok",
            "captured_at_ms": T0 + captured_min * MIN, "pair_exists": pair_exists,
            "price_usd": 1.0, "liquidity_usd": liquidity,
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    #[test]
    fn rug_rate_uses_v2_definition_by_any_snapshot() {
        let d = Some(10_000.0);
        let clean = snap_row(5, true, Some(2_100.0));
        assert_eq!(rug_outcome(d, &[&clean]), RugOutcome::Clean);
        let drop = snap_row(60, true, Some(1_900.0));
        assert_eq!(
            rug_outcome(d, &[&clean, &drop]),
            RugOutcome::Rug {
                at_ms: T0 + 60 * MIN
            }
        );
        let thin = snap_row(15, true, Some(900.0));
        assert_eq!(
            rug_outcome(None, &[&thin]),
            RugOutcome::Rug {
                at_ms: T0 + 15 * MIN
            }
        );
        // Binance rows carry no liquidity: only a non-TRADING symbol counts.
        let halted = snap_row(5, false, None);
        assert_eq!(
            rug_outcome(None, &[&halted]),
            RugOutcome::Rug {
                at_ms: T0 + 5 * MIN
            }
        );
        assert_eq!(rug_outcome(d, &[]), RugOutcome::NoSnapshots);
    }

    fn signal(id: &str, venue: Venue, detected_at_ms: i64, price: f64, ref_price: f64) -> XSignal {
        let token = format!("{}:{id}", venue.as_str());
        XSignal {
            detection_id: id.into(),
            detected_at_ms,
            venue,
            token_key: token,
            token_address: id.into(),
            symbol: (venue == Venue::Binance).then(|| format!("{id}USDT")),
            market: BTreeMap::from([
                ("price_usd".to_owned(), json!(price)),
                ("price_ref_1h_usd".to_owned(), json!(ref_price)),
                ("liquidity_usd".to_owned(), json!(10_000.0)),
            ]),
            floor_passed: true,
            ..XSignal::default()
        }
    }

    fn xtick(s: &XSignal, after_min: i64, price: f64) -> XTick {
        XTick {
            ts_ms: s.detected_at_ms + after_min * MIN,
            token_key: s.token_key.clone(),
            venue: s.venue,
            price_usd: Some(price),
            pair_exists: true,
            ..XTick::default()
        }
    }

    fn config(freeze: Option<&str>, report: &str) -> ReportConfig {
        ReportConfig {
            report_date: NaiveDate::parse_from_str(report, "%Y-%m-%d").unwrap(),
            freeze_date: freeze.map(|f| NaiveDate::parse_from_str(f, "%Y-%m-%d").unwrap()),
            now_ms: T0 + 40 * DAY_MS,
            fees: BinanceFees::default(),
        }
    }

    fn gate_of(reports: &[VenueReport], venue: Venue) -> &GateVerdict {
        &reports.iter().find(|r| r.venue == venue).unwrap().gate
    }

    fn criterion_of<'a>(gate: &'a GateVerdict, name: &str) -> &'a GateCriterion {
        gate.criteria.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn gate_drops_top_two_and_counts_only_post_freeze_signals() {
        let w1 = signal("W1", Venue::Binance, T0, 1.0, 1.0);
        let w2 = signal("W2", Venue::Binance, T0 + HOUR_MS, 1.0, 1.0);
        let l1 = signal("L1", Venue::Binance, T0 + 2 * HOUR_MS, 1.0, 1.0);
        let pre = signal("P1", Venue::Binance, T0 - DAY_MS, 1.0, 1.0);
        let ticks = vec![
            xtick(&w1, 1, 2.0),
            xtick(&w2, 1, 2.0),
            xtick(&l1, 1, 0.5),
            xtick(&pre, 1, 3.0),
        ];
        let inputs = ReportInputs {
            signals: vec![w1.clone(), w2.clone(), l1, pre.clone()],
            ticks: ticks.clone(),
            ..ReportInputs::default()
        };
        let reports = compute_report(&inputs, &config(Some("2027-01-15"), "2027-02-20"));
        let gate = gate_of(&reports, Venue::Binance);
        assert_eq!(criterion_of(gate, "days_and_signals").value, "36d / 3");
        let pnl = criterion_of(gate, "rule_exits_pnl");
        assert!(!pnl.passed, "{pnl:?}");
        assert!(pnl.value.starts_with("deterministic $"), "{pnl:?}");
        assert!(!gate.passed);

        // Three winners after the freeze survive dropping the best two.
        let w3 = signal("W3", Venue::Binance, T0 + 3 * HOUR_MS, 1.0, 1.0);
        let mut ticks = ticks;
        ticks.push(xtick(&w3, 1, 1.5));
        let inputs = ReportInputs {
            signals: vec![w1, w2, w3, pre],
            ticks,
            ..ReportInputs::default()
        };
        let reports = compute_report(&inputs, &config(Some("2027-01-15"), "2027-02-20"));
        let gate = gate_of(&reports, Venue::Binance);
        assert!(criterion_of(gate, "rule_exits_pnl").passed);
        assert_eq!(criterion_of(gate, "days_and_signals").value, "36d / 3");
        assert!(!criterion_of(gate, "days_and_signals").passed);

        let reports = compute_report(&inputs, &config(None, "2027-02-20"));
        let gate = gate_of(&reports, Venue::Binance);
        assert!(!gate.passed);
        assert_eq!(gate.criteria.len(), 5);
        assert!(
            gate.criteria
                .iter()
                .all(|c| c.value == "no freeze date" && !c.passed)
        );
    }

    #[test]
    fn lead_summary_counts_late_signals_as_zero() {
        let late = signal("LATE", Venue::Binance, T0, 1.35, 1.0);
        let early = signal("EARLY", Venue::Binance, T0, 1.1, 1.0);
        let inputs = ReportInputs {
            ticks: vec![xtick(&early, 20, 1.3)],
            signals: vec![late, early],
            ..ReportInputs::default()
        };
        let reports = compute_report(&inputs, &config(None, "2027-02-20"));
        let det = &reports[0].deterministic;
        assert_eq!(det.median_lead_minutes(), Some(10.0));
        assert_eq!(det.late_share(), Some(0.5));
    }

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object().unwrap().keys().cloned().collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn latest_json_has_the_documented_keys() {
        let dir = tempfile::tempdir().unwrap();
        let sol = signal("MINT", Venue::Solana, T0, 1.0, 0.9);
        let bin = signal("PEPE", Venue::Binance, T0, 1.0, 0.9);
        let inputs = ReportInputs {
            ticks: vec![xtick(&sol, 5, 1.2), xtick(&bin, 5, 1.1)],
            signals: vec![sol, bin],
            ..ReportInputs::default()
        };
        let (md, json_path) = generate(
            &inputs,
            &config(Some("2027-01-15"), "2027-01-18"),
            dir.path(),
        )
        .unwrap();
        assert!(md.ends_with("x-insighter-report-2027-01-18.md"));
        let markdown = std::fs::read_to_string(md).unwrap();
        assert!(markdown.contains("`peak_upper_bound` is an upper bound, never expected return."));

        let v: Value = serde_json::from_str(&std::fs::read_to_string(json_path).unwrap()).unwrap();
        assert_eq!(
            keys(&v),
            set(&[
                "schema",
                "generated_at_ms",
                "date",
                "markdown_path",
                "freeze_date",
                "venues"
            ])
        );
        assert_eq!(v["schema"], REPORT_SCHEMA);
        assert_eq!(v["markdown_path"], "x-insighter-report-2027-01-18.md");
        assert_eq!(v["freeze_date"], "2027-01-15");
        assert_eq!(keys(&v["venues"]), set(&["binance", "solana"]));
        for venue in ["binance", "solana"] {
            let section = &v["venues"][venue];
            assert_eq!(
                keys(section),
                set(&[
                    "signals",
                    "floor_passed",
                    "hit_rate",
                    "rug_rate",
                    "median_lead_time_minutes",
                    "late_share",
                    "llm_spend_usd",
                    "arms",
                    "equity_curve",
                    "gate",
                ])
            );
            assert_eq!(keys(&section["arms"]), set(&["deterministic", "haiku"]));
            for arm in ["deterministic", "haiku"] {
                assert_eq!(keys(&section["arms"][arm]), set(&ARM_KEYS));
                assert_eq!(
                    keys(&section["arms"][arm]["rule_exits"]),
                    set(&[
                        "trades",
                        "total_net_usd",
                        "mean_net_return",
                        "median_net_return",
                        "win_rate",
                        "total_net_usd_drop_top2",
                    ])
                );
            }
            let curve = section["equity_curve"].as_array().unwrap();
            assert_eq!(curve.len(), 1);
            assert_eq!(keys(&curve[0]), set(&["ts_ms", "cumulative_net_usd"]));
            assert_eq!(keys(&section["gate"]), set(&["passed", "criteria"]));
            for c in section["gate"]["criteria"].as_array().unwrap() {
                assert_eq!(keys(c), set(&["name", "value", "threshold", "passed"]));
            }
        }
        let parsed: ReportJson = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.venues["binance"].signals, 1);
    }
}

//! Live mean-reversion runtime, paper-first and capital-gated for live (port of
//! `src/mean_reversion.py`; companion to [`crate::mean_reversion_backtest`]).
//!
//! Polls Binance REST for 1m klines on the configured universe, computes the trailing
//! N-minute return per symbol and fires the rule the backtest validated: enter long when
//! the trailing return is at or below `dump_threshold_pct`; exit on the retrace target,
//! the stop loss or after `max_hold_bars` cycles.
//!
//! Modes: `plan_only` (default; audit decisions, no state mutation), `paper` (simulated
//! fills at the current close, closed trades written to the ledger) and `live`, which —
//! exactly like the Python runtime — passes the preflight gates (`MEAN_REV_ENABLED`,
//! `USE_TESTNET=false`, `DRY_RUN=false`, `CONFIRM_LIVE_MEAN_REV=yes`,
//! `MEAN_REV_PROVEN_EDGE=yes`) and then refuses: no live execution path exists.
//!
//! Prices and the decision rule run in `f64` exactly like the Python floats; money fields
//! are stored as `Decimal` holding the shortest decimal of that float, so state, ledger
//! and audit files carry the same numbers the Python runtime wrote.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use async_trait::async_trait;
use bb_binance::BinanceClient;
use bb_binance::models::Kline;
use futures::StreamExt as _;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::common::{
    self, decimal_f64, decimal_number, env_csv, env_decimal, env_flag, env_or, env_parse, env_yes,
    utc_now_ms, utc_timestamp, value_decimal,
};
use crate::error::{Result, StrategyError};

fn mr_err(message: impl Into<String>) -> StrategyError {
    StrategyError::MeanReversion(message.into())
}

/// Default universe (Python `MeanReversionSettings.universe`).
pub const DEFAULT_UNIVERSE: [&str; 8] = [
    "BTC/USDT",
    "ETH/USDT",
    "SOL/USDT",
    "BNB/USDT",
    "XRP/USDT",
    "ADA/USDT",
    "DOGE/USDT",
    "AVAX/USDT",
];

/// Runtime settings (Python `MeanReversionSettings`).
#[derive(Clone, PartialEq)]
pub struct MeanReversionSettings {
    pub api_key: String,
    pub api_secret: String,
    pub use_testnet: bool,
    pub dry_run: bool,
    pub enabled: bool,
    pub confirm_live_mean_rev: String,
    /// `MEAN_REV_PROVEN_EDGE=yes` (Python read it inside the preflight).
    pub proven_edge: bool,
    pub universe: Vec<String>,
    pub dump_lookback_bars: i64,
    pub dump_threshold_pct: f64,
    pub retrace_target_pct: f64,
    pub stop_loss_pct: f64,
    pub max_hold_bars: i64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    pub notional_per_position_usdt: Decimal,
    pub max_concurrent_positions: i64,
    pub min_balance_usdt: Decimal,
    pub dynamic_balance: bool,
    pub cycle_interval_seconds: f64,
    pub fetch_concurrency: i64,
    pub state_path: String,
    pub audit_path: String,
    pub ledger_path: String,
    pub emergency_stop_path: String,
}

impl std::fmt::Debug for MeanReversionSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeanReversionSettings")
            .field("enabled", &self.enabled)
            .field("use_testnet", &self.use_testnet)
            .field("dry_run", &self.dry_run)
            .field("universe", &self.universe)
            .field("state_path", &self.state_path)
            .finish_non_exhaustive()
    }
}

impl Default for MeanReversionSettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_secret: String::new(),
            use_testnet: false,
            dry_run: true,
            enabled: false,
            confirm_live_mean_rev: "no".into(),
            proven_edge: false,
            universe: DEFAULT_UNIVERSE.iter().map(|s| (*s).to_owned()).collect(),
            dump_lookback_bars: 15,
            dump_threshold_pct: -5.0,
            retrace_target_pct: 0.5,
            stop_loss_pct: -3.0,
            max_hold_bars: 60,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            notional_per_position_usdt: Decimal::from(50),
            max_concurrent_positions: 5,
            min_balance_usdt: Decimal::from(50),
            dynamic_balance: true,
            cycle_interval_seconds: 60.0,
            fetch_concurrency: 4,
            state_path: "data/mean-reversion-state.json".into(),
            audit_path: "data/mean-reversion-decisions.jsonl".into(),
            ledger_path: "data/mean-reversion-ledger.jsonl".into(),
            emergency_stop_path: "data/mean-reversion.stop".into(),
        }
    }
}

fn env_f64(name: &str, default: f64) -> Result<f64> {
    env_parse(name, default).map_err(|_| mr_err(format!("{name} must be numeric")))
}

fn env_dec(name: &str, default: Decimal) -> Result<Decimal> {
    env_decimal(name, default).map_err(|_| mr_err(format!("{name} must be numeric")))
}

fn env_int(name: &str, default: i64) -> Result<i64> {
    env_parse(name, default).map_err(|_| mr_err(format!("{name} must be an integer")))
}

impl MeanReversionSettings {
    pub fn validate(&self) -> Result<()> {
        if self.universe.is_empty() {
            return Err(mr_err("universe must not be empty"));
        }
        if self.dump_lookback_bars <= 0 {
            return Err(mr_err("dump_lookback_bars must be positive"));
        }
        if self.dump_threshold_pct >= 0.0 {
            return Err(mr_err("dump_threshold_pct must be negative"));
        }
        if self.max_hold_bars <= 0 {
            return Err(mr_err("max_hold_bars must be positive"));
        }
        if self.max_concurrent_positions <= 0 {
            return Err(mr_err("max_concurrent_positions must be positive"));
        }
        if self.notional_per_position_usdt <= Decimal::ZERO {
            return Err(mr_err("notional_per_position_usdt must be positive"));
        }
        if self.min_balance_usdt < Decimal::ZERO {
            return Err(mr_err("min_balance_usdt must be non-negative"));
        }
        // NaN / inf would make the daemon sleep undefined (Python: NaN slipped through).
        if !(self.cycle_interval_seconds >= 1.0 && self.cycle_interval_seconds.is_finite()) {
            return Err(mr_err("cycle_interval_seconds must be >= 1.0"));
        }
        for (name, value) in [
            ("state_path", &self.state_path),
            ("audit_path", &self.audit_path),
            ("ledger_path", &self.ledger_path),
            ("emergency_stop_path", &self.emergency_stop_path),
        ] {
            if value.trim().is_empty() {
                return Err(mr_err(format!("{name} must not be blank")));
            }
        }
        Ok(())
    }

    pub fn from_env() -> Result<Self> {
        let api_key = env_or("BINANCE_API_KEY", "");
        let api_secret = env_or("BINANCE_API_SECRET", "");
        if api_key.is_empty() || api_secret.is_empty() {
            return Err(mr_err("BINANCE_API_KEY and BINANCE_API_SECRET must be set"));
        }
        let d = Self::default();
        let universe = env_csv("MEAN_REV_UNIVERSE");
        let settings = Self {
            api_key,
            api_secret,
            use_testnet: env_flag("USE_TESTNET", false),
            dry_run: env_flag("DRY_RUN", true),
            enabled: env_flag("MEAN_REV_ENABLED", false),
            confirm_live_mean_rev: env_or("CONFIRM_LIVE_MEAN_REV", "no"),
            proven_edge: env_yes("MEAN_REV_PROVEN_EDGE"),
            universe: if universe.is_empty() {
                d.universe.clone()
            } else {
                universe
            },
            dump_lookback_bars: env_int("MEAN_REV_DUMP_LOOKBACK_BARS", d.dump_lookback_bars)?,
            dump_threshold_pct: env_f64("MEAN_REV_DUMP_THRESHOLD_PCT", d.dump_threshold_pct)?,
            retrace_target_pct: env_f64("MEAN_REV_RETRACE_TARGET_PCT", d.retrace_target_pct)?,
            stop_loss_pct: env_f64("MEAN_REV_STOP_LOSS_PCT", d.stop_loss_pct)?,
            max_hold_bars: env_int("MEAN_REV_MAX_HOLD_BARS", d.max_hold_bars)?,
            taker_fee_bps: env_f64("MEAN_REV_TAKER_FEE_BPS", d.taker_fee_bps)?,
            slippage_bps: env_f64("MEAN_REV_SLIPPAGE_BPS", d.slippage_bps)?,
            notional_per_position_usdt: env_dec(
                "MEAN_REV_NOTIONAL_PER_POSITION_USDT",
                d.notional_per_position_usdt,
            )?,
            max_concurrent_positions: env_int(
                "MEAN_REV_MAX_CONCURRENT_POSITIONS",
                d.max_concurrent_positions,
            )?,
            min_balance_usdt: env_dec("MEAN_REV_MIN_BALANCE_USDT", d.min_balance_usdt)?,
            dynamic_balance: env_flag("MEAN_REV_DYNAMIC_BALANCE", d.dynamic_balance),
            cycle_interval_seconds: env_f64(
                "MEAN_REV_CYCLE_INTERVAL_SECONDS",
                d.cycle_interval_seconds,
            )?,
            fetch_concurrency: env_int("MEAN_REV_FETCH_CONCURRENCY", d.fetch_concurrency)?,
            state_path: env_or("MEAN_REV_STATE_PATH", &d.state_path),
            audit_path: env_or("MEAN_REV_AUDIT_PATH", &d.audit_path),
            ledger_path: env_or("MEAN_REV_LEDGER_PATH", &d.ledger_path),
            emergency_stop_path: env_or("MEAN_REV_EMERGENCY_STOP_PATH", &d.emergency_stop_path),
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Taker fee + slippage, one side, as a fraction.
    fn one_side_cost_rate(&self) -> f64 {
        self.taker_fee_bps / 10_000.0 + self.slippage_bps / 10_000.0
    }
}

/// A float result in money space: the shortest decimal of `value` (Python's `repr`),
/// so files keep the exact numbers the Python runtime wrote.
fn to_money(value: f64) -> Result<Decimal> {
    value_decimal(&serde_json::Value::from(value))
        .ok_or_else(|| mr_err(format!("non-finite amount: {value}")))
}

fn null_as_zero<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<i64, D::Error> {
    Ok(Option::<i64>::deserialize(deserializer)?.unwrap_or(0))
}

/// One held paper/live position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeanReversionPosition {
    pub symbol: String,
    pub open_time_ms: i64,
    #[serde(with = "decimal_number")]
    pub entry_price: Decimal,
    #[serde(with = "decimal_number")]
    pub qty: Decimal,
    #[serde(with = "decimal_number")]
    pub notional_usdt: Decimal,
    pub dump_pct_at_entry: f64,
    #[serde(default, deserialize_with = "null_as_zero")]
    pub bars_held: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MeanReversionState {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub positions: Vec<MeanReversionPosition>,
}

fn null_as_empty<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<MeanReversionPosition>, D::Error> {
    Ok(Option::<Vec<MeanReversionPosition>>::deserialize(deserializer)?.unwrap_or_default())
}

pub fn load_state(path: &Path) -> Result<MeanReversionState> {
    if !path.exists() {
        return Ok(MeanReversionState::default());
    }
    std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
        .map_err(|e| mr_err(format!("could not read state at {}: {e}", path.display())))
}

pub fn save_state(state: &MeanReversionState, path: &Path) -> Result<()> {
    common::save_json_sorted(path, state)
}

/// Latest snapshot a symbol contributes to the decision engine.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolSignal {
    pub symbol: String,
    pub current_price: Decimal,
    pub bar_high: Decimal,
    pub bar_low: Decimal,
    pub lookback_return_pct: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Open,
    Close,
    Hold,
}

fn ser_opt_decimal<S: Serializer>(
    value: &Option<Decimal>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(d) => decimal_number::serialize(d, serializer),
        None => serializer.serialize_none(),
    }
}

/// One action a cycle would take.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeanReversionDecision {
    pub action: Action,
    pub symbol: String,
    pub reason: String,
    #[serde(with = "decimal_number")]
    pub notional_usdt: Decimal,
    #[serde(with = "decimal_number")]
    pub qty: Decimal,
    #[serde(serialize_with = "ser_opt_decimal")]
    pub entry_price: Option<Decimal>,
    #[serde(serialize_with = "ser_opt_decimal")]
    pub exit_price: Option<Decimal>,
}

impl MeanReversionDecision {
    fn hold(symbol: &str, reason: String) -> Self {
        Self {
            action: Action::Hold,
            symbol: symbol.to_owned(),
            reason,
            notional_usdt: Decimal::ZERO,
            qty: Decimal::ZERO,
            entry_price: None,
            exit_price: None,
        }
    }
}

/// Pure decision rule, mirroring the backtest's per-bar logic: exits on held positions,
/// then entries (deepest dump first) into `max_concurrent - held + queued_closes` slots.
pub fn compute_decisions(
    settings: &MeanReversionSettings,
    state: &MeanReversionState,
    signals: &[SymbolSignal],
) -> Result<Vec<MeanReversionDecision>> {
    let by_symbol: HashMap<&str, &SymbolSignal> =
        signals.iter().map(|s| (s.symbol.as_str(), s)).collect();
    let held: HashSet<&str> = state.positions.iter().map(|p| p.symbol.as_str()).collect();
    let mut closing: HashSet<&str> = HashSet::new();
    let mut decisions = Vec::new();

    // 1. Exits on currently-held positions.
    for position in &state.positions {
        let Some(signal) = by_symbol.get(position.symbol.as_str()) else {
            decisions.push(MeanReversionDecision::hold(
                &position.symbol,
                "no signal this cycle; held position retained".into(),
            ));
            continue;
        };
        let entry = decimal_f64(position.entry_price);
        let dump = position.dump_pct_at_entry.abs();
        let target_price = entry * (1.0 + dump * settings.retrace_target_pct / 100.0);
        let stop_price = entry * (1.0 + settings.stop_loss_pct / 100.0);
        let bar_low = decimal_f64(signal.bar_low);
        let bar_high = decimal_f64(signal.bar_high);
        let exit = if bar_low <= stop_price {
            Some((
                format!("stop loss: bar_low={bar_low:.4} <= stop_price={stop_price:.4}"),
                to_money(stop_price)?,
            ))
        } else if bar_high >= target_price {
            Some((
                format!("take profit: bar_high={bar_high:.4} >= target={target_price:.4}"),
                to_money(target_price)?,
            ))
        } else if position.bars_held >= settings.max_hold_bars {
            Some((
                format!("max hold ({} bars) reached", settings.max_hold_bars),
                signal.current_price,
            ))
        } else {
            None
        };
        match exit {
            Some((reason, exit_price)) => {
                decisions.push(MeanReversionDecision {
                    action: Action::Close,
                    symbol: position.symbol.clone(),
                    reason,
                    notional_usdt: Decimal::ZERO,
                    qty: position.qty,
                    entry_price: None,
                    exit_price: Some(exit_price),
                });
                closing.insert(position.symbol.as_str());
            }
            None => decisions.push(MeanReversionDecision::hold(
                &position.symbol,
                format!(
                    "no exit: bar [{bar_low:.4}, {bar_high:.4}] inside [{stop_price:.4}, {target_price:.4}]; bars_held={}",
                    position.bars_held
                ),
            )),
        }
    }

    // 2. Entries (capacity = max_concurrent - currently_open + queued_closes).
    let free_slots = settings.max_concurrent_positions - held.len() as i64 + closing.len() as i64;
    if free_slots > 0 {
        let mut candidates: Vec<&SymbolSignal> = signals
            .iter()
            .filter(|s| !(held.contains(s.symbol.as_str()) && !closing.contains(s.symbol.as_str())))
            .filter(|s| {
                s.lookback_return_pct
                    .partial_cmp(&settings.dump_threshold_pct)
                    != Some(std::cmp::Ordering::Greater)
            })
            .collect();
        // Most-negative first (stable, like Python's sort).
        candidates.sort_by(|a, b| {
            a.lookback_return_pct
                .partial_cmp(&b.lookback_return_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let notional = decimal_f64(settings.notional_per_position_usdt);
        for signal in candidates
            .into_iter()
            .take(usize::try_from(free_slots).unwrap_or(usize::MAX))
        {
            let entry_price = signal.current_price;
            if entry_price <= Decimal::ZERO {
                continue;
            }
            let entry = decimal_f64(entry_price);
            decisions.push(MeanReversionDecision {
                action: Action::Open,
                symbol: signal.symbol.clone(),
                reason: format!(
                    "15m return {:.2}% <= threshold {:.2}%; entry {entry:.4}",
                    signal.lookback_return_pct, settings.dump_threshold_pct
                ),
                notional_usdt: settings.notional_per_position_usdt,
                qty: to_money(notional / entry)?,
                entry_price: Some(entry_price),
                exit_price: None,
            });
        }
    }
    Ok(decisions)
}

pub fn preflight(settings: &MeanReversionSettings, submit_order: bool) -> Result<()> {
    if Path::new(&settings.emergency_stop_path).exists() {
        return Err(mr_err(format!(
            "emergency stop file present at {}; refusing to run",
            settings.emergency_stop_path
        )));
    }
    if !submit_order {
        return Ok(());
    }
    if !settings.enabled {
        return Err(mr_err(
            "MEAN_REV_ENABLED=true is required for live submission",
        ));
    }
    if settings.use_testnet {
        return Err(mr_err(
            "mean-rev live submission requires USE_TESTNET=false",
        ));
    }
    if settings.dry_run {
        return Err(mr_err("mean-rev live submission requires DRY_RUN=false"));
    }
    if settings.confirm_live_mean_rev != "yes" {
        return Err(mr_err(
            "mean-rev live submission requires CONFIRM_LIVE_MEAN_REV=yes",
        ));
    }
    if !settings.proven_edge {
        return Err(mr_err(
            "mean-rev live submission requires MEAN_REV_PROVEN_EDGE=yes. Run --paper for at least a few hundred \
             completed round-trips; set this only when paper expectancy reproduces the backtest verdict.",
        ));
    }
    Ok(())
}

/// Recent 1m klines per symbol, behind a trait so cycles run offline in tests.
#[async_trait]
pub trait KlineSource: Send + Sync {
    /// The latest `limit` 1-minute klines for a CCXT symbol; `None` when unavailable
    /// (HTTP/parse failure — the symbol is skipped this cycle).
    async fn recent_klines(&self, symbol: &str, limit: u32) -> Option<Vec<Kline>>;
}

/// Public `/api/v3/klines` reader.
pub struct BinanceKlines {
    client: BinanceClient,
}

impl BinanceKlines {
    pub fn new(client: BinanceClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl KlineSource for BinanceKlines {
    async fn recent_klines(&self, symbol: &str, limit: u32) -> Option<Vec<Kline>> {
        let rest_symbol = symbol.replace('/', "").to_uppercase();
        match self
            .client
            .klines(&rest_symbol, "1m", None, None, Some(limit))
            .await
        {
            Ok(klines) => Some(klines),
            Err(err) => {
                tracing::debug!(%symbol, error = %err, "mean-reversion kline fetch failed; skipping symbol");
                None
            }
        }
    }
}

/// Trailing return over `lookback` bars from the latest kline window (`None` when the
/// window is short or the lookback close is not positive).
fn signal_from_klines(symbol: &str, klines: &[Kline], lookback: usize) -> Option<SymbolSignal> {
    if klines.len() < lookback.checked_add(1)? {
        return None;
    }
    let last = klines.last()?;
    let lookback_close = decimal_f64(klines[klines.len() - (lookback + 1)].close);
    if lookback_close <= 0.0 {
        return None;
    }
    Some(SymbolSignal {
        symbol: symbol.to_owned(),
        current_price: last.close,
        bar_high: last.high,
        bar_low: last.low,
        lookback_return_pct: (decimal_f64(last.close) / lookback_close - 1.0) * 100.0,
    })
}

/// Signals for the whole universe (universe order; unavailable symbols dropped), at most
/// `fetch_concurrency` requests in flight.
pub async fn fetch_signals(
    source: &dyn KlineSource,
    settings: &MeanReversionSettings,
) -> Vec<SymbolSignal> {
    let lookback = usize::try_from(settings.dump_lookback_bars).unwrap_or(usize::MAX);
    let limit = u32::try_from(lookback.saturating_add(2)).unwrap_or(u32::MAX);
    let concurrency = usize::try_from(settings.fetch_concurrency.max(1)).unwrap_or(1);
    futures::stream::iter(&settings.universe)
        .map(|symbol| async move {
            let klines = source.recent_klines(symbol, limit).await?;
            signal_from_klines(symbol, &klines, lookback)
        })
        .buffered(concurrency)
        .filter_map(std::future::ready)
        .collect()
        .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    PlanOnly,
    Paper,
    Live,
}

/// One simulated (or live) fill appended to the ledger.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeanReversionLedgerEntry {
    pub timestamp_ms: i64,
    pub action: Action,
    pub symbol: String,
    #[serde(with = "decimal_number")]
    pub qty: Decimal,
    #[serde(with = "decimal_number")]
    pub price: Decimal,
    #[serde(with = "decimal_number")]
    pub notional_usdt: Decimal,
    #[serde(with = "decimal_number")]
    pub fee_usdt: Decimal,
    #[serde(with = "decimal_number")]
    pub pnl_usdt: Decimal,
    pub exit_reason: String,
    pub mode: Mode,
}

/// Paper-fill the decisions: tick carried positions, record closes at their exit price
/// (P&L net of exit-side costs vs. the notional invested), then open new positions
/// (P&L at entry = −entry fee).
pub fn apply_paper_decisions(
    settings: &MeanReversionSettings,
    state: &MeanReversionState,
    signals: &[SymbolSignal],
    decisions: &[MeanReversionDecision],
    cycle_ts_ms: i64,
) -> Result<(MeanReversionState, Vec<MeanReversionLedgerEntry>)> {
    let cost_rate = settings.one_side_cost_rate();
    let by_symbol: HashMap<&str, &SymbolSignal> =
        signals.iter().map(|s| (s.symbol.as_str(), s)).collect();
    let closed: HashSet<&str> = decisions
        .iter()
        .filter(|d| d.action == Action::Close)
        .map(|d| d.symbol.as_str())
        .collect();
    let mut positions: Vec<MeanReversionPosition> = Vec::with_capacity(state.positions.len());
    let mut ledger = Vec::new();

    // 1. Carry forward held positions that aren't being closed; tick bars_held.
    for position in state
        .positions
        .iter()
        .filter(|p| !closed.contains(p.symbol.as_str()))
    {
        positions.push(MeanReversionPosition {
            bars_held: position.bars_held + 1,
            ..position.clone()
        });
    }

    // 2. Close rows (the entry side already absorbed its fees at open time).
    for decision in decisions.iter().filter(|d| d.action == Action::Close) {
        let Some(exit_price) = decision.exit_price else {
            continue;
        };
        let Some(held) = state.positions.iter().find(|p| p.symbol == decision.symbol) else {
            continue;
        };
        let qty = decimal_f64(held.qty);
        let exit = decimal_f64(exit_price);
        let proceeds = qty * exit * (1.0 - cost_rate);
        ledger.push(MeanReversionLedgerEntry {
            timestamp_ms: cycle_ts_ms,
            action: Action::Close,
            symbol: held.symbol.clone(),
            qty: held.qty,
            price: exit_price,
            notional_usdt: to_money(qty * exit)?,
            fee_usdt: to_money(qty * exit * cost_rate)?,
            pnl_usdt: to_money(proceeds - decimal_f64(held.notional_usdt))?,
            exit_reason: decision.reason.clone(),
            mode: Mode::Paper,
        });
    }

    // 3. Open new positions.
    for decision in decisions.iter().filter(|d| d.action == Action::Open) {
        let Some(entry_price) = decision.entry_price else {
            continue;
        };
        let signal = by_symbol
            .get(decision.symbol.as_str())
            .ok_or_else(|| mr_err(format!("no signal for opened symbol {}", decision.symbol)))?;
        let entry_fee = decimal_f64(decision.notional_usdt) * cost_rate;
        ledger.push(MeanReversionLedgerEntry {
            timestamp_ms: cycle_ts_ms,
            action: Action::Open,
            symbol: decision.symbol.clone(),
            qty: decision.qty,
            price: entry_price,
            notional_usdt: decision.notional_usdt,
            fee_usdt: to_money(entry_fee)?,
            pnl_usdt: to_money(-entry_fee)?,
            exit_reason: String::new(),
            mode: Mode::Paper,
        });
        positions.push(MeanReversionPosition {
            symbol: decision.symbol.clone(),
            open_time_ms: cycle_ts_ms,
            entry_price,
            qty: decision.qty,
            notional_usdt: decision.notional_usdt,
            dump_pct_at_entry: signal.lookback_return_pct,
            bars_held: 0,
        });
    }
    Ok((MeanReversionState { positions }, ledger))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopSignal {
    pub symbol: String,
    pub lookback_return_pct: f64,
    #[serde(with = "decimal_number")]
    pub current_price: Decimal,
}

/// Audit payload appended to the decisions JSONL and printed by the CLI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CyclePayload {
    pub timestamp: String,
    pub mode: Mode,
    pub signals_count: usize,
    pub open_positions: usize,
    pub decisions: Vec<MeanReversionDecision>,
    pub ledger_entries_this_cycle: Vec<MeanReversionLedgerEntry>,
    pub top_signals: Vec<TopSignal>,
}

/// One pass: fetch signals, compute decisions, paper-fill them in paper mode, audit.
pub async fn run_cycle(
    settings: &MeanReversionSettings,
    mode: Mode,
    source: &dyn KlineSource,
) -> Result<CyclePayload> {
    preflight(settings, mode == Mode::Live)?;
    if mode == Mode::Live {
        return Err(mr_err(
            "live execution path is not yet implemented; only plan_only and paper are supported",
        ));
    }

    let signals = fetch_signals(source, settings).await;
    let mut state = load_state(Path::new(&settings.state_path))?;
    let decisions = compute_decisions(settings, &state, &signals)?;

    let mut ledger = Vec::new();
    if mode == Mode::Paper {
        let (next, entries) =
            apply_paper_decisions(settings, &state, &signals, &decisions, utc_now_ms())?;
        state = next;
        ledger = entries;
        save_state(&state, Path::new(&settings.state_path))?;
        common::append_jsonl_many(Path::new(&settings.ledger_path), &ledger)?;
    }

    let mut top_signals: Vec<TopSignal> = signals
        .iter()
        .map(|s| TopSignal {
            symbol: s.symbol.clone(),
            lookback_return_pct: s.lookback_return_pct,
            current_price: s.current_price,
        })
        .collect();
    top_signals.sort_by(|a, b| {
        a.lookback_return_pct
            .partial_cmp(&b.lookback_return_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    top_signals.truncate(10);

    let payload = CyclePayload {
        timestamp: utc_timestamp(),
        mode,
        signals_count: signals.len(),
        open_positions: state.positions.len(),
        decisions,
        ledger_entries_this_cycle: ledger,
        top_signals,
    };
    common::append_jsonl(Path::new(&settings.audit_path), &payload)?;
    Ok(payload)
}

/// Audit row the daemon writes when a cycle fails.
#[derive(Debug, Serialize)]
struct CycleError {
    timestamp: String,
    error: String,
}

/// Python exception class the daemon's catch-all would have reported for this failure.
fn python_exception_name(err: &StrategyError) -> &'static str {
    match err {
        StrategyError::Io { .. } => "OSError",
        StrategyError::Binance(_) | StrategyError::Http(_) => "ClientError",
        StrategyError::InvalidConfig(_)
        | StrategyError::Env(_)
        | StrategyError::Json { .. }
        | StrategyError::Serde(_) => "ValueError",
        StrategyError::FundingArb(_)
        | StrategyError::TrendFilter(_)
        | StrategyError::MeanReversion(_) => "RuntimeError",
    }
}

/// One daemon iteration: run a cycle; a failure is audited (never aborts the daemon).
/// Only a failure to write that audit row is returned.
pub async fn daemon_cycle(
    settings: &MeanReversionSettings,
    mode: Mode,
    source: &dyn KlineSource,
) -> Result<()> {
    let error = match run_cycle(settings, mode, source).await {
        Ok(_) => return Ok(()),
        Err(StrategyError::MeanReversion(message)) => message,
        Err(other) => {
            let message: String = other.to_string().chars().take(200).collect();
            format!("{}: {message}", python_exception_name(&other))
        }
    };
    common::append_jsonl(
        Path::new(&settings.audit_path),
        &CycleError {
            timestamp: utc_timestamp(),
            error,
        },
    )
}

/// Long-running loop: one cycle every `cycle_interval_seconds` (the emergency stop file
/// is checked by every cycle's preflight and audited as an error).
pub async fn run_daemon(
    settings: &MeanReversionSettings,
    mode: Mode,
    source: &dyn KlineSource,
) -> Result<Infallible> {
    let interval = Duration::try_from_secs_f64(settings.cycle_interval_seconds)
        .map_err(|_| mr_err("cycle_interval_seconds must be >= 1.0"))?;
    loop {
        daemon_cycle(settings, mode, source).await?;
        tokio::time::sleep(interval).await;
    }
}

#[derive(clap::Args, Debug, Clone, Default)]
pub struct Args {
    /// Compute decisions and audit them without mutating state (default).
    #[arg(long, conflicts_with_all = ["paper", "submit_live"])]
    pub plan_only: bool,
    /// Materialise decisions in local state with simulated fills at the current close.
    #[arg(long, conflicts_with_all = ["plan_only", "submit_live"])]
    pub paper: bool,
    /// Live submission behind the preflight gates (no live execution path exists yet).
    #[arg(long, conflicts_with_all = ["plan_only", "paper"])]
    pub submit_live: bool,
    /// loop forever at cycle_interval_seconds (default: one cycle then exit)
    #[arg(long)]
    pub daemon: bool,
}

impl Args {
    fn mode(&self) -> Mode {
        if self.submit_live {
            Mode::Live
        } else if self.paper {
            Mode::Paper
        } else {
            Mode::PlanOnly
        }
    }
}

async fn run_args(args: &Args) -> Result<Option<CyclePayload>> {
    let settings = MeanReversionSettings::from_env()?;
    let source = BinanceKlines::new(common::public_client()?);
    if args.daemon {
        let Err(err) = run_daemon(&settings, args.mode(), &source).await;
        return Err(err);
    }
    run_cycle(&settings, args.mode(), &source).await.map(Some)
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    match run_args(&args).await {
        Ok(Some(payload)) => {
            common::print_json(&payload)?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(None) => Ok(ExitCode::SUCCESS),
        Err(err) => {
            println!("mean-reversion failed: {err}");
            Ok(ExitCode::from(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn settings(dir: &Path) -> MeanReversionSettings {
        let s = MeanReversionSettings {
            api_key: "k".into(),
            api_secret: "s".into(),
            dry_run: false,
            enabled: true,
            confirm_live_mean_rev: "yes".into(),
            universe: vec!["A/USDT".into(), "B/USDT".into(), "C/USDT".into()],
            max_concurrent_positions: 3,
            state_path: dir.join("state.json").display().to_string(),
            audit_path: dir.join("audit.jsonl").display().to_string(),
            ledger_path: dir.join("ledger.jsonl").display().to_string(),
            emergency_stop_path: dir.join("stop").display().to_string(),
            ..MeanReversionSettings::default()
        };
        s.validate().unwrap();
        s
    }

    fn signal(
        symbol: &str,
        current: Decimal,
        low: Decimal,
        high: Decimal,
        lookback_pct: f64,
    ) -> SymbolSignal {
        SymbolSignal {
            symbol: symbol.into(),
            current_price: current,
            bar_high: high,
            bar_low: low,
            lookback_return_pct: lookback_pct,
        }
    }

    fn flat_signal(symbol: &str, current: Decimal, lookback_pct: f64) -> SymbolSignal {
        signal(symbol, current, current, current, lookback_pct)
    }

    fn held(symbol: &str, entry: Decimal, dump: f64, bars: i64) -> MeanReversionPosition {
        MeanReversionPosition {
            symbol: symbol.into(),
            open_time_ms: 0,
            entry_price: entry,
            qty: to_money(50.0 / decimal_f64(entry)).unwrap(),
            notional_usdt: dec!(50),
            dump_pct_at_entry: dump,
            bars_held: bars,
        }
    }

    fn state_of(positions: Vec<MeanReversionPosition>) -> MeanReversionState {
        MeanReversionState { positions }
    }

    fn decision_for<'a>(
        decisions: &'a [MeanReversionDecision],
        symbol: &str,
    ) -> &'a MeanReversionDecision {
        decisions.iter().find(|d| d.symbol == symbol).unwrap()
    }

    fn close_to(value: Decimal, expected: f64, tol: f64) -> bool {
        (decimal_f64(value) - expected).abs() <= tol
    }

    // Settings validation.

    #[test]
    fn settings_rejects_empty_universe() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            universe: Vec::new(),
            ..settings(dir.path())
        };
        assert!(s.validate().unwrap_err().to_string().contains("universe"));
    }

    #[test]
    fn settings_rejects_positive_dump_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            dump_threshold_pct: 5.0,
            ..settings(dir.path())
        };
        assert!(
            s.validate()
                .unwrap_err()
                .to_string()
                .contains("dump_threshold_pct")
        );
    }

    #[test]
    fn settings_rejects_zero_max_hold() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            max_hold_bars: 0,
            ..settings(dir.path())
        };
        assert!(
            s.validate()
                .unwrap_err()
                .to_string()
                .contains("max_hold_bars")
        );
    }

    #[test]
    fn settings_rejects_non_finite_cycle_interval() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [0.5, f64::NAN, f64::INFINITY] {
            let s = MeanReversionSettings {
                cycle_interval_seconds: bad,
                ..settings(dir.path())
            };
            assert!(
                s.validate()
                    .unwrap_err()
                    .to_string()
                    .contains("cycle_interval_seconds")
            );
        }
    }

    // Preflight gate ladder.

    #[test]
    fn preflight_passes_in_plan_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            enabled: false,
            ..settings(dir.path())
        };
        preflight(&s, false).unwrap();
    }

    #[test]
    fn preflight_blocks_when_emergency_stop_present() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        std::fs::write(&s.emergency_stop_path, "halt").unwrap();
        assert!(
            preflight(&s, false)
                .unwrap_err()
                .to_string()
                .contains("emergency stop")
        );
    }

    #[test]
    fn preflight_requires_proven_edge_for_live() {
        let dir = tempfile::tempdir().unwrap();
        let err = preflight(&settings(dir.path()), true).unwrap_err();
        assert!(err.to_string().contains("MEAN_REV_PROVEN_EDGE"));
    }

    #[test]
    fn preflight_passes_live_when_all_gates_set() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            proven_edge: true,
            ..settings(dir.path())
        };
        preflight(&s, true).unwrap();
    }

    #[test]
    fn preflight_gate_order() {
        let dir = tempfile::tempdir().unwrap();
        let base = settings(dir.path());
        let err = |s: MeanReversionSettings| preflight(&s, true).unwrap_err().to_string();
        assert!(
            err(MeanReversionSettings {
                enabled: false,
                use_testnet: true,
                ..base.clone()
            })
            .contains("MEAN_REV_ENABLED")
        );
        assert!(
            err(MeanReversionSettings {
                use_testnet: true,
                dry_run: true,
                ..base.clone()
            })
            .contains("USE_TESTNET")
        );
        assert!(
            err(MeanReversionSettings {
                dry_run: true,
                ..base.clone()
            })
            .contains("DRY_RUN")
        );
        assert!(
            err(MeanReversionSettings {
                confirm_live_mean_rev: "no".into(),
                ..base
            })
            .contains("CONFIRM_LIVE_MEAN_REV")
        );
    }

    // Decision rule.

    #[test]
    fn opens_on_dump_below_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let signals = [flat_signal("A/USDT", dec!(94), -6.0)];
        let decisions = compute_decisions(
            &settings(dir.path()),
            &MeanReversionState::default(),
            &signals,
        )
        .unwrap();
        let open = decisions.iter().find(|d| d.action == Action::Open).unwrap();
        assert_eq!(open.symbol, "A/USDT");
        assert!(close_to(open.qty, 50.0 / 94.0, 1e-12));
        assert_eq!(open.entry_price, Some(dec!(94)));
        assert_eq!(
            open.reason,
            "15m return -6.00% <= threshold -5.00%; entry 94.0000"
        );
    }

    #[test]
    fn skips_open_when_dump_above_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let signals = [flat_signal("A/USDT", dec!(100), -3.0)];
        let decisions = compute_decisions(
            &settings(dir.path()),
            &MeanReversionState::default(),
            &signals,
        )
        .unwrap();
        assert!(!decisions.iter().any(|d| d.action == Action::Open));
    }

    #[test]
    fn take_profit_when_bar_crosses_target() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 2)]);
        let signals = [signal("A/USDT", dec!(96.5), dec!(94), dec!(97), -2.0)];
        let decisions = compute_decisions(&settings(dir.path()), &state, &signals).unwrap();
        let close = decision_for(&decisions, "A/USDT");
        assert_eq!(close.action, Action::Close);
        assert!(close.reason.contains("take profit"));
        assert!(close_to(close.exit_price.unwrap(), 96.82, 0.01));
    }

    #[test]
    fn stop_loss_when_bar_crosses_stop() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 2)]);
        let signals = [signal("A/USDT", dec!(92), dec!(90), dec!(95), -2.0)];
        let decisions = compute_decisions(&settings(dir.path()), &state, &signals).unwrap();
        let close = decision_for(&decisions, "A/USDT");
        assert_eq!(close.action, Action::Close);
        assert_eq!(
            close.reason,
            "stop loss: bar_low=90.0000 <= stop_price=91.1800"
        );
        assert!(close_to(close.exit_price.unwrap(), 91.18, 0.01));
    }

    #[test]
    fn max_hold_exit_when_time_runs_out() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            max_hold_bars: 10,
            ..settings(dir.path())
        };
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 10)]);
        let signals = [signal("A/USDT", dec!(94.5), dec!(93), dec!(95), -2.0)];
        let decisions = compute_decisions(&s, &state, &signals).unwrap();
        let close = decision_for(&decisions, "A/USDT");
        assert_eq!(close.action, Action::Close);
        assert_eq!(close.reason, "max hold (10 bars) reached");
        assert_eq!(close.exit_price, Some(dec!(94.5)));
    }

    #[test]
    fn holds_when_neither_target_nor_stop_crossed() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 2)]);
        let signals = [signal("A/USDT", dec!(94.5), dec!(93), dec!(95), -2.0)];
        let decisions = compute_decisions(&settings(dir.path()), &state, &signals).unwrap();
        let hold = decision_for(&decisions, "A/USDT");
        assert_eq!(hold.action, Action::Hold);
        assert_eq!(
            hold.reason,
            "no exit: bar [93.0000, 95.0000] inside [91.1800, 96.8200]; bars_held=2"
        );
    }

    #[test]
    fn holds_without_signal() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 2)]);
        let decisions = compute_decisions(&settings(dir.path()), &state, &[]).unwrap();
        assert_eq!(
            decisions[0].reason,
            "no signal this cycle; held position retained"
        );
    }

    #[test]
    fn respects_max_concurrent_positions() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            max_concurrent_positions: 1,
            ..settings(dir.path())
        };
        let signals = [
            flat_signal("A/USDT", dec!(93), -7.0),
            flat_signal("B/USDT", dec!(94), -6.0),
        ];
        let decisions = compute_decisions(&s, &MeanReversionState::default(), &signals).unwrap();
        let opens: Vec<_> = decisions
            .iter()
            .filter(|d| d.action == Action::Open)
            .collect();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].symbol, "A/USDT");
    }

    #[test]
    fn pending_close_frees_capacity_for_open() {
        let dir = tempfile::tempdir().unwrap();
        let s = MeanReversionSettings {
            max_concurrent_positions: 1,
            ..settings(dir.path())
        };
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 20)]);
        let signals = [
            signal("A/USDT", dec!(97), dec!(94), dec!(98), -1.0),
            signal("B/USDT", dec!(90), dec!(89), dec!(92), -10.0),
        ];
        let decisions = compute_decisions(&s, &state, &signals).unwrap();
        let actions: Vec<(Action, &str)> = decisions
            .iter()
            .map(|d| (d.action, d.symbol.as_str()))
            .collect();
        assert!(actions.contains(&(Action::Close, "A/USDT")));
        assert!(actions.contains(&(Action::Open, "B/USDT")));
    }

    // Paper apply.

    #[test]
    fn apply_paper_open_creates_position_and_ledger_entry() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let signals = [flat_signal("A/USDT", dec!(94), -6.0)];
        let empty = MeanReversionState::default();
        let decisions = compute_decisions(&s, &empty, &signals).unwrap();
        let (state, ledger) =
            apply_paper_decisions(&s, &empty, &signals, &decisions, 1_000_000).unwrap();
        assert_eq!(state.positions.len(), 1);
        let p = &state.positions[0];
        assert_eq!(p.symbol, "A/USDT");
        assert_eq!(p.notional_usdt, dec!(50));
        assert_eq!(p.bars_held, 0);
        assert_eq!(p.dump_pct_at_entry, -6.0);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].action, Action::Open);
        assert_eq!(ledger[0].fee_usdt, dec!(0.075));
        assert_eq!(ledger[0].pnl_usdt, dec!(-0.075));
    }

    #[test]
    fn apply_paper_close_records_realised_pnl() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 2)]);
        let signals = [signal("A/USDT", dec!(97), dec!(94), dec!(98), -2.0)];
        let decisions = compute_decisions(&s, &state, &signals).unwrap();
        let (next, ledger) =
            apply_paper_decisions(&s, &state, &signals, &decisions, 1_000_001).unwrap();
        assert!(next.positions.is_empty());
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].action, Action::Close);
        let expected = 50.0 / 94.0 * 96.82 * (1.0 - 15.0 / 10_000.0) - 50.0;
        assert!(close_to(ledger[0].pnl_usdt, expected, 0.01));
    }

    #[test]
    fn apply_paper_increments_bars_held_on_carried_position() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let state = state_of(vec![held("A/USDT", dec!(94), -6.0, 2)]);
        let signals = [signal("A/USDT", dec!(94.5), dec!(93), dec!(95), -2.0)];
        let decisions = compute_decisions(&s, &state, &signals).unwrap();
        let (next, ledger) =
            apply_paper_decisions(&s, &state, &signals, &decisions, 2_000_000).unwrap();
        assert_eq!(next.positions.len(), 1);
        assert_eq!(next.positions[0].bars_held, 3);
        assert!(ledger.is_empty());
    }

    // State round-trip.

    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save_state(&state_of(vec![held("A/USDT", dec!(94), -6.0, 0)]), &path).unwrap();
        let restored = load_state(&path).unwrap();
        assert_eq!(restored.positions.len(), 1);
        assert_eq!(restored.positions[0].symbol, "A/USDT");
    }

    #[test]
    fn state_load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_state(&dir.path().join("missing.json"))
                .unwrap()
                .positions
                .is_empty()
        );
    }

    #[test]
    fn state_load_corrupt_file_raises() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not valid json").unwrap();
        assert!(
            load_state(&path)
                .unwrap_err()
                .to_string()
                .contains("could not read")
        );
    }

    #[test]
    fn state_loads_python_written_floats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"positions": [{"symbol": "A/USDT", "open_time_ms": 5, "entry_price": 94.0, "qty": 0.5319148936170213,
                "notional_usdt": 50.0, "dump_pct_at_entry": -6.0, "bars_held": null}]}"#,
        )
        .unwrap();
        let state = load_state(&path).unwrap();
        assert_eq!(state.positions[0].qty, dec!(0.5319148936170213));
        assert_eq!(state.positions[0].bars_held, 0);
        let saved = common::sorted_value(&state).unwrap();
        assert_eq!(
            saved["positions"][0]["qty"].to_string(),
            "0.5319148936170213"
        );
    }

    // Cycle driver.

    struct FakeKlines(HashMap<String, Vec<Kline>>);

    #[async_trait]
    impl KlineSource for FakeKlines {
        async fn recent_klines(&self, symbol: &str, _limit: u32) -> Option<Vec<Kline>> {
            self.0.get(symbol).cloned()
        }
    }

    fn kline(i: i64, low: Decimal, high: Decimal, close: Decimal) -> Kline {
        Kline {
            open_time: i * 60_000,
            open: close,
            high,
            low,
            close,
            volume: Decimal::ONE,
            close_time: None,
            quote_volume: None,
            trades: None,
            taker_buy_base_volume: None,
            taker_buy_quote_volume: None,
        }
    }

    /// 17 bars: flat at `start`, last bar closing at `last`.
    fn window(start: Decimal, last: Decimal) -> Vec<Kline> {
        let mut rows: Vec<Kline> = (0..16).map(|i| kline(i, start, start, start)).collect();
        rows.push(kline(16, last, start, last));
        rows
    }

    #[tokio::test]
    async fn signals_skip_short_or_invalid_windows() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let source = FakeKlines(
            [
                ("A/USDT".to_owned(), window(dec!(100), dec!(94))),
                (
                    "B/USDT".to_owned(),
                    window(dec!(100), dec!(94))[..15].to_vec(),
                ),
                ("C/USDT".to_owned(), window(dec!(0), dec!(94))),
            ]
            .into(),
        );
        let signals = fetch_signals(&source, &s).await;
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].symbol, "A/USDT");
        assert_eq!(signals[0].current_price, dec!(94));
        assert!((signals[0].lookback_return_pct - (94.0 / 100.0 - 1.0) * 100.0).abs() < 1e-12);
    }

    #[tokio::test]
    async fn paper_cycle_persists_state_ledger_and_audit() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let source = FakeKlines([("A/USDT".to_owned(), window(dec!(100), dec!(94)))].into());
        let payload = run_cycle(&s, Mode::Paper, &source).await.unwrap();
        assert_eq!(payload.signals_count, 1);
        assert_eq!(payload.open_positions, 1);
        assert_eq!(payload.ledger_entries_this_cycle.len(), 1);
        assert_eq!(
            load_state(Path::new(&s.state_path))
                .unwrap()
                .positions
                .len(),
            1
        );
        let ledger = std::fs::read_to_string(&s.ledger_path).unwrap();
        let row: serde_json::Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
        assert_eq!(row["action"], "open");
        assert_eq!(row["mode"], "paper");
        let audit = std::fs::read_to_string(&s.audit_path).unwrap();
        let row: serde_json::Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
        assert_eq!(row["mode"], "paper");
        assert_eq!(row["decisions"][0]["action"], "open");
        assert!(row["decisions"][0]["exit_price"].is_null());
        assert_eq!(row["top_signals"][0]["symbol"], "A/USDT");

        // Plan-only never mutates state.
        let plan = run_cycle(&s, Mode::PlanOnly, &source).await.unwrap();
        assert!(plan.ledger_entries_this_cycle.is_empty());
        assert_eq!(
            load_state(Path::new(&s.state_path)).unwrap().positions[0].bars_held,
            0
        );
    }

    #[tokio::test]
    async fn live_mode_is_not_implemented_after_gates() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeKlines(HashMap::new());
        let gated = run_cycle(&settings(dir.path()), Mode::Live, &source)
            .await
            .unwrap_err();
        assert!(gated.to_string().contains("MEAN_REV_PROVEN_EDGE"));
        let s = MeanReversionSettings {
            proven_edge: true,
            ..settings(dir.path())
        };
        let err = run_cycle(&s, Mode::Live, &source).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("live execution path is not yet implemented")
        );
        assert!(!Path::new(&s.audit_path).exists());
    }

    #[tokio::test]
    async fn daemon_cycle_audits_failures() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        std::fs::write(&s.emergency_stop_path, "halt").unwrap();
        let source = FakeKlines(HashMap::new());
        daemon_cycle(&s, Mode::Paper, &source).await.unwrap();
        std::fs::remove_file(&s.emergency_stop_path).unwrap();
        std::fs::write(&s.state_path, "{not json").unwrap();
        daemon_cycle(&s, Mode::Paper, &source).await.unwrap();
        let audit = std::fs::read_to_string(&s.audit_path).unwrap();
        let rows: Vec<serde_json::Value> = audit
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(
            rows[0]["error"]
                .as_str()
                .unwrap()
                .starts_with("emergency stop file present")
        );
        assert!(
            rows[1]["error"]
                .as_str()
                .unwrap()
                .starts_with("could not read state")
        );
        assert!(rows[0]["timestamp"].is_string());
    }

    #[tokio::test]
    async fn binance_source_requests_lookback_plus_two() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .and(query_param("symbol", "BTCUSDT"))
            .and(query_param("interval", "1m"))
            .and(query_param("limit", "17"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([[
                    0, "1.0", "2.0", "0.5", "1.5", "10", 59_999, "15", 3, "5", "7.5", "0"
                ],])),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .and(query_param("symbol", "ETHUSDT"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = BinanceClient::new(
            bb_binance::BinanceConfig::new(false)
                .with_spot_rest_url(server.uri())
                .with_futures_rest_url(server.uri()),
        )
        .unwrap();
        let source = BinanceKlines::new(client);
        let rows = source.recent_klines("BTC/USDT", 17).await.unwrap();
        assert_eq!(rows[0].close, dec!(1.5));
        assert!(source.recent_klines("ETH/USDT", 17).await.is_none());
    }
}

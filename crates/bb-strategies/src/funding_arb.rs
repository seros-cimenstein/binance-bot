//! Delta-neutral funding-rate harvest runtime (port of `src/funding_arb.py`).
//!
//! Per candidate USD-M perp: open (spot long + perp short) when the current funding
//! rate clears `entry_funding_bps`; close a held position when funding falls to
//! `exit_funding_bps`, `max_hold_funding_events` elapse, or the spot/perp basis blows
//! through the circuit breaker. Capacity is `max_concurrent_positions`, each sized at
//! `per_position_notional_pct` of the (optionally live-resolved) balance.
//!
//! Modes: `plan_only` (default, no state mutation), `paper` (simulated accrual + costs),
//! `live` (real orders, gated by `FUNDING_ARB_ENABLED=true`, `USE_TESTNET=false`,
//! `DRY_RUN=false`, `CONFIRM_LIVE_FUNDING_ARB=yes`, `FUNDING_ARB_PROVEN_EDGE=yes`).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::ExitCode;

use async_trait::async_trait;
use bb_binance::{BinanceClient, BinanceConfig, Credentials};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::common::{
    self, decimal_f64, decimal_number, env_csv, env_decimal, env_flag, env_or, env_parse, env_yes,
    utc_now_ms, utc_timestamp,
};
use crate::error::{Result, StrategyError};
use crate::funding_arb_backtest::fetch_perp_universe;
use crate::funding_arb_executor::FundingArbExecutor;

fn fa_err(message: impl Into<String>) -> StrategyError {
    StrategyError::FundingArb(message.into())
}

/// Runtime settings (Python `FundingArbSettings`).
#[derive(Clone, PartialEq)]
pub struct FundingArbSettings {
    pub api_key: String,
    pub api_secret: String,
    pub use_testnet: bool,
    pub dry_run: bool,
    pub enabled: bool,
    pub confirm_live_funding_arb: String,
    /// `FUNDING_ARB_PROVEN_EDGE=yes` (Python read it inside the preflight).
    pub proven_edge: bool,
    /// Explicit allowlist; empty = dynamic perp universe.
    pub universe: Vec<String>,
    pub dynamic_universe_size: usize,
    pub min_quote_volume_usdt: f64,
    pub entry_funding_bps: f64,
    pub exit_funding_bps: f64,
    pub max_hold_funding_events: i64,
    pub max_concurrent_positions: i64,
    pub per_position_notional_pct: f64,
    pub starting_balance_usdt: Decimal,
    pub taker_fee_spot_bps: f64,
    pub taker_fee_futures_bps: f64,
    pub slippage_per_leg_bps: f64,
    pub basis_circuit_breaker_bps: f64,
    pub state_path: String,
    pub audit_path: String,
    pub ledger_path: String,
    pub emergency_stop_path: String,
    /// Size off live spot + futures free USDT each cycle (fallback: configured balance).
    pub dynamic_balance: bool,
    /// Below this the runtime refuses to open positions (closes still run).
    pub min_balance_usdt: Decimal,
}

impl std::fmt::Debug for FundingArbSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FundingArbSettings")
            .field("enabled", &self.enabled)
            .field("use_testnet", &self.use_testnet)
            .field("dry_run", &self.dry_run)
            .field("universe", &self.universe)
            .field("state_path", &self.state_path)
            .finish_non_exhaustive()
    }
}

impl Default for FundingArbSettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_secret: String::new(),
            use_testnet: false,
            dry_run: true,
            enabled: false,
            confirm_live_funding_arb: "no".into(),
            proven_edge: false,
            universe: Vec::new(),
            dynamic_universe_size: 50,
            min_quote_volume_usdt: 50_000_000.0,
            entry_funding_bps: 3.0,
            exit_funding_bps: -1.0,
            max_hold_funding_events: 30,
            max_concurrent_positions: 5,
            per_position_notional_pct: 20.0,
            starting_balance_usdt: Decimal::from(10_000),
            taker_fee_spot_bps: 10.0,
            taker_fee_futures_bps: 5.0,
            slippage_per_leg_bps: 5.0,
            basis_circuit_breaker_bps: 200.0,
            state_path: "data/funding-arb-state.json".into(),
            audit_path: "data/funding-arb-decisions.jsonl".into(),
            ledger_path: "data/funding-arb-ledger.jsonl".into(),
            emergency_stop_path: "data/funding-arb.stop".into(),
            dynamic_balance: true,
            min_balance_usdt: Decimal::from(50),
        }
    }
}

fn env_f64(name: &str, default: f64) -> Result<f64> {
    env_parse(name, default).map_err(|_| fa_err(format!("{name} must be numeric")))
}

fn env_dec(name: &str, default: Decimal) -> Result<Decimal> {
    env_decimal(name, default).map_err(|_| fa_err(format!("{name} must be numeric")))
}

fn env_int<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    env_parse(name, default).map_err(|_| fa_err(format!("{name} must be an integer")))
}

impl FundingArbSettings {
    pub fn validate(&self) -> Result<()> {
        if self.starting_balance_usdt <= Decimal::ZERO {
            return Err(fa_err("starting_balance_usdt must be positive"));
        }
        if !(self.per_position_notional_pct > 0.0 && self.per_position_notional_pct <= 100.0) {
            return Err(fa_err("per_position_notional_pct must be in (0, 100]"));
        }
        if self.max_concurrent_positions <= 0 {
            return Err(fa_err("max_concurrent_positions must be positive"));
        }
        if self.max_hold_funding_events <= 0 {
            return Err(fa_err("max_hold_funding_events must be positive"));
        }
        if self.basis_circuit_breaker_bps <= 0.0 {
            return Err(fa_err("basis_circuit_breaker_bps must be positive"));
        }
        if self.min_balance_usdt < Decimal::ZERO {
            return Err(fa_err("min_balance_usdt must be non-negative"));
        }
        if self.entry_funding_bps <= self.exit_funding_bps {
            return Err(fa_err(
                "entry_funding_bps must be strictly above exit_funding_bps",
            ));
        }
        for (name, value) in [
            ("state_path", &self.state_path),
            ("audit_path", &self.audit_path),
            ("ledger_path", &self.ledger_path),
            ("emergency_stop_path", &self.emergency_stop_path),
        ] {
            if value.trim().is_empty() {
                return Err(fa_err(format!("{name} must not be blank")));
            }
        }
        Ok(())
    }

    pub fn from_env() -> Result<Self> {
        let api_key = env_or("BINANCE_API_KEY", "");
        let api_secret = env_or("BINANCE_API_SECRET", "");
        if api_key.is_empty() || api_secret.is_empty() {
            return Err(fa_err("BINANCE_API_KEY and BINANCE_API_SECRET must be set"));
        }
        let d = Self::default();
        let settings = Self {
            api_key,
            api_secret,
            use_testnet: env_flag("USE_TESTNET", false),
            dry_run: env_flag("DRY_RUN", true),
            enabled: env_flag("FUNDING_ARB_ENABLED", false),
            confirm_live_funding_arb: env_or("CONFIRM_LIVE_FUNDING_ARB", "no"),
            proven_edge: env_yes("FUNDING_ARB_PROVEN_EDGE"),
            universe: env_csv("FUNDING_ARB_UNIVERSE"),
            dynamic_universe_size: env_int(
                "FUNDING_ARB_DYNAMIC_UNIVERSE_SIZE",
                d.dynamic_universe_size,
            )?,
            min_quote_volume_usdt: env_f64(
                "FUNDING_ARB_MIN_QUOTE_VOLUME_USDT",
                d.min_quote_volume_usdt,
            )?,
            entry_funding_bps: env_f64("FUNDING_ARB_ENTRY_FUNDING_BPS", d.entry_funding_bps)?,
            exit_funding_bps: env_f64("FUNDING_ARB_EXIT_FUNDING_BPS", d.exit_funding_bps)?,
            max_hold_funding_events: env_int(
                "FUNDING_ARB_MAX_HOLD_FUNDING_EVENTS",
                d.max_hold_funding_events,
            )?,
            max_concurrent_positions: env_int(
                "FUNDING_ARB_MAX_CONCURRENT_POSITIONS",
                d.max_concurrent_positions,
            )?,
            per_position_notional_pct: env_f64(
                "FUNDING_ARB_PER_POSITION_NOTIONAL_PCT",
                d.per_position_notional_pct,
            )?,
            starting_balance_usdt: env_dec(
                "FUNDING_ARB_STARTING_BALANCE_USDT",
                d.starting_balance_usdt,
            )?,
            taker_fee_spot_bps: env_f64("FUNDING_ARB_TAKER_FEE_SPOT_BPS", d.taker_fee_spot_bps)?,
            taker_fee_futures_bps: env_f64(
                "FUNDING_ARB_TAKER_FEE_FUTURES_BPS",
                d.taker_fee_futures_bps,
            )?,
            slippage_per_leg_bps: env_f64(
                "FUNDING_ARB_SLIPPAGE_PER_LEG_BPS",
                d.slippage_per_leg_bps,
            )?,
            basis_circuit_breaker_bps: env_f64(
                "FUNDING_ARB_BASIS_CIRCUIT_BREAKER_BPS",
                d.basis_circuit_breaker_bps,
            )?,
            state_path: env_or("FUNDING_ARB_STATE_PATH", &d.state_path),
            audit_path: env_or("FUNDING_ARB_AUDIT_PATH", &d.audit_path),
            ledger_path: env_or("FUNDING_ARB_LEDGER_PATH", &d.ledger_path),
            emergency_stop_path: env_or("FUNDING_ARB_EMERGENCY_STOP_PATH", &d.emergency_stop_path),
            dynamic_balance: env_flag("FUNDING_ARB_DYNAMIC_BALANCE", true),
            min_balance_usdt: env_dec("FUNDING_ARB_MIN_BALANCE_USDT", d.min_balance_usdt)?,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Spot fee + futures fee + two slippage legs (one side of the round trip).
    pub fn one_side_cost_bps(&self) -> f64 {
        self.taker_fee_spot_bps + self.taker_fee_futures_bps + 2.0 * self.slippage_per_leg_bps
    }

    pub fn round_trip_cost_bps(&self) -> f64 {
        2.0 * self.one_side_cost_bps()
    }
}

/// `amount × bps / 10 000` in money space.
fn bps_of(amount: Decimal, bps: f64) -> Decimal {
    amount * bb_core::money::from_f64(bps).unwrap_or_default() / Decimal::from(10_000)
}

/// A live delta-neutral position: long spot + short perp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundingArbPosition {
    /// CCXT-style, e.g. `SOL/USDT`.
    pub symbol: String,
    pub open_time_ms: i64,
    pub entry_funding_bps: f64,
    #[serde(with = "decimal_number")]
    pub notional_usdt: Decimal,
    #[serde(with = "decimal_number")]
    pub spot_qty: Decimal,
    #[serde(with = "decimal_number")]
    pub perp_qty: Decimal,
    #[serde(default)]
    pub funding_received_bps: f64,
    #[serde(default, with = "decimal_number")]
    pub funding_received_usdt: Decimal,
    #[serde(default)]
    pub events_held: i64,
}

impl FundingArbPosition {
    /// Credit one funding event at `funding_bps`.
    fn accrue(&mut self, funding_bps: f64) {
        self.funding_received_bps += funding_bps;
        self.funding_received_usdt += bps_of(self.notional_usdt, funding_bps);
        self.events_held += 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Open,
    Close,
    Hold,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FundingArbDecision {
    pub action: Action,
    pub symbol: String,
    pub reason: String,
    pub current_funding_bps: f64,
    #[serde(with = "decimal_number")]
    pub notional_usdt: Decimal,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FundingArbState {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub positions: Vec<FundingArbPosition>,
}

fn null_as_empty<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<FundingArbPosition>, D::Error> {
    Ok(Option::<Vec<FundingArbPosition>>::deserialize(deserializer)?.unwrap_or_default())
}

pub fn load_state(path: &Path) -> Result<FundingArbState> {
    if !path.exists() {
        return Ok(FundingArbState::default());
    }
    std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
        .map_err(|e| {
            fa_err(format!(
                "could not read funding-arb state at {}: {e}",
                path.display()
            ))
        })
}

pub fn save_state(state: &FundingArbState, path: &Path) -> Result<()> {
    common::save_json_sorted(path, state)
}

pub fn preflight(settings: &FundingArbSettings, submit_order: bool) -> Result<()> {
    if Path::new(&settings.emergency_stop_path).exists() {
        return Err(fa_err(format!(
            "emergency stop file present at {}; refusing to run",
            settings.emergency_stop_path
        )));
    }
    if !submit_order {
        return Ok(());
    }
    if !settings.enabled {
        return Err(fa_err(
            "FUNDING_ARB_ENABLED=true is required for live submission",
        ));
    }
    if settings.use_testnet {
        return Err(fa_err(
            "funding-arb live submission requires USE_TESTNET=false",
        ));
    }
    if settings.dry_run {
        return Err(fa_err("funding-arb live submission requires DRY_RUN=false"));
    }
    if settings.confirm_live_funding_arb != "yes" {
        return Err(fa_err(
            "funding-arb live submission requires CONFIRM_LIVE_FUNDING_ARB=yes",
        ));
    }
    if !settings.proven_edge {
        return Err(fa_err(
            "funding-arb live submission requires FUNDING_ARB_PROVEN_EDGE=yes. Run `bb funding-arb-backtest` \
             and only set this when the harness reports verdict=edge_likely under conservative cost \
             assumptions. Until then, plan-only is the only available mode.",
        ));
    }
    Ok(())
}

/// Per-symbol snapshot driving a decision.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateSignal {
    pub symbol: String,
    pub funding_bps: f64,
    pub mark_price: Decimal,
    pub spot_price: Decimal,
}

impl CandidateSignal {
    pub fn basis_bps(&self) -> f64 {
        let spot = decimal_f64(self.spot_price);
        if spot <= 0.0 {
            return 0.0;
        }
        (decimal_f64(self.mark_price) - spot) / spot * 10_000.0
    }
}

/// Deterministic open/close/hold decider for the held + candidate universe.
pub struct FundingArbAdvisor<'a> {
    pub settings: &'a FundingArbSettings,
}

impl FundingArbAdvisor<'_> {
    pub fn decide(
        &self,
        state: &FundingArbState,
        signals: &[CandidateSignal],
    ) -> Vec<FundingArbDecision> {
        let cfg = self.settings;
        let by_symbol: HashMap<&str, &CandidateSignal> =
            signals.iter().map(|s| (s.symbol.as_str(), s)).collect();
        let held: HashSet<&str> = state.positions.iter().map(|p| p.symbol.as_str()).collect();
        let decision = |action, symbol: &str, reason: String, funding: f64| FundingArbDecision {
            action,
            symbol: symbol.to_owned(),
            reason,
            current_funding_bps: funding,
            notional_usdt: Decimal::ZERO,
        };
        let mut decisions = Vec::new();

        // 1. Per held position: close or hold.
        for position in &state.positions {
            let symbol = position.symbol.as_str();
            let Some(signal) = by_symbol.get(symbol) else {
                decisions.push(decision(
                    Action::Hold,
                    symbol,
                    "no current signal for held position; holding pending data".into(),
                    0.0,
                ));
                continue;
            };
            let basis_bps = signal.basis_bps();
            let funding = signal.funding_bps;
            let (action, reason) = if basis_bps.abs() >= cfg.basis_circuit_breaker_bps {
                (
                    Action::Close,
                    format!(
                        "basis circuit breaker: {basis_bps:.1}bps exceeds {:.1}bps cap",
                        cfg.basis_circuit_breaker_bps
                    ),
                )
            } else if position.events_held >= cfg.max_hold_funding_events {
                (
                    Action::Close,
                    format!("max hold ({} events) reached", cfg.max_hold_funding_events),
                )
            } else if funding <= cfg.exit_funding_bps {
                (
                    Action::Close,
                    format!(
                        "funding {funding:.2}bps fell to or below exit threshold {:.2}bps",
                        cfg.exit_funding_bps
                    ),
                )
            } else {
                (
                    Action::Hold,
                    format!(
                        "funding {funding:.2}bps still above exit ({:.2}bps); held {} events",
                        cfg.exit_funding_bps, position.events_held
                    ),
                )
            };
            decisions.push(decision(action, symbol, reason, funding));
        }

        // 2. Pending closes free capacity in the same cycle.
        let pending_closes: HashSet<String> = decisions
            .iter()
            .filter(|d| d.action == Action::Close)
            .map(|d| d.symbol.clone())
            .collect();
        let free_slots =
            cfg.max_concurrent_positions - held.len() as i64 + pending_closes.len() as i64;

        // 3. Rank open candidates by funding (stable, descending).
        let mut opens: Vec<&CandidateSignal> = signals
            .iter()
            .filter(|s| !held.contains(s.symbol.as_str()) || pending_closes.contains(&s.symbol))
            .filter(|s| s.funding_bps >= cfg.entry_funding_bps)
            .filter(|s| s.basis_bps().abs() < cfg.basis_circuit_breaker_bps)
            .collect();
        opens.sort_by(|a, b| b.funding_bps.total_cmp(&a.funding_bps));
        let pct = bb_core::money::from_f64(cfg.per_position_notional_pct).unwrap_or_default();
        let notional = cfg.starting_balance_usdt * pct / Decimal::from(100);
        let take = usize::try_from(free_slots.max(0)).unwrap_or(0);
        for signal in opens.into_iter().take(take) {
            decisions.push(FundingArbDecision {
                action: Action::Open,
                symbol: signal.symbol.clone(),
                reason: format!(
                    "funding {:.2}bps clears entry ({:.2}bps); basis {:.1}bps within breaker",
                    signal.funding_bps,
                    cfg.entry_funding_bps,
                    signal.basis_bps()
                ),
                current_funding_bps: signal.funding_bps,
                notional_usdt: notional,
            });
        }
        decisions
    }
}

/// One closed trade recorded for P&L attribution.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaperLedgerEntry {
    pub symbol: String,
    pub open_time_ms: i64,
    pub close_time_ms: i64,
    pub entry_funding_bps: f64,
    pub funding_received_bps: f64,
    #[serde(with = "decimal_number")]
    pub funding_received_usdt: Decimal,
    pub events_held: i64,
    pub round_trip_cost_bps: f64,
    #[serde(with = "decimal_number")]
    pub round_trip_cost_usdt: Decimal,
    pub net_pnl_bps: f64,
    #[serde(with = "decimal_number")]
    pub net_pnl_usdt: Decimal,
}

impl PaperLedgerEntry {
    fn closed(
        position: &FundingArbPosition,
        close_time_ms: i64,
        round_trip_cost_bps: f64,
        gross_pnl_usdt: Decimal,
    ) -> Self {
        let cost_usdt = bps_of(position.notional_usdt, round_trip_cost_bps);
        Self {
            symbol: position.symbol.clone(),
            open_time_ms: position.open_time_ms,
            close_time_ms,
            entry_funding_bps: position.entry_funding_bps,
            funding_received_bps: position.funding_received_bps,
            funding_received_usdt: position.funding_received_usdt,
            events_held: position.events_held,
            round_trip_cost_bps,
            round_trip_cost_usdt: cost_usdt,
            net_pnl_bps: position.funding_received_bps - round_trip_cost_bps,
            net_pnl_usdt: gross_pnl_usdt - cost_usdt,
        }
    }
}

/// Apply decisions to in-memory state for paper trading: funding is credited once per
/// cycle per open position; closes pay the full modelled round trip.
pub fn apply_paper_decisions(
    settings: &FundingArbSettings,
    state: &FundingArbState,
    signals: &[CandidateSignal],
    decisions: &[FundingArbDecision],
    cycle_ts_ms: i64,
) -> (FundingArbState, Vec<PaperLedgerEntry>) {
    let by_symbol: HashMap<&str, &CandidateSignal> =
        signals.iter().map(|s| (s.symbol.as_str(), s)).collect();
    let closes: HashSet<&str> = decisions
        .iter()
        .filter(|d| d.action == Action::Close)
        .map(|d| d.symbol.as_str())
        .collect();
    let round_trip_cost_bps = settings.round_trip_cost_bps();
    let mut updated = Vec::new();
    let mut ledger = Vec::new();

    for position in &state.positions {
        let mut position = position.clone();
        if let Some(signal) = by_symbol.get(position.symbol.as_str()) {
            position.accrue(signal.funding_bps);
        }
        if closes.contains(position.symbol.as_str()) {
            let gross = position.funding_received_usdt;
            ledger.push(PaperLedgerEntry::closed(
                &position,
                cycle_ts_ms,
                round_trip_cost_bps,
                gross,
            ));
        } else {
            updated.push(position);
        }
    }

    for decision in decisions.iter().filter(|d| d.action == Action::Open) {
        let Some(signal) = by_symbol.get(decision.symbol.as_str()) else {
            continue;
        };
        if signal.spot_price <= Decimal::ZERO {
            continue;
        }
        let spot_qty = decision.notional_usdt / signal.spot_price;
        updated.push(FundingArbPosition {
            symbol: decision.symbol.clone(),
            open_time_ms: cycle_ts_ms,
            entry_funding_bps: decision.current_funding_bps,
            notional_usdt: decision.notional_usdt,
            spot_qty,
            perp_qty: -spot_qty,
            funding_received_bps: 0.0,
            funding_received_usdt: Decimal::ZERO,
            events_held: 0,
        });
    }
    (FundingArbState { positions: updated }, ledger)
}

pub fn append_ledger(path: &Path, entries: &[PaperLedgerEntry]) -> Result<()> {
    common::append_jsonl_many(path, entries)
}

/// Universe + current signals (public market data), behind a trait for tests.
#[async_trait]
pub trait MarketData: Send + Sync {
    async fn universe(&self, settings: &FundingArbSettings) -> Result<Vec<String>>;
    async fn signals(&self, symbols: &[String]) -> Result<Vec<CandidateSignal>>;
}

pub struct BinanceMarketData {
    client: BinanceClient,
}

impl BinanceMarketData {
    pub fn new(client: BinanceClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl MarketData for BinanceMarketData {
    async fn universe(&self, settings: &FundingArbSettings) -> Result<Vec<String>> {
        if !settings.universe.is_empty() {
            return Ok(settings.universe.clone());
        }
        fetch_perp_universe(
            &self.client,
            settings.dynamic_universe_size,
            settings.min_quote_volume_usdt,
        )
        .await
    }

    /// Current funding rate + mark price (`premiumIndex`) and spot last price per symbol;
    /// symbols without a mark row or a positive spot price are skipped.
    async fn signals(&self, symbols: &[String]) -> Result<Vec<CandidateSignal>> {
        let marks = self.client.premium_indexes().await?;
        let spots = self.client.prices().await?;
        let spot_by_symbol: HashMap<&str, Decimal> =
            spots.iter().map(|p| (p.symbol.as_str(), p.price)).collect();
        let mark_by_symbol: HashMap<&str, _> =
            marks.iter().map(|m| (m.symbol.as_str(), m)).collect();
        Ok(symbols
            .iter()
            .filter_map(|symbol| {
                let rest = symbol.replace('/', "").to_uppercase();
                let mark = mark_by_symbol.get(rest.as_str())?;
                let spot_price = spot_by_symbol
                    .get(rest.as_str())
                    .copied()
                    .unwrap_or_default();
                (spot_price > Decimal::ZERO).then(|| CandidateSignal {
                    symbol: symbol.clone(),
                    funding_bps: decimal_f64(mark.last_funding_rate) * 10_000.0,
                    mark_price: mark.mark_price,
                    spot_price,
                })
            })
            .collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    PlanOnly,
    Paper,
    Live,
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

/// One live open/close outcome in the audit payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveResult {
    pub action: Action,
    pub symbol: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_decimal"
    )]
    pub realized_pnl_usdt: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spot_order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perp_order_id: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_decimal"
    )]
    pub filled_qty: Option<Decimal>,
}

impl LiveResult {
    fn failed(action: Action, symbol: &str, error: &StrategyError) -> Self {
        Self {
            action,
            symbol: symbol.to_owned(),
            ok: false,
            error: Some(error.to_string()),
            realized_pnl_usdt: None,
            spot_order_id: None,
            perp_order_id: None,
            filled_qty: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopSignal {
    pub symbol: String,
    pub funding_bps: f64,
    pub basis_bps: f64,
}

/// Audit payload appended to the decisions JSONL and printed by the CLI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CyclePayload {
    pub timestamp: String,
    pub mode: Mode,
    pub universe_size: usize,
    pub signals_count: usize,
    pub open_positions: usize,
    pub decisions: Vec<FundingArbDecision>,
    pub ledger_entries_this_cycle: Vec<PaperLedgerEntry>,
    pub live_results: Vec<LiveResult>,
    pub halt_reason: Option<String>,
    pub balance_source: &'static str,
    #[serde(with = "decimal_number")]
    pub configured_balance_usdt: Decimal,
    #[serde(with = "decimal_number")]
    pub effective_balance_usdt: Decimal,
    #[serde(with = "decimal_number")]
    pub min_balance_usdt: Decimal,
    pub insufficient_balance: bool,
    pub top_signals: Vec<TopSignal>,
}

/// Short-lived signed client for balance reads when no executor exists (plan/paper).
fn signed_client(settings: &FundingArbSettings) -> Result<BinanceClient> {
    let credentials = Credentials::new(settings.api_key.clone(), &settings.api_secret)?;
    Ok(BinanceClient::new(
        BinanceConfig::new(false).with_credentials(credentials),
    )?)
}

/// `(effective_balance_usdt, source)`; source is `configured`, `live` or `fallback_configured`.
async fn resolve_cycle_balance(
    settings: &FundingArbSettings,
    executor: Option<&FundingArbExecutor>,
) -> (Decimal, &'static str) {
    if !settings.dynamic_balance {
        return (settings.starting_balance_usdt, "configured");
    }
    let live = match executor {
        Some(executor) => executor.free_usdt().await,
        None => match signed_client(settings) {
            Ok(client) => FundingArbExecutor::binance(client).free_usdt().await,
            Err(err) => Err(err),
        },
    };
    match live {
        Ok(balance) => (balance, "live"),
        Err(err) => {
            tracing::warn!(error = %err, "funding-arb live balance fetch failed; using configured balance");
            (settings.starting_balance_usdt, "fallback_configured")
        }
    }
}

/// Python `repr(list_of_str)`.
fn py_str_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", quoted.join(", "))
}

fn signed_fixed(value: Decimal, dp: u32) -> String {
    let rounded = value.round_dp(dp);
    let sign = if rounded.is_sign_negative() { "" } else { "+" };
    format!("{sign}{rounded:.prec$}", prec = dp as usize)
}

/// Run one decision cycle and append its audit payload. Live mode reconciles first and
/// halts (error after auditing) on any mismatch.
pub async fn run_cycle(
    settings: &FundingArbSettings,
    mode: Mode,
    executor: Option<&FundingArbExecutor>,
    market: &dyn MarketData,
) -> Result<CyclePayload> {
    preflight(settings, mode == Mode::Live)?;
    let mut state = load_state(Path::new(&settings.state_path))?;
    let (effective_balance, balance_source) = resolve_cycle_balance(settings, executor).await;
    let effective = FundingArbSettings {
        starting_balance_usdt: effective_balance,
        ..settings.clone()
    };
    let insufficient_balance = effective_balance < settings.min_balance_usdt;

    let symbols = market.universe(settings).await?;
    let signals = market.signals(&symbols).await?;
    let mut decisions = FundingArbAdvisor {
        settings: &effective,
    }
    .decide(&state, &signals);
    if insufficient_balance {
        // Skip new opens but still allow closes for safety.
        decisions.retain(|d| d.action != Action::Open);
    }

    let mut ledger = Vec::new();
    let mut live_results = Vec::new();
    let mut halt_reason = None;
    match mode {
        Mode::PlanOnly => {}
        Mode::Paper => {
            let (next, entries) =
                apply_paper_decisions(&effective, &state, &signals, &decisions, utc_now_ms());
            state = next;
            ledger = entries;
            save_state(&state, Path::new(&settings.state_path))?;
            append_ledger(Path::new(&settings.ledger_path), &ledger)?;
        }
        Mode::Live => {
            let executor =
                executor.ok_or_else(|| fa_err("live mode requires an executor instance"))?;
            let report = executor.reconcile(&state.positions).await?;
            if report.ok() {
                let (next, entries, results) =
                    execute_live_decisions(&effective, &state, &signals, &decisions, executor)
                        .await;
                state = next;
                ledger = entries;
                live_results = results;
                save_state(&state, Path::new(&settings.state_path))?;
                append_ledger(Path::new(&settings.ledger_path), &ledger)?;
            } else {
                let details: Vec<String> =
                    report.mismatches.iter().map(|m| m.detail.clone()).collect();
                let reason = format!("reconciliation mismatch: {}", py_str_list(&details));
                let excerpt: String = reason.chars().take(300).collect();
                common::alert(&format!(
                    "funding arb HALTED — on-exchange state does not match local state. \
                     Manual investigation required. Detail: {excerpt}"
                ))
                .await;
                halt_reason = Some(reason);
            }
        }
    }

    let mut top_signals: Vec<TopSignal> = signals
        .iter()
        .map(|s| TopSignal {
            symbol: s.symbol.clone(),
            funding_bps: s.funding_bps,
            basis_bps: s.basis_bps(),
        })
        .collect();
    top_signals.sort_by(|a, b| b.funding_bps.total_cmp(&a.funding_bps));
    top_signals.truncate(10);

    let payload = CyclePayload {
        timestamp: utc_timestamp(),
        mode,
        universe_size: symbols.len(),
        signals_count: signals.len(),
        open_positions: state.positions.len(),
        decisions,
        ledger_entries_this_cycle: ledger,
        live_results,
        halt_reason: halt_reason.clone(),
        balance_source,
        configured_balance_usdt: settings.starting_balance_usdt,
        effective_balance_usdt: effective_balance,
        min_balance_usdt: settings.min_balance_usdt,
        insufficient_balance,
        top_signals,
    };
    common::append_jsonl(Path::new(&settings.audit_path), &payload)?;
    match halt_reason {
        Some(reason) => Err(fa_err(reason)),
        None => Ok(payload),
    }
}

/// Insertion-ordered `symbol → position` map (Python dict semantics).
#[derive(Default)]
struct OrderedPositions(Vec<FundingArbPosition>);

impl OrderedPositions {
    fn get_mut(&mut self, symbol: &str) -> Option<&mut FundingArbPosition> {
        self.0.iter_mut().find(|p| p.symbol == symbol)
    }

    fn insert(&mut self, position: FundingArbPosition) {
        match self.get_mut(&position.symbol) {
            Some(slot) => *slot = position,
            None => self.0.push(position),
        }
    }

    fn remove(&mut self, symbol: &str) {
        self.0.retain(|p| p.symbol != symbol);
    }
}

/// Live closes first (free capacity), then opens; state follows confirmed results.
/// A failed leg is recorded and the position kept for manual reconciliation.
async fn execute_live_decisions(
    settings: &FundingArbSettings,
    state: &FundingArbState,
    signals: &[CandidateSignal],
    decisions: &[FundingArbDecision],
    executor: &FundingArbExecutor,
) -> (FundingArbState, Vec<PaperLedgerEntry>, Vec<LiveResult>) {
    let cycle_ts_ms = utc_now_ms();
    let by_symbol: HashMap<&str, &CandidateSignal> =
        signals.iter().map(|s| (s.symbol.as_str(), s)).collect();
    let close_symbols: Vec<&str> = decisions
        .iter()
        .filter(|d| d.action == Action::Close)
        .map(|d| d.symbol.as_str())
        .collect();
    let round_trip_cost_bps = settings.round_trip_cost_bps();
    let mut ledger = Vec::new();
    let mut results = Vec::new();

    // 1. Accrue current funding on still-held positions (those not being closed).
    let mut held = OrderedPositions::default();
    for position in &state.positions {
        held.insert(position.clone());
    }
    for position in &mut held.0 {
        if let Some(signal) = by_symbol.get(position.symbol.as_str())
            && !close_symbols.contains(&position.symbol.as_str())
        {
            position.accrue(signal.funding_bps);
        }
    }

    // 2. Closes (accruing the final funding event first).
    for symbol in close_symbols {
        let Some(position) = held.get_mut(symbol) else {
            continue;
        };
        if let Some(signal) = by_symbol.get(symbol) {
            position.accrue(signal.funding_bps);
        }
        let position = position.clone();
        let closed = match executor.close_position(&position).await {
            Ok(closed) => closed,
            Err(err) => {
                tracing::error!(%symbol, error = %err, "funding-arb close failed");
                results.push(LiveResult::failed(Action::Close, symbol, &err));
                continue;
            }
        };
        ledger.push(PaperLedgerEntry::closed(
            &position,
            cycle_ts_ms,
            round_trip_cost_bps,
            closed.realized_pnl_usdt,
        ));
        results.push(LiveResult {
            action: Action::Close,
            symbol: symbol.to_owned(),
            ok: true,
            error: None,
            realized_pnl_usdt: Some(closed.realized_pnl_usdt),
            spot_order_id: Some(closed.spot_leg.order_id.clone()),
            perp_order_id: Some(closed.perp_leg.order_id.clone()),
            filled_qty: None,
        });
        held.remove(symbol);
        common::alert(&format!(
            "funding arb CLOSE {symbol}: realised P&L ${}, held {} funding events, funding collected {:+.2} bps",
            signed_fixed(closed.realized_pnl_usdt, 4),
            position.events_held,
            position.funding_received_bps
        ))
        .await;
    }

    // 3. Opens.
    for decision in decisions.iter().filter(|d| d.action == Action::Open) {
        let symbol = decision.symbol.as_str();
        let opened = match executor
            .open_position(
                symbol,
                decision.notional_usdt,
                decision.current_funding_bps,
                cycle_ts_ms,
            )
            .await
        {
            Ok(opened) => opened,
            Err(err) => {
                tracing::error!(%symbol, error = %err, "funding-arb open failed");
                results.push(LiveResult::failed(Action::Open, symbol, &err));
                continue;
            }
        };
        common::alert(&format!(
            "funding arb OPEN {symbol}: filled {:.6} @ entry funding {:.2} bps, notional ${:.4}",
            opened.spot_leg.filled_qty, decision.current_funding_bps, decision.notional_usdt
        ))
        .await;
        results.push(LiveResult {
            action: Action::Open,
            symbol: symbol.to_owned(),
            ok: true,
            error: None,
            realized_pnl_usdt: None,
            spot_order_id: Some(opened.spot_leg.order_id.clone()),
            perp_order_id: Some(opened.perp_leg.order_id.clone()),
            filled_qty: Some(opened.spot_leg.filled_qty),
        });
        held.insert(opened.position);
    }
    (FundingArbState { positions: held.0 }, ledger, results)
}

#[derive(clap::Args, Debug, Clone, Default)]
pub struct Args {
    /// Compute decisions without mutating state (default).
    #[arg(long, conflicts_with_all = ["paper", "submit_live"])]
    pub plan_only: bool,
    /// Materialize decisions in local state with simulated funding/fees, no real orders.
    #[arg(long, conflicts_with_all = ["plan_only", "submit_live"])]
    pub paper: bool,
    /// Live execution (spot + USD-M futures) behind the preflight gates.
    #[arg(long, conflicts_with_all = ["plan_only", "paper"])]
    pub submit_live: bool,
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

async fn run_mode(settings: &FundingArbSettings, mode: Mode) -> Result<CyclePayload> {
    let market = BinanceMarketData::new(common::public_client()?);
    if mode == Mode::Live {
        // Gate before constructing any signed trading client.
        preflight(settings, true)?;
        let executor = FundingArbExecutor::binance(signed_client(settings)?);
        run_cycle(settings, mode, Some(&executor), &market).await
    } else {
        run_cycle(settings, mode, None, &market).await
    }
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let outcome = match FundingArbSettings::from_env() {
        Ok(settings) => run_mode(&settings, args.mode()).await,
        Err(err) => Err(err),
    };
    match outcome {
        Ok(payload) => {
            common::print_json(&payload)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            println!("funding-arb failed: {err}");
            Ok(ExitCode::from(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rust_decimal_macros::dec;

    use super::*;
    use crate::funding_arb_executor::fakes::*;

    fn settings(dir: &Path) -> FundingArbSettings {
        let s = FundingArbSettings {
            api_key: "k".into(),
            api_secret: "s".into(),
            use_testnet: false,
            dry_run: false,
            enabled: true,
            confirm_live_funding_arb: "yes".into(),
            max_concurrent_positions: 3,
            dynamic_balance: false,
            state_path: dir.join("state.json").display().to_string(),
            audit_path: dir.join("audit.jsonl").display().to_string(),
            ledger_path: dir.join("ledger.jsonl").display().to_string(),
            emergency_stop_path: dir.join("stop").display().to_string(),
            ..FundingArbSettings::default()
        };
        s.validate().unwrap();
        s
    }

    fn signal(symbol: &str, funding_bps: f64, spot: Decimal, mark: Decimal) -> CandidateSignal {
        CandidateSignal {
            symbol: symbol.into(),
            funding_bps,
            mark_price: mark,
            spot_price: spot,
        }
    }

    fn sig(symbol: &str, funding_bps: f64) -> CandidateSignal {
        signal(symbol, funding_bps, dec!(100), dec!(100))
    }

    fn position(symbol: &str, events_held: i64) -> FundingArbPosition {
        FundingArbPosition {
            symbol: symbol.into(),
            open_time_ms: 0,
            entry_funding_bps: 5.0,
            notional_usdt: dec!(2000),
            spot_qty: dec!(20),
            perp_qty: dec!(-20),
            funding_received_bps: 0.0,
            funding_received_usdt: Decimal::ZERO,
            events_held,
        }
    }

    fn state(positions: Vec<FundingArbPosition>) -> FundingArbState {
        FundingArbState { positions }
    }

    fn actions(decisions: &[FundingArbDecision]) -> Vec<(Action, String)> {
        decisions
            .iter()
            .map(|d| (d.action, d.symbol.clone()))
            .collect()
    }

    #[test]
    fn advisor_entry_capacity_and_holds() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let advisor = FundingArbAdvisor { settings: &s };
        assert!(
            advisor
                .decide(
                    &FundingArbState::default(),
                    &[sig("FOO/USDT", 1.0), sig("BAR/USDT", 2.0)]
                )
                .is_empty()
        );

        let opened = advisor.decide(
            &FundingArbState::default(),
            &[sig("BTC/USDT", 5.0), sig("ETH/USDT", 4.0)],
        );
        assert_eq!(
            actions(&opened),
            vec![
                (Action::Open, "BTC/USDT".into()),
                (Action::Open, "ETH/USDT".into())
            ]
        );
        assert!(opened.iter().all(|d| d.notional_usdt == dec!(2000)));

        let two = FundingArbSettings {
            max_concurrent_positions: 2,
            ..s.clone()
        };
        let ranked = FundingArbAdvisor { settings: &two }.decide(
            &FundingArbState::default(),
            &[
                sig("A/USDT", 10.0),
                sig("B/USDT", 9.0),
                sig("C/USDT", 8.0),
                sig("D/USDT", 7.0),
            ],
        );
        let symbols: Vec<_> = ranked.iter().map(|d| d.symbol.as_str()).collect();
        assert_eq!(symbols, ["A/USDT", "B/USDT"]);

        let held = advisor.decide(
            &state(vec![position("BTC/USDT", 5)]),
            &[sig("BTC/USDT", 2.0)],
        );
        assert_eq!(actions(&held), vec![(Action::Hold, "BTC/USDT".into())]);
    }

    #[test]
    fn advisor_close_rules() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let advisor = FundingArbAdvisor { settings: &s };
        let closed = advisor.decide(
            &state(vec![position("BTC/USDT", 0)]),
            &[sig("BTC/USDT", -2.0)],
        );
        assert!(
            closed
                .iter()
                .any(|d| d.action == Action::Close && d.symbol == "BTC/USDT")
        );

        let max_hold = FundingArbSettings {
            max_hold_funding_events: 10,
            ..s.clone()
        };
        let d = FundingArbAdvisor {
            settings: &max_hold,
        }
        .decide(
            &state(vec![position("BTC/USDT", 10)]),
            &[sig("BTC/USDT", 5.0)],
        );
        assert_eq!(d[0].action, Action::Close);
        assert!(d[0].reason.contains("max hold"));

        let breaker = FundingArbSettings {
            basis_circuit_breaker_bps: 100.0,
            ..s.clone()
        };
        let blown = signal("BTC/USDT", 5.0, dec!(100), dec!(102));
        let d = FundingArbAdvisor { settings: &breaker }.decide(
            &state(vec![position("BTC/USDT", 0)]),
            std::slice::from_ref(&blown),
        );
        assert_eq!(d[0].action, Action::Close);
        assert!(d[0].reason.contains("basis circuit breaker"));
        assert!(
            FundingArbAdvisor { settings: &breaker }
                .decide(&FundingArbState::default(), &[blown])
                .is_empty()
        );
    }

    #[test]
    fn pending_close_frees_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let one = FundingArbSettings {
            max_concurrent_positions: 1,
            ..settings(dir.path())
        };
        let d = FundingArbAdvisor { settings: &one }.decide(
            &state(vec![position("BTC/USDT", 0)]),
            &[sig("BTC/USDT", -5.0), sig("SOL/USDT", 8.0)],
        );
        let acts = actions(&d);
        assert!(acts.contains(&(Action::Close, "BTC/USDT".into())));
        assert!(acts.contains(&(Action::Open, "SOL/USDT".into())));
    }

    #[test]
    fn settings_validation() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let inverted = FundingArbSettings {
            entry_funding_bps: 1.0,
            exit_funding_bps: 2.0,
            ..s.clone()
        };
        assert!(
            inverted
                .validate()
                .unwrap_err()
                .to_string()
                .contains("entry_funding_bps must be strictly above")
        );
        let bad_pct = FundingArbSettings {
            per_position_notional_pct: 150.0,
            ..s
        };
        assert!(
            bad_pct
                .validate()
                .unwrap_err()
                .to_string()
                .contains("per_position_notional_pct")
        );
    }

    #[test]
    fn preflight_gates() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        preflight(&s, false).unwrap();
        assert!(
            preflight(&s, true)
                .unwrap_err()
                .to_string()
                .contains("FUNDING_ARB_PROVEN_EDGE")
        );
        let proven = FundingArbSettings {
            proven_edge: true,
            ..s.clone()
        };
        preflight(&proven, true).unwrap();
        let testnet = FundingArbSettings {
            use_testnet: true,
            ..proven.clone()
        };
        assert!(
            preflight(&testnet, true)
                .unwrap_err()
                .to_string()
                .contains("USE_TESTNET=false")
        );
        std::fs::write(&s.emergency_stop_path, "halt").unwrap();
        assert!(
            preflight(&s, false)
                .unwrap_err()
                .to_string()
                .contains("emergency stop")
        );
    }

    #[test]
    fn state_round_trip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut a = position("BTC/USDT", 3);
        a.entry_funding_bps = 5.0;
        let mut b = position("SOL/USDT", 1);
        b.entry_funding_bps = 8.0;
        save_state(&state(vec![a.clone(), b.clone()]), &path).unwrap();
        let restored = load_state(&path).unwrap();
        assert_eq!(restored.positions, vec![a, b]);
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw["positions"][0]["notional_usdt"].is_number());
        assert!(
            load_state(&dir.path().join("missing.json"))
                .unwrap()
                .positions
                .is_empty()
        );
        // Python-written floats load.
        std::fs::write(
            &path,
            r#"{"positions": [{"symbol": "X/USDT", "open_time_ms": 1, "entry_funding_bps": 4.0,
            "notional_usdt": 199.99999999999997, "spot_qty": 1e-05, "perp_qty": -1e-05}]}"#,
        )
        .unwrap();
        let loaded = load_state(&path).unwrap();
        assert_eq!(loaded.positions[0].spot_qty, dec!(0.00001));
        assert_eq!(loaded.positions[0].events_held, 0);
    }

    fn open_decision(notional: Decimal) -> FundingArbDecision {
        FundingArbDecision {
            action: Action::Open,
            symbol: "BTC/USDT".into(),
            reason: "entry".into(),
            current_funding_bps: 5.0,
            notional_usdt: notional,
        }
    }

    #[test]
    fn paper_open_accrue_close_and_skip() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let btc = signal("BTC/USDT", 5.0, dec!(200), dec!(200));
        let (st, ledger) = apply_paper_decisions(
            &s,
            &FundingArbState::default(),
            std::slice::from_ref(&btc),
            &[open_decision(dec!(2000))],
            1,
        );
        assert!(ledger.is_empty());
        assert_eq!(st.positions[0].spot_qty, dec!(10));
        assert_eq!(st.positions[0].perp_qty, dec!(-10));
        assert_eq!(st.positions[0].events_held, 0);

        let mut held = position("BTC/USDT", 0);
        held.spot_qty = dec!(10);
        held.perp_qty = dec!(-10);
        let hold = FundingArbDecision {
            action: Action::Hold,
            ..open_decision(Decimal::ZERO)
        };
        let (st, ledger) = apply_paper_decisions(
            &s,
            &state(vec![held.clone()]),
            std::slice::from_ref(&btc),
            &[hold],
            1,
        );
        assert!(ledger.is_empty());
        assert_eq!(st.positions[0].events_held, 1);
        assert!((st.positions[0].funding_received_bps - 5.0).abs() < 1e-12);
        assert_eq!(st.positions[0].funding_received_usdt, dec!(1));

        held.funding_received_bps = 80.0;
        held.funding_received_usdt = dec!(16);
        held.events_held = 16;
        let exit = signal("BTC/USDT", -1.5, dec!(200), dec!(200));
        let close = FundingArbDecision {
            action: Action::Close,
            current_funding_bps: -1.5,
            ..open_decision(Decimal::ZERO)
        };
        let (st, ledger) = apply_paper_decisions(&s, &state(vec![held]), &[exit], &[close], 2);
        assert!(st.positions.is_empty());
        let entry = &ledger[0];
        assert!((entry.funding_received_bps - 78.5).abs() < 1e-9);
        assert!((entry.round_trip_cost_bps - 50.0).abs() < 1e-12);
        assert!((entry.net_pnl_bps - 28.5).abs() < 1e-9);
        assert_eq!(
            entry.net_pnl_usdt,
            entry.funding_received_usdt - entry.round_trip_cost_usdt
        );

        let no_price = signal("BTC/USDT", 5.0, Decimal::ZERO, dec!(200));
        let (st, ledger) = apply_paper_decisions(
            &s,
            &FundingArbState::default(),
            &[no_price],
            &[open_decision(dec!(2000))],
            0,
        );
        assert!(st.positions.is_empty() && ledger.is_empty());
    }

    #[test]
    fn append_ledger_writes_jsonl_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let entry = PaperLedgerEntry {
            symbol: "BTC/USDT".into(),
            open_time_ms: 1,
            close_time_ms: 2,
            entry_funding_bps: 5.0,
            funding_received_bps: 80.0,
            funding_received_usdt: dec!(16),
            events_held: 16,
            round_trip_cost_bps: 50.0,
            round_trip_cost_usdt: dec!(10),
            net_pnl_bps: 30.0,
            net_pnl_usdt: dec!(6),
        };
        append_ledger(&path, std::slice::from_ref(&entry)).unwrap();
        append_ledger(&path, &[entry]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let decoded: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(decoded["symbol"], "BTC/USDT");
        assert_eq!(decoded["net_pnl_bps"], 30.0);
    }

    struct FixedMarket(Vec<CandidateSignal>);

    #[async_trait]
    impl MarketData for FixedMarket {
        async fn universe(&self, _settings: &FundingArbSettings) -> Result<Vec<String>> {
            Ok(self.0.iter().map(|s| s.symbol.clone()).collect())
        }
        async fn signals(&self, _symbols: &[String]) -> Result<Vec<CandidateSignal>> {
            Ok(self.0.clone())
        }
    }

    fn live(dir: &Path) -> FundingArbSettings {
        FundingArbSettings {
            proven_edge: true,
            max_concurrent_positions: 5,
            ..settings(dir)
        }
    }

    fn held_btc(funding_usdt: Decimal) -> FundingArbState {
        let mut p = position("BTC/USDT", 0);
        p.spot_qty = dec!(10);
        p.perp_qty = dec!(-10);
        p.funding_received_usdt = funding_usdt;
        state(vec![p])
    }

    fn rig(spot: FakeSpot, futures: FakeFutures) -> FundingArbExecutor {
        executor(
            &Arc::new(spot),
            &Arc::new(futures),
            &Arc::new(FakeCollateral::default()),
        )
    }

    #[tokio::test]
    async fn live_halts_on_reconciliation_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let s = live(dir.path());
        save_state(&held_btc(Decimal::ZERO), Path::new(&s.state_path)).unwrap();
        let exec = rig(
            FakeSpot::new(dec!(200)).with_balance(&[("BTC", Decimal::ZERO), ("USDT", dec!(1000))]),
            FakeFutures::new(dec!(200)),
        );
        let market = FixedMarket(vec![signal("BTC/USDT", 2.0, dec!(200), dec!(200))]);
        let err = run_cycle(&s, Mode::Live, Some(&exec), &market)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("reconciliation mismatch: ['spot BTC balance")
        );
        let audit = std::fs::read_to_string(&s.audit_path).unwrap();
        assert!(audit.contains("\"halt_reason\":\"reconciliation mismatch"));
    }

    #[tokio::test]
    async fn live_opens_position_when_signal_passes() {
        let dir = tempfile::tempdir().unwrap();
        let s = live(dir.path());
        let exec = rig(FakeSpot::new(dec!(200)), FakeFutures::new(dec!(200)));
        let market = FixedMarket(vec![signal("BTC/USDT", 5.0, dec!(200), dec!(200))]);
        let payload = run_cycle(&s, Mode::Live, Some(&exec), &market)
            .await
            .unwrap();
        assert!(
            payload
                .live_results
                .iter()
                .any(|r| r.action == Action::Open && r.ok)
        );
        let st = load_state(Path::new(&s.state_path)).unwrap();
        assert_eq!(st.positions.len(), 1);
        assert_eq!(st.positions[0].spot_qty, dec!(10));
    }

    #[tokio::test]
    async fn live_closes_existing_position_on_exit_signal() {
        let dir = tempfile::tempdir().unwrap();
        let s = live(dir.path());
        save_state(&held_btc(dec!(4)), Path::new(&s.state_path)).unwrap();
        let exec = rig(
            FakeSpot::new(dec!(200)).with_balance(&[("BTC", dec!(10.5)), ("USDT", dec!(1000))]),
            FakeFutures::new(dec!(200)).with_position("BTCUSDT", dec!(-10)),
        );
        let market = FixedMarket(vec![signal("BTC/USDT", -2.0, dec!(200), dec!(200))]);
        let payload = run_cycle(&s, Mode::Live, Some(&exec), &market)
            .await
            .unwrap();
        assert!(
            payload
                .live_results
                .iter()
                .any(|r| r.action == Action::Close && r.ok)
        );
        assert!(
            load_state(Path::new(&s.state_path))
                .unwrap()
                .positions
                .is_empty()
        );
        let ledger = std::fs::read_to_string(&s.ledger_path).unwrap();
        assert_eq!(ledger.lines().count(), 1);
        let row: serde_json::Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
        assert_eq!(row["symbol"], "BTC/USDT");
        assert_eq!(row["events_held"], 1);
    }

    #[tokio::test]
    async fn dynamic_balance_uses_live_wallet_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = FundingArbSettings {
            dynamic_balance: true,
            ..live(dir.path())
        };
        let mut futures = FakeFutures::new(dec!(200));
        futures.free = dec!(400);
        let exec = rig(
            FakeSpot::new(dec!(200)).with_balance(&[("BTC", Decimal::ZERO), ("USDT", dec!(600))]),
            futures,
        );
        let market = FixedMarket(vec![signal("BTC/USDT", 5.0, dec!(200), dec!(200))]);
        let payload = run_cycle(&s, Mode::Live, Some(&exec), &market)
            .await
            .unwrap();
        assert_eq!(
            load_state(Path::new(&s.state_path)).unwrap().positions[0].spot_qty,
            dec!(1)
        );
        assert_eq!(payload.balance_source, "live");
        assert_eq!(payload.effective_balance_usdt, dec!(1000));
        assert_eq!(payload.configured_balance_usdt, dec!(10000));

        let dir = tempfile::tempdir().unwrap();
        let s = FundingArbSettings {
            dynamic_balance: true,
            starting_balance_usdt: dec!(5000),
            ..live(dir.path())
        };
        let flaky = FakeSpot::new(dec!(200));
        flaky
            .balance_failures
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let exec = rig(flaky, FakeFutures::new(dec!(200)));
        let payload = run_cycle(&s, Mode::Live, Some(&exec), &market)
            .await
            .unwrap();
        assert_eq!(payload.balance_source, "fallback_configured");
        assert_eq!(payload.effective_balance_usdt, dec!(5000));
    }

    #[tokio::test]
    async fn low_balance_blocks_new_opens_but_allows_closes() {
        let dir = tempfile::tempdir().unwrap();
        let s = FundingArbSettings {
            starting_balance_usdt: dec!(9),
            ..live(dir.path())
        };
        save_state(&held_btc(Decimal::ZERO), Path::new(&s.state_path)).unwrap();
        let exec = rig(
            FakeSpot::new(dec!(200)).with_balance(&[("BTC", dec!(10.5)), ("USDT", dec!(9))]),
            FakeFutures::new(dec!(200)).with_position("BTCUSDT", dec!(-10)),
        );
        let market = FixedMarket(vec![
            signal("SOL/USDT", 8.0, dec!(100), dec!(100)),
            signal("BTC/USDT", -3.0, dec!(200), dec!(200)),
        ]);
        let payload = run_cycle(&s, Mode::Live, Some(&exec), &market)
            .await
            .unwrap();
        assert!(payload.decisions.iter().any(|d| d.action == Action::Close));
        assert!(!payload.decisions.iter().any(|d| d.action == Action::Open));
        assert!(payload.insufficient_balance);
        assert!(
            load_state(Path::new(&s.state_path))
                .unwrap()
                .positions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn live_close_failure_keeps_position_and_records_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = live(dir.path());
        save_state(&held_btc(Decimal::ZERO), Path::new(&s.state_path)).unwrap();
        let mut spot =
            FakeSpot::new(dec!(200)).with_balance(&[("BTC", dec!(10)), ("USDT", dec!(0))]);
        spot.fail_on_call = Some(1);
        let exec = rig(
            spot,
            FakeFutures::new(dec!(200)).with_position("BTCUSDT", dec!(-10)),
        );
        let market = FixedMarket(vec![signal("BTC/USDT", -3.0, dec!(200), dec!(200))]);
        let payload = run_cycle(&s, Mode::Live, Some(&exec), &market)
            .await
            .unwrap();
        assert!(
            payload
                .live_results
                .iter()
                .any(|r| r.action == Action::Close && !r.ok && r.error.is_some())
        );
        assert_eq!(
            load_state(Path::new(&s.state_path))
                .unwrap()
                .positions
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn plan_only_does_not_mutate_state_and_audits() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let market = FixedMarket(vec![sig("BTC/USDT", 5.0), sig("ETH/USDT", 1.0)]);
        let payload = run_cycle(&s, Mode::PlanOnly, None, &market).await.unwrap();
        assert_eq!(payload.decisions.len(), 1);
        assert_eq!(payload.balance_source, "configured");
        assert_eq!(payload.top_signals[0].symbol, "BTC/USDT");
        assert!(!Path::new(&s.state_path).exists());
        assert_eq!(
            std::fs::read_to_string(&s.audit_path)
                .unwrap()
                .lines()
                .count(),
            1
        );

        let paper = run_cycle(&s, Mode::Paper, None, &market).await.unwrap();
        assert_eq!(paper.open_positions, 1);
        assert_eq!(
            load_state(Path::new(&s.state_path))
                .unwrap()
                .positions
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn binance_market_data_builds_signals() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fapi/v1/premiumIndex"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"symbol": "BTCUSDT", "markPrice": "101", "lastFundingRate": "0.00030000"},
                {"symbol": "ETHUSDT", "markPrice": "10", "lastFundingRate": "0.0001"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/price"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"symbol": "BTCUSDT", "price": "100"}
            ])))
            .mount(&server)
            .await;
        let config = BinanceConfig::new(false)
            .with_spot_rest_url(server.uri())
            .with_futures_rest_url(server.uri());
        let market = BinanceMarketData::new(BinanceClient::new(config).unwrap());
        let signals = market
            .signals(&["BTC/USDT".into(), "ETH/USDT".into(), "XRP/USDT".into()])
            .await
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert!((signals[0].funding_bps - 3.0).abs() < 1e-12);
        assert!((signals[0].basis_bps() - 100.0).abs() < 1e-9);
    }
}

//! Long-horizon cross-sectional momentum runtime (port of `src/trend_filter.py`).
//!
//! Every `rebalance_every_days`, rank the universe by trailing `lookback_days` return,
//! hold the top K equal-weighted: sell holdings that dropped out, buy new entrants
//! (spot-only, long-only). Holdings worth less than the minimum order notional are
//! released (untracked, left in the wallet) because the exchange cannot sell them.
//! Live buys are capped by the USDT free after the sells, and any failed order keeps
//! the rebalance due so the next run retries.
//! Live submission requires `TREND_FILTER_ENABLED=true`,
//! `USE_TESTNET=false`, `DRY_RUN=false`, `CONFIRM_LIVE_TREND_FILTER=yes` and
//! `TREND_FILTER_PROVEN_EDGE=yes`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::ExitCode;

use async_trait::async_trait;
use bb_binance::fills::FillSummary;
use bb_binance::spot::NewOrder;
use bb_binance::{BinanceClient, BinanceConfig, BinanceError, Credentials, Side};
use futures::StreamExt as _;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};

use crate::common::{
    self, decimal_f64, decimal_number, env_csv, env_decimal, env_flag, env_or, env_parse, env_yes,
    utc_now_ms, utc_timestamp,
};
use crate::error::{Result, StrategyError};

const DAY_MS: i64 = 86_400_000;

fn tf_err(message: impl Into<String>) -> StrategyError {
    StrategyError::TrendFilter(message.into())
}

const DEFAULT_UNIVERSE: [&str; 11] = [
    "BTC/USDT",
    "ETH/USDT",
    "SOL/USDT",
    "BNB/USDT",
    "XRP/USDT",
    "ADA/USDT",
    "DOGE/USDT",
    "AVAX/USDT",
    "LINK/USDT",
    "DOT/USDT",
    "LTC/USDT",
];

#[derive(Clone, PartialEq)]
pub struct TrendFilterSettings {
    pub api_key: String,
    pub api_secret: String,
    pub use_testnet: bool,
    pub dry_run: bool,
    pub enabled: bool,
    pub confirm_live_trend_filter: String,
    /// `TREND_FILTER_PROVEN_EDGE=yes` (Python read it inside the preflight).
    pub proven_edge: bool,
    pub universe: Vec<String>,
    /// Recorded for parity; signals always use daily klines (as in Python).
    pub interval: String,
    pub lookback_days: i64,
    pub top_k: i64,
    pub rebalance_every_days: i64,
    pub min_lookback_return_pct: f64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    pub min_balance_usdt: Decimal,
    pub min_order_notional_usdt: Decimal,
    pub dynamic_balance: bool,
    pub state_path: String,
    pub audit_path: String,
    pub ledger_path: String,
    pub emergency_stop_path: String,
}

impl std::fmt::Debug for TrendFilterSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrendFilterSettings")
            .field("enabled", &self.enabled)
            .field("universe", &self.universe)
            .field("top_k", &self.top_k)
            .field("state_path", &self.state_path)
            .finish_non_exhaustive()
    }
}

impl Default for TrendFilterSettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_secret: String::new(),
            use_testnet: false,
            dry_run: true,
            enabled: false,
            confirm_live_trend_filter: "no".into(),
            proven_edge: false,
            universe: DEFAULT_UNIVERSE.iter().map(|s| (*s).to_owned()).collect(),
            interval: "1d".into(),
            lookback_days: 90,
            top_k: 1,
            rebalance_every_days: 30,
            min_lookback_return_pct: 0.0,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            min_balance_usdt: Decimal::from(5),
            min_order_notional_usdt: Decimal::from(5),
            dynamic_balance: true,
            state_path: "data/trend-filter-state.json".into(),
            audit_path: "data/trend-filter-decisions.jsonl".into(),
            ledger_path: "data/trend-filter-ledger.jsonl".into(),
            emergency_stop_path: "data/trend-filter.stop".into(),
        }
    }
}

fn env_f64(name: &str, default: f64) -> Result<f64> {
    env_parse(name, default).map_err(|_| tf_err(format!("{name} must be numeric")))
}

fn env_dec(name: &str, default: Decimal) -> Result<Decimal> {
    env_decimal(name, default).map_err(|_| tf_err(format!("{name} must be numeric")))
}

fn env_int(name: &str, default: i64) -> Result<i64> {
    env_parse(name, default).map_err(|_| tf_err(format!("{name} must be an integer")))
}

impl TrendFilterSettings {
    pub fn validate(&self) -> Result<()> {
        if self.universe.is_empty() {
            return Err(tf_err("universe must not be empty"));
        }
        if self.top_k <= 0 {
            return Err(tf_err("top_k must be positive"));
        }
        if self.top_k > self.universe.len() as i64 {
            return Err(tf_err("top_k cannot exceed universe size"));
        }
        if self.lookback_days <= 0 {
            return Err(tf_err("lookback_days must be positive"));
        }
        if self.rebalance_every_days <= 0 {
            return Err(tf_err("rebalance_every_days must be positive"));
        }
        if self.min_balance_usdt < Decimal::ZERO {
            return Err(tf_err("min_balance_usdt must be non-negative"));
        }
        if self.min_order_notional_usdt <= Decimal::ZERO {
            return Err(tf_err("min_order_notional_usdt must be positive"));
        }
        for (name, value) in [
            ("state_path", &self.state_path),
            ("audit_path", &self.audit_path),
            ("ledger_path", &self.ledger_path),
            ("emergency_stop_path", &self.emergency_stop_path),
        ] {
            if value.trim().is_empty() {
                return Err(tf_err(format!("{name} must not be blank")));
            }
        }
        Ok(())
    }

    pub fn from_env() -> Result<Self> {
        let api_key = env_or("BINANCE_API_KEY", "");
        let api_secret = env_or("BINANCE_API_SECRET", "");
        if api_key.is_empty() || api_secret.is_empty() {
            return Err(tf_err("BINANCE_API_KEY and BINANCE_API_SECRET must be set"));
        }
        let d = Self::default();
        let universe = env_csv("TREND_FILTER_UNIVERSE");
        let raw_universe = env_or("TREND_FILTER_UNIVERSE", "");
        let settings = Self {
            api_key,
            api_secret,
            use_testnet: env_flag("USE_TESTNET", false),
            dry_run: env_flag("DRY_RUN", true),
            enabled: env_flag("TREND_FILTER_ENABLED", false),
            confirm_live_trend_filter: env_or("CONFIRM_LIVE_TREND_FILTER", "no"),
            proven_edge: env_yes("TREND_FILTER_PROVEN_EDGE"),
            universe: if raw_universe.is_empty() {
                d.universe.clone()
            } else {
                universe
            },
            interval: env_or("TREND_FILTER_INTERVAL", &d.interval),
            lookback_days: env_int("TREND_FILTER_LOOKBACK_DAYS", d.lookback_days)?,
            top_k: env_int("TREND_FILTER_TOP_K", d.top_k)?,
            rebalance_every_days: env_int(
                "TREND_FILTER_REBALANCE_EVERY_DAYS",
                d.rebalance_every_days,
            )?,
            min_lookback_return_pct: env_f64(
                "TREND_FILTER_MIN_LOOKBACK_RETURN_PCT",
                d.min_lookback_return_pct,
            )?,
            taker_fee_bps: env_f64("TREND_FILTER_TAKER_FEE_BPS", d.taker_fee_bps)?,
            slippage_bps: env_f64("TREND_FILTER_SLIPPAGE_BPS", d.slippage_bps)?,
            min_balance_usdt: env_dec("TREND_FILTER_MIN_BALANCE_USDT", d.min_balance_usdt)?,
            min_order_notional_usdt: env_dec(
                "TREND_FILTER_MIN_ORDER_NOTIONAL_USDT",
                d.min_order_notional_usdt,
            )?,
            dynamic_balance: env_flag("TREND_FILTER_DYNAMIC_BALANCE", d.dynamic_balance),
            state_path: env_or("TREND_FILTER_STATE_PATH", &d.state_path),
            audit_path: env_or("TREND_FILTER_AUDIT_PATH", &d.audit_path),
            ledger_path: env_or("TREND_FILTER_LEDGER_PATH", &d.ledger_path),
            emergency_stop_path: env_or("TREND_FILTER_EMERGENCY_STOP_PATH", &d.emergency_stop_path),
        };
        settings.validate()?;
        Ok(settings)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendFilterHolding {
    pub symbol: String,
    #[serde(with = "decimal_number")]
    pub qty: Decimal,
    #[serde(with = "decimal_number")]
    pub entry_price: Decimal,
    pub entry_time_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TrendFilterState {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub holdings: Vec<TrendFilterHolding>,
    #[serde(default)]
    pub last_rebalance_time_ms: i64,
}

fn null_as_empty<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<TrendFilterHolding>, D::Error> {
    Ok(Option::<Vec<TrendFilterHolding>>::deserialize(deserializer)?.unwrap_or_default())
}

pub fn load_state(path: &Path) -> Result<TrendFilterState> {
    if !path.exists() {
        return Ok(TrendFilterState::default());
    }
    std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
        .map_err(|e| tf_err(format!("could not read state at {}: {e}", path.display())))
}

pub fn save_state(state: &TrendFilterState, path: &Path) -> Result<()> {
    common::save_json_sorted(path, state)
}

pub fn preflight(settings: &TrendFilterSettings, submit_order: bool) -> Result<()> {
    if Path::new(&settings.emergency_stop_path).exists() {
        return Err(tf_err(format!(
            "emergency stop file present at {}; refusing to run",
            settings.emergency_stop_path
        )));
    }
    if !submit_order {
        return Ok(());
    }
    if !settings.enabled {
        return Err(tf_err(
            "TREND_FILTER_ENABLED=true is required for live submission",
        ));
    }
    if settings.use_testnet {
        return Err(tf_err(
            "trend-filter live submission requires USE_TESTNET=false",
        ));
    }
    if settings.dry_run {
        return Err(tf_err(
            "trend-filter live submission requires DRY_RUN=false",
        ));
    }
    if settings.confirm_live_trend_filter != "yes" {
        return Err(tf_err(
            "trend-filter live submission requires CONFIRM_LIVE_TREND_FILTER=yes",
        ));
    }
    if !settings.proven_edge {
        return Err(tf_err(
            "trend-filter live submission requires TREND_FILTER_PROVEN_EDGE=yes. Run `bb trend-filter-backtest` \
             and set this only when the harness shows positive risk-adjusted edge on your configured universe.",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrendFilterSignal {
    pub symbol: String,
    pub current_price: Decimal,
    pub lookback_return_pct: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Buy,
    Sell,
    Hold,
    /// Stop tracking a holding worth less than the exchange minimum order; it
    /// cannot be sold, so the coins stay in the wallet as dust.
    ReleaseDust,
    SkipRebalance,
    SkipCycle,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrendFilterDecision {
    pub action: Action,
    pub symbol: String,
    pub reason: String,
    #[serde(with = "decimal_number")]
    pub target_notional_usdt: Decimal,
    #[serde(with = "decimal_number")]
    pub qty: Decimal,
}

impl TrendFilterDecision {
    fn new(action: Action, symbol: &str, reason: String) -> Self {
        Self {
            action,
            symbol: symbol.to_owned(),
            reason,
            target_notional_usdt: Decimal::ZERO,
            qty: Decimal::ZERO,
        }
    }
}

/// Python `str(float)` for a whole/fractional money value (`5.0`, `5.5`).
fn py_money(value: Decimal) -> String {
    let v = value.normalize();
    if v.scale() == 0 {
        format!("{v}.0")
    } else {
        v.to_string()
    }
}

/// Daily closes for the lookback window, behind a trait for offline tests.
#[async_trait]
pub trait KlineSource: Send + Sync {
    /// `Ok(None)` when the exchange answered with an error status or too few rows.
    async fn daily_closes(&self, symbol: &str, limit: u32) -> Result<Option<Vec<Decimal>>>;
}

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
    async fn daily_closes(&self, symbol: &str, limit: u32) -> Result<Option<Vec<Decimal>>> {
        let rest = symbol.replace('/', "").to_uppercase();
        match self
            .client
            .klines(&rest, "1d", None, None, Some(limit))
            .await
        {
            Ok(rows) if rows.len() >= 2 => Ok(Some(rows.iter().map(|k| k.close).collect())),
            Ok(_) => Ok(None),
            // Python: a non-200 response skips the symbol; transport failures abort the cycle.
            Err(err @ BinanceError::Network { .. }) => Err(err.into()),
            Err(err) => {
                tracing::warn!(%symbol, error = %err, "trend-filter kline fetch failed; skipping symbol");
                Ok(None)
            }
        }
    }
}

/// Trailing-return signal per universe symbol (6 concurrent fetches, universe order kept).
pub async fn fetch_signals(
    settings: &TrendFilterSettings,
    source: &dyn KlineSource,
) -> Result<Vec<TrendFilterSignal>> {
    let lookback = usize::try_from(settings.lookback_days).unwrap_or(usize::MAX);
    let limit = u32::try_from(settings.lookback_days + 2).unwrap_or(u32::MAX);
    let results: Vec<Result<Option<TrendFilterSignal>>> =
        futures::stream::iter(settings.universe.iter())
            .map(|symbol| async move {
                let Some(closes) = source.daily_closes(symbol, limit).await? else {
                    return Ok(None);
                };
                if closes.len() < lookback.saturating_add(1) {
                    return Ok(None);
                }
                let current = closes[closes.len() - 1];
                let past = closes[closes.len() - 1 - lookback];
                if past <= Decimal::ZERO {
                    return Ok(None);
                }
                Ok(Some(TrendFilterSignal {
                    symbol: symbol.clone(),
                    current_price: current,
                    lookback_return_pct: (decimal_f64(current) / decimal_f64(past) - 1.0) * 100.0,
                }))
            })
            .buffered(6)
            .collect()
            .await;
    results.into_iter().filter_map(Result::transpose).collect()
}

pub struct TrendFilterAdvisor<'a> {
    pub settings: &'a TrendFilterSettings,
}

impl TrendFilterAdvisor<'_> {
    pub fn decide(
        &self,
        state: &TrendFilterState,
        signals: &[TrendFilterSignal],
        balance_usdt: Decimal,
        current_time_ms: i64,
    ) -> Vec<TrendFilterDecision> {
        let cfg = self.settings;
        let next_due = if state.last_rebalance_time_ms > 0 {
            state.last_rebalance_time_ms + cfg.rebalance_every_days * DAY_MS
        } else {
            0
        };
        if current_time_ms < next_due {
            return vec![TrendFilterDecision::new(
                Action::SkipCycle,
                "",
                format!("next rebalance due at {next_due} (current {current_time_ms})"),
            )];
        }

        let mut eligible: Vec<&TrendFilterSignal> = signals
            .iter()
            .filter(|s| s.lookback_return_pct >= cfg.min_lookback_return_pct)
            .collect();
        eligible.sort_by(|a, b| b.lookback_return_pct.total_cmp(&a.lookback_return_pct));
        eligible.truncate(usize::try_from(cfg.top_k).unwrap_or(0));
        let targets: HashSet<&str> = eligible.iter().map(|s| s.symbol.as_str()).collect();

        let by_symbol: HashMap<&str, &TrendFilterSignal> =
            signals.iter().map(|s| (s.symbol.as_str(), s)).collect();
        let value_of = |h: &TrendFilterHolding| {
            by_symbol
                .get(h.symbol.as_str())
                .map(|s| h.qty * s.current_price)
        };
        // Binance rejects orders below the minimum notional, so a holding worth
        // less can never be sold and would block every rotation.
        let is_dust = |h: &TrendFilterHolding| {
            value_of(h).is_some_and(|value| value < cfg.min_order_notional_usdt)
        };
        let held_value: Decimal = state
            .holdings
            .iter()
            .filter(|h| !is_dust(h))
            .filter_map(value_of)
            .sum();
        let total_equity = balance_usdt + held_value;
        let per_symbol = if eligible.is_empty() {
            Decimal::ZERO
        } else {
            total_equity / Decimal::from(eligible.len())
        };

        let mut decisions = Vec::new();
        for h in state.holdings.iter().filter(|h| is_dust(h)) {
            decisions.push(TrendFilterDecision {
                qty: h.qty,
                ..TrendFilterDecision::new(
                    Action::ReleaseDust,
                    &h.symbol,
                    format!(
                        "holding {} worth ${:.4} is below min order notional ${}; released as wallet dust",
                        h.symbol,
                        value_of(h).unwrap_or_default(),
                        py_money(cfg.min_order_notional_usdt)
                    ),
                )
            });
        }
        let held: HashSet<&str> = state
            .holdings
            .iter()
            .filter(|h| !is_dust(h))
            .map(|h| h.symbol.as_str())
            .collect();
        for h in state
            .holdings
            .iter()
            .filter(|h| !is_dust(h) && !targets.contains(h.symbol.as_str()))
        {
            decisions.push(TrendFilterDecision {
                qty: h.qty,
                ..TrendFilterDecision::new(
                    Action::Sell,
                    &h.symbol,
                    format!("holding {} no longer in top {}", h.symbol, cfg.top_k),
                )
            });
        }
        for sig in &eligible {
            if held.contains(sig.symbol.as_str()) {
                decisions.push(TrendFilterDecision::new(
                    Action::Hold,
                    &sig.symbol,
                    format!(
                        "{} remains in top {} (lookback return {:+.2}%)",
                        sig.symbol, cfg.top_k, sig.lookback_return_pct
                    ),
                ));
            } else if per_symbol < cfg.min_order_notional_usdt {
                decisions.push(TrendFilterDecision {
                    target_notional_usdt: per_symbol,
                    ..TrendFilterDecision::new(
                        Action::SkipRebalance,
                        &sig.symbol,
                        format!(
                            "target notional ${per_symbol:.4} below min order notional ${}",
                            py_money(cfg.min_order_notional_usdt)
                        ),
                    )
                });
            } else {
                decisions.push(TrendFilterDecision {
                    target_notional_usdt: per_symbol,
                    ..TrendFilterDecision::new(
                        Action::Buy,
                        &sig.symbol,
                        format!(
                            "new top-{} entry: lookback return {:+.2}%, target ${per_symbol:.4}",
                            cfg.top_k, sig.lookback_return_pct
                        ),
                    )
                });
            }
        }
        if decisions.is_empty() {
            decisions.push(TrendFilterDecision::new(
                Action::Hold,
                "",
                "no eligible signals; staying in cash".into(),
            ));
        }
        decisions
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrendFilterLedgerEntry {
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
    pub fee_quote: Decimal,
    pub order_id: String,
    pub success: bool,
    pub error: String,
}

impl TrendFilterLedgerEntry {
    fn failed(timestamp_ms: i64, action: Action, symbol: &str, error: String) -> Self {
        Self {
            timestamp_ms,
            action,
            symbol: symbol.to_owned(),
            qty: Decimal::ZERO,
            price: Decimal::ZERO,
            notional_usdt: Decimal::ZERO,
            fee_quote: Decimal::ZERO,
            order_id: String::new(),
            success: false,
            error,
        }
    }
}

/// Market order outcome.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpotFill {
    /// Base filled (BUYs: net of base-asset commission).
    pub filled: Decimal,
    /// Average price; zero when unknown.
    pub average: Decimal,
    /// Commission when paid in a single asset (ccxt `fee.cost`), else zero.
    pub fee: Decimal,
    pub order_id: String,
}

/// Signed spot trading surface used by live mode.
#[async_trait]
pub trait SpotTrader: Send + Sync {
    async fn free_usdt(&self) -> Result<Decimal>;
    async fn market_sell(&self, symbol: &str, qty: Decimal) -> Result<SpotFill>;
    /// MARKET BUY spending `quote_usdt` (`quoteOrderQty`).
    async fn market_buy_quote(&self, symbol: &str, quote_usdt: Decimal) -> Result<SpotFill>;
}

pub struct BinanceSpotTrader {
    client: BinanceClient,
}

impl BinanceSpotTrader {
    pub fn new(client: BinanceClient) -> Self {
        Self { client }
    }

    fn fill(order: &bb_binance::spot::Order, filled: Decimal) -> SpotFill {
        let summary = FillSummary::from_order(order);
        let fee = match summary.commissions.len() {
            1 => summary.commissions.values().copied().sum(),
            _ => Decimal::ZERO,
        };
        SpotFill {
            filled,
            average: summary.avg_price.unwrap_or_default(),
            fee,
            order_id: order.order_id.to_string(),
        }
    }
}

#[async_trait]
impl SpotTrader for BinanceSpotTrader {
    async fn free_usdt(&self) -> Result<Decimal> {
        Ok(self.client.account().await?.balance("USDT").free)
    }

    async fn market_sell(&self, symbol: &str, qty: Decimal) -> Result<SpotFill> {
        let rules = self.client.symbol_rules(symbol).await?;
        let qty = rules.round_market_qty(qty);
        if qty <= Decimal::ZERO {
            return Err(tf_err(format!("{symbol} sell quantity rounds to zero")));
        }
        let order = self
            .client
            .create_order(&NewOrder::market(symbol, Side::Sell, qty))
            .await?;
        Ok(Self::fill(&order, order.executed_qty))
    }

    async fn market_buy_quote(&self, symbol: &str, quote_usdt: Decimal) -> Result<SpotFill> {
        let rules = self.client.symbol_rules(symbol).await?;
        let quote = rules.round_quote_qty(quote_usdt);
        let order = self
            .client
            .create_order(&NewOrder::market_quote(symbol, Side::Buy, quote))
            .await?;
        // Commission charged in the base asset is not sellable later.
        let net = FillSummary::from_order(&order).net_base_received(&rules.base_asset);
        Ok(Self::fill(&order, net))
    }
}

/// Python `f"{type(exc).__name__}: {str(exc)[:n]}"`.
fn error_label(err: &StrategyError, max_chars: usize) -> String {
    let kind = match err {
        StrategyError::Binance(_) => "BinanceError",
        StrategyError::TrendFilter(_) => "TrendFilterError",
        StrategyError::Http(_) => "HttpError",
        _ => "StrategyError",
    };
    let message: String = err.to_string().chars().take(max_chars).collect();
    format!("{kind}: {message}")
}

/// Insertion-ordered holdings (Python dict semantics).
#[derive(Default)]
struct Holdings(Vec<TrendFilterHolding>);

impl Holdings {
    fn get(&self, symbol: &str) -> Option<&TrendFilterHolding> {
        self.0.iter().find(|h| h.symbol == symbol)
    }

    fn insert(&mut self, holding: TrendFilterHolding) {
        match self.0.iter_mut().find(|h| h.symbol == holding.symbol) {
            Some(slot) => *slot = holding,
            None => self.0.push(holding),
        }
    }

    fn remove(&mut self, symbol: &str) {
        self.0.retain(|h| h.symbol != symbol);
    }
}

/// Price for a sold symbol: its signal, else the holding's entry price.
fn reference_price(
    symbol: &str,
    by_symbol: &HashMap<&str, &TrendFilterSignal>,
    holdings: &Holdings,
) -> Decimal {
    by_symbol
        .get(symbol)
        .map(|s| s.current_price)
        .or_else(|| holdings.get(symbol).map(|h| h.entry_price))
        .unwrap_or_default()
}

/// Untrack a dust holding; no order is sent, the coins stay in the wallet.
fn release_dust(
    d: &TrendFilterDecision,
    by_symbol: &HashMap<&str, &TrendFilterSignal>,
    now_ms: i64,
    holdings: &mut Holdings,
) -> TrendFilterLedgerEntry {
    let price = reference_price(&d.symbol, by_symbol, holdings);
    holdings.remove(&d.symbol);
    TrendFilterLedgerEntry {
        timestamp_ms: now_ms,
        action: Action::ReleaseDust,
        symbol: d.symbol.clone(),
        qty: d.qty,
        price,
        notional_usdt: d.qty * price,
        fee_quote: Decimal::ZERO,
        order_id: String::new(),
        success: true,
        error: String::new(),
    }
}

/// Releases dust, sells (free cash), then buys sized to the USDT actually free
/// after the sells; every attempt is ledgered.
async fn execute_decisions(
    trader: &dyn SpotTrader,
    decisions: &[TrendFilterDecision],
    by_symbol: &HashMap<&str, &TrendFilterSignal>,
    min_order_notional_usdt: Decimal,
    now_ms: i64,
    holdings: &mut Holdings,
) -> Vec<TrendFilterLedgerEntry> {
    let mut ledger: Vec<TrendFilterLedgerEntry> = decisions
        .iter()
        .filter(|d| d.action == Action::ReleaseDust)
        .map(|d| release_dust(d, by_symbol, now_ms, holdings))
        .collect();
    for d in decisions.iter().filter(|d| d.action == Action::Sell) {
        match trader.market_sell(&d.symbol, d.qty).await {
            Ok(fill) => {
                let avg = if fill.average.is_zero() {
                    reference_price(&d.symbol, by_symbol, holdings)
                } else {
                    fill.average
                };
                let notional = fill.filled * avg;
                ledger.push(TrendFilterLedgerEntry {
                    timestamp_ms: now_ms,
                    action: Action::Sell,
                    symbol: d.symbol.clone(),
                    qty: fill.filled,
                    price: avg,
                    notional_usdt: notional,
                    fee_quote: fill.fee,
                    order_id: fill.order_id,
                    success: fill.filled > Decimal::ZERO,
                    error: String::new(),
                });
                if fill.filled > Decimal::ZERO {
                    holdings.remove(&d.symbol);
                }
                common::alert(&format!(
                    "trend filter SELL {}: {:.6} @ ${avg:.4} = ${notional:.4}",
                    d.symbol, fill.filled
                ))
                .await;
            }
            Err(err) => {
                ledger.push(TrendFilterLedgerEntry::failed(
                    now_ms,
                    Action::Sell,
                    &d.symbol,
                    error_label(&err, 200),
                ));
                common::alert(&format!(
                    "trend filter SELL FAILED {}: {}",
                    d.symbol,
                    error_label(&err, 150)
                ))
                .await;
            }
        }
    }
    let buys: Vec<&TrendFilterDecision> = decisions
        .iter()
        .filter(|d| d.action == Action::Buy)
        .collect();
    if buys.is_empty() {
        return ledger;
    }
    // Targets assume every sell filled at the signal price; the wallet is the
    // truth (fees, slippage, failed sells, cash used by other bots).
    let mut budget = match trader.free_usdt().await {
        Ok(free) => free,
        Err(err) => {
            let error = error_label(&err, 200);
            for d in &buys {
                ledger.push(TrendFilterLedgerEntry::failed(
                    now_ms,
                    Action::Buy,
                    &d.symbol,
                    error.clone(),
                ));
            }
            common::alert(&format!(
                "trend filter BUY FAILED: free USDT unavailable: {}",
                error_label(&err, 150)
            ))
            .await;
            return ledger;
        }
    };
    for (index, d) in buys.iter().enumerate() {
        let Some(sig) = by_symbol
            .get(d.symbol.as_str())
            .filter(|s| s.current_price > Decimal::ZERO)
        else {
            continue;
        };
        let spend = d
            .target_notional_usdt
            .min(budget / Decimal::from(buys.len() - index));
        if spend < min_order_notional_usdt {
            let error = format!(
                "TrendFilterError: free USDT ${budget:.4} leaves ${spend:.4} for {}, below min order notional ${}",
                d.symbol,
                py_money(min_order_notional_usdt)
            );
            common::alert(&format!("trend filter BUY SKIPPED {}: {error}", d.symbol)).await;
            ledger.push(TrendFilterLedgerEntry::failed(
                now_ms,
                Action::Buy,
                &d.symbol,
                error,
            ));
            continue;
        }
        match trader.market_buy_quote(&d.symbol, spend).await {
            Ok(fill) => {
                budget -= spend;
                let avg = if fill.average.is_zero() {
                    sig.current_price
                } else {
                    fill.average
                };
                let notional = fill.filled * avg;
                ledger.push(TrendFilterLedgerEntry {
                    timestamp_ms: now_ms,
                    action: Action::Buy,
                    symbol: d.symbol.clone(),
                    qty: fill.filled,
                    price: avg,
                    notional_usdt: notional,
                    fee_quote: fill.fee,
                    order_id: fill.order_id,
                    success: fill.filled > Decimal::ZERO,
                    error: String::new(),
                });
                if fill.filled > Decimal::ZERO {
                    holdings.insert(TrendFilterHolding {
                        symbol: d.symbol.clone(),
                        qty: fill.filled,
                        entry_price: avg,
                        entry_time_ms: now_ms,
                    });
                }
                common::alert(&format!(
                    "trend filter BUY {}: {:.6} @ ${avg:.4} = ${notional:.4}",
                    d.symbol, fill.filled
                ))
                .await;
            }
            Err(err) => {
                ledger.push(TrendFilterLedgerEntry::failed(
                    now_ms,
                    Action::Buy,
                    &d.symbol,
                    error_label(&err, 200),
                ));
                common::alert(&format!(
                    "trend filter BUY FAILED {}: {}",
                    d.symbol,
                    error_label(&err, 150)
                ))
                .await;
            }
        }
    }
    ledger
}

/// Simulated fills at the signal price.
fn apply_paper(
    settings: &TrendFilterSettings,
    decisions: &[TrendFilterDecision],
    by_symbol: &HashMap<&str, &TrendFilterSignal>,
    now_ms: i64,
    holdings: &mut Holdings,
) -> Vec<TrendFilterLedgerEntry> {
    let fee_of = |notional: Decimal| {
        notional * bb_core::money::from_f64(settings.taker_fee_bps).unwrap_or_default()
            / Decimal::from(10_000)
    };
    let mut ledger = Vec::new();
    for d in decisions {
        match d.action {
            Action::ReleaseDust => ledger.push(release_dust(d, by_symbol, now_ms, holdings)),
            Action::Sell => {
                let price = reference_price(&d.symbol, by_symbol, holdings);
                holdings.remove(&d.symbol);
                let notional = d.qty * price;
                ledger.push(TrendFilterLedgerEntry {
                    timestamp_ms: now_ms,
                    action: Action::Sell,
                    symbol: d.symbol.clone(),
                    qty: d.qty,
                    price,
                    notional_usdt: notional,
                    fee_quote: fee_of(notional),
                    order_id: "paper".into(),
                    success: true,
                    error: String::new(),
                });
            }
            Action::Buy => {
                let Some(sig) = by_symbol
                    .get(d.symbol.as_str())
                    .filter(|s| s.current_price > Decimal::ZERO)
                else {
                    continue;
                };
                let qty = d.target_notional_usdt / sig.current_price;
                holdings.insert(TrendFilterHolding {
                    symbol: d.symbol.clone(),
                    qty,
                    entry_price: sig.current_price,
                    entry_time_ms: now_ms,
                });
                ledger.push(TrendFilterLedgerEntry {
                    timestamp_ms: now_ms,
                    action: Action::Buy,
                    symbol: d.symbol.clone(),
                    qty,
                    price: sig.current_price,
                    notional_usdt: d.target_notional_usdt,
                    fee_quote: fee_of(d.target_notional_usdt),
                    order_id: "paper".into(),
                    success: true,
                    error: String::new(),
                });
            }
            _ => {}
        }
    }
    ledger
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    PlanOnly,
    Paper,
    Live,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopSignal {
    pub symbol: String,
    pub lookback_return_pct: f64,
    #[serde(with = "decimal_number")]
    pub current_price: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CyclePayload {
    pub timestamp: String,
    pub mode: Mode,
    pub universe_size: usize,
    pub signals_count: usize,
    pub balance_source: &'static str,
    #[serde(with = "decimal_number")]
    pub balance_usdt: Decimal,
    pub insufficient_balance: bool,
    pub open_holdings: Vec<TrendFilterHolding>,
    pub decisions: Vec<TrendFilterDecision>,
    pub ledger_entries: Vec<TrendFilterLedgerEntry>,
    pub top_signals: Vec<TopSignal>,
}

/// Run one cycle: plan-only (no mutation), paper (simulated fills) or live (`trader`).
pub async fn run_cycle(
    settings: &TrendFilterSettings,
    mode: Mode,
    trader: Option<&dyn SpotTrader>,
    source: &dyn KlineSource,
) -> Result<CyclePayload> {
    preflight(settings, mode == Mode::Live)?;
    let mut state = load_state(Path::new(&settings.state_path))?;
    let signals = fetch_signals(settings, source).await?;
    let by_symbol: HashMap<&str, &TrendFilterSignal> =
        signals.iter().map(|s| (s.symbol.as_str(), s)).collect();

    let (balance_usdt, balance_source) = match (mode, trader) {
        (Mode::Live, Some(trader)) if settings.dynamic_balance => match trader.free_usdt().await {
            Ok(balance) => (balance, "live"),
            Err(err) => {
                // Fail closed: a zero balance blocks trading this cycle.
                tracing::warn!(error = %err, "trend-filter balance fetch failed");
                (Decimal::ZERO, "live")
            }
        },
        (Mode::Live, _) => (Decimal::ZERO, "missing"),
        // Placeholder for plan/paper sizing.
        _ => (settings.min_balance_usdt, "synthetic"),
    };
    let insufficient_balance = mode == Mode::Live && balance_usdt < settings.min_balance_usdt;

    let now_ms = utc_now_ms();
    let decisions = TrendFilterAdvisor { settings }.decide(&state, &signals, balance_usdt, now_ms);
    let rebalanced = decisions
        .iter()
        .any(|d| matches!(d.action, Action::Buy | Action::Sell | Action::ReleaseDust));

    let mut ledger = Vec::new();
    let mut holdings = Holdings(state.holdings.clone());
    let executed = match (mode, trader) {
        (Mode::Live, Some(trader)) if rebalanced && !insufficient_balance => {
            ledger = execute_decisions(
                trader,
                &decisions,
                &by_symbol,
                settings.min_order_notional_usdt,
                now_ms,
                &mut holdings,
            )
            .await;
            true
        }
        (Mode::Paper, _) if rebalanced && !insufficient_balance => {
            ledger = apply_paper(settings, &decisions, &by_symbol, now_ms, &mut holdings);
            true
        }
        _ => false,
    };
    if executed {
        // A failed or skipped order keeps the old timestamp so the next daily
        // run retries instead of waiting a full rebalance period.
        let all_succeeded = ledger.iter().all(|entry| entry.success);
        state = TrendFilterState {
            holdings: holdings.0,
            last_rebalance_time_ms: if all_succeeded {
                now_ms
            } else {
                state.last_rebalance_time_ms
            },
        };
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
    top_signals.sort_by(|a, b| b.lookback_return_pct.total_cmp(&a.lookback_return_pct));
    top_signals.truncate(10);

    let payload = CyclePayload {
        timestamp: utc_timestamp(),
        mode,
        universe_size: settings.universe.len(),
        signals_count: signals.len(),
        balance_source,
        balance_usdt,
        insufficient_balance,
        open_holdings: state.holdings.clone(),
        decisions,
        ledger_entries: ledger,
        top_signals,
    };
    common::append_jsonl(Path::new(&settings.audit_path), &payload)?;
    Ok(payload)
}

#[derive(clap::Args, Debug, Clone, Default)]
pub struct Args {
    /// Compute decisions without mutating state (default).
    #[arg(long, conflicts_with_all = ["paper", "submit_live"])]
    pub plan_only: bool,
    /// Simulate fills at the current price and persist state.
    #[arg(long, conflicts_with_all = ["plan_only", "submit_live"])]
    pub paper: bool,
    /// Submit real spot orders (gated).
    #[arg(long, conflicts_with_all = ["plan_only", "paper"])]
    pub submit_live: bool,
}

async fn run_mode(settings: &TrendFilterSettings, args: &Args) -> Result<CyclePayload> {
    let source = BinanceKlines::new(common::public_client()?);
    if args.submit_live {
        preflight(settings, true)?;
        let credentials = Credentials::new(settings.api_key.clone(), &settings.api_secret)?;
        let client = BinanceClient::new(BinanceConfig::new(false).with_credentials(credentials))?;
        let trader = BinanceSpotTrader::new(client);
        run_cycle(settings, Mode::Live, Some(&trader), &source).await
    } else {
        let mode = if args.paper {
            Mode::Paper
        } else {
            Mode::PlanOnly
        };
        run_cycle(settings, mode, None, &source).await
    }
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let outcome = match TrendFilterSettings::from_env() {
        Ok(settings) => run_mode(&settings, &args).await,
        Err(err) => Err(err),
    };
    match outcome {
        Ok(payload) => {
            common::print_json(&payload)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            println!("trend-filter failed: {err}");
            Ok(ExitCode::from(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use rust_decimal_macros::dec;

    use super::*;

    fn settings(dir: &Path) -> TrendFilterSettings {
        let s = TrendFilterSettings {
            api_key: "k".into(),
            api_secret: "s".into(),
            dry_run: false,
            enabled: true,
            confirm_live_trend_filter: "yes".into(),
            universe: vec!["A/USDT".into(), "B/USDT".into(), "C/USDT".into()],
            lookback_days: 30,
            top_k: 1,
            rebalance_every_days: 30,
            state_path: dir.join("state.json").display().to_string(),
            audit_path: dir.join("audit.jsonl").display().to_string(),
            ledger_path: dir.join("ledger.jsonl").display().to_string(),
            emergency_stop_path: dir.join("stop").display().to_string(),
            ..TrendFilterSettings::default()
        };
        s.validate().unwrap();
        s
    }

    fn sig(symbol: &str, ret: f64) -> TrendFilterSignal {
        TrendFilterSignal {
            symbol: symbol.into(),
            current_price: dec!(100),
            lookback_return_pct: ret,
        }
    }

    fn holding(symbol: &str) -> TrendFilterHolding {
        TrendFilterHolding {
            symbol: symbol.into(),
            qty: dec!(0.1),
            entry_price: dec!(100),
            entry_time_ms: 0,
        }
    }

    fn acts(d: &[TrendFilterDecision]) -> Vec<(Action, &str)> {
        d.iter().map(|d| (d.action, d.symbol.as_str())).collect()
    }

    #[test]
    fn advisor_buys_holds_swaps() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let advisor = TrendFilterAdvisor { settings: &s };
        let signals = [
            sig("A/USDT", 10.0),
            sig("B/USDT", 50.0),
            sig("C/USDT", -5.0),
        ];
        let d = advisor.decide(
            &TrendFilterState::default(),
            &signals,
            dec!(10),
            1_000_000_000,
        );
        assert_eq!(acts(&d), vec![(Action::Buy, "B/USDT")]);
        assert!(d[0].reason.contains("lookback return +50.00%"));

        let held = TrendFilterState {
            holdings: vec![holding("B/USDT")],
            last_rebalance_time_ms: 0,
        };
        let d = advisor.decide(&held, &signals, Decimal::ZERO, DAY_MS * 60);
        assert_eq!(acts(&d), vec![(Action::Hold, "B/USDT")]);

        let swapped = [
            sig("A/USDT", 80.0),
            sig("B/USDT", 10.0),
            sig("C/USDT", -5.0),
        ];
        let d = advisor.decide(&held, &swapped, Decimal::ZERO, DAY_MS * 60);
        assert_eq!(
            acts(&d),
            vec![(Action::Sell, "B/USDT"), (Action::Buy, "A/USDT")]
        );
        assert_eq!(d[0].qty, dec!(0.1));
        assert_eq!(d[1].target_notional_usdt, dec!(10));
    }

    #[test]
    fn advisor_skips() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let advisor = TrendFilterAdvisor { settings: &s };
        let d = advisor.decide(
            &TrendFilterState::default(),
            &[sig("A/USDT", 50.0)],
            dec!(2),
            0,
        );
        assert_eq!(d[0].action, Action::SkipRebalance);
        assert!(
            d[0].reason.ends_with("below min order notional $5.0"),
            "{}",
            d[0].reason
        );

        let recent = TrendFilterState {
            holdings: vec![holding("B/USDT")],
            last_rebalance_time_ms: 1_000_000_000_000,
        };
        let d = advisor.decide(
            &recent,
            &[sig("A/USDT", 80.0)],
            Decimal::ZERO,
            1_000_000_001_000,
        );
        assert_eq!(acts(&d), vec![(Action::SkipCycle, "")]);

        let floor = TrendFilterSettings {
            min_lookback_return_pct: 20.0,
            ..s.clone()
        };
        let d = TrendFilterAdvisor { settings: &floor }.decide(
            &TrendFilterState::default(),
            &[sig("A/USDT", 5.0), sig("B/USDT", -10.0)],
            dec!(10),
            0,
        );
        assert_eq!(acts(&d), vec![(Action::Hold, "")]);
    }

    #[test]
    fn settings_validation_and_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let check = |o: TrendFilterSettings| o.validate().unwrap_err().to_string();
        assert!(
            check(TrendFilterSettings {
                top_k: 0,
                ..s.clone()
            })
            .contains("top_k")
        );
        assert!(
            check(TrendFilterSettings {
                top_k: 100,
                ..s.clone()
            })
            .contains("top_k cannot exceed")
        );
        assert!(
            check(TrendFilterSettings {
                lookback_days: 0,
                ..s.clone()
            })
            .contains("lookback_days")
        );
        assert!(
            check(TrendFilterSettings {
                rebalance_every_days: 0,
                ..s.clone()
            })
            .contains("rebalance_every_days")
        );

        assert!(
            preflight(&s, true)
                .unwrap_err()
                .to_string()
                .contains("TREND_FILTER_PROVEN_EDGE")
        );
        preflight(
            &TrendFilterSettings {
                proven_edge: true,
                ..s.clone()
            },
            true,
        )
        .unwrap();
        std::fs::write(&s.emergency_stop_path, "halt").unwrap();
        assert!(
            preflight(&s, false)
                .unwrap_err()
                .to_string()
                .contains("emergency stop")
        );
    }

    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = TrendFilterState {
            holdings: vec![TrendFilterHolding {
                symbol: "BTC/USDT".into(),
                qty: dec!(0.0001),
                entry_price: dec!(80000),
                entry_time_ms: 123,
            }],
            last_rebalance_time_ms: 456,
        };
        save_state(&state, &path).unwrap();
        assert_eq!(load_state(&path).unwrap(), state);
        assert!(
            load_state(&dir.path().join("none.json"))
                .unwrap()
                .holdings
                .is_empty()
        );
    }

    struct FixedCloses(HashMap<String, Vec<Decimal>>);

    #[async_trait]
    impl KlineSource for FixedCloses {
        async fn daily_closes(&self, symbol: &str, _limit: u32) -> Result<Option<Vec<Decimal>>> {
            Ok(self.0.get(symbol).cloned())
        }
    }

    fn closes(start: i64, end: i64, n: usize) -> Vec<Decimal> {
        let mut v = vec![Decimal::from(start); n];
        if let Some(last) = v.last_mut() {
            *last = Decimal::from(end);
        }
        v
    }

    #[tokio::test]
    async fn signals_need_full_window() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let source = FixedCloses(
            [
                ("A/USDT".to_owned(), closes(100, 150, 32)),
                ("B/USDT".to_owned(), closes(100, 90, 31)),
                ("C/USDT".to_owned(), closes(100, 200, 10)),
            ]
            .into(),
        );
        let signals = fetch_signals(&s, &source).await.unwrap();
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].symbol, "A/USDT");
        assert!((signals[0].lookback_return_pct - 50.0).abs() < 1e-9);
        assert!((signals[1].lookback_return_pct + 10.0).abs() < 1e-9);
    }

    #[derive(Default)]
    struct FakeTrader {
        calls: Mutex<Vec<(&'static str, String, Decimal)>>,
        fail_sells: bool,
        /// Free USDT; 20 when unset.
        free: Option<Decimal>,
    }

    #[async_trait]
    impl SpotTrader for FakeTrader {
        async fn free_usdt(&self) -> Result<Decimal> {
            Ok(self.free.unwrap_or(dec!(20)))
        }
        async fn market_sell(&self, symbol: &str, qty: Decimal) -> Result<SpotFill> {
            self.calls.lock().push(("sell", symbol.into(), qty));
            if self.fail_sells {
                return Err(tf_err("boom"));
            }
            Ok(SpotFill {
                filled: qty,
                average: dec!(100),
                fee: Decimal::ZERO,
                order_id: "s1".into(),
            })
        }
        async fn market_buy_quote(&self, symbol: &str, quote: Decimal) -> Result<SpotFill> {
            self.calls.lock().push(("buy", symbol.into(), quote));
            Ok(SpotFill {
                filled: quote / dec!(150),
                average: dec!(150),
                fee: Decimal::ZERO,
                order_id: "b1".into(),
            })
        }
    }

    #[tokio::test]
    async fn paper_and_live_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let s = TrendFilterSettings {
            min_balance_usdt: dec!(10),
            ..settings(dir.path())
        };
        let source = FixedCloses([("A/USDT".to_owned(), closes(100, 150, 32))].into());

        let plan = run_cycle(&s, Mode::PlanOnly, None, &source).await.unwrap();
        assert_eq!(plan.balance_source, "synthetic");
        assert!(!Path::new(&s.state_path).exists());

        let paper = run_cycle(&s, Mode::Paper, None, &source).await.unwrap();
        assert_eq!(paper.ledger_entries.len(), 1);
        let st = load_state(Path::new(&s.state_path)).unwrap();
        assert_eq!(st.holdings[0].qty, dec!(10) / dec!(150));
        assert!(st.last_rebalance_time_ms > 0);
        // Next paper cycle inside the rebalance window is skipped.
        let again = run_cycle(&s, Mode::Paper, None, &source).await.unwrap();
        assert_eq!(again.decisions[0].action, Action::SkipCycle);

        // Live: swap the held A into B; a failed sell keeps the holding.
        let live = TrendFilterSettings {
            proven_edge: true,
            ..s.clone()
        };
        save_state(
            &TrendFilterState {
                holdings: vec![holding("A/USDT")],
                last_rebalance_time_ms: 0,
            },
            Path::new(&live.state_path),
        )
        .unwrap();
        let source = FixedCloses(
            [
                ("A/USDT".to_owned(), closes(100, 90, 32)),
                ("B/USDT".to_owned(), closes(100, 150, 32)),
            ]
            .into(),
        );
        let trader = FakeTrader {
            fail_sells: true,
            ..FakeTrader::default()
        };
        let payload = run_cycle(&live, Mode::Live, Some(&trader), &source)
            .await
            .unwrap();
        assert_eq!(payload.balance_source, "live");
        assert!(!payload.ledger_entries[0].success);
        assert!(
            payload.ledger_entries[0]
                .error
                .starts_with("TrendFilterError: boom")
        );
        let st = load_state(Path::new(&live.state_path)).unwrap();
        let symbols: Vec<_> = st.holdings.iter().map(|h| h.symbol.as_str()).collect();
        assert_eq!(symbols, ["A/USDT", "B/USDT"]);
        // The failed sell leaves the rebalance due, so the next run retries.
        assert_eq!(st.last_rebalance_time_ms, 0);
        let calls = trader.calls.lock().clone();
        // Planned equity = 20 USDT + 0.1 A × 90 = 29, but only 20 USDT is free.
        assert_eq!(calls[1], ("buy", "B/USDT".to_owned(), dec!(20)));
    }

    fn swap_source() -> FixedCloses {
        FixedCloses(
            [
                ("A/USDT".to_owned(), closes(100, 90, 32)),
                ("B/USDT".to_owned(), closes(100, 150, 32)),
            ]
            .into(),
        )
    }

    fn live_with_holding(dir: &Path, qty: Decimal) -> TrendFilterSettings {
        let s = TrendFilterSettings {
            proven_edge: true,
            min_balance_usdt: Decimal::ZERO,
            ..settings(dir)
        };
        save_state(
            &TrendFilterState {
                holdings: vec![TrendFilterHolding {
                    qty,
                    ..holding("A/USDT")
                }],
                last_rebalance_time_ms: 0,
            },
            Path::new(&s.state_path),
        )
        .unwrap();
        s
    }

    #[tokio::test]
    async fn holding_below_min_notional_is_released_and_rotation_completes() {
        let dir = tempfile::tempdir().unwrap();
        // 0.01 A × 90 = $0.90: Binance would reject the sell with NOTIONAL.
        let s = live_with_holding(dir.path(), dec!(0.01));
        let trader = FakeTrader::default();
        let payload = run_cycle(&s, Mode::Live, Some(&trader), &swap_source())
            .await
            .unwrap();
        assert_eq!(
            acts(&payload.decisions),
            vec![(Action::ReleaseDust, "A/USDT"), (Action::Buy, "B/USDT")]
        );
        // Dust is excluded from equity: the buy targets the 20 free USDT only.
        assert_eq!(
            *trader.calls.lock(),
            [("buy", "B/USDT".to_owned(), dec!(20))]
        );
        assert!(payload.ledger_entries.iter().all(|e| e.success));
        let st = load_state(Path::new(&s.state_path)).unwrap();
        let symbols: Vec<_> = st.holdings.iter().map(|h| h.symbol.as_str()).collect();
        assert_eq!(symbols, ["B/USDT"]);
        assert!(st.last_rebalance_time_ms > 0);
    }

    #[tokio::test]
    async fn buy_is_skipped_when_free_cash_is_below_min_notional() {
        let dir = tempfile::tempdir().unwrap();
        let s = live_with_holding(dir.path(), dec!(0.1));
        // Equity 3 + 0.1 × 90 = 12 plans a $12 buy; the sell fails, leaving $3.
        let trader = FakeTrader {
            fail_sells: true,
            free: Some(dec!(3)),
            ..FakeTrader::default()
        };
        let payload = run_cycle(&s, Mode::Live, Some(&trader), &swap_source())
            .await
            .unwrap();
        assert_eq!(
            *trader.calls.lock(),
            [("sell", "A/USDT".to_owned(), dec!(0.1))]
        );
        let buy = &payload.ledger_entries[1];
        assert_eq!((buy.action, buy.success), (Action::Buy, false));
        assert!(buy.error.contains("below min order notional"), "{}", buy.error);
        let st = load_state(Path::new(&s.state_path)).unwrap();
        assert_eq!(st.holdings.len(), 1);
        assert_eq!(st.last_rebalance_time_ms, 0);
    }

    #[tokio::test]
    async fn live_without_dynamic_balance_is_insufficient() {
        let dir = tempfile::tempdir().unwrap();
        let s = TrendFilterSettings {
            proven_edge: true,
            dynamic_balance: false,
            ..settings(dir.path())
        };
        let source = FixedCloses([("A/USDT".to_owned(), closes(100, 150, 32))].into());
        let trader = FakeTrader::default();
        let payload = run_cycle(&s, Mode::Live, Some(&trader), &source)
            .await
            .unwrap();
        assert_eq!(payload.balance_source, "missing");
        assert!(payload.insufficient_balance);
        assert!(trader.calls.lock().is_empty());
    }
}

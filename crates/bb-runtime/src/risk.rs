//! Port of `src/risk/guard.py`: pre-trade risk checks.
//!
//! The legacy `data/go_off.flag` bypass of the daily loss check is not
//! ported: the daily loss limit is always enforced.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bb_core::Decimal;
use chrono::NaiveDate;
use rust_decimal_macros::dec;
use serde_json::{Value, json};

use crate::db::{OrderDb, OrderDbError};

/// An order violated a configured risk limit.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct RiskBreachError {
    pub message: String,
    /// Structured details about the rejected order or state.
    pub context: Value,
}

impl RiskBreachError {
    fn new(message: impl Into<String>, context: Value) -> Self {
        Self {
            message: message.into(),
            context,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error(transparent)]
    Breach(#[from] RiskBreachError),
    #[error("invalid risk configuration: {0}")]
    Config(String),
    #[error(transparent)]
    Db(#[from] OrderDbError),
}

/// Validated runtime settings for [`RiskGuard`].
#[derive(Debug, Clone, PartialEq)]
pub struct RiskConfig {
    pub max_position_size_usd: Decimal,
    pub daily_loss_limit_usd: Decimal,
    pub duplicate_window_seconds: f64,
    pub max_total_exposure_usd: Option<Decimal>,
    pub daily_drawdown_limit_pct: Option<Decimal>,
    pub min_balance_usdt: Decimal,
    pub allowed_leverage: Option<Decimal>,
    pub max_open_orders: Option<i64>,
    pub max_price_deviation_pct: Option<Decimal>,
}

impl RiskConfig {
    /// Python field defaults for everything but the two required limits.
    pub fn new(max_position_size_usd: Decimal, daily_loss_limit_usd: Decimal) -> Self {
        Self {
            max_position_size_usd,
            daily_loss_limit_usd,
            duplicate_window_seconds: 60.0,
            max_total_exposure_usd: None,
            daily_drawdown_limit_pct: None,
            min_balance_usdt: Decimal::ZERO,
            allowed_leverage: None,
            max_open_orders: None,
            max_price_deviation_pct: None,
        }
    }

    /// Pydantic field constraints (`gt=0` / `ge=0`, finite).
    pub fn validate(&self) -> Result<(), RiskError> {
        let gt = |v: Decimal, name: &str| {
            if v <= Decimal::ZERO {
                Err(RiskError::Config(format!("{name} must be greater than 0")))
            } else {
                Ok(())
            }
        };
        gt(self.max_position_size_usd, "max_position_size_usd")?;
        gt(self.daily_loss_limit_usd, "daily_loss_limit_usd")?;
        if !self.duplicate_window_seconds.is_finite() {
            return Err(RiskError::Config(
                "risk threshold must be a finite number".into(),
            ));
        }
        if self.duplicate_window_seconds <= 0.0 {
            return Err(RiskError::Config(
                "duplicate_window_seconds must be greater than 0".into(),
            ));
        }
        if let Some(v) = self.max_total_exposure_usd {
            gt(v, "max_total_exposure_usd")?;
        }
        if let Some(v) = self.daily_drawdown_limit_pct {
            gt(v, "daily_drawdown_limit_pct")?;
        }
        if self.min_balance_usdt < Decimal::ZERO {
            return Err(RiskError::Config(
                "min_balance_usdt must be at least 0".into(),
            ));
        }
        if let Some(v) = self.allowed_leverage {
            gt(v, "allowed_leverage")?;
        }
        if self.max_open_orders.is_some_and(|n| n <= 0) {
            return Err(RiskError::Config(
                "max_open_orders must be greater than 0".into(),
            ));
        }
        if self
            .max_price_deviation_pct
            .is_some_and(|v| v < Decimal::ZERO)
        {
            return Err(RiskError::Config(
                "max_price_deviation_pct must be at least 0".into(),
            ));
        }
        Ok(())
    }

    /// Whether any portfolio-aware check is configured (`_should_use_structured_risk`).
    pub fn uses_structured_checks(&self) -> bool {
        self.max_total_exposure_usd.is_some()
            || self.daily_drawdown_limit_pct.is_some()
            || self.allowed_leverage.is_some()
            || self.max_open_orders.is_some()
            || self.max_price_deviation_pct.is_some()
            || self.min_balance_usdt > Decimal::ZERO
    }
}

/// Simple order snapshot (the positional `validate_order` form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderCheck {
    pub symbol: String,
    pub side: String,
    pub amount: Decimal,
    pub price: Decimal,
    /// Existing signed base position before the order.
    pub current_position: Decimal,
}

/// Structured order request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: String,
    pub amount: Decimal,
    pub price: Decimal,
    pub leverage: Decimal,
    pub order_type: String,
}

impl OrderRequest {
    pub fn new(
        symbol: impl Into<String>,
        side: impl Into<String>,
        amount: Decimal,
        price: Decimal,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            side: side.into(),
            amount,
            price,
            leverage: Decimal::ONE,
            order_type: "limit".to_owned(),
        }
    }

    fn notional(&self) -> Decimal {
        self.amount * self.price
    }
}

/// Structured account snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct PortfolioState {
    pub total_equity_usdt: Decimal,
    pub available_balance_usdt: Decimal,
    pub realized_pnl_today_usdt: Decimal,
    pub unrealized_pnl_today_usdt: Decimal,
    /// Signed USDT exposure per symbol.
    pub positions: BTreeMap<String, Decimal>,
    pub total_exposure_usdt: Decimal,
    pub open_orders_count: usize,
}

/// Structured market snapshot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MarketData {
    pub symbol: String,
    pub mark_price: Decimal,
}

/// Monotonic clock and UTC calendar used by the guard (injectable for tests).
pub trait RiskClock: Send + Sync {
    fn monotonic(&self) -> Instant;
    fn utc_date(&self) -> NaiveDate;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl RiskClock for SystemClock {
    fn monotonic(&self) -> Instant {
        Instant::now()
    }

    fn utc_date(&self) -> NaiveDate {
        bb_core::time::now().date_naive()
    }
}

type DuplicateKey = (String, String, Decimal);

struct GuardState {
    current_daily_loss: Decimal,
    loss_window_date: NaiveDate,
    duplicate_orders: HashMap<DuplicateKey, Instant>,
    duplicate_events: VecDeque<(Instant, DuplicateKey)>,
}

/// Validates orders against position, loss, exposure and duplicate limits.
pub struct RiskGuard {
    config: RiskConfig,
    db: Option<Arc<OrderDb>>,
    clock: Arc<dyn RiskClock>,
    state: Mutex<GuardState>,
}

impl std::fmt::Debug for RiskGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RiskGuard")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

const FLOAT_TOLERANCE: Decimal = dec!(0.000000001);

impl RiskGuard {
    pub fn new(config: RiskConfig, db: Option<Arc<OrderDb>>) -> Result<Self, RiskError> {
        Self::with_clock(config, db, Arc::new(SystemClock))
    }

    pub fn with_clock(
        config: RiskConfig,
        db: Option<Arc<OrderDb>>,
        clock: Arc<dyn RiskClock>,
    ) -> Result<Self, RiskError> {
        config.validate()?;
        let today = clock.utc_date();
        Ok(Self {
            config,
            db,
            clock,
            state: Mutex::new(GuardState {
                current_daily_loss: Decimal::ZERO,
                loss_window_date: today,
                duplicate_orders: HashMap::new(),
                duplicate_events: VecDeque::new(),
            }),
        })
    }

    pub fn config(&self) -> &RiskConfig {
        &self.config
    }

    fn lock(&self) -> MutexGuard<'_, GuardState> {
        self.state.lock()
    }

    /// Reload today's accumulated loss from persistent storage on startup.
    pub fn restore_daily_loss_from_db(&self) -> Result<(), RiskError> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        let today = self.clock.utc_date();
        let loss = db.load_daily_loss(&today.to_string())?;
        {
            let mut st = self.lock();
            st.current_daily_loss = loss;
            st.loss_window_date = today;
        }
        tracing::info!(component = "risk_guard", date = %today, restored_loss = %loss, "Daily loss restored from database");
        Ok(())
    }

    /// Realized loss accumulated for the active UTC day.
    pub fn current_daily_loss(&self) -> Decimal {
        let mut st = self.lock();
        self.reset_daily_loss_if_needed(&mut st);
        st.current_daily_loss
    }

    /// Simple validation: position size, daily loss and duplicate window.
    pub fn validate_order(&self, check: &OrderCheck) -> Result<(), RiskBreachError> {
        let context = json!({
            "symbol": check.symbol,
            "side": check.side,
            "amount": check.amount,
            "price": check.price,
            "current_position": check.current_position,
        });
        tracing::info!(component = "risk_guard", context = %context, "Risk validation started");
        let result = (|| {
            validate_inputs(
                &check.symbol,
                &check.side,
                check.amount,
                check.price,
                check.current_position,
            )?;
            self.check_max_position_size(
                &check.symbol,
                check.amount,
                check.price,
                check.current_position,
                &check.side,
            )?;
            self.check_daily_loss()?;
            self.check_duplicate_order(&check.symbol, &check.side, check.amount)
        })();
        log_result(&context, &result);
        result
    }

    /// Structured validation against portfolio and market state.
    pub fn validate_structured(
        &self,
        order: &OrderRequest,
        portfolio: &PortfolioState,
        market: &MarketData,
    ) -> Result<(), RiskBreachError> {
        let context = json!({"order": order, "portfolio": portfolio, "market_data": market});
        tracing::info!(component = "risk_guard", context = %context, "Risk validation started");
        let result = (|| {
            if portfolio.total_equity_usdt <= Decimal::ZERO {
                // Pydantic rejected `PortfolioState(total_equity_usdt <= 0)`.
                return Err(RiskBreachError::new(
                    "total_equity_usdt must be greater than 0",
                    json!({"total_equity_usdt": portfolio.total_equity_usdt}),
                ));
            }
            validate_market_symbol(order, market)?;
            let symbol_exposure = portfolio
                .positions
                .get(&order.symbol)
                .copied()
                .unwrap_or_default();
            let current_position = if order.price > Decimal::ZERO {
                symbol_exposure / order.price
            } else {
                Decimal::ZERO
            };
            validate_inputs(
                &order.symbol,
                &order.side,
                order.amount,
                order.price,
                current_position,
            )?;
            self.check_position_exposure(order, portfolio)?;
            self.check_daily_drawdown(portfolio)?;
            self.check_minimum_balance(order, portfolio)?;
            self.check_leverage(order)?;
            self.check_max_open_orders(portfolio)?;
            self.check_price_deviation(order, market)?;
            self.check_daily_loss()?;
            self.check_duplicate_order(&order.symbol, &order.side, order.amount)
        })();
        log_result(&context, &result);
        result
    }

    /// Whether the resulting position value exceeds the limit.
    pub fn check_max_position_size(
        &self,
        symbol: &str,
        amount: Decimal,
        price: Decimal,
        current_position: Decimal,
        side: &str,
    ) -> Result<(), RiskBreachError> {
        let side = side.trim().to_lowercase();
        let delta = if side == "buy" { amount } else { -amount };
        let resulting = current_position + delta;
        let value = resulting.abs() * price;
        if value > self.config.max_position_size_usd {
            let context = json!({
                "symbol": symbol,
                "side": side,
                "amount": amount,
                "price": price,
                "current_position": current_position,
                "position_delta": delta,
                "resulting_position": resulting,
                "resulting_value_usd": value,
                "max_position_size_usd": self.config.max_position_size_usd,
            });
            tracing::error!(component = "risk_guard", context = %context, "Maximum position size breached");
            return Err(RiskBreachError::new(
                format!(
                    "maximum position size breached: {} USD exceeds {} USD",
                    f8(value),
                    f8(self.config.max_position_size_usd)
                ),
                context,
            ));
        }
        Ok(())
    }

    /// Whether today's realized loss limit is exhausted (resets at 00:00 UTC).
    pub fn check_daily_loss(&self) -> Result<(), RiskBreachError> {
        let (loss, date) = {
            let mut st = self.lock();
            self.reset_daily_loss_if_needed(&mut st);
            (st.current_daily_loss, st.loss_window_date)
        };
        if loss >= self.config.daily_loss_limit_usd {
            let context = json!({
                "current_daily_loss": loss,
                "daily_loss_limit_usd": self.config.daily_loss_limit_usd,
                "loss_window_date": date.to_string(),
            });
            tracing::error!(component = "risk_guard", context = %context, "Daily loss limit breached");
            return Err(RiskBreachError::new(
                format!(
                    "daily loss limit breached: {} USD has reached or exceeded {} USD",
                    f8(loss),
                    f8(self.config.daily_loss_limit_usd)
                ),
                context,
            ));
        }
        Ok(())
    }

    /// Apply realized PnL from a closed trade; only losses accumulate.
    pub fn update_realized_pnl(&self, pnl: Decimal) -> Result<(), RiskError> {
        let (increment, current, date) = {
            let mut st = self.lock();
            self.reset_daily_loss_if_needed(&mut st);
            let increment = if pnl < Decimal::ZERO {
                pnl.abs()
            } else {
                Decimal::ZERO
            };
            st.current_daily_loss += increment;
            (increment, st.current_daily_loss, st.loss_window_date)
        };
        if let Some(db) = &self.db
            && increment > Decimal::ZERO
        {
            db.upsert_daily_loss(&date.to_string(), current)?;
        }
        if self.current_daily_loss() >= self.config.daily_loss_limit_usd {
            tracing::warn!(component = "risk_guard", %pnl, loss_increment = %increment, current_daily_loss = %current, "Realized PnL applied to risk guard");
        } else {
            tracing::info!(component = "risk_guard", %pnl, loss_increment = %increment, current_daily_loss = %current, "Realized PnL applied to risk guard");
        }
        Ok(())
    }

    /// Check and register an order against the duplicate-order window.
    pub fn check_duplicate_order(
        &self,
        symbol: &str,
        side: &str,
        amount: Decimal,
    ) -> Result<(), RiskBreachError> {
        let now = self.clock.monotonic();
        let key: DuplicateKey = (
            symbol.trim().to_owned(),
            side.trim().to_lowercase(),
            amount.normalize(),
        );
        let window = self.config.duplicate_window_seconds;
        let mut st = self.lock();
        prune_duplicates(&mut st, now, window);
        if let Some(previous) = st.duplicate_orders.get(&key) {
            let elapsed = now.saturating_duration_since(*previous).as_secs_f64();
            let context = json!({
                "symbol": key.0,
                "side": key.1,
                "amount": key.2,
                "elapsed_seconds": elapsed,
                "duplicate_window_seconds": window,
            });
            tracing::error!(component = "risk_guard", context = %context, "Duplicate order breached");
            return Err(RiskBreachError::new(
                format!(
                    "duplicate order breached: identical order submitted {elapsed:.3}s ago within {window:.3}s window"
                ),
                context,
            ));
        }
        st.duplicate_orders.insert(key.clone(), now);
        st.duplicate_events.push_back((now, key));
        Ok(())
    }

    /// Test hook: pretend `key` was validated `age` ago (Python tests poked
    /// the private dicts for the same purpose).
    pub fn backdate_duplicate(&self, symbol: &str, side: &str, amount: Decimal, age: Duration) {
        let now = self.clock.monotonic();
        let stamp = now.checked_sub(age).unwrap_or(now);
        let key: DuplicateKey = (
            symbol.trim().to_owned(),
            side.trim().to_lowercase(),
            amount.normalize(),
        );
        let mut st = self.lock();
        st.duplicate_events.clear();
        st.duplicate_orders.insert(key.clone(), stamp);
        st.duplicate_events.push_back((stamp, key));
    }

    fn check_position_exposure(
        &self,
        order: &OrderRequest,
        portfolio: &PortfolioState,
    ) -> Result<(), RiskBreachError> {
        let signed_exposure = portfolio
            .positions
            .get(&order.symbol)
            .copied()
            .unwrap_or_default();
        let current_symbol = signed_exposure.abs();
        let notional = order.notional();
        let resulting_symbol = if order.side.trim().eq_ignore_ascii_case("buy") {
            signed_exposure + notional
        } else {
            signed_exposure - notional
        }
        .abs();
        let mut context = json!({
            "symbol": order.symbol,
            "side": order.side,
            "order_notional_usdt": notional,
            "current_symbol_exposure_usdt": current_symbol,
            "resulting_symbol_exposure_usdt": resulting_symbol,
            "max_position_size_usd": self.config.max_position_size_usd,
        });
        if resulting_symbol > self.config.max_position_size_usd {
            tracing::error!(component = "risk_guard", context = %context, "Maximum position size breached");
            return Err(RiskBreachError::new(
                format!(
                    "maximum position size breached: {} USD exceeds {} USD",
                    f8(resulting_symbol),
                    f8(self.config.max_position_size_usd)
                ),
                context,
            ));
        }
        let Some(max_total) = self.config.max_total_exposure_usd else {
            return Ok(());
        };
        let position_total: Decimal = portfolio.positions.values().map(|v| v.abs()).sum();
        let current_total = portfolio.total_exposure_usdt.max(position_total);
        let resulting_total = current_total - current_symbol + resulting_symbol;
        if let Value::Object(map) = &mut context {
            map.insert("current_total_exposure_usdt".into(), json!(current_total));
            map.insert(
                "resulting_total_exposure_usdt".into(),
                json!(resulting_total),
            );
            map.insert("max_total_exposure_usd".into(), json!(max_total));
        }
        if resulting_total > max_total {
            tracing::error!(component = "risk_guard", context = %context, "Maximum total exposure breached");
            return Err(RiskBreachError::new(
                format!(
                    "maximum total exposure breached: {} USD exceeds {} USD",
                    f8(resulting_total),
                    f8(max_total)
                ),
                context,
            ));
        }
        Ok(())
    }

    fn check_daily_drawdown(&self, portfolio: &PortfolioState) -> Result<(), RiskBreachError> {
        let Some(limit) = self.config.daily_drawdown_limit_pct else {
            return Ok(());
        };
        let pnl = portfolio.realized_pnl_today_usdt + portfolio.unrealized_pnl_today_usdt;
        let loss = if pnl < Decimal::ZERO {
            pnl.abs()
        } else {
            Decimal::ZERO
        };
        let drawdown = loss / portfolio.total_equity_usdt * dec!(100);
        if drawdown > limit {
            let context = json!({
                "daily_pnl_usdt": pnl,
                "daily_loss_usdt": loss,
                "total_equity_usdt": portfolio.total_equity_usdt,
                "daily_drawdown_pct": drawdown,
                "daily_drawdown_limit_pct": limit,
            });
            tracing::error!(component = "risk_guard", context = %context, "Daily drawdown breached");
            return Err(RiskBreachError::new(
                format!(
                    "daily drawdown breached: {}% exceeds {}%",
                    f8(drawdown),
                    f8(limit)
                ),
                context,
            ));
        }
        Ok(())
    }

    fn check_minimum_balance(
        &self,
        order: &OrderRequest,
        portfolio: &PortfolioState,
    ) -> Result<(), RiskBreachError> {
        let margin = order.notional() / order.leverage;
        let remaining = portfolio.available_balance_usdt - margin;
        if remaining < self.config.min_balance_usdt {
            let context = json!({
                "available_balance_usdt": portfolio.available_balance_usdt,
                "order_margin_usdt": margin,
                "remaining_balance_usdt": remaining,
                "min_balance_usdt": self.config.min_balance_usdt,
            });
            tracing::error!(component = "risk_guard", context = %context, "Minimum balance breached");
            return Err(RiskBreachError::new(
                format!(
                    "minimum balance breached: {} USDT would be below {} USDT",
                    f8(remaining),
                    f8(self.config.min_balance_usdt)
                ),
                context,
            ));
        }
        Ok(())
    }

    fn check_leverage(&self, order: &OrderRequest) -> Result<(), RiskBreachError> {
        let Some(allowed) = self.config.allowed_leverage else {
            return Ok(());
        };
        if (order.leverage - allowed).abs() <= FLOAT_TOLERANCE {
            return Ok(());
        }
        let context = json!({"requested_leverage": order.leverage, "allowed_leverage": allowed});
        tracing::error!(component = "risk_guard", context = %context, "Leverage check breached");
        Err(RiskBreachError::new(
            format!(
                "leverage check breached: {} does not match {}",
                f8(order.leverage),
                f8(allowed)
            ),
            context,
        ))
    }

    fn check_max_open_orders(&self, portfolio: &PortfolioState) -> Result<(), RiskBreachError> {
        let Some(max) = self.config.max_open_orders else {
            return Ok(());
        };
        let resulting = portfolio.open_orders_count as i64 + 1;
        if resulting > max {
            let context = json!({
                "open_orders_count": portfolio.open_orders_count,
                "resulting_open_orders": resulting,
                "max_open_orders": max,
            });
            tracing::error!(component = "risk_guard", context = %context, "Maximum open orders breached");
            return Err(RiskBreachError::new(
                format!("maximum open orders breached: {resulting} exceeds {max}"),
                context,
            ));
        }
        Ok(())
    }

    fn check_price_deviation(
        &self,
        order: &OrderRequest,
        market: &MarketData,
    ) -> Result<(), RiskBreachError> {
        let Some(max) = self.config.max_price_deviation_pct else {
            return Ok(());
        };
        let deviation = (order.price - market.mark_price).abs() / market.mark_price * dec!(100);
        if deviation > max {
            let context = json!({
                "symbol": order.symbol,
                "limit_price": order.price,
                "mark_price": market.mark_price,
                "price_deviation_pct": deviation,
                "max_price_deviation_pct": max,
            });
            tracing::error!(component = "risk_guard", context = %context, "Price deviation breached");
            return Err(RiskBreachError::new(
                format!(
                    "price deviation breached: {}% exceeds {}%",
                    f8(deviation),
                    f8(max)
                ),
                context,
            ));
        }
        Ok(())
    }

    fn reset_daily_loss_if_needed(&self, st: &mut GuardState) {
        let today = self.clock.utc_date();
        if today == st.loss_window_date {
            return;
        }
        let previous_date = st.loss_window_date;
        let previous_loss = st.current_daily_loss;
        st.current_daily_loss = Decimal::ZERO;
        st.loss_window_date = today;
        tracing::info!(
            component = "risk_guard",
            previous_loss_window_date = %previous_date,
            loss_window_date = %today,
            previous_daily_loss = %previous_loss,
            "Daily loss counter reset"
        );
    }
}

fn prune_duplicates(st: &mut GuardState, now: Instant, window_seconds: f64) {
    let window = Duration::from_secs_f64(window_seconds);
    while let Some((stamp, key)) = st.duplicate_events.front() {
        // Python: keep entries newer than `now - window`.
        if now.saturating_duration_since(*stamp) < window {
            break;
        }
        if st.duplicate_orders.get(key) == Some(stamp) {
            st.duplicate_orders.remove(key);
        }
        st.duplicate_events.pop_front();
    }
}

fn validate_market_symbol(
    order: &OrderRequest,
    market: &MarketData,
) -> Result<(), RiskBreachError> {
    let order_symbol = order.symbol.trim();
    let market_symbol = market.symbol.trim();
    if order_symbol == market_symbol {
        return Ok(());
    }
    let context = json!({"order_symbol": order_symbol, "market_symbol": market_symbol});
    tracing::error!(component = "risk_guard", context = %context, "Market data symbol mismatch");
    Err(RiskBreachError::new("market data symbol mismatch", context))
}

fn validate_inputs(
    symbol: &str,
    side: &str,
    amount: Decimal,
    price: Decimal,
    current_position: Decimal,
) -> Result<(), RiskBreachError> {
    let context = || {
        json!({
            "symbol": symbol,
            "side": side,
            "amount": amount,
            "price": price,
            "current_position": current_position,
        })
    };
    if symbol.trim().is_empty() {
        return Err(RiskBreachError::new("symbol must not be blank", context()));
    }
    if !matches!(side.trim().to_lowercase().as_str(), "buy" | "sell") {
        return Err(RiskBreachError::new(
            "side must be 'buy' or 'sell'",
            context(),
        ));
    }
    if amount <= Decimal::ZERO {
        return Err(RiskBreachError::new(
            "amount must be a finite value greater than zero",
            context(),
        ));
    }
    if price <= Decimal::ZERO {
        return Err(RiskBreachError::new(
            "price must be a finite value greater than zero",
            context(),
        ));
    }
    Ok(())
}

fn log_result(context: &Value, result: &Result<(), RiskBreachError>) {
    match result {
        Ok(()) => {
            tracing::info!(component = "risk_guard", context = %context, "Risk validation passed")
        }
        Err(err) => tracing::error!(
            component = "risk_guard",
            context = %context,
            risk_context = %err.context,
            "Risk validation failed"
        ),
    }
}

/// Python `f"{value:.8f}"`.
fn f8(value: Decimal) -> String {
    format!("{:.8}", value.round_dp(8))
}

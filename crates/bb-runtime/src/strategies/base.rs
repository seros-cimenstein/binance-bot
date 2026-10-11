//! Port of `src/strategies/base.py`: the strategy plugin interface and the
//! normalised market/fill payloads strategies receive.

use std::sync::Arc;

use async_trait::async_trait;
use bb_binance::feeds::CandleData;
use bb_core::Decimal;

use crate::orders::models::{OrderSide, OrderStatus, OrderType};

#[derive(Debug, thiserror::Error)]
pub enum StrategyError {
    /// Invalid configuration or market payload (Python `ValueError`).
    #[error("{0}")]
    Invalid(String),
    /// Order submission failed (sizing, risk, exchange, ...).
    #[error("{0}")]
    Order(String),
}

/// Normalised market tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickData {
    pub timestamp: i64,
    pub symbol: String,
    pub bid: Decimal,
    pub ask: Decimal,
    pub last_price: Decimal,
    pub volume: Decimal,
}

/// OHLCV candle passed to strategies (`closed` mirrors the Python payload's
/// `closed` / `is_closed` / `x` flag; feed candles are always closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candle {
    pub symbol: String,
    pub timestamp: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub closed: bool,
}

impl From<CandleData> for Candle {
    fn from(c: CandleData) -> Self {
        Self {
            symbol: c.symbol,
            timestamp: c.timestamp,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
            closed: true,
        }
    }
}

/// Normalised order fill (validated like the Pydantic model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillData {
    pub order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub quantity: Decimal,
    pub commission: Decimal,
    /// Epoch milliseconds.
    pub timestamp: i64,
}

impl FillData {
    /// `price > 0`, `quantity > 0`, `commission >= 0`, `timestamp >= 0`, non-blank symbol.
    pub fn validate(&self) -> Result<(), StrategyError> {
        if self.symbol.is_empty() {
            return Err(StrategyError::Invalid(
                "fill symbol must not be blank".into(),
            ));
        }
        if self.price <= Decimal::ZERO || self.quantity <= Decimal::ZERO {
            return Err(StrategyError::Invalid(
                "fill price and quantity must be greater than 0".into(),
            ));
        }
        if self.commission < Decimal::ZERO || self.timestamp < 0 {
            return Err(StrategyError::Invalid(
                "fill commission and timestamp must be non-negative".into(),
            ));
        }
        Ok(())
    }
}

/// Order a strategy asks its client to submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyOrder {
    pub symbol: String,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub amount: Decimal,
    pub price: Option<Decimal>,
    /// `size_order` flag: `false` bypasses dynamic sizing (position closes).
    pub size_order: bool,
}

impl StrategyOrder {
    pub fn market(symbol: impl Into<String>, side: OrderSide, amount: Decimal) -> Self {
        Self {
            symbol: symbol.into(),
            order_type: OrderType::Market,
            side,
            amount,
            price: None,
            size_order: true,
        }
    }
}

/// ccxt-like order result returned to strategies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderResult {
    pub id: String,
    pub symbol: String,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub amount: Decimal,
    pub price: Option<Decimal>,
    pub status: OrderStatus,
    pub filled: Decimal,
    pub remaining: Decimal,
    pub average: Option<Decimal>,
}

/// Client capability strategies use to execute signals.
#[async_trait]
pub trait OrderClient: Send + Sync {
    async fn create_order(&self, order: StrategyOrder) -> Result<OrderResult, StrategyError>;

    /// Record the most recent market price for risk validation.
    fn update_last_price(&self, price: Decimal);
}

/// A trading strategy plugin. Methods take `&self`; implementations keep
/// their mutable state behind a lock that is never held across `.await`.
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Plugin name used for logs and discovery.
    fn name(&self) -> &str;

    fn client(&self) -> &Arc<dyn OrderClient>;

    fn is_running(&self) -> bool;

    /// Setup before ticks and fills are dispatched.
    async fn start(&self);

    /// Graceful teardown.
    async fn stop(&self);

    async fn on_tick(&self, tick: TickData);

    /// Candle adapter; malformed candles are errors (they crash the feed task
    /// like the Python `ValueError` did).
    async fn on_candle(&self, candle: &Candle) -> Result<(), StrategyError>;

    async fn on_fill(&self, fill: FillData);

    async fn on_error(&self, error: &StrategyError);
}

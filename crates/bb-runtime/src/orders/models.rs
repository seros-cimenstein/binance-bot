//! Port of `src/orders/models.py`.

use std::fmt;
use std::str::FromStr;

use bb_core::Decimal;
use chrono::NaiveDateTime;

/// Order lifecycle status (persisted as the lower-case value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderStatus {
    Pending,
    Open,
    Closed,
    Canceled,
    Expired,
    Rejected,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Canceled => "canceled",
            Self::Expired => "expired",
            Self::Rejected => "rejected",
        }
    }

    /// `ACTIVE_ORDER_STATUSES`.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Open)
    }

    /// `TERMINAL_ORDER_STATUSES`.
    pub fn is_terminal(self) -> bool {
        !self.is_active()
    }
}

impl FromStr for OrderStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "canceled" => Ok(Self::Canceled),
            "expired" => Ok(Self::Expired),
            "rejected" => Ok(Self::Rejected),
            other => Err(format!("unknown order status {other:?}")),
        }
    }
}

impl fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }

    pub fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

impl FromStr for OrderSide {
    type Err = String;

    /// Case-insensitive, surrounding whitespace ignored.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "buy" => Ok(Self::Buy),
            "sell" => Ok(Self::Sell),
            _ => Err("side must be 'buy' or 'sell'".to_owned()),
        }
    }
}

impl fmt::Display for OrderSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderType {
    Limit,
    Market,
}

impl OrderType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::Market => "market",
        }
    }
}

impl FromStr for OrderType {
    type Err = String;

    /// Case-insensitive, surrounding whitespace ignored.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "limit" => Ok(Self::Limit),
            "market" => Ok(Self::Market),
            _ => Err("order_type must be 'limit' or 'market'".to_owned()),
        }
    }
}

impl fmt::Display for OrderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One execution. Timestamps are naive UTC (as persisted by Python).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    pub id: String,
    pub order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub amount: Decimal,
    pub fee: Decimal,
    pub fee_currency: String,
    pub timestamp: NaiveDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: String,
    pub symbol: String,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub amount: Decimal,
    pub price: Option<Decimal>,
    pub status: OrderStatus,
    pub filled_amount: Decimal,
    pub remaining_amount: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub fills: Vec<Fill>,
    pub timestamp: NaiveDateTime,
}

impl Order {
    /// A pending order with Python's dataclass defaults.
    pub fn new(
        id: impl Into<String>,
        symbol: impl Into<String>,
        order_type: OrderType,
        side: OrderSide,
        amount: Decimal,
    ) -> Self {
        Self {
            id: id.into(),
            symbol: symbol.into(),
            order_type,
            side,
            amount,
            price: None,
            status: OrderStatus::Pending,
            filled_amount: Decimal::ZERO,
            remaining_amount: Decimal::ZERO,
            avg_fill_price: None,
            fills: Vec::new(),
            timestamp: utc_now(),
        }
    }
}

/// Net position derived from fills (`amount` is signed: negative = short).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub symbol: String,
    pub amount: Decimal,
    pub entry_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub leverage: Decimal,
    pub timestamp: NaiveDateTime,
}

impl Position {
    pub fn new(symbol: impl Into<String>, amount: Decimal, entry_price: Decimal) -> Self {
        Self {
            symbol: symbol.into(),
            amount,
            entry_price,
            unrealized_pnl: Decimal::ZERO,
            leverage: Decimal::ONE,
            timestamp: utc_now(),
        }
    }
}
/// Naive UTC "now" at microsecond resolution (Python
/// `datetime.now(timezone.utc).replace(tzinfo=None)`), so values survive the
/// ISO round trip through SQLite unchanged.
pub fn utc_now() -> NaiveDateTime {
    use chrono::{DurationRound, TimeDelta};
    let now = bb_core::time::now().naive_utc();
    now.duration_trunc(TimeDelta::microseconds(1))
        .unwrap_or(now)
}

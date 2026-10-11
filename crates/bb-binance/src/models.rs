//! Shared wire types used by several endpoint groups.

use rust_decimal::Decimal;
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// `{"serverTime": ...}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTime {
    pub server_time: i64,
}

/// One `rateLimits` entry of `exchangeInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RateLimitRule {
    pub rate_limit_type: String,
    pub interval: String,
    pub interval_num: u32,
    pub limit: u64,
}

impl Default for RateLimitRule {
    fn default() -> Self {
        Self {
            rate_limit_type: String::new(),
            interval: String::new(),
            interval_num: 1,
            limit: 0,
        }
    }
}

/// `[price, quantity]` order-book level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "(Decimal, Decimal)")]
pub struct PriceLevel {
    pub price: Decimal,
    pub quantity: Decimal,
}

impl From<(Decimal, Decimal)> for PriceLevel {
    fn from((price, quantity): (Decimal, Decimal)) -> Self {
        Self { price, quantity }
    }
}

/// One kline row (`/api/v3/klines`, `/fapi/v1/klines`). Rows with at least the six
/// OHLCV fields are accepted; trailing fields are optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Kline {
    pub open_time: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub close_time: Option<i64>,
    pub quote_volume: Option<Decimal>,
    pub trades: Option<u64>,
    pub taker_buy_base_volume: Option<Decimal>,
    pub taker_buy_quote_volume: Option<Decimal>,
}

impl Kline {
    /// ccxt-shaped OHLCV row `[timestamp, open, high, low, close, volume]`.
    pub fn ohlcv(&self) -> Ohlcv {
        Ohlcv {
            timestamp: self.open_time,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
        }
    }
}

/// ccxt-compatible OHLCV candle (`[timestamp, open, high, low, close, volume]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ohlcv {
    pub timestamp: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
}

/// Binance encodes some integers as JSON numbers and some as strings. Parsed via
/// `serde_json::Value` because untagged enums break under `arbitrary_precision`.
fn value_to_i64<E: de::Error>(value: serde_json::Value) -> Result<i64, E> {
    match value {
        serde_json::Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| E::custom(format!("{n} is not an integer"))),
        serde_json::Value::String(s) => s.trim().parse().map_err(E::custom),
        other => Err(E::custom(format!("expected integer, got {other}"))),
    }
}

impl<'de> Deserialize<'de> for Kline {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KlineVisitor;
        impl<'de> Visitor<'de> for KlineVisitor {
            type Value = Kline;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a kline array with at least 6 fields")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Kline, A::Error> {
                let missing = |i: usize| de::Error::invalid_length(i, &"at least 6 kline fields");
                let open_time: i64 = seq.next_element::<i64>()?.ok_or_else(|| missing(0))?;
                let open: Decimal = seq.next_element()?.ok_or_else(|| missing(1))?;
                let high: Decimal = seq.next_element()?.ok_or_else(|| missing(2))?;
                let low: Decimal = seq.next_element()?.ok_or_else(|| missing(3))?;
                let close: Decimal = seq.next_element()?.ok_or_else(|| missing(4))?;
                let volume: Decimal = seq.next_element()?.ok_or_else(|| missing(5))?;
                let close_time: Option<i64> = seq.next_element()?;
                let quote_volume: Option<Decimal> = seq.next_element()?;
                let trades: Option<u64> = seq.next_element()?;
                let taker_buy_base_volume: Option<Decimal> = seq.next_element()?;
                let taker_buy_quote_volume: Option<Decimal> = seq.next_element()?;
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Kline {
                    open_time,
                    open,
                    high,
                    low,
                    close,
                    volume,
                    close_time,
                    quote_volume,
                    trades,
                    taker_buy_base_volume,
                    taker_buy_quote_volume,
                })
            }
        }
        deserializer.deserialize_seq(KlineVisitor)
    }
}

/// Deserialize an `i64` that may arrive as a number or a numeric string.
pub(crate) fn de_i64_flexible<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    value_to_i64(serde_json::Value::deserialize(d)?)
}

/// Optional variant of [`de_i64_flexible`] (`null`/missing → `None`).
pub(crate) fn de_opt_i64_flexible<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<i64>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => Ok(None),
        other => value_to_i64(other).map(Some),
    }
}

/// Binance `/sapi` paging envelope `{"total": n, "rows": [...]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rows<T> {
    #[serde(default)]
    pub total: u64,
    #[serde(default = "Vec::new")]
    pub rows: Vec<T>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn kline_rows_parse_with_and_without_trailing_fields() -> serde_json::Result<()> {
        let full: Kline = serde_json::from_str(
            r#"[1499040000000,"0.01634790","0.80000000","0.01575800","0.01577100","148976.11427815",1499644799999,"2434.19055334",308,"1756.87402397","28.46694368","0"]"#,
        )?;
        assert_eq!(full.open_time, 1_499_040_000_000);
        assert_eq!(full.close, dec!(0.01577100));
        assert_eq!(full.trades, Some(308));
        let short: Kline = serde_json::from_str(r#"[60000,"1","2","0.5","1.5","10"]"#)?;
        assert_eq!(short.ohlcv().volume, dec!(10));
        assert_eq!(short.close_time, None);
        assert!(serde_json::from_str::<Kline>(r#"[1,"1","2"]"#).is_err());
        Ok(())
    }

    #[test]
    fn price_levels_parse_from_pairs() -> serde_json::Result<()> {
        let levels: Vec<PriceLevel> = serde_json::from_str(r#"[["61999.00","0.5"]]"#)?;
        assert_eq!(levels[0].price, dec!(61999));
        assert_eq!(levels[0].quantity, dec!(0.5));
        Ok(())
    }
}

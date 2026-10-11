//! Exchange order payloads consumed by [`super::OrderManager`].
//!
//! Python fed ccxt dictionaries (or raw Binance REST/WebSocket dictionaries)
//! straight into the manager and resolved field aliases on every access. Here
//! the aliases are resolved once, into a typed [`OrderPayload`]:
//! [`OrderPayload::from_json`] accepts the same dictionary shapes, and the
//! exchange adapters build payloads directly from typed Binance responses.

use bb_core::Decimal;
use chrono::{DateTime, NaiveDateTime};
use serde_json::{Map, Value};

use crate::config::parse_decimal;

/// Order update after alias resolution. `None` means "absent from the payload".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderPayload {
    /// `id` / `orderId` / `order_id` / `info.orderId` / `info.id`.
    pub id: Option<String>,
    /// `symbol` / `info.symbol`.
    pub symbol: Option<String>,
    /// `type` / `orderType` / `info.type` (raw spelling).
    pub order_type: Option<String>,
    /// `side` / `info.side` (raw spelling).
    pub side: Option<String>,
    /// First numeric of `amount` / `origQty` / `info.origQty`.
    pub amount: Option<Decimal>,
    /// First positive of `price` / `info.price`.
    pub price: Option<Decimal>,
    /// First numeric of `filled` / `filledAmount` / `executedQty` / `info.executedQty`.
    pub filled: Option<Decimal>,
    /// First numeric of `remaining` / `remainingAmount`.
    pub remaining: Option<Decimal>,
    /// First positive of `average` / `avg_fill_price` / `avgPrice` / `info.avgPrice`.
    pub average: Option<Decimal>,
    /// First non-zero of `cost` / `cummulativeQuoteQty` / `info.cummulativeQuoteQty`.
    pub cost: Option<Decimal>,
    /// `status` / `info.status` (raw spelling, non-empty).
    pub status: Option<String>,
    /// From `timestamp` / `time` / `transactTime` / `info.*`, else `datetime`.
    pub timestamp: Option<NaiveDateTime>,
    /// `trades` or `fills`, in payload order (invalid rows kept so generated
    /// fill ids keep Python's positional index).
    pub fills: Vec<FillPayload>,
}

/// One raw fill row after alias resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FillPayload {
    pub id: Option<String>,
    pub symbol: Option<String>,
    pub side: Option<String>,
    pub price: Option<Decimal>,
    /// `amount` when the key exists, else `qty`.
    pub amount: Option<Decimal>,
    /// `fee.cost` (ccxt) or `commission` (Binance); 0 when absent.
    pub fee: Decimal,
    /// `fee.currency` (ccxt) or `commissionAsset` (Binance); empty when absent.
    pub fee_currency: String,
    pub timestamp: Option<NaiveDateTime>,
}

impl OrderPayload {
    /// Resolve a ccxt-style or raw Binance order dictionary. Non-object
    /// values yield an empty payload (Python treated `None` as `{}`).
    pub fn from_json(value: &Value) -> Self {
        let Some(map) = value.as_object() else {
            return Self::default();
        };
        let info = map.get("info").and_then(Value::as_object);
        let nested = |key: &str| info.and_then(|i| i.get(key));
        let top = |key: &str| map.get(key);

        let id = [
            top("id"),
            top("orderId"),
            top("order_id"),
            nested("orderId"),
            nested("id"),
        ]
        .into_iter()
        .find_map(|v| v.and_then(id_string));
        let symbol = [top("symbol"), nested("symbol")]
            .into_iter()
            .flatten()
            .find(|v| !v.is_null())
            .map(value_string);
        let order_type = [top("type"), top("orderType"), nested("type")]
            .into_iter()
            .flatten()
            .find(|v| !v.is_null())
            .map(value_string);
        let side = [top("side"), nested("side")]
            .into_iter()
            .flatten()
            .find(|v| !v.is_null())
            .map(value_string);
        let amount = first_number(&[top("amount"), top("origQty"), nested("origQty")]);
        let price = first_positive(&[top("price"), nested("price")]);
        let filled = first_number(&[
            top("filled"),
            top("filledAmount"),
            top("executedQty"),
            nested("executedQty"),
        ]);
        let remaining = first_number(&[top("remaining"), top("remainingAmount")]);
        let average = first_positive(&[
            top("average"),
            top("avg_fill_price"),
            top("avgPrice"),
            nested("avgPrice"),
        ]);
        // Python: `cost or cummulativeQuoteQty or info.cummulativeQuoteQty` (0 is falsy).
        let cost = [
            top("cost"),
            top("cummulativeQuoteQty"),
            nested("cummulativeQuoteQty"),
        ]
        .into_iter()
        .find_map(|v| v.and_then(number).filter(|n| !n.is_zero()));
        let status = [top("status"), nested("status")]
            .into_iter()
            .flatten()
            .find(|v| truthy(v))
            .map(value_string);
        let fills = match map
            .get("trades")
            .filter(|v| truthy(v))
            .or_else(|| map.get("fills"))
        {
            Some(Value::Array(rows)) => rows
                .iter()
                .filter_map(Value::as_object)
                .map(fill_from_map)
                .collect(),
            _ => Vec::new(),
        };
        Self {
            id,
            symbol,
            order_type,
            side,
            amount,
            price,
            filled,
            remaining,
            average,
            cost,
            status,
            timestamp: timestamp_from_map(map),
            fills,
        }
    }
}

fn fill_from_map(map: &Map<String, Value>) -> FillPayload {
    let amount = if map.contains_key("amount") {
        map.get("amount").and_then(number)
    } else {
        map.get("qty").and_then(number)
    };
    let (fee, fee_currency) = match map.get("fee") {
        Some(Value::Object(fee)) => (
            fee.get("cost")
                .and_then(number)
                .filter(|n| !n.is_zero())
                .unwrap_or_default(),
            fee.get("currency")
                .filter(|v| !v.is_null())
                .map(value_string)
                .unwrap_or_default(),
        ),
        _ => (
            map.get("commission")
                .and_then(number)
                .filter(|n| !n.is_zero())
                .unwrap_or_default(),
            map.get("commissionAsset")
                .filter(|v| !v.is_null())
                .map(value_string)
                .unwrap_or_default(),
        ),
    };
    FillPayload {
        id: map.get("id").and_then(id_string),
        symbol: map.get("symbol").filter(|v| !v.is_null()).map(value_string),
        side: map.get("side").filter(|v| !v.is_null()).map(value_string),
        price: map.get("price").and_then(number),
        amount,
        fee,
        fee_currency,
        timestamp: timestamp_from_map(map),
    }
}

/// Python `_extract_timestamp` minus the "now" fallback.
fn timestamp_from_map(map: &Map<String, Value>) -> Option<NaiveDateTime> {
    let info = map.get("info").and_then(Value::as_object);
    let candidates = [
        map.get("timestamp"),
        map.get("time"),
        map.get("transactTime"),
        info.and_then(|i| i.get("time")),
        info.and_then(|i| i.get("transactTime")),
    ];
    if let Some(ts) = candidates
        .into_iter()
        .find_map(|v| v.and_then(number).and_then(datetime_from_epoch))
    {
        return Some(ts);
    }
    match map.get("datetime") {
        Some(Value::String(text)) => parse_iso_datetime(text),
        _ => None,
    }
}

/// Python `_datetime_from_timestamp`: values above 1e10 are milliseconds.
pub fn datetime_from_epoch(value: Decimal) -> Option<NaiveDateTime> {
    let seconds = if value > Decimal::from(10_000_000_000_i64) {
        value / Decimal::from(1000)
    } else {
        value
    };
    let micros = (seconds * Decimal::from(1_000_000)).trunc();
    let micros = i64::try_from(micros).ok()?;
    DateTime::from_timestamp_micros(micros).map(|dt| dt.naive_utc())
}

/// Epoch milliseconds → naive UTC.
pub fn datetime_from_ms(ms: i64) -> Option<NaiveDateTime> {
    datetime_from_epoch(Decimal::from(ms))
}

/// Parse Python `datetime.isoformat()` output (naive or offset-aware; offsets
/// are converted to naive UTC like `_extract_timestamp`).
pub fn parse_iso_datetime(text: &str) -> Option<NaiveDateTime> {
    let text = text.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(&text.replace(' ', "T")) {
        return Some(dt.naive_utc());
    }
    let normalized = text.replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f%:z") {
        return Some(dt.naive_utc());
    }
    [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(text, fmt).ok())
    .or_else(|| {
        chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })
}

/// Python `datetime.isoformat()` for naive datetimes (fraction omitted when 0).
pub fn iso_naive(ts: NaiveDateTime) -> String {
    if ts.and_utc().timestamp_subsec_micros() == 0 {
        ts.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        ts.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

/// Python's `str(value).strip()` for id-like values; blanks and null → `None`.
fn id_string(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let text = value_string(value);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn value_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python truthiness for JSON values.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f.abs() > 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `_optional_float`: numbers or numeric strings; anything else → `None`.
fn number(value: &Value) -> Option<Decimal> {
    match value {
        Value::Number(n) => parse_decimal(&n.to_string()),
        Value::String(s) if !s.is_empty() => parse_decimal(s),
        _ => None,
    }
}

fn first_number(candidates: &[Option<&Value>]) -> Option<Decimal> {
    candidates.iter().find_map(|v| v.and_then(number))
}

fn first_positive(candidates: &[Option<&Value>]) -> Option<Decimal> {
    candidates
        .iter()
        .find_map(|v| v.and_then(number).filter(|n| *n > Decimal::ZERO))
}

/// Python `f"{value:g}"` (6 significant digits, trailing zeros stripped,
/// exponent form below 1e-4 or from 1e6). Used for generated fill ids so ids
/// match rows persisted by the Python runtime.
pub fn format_g(value: Decimal) -> String {
    let v = bb_core::money::to_f64(value);
    if value.is_zero() {
        return "0".to_owned();
    }
    if !v.is_finite() {
        return v.to_string();
    }
    const PRECISION: i32 = 6;
    let sci = format!("{:.*e}", (PRECISION - 1) as usize, v);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..PRECISION).contains(&exp) {
        let mantissa = strip_zeros(mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    } else {
        let decimals = usize::try_from(PRECISION - 1 - exp).unwrap_or(0);
        strip_zeros(&format!("{v:.decimals$}")).to_owned()
    }
}

fn strip_zeros(text: &str) -> &str {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    #[test]
    fn format_g_matches_python() {
        assert_eq!(format_g(dec!(0.001)), "0.001");
        assert_eq!(format_g(dec!(82000)), "82000");
        assert_eq!(format_g(dec!(82396.84)), "82396.8");
        assert_eq!(format_g(dec!(1234567)), "1.23457e+06");
        assert_eq!(format_g(dec!(0.00001)), "1e-05");
        assert_eq!(format_g(dec!(0.5)), "0.5");
        assert_eq!(format_g(dec!(2)), "2");
        assert_eq!(format_g(dec!(0)), "0");
    }

    #[test]
    fn iso_round_trip_matches_python_isoformat() {
        let ts = parse_iso_datetime("2026-05-11T18:27:00.970409").unwrap();
        assert_eq!(iso_naive(ts), "2026-05-11T18:27:00.970409");
        let whole = parse_iso_datetime("2026-05-11T18:27:00").unwrap();
        assert_eq!(iso_naive(whole), "2026-05-11T18:27:00");
        let aware = parse_iso_datetime("2026-05-11T20:27:00+02:00").unwrap();
        assert_eq!(iso_naive(aware), "2026-05-11T18:27:00");
        assert_eq!(
            iso_naive(parse_iso_datetime("2026-05-11T18:27:00Z").unwrap()),
            "2026-05-11T18:27:00"
        );
    }

    #[test]
    fn raw_binance_payload_aliases_are_resolved() {
        let payload = OrderPayload::from_json(&json!({
            "orderId": 123,
            "symbol": "BTC/USDT",
            "type": "LIMIT",
            "side": "BUY",
            "origQty": "0.5",
            "price": "0",
            "executedQty": "0.25",
            "cummulativeQuoteQty": "0",
            "status": "PARTIALLY_FILLED",
            "transactTime": 1_700_000_000_000_i64,
            "info": {"price": "100", "cummulativeQuoteQty": "25"},
            "fills": [{"price": "100", "qty": "0.25", "commission": "0.1", "commissionAsset": "BNB", "tradeId": 9}],
        }));
        assert_eq!(payload.id.as_deref(), Some("123"));
        assert_eq!(payload.price, Some(dec!(100)));
        assert_eq!(payload.amount, Some(dec!(0.5)));
        assert_eq!(payload.filled, Some(dec!(0.25)));
        assert_eq!(payload.cost, Some(dec!(25)));
        assert_eq!(payload.status.as_deref(), Some("PARTIALLY_FILLED"));
        assert_eq!(
            payload.timestamp.map(|t| t.and_utc().timestamp()),
            Some(1_700_000_000)
        );
        assert_eq!(payload.fills.len(), 1);
        assert_eq!(payload.fills[0].amount, Some(dec!(0.25)));
        assert_eq!(payload.fills[0].fee, dec!(0.1));
        assert_eq!(payload.fills[0].fee_currency, "BNB");
        assert_eq!(payload.fills[0].id, None);
    }

    #[test]
    fn ccxt_fee_mapping_and_empty_status_are_handled() {
        let payload = OrderPayload::from_json(&json!({
            "id": "  ",
            "info": {"id": "abc"},
            "status": "",
            "trades": [{"id": "t1", "price": 10, "amount": 1, "fee": {"cost": 0.01, "currency": "USDT"}}],
        }));
        assert_eq!(payload.id.as_deref(), Some("abc"));
        assert_eq!(payload.status, None);
        assert_eq!(payload.fills[0].fee, dec!(0.01));
        assert_eq!(payload.fills[0].fee_currency, "USDT");
        assert_eq!(
            OrderPayload::from_json(&Value::Null),
            OrderPayload::default()
        );
    }
}

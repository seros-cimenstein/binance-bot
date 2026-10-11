//! Small helpers shared by the product modules (JSON parsing, number
//! serialisation, timestamps, audit appends).

use std::path::{Path, PathBuf};
use std::str::FromStr;

use bb_core::audit::{AuditError, JsonlLog};
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

use crate::settings::Settings;

/// Serialise `Decimal` as a JSON number (Python wrote floats).
pub(crate) mod num {
    pub use rust_decimal::serde::arbitrary_precision::*;
}

/// Serialise `Option<Decimal>` as a JSON number or `null`.
pub(crate) mod num_opt {
    pub use rust_decimal::serde::arbitrary_precision_option::*;
}

/// Python `datetime.now(timezone.utc).isoformat()` (microseconds, `+00:00`).
pub fn utc_timestamp() -> String {
    iso(bb_core::time::now())
}

/// Python `datetime.isoformat()` for an aware UTC timestamp.
pub fn iso(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Micros, false)
}

/// Python `datetime.fromisoformat` subset (`Z` accepted); naive values are UTC.
pub fn parse_iso_timestamp(value: &str) -> Option<DateTime<Utc>> {
    let text = value.trim().replace('Z', "+00:00");
    if let Ok(parsed) = DateTime::parse_from_rfc3339(&text) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M%:z",
    ] {
        if let Ok(parsed) = DateTime::parse_from_str(&text, format) {
            return Some(parsed.with_timezone(&Utc));
        }
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(&text, format) {
            return Some(parsed.and_utc());
        }
    }
    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc())
}

/// `DRY_RUN` / `testnet` / `LIVE` (Python `_mode`).
pub fn mode(settings: &Settings) -> &'static str {
    if settings.dry_run {
        "DRY_RUN"
    } else if settings.use_testnet {
        "testnet"
    } else {
        "LIVE"
    }
}

/// Strip separators: `BNB/USDT` → `BNBUSDT`.
pub fn exchange_symbol(symbol: &str) -> String {
    symbol
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_uppercase()
}

pub const STABLE_ASSETS: [&str; 6] = ["USDT", "USDC", "FDUSD", "BUSD", "TUSD", "DAI"];

pub fn is_stable_asset(asset: &str) -> bool {
    STABLE_ASSETS.contains(&asset.trim().to_uppercase().as_str())
}

/// Parse a JSON number or numeric string (Python `float(value)` semantics, finite only).
pub fn jdec(value: Option<&Value>) -> Option<Decimal> {
    let text = match value? {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_owned(),
        _ => return None,
    };
    Decimal::from_str(&text)
        .ok()
        .or_else(|| Decimal::from_scientific(&text).ok())
}

/// `_float`: number or zero.
pub fn jfloat(value: Option<&Value>) -> Decimal {
    jdec(value).unwrap_or(Decimal::ZERO)
}

/// `_non_negative_float`: positive number or zero.
pub fn non_negative(value: Option<&Value>) -> Decimal {
    jdec(value)
        .filter(|d| *d > Decimal::ZERO)
        .unwrap_or(Decimal::ZERO)
}

/// `_optional_positive_float`.
pub fn optional_positive(value: Option<&Value>) -> Option<Decimal> {
    jdec(value).filter(|d| *d > Decimal::ZERO)
}

/// `_optional_bool`: only real JSON booleans.
pub fn jbool(value: Option<&Value>) -> Option<bool> {
    value.and_then(Value::as_bool)
}

/// Python `str(value or "")` for scalar JSON values.
pub fn jtext(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(true)) => "True".to_owned(),
        _ => String::new(),
    }
}

/// Python `bool(value)` for JSON values.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => !jdec(Some(&Value::Number(n.clone()))).is_some_and(|d| d.is_zero()),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Items of a JSON array, or the object itself as a single item.
pub fn rows(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => Vec::new(),
    }
}

/// Pretty JSON (Python `json.dumps(..., indent=2, sort_keys=True)`).
pub fn print_json<T: Serialize + ?Sized>(value: &T) {
    match serde_json::to_value(value).and_then(|v| serde_json::to_string_pretty(&sort_keys(v))) {
        Ok(text) => println!("{text}"),
        Err(err) => println!("{{\"error\": \"{err}\"}}"),
    }
}

/// Recursively sort object keys (independent of serde_json's map feature).
pub fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sort_keys(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Append one sorted-key JSON line to an audit file, creating parents.
pub fn append_audit<T: Serialize + ?Sized>(path: &Path, record: &T) -> Result<PathBuf, AuditError> {
    let value = sort_keys(serde_json::to_value(record)?);
    JsonlLog::new(path).append(&value)?;
    Ok(path.to_path_buf())
}

/// Format a decimal without trailing zeros (`10.500` → `10.5`).
pub fn fmt_dec(value: Decimal) -> String {
    value.normalize().to_string()
}

pub fn fmt_opt(value: Option<Decimal>) -> String {
    value.map_or_else(|| "unknown".to_owned(), fmt_dec)
}

pub fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_numbers_and_strings() {
        assert_eq!(jdec(Some(&json!("3.2"))), Some(dec!(3.2)));
        assert_eq!(jdec(Some(&json!(5))), Some(dec!(5)));
        assert_eq!(jdec(Some(&json!("1e-5"))), Some(dec!(0.00001)));
        assert_eq!(jdec(Some(&json!(true))), None);
        assert_eq!(non_negative(Some(&json!("-1"))), Decimal::ZERO);
        assert_eq!(optional_positive(Some(&json!("0"))), None);
        assert_eq!(exchange_symbol("bnb/usdt"), "BNBUSDT");
        assert!(is_stable_asset(" usdc "));
        assert!(truthy(&json!(1)));
        assert!(!truthy(&json!(0)));
    }

    #[test]
    fn timestamp_matches_python_isoformat_shape() {
        let ts = utc_timestamp();
        assert!(ts.ends_with("+00:00"), "{ts}");
        assert_eq!(ts.len(), "2026-01-01T00:00:00.000000+00:00".len());
    }
}

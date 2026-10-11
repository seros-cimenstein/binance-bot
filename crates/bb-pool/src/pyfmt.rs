//! Python-compatible formatting helpers so persisted values and CLI output keep the
//! exact shapes the Python implementation produced (`datetime.isoformat()`,
//! `json.dumps`, `%g`, float-looking JSON numbers).

use std::str::FromStr;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Timelike, Utc};
use rust_decimal::Decimal;
use serde_json::{Map, Number, Value};

/// `datetime.isoformat()` for an aware UTC timestamp: microseconds only when non-zero,
/// offset spelled `+00:00`.
pub fn py_isoformat(ts: DateTime<Utc>) -> String {
    if ts.nanosecond() / 1_000 == 0 {
        ts.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// `datetime.fromisoformat(value.replace("Z", "+00:00"))`; naive values are UTC.
pub fn parse_py_iso(value: &str) -> Option<DateTime<Utc>> {
    let text = value.trim();
    let text = text
        .strip_suffix('Z')
        .map_or_else(|| text.to_owned(), |t| format!("{t}+00:00"));
    if let Ok(ts) = DateTime::parse_from_rfc3339(&text) {
        return Some(ts.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M%:z",
    ] {
        if let Ok(ts) = DateTime::parse_from_str(&text, format) {
            return Some(ts.with_timezone(&Utc));
        }
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(ts) = NaiveDateTime::parse_from_str(&text, format) {
            return Some(ts.and_utc());
        }
    }
    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|ts| ts.and_utc())
}

/// Python `format(value, ".{precision}g")`.
pub fn fmt_g(value: f64, precision: usize) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let precision = precision.max(1);
    let scientific = format!("{:.*e}", precision - 1, value);
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let max_exponent = i64::try_from(precision).unwrap_or(i64::MAX);
    if exponent < -4 || exponent >= max_exponent {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip_zeros(mantissa), exponent.abs())
    } else {
        let decimals = usize::try_from(max_exponent - 1 - exponent).unwrap_or(0);
        strip_zeros(&format!("{value:.decimals$}")).to_owned()
    }
}

fn strip_zeros(text: &str) -> &str {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        text
    }
}

/// Decimal from a SQLite REAL / f64 via its shortest round-trip spelling (Python `repr`).
pub fn decimal_from_f64(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    Decimal::from_str(&value.to_string())
        .ok()
        .or_else(|| bb_core::money::from_f64(value))
}

/// JSON number for a decimal, spelled like a Python float (`10.0`, `0.025`).
pub fn decimal_number(value: Decimal) -> Number {
    let mut text = value.normalize().to_string();
    if !text.contains('.') {
        text.push_str(".0");
    }
    Number::from_str(&text).unwrap_or_else(|_| Number::from(0))
}

/// `json.dumps(value, sort_keys=sort_keys)` with Python's default `", "` / `": "`
/// separators and `ensure_ascii=True`.
pub fn py_json_dumps(value: &Value, sort_keys: bool) -> String {
    let mut out = String::new();
    write_py_json(value, sort_keys, &mut out);
    out
}

fn write_py_json(value: &Value, sort_keys: bool, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_ascii_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_py_json(item, sort_keys, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_ascii_string(key, out);
                out.push_str(": ");
                write_py_json(item, sort_keys, out);
            }
            out.push('}');
        }
    }
}

fn write_ascii_string(text: &str, out: &mut String) {
    use std::fmt::Write as _;
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Recursively sort object keys (Python `sort_keys=True`) regardless of serde_json features.
pub fn sort_keys(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = Map::new();
            for (key, item) in entries {
                sorted.insert(key, sort_keys(item));
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

/// `json.dumps(value, indent=2, sort_keys=True)` for any serializable value.
pub fn pretty_sorted<T: serde::Serialize>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&sort_keys(serde_json::to_value(value)?))
}

/// Serde adapters that emit decimals as JSON numbers (Python floats) instead of strings.
pub mod num {
    use std::collections::BTreeMap;

    use rust_decimal::Decimal;
    use serde::Serializer;
    use serde::ser::{Serialize, SerializeMap};

    use super::decimal_number;

    struct Num(Decimal);

    impl Serialize for Num {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            decimal_number(self.0).serialize(serializer)
        }
    }

    pub fn decimal<S: Serializer>(value: &Decimal, serializer: S) -> Result<S::Ok, S::Error> {
        Num(*value).serialize(serializer)
    }

    pub fn opt_decimal<S: Serializer>(
        value: &Option<Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(Num).serialize(serializer)
    }

    pub fn map<S: Serializer>(
        value: &BTreeMap<String, Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_map(Some(value.len()))?;
        for (key, item) in value {
            out.serialize_entry(key, &Num(*item))?;
        }
        out.end()
    }

    pub fn pairs<S: Serializer>(
        value: &[(String, Decimal)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_map(Some(value.len()))?;
        for (key, item) in value {
            out.serialize_entry(key, &Num(*item))?;
        }
        out.end()
    }

    pub fn nested_map<S: Serializer>(
        value: &BTreeMap<String, BTreeMap<String, Decimal>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        struct Inner<'a>(&'a BTreeMap<String, Decimal>);
        impl Serialize for Inner<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                map(self.0, serializer)
            }
        }
        let mut out = serializer.serialize_map(Some(value.len()))?;
        for (key, item) in value {
            out.serialize_entry(key, &Inner(item))?;
        }
        out.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;
    use serde_json::json;

    #[test]
    fn isoformat_matches_python() {
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 25, 0, 15, 0)
            .single()
            .unwrap_or_default();
        assert_eq!(py_isoformat(ts), "2026-05-25T00:15:00+00:00");
        let micro = ts + chrono::Duration::microseconds(515_888);
        assert_eq!(py_isoformat(micro), "2026-05-25T00:15:00.515888+00:00");
        assert_eq!(
            parse_py_iso("2026-05-25T00:15:00.515888+00:00"),
            Some(micro)
        );
        assert_eq!(parse_py_iso("2026-05-25T00:15:00Z"), Some(ts));
        assert_eq!(parse_py_iso("2026-05-25T00:15"), Some(ts));
        assert!(parse_py_iso("2026-05-25").is_some());
        assert!(parse_py_iso("nope").is_none());
    }

    #[test]
    fn g_format_matches_python() {
        assert_eq!(fmt_g(12.47875391, 12), "12.47875391");
        assert_eq!(fmt_g(0.025, 12), "0.025");
        assert_eq!(fmt_g(0.0, 12), "0");
        assert_eq!(fmt_g(1e-05, 12), "1e-05");
        assert_eq!(fmt_g(123_456_789_012_345.0, 12), "1.23456789012e+14");
        assert_eq!(fmt_g(0.20000000000000007, 6), "0.2");
        assert_eq!(fmt_g(-0.000123456789, 6), "-0.000123457");
        assert_eq!(fmt_g(1_000_000.0, 6), "1e+06");
    }

    #[test]
    fn python_json_dumps_shape() {
        let value = json!([{"b": 1, "a": "é"}]);
        assert_eq!(py_json_dumps(&value, true), r#"[{"a": "\u00e9", "b": 1}]"#);
        assert_eq!(py_json_dumps(&json!(["x", "y"]), false), r#"["x", "y"]"#);
        assert_eq!(py_json_dumps(&json!([]), false), "[]");
    }

    #[test]
    fn decimal_numbers_look_like_floats() {
        assert_eq!(decimal_number(dec!(10)).to_string(), "10.0");
        assert_eq!(decimal_number(dec!(0.02500)).to_string(), "0.025");
        assert_eq!(decimal_from_f64(12.47875391), Some(dec!(12.47875391)));
    }
}

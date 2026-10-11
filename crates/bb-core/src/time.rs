//! UTC time helpers.

use chrono::{DateTime, Utc};

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// RFC 3339 with millisecond precision and `Z` suffix, e.g. `2026-10-10T12:00:00.000Z`.
pub fn iso(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn from_ms(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms)
}

/// `YYYY-MM-DD` of the UTC day containing `ts`.
pub fn utc_date(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d").to_string()
}

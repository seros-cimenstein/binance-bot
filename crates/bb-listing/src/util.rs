//! Small shared helpers.

use std::path::Path;

use bb_binance::BinanceError;
use bb_core::audit::JsonlLog;
use chrono::{DateTime, Timelike, Utc};
use serde::Serialize;

use crate::error::Result;

/// Python `datetime.isoformat()` for an aware UTC datetime:
/// `2026-05-14T10:00:03+00:00`, with `.ffffff` only when microseconds are non-zero.
/// Every timestamp this crate writes uses it so existing data files stay uniform.
pub fn py_isoformat(ts: DateTime<Utc>) -> String {
    if ts.nanosecond() / 1_000 == 0 {
        ts.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// `py_isoformat(now)`.
pub fn timestamp() -> String {
    py_isoformat(bb_core::time::now())
}

/// Durably append one JSON record to the JSONL file at `path`.
pub fn append_jsonl<T: Serialize + ?Sized>(path: &Path, record: &T) -> Result<()> {
    Ok(JsonlLog::new(path).append(record)?)
}

/// First `max` characters of `text`.
pub fn truncate(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .map_or(text, |(idx, _)| &text[..idx])
}

/// `"{ErrorKind}: {message}"` capped at 200 characters of message, the
/// equivalent of Python's `f"{type(exc).__name__}: {str(exc)[:200]}"`.
pub fn describe(err: &BinanceError) -> String {
    format!("{:?}: {}", err.kind(), truncate(&err.to_string(), 200))
}

/// `Duration` from (possibly fractional) seconds, clamped at zero.
pub fn seconds(secs: f64) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn isoformat_matches_python() {
        let t = Utc.with_ymd_and_hms(2026, 5, 14, 10, 0, 3).unwrap();
        assert_eq!(py_isoformat(t), "2026-05-14T10:00:03+00:00");
        let t = t + chrono::TimeDelta::microseconds(123_000);
        assert_eq!(py_isoformat(t), "2026-05-14T10:00:03.123000+00:00");
    }

    #[test]
    fn truncate_counts_chars() {
        assert_eq!(truncate("ⓈⓈⓈ", 2), "ⓈⓈ");
        assert_eq!(truncate("ab", 5), "ab");
    }
}

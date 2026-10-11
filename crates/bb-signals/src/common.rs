//! Shared utilities for signal detectors: clock, ledgers, dedupe, rate limits.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::error::SignalsError;
use crate::jsonv::{Object, sort_keys};

/// Wall clock in epoch milliseconds; injectable for deterministic tests.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        bb_core::time::now_ms()
    }
}

/// Clock frozen at a fixed instant (tests, replays).
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// Append one record as a key-sorted JSON line (durably).
pub fn append_ledger<T: Serialize + ?Sized>(path: &Path, record: &T) -> Result<(), SignalsError> {
    let value = sort_keys(serde_json::to_value(record)?);
    bb_core::audit::JsonlLog::new(path).append(&value)?;
    Ok(())
}

/// Atomically replace `path` with pretty, key-sorted JSON.
pub fn atomic_write_json<T: Serialize + ?Sized>(
    path: &Path,
    payload: &T,
) -> Result<(), SignalsError> {
    let value = sort_keys(serde_json::to_value(payload)?);
    bb_core::fsutil::write_json_atomic(path, &value).map_err(|e| SignalsError::io(path, e))
}

/// Read a JSON file; `None` when missing, unreadable or malformed.
pub fn read_json_lenient(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// All object rows of a JSONL file. Missing file = empty; unreadable file and
/// malformed rows are logged and skipped.
pub fn load_jsonl_objects(path: &Path) -> Vec<Object> {
    match bb_core::audit::JsonlLog::new(path).read_all::<Value>() {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|row| match row {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .collect(),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "unable to read JSONL file");
            Vec::new()
        }
    }
}

/// Operator emergency stop: presence of the file halts the detector loop.
pub fn emergency_stop_active(path: &Path) -> bool {
    bb_core::fsutil::stop_requested(path)
}

/// Insertion-ordered set of seen token addresses, trimmed oldest-first at `capacity`.
#[derive(Debug, Clone)]
pub struct SeenAddresses {
    capacity: usize,
    order: VecDeque<String>,
    members: HashSet<String>,
}

impl SeenAddresses {
    /// Capacity used by the V1 DexScreener detector.
    pub const DEFAULT_CAPACITY: usize = 10_000;

    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            members: HashSet::new(),
        }
    }

    /// Record `addresses`, returning those not seen before (in input order).
    pub fn dedupe<I, S>(&mut self, addresses: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut fresh = Vec::new();
        for address in addresses {
            let address = address.as_ref();
            if self.members.contains(address) {
                continue;
            }
            self.members.insert(address.to_owned());
            self.order.push_back(address.to_owned());
            fresh.push(address.to_owned());
            while self.order.len() > self.capacity {
                if let Some(oldest) = self.order.pop_front() {
                    self.members.remove(&oldest);
                }
            }
        }
        fresh
    }

    pub fn contains(&self, address: &str) -> bool {
        self.members.contains(address)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Rolling one-hour limiter for detector-local Telegram alerts.
#[derive(Debug, Clone)]
pub struct AlertRateLimiter {
    max_per_hour: usize,
    sent_at_ms: VecDeque<i64>,
}

impl AlertRateLimiter {
    pub fn new(max_per_hour: usize) -> Self {
        Self {
            max_per_hour,
            sent_at_ms: VecDeque::new(),
        }
    }

    pub fn allow(&mut self, now_ms: i64) -> bool {
        let cutoff = now_ms - 3_600_000;
        while self.sent_at_ms.front().is_some_and(|ts| *ts < cutoff) {
            self.sent_at_ms.pop_front();
        }
        if self.sent_at_ms.len() >= self.max_per_hour {
            return false;
        }
        self.sent_at_ms.push_back(now_ms);
        true
    }
}

/// Python `datetime.fromtimestamp(ms / 1000, tz=utc).isoformat()`:
/// `2026-06-27T12:00:00+00:00`, with microseconds only when non-zero.
pub fn iso_from_ms(ms: i64) -> String {
    let Some(ts) = DateTime::<Utc>::from_timestamp_millis(ms) else {
        return String::new();
    };
    let fmt = if ms.rem_euclid(1000) == 0 {
        "%Y-%m-%dT%H:%M:%S+00:00"
    } else {
        "%Y-%m-%dT%H:%M:%S%.6f+00:00"
    };
    ts.format(fmt).to_string()
}

/// Python `datetime.fromisoformat` → epoch ms; naive values are taken as UTC.
pub fn parse_iso_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.timestamp_millis());
    }
    let normalized = value.replacen(' ', "T", 1);
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.timestamp_millis());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z"] {
        if let Ok(dt) = DateTime::parse_from_str(&normalized, fmt) {
            return Some(dt.timestamp_millis());
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&normalized, fmt) {
            return Some(dt.and_utc().timestamp_millis());
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp_millis())
}

/// RFC 3339 rendering for logs.
pub fn iso_millis(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|ts| ts.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Shared HTTP client with a total request timeout.
pub fn http_client(timeout: Duration) -> Result<reqwest::Client, SignalsError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("bb-signals/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| SignalsError::http("<client builder>", e))
}

/// Resolves on SIGINT or SIGTERM (daemon shutdown).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                tracing::warn!(%err, "cannot install SIGTERM handler; only Ctrl+C stops the daemon");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Seconds (possibly fractional, from env) → `Duration`, clamping negatives to zero.
pub fn secs(value: f64) -> Duration {
    Duration::try_from_secs_f64(value.max(0.0)).unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn append_ledger_writes_sorted_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ledger.jsonl");
        append_ledger(&path, &json!({"b": 2, "a": 1})).unwrap();
        append_ledger(&path, &json!({"c": 3})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec![r#"{"a":1,"b":2}"#, r#"{"c":3}"#]);
    }

    #[test]
    fn atomic_write_json_replaces_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write_json(&path, &json!({"old": true})).unwrap();
        atomic_write_json(&path, &json!({"new": [1, 2, 3]})).unwrap();
        assert_eq!(read_json_lenient(&path), Some(json!({"new": [1, 2, 3]})));
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn emergency_stop_follows_file_presence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stop");
        assert!(!emergency_stop_active(&path));
        std::fs::write(&path, b"").unwrap();
        assert!(emergency_stop_active(&path));
    }

    #[test]
    fn dedupe_mutates_seen_and_trims_oldest() {
        let mut seen = SeenAddresses::new(SeenAddresses::DEFAULT_CAPACITY);
        seen.dedupe((0..9_999).map(|i| format!("old-{i}")));
        let fresh = seen.dedupe(["old-1", "new-1", "new-2"]);
        assert_eq!(fresh, vec!["new-1", "new-2"]);
        assert!(seen.contains("new-1") && seen.contains("new-2"));
        assert_eq!(seen.len(), 10_000);
        assert!(!seen.contains("old-0"));
    }

    #[test]
    fn rate_limiter_rolls_over_one_hour() {
        let mut limiter = AlertRateLimiter::new(2);
        assert!(limiter.allow(10_000));
        assert!(limiter.allow(20_000));
        assert!(!limiter.allow(30_000));
        assert!(limiter.allow(10_000 + 3_600_001));
    }

    #[test]
    fn iso_round_trips_python_format() {
        assert_eq!(iso_from_ms(2_000_000), "1970-01-01T00:33:20+00:00");
        assert_eq!(iso_from_ms(2_000_123), "1970-01-01T00:33:20.123000+00:00");
        assert_eq!(
            parse_iso_ms("1970-01-01T00:33:20.123000+00:00"),
            Some(2_000_123)
        );
        assert_eq!(parse_iso_ms("1970-01-01T00:33:20"), Some(2_000_000));
        assert_eq!(parse_iso_ms("garbage"), None);
    }
}

//! Shared TTL watchlist (`data/signals-watchlist.json`), `{"watching": [...]}`.
//!
//! Entries are kept as JSON objects so hand-edited or foreign rows survive a
//! rewrite unchanged; detectors only add rows built from [`SignalCandidate`].

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::candidate::SignalCandidate;
use crate::common::{atomic_write_json, iso_from_ms, parse_iso_ms, read_json_lenient};
use crate::error::SignalsError;
use crate::jsonv::{Object, objects, py_str};

#[derive(Debug, Clone)]
pub struct Watchlist {
    path: PathBuf,
    entries: Vec<Object>,
}

impl Watchlist {
    /// Load the watchlist; missing/malformed files start empty.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = match read_json_lenient(&path) {
            Some(Value::Object(mut root)) => match root.remove("watching") {
                Some(Value::Array(items)) => objects(items),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        Self { path, entries }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[Object] {
        &self.entries
    }

    pub fn add(&mut self, candidate: &SignalCandidate, ttl_hours: f64) {
        let detected_at_ms = candidate.detected_at_ms;
        // Python int(): truncation of the fractional-millisecond TTL is intended.
        let expires_at_ms = detected_at_ms + (ttl_hours * 3_600_000.0) as i64;
        let entry = json!({
            "token": candidate.token_address,
            "chain": candidate.chain,
            "source": candidate.source,
            "detected_at_iso": iso_from_ms(detected_at_ms),
            "expires_at_iso": iso_from_ms(expires_at_ms),
        });
        if let Value::Object(map) = entry {
            self.entries.push(map);
        }
    }

    /// Drop entries whose `expires_at_iso` is at or before `now_ms`; entries
    /// without a parseable expiry are kept.
    pub fn prune(&mut self, now_ms: i64) {
        self.entries.retain(|item| {
            parse_iso_ms(&py_str(item.get("expires_at_iso"))).is_none_or(|exp| exp > now_ms)
        });
    }

    pub fn save(&self) -> Result<(), SignalsError> {
        atomic_write_json(&self.path, &json!({ "watching": self.entries }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(at: i64) -> SignalCandidate {
        SignalCandidate {
            source: "dexscreener".into(),
            chain: "solana".into(),
            token_address: "Tok".into(),
            token_symbol: String::new(),
            detected_at_ms: at,
            raw_signal: Object::new(),
            confidence: 0.5,
        }
    }

    #[test]
    fn add_prune_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        let mut wl = Watchlist::load(&path);
        wl.add(&candidate(0), 1.0);
        wl.add(&candidate(10_000_000), 1.0);
        wl.prune(3_600_000);
        assert_eq!(wl.entries().len(), 1);
        wl.save().unwrap();
        let reloaded = Watchlist::load(&path);
        assert_eq!(reloaded.entries(), wl.entries());
        assert_eq!(
            reloaded.entries()[0]["expires_at_iso"],
            json!("1970-01-01T03:46:40+00:00")
        );
    }

    #[test]
    fn malformed_file_loads_empty_and_unknown_rows_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        std::fs::write(&path, "{").unwrap();
        assert!(Watchlist::load(&path).entries().is_empty());
        std::fs::write(&path, r#"{"watching": [{"token": "X", "note": 1}, 5]}"#).unwrap();
        let mut wl = Watchlist::load(&path);
        wl.prune(i64::MAX);
        assert_eq!(wl.entries().len(), 1);
    }
}

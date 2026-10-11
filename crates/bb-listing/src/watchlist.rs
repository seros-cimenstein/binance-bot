//! Shared watchlist of upcoming Binance spot listings.
//!
//! The detector appends one entry per token of every fresh `spot_list`
//! announcement; each sniper reads it every cycle and pre-arms
//! `{TOKEN}{QUOTE}` so it fires the moment the pair turns TRADING, even when
//! the symbol was already captured by its bootstrap snapshot.
//!
//! File shape (unchanged from the Python implementation):
//!
//! ```json
//! {"watching": [
//!   {"token": "AIGENSYN", "article_id": 274005,
//!    "announced_at_iso": "2026-05-14T10:00:03+00:00",
//!    "expires_at_iso":   "2026-05-21T10:00:03+00:00"}
//! ]}
//! ```
//!
//! Entries expire [`DEFAULT_EXPIRY_DAYS`] after the announcement and are
//! pruned whenever the file is loaded.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ListingError, Result};
use crate::util::py_isoformat;

pub const DEFAULT_EXPIRY_DAYS: i64 = 7;
const DEFAULT_RELATIVE_PATH: &str = ".local/share/binance-bot/data/listing-watchlist.json";

/// `~/.local/share/binance-bot/data/listing-watchlist.json` (the library default;
/// both runtimes override it with `LISTING_WATCHLIST_PATH`, default `data/listing-watchlist.json`).
pub fn default_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(DEFAULT_RELATIVE_PATH)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntry {
    pub token: String,
    pub article_id: i64,
    pub announced_at_iso: String,
    pub expires_at_iso: String,
}

impl WatchEntry {
    /// Entry for `token` announced at `announced_at_ms`, expiring `expiry_days` later.
    pub fn new(token: &str, article_id: i64, announced_at_ms: i64, expiry_days: i64) -> Self {
        let announced = DateTime::from_timestamp_millis(announced_at_ms).unwrap_or_default();
        let expires = announced + TimeDelta::days(expiry_days);
        Self {
            token: token.to_owned(),
            article_id,
            announced_at_iso: py_isoformat(announced),
            expires_at_iso: py_isoformat(expires),
        }
    }

    fn from_value(value: &serde_json::Map<String, Value>) -> Result<Self> {
        let field = |name: &str| {
            value.get(name).ok_or_else(|| {
                ListingError::Watchlist(format!("watch entry missing field: '{name}'"))
            })
        };
        let text = |v: &Value| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let article = field("article_id")?;
        let article_id = match article {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
        .ok_or_else(|| {
            ListingError::Watchlist(format!(
                "watch entry article_id is not an integer: {article}"
            ))
        })?;
        Ok(Self {
            token: text(field("token")?),
            article_id,
            announced_at_iso: text(field("announced_at_iso")?),
            expires_at_iso: text(field("expires_at_iso")?),
        })
    }

    /// Unparseable expiry timestamps never expire (Python semantics).
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        parse_iso(&self.expires_at_iso).is_some_and(|expires| now >= expires)
    }
}

/// Python `datetime.fromisoformat` subset: RFC 3339 with offset, or naive (treated as UTC).
fn parse_iso(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Watchlist {
    #[serde(rename = "watching")]
    pub entries: Vec<WatchEntry>,
}

impl Watchlist {
    /// Lenient parse: a missing/non-list `watching` is empty and non-object
    /// rows are skipped, but an object row missing a field is an error.
    pub fn from_value(value: &Value) -> Result<Self> {
        let Some(rows) = value.get("watching").and_then(Value::as_array) else {
            return Ok(Self::default());
        };
        let entries = rows
            .iter()
            .filter_map(Value::as_object)
            .map(WatchEntry::from_value)
            .collect::<Result<_>>()?;
        Ok(Self { entries })
    }

    pub fn article_ids(&self) -> BTreeSet<i64> {
        self.entries.iter().map(|e| e.article_id).collect()
    }

    /// Append unless an entry with the same `article_id` and `token` exists; `true` if added.
    /// (Python keyed on `article_id` alone, which silently dropped every token
    /// after the first of a multi-token announcement.)
    pub fn add(&mut self, entry: WatchEntry) -> bool {
        if self
            .entries
            .iter()
            .any(|e| e.article_id == entry.article_id && e.token == entry.token)
        {
            return false;
        }
        self.entries.push(entry);
        true
    }

    /// Drop every entry for `token`; returns the count removed.
    pub fn remove_token(&mut self, token: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.token != token);
        before - self.entries.len()
    }

    /// Drop expired entries; returns the count removed.
    pub fn prune_expired(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| !e.is_expired(now));
        before - self.entries.len()
    }

    /// Load `path` (missing file = empty), pruning expired entries against `now` when `prune`.
    pub fn load(path: &Path, prune: bool, now: DateTime<Utc>) -> Result<Self> {
        let unreadable = |err: &dyn std::fmt::Display| {
            ListingError::Watchlist(format!(
                "could not read watchlist at {}: {err}",
                path.display()
            ))
        };
        let value: Value = match bb_core::fsutil::read_json(path) {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(Self::default()),
            Err(err) => return Err(unreadable(&err)),
        };
        let mut watchlist = Self::from_value(&value)?;
        if prune {
            watchlist.prune_expired(now);
        }
        Ok(watchlist)
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        bb_core::fsutil::write_json_atomic(path, self).map_err(|source| ListingError::Write {
            path: path.to_owned(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn entry(token: &str, article_id: i64, days_ago: i64, expiry_days: i64) -> WatchEntry {
        let announced = now() - TimeDelta::days(days_ago);
        WatchEntry::new(token, article_id, announced.timestamp_millis(), expiry_days)
    }

    #[test]
    fn build_entry_sets_expiry_to_announce_plus_default_days() {
        let e = WatchEntry::new("FOO", 42, now().timestamp_millis(), DEFAULT_EXPIRY_DAYS);
        let a = parse_iso(&e.announced_at_iso).unwrap();
        let x = parse_iso(&e.expires_at_iso).unwrap();
        assert_eq!((x - a).num_days(), DEFAULT_EXPIRY_DAYS);
        assert_eq!(e.announced_at_iso, "2026-06-01T12:00:00+00:00");
    }

    #[test]
    fn add_rejects_duplicate_article_id() {
        let mut wl = Watchlist::default();
        assert!(wl.add(entry("FOO", 1, 0, 7)));
        assert!(!wl.add(entry("FOO", 1, 0, 7)));
        assert_eq!(wl.entries.len(), 1);
    }

    #[test]
    fn add_accepts_different_article_ids_for_same_token() {
        let mut wl = Watchlist::default();
        wl.add(entry("FOO", 1, 0, 7));
        wl.add(entry("FOO", 2, 0, 7));
        assert_eq!(wl.entries.len(), 2);
    }

    #[test]
    fn prune_drops_expired_entries() {
        let mut wl = Watchlist::default();
        wl.add(entry("OLD", 1, 10, 7));
        wl.add(entry("FRESH", 2, 1, 7));
        assert_eq!(wl.prune_expired(now()), 1);
        assert_eq!(wl.entries[0].token, "FRESH");
        assert_eq!(wl.entries.len(), 1);
    }

    #[test]
    fn remove_token_drops_all_matching() {
        let mut wl = Watchlist::default();
        wl.add(entry("FOO", 1, 0, 7));
        wl.add(entry("FOO", 2, 0, 7));
        wl.add(entry("BAR", 3, 0, 7));
        assert_eq!(wl.remove_token("FOO"), 2);
        assert_eq!(
            wl.entries
                .iter()
                .map(|e| e.token.as_str())
                .collect::<Vec<_>>(),
            ["BAR"]
        );
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watchlist.json");
        let mut wl = Watchlist::default();
        wl.add(entry("FOO", 1, 0, 7));
        wl.save(&path).unwrap();
        let loaded = Watchlist::load(&path, false, now()).unwrap();
        assert_eq!(loaded, wl);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Watchlist::load(&dir.path().join("nope.json"), true, now()).unwrap();
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wl.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(
            Watchlist::load(&path, true, now()),
            Err(ListingError::Watchlist(_))
        ));
    }

    #[test]
    fn entry_missing_field_is_an_error_but_junk_rows_are_skipped() {
        let ok = serde_json::json!({"watching": [1, "x", {"token": "A", "article_id": "7",
            "announced_at_iso": "2026-06-01T12:00:00+00:00", "expires_at_iso": "2026-06-08T12:00:00Z"}]});
        let wl = Watchlist::from_value(&ok).unwrap();
        assert_eq!(wl.entries[0].article_id, 7);
        let bad = serde_json::json!({"watching": [{"token": "A"}]});
        let err = Watchlist::from_value(&bad).unwrap_err().to_string();
        assert!(err.contains("missing field"), "{err}");
        assert!(
            Watchlist::from_value(&serde_json::json!({"watching": 3}))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn load_prunes_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wl.json");
        let mut wl = Watchlist::default();
        wl.add(entry("OLD", 1, 10, 7));
        wl.save(&path).unwrap();
        assert!(
            Watchlist::load(&path, true, now())
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn unparseable_expiry_never_expires() {
        let mut e = entry("X", 1, 100, 7);
        e.expires_at_iso = "soon".into();
        assert!(!e.is_expired(now()));
    }
}

//! Persistent sniper state, symbol diff and watchlist arming.

use std::collections::BTreeSet;
use std::path::Path;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{ListingError, Result};
use crate::watchlist::Watchlist;

/// Per-UTC-day notional spend tracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailySpend {
    pub date_utc: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub spent_usdt: Decimal,
}

impl Default for DailySpend {
    fn default() -> Self {
        Self {
            date_utc: today(),
            spent_usdt: Decimal::ZERO,
        }
    }
}

fn today() -> String {
    bb_core::time::utc_date(bb_core::time::now())
}

impl DailySpend {
    /// Reset the counter when `today` (`YYYY-MM-DD`) differs from the tracked day.
    pub fn roll_to(&mut self, today: &str) {
        if self.date_utc != today {
            self.date_utc = today.to_owned();
            self.spent_usdt = Decimal::ZERO;
        }
    }

    pub fn reset_if_new_day(&mut self) {
        self.roll_to(&today());
    }

    pub fn would_exceed(&mut self, amount: Decimal, cap: Decimal) -> bool {
        self.reset_if_new_day();
        self.spent_usdt + amount > cap
    }

    pub fn add(&mut self, amount: Decimal) {
        self.reset_if_new_day();
        self.spent_usdt += amount;
    }
}

/// `data/listing-sniper*-state.json`; field names and shapes match the Python file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SniperState {
    /// False until the first poll captured the existing universe (which never fires).
    pub bootstrapped: bool,
    pub daily_spend: DailySpend,
    /// Symbols TRADING for the quote asset as of the last cycle.
    pub known_trading_symbols: BTreeSet<String>,
    /// Watchlist article ids already pulled into `watched_pending`.
    pub processed_article_ids: BTreeSet<i64>,
    /// Symbols already fired on (never fire twice, across restarts).
    pub sniped_symbols: BTreeSet<String>,
    /// Watchlist-armed symbols that fire as soon as they are TRADING.
    pub watched_pending: BTreeSet<String>,
}

impl SniperState {
    /// Missing file = fresh state; unreadable/corrupt file is an error.
    pub fn load(path: &Path) -> Result<Self> {
        bb_core::fsutil::read_json(path)
            .map(Option::unwrap_or_default)
            .map_err(|source| ListingError::StateRead {
                path: path.to_owned(),
                source,
            })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        bb_core::fsutil::write_json_atomic(path, self).map_err(|source| ListingError::Write {
            path: path.to_owned(),
            source,
        })
    }

    /// Symbols to fire on this cycle, sorted: the diff against the previous
    /// snapshot (only once bootstrapped) union watched symbols now TRADING,
    /// minus anything already sniped.
    pub fn compute_new_listings(&self, current: &BTreeSet<String>) -> Vec<String> {
        let diff = current
            .iter()
            .filter(|s| self.bootstrapped && !self.known_trading_symbols.contains(*s));
        let watched = self.watched_pending.intersection(current);
        diff.chain(watched)
            .filter(|s| !self.sniped_symbols.contains(*s))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Pull unprocessed watchlist entries into `watched_pending` as
    /// `{TOKEN}{QUOTE}`; returns the symbols newly armed. A malformed
    /// watchlist is logged and treated as "no new entries".
    pub fn arm_from_watchlist(&mut self, watchlist_path: &Path, quote_asset: &str) -> Vec<String> {
        let watchlist = match Watchlist::load(watchlist_path, true, bb_core::time::now()) {
            Ok(wl) => wl,
            Err(err) => {
                tracing::warn!(error = %err, "listing sniper: watchlist read failed");
                return Vec::new();
            }
        };
        // Python marked an article processed on its first entry, so the other
        // tokens of a multi-token announcement were never armed.
        let already_processed = self.processed_article_ids.clone();
        let mut armed = Vec::new();
        for entry in watchlist.entries {
            if already_processed.contains(&entry.article_id) {
                continue;
            }
            self.processed_article_ids.insert(entry.article_id);
            let symbol = format!("{}{quote_asset}", entry.token).to_uppercase();
            if self.sniped_symbols.contains(&symbol) {
                continue;
            }
            if self.watched_pending.insert(symbol.clone()) {
                armed.push(symbol);
            }
        }
        armed
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::watchlist::WatchEntry;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn first_cycle_returns_nothing() {
        let state = SniperState::default();
        assert!(!state.bootstrapped);
        assert!(
            state
                .compute_new_listings(&set(&["BTCUSDT", "ETHUSDT"]))
                .is_empty()
        );
    }

    #[test]
    fn diff_after_bootstrap_and_sniped_filter() {
        let state = SniperState {
            known_trading_symbols: set(&["BTCUSDT", "ETHUSDT"]),
            bootstrapped: true,
            ..SniperState::default()
        };
        assert_eq!(
            state.compute_new_listings(&set(&["BTCUSDT", "ETHUSDT", "NEWUSDT"])),
            ["NEWUSDT"]
        );
        let state = SniperState {
            known_trading_symbols: set(&["BTCUSDT"]),
            bootstrapped: true,
            sniped_symbols: set(&["OLDNEWUSDT"]),
            ..SniperState::default()
        };
        assert_eq!(
            state.compute_new_listings(&set(&["BTCUSDT", "OLDNEWUSDT", "FRESHUSDT"])),
            ["FRESHUSDT"]
        );
    }

    #[test]
    fn watched_pairs_fire_even_when_known() {
        let state = SniperState {
            bootstrapped: true,
            known_trading_symbols: set(&["AIGENSYNUSDT", "BTCUSDT", "ETHUSDT"]),
            watched_pending: set(&["AIGENSYNUSDT"]),
            ..SniperState::default()
        };
        assert_eq!(
            state.compute_new_listings(&set(&["AIGENSYNUSDT", "BTCUSDT", "ETHUSDT"])),
            ["AIGENSYNUSDT"]
        );
        // Watched pairs fire even before bootstrap.
        let unbooted = SniperState {
            bootstrapped: false,
            ..state
        };
        assert_eq!(
            unbooted.compute_new_listings(&set(&["AIGENSYNUSDT"])),
            ["AIGENSYNUSDT"]
        );
    }

    #[test]
    fn watched_pair_not_yet_trading_does_not_fire() {
        let state = SniperState {
            bootstrapped: true,
            known_trading_symbols: set(&["BTCUSDT"]),
            watched_pending: set(&["NEWCOINUSDT"]),
            ..SniperState::default()
        };
        assert!(state.compute_new_listings(&set(&["BTCUSDT"])).is_empty());
    }

    #[test]
    fn watched_unions_with_diff_and_sniped_excludes_both() {
        let state = SniperState {
            bootstrapped: true,
            known_trading_symbols: set(&["BTCUSDT", "AIGENSYNUSDT"]),
            watched_pending: set(&["AIGENSYNUSDT"]),
            ..SniperState::default()
        };
        assert_eq!(
            state.compute_new_listings(&set(&["BTCUSDT", "AIGENSYNUSDT", "FRESHCOINUSDT"])),
            ["AIGENSYNUSDT", "FRESHCOINUSDT"]
        );
        let state = SniperState {
            bootstrapped: true,
            known_trading_symbols: set(&["BTCUSDT"]),
            watched_pending: set(&["FOOUSDT"]),
            sniped_symbols: set(&["FOOUSDT"]),
            ..SniperState::default()
        };
        assert!(
            state
                .compute_new_listings(&set(&["BTCUSDT", "FOOUSDT"]))
                .is_empty()
        );
    }

    #[test]
    fn daily_spend_cap_and_rollover() {
        let mut s = DailySpend {
            date_utc: today(),
            spent_usdt: dec!(40),
        };
        assert!(s.would_exceed(dec!(15), dec!(50)));
        assert!(!s.would_exceed(dec!(5), dec!(50)));
        let mut old = DailySpend {
            date_utc: "1999-01-01".into(),
            spent_usdt: dec!(999),
        };
        old.reset_if_new_day();
        assert_ne!(old.date_utc, "1999-01-01");
        assert_eq!(old.spent_usdt, Decimal::ZERO);
        old.add(dec!(2));
        old.roll_to("2999-01-01");
        assert_eq!(
            (old.date_utc.as_str(), old.spent_usdt),
            ("2999-01-01", Decimal::ZERO)
        );
    }

    #[test]
    fn state_round_trip_missing_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let mut state = SniperState {
            known_trading_symbols: set(&["BTCUSDT"]),
            bootstrapped: true,
            sniped_symbols: set(&["NEWUSDT"]),
            watched_pending: set(&["FOOUSDT", "BARUSDT"]),
            processed_article_ids: [1, 2, 3].into(),
            ..SniperState::default()
        };
        state.daily_spend.spent_usdt = dec!(12.34);
        state.save(&p).unwrap();
        assert_eq!(SniperState::load(&p).unwrap(), state);

        let fresh = SniperState::load(&dir.path().join("nope.json")).unwrap();
        assert!(!fresh.bootstrapped && fresh.known_trading_symbols.is_empty());

        std::fs::write(&p, "not json").unwrap();
        let err = SniperState::load(&p).unwrap_err().to_string();
        assert!(err.contains("could not read"), "{err}");
    }

    #[test]
    fn loads_python_written_state() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        std::fs::write(
            &p,
            r#"{"bootstrapped": true, "daily_spend": {"date_utc": "2026-06-10", "spent_usdt": 2.0},
               "known_trading_symbols": ["BTCUSDT"], "processed_article_ids": [274005],
               "sniped_symbols": [], "watched_pending": ["AIGENSYNUSDT"]}"#,
        )
        .unwrap();
        let s = SniperState::load(&p).unwrap();
        assert_eq!(s.daily_spend.spent_usdt, dec!(2));
        assert!(s.processed_article_ids.contains(&274005));
        let written = serde_json::to_value(&s).unwrap();
        assert_eq!(written["daily_spend"]["spent_usdt"], serde_json::json!(2.0));
    }

    fn write_watchlist(dir: &Path, token: &str, article_id: i64) -> std::path::PathBuf {
        let path = dir.join("wl.json");
        let mut wl = Watchlist::default();
        let announced = bb_core::time::now() - TimeDelta::days(1);
        wl.add(WatchEntry::new(
            token,
            article_id,
            announced.timestamp_millis(),
            7,
        ));
        wl.save(&path).unwrap();
        path
    }

    #[test]
    fn arming_from_watchlist() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_watchlist(dir.path(), "AIGENSYN", 274005);
        let mut state = SniperState {
            bootstrapped: true,
            ..SniperState::default()
        };
        assert_eq!(state.arm_from_watchlist(&path, "USDT"), ["AIGENSYNUSDT"]);
        assert!(state.watched_pending.contains("AIGENSYNUSDT"));
        assert!(state.processed_article_ids.contains(&274005));
        // Same article never re-arms.
        assert!(state.arm_from_watchlist(&path, "USDT").is_empty());

        let path = write_watchlist(dir.path(), "FOO", 1);
        let mut processed = SniperState {
            processed_article_ids: [1].into(),
            ..SniperState::default()
        };
        assert!(processed.arm_from_watchlist(&path, "USDT").is_empty());
        assert!(processed.watched_pending.is_empty());

        let mut sniped = SniperState {
            sniped_symbols: set(&["FOOUSDT"]),
            ..SniperState::default()
        };
        assert!(sniped.arm_from_watchlist(&path, "USDT").is_empty());
        assert!(sniped.processed_article_ids.contains(&1));

        let mut usdc = SniperState::default();
        assert_eq!(usdc.arm_from_watchlist(&path, "USDC"), ["FOOUSDC"]);

        let mut missing = SniperState::default();
        assert!(
            missing
                .arm_from_watchlist(&dir.path().join("nope.json"), "USDT")
                .is_empty()
        );

        std::fs::write(&path, "garbage").unwrap();
        assert!(
            SniperState::default()
                .arm_from_watchlist(&path, "USDT")
                .is_empty()
        );
    }

    #[test]
    fn arms_every_token_of_a_multi_token_announcement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wl.json");
        let mut wl = Watchlist::default();
        let at = bb_core::time::now().timestamp_millis();
        assert!(wl.add(WatchEntry::new("GENIUS", 9, at, 7)));
        assert!(wl.add(WatchEntry::new("OPG", 9, at, 7)));
        assert!(!wl.add(WatchEntry::new("OPG", 9, at, 7)));
        wl.save(&path).unwrap();
        let mut state = SniperState::default();
        assert_eq!(
            state.arm_from_watchlist(&path, "USDT"),
            ["GENIUSUSDT", "OPGUSDT"]
        );
        assert!(state.arm_from_watchlist(&path, "USDT").is_empty());
    }
}

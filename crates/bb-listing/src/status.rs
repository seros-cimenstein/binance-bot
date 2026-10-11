//! Read-only views of listing bot files for the fleet-status report.

use std::collections::BTreeSet;
use std::path::Path;

use bb_core::audit::JsonlLog;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::detector::DetectorState;
use crate::error::Result;
use crate::sniper::SniperState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingBotKind {
    Detector,
    Sniper,
}

/// File names (relative to the data dir) and service unit of one listing bot,
/// as registered in `bot_status.KNOWN_BOTS` before the sunset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListingBot {
    pub name: &'static str,
    pub kind: ListingBotKind,
    pub state: &'static str,
    pub audit: &'static str,
    pub ledger: &'static str,
    pub unit: &'static str,
}

pub const LISTING_BOTS: [ListingBot; 4] = [
    ListingBot {
        name: "listing_detector",
        kind: ListingBotKind::Detector,
        state: "listing-detector-state.json",
        audit: "listing-detector-events.jsonl",
        ledger: "listing-detector-ledger.jsonl",
        unit: "binance-listing-detector.timer",
    },
    ListingBot {
        name: "listing_sniper_flat",
        kind: ListingBotKind::Sniper,
        state: "listing-sniper-flat-state.json",
        audit: "listing-sniper-flat-events.jsonl",
        ledger: "listing-sniper-flat-ledger.jsonl",
        unit: "binance-listing-sniper-flat.service",
    },
    ListingBot {
        name: "listing_sniper_trailing",
        kind: ListingBotKind::Sniper,
        state: "listing-sniper-trailing-state.json",
        audit: "listing-sniper-trailing-events.jsonl",
        ledger: "listing-sniper-trailing-ledger.jsonl",
        unit: "binance-listing-sniper-trailing.service",
    },
    ListingBot {
        name: "listing_sniper_momentum",
        kind: ListingBotKind::Sniper,
        state: "listing-sniper-momentum-state.json",
        audit: "listing-sniper-momentum-events.jsonl",
        ledger: "listing-sniper-momentum-ledger.jsonl",
        unit: "binance-listing-sniper-momentum.service",
    },
];

/// Detector cursor; `summary()` is the former `_summarize_state` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectorStatus {
    pub last_seen_article_id: i64,
    pub last_poll_time_ms: i64,
}

impl DetectorStatus {
    pub fn summary(&self) -> String {
        format!("cursor article #{}", self.last_seen_article_id)
    }
}

/// `None` when the state file does not exist yet.
pub fn read_detector_status(path: &Path) -> Result<Option<DetectorStatus>> {
    if !path.exists() {
        return Ok(None);
    }
    let s = DetectorState::load(path)?;
    Ok(Some(DetectorStatus {
        last_seen_article_id: s.last_seen_article_id,
        last_poll_time_ms: s.last_poll_time_ms,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SniperStatus {
    pub bootstrapped: bool,
    pub known_trading_symbols: usize,
    pub sniped_symbols: BTreeSet<String>,
    pub watched_pending: BTreeSet<String>,
    pub daily_spend_date_utc: String,
    pub daily_spent_usdt: Decimal,
}

impl SniperStatus {
    pub fn summary(&self) -> String {
        format!(
            "{} symbols tracked, {} sniped, {} armed, ${} spent {}",
            self.known_trading_symbols,
            self.sniped_symbols.len(),
            self.watched_pending.len(),
            self.daily_spent_usdt.normalize(),
            self.daily_spend_date_utc
        )
    }
}

/// `None` when the state file does not exist yet.
pub fn read_sniper_status(path: &Path) -> Result<Option<SniperStatus>> {
    if !path.exists() {
        return Ok(None);
    }
    let s = SniperState::load(path)?;
    Ok(Some(SniperStatus {
        bootstrapped: s.bootstrapped,
        known_trading_symbols: s.known_trading_symbols.len(),
        sniped_symbols: s.sniped_symbols,
        watched_pending: s.watched_pending,
        daily_spend_date_utc: s.daily_spend.date_utc,
        daily_spent_usdt: s.daily_spend.spent_usdt,
    }))
}

#[derive(Debug, Deserialize)]
struct LedgerRowView {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    pnl_usdt: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    pnl_quote: Option<Decimal>,
}

/// Aggregate of a sniper ledger (one row per fire).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerSummary {
    /// Rows considered (every fire, including failed/skipped-price ones).
    pub fires: usize,
    /// `round_trip_complete` rows.
    pub round_trips: usize,
    /// Paper `pnl_usdt` plus live `pnl_quote`.
    pub pnl_usdt: Decimal,
    pub last_timestamp: Option<String>,
}

/// Summarise `path` (missing file = empty), only rows at/after `since` when given.
pub fn read_ledger_summary(path: &Path, since: Option<DateTime<Utc>>) -> Result<LedgerSummary> {
    let rows: Vec<LedgerRowView> = JsonlLog::new(path).read_all()?;
    let mut out = LedgerSummary::default();
    for row in rows {
        let at = row
            .timestamp
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        if let Some(since) = since
            && at.is_none_or(|t| t < since)
        {
            continue;
        }
        out.fires += 1;
        if row.action.as_deref() == Some("round_trip_complete") {
            out.round_trips += 1;
        }
        out.pnl_usdt += row.pnl_usdt.or(row.pnl_quote).unwrap_or_default();
        if row.timestamp.is_some() {
            out.last_timestamp = row.timestamp;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn detector_summary_matches_bot_status() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(LISTING_BOTS[0].state);
        assert_eq!(read_detector_status(&p).unwrap(), None);
        std::fs::write(
            &p,
            r#"{"last_seen_article_id": 274005, "last_poll_time_ms": 1}"#,
        )
        .unwrap();
        assert_eq!(
            read_detector_status(&p).unwrap().unwrap().summary(),
            "cursor article #274005"
        );
    }

    #[test]
    fn sniper_status_reads_state() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(
            &p,
            r#"{"bootstrapped": true, "known_trading_symbols": ["A","B"], "sniped_symbols": ["C"],
                "daily_spend": {"date_utc": "2026-06-10", "spent_usdt": 2.0}, "watched_pending": [], "processed_article_ids": []}"#,
        )
        .unwrap();
        let s = read_sniper_status(&p).unwrap().unwrap();
        assert_eq!(
            s.summary(),
            "2 symbols tracked, 1 sniped, 0 armed, $2 spent 2026-06-10"
        );
    }

    #[test]
    fn ledger_summary_sums_paper_and_live_pnl() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"timestamp": "2026-06-01T00:00:00+00:00", "symbol": "A", "mode": "paper", "action": "round_trip_complete", "pnl_usdt": 0.98}"#,
                "\n",
                r#"{"timestamp": "2026-06-02T00:00:00+00:00", "symbol": "B/USDT", "mode": "live", "action": "round_trip_complete", "pnl_quote": -0.5}"#,
                "\n",
                r#"{"timestamp": "2026-06-03T00:00:00.123456+00:00", "symbol": "C/USDT", "mode": "live", "action": "buy_failed", "reason": "x"}"#,
                "\n",
            ),
        )
        .unwrap();
        let all = read_ledger_summary(&p, None).unwrap();
        assert_eq!(
            (all.fires, all.round_trips, all.pnl_usdt),
            (3, 2, dec!(0.48))
        );
        assert_eq!(
            all.last_timestamp.as_deref(),
            Some("2026-06-03T00:00:00.123456+00:00")
        );
        let since = Utc.with_ymd_and_hms(2026, 6, 2, 0, 0, 0).unwrap();
        let recent = read_ledger_summary(&p, Some(since)).unwrap();
        assert_eq!((recent.fires, recent.pnl_usdt), (2, dec!(-0.5)));
        assert_eq!(
            read_ledger_summary(&dir.path().join("none"), None).unwrap(),
            LedgerSummary::default()
        );
    }
}

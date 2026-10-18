//! When the bot buys: the price move in the 30 minutes before each entry and the move 20 and
//! 90 minutes after it, measured on 1-minute candles against the entry fill. Information for
//! the bot (its "Your record" section), never a gate.
//!
//! Each finished entry is measured once and kept in a small JSON cache next to the audit log,
//! so a cycle only fetches candles for entries it has not measured yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bb_binance::BinanceClient;
use bb_binance::models::Kline;
use bb_core::money::to_f64;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::exit_replay::RoundTrip;

const MINUTE_MS: i64 = 60_000;
/// Minutes before the entry the "before" move starts.
pub const BEFORE_MINUTES: i64 = 30;
/// Minutes after the entry the two "after" moves end.
pub const AFTER_MINUTES: [i64; 2] = [20, 90];

/// One measured entry, in percent of the entry fill price (gross, before fees).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EntryTiming {
    pub before_30m_pct: f64,
    pub after_20m_pct: f64,
    pub after_90m_pct: f64,
}

/// Means over every measured entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryTimingSummary {
    pub entries: usize,
    /// Entries whose price had risen in the 30 minutes before the buy.
    pub rose_before: usize,
    pub mean_before_30m_pct: f64,
    pub mean_after_20m_pct: f64,
    pub mean_after_90m_pct: f64,
}

/// Cache file next to the audit log.
pub fn cache_path(audit_path: &Path) -> PathBuf {
    audit_path.with_file_name("bot-spot-entry-timing.json")
}

fn floor_minute(ms: i64) -> i64 {
    ms - ms.rem_euclid(MINUTE_MS)
}

fn pct(from: Decimal, to: Decimal) -> Option<f64> {
    (from > Decimal::ZERO).then(|| to_f64((to / from - Decimal::ONE) * Decimal::ONE_HUNDRED))
}

/// Measure one entry from 1-minute candles. "Before": the open of the candle starting 30
/// minutes before the entry minute to the entry fill. "After n": the entry fill to the close
/// of the candle that ends n minutes after the entry minute starts. `None` when a candle is
/// missing.
pub fn measure(entry_ms: i64, entry_price: Decimal, candles: &[Kline]) -> Option<EntryTiming> {
    let minute = floor_minute(entry_ms);
    let open_at = |at: i64| candles.iter().find(|k| k.open_time == at).map(|k| k.open);
    let close_of = |at: i64| candles.iter().find(|k| k.open_time == at).map(|k| k.close);
    let before = open_at(minute - BEFORE_MINUTES * MINUTE_MS)?;
    let [first, second] = AFTER_MINUTES;
    Some(EntryTiming {
        before_30m_pct: pct(before, entry_price)?,
        after_20m_pct: pct(entry_price, close_of(minute + (first - 1) * MINUTE_MS)?)?,
        after_90m_pct: pct(entry_price, close_of(minute + (second - 1) * MINUTE_MS)?)?,
    })
}

/// Cache key of one entry.
fn key(trip: &RoundTrip) -> String {
    format!("{}@{}", trip.symbol, trip.entry_ms)
}

/// Means over `timings`; `None` when empty.
pub fn summarize<'a>(timings: impl IntoIterator<Item = &'a EntryTiming>) -> Option<EntryTimingSummary> {
    let timings: Vec<&EntryTiming> = timings.into_iter().collect();
    if timings.is_empty() {
        return None;
    }
    let n = timings.len() as f64;
    let mean = |f: fn(&EntryTiming) -> f64| timings.iter().map(|t| f(t)).sum::<f64>() / n;
    Some(EntryTimingSummary {
        entries: timings.len(),
        rose_before: timings.iter().filter(|t| t.before_30m_pct > 0.0).count(),
        mean_before_30m_pct: mean(|t| t.before_30m_pct),
        mean_after_20m_pct: mean(|t| t.after_20m_pct),
        mean_after_90m_pct: mean(|t| t.after_90m_pct),
    })
}

/// Entries on USDT markets whose 90-minute window has passed by `now_ms`.
fn finished(trips: &[RoundTrip], now_ms: i64) -> impl Iterator<Item = &RoundTrip> {
    let span = (AFTER_MINUTES[1] + 1) * MINUTE_MS;
    trips
        .iter()
        .filter(move |t| t.symbol.ends_with("/USDT") && floor_minute(t.entry_ms) + span <= now_ms)
}

/// Summary over every finished entry, fetching candles only for entries missing from the
/// cache at `path` (which is then rewritten). A fetch error leaves that entry out this time.
pub async fn summary(
    client: &BinanceClient,
    path: &Path,
    trips: &[RoundTrip],
    now_ms: i64,
) -> Option<EntryTimingSummary> {
    let mut cache: BTreeMap<String, EntryTiming> = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut added = false;
    for trip in finished(trips, now_ms) {
        let key = key(trip);
        if cache.contains_key(&key) {
            continue;
        }
        let start = floor_minute(trip.entry_ms) - BEFORE_MINUTES * MINUTE_MS;
        let limit = u32::try_from(BEFORE_MINUTES + AFTER_MINUTES[1] + 2).unwrap_or(1000);
        match client.klines(&trip.symbol, "1m", Some(start), None, Some(limit)).await {
            Ok(candles) => {
                if let Some(timing) = measure(trip.entry_ms, trip.entry_price, &candles) {
                    cache.insert(key, timing);
                    added = true;
                }
            }
            Err(error) => tracing::warn!(%error, symbol = %trip.symbol, "entry timing candles unavailable"),
        }
    }
    if added {
        match serde_json::to_string(&cache) {
            Ok(text) => {
                if let Err(error) = bb_core::fsutil::atomic_write(path, text.as_bytes()) {
                    tracing::warn!(%error, path = %path.display(), "entry timing cache not written");
                }
            }
            Err(error) => tracing::warn!(%error, "entry timing cache not serialized"),
        }
    }
    let wanted: std::collections::BTreeSet<String> = finished(trips, now_ms).map(key).collect();
    summarize(cache.iter().filter(|(k, _)| wanted.contains(*k)).map(|(_, t)| t))
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn candle(open_time: i64, open: Decimal, close: Decimal) -> Kline {
        Kline {
            open_time,
            open,
            high: open.max(close),
            low: open.min(close),
            close,
            volume: Decimal::ONE,
            close_time: Some(open_time + MINUTE_MS - 1),
            quote_volume: None,
            trades: None,
            taker_buy_base_volume: None,
            taker_buy_quote_volume: None,
        }
    }

    #[test]
    fn measures_the_run_up_before_and_the_drift_after_an_entry() {
        // Entry 12:00:20 at 101. 11:30 opened at 100; the 12:19 candle closes at 100.5,
        // the 13:29 candle at 99.99.
        let minute = 1_792_324_800_000; // 2026-10-18T12:00:00Z
        let candles: Vec<Kline> = (-30..=92)
            .map(|m| {
                let at = minute + m * MINUTE_MS;
                let close = match m {
                    19 => dec!(100.5),
                    89 => dec!(99.99),
                    _ => dec!(101),
                };
                candle(at, if m == -30 { dec!(100) } else { dec!(101) }, close)
            })
            .collect();
        let timing = measure(minute + 20_000, dec!(101), &candles).unwrap();
        assert!((timing.before_30m_pct - 1.0).abs() < 1e-9);
        assert!((timing.after_20m_pct - (100.5 / 101.0 - 1.0) * 100.0).abs() < 1e-9);
        assert!((timing.after_90m_pct - -1.0).abs() < 1e-9);
        // A missing candle means no measurement, not a made-up one.
        assert_eq!(measure(minute + 20_000, dec!(101), &candles[..100]), None);

        let summary = summarize([&timing, &EntryTiming { before_30m_pct: -0.5, after_20m_pct: 0.25, after_90m_pct: 0.5 }]).unwrap();
        assert_eq!((summary.entries, summary.rose_before), (2, 1));
        assert!((summary.mean_before_30m_pct - 0.25).abs() < 1e-9);
        assert!((summary.mean_after_90m_pct - -0.25).abs() < 1e-9);
    }
}

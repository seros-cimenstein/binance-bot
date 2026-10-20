//! Offline 1m candles for the persona backtest, read from the `bb entry-study` cache
//! (`data/entry-study-cache/1m_<BASE>_USDT_<start>_<end>.json`, rows `{t,o,h,l,c,v}`).
//!
//! Point-in-time access only: every reader takes a time `t` and sees just the bars that
//! closed by `t`. A coin without a bar at a time is left out of that time (no forward
//! fill), so a late listing (HYPE) simply starts with its own first bar.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Result, TraderError};

pub const MINUTE_MS: i64 = 60_000;
/// The only quote the cache files are read for.
pub const QUOTE: &str = "USDT";

/// One candle, keyed by its open time.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Bar {
    #[serde(rename = "t")]
    pub open_ms: i64,
    #[serde(rename = "o")]
    pub open: f64,
    #[serde(rename = "h")]
    pub high: f64,
    #[serde(rename = "l")]
    pub low: f64,
    #[serde(rename = "c")]
    pub close: f64,
    #[serde(rename = "v")]
    pub volume: f64,
}

/// Start of the `span_ms` bucket that contains `t`.
pub fn floor_to(t: i64, span_ms: i64) -> i64 {
    t - t.rem_euclid(span_ms)
}

/// Aggregate 1m bars (sorted by open time) into `minutes` bars by open-time floor: first
/// open, highest high, lowest low, last close, summed volume. Missing minutes are skipped,
/// so a bucket with a gap is still one bar.
pub fn aggregate(bars: &[Bar], minutes: i64) -> Vec<Bar> {
    let span = minutes * MINUTE_MS;
    let mut out: Vec<Bar> = Vec::new();
    for bar in bars {
        let open_ms = floor_to(bar.open_ms, span);
        match out.last_mut() {
            Some(last) if last.open_ms == open_ms => {
                last.high = last.high.max(bar.high);
                last.low = last.low.min(bar.low);
                last.close = bar.close;
                last.volume += bar.volume;
            }
            _ => out.push(Bar { open_ms, ..*bar }),
        }
    }
    out
}

/// One coin's 1m bars, sorted by open time with duplicates removed.
#[derive(Debug, Clone)]
pub struct Series {
    asset: String,
    bars: Vec<Bar>,
    /// `cum_quote[i]` = Σ volume·close over `bars[..i]`.
    cum_quote: Vec<f64>,
}

impl Series {
    /// Sorts the bars and keeps the first bar of each open time.
    pub fn new(asset: impl Into<String>, mut bars: Vec<Bar>) -> Self {
        bars.sort_by_key(|b| b.open_ms);
        bars.dedup_by_key(|b| b.open_ms);
        let mut cum_quote = Vec::with_capacity(bars.len() + 1);
        let mut sum = 0.0;
        cum_quote.push(sum);
        for bar in &bars {
            sum += bar.volume * bar.close;
            cum_quote.push(sum);
        }
        Self {
            asset: asset.into(),
            bars,
            cum_quote,
        }
    }

    pub fn asset(&self) -> &str {
        &self.asset
    }

    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    pub fn first_open_ms(&self) -> Option<i64> {
        self.bars.first().map(|b| b.open_ms)
    }

    pub fn last_open_ms(&self) -> Option<i64> {
        self.bars.last().map(|b| b.open_ms)
    }

    /// Index of the first bar opened at or after `open_ms`.
    fn lower(&self, open_ms: i64) -> usize {
        self.bars.partition_point(|b| b.open_ms < open_ms)
    }

    /// Every bar that closed by `t` (open time + 1m <= `t`).
    pub fn closed_by(&self, t: i64) -> &[Bar] {
        &self.bars[..self.lower(t - MINUTE_MS + 1)]
    }

    /// The bar that closes exactly at `t`; `None` when that minute is missing.
    pub fn bar_closing_at(&self, t: i64) -> Option<&Bar> {
        let i = self.lower(t - MINUTE_MS);
        self.bars.get(i).filter(|b| b.open_ms == t - MINUTE_MS)
    }

    /// Close of the bar that closes exactly at `t`.
    pub fn close_at(&self, t: i64) -> Option<f64> {
        self.bar_closing_at(t).map(|b| b.close)
    }

    /// The first bar opened at or after `t`, i.e. the next fill bar for a decision at `t`.
    pub fn next_bar(&self, t: i64) -> Option<&Bar> {
        self.bars.get(self.lower(t))
    }

    /// Percent change of the close at `t` against the close `minutes` earlier. `None` when
    /// either minute is missing.
    pub fn return_pct(&self, t: i64, minutes: i64) -> Option<f64> {
        let now = self.close_at(t)?;
        let then = self.close_at(t - minutes * MINUTE_MS)?;
        (then > 0.0).then(|| (now / then - 1.0) * 100.0)
    }

    /// Σ volume·close (USDT) of the bars that opened in `[t - minutes, t)` and closed by
    /// `t`: the trailing quote volume, like Binance's rolling 24h ticker.
    pub fn quote_volume(&self, t: i64, minutes: i64) -> f64 {
        let from = self.lower(t - minutes * MINUTE_MS);
        let to = self.lower(t - MINUTE_MS + 1);
        if to <= from {
            0.0
        } else {
            self.cum_quote[to] - self.cum_quote[from]
        }
    }

    /// The last `n` `minutes` bars fully closed by `t` (oldest first). A bucket that is
    /// still forming at `t` is left out.
    pub fn closed_bars(&self, t: i64, minutes: i64, n: usize) -> Vec<Bar> {
        let span = minutes * MINUTE_MS;
        let end = floor_to(t, span);
        let start = end - span * i64::try_from(n).unwrap_or(i64::MAX / span);
        let bars = &self.bars[self.lower(start)..self.lower(end)];
        aggregate(bars, minutes)
    }

    /// Open time of the newest `minutes` bucket at `t`: the bucket holding the last minute
    /// closed by `t` (still forming, or just completed when `t` is on a boundary).
    pub fn newest_bucket_open(t: i64, minutes: i64) -> i64 {
        floor_to(t - MINUTE_MS, minutes * MINUTE_MS)
    }

    /// Closes of the `minutes` buckets by bucket open time: the newest bucket at `t` (see
    /// [`Self::newest_bucket_open`]; its close is the newest 1m close by `t`) and the `n`
    /// before it. This is the point-in-time view the live trader gets from klines whose
    /// newest bar is still in progress.
    pub fn closes_by_open(&self, t: i64, minutes: i64, n: usize) -> HashMap<i64, f64> {
        let span = minutes * MINUTE_MS;
        let end = Self::newest_bucket_open(t, minutes) + span;
        let start = end - span * i64::try_from(n + 1).unwrap_or(i64::MAX / span);
        let bars = &self.bars[self.lower(start)..self.lower(t - MINUTE_MS + 1)];
        aggregate(bars, minutes)
            .into_iter()
            .map(|b| (b.open_ms, b.close))
            .collect()
    }
}

/// Every cached coin, keyed by base asset (sorted, so runs are deterministic).
#[derive(Debug, Clone, Default)]
pub struct Market {
    series: BTreeMap<String, Series>,
}

impl Market {
    pub fn from_series(series: impl IntoIterator<Item = Series>) -> Self {
        Self {
            series: series.into_iter().map(|s| (s.asset.clone(), s)).collect(),
        }
    }

    /// Read every `1m_<BASE>_USDT_<start>_<end>.json` in `dir`. Files of the same coin are
    /// merged. Fails when the directory is missing or holds no such file; never fetches.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            TraderError::config(format!("missing cache directory {}: {e}", dir.display()))
        })?;
        let mut names: Vec<(String, std::path::PathBuf)> = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(asset) = cache_file_asset(name) {
                names.push((asset.to_string(), path));
            }
        }
        if names.is_empty() {
            return Err(TraderError::config(format!(
                "missing cache: no 1m_<BASE>_{QUOTE}_<start>_<end>.json files in {}",
                dir.display()
            )));
        }
        names.sort();
        let mut bars: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
        for (asset, path) in names {
            let file = File::open(&path)?;
            let rows: Vec<Bar> = serde_json::from_reader(BufReader::new(file)).map_err(|e| {
                TraderError::config(format!("bad cache file {}: {e}", path.display()))
            })?;
            bars.entry(asset).or_default().extend(rows);
        }
        Ok(Self::from_series(
            bars.into_iter()
                .map(|(asset, bars)| Series::new(asset, bars)),
        ))
    }

    pub fn get(&self, asset: &str) -> Option<&Series> {
        self.series.get(asset)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Series> {
        self.series.values()
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }
}

/// The base asset of a cache file name `1m_<BASE>_USDT_<start>_<end>.json`.
fn cache_file_asset(name: &str) -> Option<&str> {
    let stem = name.strip_prefix("1m_")?.strip_suffix(".json")?;
    let mut parts = stem.split('_');
    let (base, quote, start, end) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (parts.next().is_none() && !base.is_empty() && quote == QUOTE && numeric(start) && numeric(end))
        .then_some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_784_448_000_000; // a 15m and 1h boundary (2026-07-19T08:00Z)

    fn bar(minute: i64, open: f64, high: f64, low: f64, close: f64, volume: f64) -> Bar {
        Bar {
            open_ms: T0 + minute * MINUTE_MS,
            open,
            high,
            low,
            close,
            volume,
        }
    }

    fn flat(minute: i64, price: f64) -> Bar {
        bar(minute, price, price, price, price, 1.0)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn aggregates_ohlcv_by_open_time_floor_across_gaps() {
        // Minutes 0..15 form one 15m bucket (minute 7 missing), 15 and 16 start the next.
        let mut bars: Vec<Bar> = (0..15).filter(|m| *m != 7).map(|m| flat(m, 10.0)).collect();
        bars[0] = bar(0, 9.0, 10.0, 8.5, 10.0, 2.0);
        bars[5] = bar(5, 10.0, 12.5, 10.0, 11.0, 3.0);
        bars[13] = bar(14, 11.0, 11.0, 10.5, 10.75, 1.0);
        bars.push(bar(15, 10.75, 11.0, 10.0, 10.2, 4.0));
        bars.push(bar(16, 10.2, 10.3, 9.0, 9.5, 5.0));

        let out = aggregate(&bars, 15);
        assert_eq!(out.len(), 2);
        let first = out[0];
        assert_eq!(first.open_ms, T0);
        assert!(close(first.open, 9.0));
        assert!(close(first.high, 12.5));
        assert!(close(first.low, 8.5));
        assert!(close(first.close, 10.75));
        // 14 bars: 11 flat (volume 1) + volumes 2, 3, 1.
        assert!(close(first.volume, 17.0));
        let second = out[1];
        assert_eq!(second.open_ms, T0 + 15 * MINUTE_MS);
        assert!(close(second.open, 10.75));
        assert!(close(second.high, 11.0));
        assert!(close(second.low, 9.0));
        assert!(close(second.close, 9.5));
        assert!(close(second.volume, 9.0));
    }

    #[test]
    fn sorts_dedups_and_reads_only_bars_closed_by_t() {
        let s = Series::new(
            "BTC",
            vec![flat(2, 12.0), flat(0, 10.0), flat(1, 11.0), flat(1, 99.0)],
        );
        assert_eq!(s.bars().len(), 3);
        // The bar opened at minute 1 closes at minute 2.
        let t = T0 + 2 * MINUTE_MS;
        assert_eq!(s.closed_by(t).len(), 2);
        assert_eq!(s.closed_by(t - 1).len(), 1);
        assert_eq!(s.close_at(t), Some(11.0));
        assert_eq!(s.next_bar(t).map(|b| b.open_ms), Some(t));
    }

    #[test]
    fn a_missing_minute_leaves_the_coin_out_instead_of_forward_filling() {
        let s = Series::new("HYPE", vec![flat(0, 10.0), flat(1, 10.0), flat(3, 11.0)]);
        // The minute-2 bar (closing at minute 3) is missing.
        assert_eq!(s.close_at(T0 + 3 * MINUTE_MS), None);
        assert_eq!(s.return_pct(T0 + 3 * MINUTE_MS, 2), None);
        assert!(close(
            s.return_pct(T0 + 4 * MINUTE_MS, 3).unwrap_or(f64::NAN),
            10.0
        ));
        // Before the coin's first bar there is nothing.
        assert_eq!(s.close_at(T0), None);
        assert_eq!(s.first_open_ms(), Some(T0));
    }

    #[test]
    fn quote_volume_sums_volume_times_close_over_the_trailing_window() {
        let bars = (0..10)
            .map(|m| bar(m, 1.0, 1.0, 1.0, (m + 1) as f64, 2.0))
            .collect();
        let s = Series::new("ETH", bars);
        // At minute 10 the last 3 closed bars are minutes 7, 8, 9: closes 8, 9, 10.
        assert!(close(
            s.quote_volume(T0 + 10 * MINUTE_MS, 3),
            2.0 * (8.0 + 9.0 + 10.0)
        ));
        // A window past the first bar only counts what exists.
        assert!(close(
            s.quote_volume(T0 + 2 * MINUTE_MS, 1440),
            2.0 * (1.0 + 2.0)
        ));
        assert!(close(s.quote_volume(T0, 1440), 0.0));
    }

    #[test]
    fn closed_bars_skip_the_forming_bucket_and_closes_include_it() {
        let bars = (0..40).map(|m| flat(m, 100.0 + m as f64)).collect();
        let s = Series::new("SOL", bars);
        let t = T0 + 37 * MINUTE_MS; // bucket 30..45 is forming, 36 is its newest closed minute
        let closed = s.closed_bars(t, 15, 5);
        assert_eq!(
            closed.iter().map(|b| b.open_ms).collect::<Vec<_>>(),
            vec![T0, T0 + 15 * MINUTE_MS]
        );
        assert!(close(closed[1].close, 129.0));
        let closes = s.closes_by_open(t, 15, 1);
        assert_eq!(closes.len(), 2);
        assert!(close(closes[&(T0 + 30 * MINUTE_MS)], 136.0));
        assert!(close(closes[&(T0 + 15 * MINUTE_MS)], 129.0));
        // On a boundary (t = minute 30) the newest bucket is the one that just completed.
        let boundary = T0 + 30 * MINUTE_MS;
        assert_eq!(
            Series::newest_bucket_open(boundary, 15),
            T0 + 15 * MINUTE_MS
        );
        let closes = s.closes_by_open(boundary, 15, 1);
        assert_eq!(closes.len(), 2);
        assert!(close(closes[&(T0 + 15 * MINUTE_MS)], 129.0));
        assert!(close(closes[&T0], 114.0));
    }

    #[test]
    fn loads_cache_files_merges_windows_and_fails_on_a_missing_cache() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
        write(
            "1m_BTC_USDT_1_2.json",
            r#"[{"t":1784448060000,"o":2,"h":2,"l":2,"c":2,"v":1},{"t":1784448000000,"o":1,"h":1,"l":1,"c":1,"v":1}]"#,
        );
        write(
            "1m_BTC_USDT_3_4.json",
            r#"[{"t":1784448120000,"o":3,"h":3,"l":3,"c":3,"v":1}]"#,
        );
        write(
            "1m_ETH_USDT_1_2.json",
            r#"[{"t":1784448000000,"o":5,"h":5,"l":5,"c":5,"v":1}]"#,
        );
        write("1m_ETH_BTC_1_2.json", "[]");
        write("1h_ETH_USDT_1_2.json", "[]");
        write("notes.txt", "x");

        let market = Market::load_dir(dir.path()).unwrap();
        assert_eq!(market.len(), 2);
        let btc = market.get("BTC").unwrap();
        assert_eq!(btc.bars().len(), 3);
        assert_eq!(btc.first_open_ms(), Some(T0));
        assert_eq!(btc.last_open_ms(), Some(T0 + 2 * MINUTE_MS));

        let empty = tempfile::tempdir().unwrap();
        let err = Market::load_dir(empty.path()).unwrap_err().to_string();
        assert!(err.contains("missing cache"), "{err}");
        let err = Market::load_dir(&empty.path().join("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing cache"), "{err}");
    }
}

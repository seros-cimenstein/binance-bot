//! `bb tag-scoreboard` (Phase 53): which tags and signals moved prices their way, at
//! which horizon, after fees. Research only: public klines, no account, no order path.
//!
//! For every hourly `tags-feed-v1` file of a history (`biz-insights feed --history`) and
//! every name of the signal catalog ([`crate::signals`]):
//! - per-asset names are evaluated for every tradable asset the feed reaches
//!   ([`crate::reach`], with the operator's proxy file); market-wide names
//!   (`market.<macro|mood>.*`) once, against BTC and against the liquid basket
//!   ([`crate::algo_backtest::engine::LIQUID`], equal weights);
//! - a sample is a non-zero signal value `v` at the feed's hour `f` (the close at or before
//!   its `as_of`); its outcome is the forward return `r` from the close at `f` to the close
//!   at `f + h` for `h` = 1h, 24h, 7d, and its excess is `r` minus the same target's mean
//!   `h`-return over every hour of the history ("random hours");
//! - per name, target and horizon: `excess_pct` = Σ|v|·sign(v)·excess / Σ|v| (the signal's
//!   own direction: positive = prices moved its way), `net_pct` = Σ|v|·sign(v)·r / Σ|v|
//!   minus the round-trip fee (`--fee-bps`, 15), the weighted hit rate, and `t` over
//!   horizon-long time blocks (block means, so overlapping hourly samples do not inflate
//!   it; 0 under [`MIN_BLOCKS`] blocks); for the whole history and its older and newer half; `decayed` when the newer
//!   half's excess is under half of a positive older one.
//!
//! Point in time: with `--as-of T` (default now) only feeds with `as_of <= T` and only
//! outcomes observed by `T` (`f + h <= T`, candles closed by `T`) are used, the random-hour
//! baselines included. The file is stamped with `available_at`, the latest outcome time it
//! contains: a +7d result for a signal at `f` is only in a scoreboard available from
//! `f + 7d`. A decision at `D` may use a scoreboard only when `available_at <= D`
//! ([`Scoreboard::usable_at`]).
//!
//! Candles: an incremental cache of closed 1h bars per asset under `--cache-dir`
//! (`1h_<ASSET>_USDT.json`, `[[close ms, close], ...]`): a run fetches only the hours
//! before the first or after the last cached bar that the history needs, keeps at most
//! `--max-bars` bars per asset, and never refetches covered hours. Survivorship: the
//! tradable universe is today's Binance Spot USDT markets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, SecondsFormat, Utc};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

use crate::algo_backtest::engine::LIQUID;
use crate::book::CoinView;
use crate::error::{Result, TraderError};
use crate::reach::{Proxies, Reach};
use crate::signals::{Name, Signals, Span};
use crate::tags::Feed;

/// The scoreboard file format.
pub const SCHEMA: &str = "tag-scoreboard-v1";
/// Forward horizons measured.
pub const HORIZONS: [Span; 3] = [Span::H1, Span::H24, Span::D7];
const HOUR_MS: i64 = 3_600_000;
/// Hours the trailing 7d return needs before the first feed.
const LOOKBACK_MS: i64 = (7 * 24 + 1) * HOUR_MS;
/// Horizon-long time blocks a `t` needs.
pub const MIN_BLOCKS: usize = 5;

fn span_ms(span: Span) -> i64 {
    match span {
        Span::H1 => HOUR_MS,
        Span::H6 => 6 * HOUR_MS,
        Span::H24 => 24 * HOUR_MS,
        Span::D7 => 7 * 24 * HOUR_MS,
        Span::D30 => 30 * 24 * HOUR_MS,
    }
}

fn floor_hour(ms: i64) -> i64 {
    ms - ms.rem_euclid(HOUR_MS)
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |d| d.to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

/// One asset's closed 1h bars: close by close time (open + 1h).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hourly {
    pub closes: BTreeMap<i64, f64>,
}

impl Hourly {
    /// From `(open ms, close)` bars.
    pub fn from_bars(bars: impl IntoIterator<Item = (i64, f64)>) -> Self {
        Self {
            closes: bars
                .into_iter()
                .filter(|(_, c)| *c > 0.0)
                .map(|(open, close)| (open + HOUR_MS, close))
                .collect(),
        }
    }

    fn close(&self, t: i64) -> Option<f64> {
        self.closes.get(&t).copied()
    }

    /// Percent return from the close at `from` to the close at `to`.
    fn ret(&self, from: i64, to: i64) -> Option<f64> {
        let (a, b) = (self.close(from)?, self.close(to)?);
        Some((b / a - 1.0) * 100.0)
    }
}

/// Every asset's hourly closes.
pub type Candles = BTreeMap<String, Hourly>;

/// Where closed 1h bars come from (Binance public klines; a counting fake in tests).
#[async_trait::async_trait]
pub trait HourSource {
    /// `(open ms, close)` of the 1h bars of `<asset>/USDT` opened in `[start, end]`.
    async fn hours(
        &self,
        asset: &str,
        start_open_ms: i64,
        end_open_ms: i64,
    ) -> Result<Vec<(i64, f64)>>;
}

#[async_trait::async_trait]
impl HourSource for bb_binance::BinanceClient {
    async fn hours(
        &self,
        asset: &str,
        start_open_ms: i64,
        end_open_ms: i64,
    ) -> Result<Vec<(i64, f64)>> {
        let mut out = Vec::new();
        let mut start = start_open_ms;
        while start <= end_open_ms {
            let page = self
                .klines(
                    &format!("{asset}/USDT"),
                    "1h",
                    Some(start),
                    Some(end_open_ms),
                    Some(1000),
                )
                .await?;
            let Some(last) = page.last().map(|k| k.open_time) else {
                break;
            };
            out.extend(
                page.iter()
                    .filter_map(|k| Some((k.open_time, k.close.to_f64()?))),
            );
            if page.len() < 1000 {
                break;
            }
            start = last + HOUR_MS;
        }
        Ok(out)
    }
}

/// The incremental 1h cache (see the module docs).
pub struct CandleCache {
    pub dir: PathBuf,
    pub max_bars: usize,
    /// Kline reads made by this run (one per missing range; more for ranges over 1000 h).
    pub reads: usize,
}

impl CandleCache {
    pub fn new(dir: PathBuf, max_bars: usize) -> Self {
        Self {
            dir,
            max_bars,
            reads: 0,
        }
    }

    fn path(&self, asset: &str) -> PathBuf {
        self.dir.join(format!("1h_{asset}_USDT.json"))
    }

    /// The cached closes and the close-time span already read (`None` when nothing was).
    fn load(&self, asset: &str) -> Result<(Hourly, Option<(i64, i64)>)> {
        let path = self.path(asset);
        if !path.exists() {
            return Ok((Hourly::default(), None));
        }
        let file: CacheFile = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let hourly = Hourly {
            closes: file.bars.into_iter().collect(),
        };
        Ok((hourly, Some((file.covered_from, file.covered_to))))
    }

    fn save(&self, asset: &str, hourly: &Hourly, covered: (i64, i64)) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let file = CacheFile {
            covered_from: covered.0,
            covered_to: covered.1,
            bars: hourly.closes.iter().map(|(t, c)| (*t, *c)).collect(),
        };
        bb_core::fsutil::atomic_write(&self.path(asset), serde_json::to_string(&file)?.as_bytes())?;
        Ok(())
    }

    /// `asset`'s closes from `from_ms` to `to_ms` (close times), reading only the hours the
    /// cache has not read yet (hours before a listing count as read once asked for); bars
    /// still forming at `to_ms` are never stored.
    pub async fn update(
        &mut self,
        source: &(impl HourSource + Sync),
        asset: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Hourly> {
        let (mut hourly, covered) = self.load(asset)?;
        let (first, last) = (floor_hour(from_ms), floor_hour(to_ms));
        let mut missing = Vec::new();
        match covered {
            Some((lo, hi)) => {
                if first < lo {
                    missing.push((first, lo - HOUR_MS));
                }
                if hi < last {
                    missing.push((hi + HOUR_MS, last));
                }
            }
            None => missing.push((first, last)),
        }
        missing.retain(|(lo, hi)| lo <= hi);
        if missing.is_empty() {
            return Ok(hourly);
        }
        for (lo_close, hi_close) in missing {
            self.reads += 1;
            let bars = source
                .hours(asset, lo_close - HOUR_MS, hi_close - HOUR_MS)
                .await?;
            for (open, close) in bars {
                let closed = open + HOUR_MS;
                if close > 0.0 && closed <= to_ms {
                    hourly.closes.insert(closed, close);
                }
            }
        }
        let mut span = covered.map_or((first, last), |(lo, hi)| (lo.min(first), hi.max(last)));
        if !hourly.closes.contains_key(&last) {
            // The newest closed hour may not be published yet: read it again next run.
            span.1 = covered.map_or(last - HOUR_MS, |(_, hi)| hi.max(last - HOUR_MS));
        }
        while hourly.closes.len() > self.max_bars {
            hourly.closes.pop_first();
        }
        if hourly.closes.len() == self.max_bars {
            // Bars dropped by the bound are no longer covered.
            if let Some(oldest) = hourly.closes.keys().next() {
                span.0 = span.0.max(*oldest);
            }
        }
        self.save(asset, &hourly, span)?;
        Ok(hourly)
    }
}

/// One asset's cache file.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CacheFile {
    /// Close times already read (bars or not, e.g. before a listing).
    covered_from: i64,
    covered_to: i64,
    /// `[close ms, close]`.
    bars: Vec<(i64, f64)>,
}

/// What a sample is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Each reached tradable asset (per-asset names).
    Assets,
    /// BTC (market-wide names).
    #[serde(rename = "BTC")]
    Btc,
    /// The liquid basket, equal weights (market-wide names).
    Basket,
}

/// One half's (or the whole history's) numbers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub n: usize,
    /// Signal-weighted mean of sign(v) × (return − random-hour return), percent.
    pub excess_pct: f64,
    /// Signal-weighted mean of sign(v) × return, percent, minus the round-trip fee.
    pub net_pct: f64,
    /// Weighted share of samples whose excess went the signal's way.
    pub hit_rate: f64,
    /// t of the excess over horizon-long time blocks; 0 under [`MIN_BLOCKS`] blocks (a
    /// 7d horizon over a 30-day history has only 4).
    pub t: f64,
    pub blocks: usize,
}

/// One name × target × horizon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub name: String,
    pub target: Target,
    pub horizon: String,
    pub all: Stats,
    pub older: Stats,
    pub newer: Stats,
    /// The newer half's excess is under half of a positive older half's.
    pub decayed: bool,
}

/// The history a scoreboard used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub from: String,
    pub to: String,
    /// The split between the older and the newer half.
    pub split: String,
    pub feeds: usize,
}

/// A scoreboard file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scoreboard {
    pub schema: String,
    pub generated_at: String,
    /// Nothing after this time was used.
    pub as_of: String,
    /// The latest outcome (forward-return close) it contains: a decision may use it only
    /// from this time on.
    pub available_at: Option<String>,
    #[serde(skip)]
    pub available_at_ms: Option<i64>,
    pub history: History,
    /// Per horizon: the latest signal time used and its outcome time (`signals_to` + h).
    pub horizons: BTreeMap<String, HorizonSpan>,
    pub fee_bps: f64,
    pub min_posters: u64,
    pub assets: usize,
    pub rows: Vec<Row>,
}

/// The signals one horizon's rows cover.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonSpan {
    pub signals_to: String,
    pub outcomes_to: String,
    #[serde(skip)]
    pub signals_to_ms: i64,
}

impl Scoreboard {
    /// Whether a decision at `decision_ms` may use this scoreboard: every outcome in it was
    /// observable by then.
    pub fn usable_at(&self, decision_ms: i64) -> bool {
        self.available_at_ms.is_some_and(|t| t <= decision_ms)
    }

    /// Parse a scoreboard file's text: refuses another schema; `available_at_ms` and each
    /// horizon's `signals_to_ms` come from their timestamps.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let mut board: Scoreboard = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if board.schema != SCHEMA {
            return Err(format!("schema {:?}, expected {SCHEMA}", board.schema));
        }
        let ms = |t: &str| {
            DateTime::parse_from_rfc3339(t)
                .map(|d| d.timestamp_millis())
                .map_err(|e| format!("bad timestamp {t:?}: {e}"))
        };
        board.available_at_ms = board.available_at.as_deref().map(ms).transpose()?;
        for span in board.horizons.values_mut() {
            span.signals_to_ms = ms(&span.signals_to)?;
        }
        Ok(board)
    }

    /// Read and parse the scoreboard file at `path`.
    pub fn read(path: &Path) -> std::result::Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[derive(Debug, Clone, Default)]
struct Half {
    n: usize,
    w: f64,
    wx: f64,
    wr: f64,
    hit: f64,
    blocks: BTreeMap<i64, (f64, f64)>,
}

impl Half {
    fn add(&mut self, weight: f64, x: f64, r: f64, block: i64) {
        self.n += 1;
        self.w += weight;
        self.wx += weight * x;
        self.wr += weight * r;
        if x > 0.0 {
            self.hit += weight;
        }
        let entry = self.blocks.entry(block).or_insert((0.0, 0.0));
        entry.0 += weight * x;
        entry.1 += weight;
    }

    fn merge(&self, other: &Half) -> Half {
        let mut out = self.clone();
        out.n += other.n;
        out.w += other.w;
        out.wx += other.wx;
        out.wr += other.wr;
        out.hit += other.hit;
        for (block, (wx, w)) in &other.blocks {
            let entry = out.blocks.entry(*block).or_insert((0.0, 0.0));
            entry.0 += wx;
            entry.1 += w;
        }
        out
    }

    fn stats(&self, fee_pct: f64) -> Stats {
        if self.w <= 0.0 {
            return Stats::default();
        }
        let means: Vec<f64> = self
            .blocks
            .values()
            .filter(|(_, w)| *w > 0.0)
            .map(|(wx, w)| wx / w)
            .collect();
        let k = means.len();
        let t = if k >= MIN_BLOCKS {
            let mean = means.iter().sum::<f64>() / k as f64;
            let var = means.iter().map(|m| (m - mean).powi(2)).sum::<f64>() / (k - 1) as f64;
            if var > 0.0 {
                mean / (var / k as f64).sqrt()
            } else {
                0.0
            }
        } else {
            0.0
        };
        Stats {
            n: self.n,
            excess_pct: self.wx / self.w,
            net_pct: self.wr / self.w - fee_pct,
            hit_rate: self.hit / self.w,
            t,
            blocks: k,
        }
    }
}

/// Settings of one scoreboard.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub fee_bps: f64,
    pub min_posters: u64,
    /// Nothing after this is used.
    pub as_of_ms: i64,
    /// The first and last feed time of the history (for the random hours and the split).
    pub from_ms: i64,
    pub to_ms: i64,
}

/// The scoreboard being built, one feed at a time (see the module docs).
pub struct Study<'a> {
    candles: &'a Candles,
    params: Params,
    mid_ms: i64,
    asset_names: Vec<Name>,
    market_names: Vec<Name>,
    basket: Vec<&'static str>,
    /// Random-hour mean return per target, for each of [`HORIZONS`].
    base: BTreeMap<String, [Option<f64>; 3]>,
    acc: BTreeMap<(usize, Target, Span), [Half; 2]>,
    feeds: usize,
    /// Latest signal time used per horizon.
    signals_to: BTreeMap<Span, i64>,
}

impl<'a> Study<'a> {
    pub fn new(candles: &'a Candles, params: Params) -> Self {
        let (market_names, asset_names) = Name::all()
            .into_iter()
            .partition(|n| n.is_market());
        Self {
            candles,
            mid_ms: params.from_ms + (params.to_ms - params.from_ms) / 2,
            params,
            asset_names,
            market_names,
            basket: LIQUID
                .iter()
                .copied()
                .filter(|a| candles.contains_key(*a))
                .collect(),
            base: BTreeMap::new(),
            acc: BTreeMap::new(),
            feeds: 0,
            signals_to: BTreeMap::new(),
        }
    }

    /// Forward return of `target` (an asset name or the basket) from `f` over `h`, if its
    /// outcome is observed by `as_of`.
    fn forward(&self, target: &str, f: i64, h: Span) -> Option<f64> {
        let to = f + span_ms(h);
        if to > self.params.as_of_ms {
            return None;
        }
        if target == "basket" {
            let rets: Vec<f64> = self
                .basket
                .iter()
                .filter_map(|a| self.candles.get(*a)?.ret(f, to))
                .collect();
            return (!rets.is_empty()).then(|| rets.iter().sum::<f64>() / rets.len() as f64);
        }
        self.candles.get(target)?.ret(f, to)
    }

    /// The mean `h`-return of `target` over every hour of the history whose outcome is
    /// observed by `as_of`, for each of [`HORIZONS`].
    fn baselines(&mut self, target: &str) -> [Option<f64>; 3] {
        if let Some(cached) = self.base.get(target) {
            return *cached;
        }
        let base = HORIZONS.map(|h| {
            let (mut sum, mut n) = (0.0, 0usize);
            let mut f = floor_hour(self.params.from_ms);
            while f + span_ms(h) <= self.params.as_of_ms && f <= self.params.to_ms {
                if let Some(r) = self.forward(target, f, h) {
                    sum += r;
                    n += 1;
                }
                f += HOUR_MS;
            }
            (n > 0).then(|| sum / n as f64)
        });
        self.base.insert(target.to_owned(), base);
        base
    }

    fn coin_view(&self, asset: &str, f: i64) -> Option<CoinView> {
        let c = self.candles.get(asset)?;
        c.close(f)?;
        Some(CoinView {
            ret_15m: None,
            ret_1h: c.ret(f - HOUR_MS, f),
            ret_4h: None,
            ret_24h: c.ret(f - 24 * HOUR_MS, f),
            ret_7d: c.ret(f - 7 * 24 * HOUR_MS, f),
            quote_volume_24h: 0.0,
        })
    }

    fn sample(&mut self, name: usize, target: Target, key: &str, f: i64, v: f64) {
        let bases = self.baselines(key);
        for (h, base) in HORIZONS.into_iter().zip(bases) {
            let (Some(r), Some(base)) = (self.forward(key, f, h), base) else {
                continue;
            };
            let sign = v.signum();
            let half = usize::from(f >= self.mid_ms);
            let block = f.div_euclid(span_ms(h));
            self.acc.entry((name, target, h)).or_default()[half].add(
                v.abs(),
                sign * (r - base),
                sign * r,
                block,
            );
            let latest = self.signals_to.entry(h).or_insert(f);
            *latest = (*latest).max(f);
        }
    }

    /// Add one feed (skipped when it is newer than `as_of`). `reach` is its reach with the
    /// study's proxies and tradable assets.
    pub fn add_feed(&mut self, feed: &Feed, reach: &Reach) {
        if feed.as_of_ms > self.params.as_of_ms {
            return;
        }
        self.feeds += 1;
        let f = floor_hour(feed.as_of_ms);
        let signals = Signals::new(feed, reach, self.params.min_posters);
        let own: BTreeMap<&str, _> = feed.assets().collect();
        let assets: Vec<String> = reach
            .paths
            .keys()
            .filter(|a| self.candles.contains_key(*a))
            .cloned()
            .collect();
        for asset in &assets {
            let coin = self.coin_view(asset, f);
            let own = own.get(asset.as_str()).copied();
            for i in 0..self.asset_names.len() {
                let v = signals.value(&self.asset_names[i], asset, own, coin.as_ref());
                if v != 0.0 && v.is_finite() {
                    self.sample(i, Target::Assets, asset, f, v);
                }
            }
        }
        let offset = self.asset_names.len();
        for i in 0..self.market_names.len() {
            let v = signals.value(&self.market_names[i], "", None, None);
            if v != 0.0 && v.is_finite() {
                self.sample(offset + i, Target::Btc, "BTC", f, v);
                self.sample(offset + i, Target::Basket, "basket", f, v);
            }
        }
    }

    pub fn finish(self, generated_at: String, assets: usize) -> Scoreboard {
        let fee_pct = self.params.fee_bps / 100.0;
        let names: Vec<&Name> = self.asset_names.iter().chain(&self.market_names).collect();
        let rows = self
            .acc
            .iter()
            .map(|((i, target, h), [older, newer])| {
                let (o, n) = (older.stats(fee_pct), newer.stats(fee_pct));
                Row {
                    name: names[*i].to_string(),
                    target: *target,
                    horizon: h.as_str().to_owned(),
                    all: older.merge(newer).stats(fee_pct),
                    decayed: o.n > 0
                        && n.n > 0
                        && o.excess_pct > 0.0
                        && n.excess_pct < 0.5 * o.excess_pct,
                    older: o,
                    newer: n,
                }
            })
            .collect();
        let horizons: BTreeMap<String, HorizonSpan> = self
            .signals_to
            .iter()
            .map(|(h, f)| {
                let span = HorizonSpan {
                    signals_to: iso(*f),
                    outcomes_to: iso(f + span_ms(*h)),
                    signals_to_ms: *f,
                };
                (h.as_str().to_owned(), span)
            })
            .collect();
        let available_at_ms = self.signals_to.iter().map(|(h, f)| f + span_ms(*h)).max();
        Scoreboard {
            schema: SCHEMA.to_owned(),
            generated_at,
            as_of: iso(self.params.as_of_ms),
            available_at: available_at_ms.map(iso),
            available_at_ms,
            history: History {
                from: iso(self.params.from_ms),
                to: iso(self.params.to_ms),
                split: iso(self.mid_ms),
                feeds: self.feeds,
            },
            horizons,
            fee_bps: self.params.fee_bps,
            min_posters: self.params.min_posters,
            assets,
            rows,
        }
    }
}

/// `bb tag-scoreboard` arguments.
#[derive(clap::Args, Debug, Clone)]
pub struct TagScoreboardArgs {
    /// Hourly `tags-feed-v1` history (`biz-insights feed --history`).
    #[arg(long)]
    pub history_dir: PathBuf,
    /// The incremental 1h candle cache.
    #[arg(long, default_value = "data/scoreboard-cache")]
    pub cache_dir: PathBuf,
    /// Where `scoreboard-<date>.json` and `latest.json` go.
    #[arg(long, default_value = "data/scoreboard")]
    pub output_dir: PathBuf,
    /// The operator's proxy file (`BOT_SPOT_PROXIES`).
    #[arg(long, default_value = "config/proxies.json")]
    pub proxies: PathBuf,
    /// Round-trip fee charged on `net_pct`, in bps.
    #[arg(long, default_value_t = 15.0)]
    pub fee_bps: f64,
    /// Posters a window needs before its direction counts (as the trader).
    #[arg(long, default_value_t = 2)]
    pub min_posters: u64,
    /// Use nothing after this time (RFC 3339; default now).
    #[arg(long)]
    pub as_of: Option<String>,
    /// 1h bars kept per asset in the cache.
    #[arg(long, default_value_t = 10_000)]
    pub max_bars: usize,
}

fn history_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| TraderError::config(format!("tags history {}: {e}", dir.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(TraderError::config(format!(
            "no tags feeds (*.json) in {}",
            dir.display()
        )));
    }
    Ok(paths)
}

fn read_feed(path: &Path) -> Result<Feed> {
    crate::tags::parse_raw(&std::fs::read_to_string(path)?)
        .map_err(|e| TraderError::config(format!("bad tags feed {}: {e}", path.display())))
}

/// Base assets with a trading Binance Spot USDT market now (cash-like left out).
async fn usdt_markets(client: &bb_binance::BinanceClient) -> Result<BTreeSet<String>> {
    let info = client.exchange_info(None).await?;
    Ok(info
        .symbols
        .into_iter()
        .filter(|s| s.quote_asset == "USDT" && s.status == "TRADING" && s.is_spot_trading_allowed)
        .map(|s| s.base_asset)
        .filter(|b| !crate::assets::is_low_upside(b))
        .collect())
}

/// Run `bb tag-scoreboard` (see the module docs).
pub async fn run(args: &TagScoreboardArgs) -> Result<Vec<PathBuf>> {
    let as_of_ms = match &args.as_of {
        Some(t) => DateTime::parse_from_rfc3339(t)
            .map_err(|e| TraderError::config(format!("--as-of {t:?}: {e}")))?
            .timestamp_millis(),
        None => bb_core::time::now_ms(),
    };
    let proxies = Proxies::load(&args.proxies).map_err(TraderError::config)?;
    let client = bb_binance::BinanceClient::new(bb_binance::BinanceConfig::new(false))?;
    let tradable = usdt_markets(&client).await?;
    let paths = history_files(&args.history_dir)?;
    // Pass 1: the span and every tradable asset the history reaches.
    let (mut from_ms, mut to_ms, mut used) = (i64::MAX, i64::MIN, 0usize);
    let mut assets: BTreeSet<String> = LIQUID.iter().map(|a| (*a).to_owned()).collect();
    for path in &paths {
        let feed = read_feed(path)?;
        if feed.as_of_ms > as_of_ms {
            continue;
        }
        from_ms = from_ms.min(feed.as_of_ms);
        to_ms = to_ms.max(feed.as_of_ms);
        used += 1;
        let reach = crate::reach::build(&feed, &proxies, |a| tradable.contains(a));
        assets.extend(reach.paths.into_keys());
    }
    if from_ms > to_ms {
        return Err(TraderError::config(format!(
            "no tags feed at or before {}",
            iso(as_of_ms)
        )));
    }
    // Candles: only the hours the cache does not cover.
    let mut cache = CandleCache::new(args.cache_dir.clone(), args.max_bars);
    let mut candles = Candles::new();
    for asset in assets.iter().filter(|a| tradable.contains(*a)) {
        let hourly = cache
            .update(&client, asset, floor_hour(from_ms) - LOOKBACK_MS, as_of_ms)
            .await?;
        if !hourly.closes.is_empty() {
            candles.insert(asset.clone(), hourly);
        }
    }
    eprintln!(
        "tag-scoreboard: {used} of {} feeds ({} .. {}), {} assets with candles, {} kline reads",
        paths.len(),
        iso(from_ms),
        iso(to_ms),
        candles.len(),
        cache.reads
    );
    // Pass 2: the samples.
    let params = Params {
        fee_bps: args.fee_bps,
        min_posters: args.min_posters,
        as_of_ms,
        from_ms,
        to_ms,
    };
    let mut study = Study::new(&candles, params);
    for path in &paths {
        let feed = read_feed(path)?;
        let reach = crate::reach::build(&feed, &proxies, |a| tradable.contains(a));
        study.add_feed(&feed, &reach);
    }
    let board = study.finish(iso(bb_core::time::now_ms()), candles.len());
    std::fs::create_dir_all(&args.output_dir)?;
    let body = serde_json::to_string_pretty(&board)?;
    let dated = args
        .output_dir
        .join(format!("scoreboard-{}.json", &board.as_of[..10]));
    let latest = args.output_dir.join("latest.json");
    for path in [&dated, &latest] {
        bb_core::fsutil::atomic_write(path, body.as_bytes())?;
    }
    eprintln!(
        "tag-scoreboard: {} rows, available_at {}",
        board.rows.len(),
        board.available_at.as_deref().unwrap_or("none")
    );
    Ok(vec![dated, latest])
}

pub async fn run_cli(args: &TagScoreboardArgs) -> Result<ExitCode> {
    for path in run(args).await? {
        println!("wrote {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const H0: i64 = 1_790_812_800_000; // 2026-10-01T00:00:00Z, an hour boundary

    /// Deterministic noise in [−1, 1).
    fn noise(seed: u64) -> f64 {
        let mut x = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }

    /// A feed at hour `i` with asset tags LEAD (6h direction `lead`) and NOISE (6h
    /// direction `noisy`), each with enough posters.
    fn feed(i: i64, lead: f64, noisy: f64) -> Feed {
        let w = |dir: f64| {
            let (up, down) = if dir >= 0.0 {
                (1.0 + 4.0 * dir, 1.0)
            } else {
                (1.0, 1.0 - 4.0 * dir)
            };
            serde_json::json!({"6h": {"mentions": 9, "posters": 5, "up": up, "down": down}})
        };
        let text = serde_json::json!({
            "schema": "tags-feed-v1", "as_of": iso(H0 + i * HOUR_MS),
            "tags": {"asset:LEAD": {"kind": "asset", "name": "LEAD", "windows": w(lead)},
                     "asset:NOISE": {"kind": "asset", "name": "NOISE", "windows": w(noisy)}}
        })
        .to_string();
        crate::tags::parse_raw(&text).unwrap()
    }

    /// 60 days of hourly prices from H0 − 8 days. LEAD rises 1% in the 24 hours after
    /// every hour whose (hidden) lead sign is positive and falls 1% after a negative one,
    /// plus noise; NOISE and BTC are noise only.
    fn world() -> (Candles, Vec<f64>, Vec<f64>) {
        let hours = 60 * 24;
        let start = H0 - 8 * 24 * HOUR_MS;
        let lead: Vec<f64> = (0..hours)
            .map(|i| if noise(i as u64) > 0.0 { 1.0 } else { -1.0 })
            .collect();
        let noisy: Vec<f64> = (0..hours).map(|i| noise(10_000 + i as u64)).collect();
        let series = |drift: &dyn Fn(i64) -> f64, seed: u64| {
            let mut price = 100.0;
            let bars: Vec<(i64, f64)> = (0..hours as i64 + 8 * 24)
                .map(|k| {
                    price *= 1.0 + drift(k) / 100.0 + 0.2 * noise(seed + k as u64) / 100.0;
                    (start + k * HOUR_MS - HOUR_MS, price)
                })
                .collect();
            Hourly::from_bars(bars)
        };
        // At bar k (close at start + k h) LEAD drifts by the average sign of the signals of
        // the previous 24 hours, 1/24 % per hour each: +1% over the day after a + signal.
        let lead_drift = {
            let lead = lead.clone();
            move |k: i64| {
                let i = k - 8 * 24; // feed index of this close
                (1..=24)
                    .filter_map(|back| usize::try_from(i - back).ok().and_then(|j| lead.get(j)))
                    .sum::<f64>()
                    / 24.0
            }
        };
        let flat = |_k: i64| 0.0;
        let candles = Candles::from([
            ("LEAD".to_owned(), series(&lead_drift, 1)),
            ("NOISE".to_owned(), series(&flat, 2)),
            ("BTC".to_owned(), series(&flat, 3)),
        ]);
        (candles, lead, noisy)
    }

    fn run_study(
        candles: &Candles,
        lead: &[f64],
        noisy: &[f64],
        feeds: i64,
        as_of_ms: i64,
    ) -> Scoreboard {
        let reach_of =
            |f: &Feed| crate::reach::build(f, &Proxies::default(), |a| candles.contains_key(a));
        let to_ms = (H0 + (feeds - 1) * HOUR_MS).min(floor_hour(as_of_ms));
        let params = Params {
            fee_bps: 15.0,
            min_posters: 2,
            as_of_ms,
            from_ms: H0,
            to_ms,
        };
        let mut study = Study::new(candles, params);
        for i in 0..feeds {
            let f = feed(i, lead[i as usize], noisy[i as usize]);
            study.add_feed(&f, &reach_of(&f));
        }
        study.finish("test".into(), candles.len())
    }

    fn row<'b>(board: &'b Scoreboard, name: &str, horizon: &str) -> &'b Row {
        board
            .rows
            .iter()
            .find(|r| r.name == name && r.horizon == horizon)
            .unwrap()
    }

    #[test]
    fn a_planted_lead_is_found_and_noise_is_not() {
        let (candles, lead, noisy) = world();
        let feeds = 50 * 24;
        let board = run_study(&candles, &lead, &noisy, feeds, H0 + 52 * 24 * HOUR_MS);
        // Both tags are asset tags: `asset.direction.6h` pools LEAD (planted) and NOISE.
        // Split them by checking the legacy `stance.6h` per asset through a LEAD-only study.
        let only_lead: Candles = candles
            .iter()
            .filter(|(a, _)| a.as_str() != "NOISE")
            .map(|(a, h)| (a.clone(), h.clone()))
            .collect();
        let lead_board = run_study(&only_lead, &lead, &noisy, feeds, H0 + 52 * 24 * HOUR_MS);
        let found = row(&lead_board, "stance.6h", "24h");
        assert!(found.all.excess_pct > 0.5 && found.all.t > 3.0, "{found:?}");
        assert!(found.older.excess_pct > 0.0 && found.newer.excess_pct > 0.0 && !found.decayed);
        assert!(found.all.hit_rate > 0.55, "{found:?}");
        // Net of the 15 bps fee it still pays at +24h; at +1h the edge is a 24th of it.
        assert!(found.all.net_pct > 0.3, "{found:?}");
        let only_noise: Candles = candles
            .iter()
            .filter(|(a, _)| a.as_str() != "LEAD")
            .map(|(a, h)| (a.clone(), h.clone()))
            .collect();
        let noise_board = run_study(&only_noise, &lead, &noisy, feeds, H0 + 52 * 24 * HOUR_MS);
        for horizon in ["1h", "24h", "7d"] {
            let r = row(&noise_board, "stance.6h", horizon);
            assert!(
                r.all.t.abs() < 2.0,
                "noise looks like a lead at {horizon}: {r:?}"
            );
        }
        // Market-wide names need macro or mood tags: none here, so no market rows.
        assert!(board.rows.iter().all(|r| r.target == Target::Assets));
    }

    #[test]
    fn future_outcomes_never_enter_a_scoreboard_an_earlier_decision_uses() {
        let (candles, lead, noisy) = world();
        let feeds = 30 * 24;
        let cut = H0 + 20 * 24 * HOUR_MS; // decision time D, inside the history
        let board = run_study(&candles, &lead, &noisy, feeds, cut);
        // Stamped with its latest outcome, never after D; the history it saw ends by D.
        let available = board.available_at_ms.unwrap();
        assert!(available <= cut && board.usable_at(cut));
        assert!(board.history.feeds <= 20 * 24 + 1);
        // A +7d result for a signal one day before D is not in it (its outcome is after D):
        // the 7d rows hold only signals up to D − 7d.
        let r7 = row(&board, "stance.6h", "7d");
        assert!(r7.all.n > 0);
        let h7 = &board.horizons["7d"];
        assert!(
            h7.signals_to_ms <= cut - 7 * 24 * HOUR_MS
                && h7.signals_to_ms + 7 * 24 * HOUR_MS <= cut
        );
        // The 1h rows reach up to an hour before D.
        assert_eq!(board.horizons["1h"].signals_to_ms, cut - HOUR_MS);
        // Candles after D can change arbitrarily without changing the scoreboard at D.
        let mut changed = candles.clone();
        for hourly in changed.values_mut() {
            for (t, close) in hourly.closes.iter_mut() {
                if *t > cut {
                    *close *= 3.0;
                }
            }
        }
        let again = run_study(&changed, &lead, &noisy, feeds, cut);
        assert_eq!(
            serde_json::to_value(&board).unwrap(),
            serde_json::to_value(&again).unwrap()
        );
        // A scoreboard of the whole history is not usable by the decision at D.
        let full = run_study(&candles, &lead, &noisy, feeds, H0 + 40 * 24 * HOUR_MS);
        assert!(full.available_at_ms.unwrap() > cut && !full.usable_at(cut));
        assert_ne!(
            serde_json::to_value(&full.rows).unwrap(),
            serde_json::to_value(&board.rows).unwrap()
        );
    }

    /// Counts reads and serves bars for any range from `listed` on.
    struct Fake {
        listed: i64,
        reads: Mutex<Vec<(String, i64, i64)>>,
    }

    #[async_trait::async_trait]
    impl HourSource for Fake {
        async fn hours(&self, asset: &str, start: i64, end: i64) -> Result<Vec<(i64, f64)>> {
            self.reads
                .lock()
                .unwrap()
                .push((asset.to_owned(), start, end));
            Ok((0..)
                .map(|k| start + k * HOUR_MS)
                .take_while(|open| *open <= end)
                .filter(|open| *open >= self.listed)
                .map(|open| (open, 10.0 + (open / HOUR_MS % 7) as f64))
                .collect())
        }
    }

    #[tokio::test]
    async fn the_cache_reads_only_uncovered_hours_and_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake {
            listed: i64::MIN,
            reads: Mutex::new(Vec::new()),
        };
        let mut cache = CandleCache::new(dir.path().to_path_buf(), 500);
        let now = H0 + 100 * HOUR_MS + 30 * 60_000; // mid-hour: the forming bar is not stored
        let first = cache.update(&fake, "BTC", H0, now).await.unwrap();
        assert_eq!(
            first.closes.keys().next_back().copied(),
            Some(H0 + 100 * HOUR_MS)
        );
        assert_eq!(first.closes.len(), 101);
        assert_eq!(fake.reads.lock().unwrap().len(), 1);
        // The same hour again: no read at all.
        let again = cache
            .update(&fake, "BTC", H0, now + 10 * 60_000)
            .await
            .unwrap();
        assert_eq!(
            (again, fake.reads.lock().unwrap().len()),
            (first.clone(), 1)
        );
        // Three hours later: one read, of exactly the three new hours.
        let later = cache
            .update(&fake, "BTC", H0, now + 3 * HOUR_MS)
            .await
            .unwrap();
        let reads = fake.reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 2);
        assert_eq!(
            (reads[1].1, reads[1].2),
            (H0 + 100 * HOUR_MS, H0 + 102 * HOUR_MS)
        );
        assert_eq!(later.closes.len(), 104);
        // An earlier start reads only the hours before the cache, and the bound drops the
        // oldest bars beyond 500.
        let wider = cache
            .update(&fake, "BTC", H0 - 1000 * HOUR_MS, now + 3 * HOUR_MS)
            .await
            .unwrap();
        let reads = fake.reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 3);
        assert_eq!(reads[2].2, H0 - 2 * HOUR_MS); // up to the hour before the first cached close
        assert_eq!(wider.closes.len(), 500);
        assert_eq!(
            wider.closes.keys().next_back().copied(),
            Some(H0 + 103 * HOUR_MS)
        );
        // The file holds what the run returned.
        let (reloaded, covered) = CandleCache::new(dir.path().to_path_buf(), 500)
            .load("BTC")
            .unwrap();
        assert_eq!(reloaded, wider);
        assert_eq!(covered, Some((H0 - 396 * HOUR_MS, H0 + 103 * HOUR_MS)));
    }

    #[tokio::test]
    async fn hours_before_a_listing_are_read_once() {
        let dir = tempfile::tempdir().unwrap();
        // Listed 50 hours after the history start: the first run asks for those 50 hours
        // too (Binance returns nothing), the next runs do not ask again.
        let fake = Fake {
            listed: H0 + 50 * HOUR_MS,
            reads: Mutex::new(Vec::new()),
        };
        let mut cache = CandleCache::new(dir.path().to_path_buf(), 500);
        let now = H0 + 100 * HOUR_MS;
        let first = cache.update(&fake, "HYPE", H0, now).await.unwrap();
        assert_eq!(first.closes.keys().next().copied(), Some(H0 + 51 * HOUR_MS));
        cache
            .update(&fake, "HYPE", H0, now + 20 * 60_000)
            .await
            .unwrap();
        assert_eq!(fake.reads.lock().unwrap().len(), 1);
        // A newest hour not published yet is read again on the next run.
        let late = Fake {
            listed: i64::MIN,
            reads: Mutex::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut cache = CandleCache::new(dir.path().to_path_buf(), 500);
        let lagging = Fake {
            listed: i64::MIN,
            reads: Mutex::new(Vec::new()),
        };
        struct Lag<'f>(&'f Fake);
        #[async_trait::async_trait]
        impl HourSource for Lag<'_> {
            async fn hours(&self, asset: &str, start: i64, end: i64) -> Result<Vec<(i64, f64)>> {
                // Serves every bar but the newest one asked for.
                let mut bars = self.0.hours(asset, start, end).await?;
                bars.retain(|(open, _)| *open < end);
                Ok(bars)
            }
        }
        cache.update(&Lag(&lagging), "BTC", H0, now).await.unwrap();
        let caught_up = cache.update(&late, "BTC", H0, now).await.unwrap();
        assert_eq!(caught_up.closes.keys().next_back().copied(), Some(now));
        assert_eq!(
            late.reads.lock().unwrap().clone(),
            [("BTC".to_owned(), now - HOUR_MS, now - HOUR_MS)]
        );
    }
}

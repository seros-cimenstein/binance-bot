//! `bb biz-lead-study`: does /biz/ talk about a coin come before its price moves?
//!
//! Research only (public klines, no account, no order path). Reads a folder of
//! `biz-feed-v1` files written by biz-app (`biz-insights feed --history`), one per feed
//! time, each holding what was visible on /biz/ at its `as_of`: per coin and per window
//! (`1h`, `6h`, `24h`) the mentions over every post, the distinct posters, and the AI's
//! quality-weighted bullish and bearish tags. For every feed × studied coin it buys on
//! paper at the open of the first whole minute after `as_of` plus `--delay-minutes`, and
//! measures the move to +1h, +6h and +24h on the `bb entry-study` 1m candle cache
//! (fetching public candles that are missing).
//!
//! Each bucket of a signal is compared with random hours of the same coin: a sample's
//! excess is its gross move minus that coin's mean move over every feed time of the same
//! period, so a bucket full of BTC is measured against BTC. Both directions count: a
//! negative excess after bullish talk is as much a finding as a positive one. Periods:
//! the older and the newer half of the feed times, and the whole window.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bb_binance::history::HistoryFetcher;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common;
use crate::entry_study::{self, Candle, Series};
use crate::error::{Result, StrategyError};

const MINUTE_MS: i64 = 60_000;
/// The feed format biz-app writes.
pub const FEED_SCHEMA: &str = "biz-feed-v1";
/// Forward horizons in minutes (+1h, +6h, +24h).
pub const HORIZONS: [i64; 3] = [60, 360, 1440];
/// Period names: older half, newer half, whole window.
pub const PERIODS: [&str; 3] = ["older", "newer", "whole"];
/// The `coin` of the rows pooled over every studied coin.
pub const ALL_COINS: &str = "*";
/// 24h mentions a coin needs before its 6h-vs-24h surge is classified.
const SURGE_MIN_MENTIONS: u64 = 4;
/// Coins shown per coin in the Markdown report.
const MARKDOWN_COINS: usize = 10;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Folder of biz-feed-v1 files (`biz-insights feed --history`).
    #[arg(long)]
    pub feeds: PathBuf,
    /// Directory of cached 1m candles (shared with `bb entry-study`).
    #[arg(long, default_value = "data/entry-study-cache")]
    pub cache_dir: PathBuf,
    /// Minutes between a feed's `as_of` (rounded up to the minute) and the paper entry.
    #[arg(long, default_value_t = 1)]
    pub delay_minutes: i64,
    /// Round-trip fee in bps (the bot paid ~15 with the BNB discount).
    #[arg(long, default_value_t = 15.0)]
    pub fee_bps: f64,
    /// A coin is studied when its 24h window has mentions in at least this many feeds.
    #[arg(long, default_value_t = 48)]
    pub min_feeds: usize,
    /// At most this many coins, the most mentioned first.
    #[arg(long, default_value_t = 30)]
    pub top: usize,
    /// Distinct posters a window needs before its stance counts.
    #[arg(long, default_value_t = 2)]
    pub min_posters: u64,
    /// Folder for `biz_lead_study_<date>.md` and `.csv`.
    #[arg(long, default_value = "reports")]
    pub report_dir: PathBuf,
    /// Also write the full JSON report here.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// One coin's numbers over one window of a feed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Window {
    /// Posts naming the coin (every post, no AI).
    pub mentions: u64,
    /// Distinct posters among them.
    pub posters: u64,
    /// AI bullish tags weighted by post score (1-3).
    pub bullish: f64,
    /// AI bearish tags weighted by post score (1-3).
    pub bearish: f64,
}

/// One coin's windows in a feed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CoinWindows {
    #[serde(rename = "1h")]
    pub h1: Window,
    #[serde(rename = "6h")]
    pub h6: Window,
    #[serde(rename = "24h")]
    pub h24: Window,
}

#[derive(Debug, Deserialize)]
struct FeedFile {
    schema: String,
    as_of: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt_version: String,
    #[serde(default)]
    coins: BTreeMap<String, CoinWindows>,
}

/// One feed as the study uses it.
#[derive(Debug, Clone, PartialEq)]
pub struct Feed {
    /// Only posts visible by this time are in the feed.
    pub as_of_ms: i64,
    /// `model@prompt_version` of the AI tags.
    pub reader: String,
    pub coins: BTreeMap<String, CoinWindows>,
}

/// Parse one `biz-feed-v1` file; any other schema is an error.
pub fn parse_feed(text: &str, path: &Path) -> Result<Feed> {
    let file: FeedFile = serde_json::from_str(text).map_err(|e| common::json_error(path, &e))?;
    if file.schema != FEED_SCHEMA {
        return Err(StrategyError::InvalidConfig(format!(
            "{}: schema {:?}, expected {FEED_SCHEMA}",
            path.display(),
            file.schema
        )));
    }
    let as_of = common::parse_iso(&file.as_of).map_err(|e| {
        StrategyError::InvalidConfig(format!("{}: as_of: {e}", path.display()))
    })?;
    Ok(Feed {
        as_of_ms: as_of.timestamp_millis(),
        reader: format!("{}@{}", file.model, file.prompt_version),
        coins: file
            .coins
            .into_iter()
            .map(|(coin, windows)| (coin.trim().to_ascii_uppercase(), windows))
            .collect(),
    })
}

/// Every `*.json` feed in `dir`, oldest first; of two feeds with the same `as_of` the
/// first by file name is kept.
pub fn load_feeds(dir: &Path) -> Result<Vec<Feed>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| StrategyError::io(dir, e))? {
        let path = entry.map_err(|e| StrategyError::io(dir, e))?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut feeds = Vec::with_capacity(paths.len());
    for path in &paths {
        let text = std::fs::read_to_string(path).map_err(|e| StrategyError::io(path, e))?;
        feeds.push(parse_feed(&text, path)?);
    }
    feeds.sort_by_key(|f| f.as_of_ms);
    feeds.dedup_by_key(|f| f.as_of_ms);
    if feeds.is_empty() {
        return Err(StrategyError::InvalidConfig(format!(
            "no {FEED_SCHEMA} files in {}",
            dir.display()
        )));
    }
    Ok(feeds)
}

/// Tradable coins mentioned (24h window) in at least `min_feeds` feeds, the most
/// mentioned (sum of the 1h windows) first, at most `top`.
pub fn study_coins(
    feeds: &[Feed],
    tradable: &BTreeSet<String>,
    min_feeds: usize,
    top: usize,
) -> Vec<String> {
    let mut seen: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    for feed in feeds {
        for (coin, windows) in &feed.coins {
            if windows.h24.mentions > 0 {
                let entry = seen.entry(coin.as_str()).or_default();
                entry.0 += 1;
                entry.1 += windows.h1.mentions;
            }
        }
    }
    let mut ranked: Vec<(&str, u64)> = seen
        .into_iter()
        .filter(|(coin, (seen_in, _))| *seen_in >= min_feeds && tradable.contains(*coin))
        .map(|(coin, (_, mentions))| (coin, mentions))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    ranked
        .into_iter()
        .take(top)
        .map(|(coin, _)| coin.to_owned())
        .collect()
}

/// Open of the paper entry minute: the first whole minute at or after `as_of`, plus the
/// delay.
pub fn entry_minute(as_of_ms: i64, delay_minutes: i64) -> i64 {
    let up = (MINUTE_MS - as_of_ms.rem_euclid(MINUTE_MS)) % MINUTE_MS;
    as_of_ms + up + delay_minutes * MINUTE_MS
}

/// What the feed said about a coin at a feed time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Features {
    pub mentions_6h: u64,
    pub mentions_24h: u64,
    /// Rank among the studied coins by 24h mentions at this feed (1 = most talked
    /// about; 0 = not mentioned).
    pub rank_24h: usize,
    /// `bullish`, `bearish`, `split` or `untagged`; `None` with too few posters.
    pub stance_6h: Option<&'static str>,
    pub stance_24h: Option<&'static str>,
}

fn stance(window: &Window, min_posters: u64) -> Option<&'static str> {
    if window.posters < min_posters {
        return None;
    }
    let net = window.bullish - window.bearish;
    Some(if window.bullish + window.bearish <= 0.0 {
        "untagged"
    } else if net > 0.0 {
        "bullish"
    } else if net < 0.0 {
        "bearish"
    } else {
        "split"
    })
}

/// One feed time × studied coin.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub coin: String,
    /// Open of the entry minute.
    pub t: i64,
    pub features: Features,
    /// Round-trip cost in percent (fee + spread proxy).
    pub cost_pct: f64,
    /// Gross move in percent from the entry open to the close of the minute ending each
    /// of [`HORIZONS`] later; `None` when that candle is missing.
    pub forward: [Option<f64>; 3],
}

fn pct(from: f64, to: f64) -> Option<f64> {
    (from > 0.0 && to > 0.0).then(|| (to / from - 1.0) * 100.0)
}

/// Every feed × coin of `series` (keyed by coin) with an entry candle and at least one
/// forward close.
pub fn samples(
    feeds: &[Feed],
    series: &BTreeMap<String, Series>,
    delay_minutes: i64,
    fee_bps: f64,
    min_posters: u64,
) -> Vec<Sample> {
    let mut out = Vec::new();
    for feed in feeds {
        let t = entry_minute(feed.as_of_ms, delay_minutes);
        let windows = |coin: &str| feed.coins.get(coin).copied().unwrap_or_default();
        let mut by_talk: Vec<(&str, u64)> = series
            .keys()
            .map(|coin| (coin.as_str(), windows(coin).h24.mentions))
            .filter(|(_, mentions)| *mentions > 0)
            .collect();
        by_talk.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        for (coin, s) in series {
            let Some(entry) = s.at(t) else { continue };
            let forward = HORIZONS.map(|h| {
                s.at(t + (h - 1) * MINUTE_MS)
                    .and_then(|c: &Candle| pct(entry.o, c.c))
            });
            if forward.iter().all(Option::is_none) {
                continue;
            }
            let w = windows(coin);
            out.push(Sample {
                coin: coin.clone(),
                t,
                features: Features {
                    mentions_6h: w.h6.mentions,
                    mentions_24h: w.h24.mentions,
                    rank_24h: by_talk
                        .iter()
                        .position(|(c, _)| *c == coin.as_str())
                        .map_or(0, |i| i + 1),
                    stance_6h: stance(&w.h6, min_posters),
                    stance_24h: stance(&w.h24, min_posters),
                },
                cost_pct: (fee_bps + entry_study::spread_bps(entry)) / 100.0,
                forward,
            });
        }
    }
    out
}

type Classifier = fn(&Features) -> Option<&'static str>;

/// Signal families: name and classifier. `all hours` is the random-hours baseline.
pub fn families() -> [(&'static str, Classifier); 5] {
    [
        ("all hours", |_| Some("all")),
        ("talk rank 24h", |f| {
            Some(match f.rank_24h {
                0 => "not mentioned",
                1..=3 => "top 3",
                4..=10 => "4-10",
                _ => "11+",
            })
        }),
        ("talk surge 6h vs 24h", |f| {
            (f.mentions_24h >= SURGE_MIN_MENTIONS).then(|| {
                let ratio = f.mentions_6h as f64 * 4.0 / f.mentions_24h as f64;
                if ratio < 1.0 {
                    "cooling (<1x)"
                } else if ratio < 2.0 {
                    "steady (1-2x)"
                } else {
                    "surging (>=2x)"
                }
            })
        }),
        ("stance 6h", |f| f.stance_6h),
        ("stance 24h", |f| f.stance_24h),
    ]
}

/// One coin (or [`ALL_COINS`]) × signal bucket × period × horizon.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub coin: String,
    pub family: &'static str,
    pub bucket: &'static str,
    pub period: &'static str,
    pub horizon_min: i64,
    pub n: usize,
    pub mean_gross_pct: f64,
    pub mean_net_pct: f64,
    /// Mean of each sample's gross move minus its coin's mean over the period.
    pub mean_excess_pct: f64,
    /// Share of samples with a positive gross move.
    pub up_share: f64,
    pub t_excess: f64,
    /// `t_excess` with `n` shrunk by the overlap of the forward windows
    /// (`horizon / step` samples per independent window of one coin).
    pub t_excess_conservative: f64,
}

/// `(gross, net, excess)` of one sample at one horizon.
type Move = (f64, f64, f64);

fn row(
    coin: &str,
    (family, bucket, period, h): (&'static str, &'static str, usize, usize),
    moves: &[Move],
    step_minutes: i64,
) -> Row {
    let n = moves.len() as f64;
    let (mut gross, mut net, mut up) = (0.0, 0.0, 0usize);
    for (g, nt, _) in moves {
        gross += g;
        net += nt;
        up += usize::from(*g > 0.0);
    }
    let excess: Vec<f64> = moves.iter().map(|m| m.2).collect();
    let mean_excess = excess.iter().sum::<f64>() / n;
    let sd = common::stdev(&excess);
    let t = if sd > 0.0 {
        mean_excess / (sd / n.sqrt())
    } else {
        0.0
    };
    let overlap = (HORIZONS[h] as f64 / step_minutes as f64).max(1.0);
    Row {
        coin: coin.to_owned(),
        family,
        bucket,
        period: PERIODS[period],
        horizon_min: HORIZONS[h],
        n: moves.len(),
        mean_gross_pct: gross / n,
        mean_net_pct: net / n,
        mean_excess_pct: mean_excess,
        up_share: up as f64 / n,
        t_excess: t,
        t_excess_conservative: t / overlap.sqrt(),
    }
}

/// Rows per coin and pooled over every coin, for every family bucket, period and horizon.
/// Samples before `split_ms` are the older half.
pub fn rows(samples: &[Sample], split_ms: i64, step_minutes: i64) -> Vec<Row> {
    let periods = |s: &Sample| [usize::from(s.t >= split_ms), 2];
    let mut baseline: BTreeMap<(&str, usize, usize), (f64, usize)> = BTreeMap::new();
    for s in samples {
        for p in periods(s) {
            for (h, gross) in s.forward.iter().enumerate() {
                if let Some(gross) = gross {
                    let entry = baseline.entry((s.coin.as_str(), p, h)).or_default();
                    entry.0 += gross;
                    entry.1 += 1;
                }
            }
        }
    }
    let families = families();
    let mut groups: BTreeMap<(&str, usize, &'static str, usize, usize), Vec<Move>> =
        BTreeMap::new();
    for s in samples {
        for (family, (_, classify)) in families.iter().enumerate() {
            let Some(bucket) = classify(&s.features) else {
                continue;
            };
            for p in periods(s) {
                for (h, gross) in s.forward.iter().enumerate() {
                    let Some(gross) = *gross else { continue };
                    let mean = baseline
                        .get(&(s.coin.as_str(), p, h))
                        .map_or(gross, |(sum, n)| sum / *n as f64);
                    let moved = (gross, gross - s.cost_pct, gross - mean);
                    for coin in [ALL_COINS, s.coin.as_str()] {
                        groups
                            .entry((coin, family, bucket, p, h))
                            .or_default()
                            .push(moved);
                    }
                }
            }
        }
    }
    groups
        .iter()
        .map(|((coin, family, bucket, p, h), moves)| {
            row(coin, (families[*family].0, *bucket, *p, *h), moves, step_minutes)
        })
        .collect()
}

/// Median minutes between consecutive feeds (60 with a single feed).
pub fn step_minutes(feeds: &[Feed]) -> i64 {
    let mut gaps: Vec<i64> = feeds
        .windows(2)
        .map(|pair| (pair[1].as_of_ms - pair[0].as_of_ms) / MINUTE_MS)
        .collect();
    gaps.sort_unstable();
    gaps.get(gaps.len() / 2).copied().unwrap_or(60).max(1)
}

/// Candles from any cache file of `symbol` whose range covers `[start_ms, end_ms]`
/// (`bb entry-study` names cache files `1m_<BASE>_USDT_<start s>_<end s>.json`).
pub fn cached_candles(
    cache_dir: &Path,
    symbol: &str,
    start_ms: i64,
    end_ms: i64,
) -> Option<Vec<Candle>> {
    let prefix = format!("1m_{}_", symbol.replace('/', "_"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(cache_dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    paths.sort();
    paths.into_iter().find_map(|path| {
        let name = path.file_name()?.to_str()?;
        let (from, to) = name
            .strip_prefix(&prefix)?
            .strip_suffix(".json")?
            .split_once('_')?;
        let (from, to): (i64, i64) = (from.parse().ok()?, to.parse().ok()?);
        (from * 1000 <= start_ms && to * 1000 >= end_ms).then_some(())?;
        serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()
    })
}

/// The JSON report.
#[derive(Debug, Serialize)]
pub struct Report {
    pub generated_at: String,
    pub feeds_dir: String,
    /// Every `model@prompt_version` seen in the feeds.
    pub readers: Vec<String>,
    pub first_feed: String,
    pub split: String,
    pub last_feed: String,
    pub feeds: usize,
    pub step_minutes: i64,
    pub delay_minutes: i64,
    pub fee_bps: f64,
    pub min_posters: u64,
    /// Studied coins, the most mentioned first.
    pub coins: Vec<String>,
    pub samples: usize,
    pub rows: Vec<Row>,
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(String::new, |t| t.to_rfc3339())
}

fn header(report: &Report) -> String {
    let mut out = format!(
        "# /biz/ lead study\n\n{} to {} ({} feeds, one every {} min; the older half ends at {}), {} coins: {}. {} coin-feed samples. Entry: open of the first whole minute after the feed's `as_of` plus {} min. Net = gross minus {} bps fee and the spread proxy. Excess = gross minus the same coin's mean over every feed time of the period (its random hours). t_c = t corrected for overlapping windows; coins move together, so pooled t still overstates. Stances count with at least {} posters in the window.\n\n",
        report.first_feed,
        report.last_feed,
        report.feeds,
        report.step_minutes,
        report.split,
        report.coins.len(),
        report.coins.join(", "),
        report.samples,
        report.delay_minutes,
        report.fee_bps,
        report.min_posters,
    );
    if report.readers.len() > 1 {
        out.push_str(&format!(
            "**Mixed AI readers:** {}. Stance rows mix verdicts of different models or instructions.\n\n",
            report.readers.join(", ")
        ));
    }
    out
}

fn pooled_table(rows: &[&Row]) -> String {
    let mut out = String::from(
        "| family | bucket | period | horizon | n | mean gross | mean net | excess | up share | t | t_c |\n|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for r in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {}m | {} | {:+.3}% | {:+.3}% | {:+.3}% | {:.0}% | {:.1} | {:.1} |\n",
            r.family,
            r.bucket,
            r.period,
            r.horizon_min,
            r.n,
            r.mean_gross_pct,
            r.mean_net_pct,
            r.mean_excess_pct,
            r.up_share * 100.0,
            r.t_excess,
            r.t_excess_conservative
        ));
    }
    out
}

/// Header and the pooled rows at +6h (printed on stdout).
pub fn summary(report: &Report) -> String {
    let pooled: Vec<&Row> = report
        .rows
        .iter()
        .filter(|r| r.coin == ALL_COINS && r.horizon_min == HORIZONS[1])
        .collect();
    format!(
        "{}## All coins at +6h\n\n{}",
        header(report),
        pooled_table(&pooled)
    )
}

/// The Markdown report: every pooled row, then the most mentioned coins' surge and
/// stance rows at +6h over the whole window.
pub fn markdown(report: &Report) -> String {
    let pooled: Vec<&Row> = report.rows.iter().filter(|r| r.coin == ALL_COINS).collect();
    let mut out = format!(
        "{}## All coins\n\n{}\n## Per coin at +6h, whole window\n\n| coin | family | bucket | n | mean gross | excess | t_c |\n|---|---|---|---:|---:|---:|---:|\n",
        header(report),
        pooled_table(&pooled)
    );
    for coin in report.coins.iter().take(MARKDOWN_COINS) {
        for r in report.rows.iter().filter(|r| {
            &r.coin == coin
                && r.period == PERIODS[2]
                && r.horizon_min == HORIZONS[1]
                && matches!(r.family, "talk surge 6h vs 24h" | "stance 6h")
        }) {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {:+.3}% | {:+.3}% | {:.1} |\n",
                r.coin,
                r.family,
                r.bucket,
                r.n,
                r.mean_gross_pct,
                r.mean_excess_pct,
                r.t_excess_conservative
            ));
        }
    }
    out.push_str("\nEvery coin, period and horizon: the CSV next to this file.\n");
    out
}

fn csv_bytes(rows: &[Row], path: &Path) -> Result<Vec<u8>> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    for row in rows {
        writer
            .serialize(row)
            .map_err(|e| StrategyError::io(path, e.into()))?;
    }
    writer
        .into_inner()
        .map_err(|e| StrategyError::io(path, e.into_error()))
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    if args.delay_minutes < 0 || args.top == 0 {
        return Ok(crate::cli::usage_error(
            "biz-lead-study",
            "--delay-minutes must not be negative and --top must be positive",
        ));
    }
    let feeds = load_feeds(&args.feeds)?;
    let (first, last) = (&feeds[0], &feeds[feeds.len() - 1]);
    let client = common::public_client()?;
    let tradable: BTreeSet<String> = entry_study::universe(&client, usize::MAX)
        .await?
        .into_iter()
        .filter_map(|symbol| symbol.strip_suffix("/USDT").map(str::to_owned))
        .collect();
    let coins = study_coins(&feeds, &tradable, args.min_feeds, args.top);
    if coins.is_empty() {
        return Ok(crate::cli::usage_error(
            "biz-lead-study",
            "no coin with a Binance USDT market is mentioned in --min-feeds feeds",
        ));
    }
    let start_ms = entry_minute(first.as_of_ms, args.delay_minutes);
    let now_ms = bb_core::time::now_ms();
    let end_ms = (entry_minute(last.as_of_ms, args.delay_minutes) + HORIZONS[2] * MINUTE_MS)
        .min(now_ms - now_ms.rem_euclid(MINUTE_MS) - MINUTE_MS);
    if end_ms <= start_ms {
        return Ok(crate::cli::usage_error(
            "biz-lead-study",
            "the feeds are too recent: no candle after their first entry has closed yet",
        ));
    }
    eprintln!(
        "biz-lead-study: {} feeds, {} coins: {}",
        feeds.len(),
        coins.len(),
        coins.join(", ")
    );
    let fetcher = HistoryFetcher::spot(client.clone());
    let mut series = BTreeMap::new();
    for coin in &coins {
        let symbol = format!("{coin}/USDT");
        let candles = match cached_candles(&args.cache_dir, &symbol, start_ms, end_ms) {
            Some(candles) => candles,
            None => {
                entry_study::candles(&fetcher, &symbol, start_ms, end_ms, &args.cache_dir).await?
            }
        };
        series.insert(coin.clone(), Series::new(start_ms, end_ms, &candles));
    }
    let samples = samples(
        &feeds,
        &series,
        args.delay_minutes,
        args.fee_bps,
        args.min_posters,
    );
    let split_ms = entry_minute(
        first.as_of_ms + (last.as_of_ms - first.as_of_ms) / 2,
        args.delay_minutes,
    );
    let step = step_minutes(&feeds);
    let generated = bb_core::time::now();
    let report = Report {
        generated_at: generated.to_rfc3339(),
        feeds_dir: args.feeds.display().to_string(),
        readers: feeds
            .iter()
            .map(|f| f.reader.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        first_feed: iso(first.as_of_ms),
        split: iso(split_ms),
        last_feed: iso(last.as_of_ms),
        feeds: feeds.len(),
        step_minutes: step,
        delay_minutes: args.delay_minutes,
        fee_bps: args.fee_bps,
        min_posters: args.min_posters,
        coins,
        samples: samples.len(),
        rows: rows(&samples, split_ms, step),
    };
    std::fs::create_dir_all(&args.report_dir)
        .map_err(|e| StrategyError::io(&args.report_dir, e))?;
    let stem = format!("biz_lead_study_{}", generated.format("%Y-%m-%d"));
    let md_path = args.report_dir.join(format!("{stem}.md"));
    let csv_path = args.report_dir.join(format!("{stem}.csv"));
    std::fs::write(&md_path, markdown(&report)).map_err(|e| StrategyError::io(&md_path, e))?;
    std::fs::write(&csv_path, csv_bytes(&report.rows, &csv_path)?)
        .map_err(|e| StrategyError::io(&csv_path, e))?;
    if let Some(output) = &args.output {
        common::write_report(output, &report)?;
    }
    println!("{}", summary(&report));
    eprintln!(
        "biz-lead-study: wrote {} and {}",
        md_path.display(),
        csv_path.display()
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(t: i64, price: f64) -> Candle {
        Candle {
            t,
            o: price,
            h: price,
            l: price,
            c: price,
            v: 1.0,
        }
    }

    #[test]
    fn a_feed_of_another_schema_is_refused_and_v1_windows_are_read() {
        let path = Path::new("feed.json");
        let other = r#"{"schema":"biz-feed-v2","as_of":"2026-10-20T10:00:00Z","coins":{}}"#;
        assert!(parse_feed(other, path).is_err());
        let v1 = r#"{"schema":"biz-feed-v1","as_of":"2026-10-20T10:00:00Z","model":"m","prompt_version":"p",
            "coins":{"xmr":{"6h":{"mentions":7,"posters":4,"bullish":3.0,"bearish":1.0,"posts":[62804123]}}}}"#;
        let feed = parse_feed(v1, path).unwrap();
        assert_eq!(feed.reader, "m@p");
        let xmr = feed.coins["XMR"];
        assert_eq!(xmr.h6.mentions, 7);
        assert_eq!(xmr.h24, Window::default());
    }

    #[test]
    fn the_entry_waits_for_the_next_whole_minute_plus_the_delay() {
        // 2000 flat minutes at 100; the minute ending 1h after the entry closes at 102.
        let mut candles: Vec<Candle> = (0..2000).map(|m| flat(m * MINUTE_MS, 100.0)).collect();
        candles[12 + 59].c = 102.0;
        candles[12 + 59].h = 102.0;
        let s = Series::new(0, 1999 * MINUTE_MS, &candles);
        let series = BTreeMap::from([("AAA".to_owned(), s.clone()), ("BBB".to_owned(), s)]);
        let window = |mentions, posters, bullish| Window {
            mentions,
            posters,
            bullish,
            bearish: 0.0,
        };
        let feed = Feed {
            as_of_ms: 10 * MINUTE_MS + 30_000,
            reader: String::new(),
            coins: BTreeMap::from([
                (
                    "AAA".to_owned(),
                    CoinWindows {
                        h6: window(3, 3, 2.0),
                        h24: window(5, 3, 2.0),
                        ..Default::default()
                    },
                ),
                (
                    "BBB".to_owned(),
                    CoinWindows {
                        h6: window(1, 1, 3.0),
                        h24: window(9, 1, 3.0),
                        ..Default::default()
                    },
                ),
            ]),
        };
        let out = samples(&[feed], &series, 1, 15.0, 2);
        assert_eq!(out.len(), 2);
        let aaa = &out[0];
        assert_eq!(aaa.coin, "AAA");
        assert_eq!(aaa.t, 12 * MINUTE_MS);
        assert!((aaa.forward[0].unwrap() - 2.0).abs() < 1e-9, "+1h");
        assert!(aaa.forward[2].unwrap().abs() < 1e-9, "+24h");
        assert!((aaa.cost_pct - 0.16).abs() < 1e-12);
        assert_eq!(aaa.features.rank_24h, 2);
        assert_eq!(aaa.features.stance_6h, Some("bullish"));
        // One poster is too few for a stance, whatever the tags say.
        assert_eq!(out[1].features.rank_24h, 1);
        assert_eq!(out[1].features.stance_6h, None);
    }

    #[test]
    fn excess_is_measured_against_the_same_coins_hours_of_the_same_period() {
        let mk = |coin: &str, t: i64, gross: f64, stance: &'static str| Sample {
            coin: coin.into(),
            t,
            features: Features {
                stance_6h: Some(stance),
                ..Default::default()
            },
            cost_pct: 0.2,
            forward: [Some(gross), None, None],
        };
        // A averages 0 and B averages 2 in the older half: bullish beats both by 1.
        let samples = vec![
            mk("A", 0, 1.0, "bullish"),
            mk("A", 0, -1.0, "untagged"),
            mk("B", 0, 3.0, "bullish"),
            mk("B", 0, 1.0, "untagged"),
            mk("B", 20, -5.0, "bearish"),
        ];
        let rows = rows(&samples, 10, 60);
        let get = |coin: &str, bucket: &str, period: &str| {
            rows.iter().find(|r| {
                r.coin == coin && r.family == "stance 6h" && r.bucket == bucket && r.period == period
            })
        };
        let pooled = get(ALL_COINS, "bullish", "older").unwrap();
        assert_eq!(pooled.n, 2);
        assert!((pooled.mean_gross_pct - 2.0).abs() < 1e-12);
        assert!((pooled.mean_net_pct - 1.8).abs() < 1e-12);
        assert!((pooled.mean_excess_pct - 1.0).abs() < 1e-12);
        assert!(get(ALL_COINS, "bullish", "newer").is_none());
        // Over the whole window B averages -1/3, so its bullish hour beats it by 10/3.
        let b_whole = get("B", "bullish", "whole").unwrap();
        assert!((b_whole.mean_excess_pct - 10.0 / 3.0).abs() < 1e-12);
        // The only newer sample is its coin's whole newer half: no excess.
        assert!(get("B", "bearish", "newer").unwrap().mean_excess_pct.abs() < 1e-12);
    }
}

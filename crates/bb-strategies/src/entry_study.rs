//! `bb entry-study`: when does buying a liquid Binance USDT coin pay after costs?
//!
//! Research only (public klines, no account, no order path). Over the most-traded USDT crypto
//! markets it samples every `--step-minutes` mark × coin, records what the bot can see at that
//! moment (returns over 5m to 24h, volume ratio, market breadth) and what happened next (the
//! move to +20/+60/+90/+120 minutes), then reports the mean net-of-cost forward return per
//! bucket of each feature, train period vs test period. `--audit` also measures the bot's real
//! entries with the same candles, to check the harness against reality.
//!
//! Prices: an entry at mark `t` buys at the open of the minute starting at `t`; a return over
//! `k` minutes before it is `open(t) / open(t - k) - 1`; the forward return to `+h` is
//! `close of the minute starting at t + h - 1` over `open(t)`. Costs: `--fee-bps` per round
//! trip plus a spread proxy, half the entry minute's high-low range, clipped to 1-10 bps.
//! The universe is ranked by today's volume, so it carries survivorship bias.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bb_binance::history::HistoryFetcher;
use bb_binance::spot::SpotSymbolInfo;
use bb_binance::{BinanceClient, fiat};
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{self, decimal_f64};
use crate::error::{Result, StrategyError};

const MINUTE_MS: i64 = 60_000;
/// Forward horizons in minutes.
pub const HORIZONS: [i64; 4] = [20, 60, 90, 120];
/// Lookbacks in minutes for the return features (5m, 15m, 30m, 1h, 4h, 24h).
pub const LOOKBACKS: [i64; 6] = [5, 15, 30, 60, 240, 1440];
/// Binance's permission group for tokenized stocks/ETFs (observed 2026-10-17, undocumented).
const TOKENIZED_STOCK_GROUP: &str = "TRD_GRP_261";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Days of history (ending now, or at --end).
    #[arg(long, default_value_t = 90)]
    pub days: i64,
    /// ISO end timestamp; defaults to now rounded down to the minute.
    #[arg(long, default_value = "")]
    pub end: String,
    /// Number of USDT crypto markets (by 24h quote volume).
    #[arg(long, default_value_t = 20)]
    pub top: usize,
    /// Comma-separated symbols (e.g. BTC/USDT,ETH/USDT) instead of the volume ranking.
    #[arg(long, default_value = "")]
    pub symbols: String,
    /// Minutes between sampled marks (the bot cycles every 20).
    #[arg(long, default_value_t = 20)]
    pub step_minutes: i64,
    /// Round-trip fee in bps (the bot paid ~15 with the BNB discount).
    #[arg(long, default_value_t = 15.0)]
    pub fee_bps: f64,
    /// Days at the end held out as the test period.
    #[arg(long, default_value_t = 30)]
    pub test_days: i64,
    /// Directory for cached 1m candles.
    #[arg(long, default_value = "data/entry-study-cache")]
    pub cache_dir: String,
    /// Bot Spot audit JSONL whose real entries are measured with the same candles.
    #[arg(long, default_value = "")]
    pub audit: String,
    /// Write the JSON report here.
    #[arg(long, default_value = "")]
    pub output: String,
    /// Write the Markdown summary here (else print it).
    #[arg(long, default_value = "")]
    pub markdown: String,
}

/// One 1-minute candle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
}

/// A symbol's candles indexed by minute from `start_ms`.
#[derive(Debug, Clone)]
pub struct Series {
    pub start_ms: i64,
    pub minutes: Vec<Option<Candle>>,
}

impl Series {
    pub fn new(start_ms: i64, end_ms: i64, candles: &[Candle]) -> Self {
        let len = usize::try_from((end_ms - start_ms) / MINUTE_MS + 1).unwrap_or(0);
        let mut minutes = vec![None; len];
        for candle in candles {
            if let Some(slot) = Self::index(start_ms, len, candle.t) {
                minutes[slot] = Some(*candle);
            }
        }
        Self { start_ms, minutes }
    }

    fn index(start_ms: i64, len: usize, t: i64) -> Option<usize> {
        if t < start_ms || (t - start_ms) % MINUTE_MS != 0 {
            return None;
        }
        usize::try_from((t - start_ms) / MINUTE_MS)
            .ok()
            .filter(|i| *i < len)
    }

    pub fn at(&self, t: i64) -> Option<&Candle> {
        Self::index(self.start_ms, self.minutes.len(), t).and_then(|i| self.minutes[i].as_ref())
    }

    /// Return in percent over `minutes` before the open of `t`.
    fn ret_before(&self, t: i64, minutes: i64) -> Option<f64> {
        pct(self.at(t - minutes * MINUTE_MS)?.o, self.at(t)?.o)
    }

    /// Gross return in percent from `price` to the close of the minute ending `h` after `t`.
    fn forward(&self, t: i64, price: f64, h: i64) -> Option<f64> {
        pct(price, self.at(t + (h - 1) * MINUTE_MS)?.c)
    }

    /// Volume of the last 30 minutes over the mean 30-minute volume of the prior 24 hours.
    fn volume_ratio(&self, t: i64) -> Option<f64> {
        let sum = |from: i64, to: i64| -> Option<f64> {
            let mut total = 0.0;
            let mut m = from;
            while m < to {
                total += self.at(m)?.v;
                m += MINUTE_MS;
            }
            Some(total)
        };
        let recent = sum(t - 30 * MINUTE_MS, t)?;
        let day = sum(t - 1470 * MINUTE_MS, t - 30 * MINUTE_MS)?;
        (day > 0.0).then(|| recent / (day / 48.0))
    }
}

fn pct(from: f64, to: f64) -> Option<f64> {
    (from > 0.0 && to > 0.0).then(|| (to / from - 1.0) * 100.0)
}

/// One sampled mark × coin.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub symbol: String,
    pub t: i64,
    /// Percent returns over [`LOOKBACKS`].
    pub returns: [f64; 6],
    pub volume_ratio: Option<f64>,
    /// Share of the universe up over the last hour at `t`.
    pub breadth_1h: f64,
    /// Round-trip cost in percent (fee + spread proxy).
    pub cost_pct: f64,
    /// Gross forward returns in percent over [`HORIZONS`].
    pub forward: [f64; 4],
}

/// Spread proxy in bps: half the minute's high-low range, clipped to 1-10.
fn spread_bps(candle: &Candle) -> f64 {
    let mid = (candle.h + candle.l) / 2.0;
    if mid <= 0.0 {
        return 10.0;
    }
    ((candle.h - candle.l) / mid * 10_000.0 / 2.0).clamp(1.0, 10.0)
}

/// Every complete sample on the `step_ms` grid from `from_ms` to `to_ms` (inclusive).
pub fn samples(
    series: &BTreeMap<String, Series>,
    from_ms: i64,
    to_ms: i64,
    step_ms: i64,
    fee_bps: f64,
) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut t = from_ms;
    while t <= to_ms {
        let hour: Vec<f64> = series
            .values()
            .filter_map(|s| s.ret_before(t, 60))
            .collect();
        let breadth = if hour.is_empty() {
            0.0
        } else {
            hour.iter().filter(|r| **r > 0.0).count() as f64 / hour.len() as f64
        };
        for (symbol, s) in series {
            let Some(entry) = s.at(t) else { continue };
            let returns: Option<Vec<f64>> = LOOKBACKS.iter().map(|k| s.ret_before(t, *k)).collect();
            let forward: Option<Vec<f64>> =
                HORIZONS.iter().map(|h| s.forward(t, entry.o, *h)).collect();
            let (Some(returns), Some(forward)) = (returns, forward) else {
                continue;
            };
            out.push(Sample {
                symbol: symbol.clone(),
                t,
                returns: returns.try_into().unwrap_or([0.0; 6]),
                volume_ratio: s.volume_ratio(t),
                breadth_1h: breadth,
                cost_pct: (fee_bps + spread_bps(entry)) / 100.0,
                forward: forward.try_into().unwrap_or([0.0; 4]),
            });
        }
        t += step_ms;
    }
    out
}

/// Mean, median and t-statistics of one bucket's net forward returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BucketStat {
    pub family: String,
    pub bucket: String,
    pub period: &'static str,
    pub horizon_min: i64,
    pub n: usize,
    pub mean_net_pct: f64,
    pub median_net_pct: f64,
    pub mean_gross_pct: f64,
    pub t: f64,
    /// t with `n` shrunk by the overlap of the forward windows (`horizon / step` samples per
    /// independent window of one coin).
    pub t_conservative: f64,
}

fn stat(
    family: &str,
    bucket: &str,
    period: &'static str,
    h: usize,
    step_min: i64,
    rows: &[&Sample],
) -> Option<BucketStat> {
    if rows.is_empty() {
        return None;
    }
    let net: Vec<f64> = rows.iter().map(|s| s.forward[h] - s.cost_pct).collect();
    let n = net.len() as f64;
    let mean = net.iter().sum::<f64>() / n;
    let sd = common::stdev(&net);
    let mut sorted = net.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let t = if sd > 0.0 {
        mean / (sd / n.sqrt())
    } else {
        0.0
    };
    let overlap = (HORIZONS[h] as f64 / step_min as f64).max(1.0);
    Some(BucketStat {
        family: family.into(),
        bucket: bucket.into(),
        period,
        horizon_min: HORIZONS[h],
        n: net.len(),
        mean_net_pct: mean,
        median_net_pct: median,
        mean_gross_pct: rows.iter().map(|s| s.forward[h]).sum::<f64>() / n,
        t,
        t_conservative: t / overlap.sqrt(),
    })
}

type Classifier = fn(&Sample) -> Option<&'static str>;

/// Bucket families: name and classifier.
pub fn families() -> Vec<(&'static str, Classifier)> {
    vec![
        ("all", |_| Some("all")),
        ("run-up 30m", |s| {
            let r = s.returns[2];
            Some(match r {
                r if r <= -1.0 => "<= -1%",
                r if r <= -0.25 => "-1..-0.25%",
                r if r < 0.25 => "-0.25..+0.25%",
                r if r < 1.0 => "+0.25..+1%",
                _ => ">= +1%",
            })
        }),
        ("trend 1h", |s| {
            Some(if s.returns[3] > 0.0 { "up" } else { "down" })
        }),
        ("trend 4h", |s| {
            Some(if s.returns[4] > 0.0 { "up" } else { "down" })
        }),
        ("breadth 1h", |s| {
            Some(match s.breadth_1h {
                b if b < 1.0 / 3.0 => "< 1/3 up",
                b if b <= 2.0 / 3.0 => "1/3..2/3 up",
                _ => "> 2/3 up",
            })
        }),
        ("volume 30m vs 24h", |s| {
            s.volume_ratio.map(|v| match v {
                v if v < 0.75 => "< 0.75x",
                v if v < 1.5 => "0.75..1.5x",
                _ => ">= 1.5x",
            })
        }),
        // The bot's habit: buying after a rise while the hour is up.
        ("run-up >= +1% and 1h up", |s| {
            (s.returns[2] >= 1.0 && s.returns[3] > 0.0).then_some("yes")
        }),
        ("dip: 30m <= -1% and 4h up", |s| {
            (s.returns[2] <= -1.0 && s.returns[4] > 0.0).then_some("yes")
        }),
    ]
}

/// Bucket statistics for both periods and every horizon.
pub fn bucket_stats(samples: &[Sample], split_ms: i64, step_min: i64) -> Vec<BucketStat> {
    let mut out = Vec::new();
    for (family, classify) in families() {
        let mut groups: BTreeMap<&'static str, (Vec<&Sample>, Vec<&Sample>)> = BTreeMap::new();
        for s in samples {
            if let Some(bucket) = classify(s) {
                let group = groups.entry(bucket).or_default();
                if s.t < split_ms {
                    group.0.push(s)
                } else {
                    group.1.push(s)
                }
            }
        }
        for (bucket, (train, test)) in &groups {
            for h in 0..HORIZONS.len() {
                out.extend(stat(family, bucket, "train", h, step_min, train));
                out.extend(stat(family, bucket, "test", h, step_min, test));
            }
        }
    }
    out
}

/// One of the bot's real entries measured with the study's candles.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BotEntry {
    pub symbol: String,
    pub entry_ms: i64,
    pub price: f64,
    /// Open of the minute 30 minutes before the entry minute → the fill, percent.
    pub before_30m_pct: f64,
    /// Fill → close of the minute ending `h` after the entry minute, percent, per [`HORIZONS`].
    pub forward: [f64; 4],
}

/// USDT buys from a Bot Spot audit: `(symbol, fill ms, average price)`.
pub fn audit_entries(text: &str) -> Vec<(String, i64, f64)> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| {
            matches!(
                row.get("row_type").and_then(Value::as_str),
                None | Some("route")
            ) && row.get("status").and_then(Value::as_str) == Some("submitted")
        })
        .filter_map(|row| {
            let leg = row.get("submitted_legs")?.as_array()?.last()?;
            let order = leg
                .get("final_order")
                .filter(|o| o.is_object())
                .or_else(|| leg.get("order"))?;
            let symbol = leg.get("symbol")?.as_str()?;
            (leg.get("side")?.as_str()? == "buy" && symbol.ends_with("/USDT")).then_some(())?;
            let price = number(order.get("average")?)?;
            Some((symbol.to_owned(), order.get("timestamp")?.as_i64()?, price))
        })
        .collect()
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

/// Measure entries against `series` (`None` for entries whose candles are missing).
pub fn measure_entries(
    entries: &[(String, i64, f64)],
    series: &BTreeMap<String, Series>,
) -> Vec<BotEntry> {
    entries
        .iter()
        .filter_map(|(symbol, ms, price)| {
            let s = series.get(symbol)?;
            let minute = ms - ms.rem_euclid(MINUTE_MS);
            let forward: Option<Vec<f64>> = HORIZONS
                .iter()
                .map(|h| s.forward(minute, *price, *h))
                .collect();
            Some(BotEntry {
                symbol: symbol.clone(),
                entry_ms: *ms,
                price: *price,
                before_30m_pct: pct(s.at(minute - 30 * MINUTE_MS)?.o, *price)?,
                forward: forward?.try_into().ok()?,
            })
        })
        .collect()
}

/// The `top` most-traded USDT crypto markets (no fiat, stablecoins, leveraged or tokenized
/// stocks), as `BASE/USDT`.
pub async fn universe(client: &BinanceClient, top: usize) -> Result<Vec<String>> {
    let info = client.exchange_info(None).await?;
    let markets: HashMap<&str, &SpotSymbolInfo> = info
        .symbols
        .iter()
        .map(|s| (s.symbol.as_str(), s))
        .collect();
    let mut tickers = client.tickers_24h().await?;
    tickers.sort_by(|a, b| decimal_f64(b.quote_volume).total_cmp(&decimal_f64(a.quote_volume)));
    Ok(tickers
        .iter()
        .filter_map(|t| {
            let m = markets.get(t.symbol.as_str())?;
            let last = decimal_f64(t.last_price);
            let (high, low) = (decimal_f64(t.high_price), decimal_f64(t.low_price));
            let stable = (last - 1.0).abs() < 0.02 && low > 0.0 && (high / low - 1.0) < 0.02;
            let base = m.base_asset.as_str();
            (m.status == "TRADING"
                && m.quote_asset == "USDT"
                && !fiat::is_fiat(base)
                && !stable
                && !["UP", "DOWN", "BULL", "BEAR"]
                    .iter()
                    .any(|s| base.ends_with(s))
                && !m
                    .permission_sets
                    .iter()
                    .flatten()
                    .any(|g| g == TOKENIZED_STOCK_GROUP))
            .then(|| format!("{base}/USDT"))
        })
        .take(top)
        .collect())
}

/// 1m candles for `symbol` in `[start_ms, end_ms]`, from the cache when present.
async fn candles(
    fetcher: &HistoryFetcher,
    symbol: &str,
    start_ms: i64,
    end_ms: i64,
    cache_dir: &Path,
) -> Result<Vec<Candle>> {
    let path = cache_dir.join(format!(
        "1m_{}_{}_{}.json",
        symbol.replace('/', "_"),
        start_ms / 1000,
        end_ms / 1000
    ));
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(cached) = serde_json::from_str::<Vec<Candle>>(&text)
    {
        return Ok(cached);
    }
    let time = |ms: i64| {
        DateTime::<Utc>::from_timestamp_millis(ms)
            .ok_or_else(|| StrategyError::InvalidConfig(format!("bad time {ms}")))
    };
    let fetched: Vec<Candle> = fetcher
        .fetch_klines(symbol, "1m", time(start_ms)?, Some(time(end_ms)?))
        .await?
        .iter()
        .map(|k| Candle {
            t: k.open_time,
            o: decimal_f64(k.open),
            h: decimal_f64(k.high),
            l: decimal_f64(k.low),
            c: decimal_f64(k.close),
            v: decimal_f64(k.volume),
        })
        .collect();
    std::fs::create_dir_all(cache_dir).map_err(|e| StrategyError::io(cache_dir, e))?;
    std::fs::write(&path, serde_json::to_vec(&fetched)?)
        .map_err(|e| StrategyError::io(&path, e))?;
    Ok(fetched)
}

/// The JSON report.
#[derive(Debug, Serialize)]
pub struct Report {
    pub generated_at: String,
    pub symbols: Vec<String>,
    pub start: String,
    pub split: String,
    pub end: String,
    pub step_minutes: i64,
    pub fee_bps: f64,
    pub samples: usize,
    pub buckets: Vec<BucketStat>,
    pub bot_entries: Vec<BotEntry>,
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(String::new, |t| t.to_rfc3339())
}

/// Markdown: the bot-entry check, then every family at +90 min (train and test) and the
/// +20/+120 test rows.
pub fn markdown(report: &Report) -> String {
    let mut out = format!(
        "# Entry study\n\n{} to {} (test from {}), {} coins, a sample every {} min, {} samples. Net = forward return minus {} bps fee and the spread proxy. t_c = t corrected for overlapping windows.\n\n",
        report.start,
        report.end,
        report.split,
        report.symbols.len(),
        report.step_minutes,
        report.samples,
        report.fee_bps
    );
    if !report.bot_entries.is_empty() {
        let n = report.bot_entries.len() as f64;
        let mean = |f: &dyn Fn(&BotEntry) -> f64| report.bot_entries.iter().map(f).sum::<f64>() / n;
        out.push_str(&format!(
            "## The bot's real entries (gross, same candles)\n\n| entries | 30m before | +20m | +60m | +90m | +120m |\n|---:|---:|---:|---:|---:|---:|\n| {} | {:+.3}% | {:+.3}% | {:+.3}% | {:+.3}% | {:+.3}% |\n\n",
            report.bot_entries.len(),
            mean(&|e| e.before_30m_pct),
            mean(&|e| e.forward[0]),
            mean(&|e| e.forward[1]),
            mean(&|e| e.forward[2]),
            mean(&|e| e.forward[3]),
        ));
    }
    out.push_str("## Net forward return by bucket\n\n| family | bucket | period | horizon | n | mean net | median net | mean gross | t | t_c |\n|---|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for b in &report.buckets {
        out.push_str(&format!(
            "| {} | {} | {} | {}m | {} | {:+.3}% | {:+.3}% | {:+.3}% | {:.1} | {:.1} |\n",
            b.family,
            b.bucket,
            b.period,
            b.horizon_min,
            b.n,
            b.mean_net_pct,
            b.median_net_pct,
            b.mean_gross_pct,
            b.t,
            b.t_conservative
        ));
    }
    out
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    if args.days <= args.test_days || args.step_minutes <= 0 {
        return Ok(crate::cli::usage_error(
            "entry-study",
            "--days must exceed --test-days and --step-minutes must be positive",
        ));
    }
    let end = if args.end.is_empty() {
        bb_core::time::now()
    } else {
        common::parse_iso(&args.end)?
    };
    let end_ms = end.timestamp_millis() - end.timestamp_millis().rem_euclid(MINUTE_MS);
    let first_mark = end_ms - (args.days * 1440 + HORIZONS[3]) * MINUTE_MS;
    let last_mark = end_ms - HORIZONS[3] * MINUTE_MS;
    let data_start = first_mark - (LOOKBACKS[5] + 30) * MINUTE_MS;
    let split_ms = last_mark - args.test_days * 1440 * MINUTE_MS;
    let client = common::public_client()?;
    let symbols = if args.symbols.is_empty() {
        universe(&client, args.top).await?
    } else {
        common::split_csv(&args.symbols)
    };
    eprintln!(
        "entry-study: {} symbols, {} days of 1m candles: {}",
        symbols.len(),
        args.days,
        symbols.join(", ")
    );
    let fetcher = HistoryFetcher::spot(client.clone());
    let cache_dir = PathBuf::from(&args.cache_dir);
    let (fetcher, cache_dir) = (&fetcher, &cache_dir);
    let fetched: Vec<(String, Vec<Candle>)> = futures::stream::iter(symbols.iter().cloned())
        .map(|symbol| async move {
            let candles = candles(fetcher, &symbol, data_start, end_ms, cache_dir).await?;
            Ok::<_, StrategyError>((symbol, candles))
        })
        .buffer_unordered(3)
        .try_collect()
        .await?;
    let series: BTreeMap<String, Series> = fetched
        .iter()
        .map(|(symbol, candles)| (symbol.clone(), Series::new(data_start, end_ms, candles)))
        .collect();
    // Align the grid with the clock so marks fall on whole steps.
    let step_ms = args.step_minutes * MINUTE_MS;
    let first_mark = first_mark - first_mark.rem_euclid(step_ms) + step_ms;
    let samples = samples(&series, first_mark, last_mark, step_ms, args.fee_bps);
    let mut bot_entries = Vec::new();
    if !args.audit.is_empty() {
        let text =
            std::fs::read_to_string(&args.audit).map_err(|e| StrategyError::io(&args.audit, e))?;
        let entries: Vec<_> = audit_entries(&text)
            .into_iter()
            .filter(|(_, ms, _)| *ms <= last_mark)
            .collect();
        // Entries on coins outside the universe get their own candles.
        let mut extra = BTreeMap::new();
        for (symbol, _, _) in &entries {
            if !series.contains_key(symbol) && !extra.contains_key(symbol) {
                let candles = candles(fetcher, symbol, data_start, end_ms, cache_dir).await?;
                extra.insert(symbol.clone(), Series::new(data_start, end_ms, &candles));
            }
        }
        let all: BTreeMap<String, Series> = series.clone().into_iter().chain(extra).collect();
        bot_entries = measure_entries(&entries, &all);
    }
    let report = Report {
        generated_at: bb_core::time::now().to_rfc3339(),
        symbols,
        start: iso(first_mark),
        split: iso(split_ms),
        end: iso(last_mark),
        step_minutes: args.step_minutes,
        fee_bps: args.fee_bps,
        samples: samples.len(),
        buckets: bucket_stats(&samples, split_ms, args.step_minutes),
        bot_entries,
    };
    if !args.output.is_empty() {
        std::fs::write(&args.output, serde_json::to_vec_pretty(&report)?)
            .map_err(|e| StrategyError::io(&args.output, e))?;
    }
    let md = markdown(&report);
    if args.markdown.is_empty() {
        println!("{md}");
    } else {
        std::fs::write(&args.markdown, md).map_err(|e| StrategyError::io(&args.markdown, e))?;
    }
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

    /// 1500 minutes of candles at 100, then a series of opens/closes set per minute.
    fn series_with(start: i64, overrides: &[(i64, f64, f64)]) -> Series {
        let mut candles: Vec<Candle> = (0..1700)
            .map(|m| flat(start + m * MINUTE_MS, 100.0))
            .collect();
        for (m, o, c) in overrides {
            let candle = &mut candles[usize::try_from(*m).unwrap()];
            candle.o = *o;
            candle.c = *c;
            candle.h = o.max(*c);
            candle.l = o.min(*c);
        }
        Series::new(start, start + 1699 * MINUTE_MS, &candles)
    }

    #[test]
    fn features_look_back_from_the_entry_open_and_forward_returns_end_at_the_horizon_close() {
        let start = 0;
        let t_min = 1500;
        let t = start + t_min * MINUTE_MS;
        // 30 min before the mark the open was 99; the mark opens at 100;
        // the minute ending +20 closes at 101, the one ending +120 at 98.
        let s = series_with(
            start,
            &[
                (t_min - 30, 99.0, 99.0),
                (t_min + 19, 100.0, 101.0),
                (t_min + 119, 100.0, 98.0),
            ],
        );
        let series = BTreeMap::from([("X/USDT".to_owned(), s)]);
        let out = samples(&series, t, t, 20 * MINUTE_MS, 15.0);
        assert_eq!(out.len(), 1);
        let sample = &out[0];
        assert!(
            (sample.returns[2] - (100.0 / 99.0 - 1.0) * 100.0).abs() < 1e-12,
            "30m run-up"
        );
        assert!((sample.returns[0]).abs() < 1e-12, "5m flat");
        assert!((sample.forward[0] - 1.0).abs() < 1e-12, "+20m");
        assert!((sample.forward[3] - -2.0).abs() < 1e-12, "+120m");
        // Flat entry minute: 15 bps fee plus the 1 bp spread floor.
        assert!((sample.cost_pct - 0.16).abs() < 1e-12);
        assert_eq!(sample.breadth_1h, 0.0);

        // The same moves measured for a real fill at 100.5 inside the mark's minute.
        let entries = vec![("X/USDT".to_owned(), t + 25_000, 100.5)];
        let measured = measure_entries(&entries, &series);
        assert!((measured[0].before_30m_pct - (100.5 / 99.0 - 1.0) * 100.0).abs() < 1e-12);
        assert!((measured[0].forward[0] - (101.0 / 100.5 - 1.0) * 100.0).abs() < 1e-12);
    }

    #[test]
    fn a_missing_candle_drops_the_sample_instead_of_guessing() {
        let s = series_with(0, &[]);
        let mut gappy = s.clone();
        gappy.minutes[1500 + 89] = None;
        let t = 1500 * MINUTE_MS;
        let series = BTreeMap::from([("X/USDT".to_owned(), gappy)]);
        assert!(samples(&series, t, t, 20 * MINUTE_MS, 15.0).is_empty());
    }

    #[test]
    fn buckets_split_train_and_test_and_net_out_costs() {
        let mk = |t: i64, run_up: f64, fwd: f64| Sample {
            symbol: "X".into(),
            t,
            returns: [0.0, 0.0, run_up, 0.5, 0.5, 0.0],
            volume_ratio: None,
            breadth_1h: 0.5,
            cost_pct: 0.15,
            forward: [fwd; 4],
        };
        let rows = vec![
            mk(0, 1.5, 0.05),
            mk(1, 1.2, -0.25),
            mk(10, -1.5, 0.45),
            mk(11, 2.0, -0.05),
        ];
        let stats = bucket_stats(&rows, 10, 20);
        let get = |family: &str, bucket: &str, period: &str| {
            stats
                .iter()
                .find(|s| {
                    s.family == family
                        && s.bucket == bucket
                        && s.period == period
                        && s.horizon_min == 20
                })
                .unwrap()
        };
        let chase = get("run-up 30m", ">= +1%", "train");
        assert_eq!(chase.n, 2);
        assert!((chase.mean_net_pct - (-0.1 - 0.4) / 2.0).abs() < 1e-12);
        assert!((chase.mean_gross_pct - -0.1).abs() < 1e-12);
        assert_eq!(get("run-up 30m", "<= -1%", "test").n, 1);
        assert_eq!(get("all", "all", "test").n, 2);
        assert_eq!(get("run-up >= +1% and 1h up", "yes", "train").n, 2);
    }

    #[test]
    fn audit_entries_are_the_usdt_buys_with_their_fill() {
        let text = [
            r#"{"row_type":"route","status":"submitted","submitted_legs":[{"symbol":"AAVE/USDT","side":"buy","order":{"average":152.64,"timestamp":1792290996000}}]}"#,
            r#"{"row_type":"route","status":"submitted","submitted_legs":[{"symbol":"AAVE/USDT","side":"sell","order":{"average":153.59,"timestamp":1792296272918}}]}"#,
            r#"{"row_type":"cycle_summary","status":"held"}"#,
            r#"{"row_type":"route","status":"failed","submitted_legs":[{"symbol":"SOL/USDT","side":"buy","order":{"average":1,"timestamp":1}}]}"#,
        ]
        .join("\n");
        assert_eq!(
            audit_entries(&text),
            vec![("AAVE/USDT".to_owned(), 1_792_290_996_000, 152.64)]
        );
    }
}

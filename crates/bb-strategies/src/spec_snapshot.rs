//! Reproducible PUBLIC-data Binance speculation snapshot (former `scripts/spec_snapshot.py`).
//!
//! Fetches public market data only — no API keys, no signed endpoints, no orders — from
//! spot `ticker/24hr` + `klines` and USD-M `premiumIndex`, `ticker/24hr`, `fundingInfo`,
//! `openInterestHist`, `globalLongShortAccountRatio`, then writes `spot_tickers.csv`,
//! `funding.csv`, `open_interest.csv`, `momentum.csv` (CRLF, same columns as the Python
//! script) and a facts-only `SUMMARY.md` into the output directory
//! (default `data/speculation/<UTC date>`).
//!
//! Per-symbol HTTP errors are skipped and counted in `SUMMARY.md`; the exit status is
//! non-zero only when the spot 24h ticker fetch fails.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde_json::Value;

use crate::error::{Result, StrategyError};

pub const SPOT_BASE: &str = "https://api.binance.com";
pub const FAPI_BASE: &str = "https://fapi.binance.com";

const SPOT_MIN_QUOTE_VOLUME: f64 = 1_000_000.0;
const PERP_MIN_QUOTE_VOLUME: f64 = 5_000_000.0;
const SUMMARY_MIN_QUOTE_VOLUME: f64 = 5_000_000.0;
const TOP_OI: usize = 40;
const TOP_MOMENTUM: usize = 60;
const KLINE_LIMIT: u32 = 200;
const RSI_PERIOD: usize = 14;
const DEFAULT_FUNDING_INTERVAL_H: i64 = 8;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_RETRIES: u32 = 1;

const LEVERAGED_SUFFIXES: [&str; 4] = ["UP", "DOWN", "BULL", "BEAR"];
/// Sorted; rendered verbatim (Python list repr) in the summary notes.
const LEVERAGED_ALLOWLIST: [&str; 2] = ["JUP", "SYRUP"];
/// Sorted (binary-searched and rendered in this order).
const STABLE_FIAT_BASES: [&str; 57] = [
    "AEUR", "ALUSD", "ARS", "AUD", "BIDR", "BKRW", "BRL", "BUSD", "BVND", "CEUR", "CUSD", "DAI",
    "DOLA", "EUR", "EURI", "EURR", "FDUSD", "FRAX", "GBP", "GUSD", "GYEN", "HUSD", "IDR", "IDRT",
    "JPY", "JPYC", "LUSD", "MIM", "NGN", "PAX", "PYUSD", "RLUSD", "RUB", "SUSD", "TRY", "TUSD",
    "UAH", "USD1", "USDA", "USDC", "USDD", "USDE", "USDF", "USDG", "USDP", "USDR", "USDS", "USDT",
    "USDX", "USDY", "UST", "USTC", "VAI", "XSGD", "XUSD", "ZAR", "ZUSD",
];

const EP_SPOT: &str = "spot ticker/24hr";
const EP_KLINES: &str = "spot klines";
const EP_PREMIUM: &str = "fapi premiumIndex";
const EP_FAPI_TICKER: &str = "fapi ticker/24hr";
const EP_FUNDING_INFO: &str = "fapi fundingInfo";
const EP_OI_HIST: &str = "fapi openInterestHist";
const EP_LS_RATIO: &str = "fapi globalLongShortAccountRatio";

pub const SPOT_CSV_HEADER: [&str; 8] = [
    "symbol",
    "lastPrice",
    "priceChangePercent",
    "quoteVolume",
    "highPrice",
    "lowPrice",
    "range_pct",
    "count",
];
pub const FUNDING_CSV_HEADER: [&str; 9] = [
    "symbol",
    "markPrice",
    "indexPrice",
    "lastFundingRate",
    "funding_interval_h",
    "annualized_pct",
    "basis_bps",
    "nextFundingTime",
    "quoteVolume_24h",
];
pub const OI_CSV_HEADER: [&str; 6] = [
    "symbol",
    "oi_now_usd",
    "oi_change_24h_pct",
    "longShortRatio",
    "price_change_24h_pct",
    "quoteVolume_24h",
];
pub const MOMENTUM_CSV_HEADER: [&str; 11] = [
    "symbol",
    "lastPrice",
    "ret_1h",
    "ret_4h",
    "ret_24h",
    "ret_7d",
    "realized_vol_24h",
    "volume_ratio",
    "rsi_14",
    "dist_from_7d_high_pct",
    "quoteVolume_24h",
];

// --------------------------------------------------------------------------- //
// rows
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq)]
pub struct SpotRow {
    pub symbol: String,
    pub last_price: f64,
    pub price_change_percent: f64,
    pub quote_volume: f64,
    pub high_price: f64,
    pub low_price: f64,
    /// `(high - low) / low * 100`; `None` when `low <= 0`.
    pub range_pct: Option<f64>,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FundingRow {
    pub symbol: String,
    pub mark_price: f64,
    pub index_price: f64,
    pub last_funding_rate: f64,
    pub funding_interval_h: i64,
    pub annualized_pct: f64,
    /// `(mark - index) / index * 1e4`; `None` when the index price is zero.
    pub basis_bps: Option<f64>,
    /// `YYYY-MM-DDTHH:MM:SSZ`, empty when Binance omitted the timestamp.
    pub next_funding_time: String,
    pub quote_volume_24h: f64,
    pub price_change_percent: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OiRow {
    pub symbol: String,
    pub oi_now_usd: f64,
    pub oi_change_24h_pct: Option<f64>,
    pub long_short_ratio: Option<f64>,
    pub price_change_24h_pct: f64,
    pub quote_volume_24h: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MomentumRow {
    pub symbol: String,
    pub last_price: f64,
    pub ret_1h: Option<f64>,
    pub ret_4h: Option<f64>,
    pub ret_24h: Option<f64>,
    pub ret_7d: Option<f64>,
    pub realized_vol_24h: Option<f64>,
    pub volume_ratio: Option<f64>,
    pub rsi_14: Option<f64>,
    pub dist_from_7d_high_pct: Option<f64>,
    pub quote_volume_24h: f64,
}

/// Last observed outcome of an endpoint call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpStatus {
    Code(u16),
    NetworkError,
}

impl HttpStatus {
    /// Wording used in the "Endpoint failures" section.
    fn failure_text(self) -> String {
        match self {
            Self::Code(code) => format!("HTTP {code}"),
            Self::NetworkError => "network-error".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FundingMeta {
    pub available: bool,
    pub status: Option<HttpStatus>,
    /// Symbols with an explicit `fundingIntervalHours` in `/fapi/v1/fundingInfo`.
    pub configured: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointFailure {
    pub endpoint: &'static str,
    pub count: u32,
    pub last_status: Option<HttpStatus>,
}

/// Everything `SUMMARY.md` and the CSVs are rendered from.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// `YYYY-MM-DDTHH:MM:SSZ`, stamped before the first request.
    pub generated_at: String,
    pub spot_failed: bool,
    pub spot_rows: Vec<SpotRow>,
    pub funding_rows: Vec<FundingRow>,
    pub funding_meta: FundingMeta,
    pub funding_note: Option<String>,
    pub oi_rows: Vec<OiRow>,
    pub oi_skipped: usize,
    pub momentum_rows: Vec<MomentumRow>,
    pub momentum_skipped: usize,
    /// Sorted by endpoint label.
    pub endpoint_failures: Vec<EndpointFailure>,
}

// --------------------------------------------------------------------------- //
// Python-compatible value parsing and formatting
// --------------------------------------------------------------------------- //

/// Python `float(value)`: JSON numbers, numeric strings (incl. `nan`/`inf`) and bools.
fn py_float(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        _ => None,
    }
}

/// Python `float(value or 0.0)`: falsy values (missing, null, `""`, empty containers) read as 0.
fn py_float_or_zero(value: Option<&Value>) -> Option<f64> {
    if is_falsy(value) {
        Some(0.0)
    } else {
        py_float(value)
    }
}

/// Python `int(value)`: integers, integral strings, floats truncated toward zero.
fn py_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|float| float.is_finite())
                .map(|float| float.trunc() as i64)
        }),
        Value::String(text) => text.trim().parse().ok(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

fn is_falsy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        Some(Value::Array(items)) => items.is_empty(),
        Some(Value::Object(map)) => map.is_empty(),
        Some(Value::Bool(flag)) => !flag,
        Some(Value::Number(number)) => number.as_f64() == Some(0.0),
    }
}

/// Python `sum()` over floats (CPython >= 3.12 uses Neumaier compensated summation).
fn py_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0_f64;
    let mut compensation = 0.0_f64;
    for value in values {
        let next = total + value;
        if total.abs() >= value.abs() {
            compensation += (total - next) + value;
        } else {
            compensation += (value - next) + total;
        }
        total = next;
    }
    if compensation != 0.0 && compensation.is_finite() {
        total += compensation;
    }
    total
}

/// Python `f"{value:.{digits}f}"`, including its `nan`/`inf` spelling.
fn py_fixed(value: f64, digits: usize) -> String {
    if value.is_nan() {
        "nan".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        format!("{value:.digits$}")
    }
}

/// Fixed digits with trailing zeros (and a dangling dot) stripped.
pub fn fmt(value: f64, digits: usize) -> String {
    let text = py_fixed(value, digits);
    if !text.contains('.') {
        return text;
    }
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn fmt_opt(value: Option<f64>, digits: usize) -> String {
    value.map(|value| fmt(value, digits)).unwrap_or_default()
}

fn pct(value: Option<f64>, digits: usize) -> String {
    value
        .map(|value| format!("{}%", fmt(value, digits)))
        .unwrap_or_default()
}

/// Compact USD magnitude, e.g. `101.9M`, `2.4B`, `6M`.
pub fn human_usd(value: f64) -> String {
    for (scale, suffix) in [(1e9, "B"), (1e6, "M"), (1e3, "K")] {
        if value.abs() >= scale {
            let text = py_fixed(value / scale, 1);
            let text = text.strip_suffix(".0").unwrap_or(&text);
            return format!("{text}{suffix}");
        }
    }
    py_fixed(value, 0)
}

/// Epoch milliseconds → `YYYY-MM-DDTHH:MM:SSZ`; empty for missing/unparsable input.
fn iso_utc(ms: Option<&Value>) -> String {
    py_float(ms)
        .filter(|ms| ms.is_finite())
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms.floor() as i64))
        .map(|ts| ts.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

pub fn md_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format!("| {} |", headers.join(" | ")));
    lines.push(format!("|{}|", vec!["---"; headers.len()].join("|")));
    lines.extend(rows.iter().map(|row| format!("| {} |", row.join(" | "))));
    lines.join("\n")
}

/// CSV bytes with CRLF line endings and minimal quoting (Python `csv.writer` defaults).
pub fn render_csv(header: &[&str], rows: &[Vec<String>]) -> std::io::Result<Vec<u8>> {
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(Vec::new());
    writer.write_record(header)?;
    for row in rows {
        writer.write_record(row)?;
    }
    writer.into_inner().map_err(|err| err.into_error())
}

// --------------------------------------------------------------------------- //
// filters and row builders (pure)
// --------------------------------------------------------------------------- //

pub fn is_leveraged_token(base: &str) -> bool {
    let upper = base.to_uppercase();
    LEVERAGED_SUFFIXES
        .iter()
        .any(|suffix| upper.ends_with(suffix))
        && !LEVERAGED_ALLOWLIST.contains(&upper.as_str())
}

pub fn is_stable_or_fiat(base: &str) -> bool {
    STABLE_FIAT_BASES
        .binary_search(&base.to_uppercase().as_str())
        .is_ok()
}

/// Liquid, non-stable, non-leveraged USDT spot pairs sorted by quote volume (desc).
pub fn filter_spot_tickers(raw: &[Value]) -> Vec<SpotRow> {
    let mut rows: Vec<SpotRow> = raw.iter().filter_map(spot_row).collect();
    rows.sort_by(|a, b| b.quote_volume.total_cmp(&a.quote_volume));
    rows
}

fn spot_row(ticker: &Value) -> Option<SpotRow> {
    let symbol = ticker.get("symbol")?.as_str()?;
    let base = symbol.strip_suffix("USDT")?;
    let quote_volume = py_float_or_zero(ticker.get("quoteVolume"))?;
    if quote_volume < SPOT_MIN_QUOTE_VOLUME || is_stable_or_fiat(base) || is_leveraged_token(base) {
        return None;
    }
    let high_price = py_float_or_zero(ticker.get("highPrice"))?;
    let low_price = py_float_or_zero(ticker.get("lowPrice"))?;
    Some(SpotRow {
        symbol: symbol.to_owned(),
        last_price: py_float_or_zero(ticker.get("lastPrice"))?,
        price_change_percent: py_float_or_zero(ticker.get("priceChangePercent"))?,
        quote_volume,
        high_price,
        low_price,
        range_pct: (low_price > 0.0).then(|| (high_price - low_price) / low_price * 100.0),
        count: if is_falsy(ticker.get("count")) {
            0
        } else {
            py_int(ticker.get("count"))?
        },
    })
}

/// `symbol -> fundingIntervalHours` from `/fapi/v1/fundingInfo`.
pub fn parse_funding_intervals(info: &[Value]) -> HashMap<String, i64> {
    info.iter()
        .filter_map(|entry| {
            let symbol = entry.get("symbol")?.as_str()?;
            let hours = py_int(entry.get("fundingIntervalHours"))?;
            (hours > 0).then(|| (symbol.to_owned(), hours))
        })
        .collect()
}

/// Perp funding rows (delivery contracts and perps below 5M quote volume dropped),
/// sorted by 24h quote volume (desc).
pub fn build_funding_rows(
    premium: &[Value],
    tickers: &[Value],
    intervals: &HashMap<String, i64>,
) -> Vec<FundingRow> {
    let ticker_by_symbol: HashMap<&str, &Value> = tickers
        .iter()
        .filter_map(|ticker| {
            let symbol = ticker.get("symbol")?.as_str()?;
            (!symbol.is_empty()).then_some((symbol, ticker))
        })
        .collect();
    let mut rows: Vec<FundingRow> = premium
        .iter()
        .filter_map(|entry| {
            let symbol = entry.get("symbol")?.as_str()?;
            // Perps only; delivery contracts look like "BTCUSDT_250926".
            if symbol.is_empty() || symbol.contains('_') {
                return None;
            }
            let ticker = ticker_by_symbol.get(symbol)?;
            funding_row(symbol, entry, ticker, intervals)
        })
        .collect();
    rows.sort_by(|a, b| b.quote_volume_24h.total_cmp(&a.quote_volume_24h));
    rows
}

fn funding_row(
    symbol: &str,
    entry: &Value,
    ticker: &Value,
    intervals: &HashMap<String, i64>,
) -> Option<FundingRow> {
    let quote_volume_24h = py_float_or_zero(ticker.get("quoteVolume"))?;
    let mark_price = py_float(entry.get("markPrice"))?;
    let index_price = py_float(entry.get("indexPrice"))?;
    let last_funding_rate = py_float(entry.get("lastFundingRate"))?;
    if quote_volume_24h < PERP_MIN_QUOTE_VOLUME {
        return None;
    }
    let funding_interval_h = intervals
        .get(symbol)
        .copied()
        .unwrap_or(DEFAULT_FUNDING_INTERVAL_H);
    Some(FundingRow {
        symbol: symbol.to_owned(),
        mark_price,
        index_price,
        last_funding_rate,
        funding_interval_h,
        annualized_pct: last_funding_rate * (24.0 / funding_interval_h as f64) * 365.0 * 100.0,
        basis_bps: (index_price != 0.0).then(|| (mark_price - index_price) / index_price * 1e4),
        next_funding_time: iso_utc(entry.get("nextFundingTime")),
        quote_volume_24h,
        price_change_percent: py_float_or_zero(ticker.get("priceChangePercent"))?,
    })
}

/// Open-interest row from `openInterestHist` (needs >= 2 samples) and the latest
/// account long/short ratio (optional).
pub fn compute_oi_row(
    perp: &FundingRow,
    history: Option<&Value>,
    ratio: Option<&Value>,
) -> Option<OiRow> {
    let history = history?.as_array().filter(|items| items.len() >= 2)?;
    let first = py_float(history.first()?.get("sumOpenInterestValue"))?;
    let last = py_float(history.last()?.get("sumOpenInterestValue"))?;
    let long_short_ratio = ratio
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .and_then(|latest| py_float(latest.get("longShortRatio")));
    Some(OiRow {
        symbol: perp.symbol.clone(),
        oi_now_usd: last,
        oi_change_24h_pct: (first != 0.0).then(|| (last / first - 1.0) * 100.0),
        long_short_ratio,
        price_change_24h_pct: perp.price_change_percent,
        quote_volume_24h: perp.quote_volume_24h,
    })
}

/// Wilder-smoothed RSI over `period` deltas; `None` without `period + 1` closes.
pub fn wilder_rsi(closes: &[f64], period: usize) -> Option<f64> {
    if period == 0 || closes.len() < period + 1 {
        return None;
    }
    let deltas: Vec<f64> = closes.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let gains: Vec<f64> = deltas.iter().map(|delta| delta.max(0.0)).collect();
    let losses: Vec<f64> = deltas.iter().map(|delta| (-delta).max(0.0)).collect();
    let period_f = period as f64;
    let mut avg_gain = py_sum(gains[..period].iter().copied()) / period_f;
    let mut avg_loss = py_sum(losses[..period].iter().copied()) / period_f;
    for (gain, loss) in gains[period..].iter().zip(&losses[period..]) {
        avg_gain = (avg_gain * (period_f - 1.0) + gain) / period_f;
        avg_loss = (avg_loss * (period_f - 1.0) + loss) / period_f;
    }
    if avg_loss == 0.0 {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

/// Sample standard deviation (Python `statistics.stdev`); needs >= 2 values.
fn sample_stdev(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let n = values.len() as f64;
    let mean = py_sum(values.iter().copied()) / n;
    let squares = py_sum(values.iter().map(|value| (value - mean) * (value - mean)));
    Some((squares / (n - 1.0)).sqrt())
}

/// Momentum metrics from 1h klines (`[openTime, open, high, low, close, volume, closeTime, quoteVolume, ...]`).
pub fn compute_momentum(spot: &SpotRow, klines: &Value) -> Option<MomentumRow> {
    let klines = klines.as_array().filter(|items| items.len() >= 2)?;
    let mut closes = Vec::with_capacity(klines.len());
    let mut highs = Vec::with_capacity(klines.len());
    let mut quote_volumes = Vec::with_capacity(klines.len());
    for kline in klines {
        let fields = kline.as_array()?;
        closes.push(py_float(fields.get(4))?);
        highs.push(py_float(fields.get(2))?);
        quote_volumes.push(py_float(fields.get(7))?);
    }
    let len = closes.len();
    let last = closes[len - 1];
    let ret = |bars_back: usize| -> Option<f64> {
        let base = *closes.get(len.checked_sub(bars_back + 1)?)?;
        (base != 0.0).then(|| (last / base - 1.0) * 100.0)
    };

    let realized_vol_24h = if len >= 25 {
        let log_returns: Vec<f64> = (len - 24..len)
            .filter(|&i| closes[i - 1] > 0.0 && closes[i] > 0.0)
            .map(|i| (closes[i] / closes[i - 1]).ln())
            .collect();
        sample_stdev(&log_returns).map(|stdev| stdev * 24.0_f64.sqrt() * 100.0)
    } else {
        None
    };

    let volume_ratio = if len >= 168 {
        let recent = py_sum(quote_volumes[len - 24..].iter().copied());
        let prior_daily_mean = py_sum(quote_volumes[len - 168..len - 24].iter().copied()) / 6.0;
        (prior_daily_mean > 0.0).then(|| recent / prior_daily_mean)
    } else {
        None
    };

    let week_high = highs[len.saturating_sub(168)..]
        .iter()
        .copied()
        .reduce(|best, high| if high > best { high } else { best });
    let dist_from_7d_high_pct = week_high
        .filter(|high| *high != 0.0)
        .map(|high| (last / high - 1.0) * 100.0);

    Some(MomentumRow {
        symbol: spot.symbol.clone(),
        last_price: last,
        ret_1h: ret(1),
        ret_4h: ret(4),
        ret_24h: ret(24),
        ret_7d: ret(168),
        realized_vol_24h,
        volume_ratio,
        rsi_14: wilder_rsi(&closes, RSI_PERIOD),
        dist_from_7d_high_pct,
        quote_volume_24h: spot.quote_volume,
    })
}

// --------------------------------------------------------------------------- //
// HTTP
// --------------------------------------------------------------------------- //

/// Endpoint bases, request pacing and retry backoff (tests zero the delays).
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    pub spot_base: String,
    pub fapi_base: String,
    /// Concurrent per-symbol requests (open interest, klines).
    pub concurrency: usize,
    /// Base delay before retrying a 5xx (multiplied by the attempt number).
    pub retry_backoff: Duration,
    /// Delay before and after each symbol's open-interest/ratio pair.
    pub oi_pacing: Duration,
    /// Delay after each klines request.
    pub kline_pacing: Duration,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            spot_base: SPOT_BASE.to_owned(),
            fapi_base: FAPI_BASE.to_owned(),
            concurrency: 5,
            retry_backoff: Duration::from_secs(1),
            oi_pacing: Duration::from_millis(100),
            kline_pacing: Duration::from_millis(50),
        }
    }
}

#[derive(Debug, Default)]
struct HttpStats {
    failures: BTreeMap<&'static str, u32>,
    statuses: HashMap<&'static str, HttpStatus>,
}

/// JSON GET client recording per-endpoint last status and failure counts.
struct SnapshotHttp {
    http: reqwest::Client,
    retry_backoff: Duration,
    stats: Mutex<HttpStats>,
}

enum Fetched {
    Json(Value),
    Status(u16),
}

impl SnapshotHttp {
    fn new(retry_backoff: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|err| StrategyError::Http(err.to_string()))?;
        Ok(Self {
            http,
            retry_backoff,
            stats: Mutex::new(HttpStats::default()),
        })
    }

    fn stats(&self) -> MutexGuard<'_, HttpStats> {
        self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self, endpoint: &str) -> Option<HttpStatus> {
        self.stats().statuses.get(endpoint).copied()
    }

    /// Body on HTTP 200; `None` (recorded as a failure) otherwise. 5xx is retried once.
    async fn get_json(
        &self,
        url: &str,
        endpoint: &'static str,
        query: &[(&str, String)],
    ) -> Option<Value> {
        for attempt in 0..=HTTP_RETRIES {
            let status = match self.fetch(url, query).await {
                Ok(Fetched::Json(body)) => {
                    self.stats()
                        .statuses
                        .insert(endpoint, HttpStatus::Code(200));
                    return Some(body);
                }
                Ok(Fetched::Status(code)) => HttpStatus::Code(code),
                Err(err) => {
                    tracing::debug!(endpoint, url, error = %err, "spec snapshot request failed");
                    HttpStatus::NetworkError
                }
            };
            self.stats().statuses.insert(endpoint, status);
            if matches!(status, HttpStatus::Code(code) if code >= 500) && attempt < HTTP_RETRIES {
                tokio::time::sleep(self.retry_backoff * (attempt + 1)).await;
                continue;
            }
            break;
        }
        *self.stats().failures.entry(endpoint).or_default() += 1;
        None
    }

    async fn fetch(&self, url: &str, query: &[(&str, String)]) -> Result<Fetched, String> {
        let response = self
            .http
            .get(url)
            .query(query)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        let status = response.status().as_u16();
        if status != 200 {
            return Ok(Fetched::Status(status));
        }
        let body = response.bytes().await.map_err(|err| err.to_string())?;
        if body.trim_ascii().is_empty() {
            return Ok(Fetched::Json(Value::Null));
        }
        serde_json::from_slice(&body)
            .map(Fetched::Json)
            .map_err(|err| err.to_string())
    }

    fn failures(&self) -> Vec<EndpointFailure> {
        let stats = self.stats();
        stats
            .failures
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(&endpoint, &count)| EndpointFailure {
                endpoint,
                count,
                last_status: stats.statuses.get(endpoint).copied(),
            })
            .collect()
    }
}

// --------------------------------------------------------------------------- //
// fetchers
// --------------------------------------------------------------------------- //

async fn fetch_spot_rows(http: &SnapshotHttp, config: &SnapshotConfig) -> Option<Vec<SpotRow>> {
    let url = format!("{}/api/v3/ticker/24hr", config.spot_base);
    let raw = http.get_json(&url, EP_SPOT, &[]).await?;
    Some(filter_spot_tickers(raw.as_array()?))
}

async fn fetch_funding_rows(
    http: &SnapshotHttp,
    config: &SnapshotConfig,
) -> (Vec<FundingRow>, FundingMeta) {
    let mut meta = FundingMeta::default();
    let premium_url = format!("{}/fapi/v1/premiumIndex", config.fapi_base);
    let premium = http.get_json(&premium_url, EP_PREMIUM, &[]).await;
    meta.status = http.status(EP_PREMIUM);
    // Blocked (HTTP 418/451/403) or failed host: do not hammer the endpoint.
    let Some(Value::Array(premium)) = premium else {
        return (Vec::new(), meta);
    };
    let ticker_url = format!("{}/fapi/v1/ticker/24hr", config.fapi_base);
    let Some(Value::Array(tickers)) = http.get_json(&ticker_url, EP_FAPI_TICKER, &[]).await else {
        meta.status = http.status(EP_FAPI_TICKER);
        return (Vec::new(), meta);
    };
    let info_url = format!("{}/fapi/v1/fundingInfo", config.fapi_base);
    let intervals = match http.get_json(&info_url, EP_FUNDING_INFO, &[]).await {
        Some(Value::Array(info)) => parse_funding_intervals(&info),
        _ => HashMap::new(),
    };
    meta.configured = intervals.len();
    meta.available = true;
    (build_funding_rows(&premium, &tickers, &intervals), meta)
}

async fn fetch_oi_rows(
    http: &SnapshotHttp,
    config: &SnapshotConfig,
    perps: &[FundingRow],
) -> (Vec<OiRow>, usize) {
    let history_url = format!("{}/futures/data/openInterestHist", config.fapi_base);
    let ratio_url = format!(
        "{}/futures/data/globalLongShortAccountRatio",
        config.fapi_base
    );
    let rows: Vec<OiRow> = stream::iter(perps)
        .map(|perp| {
            let (history_url, ratio_url) = (&history_url, &ratio_url);
            async move {
                tokio::time::sleep(config.oi_pacing).await;
                let symbol = perp.symbol.clone();
                let history = http
                    .get_json(
                        history_url,
                        EP_OI_HIST,
                        &[
                            ("symbol", symbol.clone()),
                            ("period", "1h".to_owned()),
                            ("limit", "25".to_owned()),
                        ],
                    )
                    .await;
                let ratio = http
                    .get_json(
                        ratio_url,
                        EP_LS_RATIO,
                        &[
                            ("symbol", symbol),
                            ("period", "1h".to_owned()),
                            ("limit", "1".to_owned()),
                        ],
                    )
                    .await;
                tokio::time::sleep(config.oi_pacing).await;
                compute_oi_row(perp, history.as_ref(), ratio.as_ref())
            }
        })
        .buffered(config.concurrency.max(1))
        .filter_map(|row| async move { row })
        .collect()
        .await;
    let skipped = perps.len() - rows.len();
    (rows, skipped)
}

async fn fetch_momentum_rows(
    http: &SnapshotHttp,
    config: &SnapshotConfig,
    spot_rows: &[SpotRow],
) -> (Vec<MomentumRow>, usize) {
    let candidates = &spot_rows[..spot_rows.len().min(TOP_MOMENTUM)];
    let url = format!("{}/api/v3/klines", config.spot_base);
    let rows: Vec<MomentumRow> = stream::iter(candidates)
        .map(|spot| {
            let url = &url;
            async move {
                let klines = http
                    .get_json(
                        url,
                        EP_KLINES,
                        &[
                            ("symbol", spot.symbol.clone()),
                            ("interval", "1h".to_owned()),
                            ("limit", KLINE_LIMIT.to_string()),
                        ],
                    )
                    .await;
                tokio::time::sleep(config.kline_pacing).await;
                compute_momentum(spot, &klines?)
            }
        })
        .buffered(config.concurrency.max(1))
        .filter_map(|row| async move { row })
        .collect()
        .await;
    let skipped = candidates.len() - rows.len();
    (rows, skipped)
}

/// Fetch every dataset. Never fails on HTTP errors: they are recorded in the snapshot.
pub async fn collect(config: &SnapshotConfig) -> Result<Snapshot> {
    let generated_at = bb_core::time::now()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let http = SnapshotHttp::new(config.retry_backoff)?;

    let spot = fetch_spot_rows(&http, config).await;
    let spot_failed = spot.is_none();
    let spot_rows = spot.unwrap_or_default();
    if spot_failed {
        let status = match http.status(EP_SPOT) {
            Some(HttpStatus::Code(code)) => code.to_string(),
            Some(HttpStatus::NetworkError) => "network-error".to_owned(),
            None => "None".to_owned(),
        };
        eprintln!("ERROR: spot 24h ticker fetch failed ({status}); writing partial snapshot");
    }

    let (funding_rows, funding_meta) = fetch_funding_rows(&http, config).await;
    let mut funding_note = None;
    let (mut oi_rows, mut oi_skipped) = (Vec::new(), 0);
    if funding_meta.available {
        let non_8h = funding_rows
            .iter()
            .filter(|row| row.funding_interval_h != 8)
            .count();
        funding_note = Some(format!(
            "Funding intervals sourced from `/fapi/v1/fundingInfo`: {} symbols explicitly configured; \
             {non_8h}/{} of the kept perps use a non-8h interval.",
            funding_meta.configured,
            funding_rows.len(),
        ));
        let perps = &funding_rows[..funding_rows.len().min(TOP_OI)];
        (oi_rows, oi_skipped) = fetch_oi_rows(&http, config, perps).await;
    }

    let (momentum_rows, momentum_skipped) = fetch_momentum_rows(&http, config, &spot_rows).await;

    Ok(Snapshot {
        generated_at,
        spot_failed,
        spot_rows,
        funding_rows,
        funding_meta,
        funding_note,
        oi_rows,
        oi_skipped,
        momentum_rows,
        momentum_skipped,
        endpoint_failures: http.failures(),
    })
}

// --------------------------------------------------------------------------- //
// CSV rows
// --------------------------------------------------------------------------- //

pub fn spot_csv_rows(rows: &[SpotRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            vec![
                row.symbol.clone(),
                fmt(row.last_price, 10),
                fmt(row.price_change_percent, 4),
                fmt(row.quote_volume, 2),
                fmt(row.high_price, 10),
                fmt(row.low_price, 10),
                fmt_opt(row.range_pct, 4),
                row.count.to_string(),
            ]
        })
        .collect()
}

pub fn funding_csv_rows(rows: &[FundingRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            vec![
                row.symbol.clone(),
                fmt(row.mark_price, 10),
                fmt(row.index_price, 10),
                fmt(row.last_funding_rate, 10),
                row.funding_interval_h.to_string(),
                fmt(row.annualized_pct, 4),
                fmt_opt(row.basis_bps, 4),
                row.next_funding_time.clone(),
                fmt(row.quote_volume_24h, 2),
            ]
        })
        .collect()
}

pub fn oi_csv_rows(rows: &[OiRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            vec![
                row.symbol.clone(),
                fmt(row.oi_now_usd, 2),
                fmt_opt(row.oi_change_24h_pct, 4),
                fmt_opt(row.long_short_ratio, 4),
                fmt(row.price_change_24h_pct, 4),
                fmt(row.quote_volume_24h, 2),
            ]
        })
        .collect()
}

pub fn momentum_csv_rows(rows: &[MomentumRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            vec![
                row.symbol.clone(),
                fmt(row.last_price, 10),
                fmt_opt(row.ret_1h, 4),
                fmt_opt(row.ret_4h, 4),
                fmt_opt(row.ret_24h, 4),
                fmt_opt(row.ret_7d, 4),
                fmt_opt(row.realized_vol_24h, 4),
                fmt_opt(row.volume_ratio, 4),
                fmt_opt(row.rsi_14, 4),
                fmt_opt(row.dist_from_7d_high_pct, 4),
                fmt(row.quote_volume_24h, 2),
            ]
        })
        .collect()
}

// --------------------------------------------------------------------------- //
// SUMMARY.md
// --------------------------------------------------------------------------- //

/// Stable sort of `rows` by an `f64` key, descending or ascending, truncated to `limit`.
fn top_by<'a, T>(
    rows: impl IntoIterator<Item = &'a T>,
    key: impl Fn(&T) -> f64,
    descending: bool,
    limit: usize,
) -> Vec<&'a T> {
    let mut sorted: Vec<&T> = rows.into_iter().collect();
    sorted.sort_by(|a, b| {
        let order = key(a).total_cmp(&key(b));
        if descending { order.reverse() } else { order }
    });
    sorted.truncate(limit);
    sorted
}

fn spot_table(rows: &[&SpotRow]) -> String {
    md_table(
        &["symbol", "lastPrice", "chg_24h", "quoteVolume", "range_24h"],
        &rows
            .iter()
            .map(|row| {
                vec![
                    format!("`{}`", row.symbol),
                    fmt(row.last_price, 10),
                    pct(Some(row.price_change_percent), 2),
                    human_usd(row.quote_volume),
                    pct(row.range_pct, 4),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

fn funding_table(rows: &[&FundingRow]) -> String {
    md_table(
        &[
            "symbol",
            "annualized",
            "rate_per_interval",
            "interval",
            "basis_bps",
            "markPrice",
            "volume",
        ],
        &rows
            .iter()
            .map(|row| {
                vec![
                    format!("`{}`", row.symbol),
                    pct(Some(row.annualized_pct), 2),
                    pct(Some(row.last_funding_rate), 4),
                    format!("{}h", row.funding_interval_h),
                    fmt_opt(row.basis_bps, 4),
                    fmt(row.mark_price, 10),
                    human_usd(row.quote_volume_24h),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

fn oi_table(rows: &[&OiRow]) -> String {
    md_table(
        &[
            "symbol",
            "oi_chg_24h",
            "oi_now_usd",
            "price_chg_24h",
            "long/short acct",
        ],
        &rows
            .iter()
            .map(|row| {
                vec![
                    format!("`{}`", row.symbol),
                    pct(row.oi_change_24h_pct, 2),
                    human_usd(row.oi_now_usd),
                    pct(Some(row.price_change_24h_pct), 2),
                    fmt_opt(row.long_short_ratio, 4),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

fn rsi_table(rows: &[&MomentumRow]) -> String {
    md_table(
        &["symbol", "rsi", "chg_24h", "chg_7d", "dist_from_7d_high"],
        &rows
            .iter()
            .map(|row| {
                vec![
                    format!("`{}`", row.symbol),
                    fmt_opt(row.rsi_14, 4),
                    pct(row.ret_24h, 2),
                    pct(row.ret_7d, 2),
                    pct(row.dist_from_7d_high_pct, 2),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

/// Facts-only markdown summary (byte-compatible with the Python script).
pub fn render_summary(snapshot: &Snapshot, out_dir: &Path) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: &str| lines.push(line.to_owned());
    push("# Speculation snapshot — public Binance REST data");
    push("");
    push(&format!(
        "- Fetch timestamp (UTC): {}",
        snapshot.generated_at
    ));
    push(
        "- Spot endpoint: `GET https://api.binance.com/api/v3/ticker/24hr`; klines: `GET https://api.binance.com/api/v3/klines`",
    );
    push(
        "- Futures endpoints: `GET https://fapi.binance.com/fapi/v1/premiumIndex`, \
         `GET https://fapi.binance.com/fapi/v1/ticker/24hr`, \
         `GET https://fapi.binance.com/fapi/v1/fundingInfo`, \
         `GET https://fapi.binance.com/futures/data/openInterestHist`, \
         `GET https://fapi.binance.com/futures/data/globalLongShortAccountRatio`",
    );
    push(
        "- Filter: spot USDT pairs quoteVolume >= 1,000,000 USDT, leveraged tokens \
         (UP/DOWN/BULL/BEAR suffixes) and stablecoin/fiat bases excluded; perps quoteVolume >= 5,000,000 USDT.",
    );
    if snapshot.spot_failed {
        push("- Spot 24h ticker fetch FAILED; snapshot is incomplete (see Endpoint failures).");
    }
    let meta = snapshot.funding_meta;
    if meta.available {
        push("- Futures API status: OK (HTTP 200)");
    } else {
        let status_text = match meta.status {
            Some(HttpStatus::Code(code)) => format!("HTTP {code}"),
            _ => "network error".to_owned(),
        };
        push(&format!(
            "- Futures API status: unavailable ({status_text}); snapshot is spot-only where futures data is missing."
        ));
    }
    push(&format!("- Output directory: `{}`", out_dir.display()));
    push("");

    push("## Row counts");
    push("");
    push(&md_table(
        &["dataset", "rows"],
        &[
            vec![
                "spot_tickers.csv".to_owned(),
                snapshot.spot_rows.len().to_string(),
            ],
            vec![
                "funding.csv".to_owned(),
                snapshot.funding_rows.len().to_string(),
            ],
            vec![
                "open_interest.csv".to_owned(),
                snapshot.oi_rows.len().to_string(),
            ],
            vec![
                "momentum.csv".to_owned(),
                snapshot.momentum_rows.len().to_string(),
            ],
        ],
    ));
    push("");

    push("## Endpoint failures");
    push("");
    let mut failure_lines: Vec<String> = snapshot
        .endpoint_failures
        .iter()
        .filter(|failure| failure.count > 0)
        .map(|failure| {
            let status = failure
                .last_status
                .map_or_else(|| "None".to_owned(), HttpStatus::failure_text);
            format!(
                "- `{}`: {} failed call(s), last status {status}",
                failure.endpoint, failure.count
            )
        })
        .collect();
    if let (false, Some(HttpStatus::Code(code @ (403 | 418 | 451)))) = (meta.available, meta.status)
    {
        failure_lines.push(format!(
            "- Futures API access blocked (HTTP {code}); funding and open-interest sections skipped, snapshot is spot-only."
        ));
    }
    if snapshot.momentum_skipped > 0 {
        failure_lines.push(format!(
            "- momentum: {}/{} per-symbol failures (klines unavailable)",
            snapshot.momentum_skipped,
            snapshot.spot_rows.len().min(TOP_MOMENTUM)
        ));
    }
    if snapshot.oi_skipped > 0 {
        failure_lines.push(format!(
            "- open_interest: {}/{} per-symbol failures (history unavailable)",
            snapshot.oi_skipped,
            snapshot.funding_rows.len().min(TOP_OI)
        ));
    }
    if failure_lines.is_empty() {
        push("None.");
    } else {
        failure_lines.iter().for_each(|line| push(line));
    }
    push("");

    if !snapshot.spot_rows.is_empty() {
        let liquid = || {
            snapshot
                .spot_rows
                .iter()
                .filter(|row| row.quote_volume >= SUMMARY_MIN_QUOTE_VOLUME)
        };
        let gainers = top_by(liquid(), |row| row.price_change_percent, true, 15);
        let losers = top_by(liquid(), |row| row.price_change_percent, false, 15);
        push("## Top 15 spot gainers 24h (quoteVolume >= 5M USDT)");
        push("");
        push(&spot_table(&gainers));
        push("");
        push("## Top 15 spot losers 24h (quoteVolume >= 5M USDT)");
        push("");
        push(&spot_table(&losers));
        push("");
    }

    if !snapshot.momentum_rows.is_empty() {
        let by_ratio: Vec<(&MomentumRow, f64)> = top_by(
            snapshot
                .momentum_rows
                .iter()
                .filter(|row| row.volume_ratio.is_some()),
            |row| row.volume_ratio.unwrap_or_default(),
            true,
            15,
        )
        .into_iter()
        .filter_map(|row| Some((row, row.volume_ratio?)))
        .collect();
        push("## Top 15 by volume_ratio (last 24h vs previous 6d daily avg)");
        push("");
        push(&md_table(
            &[
                "symbol",
                "volume_ratio",
                "chg_24h",
                "realized_vol_24h",
                "quoteVolume",
            ],
            &by_ratio
                .iter()
                .map(|(row, ratio)| {
                    vec![
                        format!("`{}`", row.symbol),
                        format!("{}x", fmt(*ratio, 4)),
                        pct(row.ret_24h, 2),
                        fmt_opt(row.realized_vol_24h, 4),
                        human_usd(row.quote_volume_24h),
                    ]
                })
                .collect::<Vec<_>>(),
        ));
        push("");
    }

    if !snapshot.funding_rows.is_empty() {
        let most_positive = top_by(&snapshot.funding_rows, |row| row.annualized_pct, true, 10);
        let most_negative = top_by(&snapshot.funding_rows, |row| row.annualized_pct, false, 10);
        push("## Top 10 most positive funding (annualized)");
        push("");
        push(&funding_table(&most_positive));
        push("");
        push("## Top 10 most negative funding (annualized)");
        push("");
        push(&funding_table(&most_negative));
        push("");
        if let Some(note) = &snapshot.funding_note {
            push(note);
            push("");
        }
    }

    if !snapshot.oi_rows.is_empty() {
        let with_change = || {
            snapshot
                .oi_rows
                .iter()
                .filter(|row| row.oi_change_24h_pct.is_some())
        };
        let change = |row: &OiRow| row.oi_change_24h_pct.unwrap_or_default();
        let oi_up = top_by(with_change(), change, true, 10);
        let oi_down = top_by(with_change(), change, false, 10);
        push("## Top 10 OI 24h change up (with price change)");
        push("");
        push(&oi_table(&oi_up));
        push("");
        push("## Top 10 OI 24h change down (with price change)");
        push("");
        push(&oi_table(&oi_down));
        push("");
    }

    if !snapshot.momentum_rows.is_empty() {
        let rsi = |row: &MomentumRow| row.rsi_14.unwrap_or_default();
        let overbought = top_by(
            snapshot
                .momentum_rows
                .iter()
                .filter(|row| row.rsi_14.is_some_and(|value| value > 70.0)),
            rsi,
            true,
            usize::MAX,
        );
        let oversold = top_by(
            snapshot
                .momentum_rows
                .iter()
                .filter(|row| row.rsi_14.is_some_and(|value| value < 30.0)),
            rsi,
            false,
            usize::MAX,
        );
        for (comparison, rows) in [("> 70", &overbought), ("< 30", &oversold)] {
            push(&format!(
                "## RSI(14, 1h) {comparison} (n={}, from top {TOP_MOMENTUM} spot by volume)",
                rows.len()
            ));
            push("");
            if rows.is_empty() {
                push("None.");
            } else {
                push(&rsi_table(rows));
            }
            push("");
        }
    }

    push("## Notes");
    push("");
    push("- Facts only; no trade recommendations.");
    push(
        "- `volume_ratio` uses hourly kline quote volume: last 24 bars / mean daily volume of the preceding 6 days; \
         the latest hourly bar is still forming.",
    );
    push(
        "- `oi_change_24h_pct` compares latest `sumOpenInterestValue` to the value 24 hourly samples earlier (limit=25).",
    );
    push(
        "- `annualized_pct` = lastFundingRate x (24 / funding_interval_h) x 365 x 100; \
         `funding_interval_h` comes from `/fapi/v1/fundingInfo` (default 8h when the symbol is absent from that list).",
    );
    push("- `range_pct` = (high24h - low24h) / low24h x 100.");
    push("- `realized_vol_24h` = stdev of the last 24 hourly log returns x sqrt(24) x 100 (%).");
    push(
        "- `dist_from_7d_high_pct` = (last close / max high of the last 168 hourly bars - 1) x 100.",
    );
    push(&format!(
        "- Stablecoin/fiat bases excluded: {}",
        STABLE_FIAT_BASES.join(", ")
    ));
    let allowlist: Vec<String> = LEVERAGED_ALLOWLIST
        .iter()
        .map(|base| format!("'{base}'"))
        .collect();
    push(&format!(
        "- Leveraged tokens excluded by UP/DOWN/BULL/BEAR base suffix, with allowlist [{}].",
        allowlist.join(", ")
    ));
    push("");
    lines.join("\n")
}

// --------------------------------------------------------------------------- //
// output + CLI
// --------------------------------------------------------------------------- //

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    bb_core::fsutil::atomic_write(path, bytes).map_err(|err| StrategyError::io(path, err))
}

fn write_csv(path: &Path, header: &[&str], rows: &[Vec<String>]) -> Result<()> {
    let bytes = render_csv(header, rows).map_err(|err| StrategyError::io(path, err))?;
    write_file(path, &bytes)
}

/// Write the four CSVs and `SUMMARY.md` into `out_dir`.
pub fn write_outputs(out_dir: &Path, snapshot: &Snapshot) -> Result<()> {
    write_csv(
        &out_dir.join("spot_tickers.csv"),
        &SPOT_CSV_HEADER,
        &spot_csv_rows(&snapshot.spot_rows),
    )?;
    write_csv(
        &out_dir.join("funding.csv"),
        &FUNDING_CSV_HEADER,
        &funding_csv_rows(&snapshot.funding_rows),
    )?;
    write_csv(
        &out_dir.join("open_interest.csv"),
        &OI_CSV_HEADER,
        &oi_csv_rows(&snapshot.oi_rows),
    )?;
    write_csv(
        &out_dir.join("momentum.csv"),
        &MOMENTUM_CSV_HEADER,
        &momentum_csv_rows(&snapshot.momentum_rows),
    )?;
    write_file(
        &out_dir.join("SUMMARY.md"),
        render_summary(snapshot, out_dir).as_bytes(),
    )
}

/// Create `out_dir`, fetch the snapshot and write every output file.
pub async fn run(out_dir: &Path, config: &SnapshotConfig) -> Result<Snapshot> {
    std::fs::create_dir_all(out_dir).map_err(|err| StrategyError::io(out_dir, err))?;
    let snapshot = collect(config).await?;
    write_outputs(out_dir, &snapshot)?;
    Ok(snapshot)
}

/// `data/speculation/<UTC date YYYY-MM-DD>`.
pub fn default_out_dir() -> PathBuf {
    Path::new("data")
        .join("speculation")
        .join(bb_core::time::utc_date(bb_core::time::now()))
}

#[derive(clap::Args, Debug, Clone, Default)]
pub struct Args {
    /// output directory (default: data/speculation/<UTC date>)
    #[arg(long)]
    pub out_dir: Option<PathBuf>,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let out_dir = args.out_dir.unwrap_or_else(default_out_dir);
    let snapshot = run(&out_dir, &SnapshotConfig::default()).await?;
    println!("out-dir: {}", out_dir.display());
    println!("spot_tickers.csv: {} rows", snapshot.spot_rows.len());
    println!("funding.csv: {} rows", snapshot.funding_rows.len());
    println!(
        "open_interest.csv: {} rows (skipped {})",
        snapshot.oi_rows.len(),
        snapshot.oi_skipped
    );
    println!(
        "momentum.csv: {} rows (skipped {})",
        snapshot.momentum_rows.len(),
        snapshot.momentum_skipped
    );
    Ok(if snapshot.spot_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    const RSI_CLOSES: [f64; 20] = [
        44.34, 44.09, 44.15, 43.61, 44.33, 44.83, 45.10, 45.42, 45.84, 46.08, 45.89, 46.03, 45.61,
        46.28, 46.28, 46.00, 46.03, 46.41, 46.22, 45.64,
    ];

    fn spot(symbol: &str, quote_volume: f64) -> SpotRow {
        SpotRow {
            symbol: symbol.to_owned(),
            last_price: 1.0,
            price_change_percent: 0.0,
            quote_volume,
            high_price: 1.0,
            low_price: 1.0,
            range_pct: Some(0.0),
            count: 0,
        }
    }

    /// Same synthetic series the Python reference values were computed from.
    fn synthetic_klines(n: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|i| {
                    let close =
                        100.0 + (i % 7) as f64 * 1.5 - (i % 3) as f64 * 0.75 + i as f64 * 0.1;
                    let quote_volume =
                        1000.0 + (i % 5) as f64 * 100.0 + if i >= 176 { 500.0 } else { 0.0 };
                    json!([0, 0, close + 1.0, 0, close, 0, 0, quote_volume])
                })
                .collect(),
        )
    }

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("value present");
        assert!(
            (actual - expected).abs() <= expected.abs() * 1e-12,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn wilder_rsi_matches_python_reference() {
        assert_eq!(wilder_rsi(&RSI_CLOSES, 14), Some(57.91502067008556));
        assert_eq!(wilder_rsi(&RSI_CLOSES[..15], 14), Some(70.46413502109705));
        assert_eq!(wilder_rsi(&RSI_CLOSES[..14], 14), None);
        assert_eq!(wilder_rsi(&[1.0, 2.0, 3.0, 4.0], 3), Some(100.0));
        assert_eq!(wilder_rsi(&[1.0, 2.0], 0), None);
    }

    #[test]
    fn momentum_matches_python_reference_full_window() {
        let row = compute_momentum(&spot("TESTUSDT", 123.0), &synthetic_klines(200)).unwrap();
        assert_eq!(row.symbol, "TESTUSDT");
        assert_eq!(row.last_price, 123.65);
        assert_eq!(row.ret_1h, Some(0.6921824104234586));
        assert_eq!(row.ret_4h, Some(-3.7743190661478576));
        assert_eq!(row.ret_24h, Some(5.910064239828694));
        assert_eq!(row.ret_7d, Some(15.722976134768384));
        assert_close(row.realized_vol_24h, 14.84279690200044);
        assert_eq!(row.volume_ratio, Some(1.4227877385772123));
        assert_eq!(row.rsi_14, Some(52.171091512534595));
        assert_eq!(row.dist_from_7d_high_pct, Some(-4.517374517374517));
        assert_eq!(row.quote_volume_24h, 123.0);
    }

    #[test]
    fn momentum_short_history_leaves_long_windows_empty() {
        let row = compute_momentum(&spot("TESTUSDT", 123.0), &synthetic_klines(30)).unwrap();
        assert_eq!(row.last_price, 102.9);
        assert_eq!(row.ret_1h, Some(0.8329250367467012));
        assert_eq!(row.ret_24h, Some(-3.3802816901408406));
        assert_eq!(row.ret_7d, None);
        assert_eq!(row.volume_ratio, None);
        assert_close(row.realized_vol_24h, 19.617149598368602);
        assert_eq!(row.rsi_14, Some(48.57931387435618));
        assert_eq!(row.dist_from_7d_high_pct, Some(-8.695652173913038));
    }

    #[test]
    fn momentum_rejects_short_or_malformed_klines() {
        let row = spot("X", 1.0);
        assert_eq!(compute_momentum(&row, &synthetic_klines(1)), None);
        assert_eq!(compute_momentum(&row, &json!({"code": -1121})), None);
        assert_eq!(
            compute_momentum(
                &row,
                &json!([
                    [0, 0, "1", 0, "x", 0, 0, "1"],
                    [0, 0, "1", 0, "1", 0, 0, "1"]
                ])
            ),
            None
        );
        assert_eq!(
            compute_momentum(&row, &json!([[0, 0, "1"], [0, 0, "1"]])),
            None
        );
    }

    #[test]
    fn py_sum_is_compensated_like_cpython() {
        assert_eq!(py_sum([0.1; 10]), 1.0);
        assert_eq!(py_sum([1e100, 1.0, -1e100, 1.0]), 2.0);
        assert_eq!(py_sum(std::iter::empty()), 0.0);
    }

    #[test]
    fn fmt_and_pct_strip_like_python() {
        assert_eq!(fmt(84900.1, 10), "84900.1");
        assert_eq!(fmt(-0.00001, 4), "-0");
        assert_eq!(fmt(0.125, 2), "0.12");
        assert_eq!(fmt(f64::NAN, 4), "nan");
        assert_eq!(fmt(3_011_457.0, 0), "3011457");
        assert_eq!(fmt(100.0, 4), "100");
        assert_eq!(pct(Some(-8.7), 2), "-8.7%");
        assert_eq!(pct(None, 2), "");
        assert_eq!(fmt_opt(None, 4), "");
    }

    #[test]
    fn human_usd_matches_python() {
        let got: Vec<String> = [
            101_900_000.0,
            6e6,
            2.4e9,
            999.4,
            1500.0,
            -2e6,
            999_960_000.0,
            0.0,
        ]
        .into_iter()
        .map(human_usd)
        .collect();
        assert_eq!(
            got,
            ["101.9M", "6M", "2.4B", "999", "1.5K", "-2M", "1000M", "0"]
        );
    }

    #[test]
    fn iso_utc_formats_epoch_millis() {
        assert_eq!(
            iso_utc(Some(&json!(1_790_812_800_000_i64))),
            "2026-10-01T00:00:00Z"
        );
        assert_eq!(iso_utc(Some(&json!(0))), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(Some(&Value::Null)), "");
        assert_eq!(iso_utc(None), "");
    }

    #[test]
    fn stable_list_is_sorted_and_filters_apply() {
        assert!(STABLE_FIAT_BASES.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(is_stable_or_fiat("usdc"));
        assert!(!is_stable_or_fiat("BTC"));
        assert!(is_leveraged_token("BTCUP"));
        assert!(is_leveraged_token("ethbear"));
        assert!(!is_leveraged_token("JUP"));
        assert!(!is_leveraged_token("SYRUP"));
        assert!(!is_leveraged_token("BTC"));
    }

    #[test]
    fn spot_filter_drops_illiquid_stable_leveraged_and_sorts() {
        let raw = json!([
            {"symbol": "ETHBTC", "quoteVolume": "9e9"},
            {"symbol": "AAAUSDT", "lastPrice": "2", "priceChangePercent": "1", "quoteVolume": "2000000",
             "highPrice": "3", "lowPrice": "2", "count": 7},
            {"symbol": "BTCUPUSDT", "quoteVolume": "9e9"},
            {"symbol": "FDUSDUSDT", "quoteVolume": "9e9"},
            {"symbol": "JUPUSDT", "lastPrice": "0.4", "priceChangePercent": "-1", "quoteVolume": "5000000",
             "highPrice": "0.5", "lowPrice": "0", "count": null},
            {"symbol": "DUSTUSDT", "quoteVolume": "999999.99"},
            {"symbol": "BADUSDT", "quoteVolume": "abc"}
        ]);
        let rows = filter_spot_tickers(raw.as_array().unwrap());
        assert_eq!(
            rows,
            vec![
                SpotRow {
                    symbol: "JUPUSDT".into(),
                    last_price: 0.4,
                    price_change_percent: -1.0,
                    quote_volume: 5e6,
                    high_price: 0.5,
                    low_price: 0.0,
                    range_pct: None,
                    count: 0,
                },
                SpotRow {
                    symbol: "AAAUSDT".into(),
                    last_price: 2.0,
                    price_change_percent: 1.0,
                    quote_volume: 2e6,
                    high_price: 3.0,
                    low_price: 2.0,
                    range_pct: Some(50.0),
                    count: 7,
                },
            ]
        );
    }

    #[test]
    fn funding_rows_use_interval_map_and_skip_delivery_and_illiquid() {
        let premium = json!([
            {"symbol": "AUSDT", "markPrice": "101", "indexPrice": "100", "lastFundingRate": "0.0001",
             "nextFundingTime": 1_790_812_800_000_i64},
            {"symbol": "AUSDT_261226", "markPrice": "1", "indexPrice": "1", "lastFundingRate": "0"},
            {"symbol": "BUSDT", "markPrice": "1", "indexPrice": "0", "lastFundingRate": "-0.001"},
            {"symbol": "CUSDT", "markPrice": "1", "indexPrice": "1", "lastFundingRate": "0"},
            {"symbol": "DUSDT", "indexPrice": "1", "lastFundingRate": "0"}
        ]);
        let tickers = json!([
            {"symbol": "AUSDT", "quoteVolume": "6000000", "priceChangePercent": "2"},
            {"symbol": "BUSDT", "quoteVolume": "9000000", "priceChangePercent": "-3"},
            {"symbol": "CUSDT", "quoteVolume": "4999999", "priceChangePercent": "0"},
            {"symbol": "DUSDT", "quoteVolume": "9000000", "priceChangePercent": "0"}
        ]);
        let intervals = parse_funding_intervals(
            json!([
                {"symbol": "BUSDT", "fundingIntervalHours": 4},
                {"symbol": "ZUSDT"},
                {"symbol": "YUSDT", "fundingIntervalHours": 0}
            ])
            .as_array()
            .unwrap(),
        );
        assert_eq!(intervals.len(), 1);
        let rows = build_funding_rows(
            premium.as_array().unwrap(),
            tickers.as_array().unwrap(),
            &intervals,
        );
        let symbols: Vec<&str> = rows.iter().map(|row| row.symbol.as_str()).collect();
        assert_eq!(symbols, ["BUSDT", "AUSDT"]);
        assert_eq!(rows[0].funding_interval_h, 4);
        assert_eq!(
            rows[0].annualized_pct,
            -0.001 * (24.0 / 4.0) * 365.0 * 100.0
        );
        assert_eq!(rows[0].basis_bps, None);
        assert_eq!(rows[0].next_funding_time, "");
        assert_eq!(rows[1].funding_interval_h, 8);
        assert_eq!(rows[1].basis_bps, Some(100.0));
        assert_eq!(rows[1].next_funding_time, "2026-10-01T00:00:00Z");
    }

    #[test]
    fn oi_row_needs_two_samples_and_tolerates_missing_ratio() {
        let perp = FundingRow {
            symbol: "AUSDT".into(),
            mark_price: 1.0,
            index_price: 1.0,
            last_funding_rate: 0.0,
            funding_interval_h: 8,
            annualized_pct: 0.0,
            basis_bps: Some(0.0),
            next_funding_time: String::new(),
            quote_volume_24h: 6e6,
            price_change_percent: 2.5,
        };
        let history = json!([{"sumOpenInterestValue": "100"}, {"sumOpenInterestValue": "125"}]);
        let row = compute_oi_row(
            &perp,
            Some(&history),
            Some(&json!([{"longShortRatio": "1.5"}])),
        )
        .unwrap();
        assert_eq!(row.oi_now_usd, 125.0);
        assert_eq!(row.oi_change_24h_pct, Some(25.0));
        assert_eq!(row.long_short_ratio, Some(1.5));
        assert_eq!(row.price_change_24h_pct, 2.5);
        let row = compute_oi_row(&perp, Some(&history), Some(&json!([]))).unwrap();
        assert_eq!(row.long_short_ratio, None);
        let zero_first = json!([{"sumOpenInterestValue": "0"}, {"sumOpenInterestValue": "5"}]);
        assert_eq!(
            compute_oi_row(&perp, Some(&zero_first), None)
                .unwrap()
                .oi_change_24h_pct,
            None
        );
        assert_eq!(
            compute_oi_row(&perp, Some(&json!([{"sumOpenInterestValue": "1"}])), None),
            None
        );
        assert_eq!(compute_oi_row(&perp, None, None), None);
    }

    #[test]
    fn csv_uses_crlf_and_minimal_quoting() {
        let bytes = render_csv(
            &["symbol", "note"],
            &[
                vec!["A,B".into(), "plain".into()],
                vec!["牛来USDT".into(), String::new()],
            ],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "symbol,note\r\n\"A,B\",plain\r\n牛来USDT,\r\n"
        );
    }

    #[test]
    fn md_table_renders_pipes() {
        assert_eq!(
            md_table(&["a", "b"], &[vec!["1".into(), "2".into()]]),
            "| a | b |\n|---|---|\n| 1 | 2 |"
        );
    }

    #[test]
    fn summary_reports_network_error_and_empty_failures() {
        let snapshot = Snapshot {
            generated_at: "2026-10-11T00:00:00Z".into(),
            funding_meta: FundingMeta {
                available: false,
                status: Some(HttpStatus::NetworkError),
                configured: 0,
            },
            ..Snapshot::default()
        };
        let summary = render_summary(&snapshot, Path::new("out"));
        assert!(summary.contains(
            "- Futures API status: unavailable (network error); snapshot is spot-only where futures data is missing.\n"
        ));
        assert!(summary.contains("## Endpoint failures\n\nNone.\n"));
        assert!(!summary.contains("## Top 15"));
        assert!(summary.ends_with("allowlist ['JUP', 'SYRUP'].\n"));
    }
}

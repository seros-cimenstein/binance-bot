//! Helpers shared by the strategy runtimes and the backtest harnesses:
//! kline rows, the cached concurrent kline fetcher, ISO parsing, Python-compatible
//! rounding/statistics and sorted-key JSON persistence.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use bb_binance::history::HistoryFetcher;
use bb_binance::models::Kline;
use bb_binance::{BinanceClient, BinanceConfig};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use futures::{StreamExt as _, TryStreamExt as _};
use rust_decimal::Decimal;
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, StrategyError};

/// Binance REST hosts the Python modules hard-coded for public market data.
pub const SPOT_REST_URL: &str = "https://api.binance.com";
pub const FUTURES_REST_URL: &str = "https://fapi.binance.com";

/// Bars per year for each Binance kline interval (identical table in every
/// Python backtest module; each module supplies its own fallback default).
const BARS_PER_YEAR_BY_INTERVAL: [(&str, f64); 12] = [
    ("1m", 525_600.0),
    ("3m", 175_200.0),
    ("5m", 105_120.0),
    ("15m", 35_040.0),
    ("30m", 17_520.0),
    ("1h", 8_760.0),
    ("2h", 4_380.0),
    ("4h", 2_190.0),
    ("6h", 1_460.0),
    ("8h", 1_095.0),
    ("12h", 730.0),
    ("1d", 365.0),
];

pub fn bars_per_year(interval: &str, default: f64) -> f64 {
    BARS_PER_YEAR_BY_INTERVAL
        .iter()
        .find(|(name, _)| *name == interval)
        .map_or(default, |(_, bars)| *bars)
}

/// One kline reduced to the fields the strategies read (`[open_ms, open, high, low, close, ...]`).
/// Prices are `f64`: the replays are statistical simulations whose outputs must match
/// the Python float arithmetic bit-for-bit, not money ledgers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub open_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

impl Bar {
    pub fn new(open_ms: i64, open: f64, high: f64, low: f64, close: f64) -> Self {
        Self {
            open_ms,
            open,
            high,
            low,
            close,
        }
    }

    pub fn from_kline(kline: &Kline) -> Self {
        Self {
            open_ms: kline.open_time,
            open: decimal_f64(kline.open),
            high: decimal_f64(kline.high),
            low: decimal_f64(kline.low),
            close: decimal_f64(kline.close),
        }
    }

    /// Parse a raw kline row (numbers or numeric strings, Binance or fixture shaped).
    pub fn from_values(row: &[Value]) -> std::result::Result<Self, String> {
        if row.len() < 5 {
            return Err(format!(
                "kline row needs at least 5 fields, got {}",
                row.len()
            ));
        }
        let open_ms =
            value_i64(&row[0]).ok_or_else(|| format!("invalid kline open time: {}", row[0]))?;
        let num = |i: usize| {
            value_f64(&row[i]).ok_or_else(|| format!("invalid kline field {i}: {}", row[i]))
        };
        Ok(Self {
            open_ms,
            open: num(1)?,
            high: num(2)?,
            low: num(3)?,
            close: num(4)?,
        })
    }
}

impl<'de> Deserialize<'de> for Bar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct RowVisitor;
        impl<'de> Visitor<'de> for RowVisitor {
            type Value = Bar;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a kline row [open_ms, open, high, low, close, ...]")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Bar, A::Error> {
                let mut values = Vec::with_capacity(12);
                while let Some(v) = seq.next_element::<Value>()? {
                    values.push(v);
                }
                Bar::from_values(&values).map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_seq(RowVisitor)
    }
}

/// Python `float(str(d))`: correctly rounded, unlike a mantissa/scale division.
pub fn decimal_f64(value: Decimal) -> f64 {
    value.to_string().parse().unwrap_or(f64::NAN)
}

/// Python `bool` env helper: unset/blank → `default`, else `{1,true,yes,on}` (case-insensitive).
pub fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => matches!(
            value.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        _ => default,
    }
}

/// `os.getenv(name, default)`: set-but-empty stays empty.
pub fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// `T(os.getenv(name) or default)`: unset/blank → `default`; `Err(raw value)` when unparsable.
pub fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> std::result::Result<T, String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value.trim().parse().map_err(|_| value),
        _ => Ok(default),
    }
}

/// Decimal variant of [`env_parse`] accepting Python float syntax (`1e3`).
pub fn env_decimal(name: &str, default: Decimal) -> std::result::Result<Decimal, String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value_decimal(&Value::String(value.clone())).ok_or(value),
        _ => Ok(default),
    }
}

/// `tuple(s.strip() for s in os.getenv(name, "").split(",") if s.strip())`.
pub fn env_csv(name: &str) -> Vec<String> {
    split_csv(&std::env::var(name).unwrap_or_default())
}

/// Python `os.getenv(name, "").strip().lower() == "yes"`.
pub fn env_yes(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.trim().eq_ignore_ascii_case("yes"))
}

/// Python `float(x)` over a JSON scalar.
pub fn value_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Python `int(x)` over a JSON scalar.
pub fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Symbol → bars. Ordered by symbol so every replay is deterministic.
pub type KlineSet = BTreeMap<String, Vec<Bar>>;

/// Raw Binance kline array (`[openTime, "open", "high", "low", "close", "volume",
/// closeTime, "quoteVolume", trades, "takerBuyBase", "takerBuyQuote", "0"]`), the
/// on-disk cache format the Python harnesses wrote.
pub fn kline_row(kline: &Kline) -> Value {
    let dec = |d: Decimal| Value::String(d.to_string());
    let mut row = vec![
        Value::from(kline.open_time),
        dec(kline.open),
        dec(kline.high),
        dec(kline.low),
        dec(kline.close),
        dec(kline.volume),
    ];
    if let Some(close_time) = kline.close_time {
        row.push(Value::from(close_time));
        row.push(dec(kline.quote_volume.unwrap_or_default()));
        row.push(Value::from(kline.trades.unwrap_or_default()));
        row.push(dec(kline.taker_buy_base_volume.unwrap_or_default()));
        row.push(dec(kline.taker_buy_quote_volume.unwrap_or_default()));
        row.push(Value::String("0".into()));
    }
    Value::Array(row)
}

/// `{symbol: [kline, ...]}` fixture (Python `_load_fixture`).
pub fn load_kline_fixture(path: &Path) -> Result<KlineSet> {
    let text = std::fs::read_to_string(path).map_err(|e| StrategyError::io(path, e))?;
    let raw: Value = serde_json::from_str(&text).map_err(|e| json_error(path, &e))?;
    let Value::Object(map) = raw else {
        return Err(StrategyError::InvalidConfig(
            "klines fixture must be a mapping of symbol -> klines".into(),
        ));
    };
    map.into_iter()
        .map(|(symbol, rows)| {
            let bars: Vec<Bar> = serde_json::from_value(rows).map_err(|e| json_error(path, &e))?;
            Ok((symbol, bars))
        })
        .collect()
}

pub fn json_error(path: &Path, err: &serde_json::Error) -> StrategyError {
    StrategyError::Json {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

/// Python `datetime.fromisoformat` with naive values interpreted as UTC.
pub fn parse_iso(value: &str) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(value, fmt) {
            return Ok(dt.with_timezone(&Utc));
        }
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, fmt) {
            return Ok(naive.and_utc());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    Err(StrategyError::InvalidConfig(format!(
        "Invalid isoformat string: '{value}'"
    )))
}

/// `[s.strip() for s in raw.split(",") if s.strip()]`.
pub fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Public mainnet client rooted at the hosts the Python harnesses used.
pub fn public_client() -> Result<BinanceClient> {
    Ok(BinanceClient::new(BinanceConfig::new(false))?)
}

fn cache_key(symbol: &str) -> String {
    symbol.replace(['/', ':'], "_")
}

/// `{cache_dir}/{interval}_{SYM}_{startTs}_{endTs}.json` (Python `_fetch_klines_for_symbols`).
pub fn kline_cache_path(
    cache_dir: &Path,
    interval: &str,
    symbol: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> PathBuf {
    cache_dir.join(format!(
        "{interval}_{}_{}_{}.json",
        cache_key(symbol),
        start.timestamp(),
        end.timestamp()
    ))
}

/// Fetch spot klines for many symbols concurrently, reading/writing the optional disk
/// cache (port of `claude_spot_backtest._fetch_klines_for_symbols`). An unreadable cache
/// file is ignored and refetched; any fetch failure fails the whole call.
pub async fn fetch_klines_for_symbols(
    client: &BinanceClient,
    symbols: &[String],
    interval: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    cache_dir: Option<&Path>,
    concurrency: usize,
) -> Result<KlineSet> {
    let fetcher = HistoryFetcher::spot(client.clone());
    let fetcher = &fetcher;
    futures::stream::iter(symbols.iter().cloned())
        .map(|symbol| async move {
            let path = cache_dir.map(|dir| kline_cache_path(dir, interval, &symbol, start, end));
            if let Some(path) = path.as_deref()
                && let Some(bars) = read_cached_bars(path)
            {
                return Ok((symbol, bars));
            }
            let klines = fetcher
                .fetch_klines(&symbol, interval, start, Some(end))
                .await?;
            if let Some(path) = path.as_deref() {
                let rows: Vec<Value> = klines.iter().map(kline_row).collect();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| StrategyError::io(parent, e))?;
                }
                let bytes = serde_json::to_vec(&rows)?;
                std::fs::write(path, bytes).map_err(|e| StrategyError::io(path, e))?;
            }
            Ok::<_, StrategyError>((symbol, klines.iter().map(Bar::from_kline).collect()))
        })
        .buffer_unordered(concurrency.max(1))
        .try_collect()
        .await
}

fn read_cached_bars(path: &Path) -> Option<Vec<Bar>> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Python `round(x, digits)` (correctly rounded, ties-to-even on the exact binary value).
pub fn py_round(x: f64, digits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// `statistics.fmean`.
pub fn fmean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    fsum(values) / values.len() as f64
}

/// `statistics.pstdev`.
pub fn pstdev(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = fmean(values);
    let squares: Vec<f64> = values.iter().map(|v| (v - mean) * (v - mean)).collect();
    (fsum(&squares) / values.len() as f64).sqrt()
}

/// `statistics.stdev` (sample standard deviation); `0.0` with fewer than two values.
pub fn stdev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = fmean(values);
    let squares: Vec<f64> = values.iter().map(|v| (v - mean) * (v - mean)).collect();
    (fsum(&squares) / (values.len() - 1) as f64).sqrt()
}

/// `math.fsum` (Shewchuk exact summation).
pub fn fsum(values: &[f64]) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    for &value in values {
        let mut x = value;
        let mut i = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        partials.truncate(i);
        partials.push(x);
    }
    partials.iter().sum()
}

/// Python 3.12+ builtin `sum()` over floats (Neumaier compensated summation).
pub fn py_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0_f64;
    let mut compensation = 0.0_f64;
    for x in values {
        let t = total + x;
        if total.abs() >= x.abs() {
            compensation += (total - t) + x;
        } else {
            compensation += (x - t) + total;
        }
        total = t;
    }
    if compensation != 0.0 && compensation.is_finite() {
        total += compensation;
    }
    total
}

/// Python `repr(float)` (shortest round-trip; `1.0`, `0.0001`, `1e-05`, `1e+16`).
pub fn py_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    let magnitude = x.abs();
    if magnitude == 0.0 || (1e-4..1e16).contains(&magnitude) {
        let plain = x.to_string();
        return if plain.contains('.') {
            plain
        } else {
            format!("{plain}.0")
        };
    }
    let sci = format!("{x:e}");
    match sci.split_once('e') {
        Some((mantissa, exp)) => {
            let exp: i32 = exp.parse().unwrap_or(0);
            let sign = if exp < 0 { '-' } else { '+' };
            format!("{mantissa}e{sign}{:02}", exp.unsigned_abs())
        }
        None => sci,
    }
}

/// Annualised Sharpe of per-step returns (`mean / pstdev * sqrt(bars_per_year)`), `0.0`
/// with fewer than two returns or zero variance — the formula every replay shares.
pub fn sharpe_annualized(returns: &[f64], bars_per_year: f64) -> f64 {
    if returns.len() <= 1 {
        return 0.0;
    }
    let sd = pstdev(returns);
    if sd > 0.0 {
        fmean(returns) / sd * bars_per_year.sqrt()
    } else {
        0.0
    }
}

/// Shared backtest verdict ladder.
pub fn verdict(
    bars_evaluated: usize,
    num_trades: usize,
    sharpe: f64,
    total_return_pct: f64,
) -> &'static str {
    if bars_evaluated == 0 {
        "no_data"
    } else if num_trades == 0 {
        "no_trades"
    } else if sharpe > 1.0 && total_return_pct > 0.0 {
        "edge_likely"
    } else if total_return_pct > 0.0 {
        "edge_marginal"
    } else {
        "no_edge"
    }
}

/// Serialise with sorted keys (Python `json.dumps(..., sort_keys=True)`).
pub fn sorted_value<T: Serialize + ?Sized>(value: &T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

/// `json.dumps(payload, indent=2, sort_keys=True)`.
pub fn to_pretty_sorted<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(&sorted_value(value)?)?)
}

/// Print command output (pretty, sorted keys) to stdout.
pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", to_pretty_sorted(value)?);
    Ok(())
}

/// Append one sorted-key JSON line, creating parent directories.
pub fn append_jsonl<T: Serialize + ?Sized>(path: &Path, record: &T) -> Result<()> {
    append_jsonl_many(path, std::slice::from_ref(&sorted_value(record)?))
}

/// Append many sorted-key JSON lines in one write.
pub fn append_jsonl_many<T: Serialize>(path: &Path, records: &[T]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut buf = Vec::new();
    for record in records {
        serde_json::to_writer(&mut buf, &sorted_value(record)?)?;
        buf.push(b'\n');
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| StrategyError::io(parent, e))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| StrategyError::io(path, e))?;
    file.write_all(&buf)
        .map_err(|e| StrategyError::io(path, e))?;
    file.sync_data().map_err(|e| StrategyError::io(path, e))
}

/// Atomic `json.dumps(state, indent=2, sort_keys=True)` state write.
pub fn save_json_sorted<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let text = to_pretty_sorted(value)?;
    bb_core::fsutil::atomic_write(path, text.as_bytes()).map_err(|e| StrategyError::io(path, e))
}

/// Write a report file (`Path(output).write_text(json.dumps(..., indent=2, sort_keys=True))`).
pub fn write_report<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let text = to_pretty_sorted(value)?;
    std::fs::write(path, text).map_err(|e| StrategyError::io(path, e))
}

/// `int(datetime.now(tz=timezone.utc).timestamp() * 1000)`.
pub fn utc_now_ms() -> i64 {
    bb_core::time::now_ms()
}

/// `datetime.now(tz=timezone.utc).isoformat()`.
pub fn utc_timestamp() -> String {
    python_isoformat(bb_core::time::now())
}

/// Python `datetime.isoformat()` for an aware UTC timestamp.
pub fn python_isoformat(ts: DateTime<Utc>) -> String {
    if ts.timestamp_subsec_micros() == 0 {
        ts.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// Python `src.alerts.maybe_telegram(message)`: escape plain text, send if configured,
/// never fails.
pub async fn alert(message: &str) -> bool {
    bb_core::telegram::maybe_send(&bb_core::telegram::escape(message)).await
}

/// Parse a JSON scalar (number or numeric string, incl. Python float reprs like `1e-05`).
pub fn value_decimal(value: &Value) -> Option<Decimal> {
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_owned(),
        _ => return None,
    };
    Decimal::from_str_exact(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .ok()
}

/// Serde adapter keeping money fields as plain JSON numbers (the Python state/ledger
/// files hold floats) while the Rust side stays `Decimal`.
pub mod decimal_number {
    use rust_decimal::Decimal;
    use serde::de::Error as _;
    use serde::ser::Error as _;
    use serde::{Deserialize as _, Deserializer, Serialize as _, Serializer};
    use serde_json::Value;

    pub fn serialize<S: Serializer>(value: &Decimal, serializer: S) -> Result<S::Ok, S::Error> {
        let number: serde_json::Number = value
            .normalize()
            .to_string()
            .parse()
            .map_err(S::Error::custom)?;
        number.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
        let value = Value::deserialize(deserializer)?;
        super::value_decimal(&value)
            .ok_or_else(|| D::Error::custom(format!("expected a decimal number, got {value}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_round_matches_python() {
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(-1.23456, 4), -1.2346);
    }
    #[test]
    fn bars_parse_strings_and_numbers() {
        let bars: Vec<Bar> =
            serde_json::from_str(r#"[[1, "1.5", 2, "0.5", "1.0", "9"], [2, 1, 1, 1, 1]]"#).unwrap();
        assert_eq!(bars[0], Bar::new(1, 1.5, 2.0, 0.5, 1.0));
        assert_eq!(bars[1].open_ms, 2);
    }

    #[test]
    fn parse_iso_variants() {
        let a = parse_iso("2024-01-01").unwrap();
        let b = parse_iso("2024-01-01T00:00:00").unwrap();
        let c = parse_iso("2024-01-01T00:00:00+00:00").unwrap();
        let d = parse_iso("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(c, d);
        assert_eq!(parse_iso("2024-01-01T03:00:00+03:00").unwrap(), a);
    }

    #[test]
    fn stats_match_python() {
        let xs = [0.01, -0.02, 0.03];
        assert!((fmean(&xs) - 0.006_666_666_666_666_667).abs() < 1e-15);
        assert!((pstdev(&xs) - 0.020_548_046_676_563_25).abs() < 1e-12);
        assert_eq!(fsum(&[0.1; 10]), 1.0);
    }
}

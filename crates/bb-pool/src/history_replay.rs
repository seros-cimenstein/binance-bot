//! Isolated public-candle replay for paper Spot strategy research.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use async_trait::async_trait;
use bb_binance::history::HistoryFetcher;
use bb_binance::models::Ohlcv;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::error::{PoolError, Result};
use crate::pyfmt::{decimal_number, py_isoformat};
use crate::replay::{
    FixtureCommission, FixtureMarket, ReplayFixture, ReplaySummary, run_replay_fixture,
};

/// Default research ledger path.
pub const DEFAULT_HISTORY_DB: &str = "/tmp/binance-bot-history-replay.db";
/// Scope statement attached to every research summary.
pub const SCOPE: &str = "research only: public direct-USDT candles with modeled fees/spread/minimums; \
                         does not satisfy live readiness";

fn history_error(message: impl Into<String>) -> PoolError {
    PoolError::HistoricalReplay(message.into())
}

/// Public historical candle reader.
#[async_trait]
pub trait CandleFetcher: Send + Sync {
    /// Candles for `symbol` (`BTC/USDT`) between `start` and `end`.
    async fn fetch_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<Ohlcv>>;
}

#[async_trait]
impl CandleFetcher for HistoryFetcher {
    async fn fetch_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<Ohlcv>> {
        Ok(HistoryFetcher::fetch_ohlcv(self, symbol, timeframe, start, end).await?)
    }
}

/// Modeled costs and window for one research replay.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalReplayRequest {
    pub symbols: Vec<String>,
    pub initial_balances: Vec<(String, Decimal)>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub timeframe: String,
    pub required_observations: usize,
    pub cycles: Option<usize>,
    pub taker_fee: Decimal,
    pub spread_bps: Decimal,
    pub slippage_buffer_bps: Decimal,
    pub min_notional: Decimal,
}

impl HistoricalReplayRequest {
    /// Request with the Python CLI defaults.
    pub fn new(
        symbols: Vec<String>,
        initial_balances: Vec<(String, Decimal)>,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> Self {
        Self {
            symbols,
            initial_balances,
            start_time,
            end_time,
            timeframe: "15m".into(),
            required_observations: 96,
            cycles: None,
            taker_fee: Decimal::new(1, 3),
            spread_bps: Decimal::TEN,
            slippage_buffer_bps: Decimal::from(25),
            min_notional: Decimal::from(5),
        }
    }
}

/// Outcome and boundaries of a public-candle strategy evaluation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoricalReplaySummary {
    pub symbols: Vec<String>,
    pub timeframe: String,
    pub candle_cycles: usize,
    pub started_at: String,
    pub ended_at: String,
    pub db_path: String,
    pub replay: ReplaySummary,
    pub scope: String,
}

/// Evaluate direct-USDT assets using historical public candles only, in an isolated ledger.
pub async fn run_historical_replay(
    request: &HistoricalReplayRequest,
    db_path: impl AsRef<Path>,
    fetcher: &dyn CandleFetcher,
) -> Result<HistoricalReplaySummary> {
    if request.end_time <= request.start_time {
        return Err(history_error("end time must be after start time"));
    }
    if request.required_observations <= 1 {
        return Err(history_error(
            "required observations must be greater than one",
        ));
    }
    let balances = normalize_balances(&request.initial_balances)?;
    let selected = required_symbols(&request.symbols, &balances)?;

    let mut series: BTreeMap<String, BTreeMap<i64, (Decimal, Decimal)>> = BTreeMap::new();
    for symbol in &selected {
        let rows = fetcher
            .fetch_ohlcv(
                symbol,
                &request.timeframe,
                request.start_time,
                Some(request.end_time),
            )
            .await?;
        let parsed = candle_series(&rows)?;
        if parsed.is_empty() {
            return Err(history_error(format!(
                "no historical candles returned for {symbol}"
            )));
        }
        series.insert(symbol.clone(), parsed);
    }
    let mut common: Option<BTreeSet<i64>> = None;
    for values in series.values() {
        let keys: BTreeSet<i64> = values.keys().copied().collect();
        common = Some(match common {
            None => keys,
            Some(acc) => acc.intersection(&keys).copied().collect(),
        });
    }
    let mut common_times: Vec<i64> = common.unwrap_or_default().into_iter().collect();
    if common_times.len() < request.required_observations {
        return Err(history_error(format!(
            "historical series do not contain enough aligned candles: {}/{}",
            common_times.len(),
            request.required_observations
        )));
    }
    if let Some(cycles) = request.cycles {
        if cycles == 0 {
            return Err(history_error("cycles must be positive"));
        }
        common_times = common_times.split_off(common_times.len().saturating_sub(cycles));
    }

    let fixture = build_fixture(request, &selected, &balances, &series, &common_times)?;
    let db_path = db_path.as_ref();
    let replay = run_replay_fixture(fixture, Some(common_times.len()), db_path, true).await?;
    let first = common_times.first().copied().unwrap_or_default();
    let last = common_times.last().copied().unwrap_or_default();
    Ok(HistoricalReplaySummary {
        symbols: selected,
        timeframe: request.timeframe.clone(),
        candle_cycles: common_times.len(),
        started_at: iso_ms(first)?,
        ended_at: iso_ms(last)?,
        db_path: db_path.display().to_string(),
        replay,
        scope: SCOPE.into(),
    })
}

fn iso_ms(ms: i64) -> Result<String> {
    bb_core::time::from_ms(ms)
        .map(py_isoformat)
        .ok_or_else(|| history_error(format!("invalid candle timestamp: {ms}")))
}

fn required_symbols(
    requested: &[String],
    balances: &BTreeMap<String, Decimal>,
) -> Result<Vec<String>> {
    let mut symbols = BTreeSet::new();
    for symbol in requested.iter().filter(|s| !s.trim().is_empty()) {
        symbols.insert(direct_usdt_symbol(symbol)?);
    }
    for asset in balances
        .keys()
        .map(String::as_str)
        .chain(["BTC", "PAXG", "XAUT"])
    {
        if asset != "USDT" {
            symbols.insert(format!("{asset}/USDT"));
        }
    }
    Ok(symbols.into_iter().collect())
}

/// Normalize `ETHUSDT` / `eth-usdt` / `ETH/USDT` to `ETH/USDT`; only direct USDT markets.
pub fn direct_usdt_symbol(symbol: &str) -> Result<String> {
    let mut value = symbol.trim().to_uppercase().replace('-', "/");
    if !value.contains('/') {
        match value.strip_suffix("USDT") {
            Some(base) if !base.is_empty() => value = format!("{base}/USDT"),
            _ => {
                return Err(history_error(
                    "historical analysis symbols must be direct USDT markets",
                ));
            }
        }
    }
    match value.split_once('/') {
        Some((base, "USDT")) if !base.is_empty() => Ok(format!("{base}/USDT")),
        _ => Err(history_error(
            "historical analysis currently supports direct */USDT Spot markets only",
        )),
    }
}

fn normalize_balances(raw: &[(String, Decimal)]) -> Result<BTreeMap<String, Decimal>> {
    let mut balances = BTreeMap::new();
    for (asset, amount) in raw {
        let normalized = asset.trim().to_uppercase();
        if normalized.is_empty() || *amount <= Decimal::ZERO {
            return Err(history_error(
                "initial balances must be positive asset amounts",
            ));
        }
        balances.insert(normalized, *amount);
    }
    if balances.is_empty() {
        return Err(history_error("at least one initial balance is required"));
    }
    Ok(balances)
}

/// `open_time -> (close, close * volume)`.
fn candle_series(rows: &[Ohlcv]) -> Result<BTreeMap<i64, (Decimal, Decimal)>> {
    rows.iter()
        .map(|row| {
            if row.close <= Decimal::ZERO || row.volume.is_sign_negative() && !row.volume.is_zero()
            {
                return Err(history_error("historical candle values are invalid"));
            }
            let quote_volume = row
                .close
                .checked_mul(row.volume)
                .ok_or_else(|| history_error("historical candle values are invalid"))?;
            Ok((row.timestamp, (row.close, quote_volume)))
        })
        .collect()
}

fn build_fixture(
    request: &HistoricalReplayRequest,
    symbols: &[String],
    balances: &BTreeMap<String, Decimal>,
    series: &BTreeMap<String, BTreeMap<i64, (Decimal, Decimal)>>,
    common_times: &[i64],
) -> Result<ReplayFixture> {
    let costs = [
        request.taker_fee,
        request.spread_bps,
        request.slippage_buffer_bps,
        request.min_notional,
    ];
    if costs.iter().any(|v| v.is_sign_negative() && !v.is_zero()) {
        return Err(history_error("modeled cost inputs must be non-negative"));
    }
    let max_spread = (request.spread_bps + Decimal::new(1, 3)).max(Decimal::ONE);
    let settings = json!({
        "benchmarks": ["USDT", "BTC", "PAXG", "XAUT"],
        "benchmark_weights": [0.25, 0.25, 0.25, 0.25],
        "min_quote_volume_usdt": 0,
        // Allow the modeled spread itself plus a small tolerance.
        "max_spread_bps": Value::Number(decimal_number(max_spread)),
        "slippage_buffer_bps": Value::Number(decimal_number(request.slippage_buffer_bps)),
        "ticker_stale_seconds": 0,
        "max_scoring_assets": 0,
        "momentum_observations": request.required_observations,
        "decision_cooldown_seconds": 0,
        "max_allocation_percent": 100,
        "max_reallocation_percent": 10,
        "min_improvement_bps": 0,
    });
    let Value::Object(settings) = settings else {
        return Err(history_error("internal: settings must be an object"));
    };
    let lookup =
        |symbol: &str, timestamp: i64| series.get(symbol).and_then(|s| s.get(&timestamp)).copied();
    let markets = symbols
        .iter()
        .map(|symbol| {
            let points = common_times
                .iter()
                .map(|t| {
                    lookup(symbol, *t)
                        .ok_or_else(|| history_error("historical series are misaligned"))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(FixtureMarket {
                symbol: symbol.clone(),
                base: None,
                quote: None,
                price_series: Some(points.iter().map(|(close, _)| *close).collect()),
                prices: None,
                price: None,
                quote_volume_series: Some(points.iter().map(|(_, volume)| *volume).collect()),
                quote_volume: None,
                min_amount: Some(Decimal::new(1, 8)),
                min_notional: Some(request.min_notional),
                amount_precision: Some(Decimal::new(1, 8)),
                spread_bps: Some(request.spread_bps),
                spot: None,
                active: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ReplayFixture {
        started_at: iso_ms(common_times.first().copied().unwrap_or_default())?,
        cycle_seconds: Decimal::from(timeframe_seconds(&request.timeframe)?),
        initial_balances: balances.clone(),
        commission: FixtureCommission {
            maker: Some(request.taker_fee),
            taker: request.taker_fee,
            discount_enabled: false,
            ..FixtureCommission::default()
        },
        settings: Map::from_iter(settings),
        markets,
    })
}

/// `15m` / `1h` / `1d` in seconds.
pub fn timeframe_seconds(timeframe: &str) -> Result<i64> {
    let normalized = timeframe.trim().to_lowercase();
    let invalid = || history_error("timeframe must look like 15m, 1h, or 1d");
    let unit = match normalized.chars().last().ok_or_else(invalid)? {
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(invalid()),
    };
    let value: i64 = normalized[..normalized.len() - 1]
        .parse()
        .map_err(|_| invalid())?;
    let seconds = value.checked_mul(unit).ok_or_else(invalid)?;
    if seconds <= 0 {
        return Err(history_error("timeframe must be positive"));
    }
    Ok(seconds)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::report::{ReportOptions, build_report};

    struct FakeCandleFetcher {
        symbols: parking_lot::Mutex<Vec<String>>,
        misalign: bool,
    }

    impl FakeCandleFetcher {
        fn new(misalign: bool) -> Self {
            Self {
                symbols: parking_lot::Mutex::new(Vec::new()),
                misalign,
            }
        }
    }

    #[async_trait]
    impl CandleFetcher for FakeCandleFetcher {
        async fn fetch_ohlcv(
            &self,
            symbol: &str,
            timeframe: &str,
            start: DateTime<Utc>,
            _end: Option<DateTime<Utc>>,
        ) -> Result<Vec<Ohlcv>> {
            self.symbols.lock().push(symbol.to_owned());
            assert_eq!(timeframe, "15m");
            let base = symbol.split_once('/').unwrap().0;
            let prices: Vec<Decimal> = match base {
                "BNB" => vec![dec!(600); 6],
                "BTC" => vec![dec!(70000); 6],
                "PAXG" => vec![dec!(4500); 6],
                "XAUT" => vec![dec!(4490); 6],
                _ => vec![
                    dec!(1),
                    dec!(1.05),
                    dec!(1.10),
                    dec!(1.20),
                    dec!(1.30),
                    dec!(1.40),
                ],
            };
            let offset = i64::from(self.misalign && base == "EDEN");
            Ok(prices
                .into_iter()
                .zip(0i64..)
                .map(|(price, index)| Ohlcv {
                    timestamp: (start + chrono::Duration::minutes(15 * (index + offset)))
                        .timestamp_millis(),
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume: dec!(1000000),
                })
                .collect())
        }
    }

    fn request(required: usize) -> HistoricalReplayRequest {
        let start = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).single().unwrap();
        let mut request = HistoricalReplayRequest::new(
            vec!["EDEN/USDT".into()],
            vec![("BNB".into(), dec!(0.2))],
            start,
            start + chrono::Duration::hours(2),
        );
        request.required_observations = required;
        request
    }

    #[tokio::test]
    async fn uses_public_candles_in_isolated_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.db");
        let fetcher = FakeCandleFetcher::new(false);
        let summary = run_historical_replay(&request(4), &db, &fetcher)
            .await
            .unwrap();
        let mut fetched = fetcher.symbols.lock().clone();
        fetched.sort();
        assert_eq!(
            fetched,
            [
                "BNB/USDT",
                "BTC/USDT",
                "EDEN/USDT",
                "PAXG/USDT",
                "XAUT/USDT"
            ]
        );
        assert_eq!(summary.candle_cycles, 6);
        assert!(summary.replay.decisions["rebalance"] >= 1);
        assert!(summary.scope.contains("research only"));
        let options = ReportOptions {
            required_observations: 4,
            ..ReportOptions::default()
        };
        assert!(build_report(&db, &options).unwrap().ranking_available());
    }

    #[tokio::test]
    async fn requires_enough_aligned_candles() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_historical_replay(
            &request(6),
            dir.path().join("misaligned.db"),
            &FakeCandleFetcher::new(true),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("aligned candles"));
    }

    #[tokio::test]
    async fn refuses_production_paper_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_historical_replay(
            &request(4),
            dir.path().join("pool-paper.db"),
            &FakeCandleFetcher::new(false),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("production paper ledger"));
    }

    #[test]
    fn symbols_and_timeframes() {
        assert_eq!(direct_usdt_symbol("ethusdt").unwrap(), "ETH/USDT");
        assert_eq!(direct_usdt_symbol("eth-usdt").unwrap(), "ETH/USDT");
        assert!(direct_usdt_symbol("ETH/BTC").is_err());
        assert!(direct_usdt_symbol("USDT").is_err());
        assert_eq!(timeframe_seconds("15m").unwrap(), 900);
        assert_eq!(timeframe_seconds("1D").unwrap(), 86_400);
        assert!(timeframe_seconds("15x").is_err());
        assert!(timeframe_seconds("0h").is_err());
    }
}

//! Multi-horizon market evidence and relative momentum (exact Python math). Purely
//! informational: nothing here gates a trade.

use std::collections::{BTreeMap, HashMap};

use bb_binance::history::HistoryFetcher;
use bb_binance::models::Ohlcv;
use bb_core::money::to_f64;
use bb_pool::MarketSnapshot;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tokio::sync::Mutex;

/// Hourly windows (name, candles back, weight).
pub const LONG_WINDOWS: [(&str, usize, f64); 4] = [
    ("1h", 1, 0.15),
    ("4h", 4, 0.20),
    ("24h", 24, 0.25),
    ("7d", 168, 0.15),
];
/// Minute windows (name, candles back, weight).
pub const SHORT_WINDOWS: [(&str, usize, f64); 2] = [("5m", 5, 0.10), ("15m", 15, 0.15)];
pub const SHORT_TIMEFRAME: &str = "1m";
pub const SHORT_LOOKBACK_MINUTES: i64 = 30;
/// Stables and major fiats: modelled as flat.
pub const LOW_UPSIDE_ASSETS: [&str; 14] = [
    "AUD", "BRL", "BUSD", "DAI", "EUR", "FDUSD", "GBP", "JPY", "TUSD", "TRY", "USDC", "USDP",
    "USD1", "USDT",
];
const HISTORY_QUOTES: [&str; 7] = ["USDT", "FDUSD", "USDC", "USD1", "BTC", "BNB", "ETH"];

pub fn is_low_upside(asset: &str) -> bool {
    LOW_UPSIDE_ASSETS.contains(&asset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceStatus {
    Ready,
    Insufficient,
    Unavailable,
}

/// One cleaned candle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candle {
    pub timestamp: i64,
    pub close: f64,
    pub volume: f64,
}

impl Candle {
    /// Rejects non-finite values, non-positive closes and negative volumes.
    pub fn from_ohlcv(row: &Ohlcv) -> Option<Self> {
        let close = to_f64(row.close);
        let volume = to_f64(row.volume);
        (close.is_finite() && volume.is_finite() && close > 0.0 && volume >= 0.0).then_some(Self {
            timestamp: row.timestamp,
            close,
            volume,
        })
    }
}

/// Cleaned, time-sorted candles.
pub fn clean_candles(rows: &[Ohlcv]) -> Vec<Candle> {
    let mut candles: Vec<Candle> = rows.iter().filter_map(Candle::from_ohlcv).collect();
    candles.sort_by_key(|candle| candle.timestamp);
    candles
}

/// Evidence packet for one asset.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssetEvidence {
    pub asset: String,
    pub symbol: String,
    pub status: EvidenceStatus,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candles: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returns_bps: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_returns_bps: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_status: Option<EvidenceStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_timeframe: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_candles: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hourly_volatility_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_24h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_volume_24h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_change_percent: Option<f64>,
}

impl AssetEvidence {
    pub fn unavailable(asset: &str, symbol: &str, reason: impl Into<String>) -> Self {
        Self {
            asset: asset.to_owned(),
            symbol: symbol.to_owned(),
            status: EvidenceStatus::Unavailable,
            reason: reason.into(),
            candles: None,
            returns_bps: None,
            short_returns_bps: None,
            short_status: None,
            short_reason: None,
            short_timeframe: None,
            short_candles: None,
            hourly_volatility_bps: None,
            volume_24h: None,
            previous_volume_24h: None,
            volume_change_percent: None,
        }
    }

    /// Flat evidence for stables / major fiats.
    pub fn stable(asset: &str) -> Self {
        let zeros = |windows: &[(&str, usize, f64)]| {
            windows
                .iter()
                .map(|(name, _, _)| ((*name).to_owned(), 0.0))
                .collect::<BTreeMap<_, _>>()
        };
        Self {
            status: EvidenceStatus::Ready,
            candles: Some(0),
            returns_bps: Some(zeros(&LONG_WINDOWS)),
            short_returns_bps: Some(zeros(&SHORT_WINDOWS)),
            short_status: Some(EvidenceStatus::Ready),
            short_timeframe: Some(SHORT_TIMEFRAME),
            short_candles: Some(0),
            hourly_volatility_bps: Some(0.0),
            ..Self::unavailable(
                asset,
                asset,
                "stable or low-upside benchmark asset; modeled as flat",
            )
        }
    }
}

/// `close[-1] / close[-(n+1)] - 1` in bps.
pub fn window_return_bps(candles: &[Candle], back: usize) -> Option<f64> {
    if candles.len() < back + 1 {
        return None;
    }
    let start = candles[candles.len() - back - 1].close;
    let end = candles[candles.len() - 1].close;
    (start > 0.0).then(|| (end / start - 1.0) * 10_000.0)
}

fn window_returns(candles: &[Candle], windows: &[(&str, usize, f64)]) -> BTreeMap<String, f64> {
    windows
        .iter()
        .filter_map(|(name, back, _)| {
            window_return_bps(candles, *back).map(|value| ((*name).to_owned(), value))
        })
        .collect()
}

fn population_std(values: &[f64]) -> f64 {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    (values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / n)
        .sqrt()
}

/// Python `_ohlcv_evidence`.
pub fn ohlcv_evidence(
    asset: &str,
    symbol: &str,
    hourly: &[Candle],
    minute: &[Candle],
    short_error: Option<&str>,
) -> AssetEvidence {
    let short_returns = window_returns(minute, &SHORT_WINDOWS);
    let (short_status, short_reason) = match short_error {
        Some(error) => (
            EvidenceStatus::Unavailable,
            format!("short history fetch failed: {error}"),
        ),
        None if short_returns.len() == SHORT_WINDOWS.len() => (
            EvidenceStatus::Ready,
            "compact minute evidence ready".to_owned(),
        ),
        None => (
            EvidenceStatus::Insufficient,
            "not enough minute candles for 5m/15m evidence".to_owned(),
        ),
    };
    let returns = window_returns(hourly, &LONG_WINDOWS);
    let mut evidence = AssetEvidence {
        status: EvidenceStatus::Insufficient,
        candles: Some(hourly.len()),
        short_returns_bps: Some(short_returns),
        short_status: Some(short_status),
        short_reason: Some(short_reason),
        short_timeframe: Some(SHORT_TIMEFRAME),
        short_candles: Some(minute.len()),
        ..AssetEvidence::unavailable(
            asset,
            symbol,
            "not enough hourly candles for compact evidence windows",
        )
    };
    if returns.len() != LONG_WINDOWS.len() {
        evidence.returns_bps = Some(returns);
        return evidence;
    }
    let hourly_returns: Vec<f64> = hourly
        .windows(2)
        .filter(|pair| pair[0].close > 0.0)
        .map(|pair| pair[1].close / pair[0].close - 1.0)
        .collect();
    let volume_24h = (hourly.len() >= 24).then(|| {
        hourly[hourly.len() - 24..]
            .iter()
            .map(|c| c.volume)
            .sum::<f64>()
    });
    let previous_volume_24h = (hourly.len() >= 48).then(|| {
        hourly[hourly.len() - 48..hourly.len() - 24]
            .iter()
            .map(|c| c.volume)
            .sum::<f64>()
    });
    let volume_change_percent = previous_volume_24h
        .filter(|previous| *previous > 0.0)
        .map(|previous| (volume_24h.unwrap_or(0.0) / previous - 1.0) * 100.0);
    evidence.status = EvidenceStatus::Ready;
    evidence.reason = "compact hourly evidence ready".into();
    evidence.returns_bps = Some(returns);
    evidence.hourly_volatility_bps = Some(if hourly_returns.len() > 1 {
        population_std(&hourly_returns) * 10_000.0
    } else {
        0.0
    });
    evidence.volume_24h = volume_24h;
    evidence.previous_volume_24h = previous_volume_24h;
    evidence.volume_change_percent = volume_change_percent;
    evidence
}

/// Weighted `(dest - source)` sums over `windows`. With `require_all`, any missing window
/// yields `None`; otherwise missing windows are skipped.
fn weighted_delta(
    source: Option<&BTreeMap<String, f64>>,
    destination: Option<&BTreeMap<String, f64>>,
    windows: &[(&str, usize, f64)],
    require_all: bool,
) -> Option<(f64, f64)> {
    let (Some(source), Some(destination)) = (source, destination) else {
        return if require_all { None } else { Some((0.0, 0.0)) };
    };
    let mut weighted = 0.0;
    let mut weights = 0.0;
    for (name, _, weight) in windows {
        let pair = source
            .get(*name)
            .copied()
            .filter(|v| v.is_finite())
            .zip(destination.get(*name).copied().filter(|v| v.is_finite()));
        match pair {
            Some((src, dst)) => {
                weighted += (dst - src) * weight;
                weights += weight;
            }
            None if require_all => return None,
            None => {}
        }
    }
    if require_all && weights <= 0.0 {
        return None;
    }
    Some((weighted, weights))
}

/// All long windows (required) plus whichever short windows exist, weight-normalised.
pub fn weighted_relative_momentum_bps(
    source: &AssetEvidence,
    destination: &AssetEvidence,
) -> Option<f64> {
    let (mut weighted, mut weights) = weighted_delta(
        source.returns_bps.as_ref(),
        destination.returns_bps.as_ref(),
        &LONG_WINDOWS,
        true,
    )?;
    if let Some((short_weighted, short_weights)) = weighted_delta(
        source.short_returns_bps.as_ref(),
        destination.short_returns_bps.as_ref(),
        &SHORT_WINDOWS,
        false,
    ) {
        weighted += short_weighted;
        weights += short_weights;
    }
    (weights > 0.0).then(|| weighted / weights)
}

/// Short windows only.
pub fn weighted_short_relative_momentum_bps(
    source: &AssetEvidence,
    destination: &AssetEvidence,
) -> Option<f64> {
    let (weighted, weights) = weighted_delta(
        source.short_returns_bps.as_ref(),
        destination.short_returns_bps.as_ref(),
        &SHORT_WINDOWS,
        false,
    )?;
    (weights > 0.0).then(|| weighted / weights)
}

/// Relative-momentum packet for one route (information for Claude, never a gate).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteEvidence {
    pub source: AssetEvidence,
    pub destination: AssetEvidence,
    pub route_friction_bps: f64,
    pub status: EvidenceStatus,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_momentum_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_relative_momentum_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_edge_after_friction_bps: Option<f64>,
}

/// Python `_route_market_evidence` without the min-edge gate fields.
pub fn route_evidence(
    immediate_friction_bps: Option<f64>,
    source: AssetEvidence,
    destination: AssetEvidence,
) -> RouteEvidence {
    let friction = immediate_friction_bps.unwrap_or(0.0).max(0.0);
    let mut packet = RouteEvidence {
        route_friction_bps: friction,
        status: EvidenceStatus::Insufficient,
        reason: "source or destination market evidence is not ready".into(),
        relative_momentum_bps: None,
        short_relative_momentum_bps: None,
        expected_edge_after_friction_bps: None,
        source,
        destination,
    };
    if packet.source.status != EvidenceStatus::Ready
        || packet.destination.status != EvidenceStatus::Ready
    {
        return packet;
    }
    let Some(momentum) = weighted_relative_momentum_bps(&packet.source, &packet.destination) else {
        packet.reason = "missing return windows for relative momentum".into();
        return packet;
    };
    packet.status = EvidenceStatus::Ready;
    packet.reason = "relative momentum minus immediate friction (information, not a rule)".into();
    packet.relative_momentum_bps = Some(momentum);
    packet.short_relative_momentum_bps =
        weighted_short_relative_momentum_bps(&packet.source, &packet.destination);
    packet.expected_edge_after_friction_bps = Some(momentum - friction);
    packet
}

/// First eligible `{asset}/{quote}` market among the history quotes, else any eligible
/// market with that base.
pub fn history_symbol(asset: &str, markets: &BTreeMap<String, MarketSnapshot>) -> Option<String> {
    let normalized = asset.trim().to_uppercase();
    HISTORY_QUOTES
        .iter()
        .filter(|quote| **quote != normalized)
        .map(|quote| format!("{normalized}/{quote}"))
        .find(|symbol| markets.get(symbol).is_some_and(|market| market.eligible))
        .or_else(|| {
            markets
                .values()
                .find(|market| {
                    market.eligible && market.base.trim().eq_ignore_ascii_case(&normalized)
                })
                .map(|market| market.symbol.clone())
        })
}

/// Fetches and caches per-asset evidence from public klines.
#[derive(Debug)]
pub struct EvidenceService {
    history: HistoryFetcher,
    lookback_hours: u32,
    max_symbols: usize,
    cache: Mutex<HashMap<String, AssetEvidence>>,
}

impl EvidenceService {
    pub fn new(history: HistoryFetcher, lookback_hours: u32, max_symbols: usize) -> Self {
        Self {
            history,
            lookback_hours,
            max_symbols,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Forget every cached packet (new cycle).
    pub async fn clear(&self) {
        self.cache.lock().await.clear();
    }

    /// Evidence for `asset`. With `enforce_cap`, assets beyond `max_symbols` fetched this
    /// cycle are reported unavailable (request-weight budget of the cycle prompt).
    pub async fn asset(
        &self,
        asset: &str,
        markets: &BTreeMap<String, MarketSnapshot>,
        enforce_cap: bool,
    ) -> AssetEvidence {
        let normalized = asset.trim().to_uppercase();
        {
            let cache = self.cache.lock().await;
            if let Some(hit) = cache.get(&normalized) {
                return hit.clone();
            }
            if enforce_cap && self.max_symbols > 0 && cache.len() >= self.max_symbols {
                return AssetEvidence::unavailable(
                    &normalized,
                    "",
                    "market evidence symbol cap reached",
                );
            }
        }
        let evidence = self.fetch(&normalized, markets, bb_core::time::now()).await;
        self.cache.lock().await.insert(normalized, evidence.clone());
        evidence
    }

    async fn fetch(
        &self,
        asset: &str,
        markets: &BTreeMap<String, MarketSnapshot>,
        now: DateTime<Utc>,
    ) -> AssetEvidence {
        if is_low_upside(asset) {
            return AssetEvidence::stable(asset);
        }
        let Some(symbol) = history_symbol(asset, markets) else {
            return AssetEvidence::unavailable(asset, "", "no eligible history symbol found");
        };
        let start = now - Duration::hours(i64::from(self.lookback_hours.max(168)) + 2);
        let hourly = match self
            .history
            .fetch_ohlcv(&symbol, "1h", start, Some(now))
            .await
        {
            Ok(rows) => clean_candles(&rows),
            Err(error) => {
                return AssetEvidence::unavailable(
                    asset,
                    &symbol,
                    format!("history fetch failed: {error}"),
                );
            }
        };
        let short_start = now - Duration::minutes(SHORT_LOOKBACK_MINUTES);
        let (minute, short_error) = match self
            .history
            .fetch_ohlcv(&symbol, SHORT_TIMEFRAME, short_start, Some(now))
            .await
        {
            Ok(rows) => (clean_candles(&rows), None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        ohlcv_evidence(asset, &symbol, &hourly, &minute, short_error.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candles(closes: &[f64]) -> Vec<Candle> {
        closes
            .iter()
            .enumerate()
            .map(|(index, close)| Candle {
                timestamp: index as i64,
                close: *close,
                volume: 10.0,
            })
            .collect()
    }

    fn returns(pairs: &[(&str, f64)]) -> Option<BTreeMap<String, f64>> {
        Some(pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect())
    }

    fn packet(long: &[(&str, f64)], short: &[(&str, f64)]) -> AssetEvidence {
        AssetEvidence {
            status: EvidenceStatus::Ready,
            returns_bps: returns(long),
            short_returns_bps: returns(short),
            ..AssetEvidence::unavailable("X", "X/USDT", "")
        }
    }

    #[test]
    fn window_returns_use_trailing_closes() {
        let series = candles(&[100.0, 110.0, 121.0]);
        assert!((window_return_bps(&series, 1).unwrap() - 1_000.0).abs() < 1e-9);
        assert!((window_return_bps(&series, 2).unwrap() - 2_100.0).abs() < 1e-9);
        assert_eq!(window_return_bps(&series, 3), None);
    }

    #[test]
    fn clean_candles_drops_bad_rows_and_sorts() {
        use rust_decimal::Decimal;
        let row = |ts: i64, close: i64, volume: i64| Ohlcv {
            timestamp: ts,
            open: Decimal::ONE,
            high: Decimal::ONE,
            low: Decimal::ONE,
            close: Decimal::from(close),
            volume: Decimal::from(volume),
        };
        let cleaned = clean_candles(&[row(3, 3, 1), row(1, 0, 1), row(2, 2, -1), row(0, 5, 0)]);
        assert_eq!(
            cleaned.iter().map(|c| c.timestamp).collect::<Vec<_>>(),
            vec![0, 3]
        );
    }

    #[test]
    fn compact_windows_volume_and_volatility() {
        let closes: Vec<f64> = (0..170).map(|i| 100.0 + i as f64).collect();
        let hourly = candles(&closes);
        let minute = candles(&(0..31).map(|i| 50.0 + f64::from(i)).collect::<Vec<_>>());
        let evidence = ohlcv_evidence("ETH", "ETH/USDT", &hourly, &minute, None);
        assert_eq!(evidence.status, EvidenceStatus::Ready);
        let long = evidence.returns_bps.as_ref().unwrap();
        assert_eq!(
            long.keys().cloned().collect::<Vec<_>>(),
            vec!["1h", "24h", "4h", "7d"]
        );
        assert!((long["1h"] - (269.0 / 268.0 - 1.0) * 10_000.0).abs() < 1e-9);
        assert!((long["7d"] - (269.0 / 101.0 - 1.0) * 10_000.0).abs() < 1e-9);
        assert_eq!(evidence.short_status, Some(EvidenceStatus::Ready));
        assert_eq!(evidence.short_candles, Some(31));
        assert_eq!(evidence.volume_24h, Some(240.0));
        assert_eq!(evidence.previous_volume_24h, Some(240.0));
        assert_eq!(evidence.volume_change_percent, Some(0.0));
        assert!(evidence.hourly_volatility_bps.unwrap() > 0.0);
    }

    #[test]
    fn insufficient_history_and_short_fetch_failure() {
        let evidence = ohlcv_evidence("ETH", "ETH/USDT", &candles(&[1.0, 2.0]), &[], Some("boom"));
        assert_eq!(evidence.status, EvidenceStatus::Insufficient);
        assert_eq!(evidence.short_status, Some(EvidenceStatus::Unavailable));
        assert_eq!(
            evidence.short_reason.as_deref(),
            Some("short history fetch failed: boom")
        );
        assert_eq!(evidence.returns_bps.unwrap().len(), 1);
    }

    #[test]
    fn weighted_relative_momentum_uses_short_tape() {
        // Python test_weighted_relative_momentum_uses_short_tape.
        let source = packet(
            &[("1h", 0.0), ("4h", 0.0), ("24h", 0.0), ("7d", 0.0)],
            &[("5m", -200.0), ("15m", -300.0)],
        );
        let destination = packet(
            &[("1h", 0.0), ("4h", 0.0), ("24h", 0.0), ("7d", 0.0)],
            &[("5m", 0.0), ("15m", 0.0)],
        );
        let short = weighted_short_relative_momentum_bps(&source, &destination).unwrap();
        assert!((short - 260.0).abs() < 1e-9, "{short}");
        let momentum = weighted_relative_momentum_bps(&source, &destination).unwrap();
        assert!((momentum - 65.0).abs() < 1e-9, "{momentum}");
    }

    #[test]
    fn route_evidence_is_information_without_gates() {
        let source = AssetEvidence::stable("USDT");
        let destination = packet(
            &[("1h", 10.0), ("4h", 20.0), ("24h", 30.0), ("7d", 40.0)],
            &[],
        );
        let packet = route_evidence(Some(12.0), source, destination);
        assert_eq!(packet.status, EvidenceStatus::Ready);
        let expected = (10.0 * 0.15 + 20.0 * 0.20 + 30.0 * 0.25 + 40.0 * 0.15) / 0.75;
        assert!((packet.relative_momentum_bps.unwrap() - expected).abs() < 1e-9);
        assert!(
            (packet.expected_edge_after_friction_bps.unwrap() - (expected - 12.0)).abs() < 1e-9
        );
        let json = serde_json::to_value(&packet).unwrap();
        assert!(json.get("gate_pass").is_none() && json.get("minimum_expected_edge_bps").is_none());

        let missing = route_evidence(
            Some(-5.0),
            AssetEvidence::unavailable("A", "", "x"),
            AssetEvidence::stable("USDT"),
        );
        assert_eq!(missing.status, EvidenceStatus::Insufficient);
        assert_eq!(missing.route_friction_bps, 0.0);
    }
}

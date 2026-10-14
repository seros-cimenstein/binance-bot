//! Public market state for tracked tokens (design doc "Engine" step 3).
//!
//! Network access sits behind [`MarketSource`] ([`LiveMarket`] in production:
//! public Binance Spot endpoints only, DexScreener public API). Turning raw
//! payloads into a [`MarketSnapshot`] is pure.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bb_binance::BinanceClient;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::resolve::{BinanceUniverse, is_stablecoin_pair};
use super::schema::Features;
use crate::dexscreener_api::DexScreenerApi;
use crate::jsonv::{Object, first_max_by, float_or_none, float_or_zero, nested, py_str};

/// DexScreener `tokens/{mints}` batch size.
pub const DEX_BATCH: usize = 30;
/// 1m klines fetched per Binance token (60 min reference + current minute).
pub const KLINE_LIMIT: u32 = 61;
/// Per-token market samples kept for Solana 15-min features.
pub const MARKET_HISTORY_MS: i64 = 20 * 60_000;
/// Tolerance when matching a history sample to a past instant.
const SAMPLE_TOLERANCE_MS: i64 = 120_000;
const MINUTE_MS: i64 = 60_000;

/// One 1m kline reduced to what the features need.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Candle {
    pub open_time: i64,
    pub open: f64,
    pub close: f64,
    pub quote_volume: f64,
}

/// Market data access (mockable in engine tests).
#[async_trait]
pub trait MarketSource: Send + Sync {
    /// Binance USDT listings plus exclusions (`exchangeInfo` + `ticker/24hr`).
    async fn binance_universe(&self, now_ms: i64) -> anyhow::Result<BinanceUniverse>;
    /// `ticker/price` for every symbol (one call): symbol -> price.
    async fn binance_prices(&self) -> anyhow::Result<BTreeMap<String, f64>>;
    /// Last `limit` 1m klines of `symbol` (oldest first).
    async fn binance_klines_1m(&self, symbol: &str, limit: u32) -> anyhow::Result<Vec<Candle>>;
    /// `bookTicker`: `(bid, ask)`.
    async fn binance_book(&self, symbol: &str) -> anyhow::Result<(f64, f64)>;
    /// `exchangeInfo` status per requested symbol (missing symbols omitted).
    async fn binance_symbol_status(
        &self,
        symbols: &[String],
    ) -> anyhow::Result<BTreeMap<String, String>>;
    /// DexScreener `latest/dex/tokens/{mints}` (at most [`DEX_BATCH`] mints).
    async fn dex_token_pairs(&self, mints: &[String]) -> anyhow::Result<Vec<Object>>;
    /// DexScreener `latest/dex/search?q=`.
    async fn dex_search(&self, query: &str) -> anyhow::Result<Vec<Object>>;
}

/// Production market source: public endpoints only, no credentials.
pub struct LiveMarket {
    binance: BinanceClient,
    dex: DexScreenerApi,
}

impl LiveMarket {
    pub fn new(binance: BinanceClient, dex: DexScreenerApi) -> Self {
        Self { binance, dex }
    }
}

fn dec(value: rust_decimal::Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

#[async_trait]
impl MarketSource for LiveMarket {
    async fn binance_universe(&self, now_ms: i64) -> anyhow::Result<BinanceUniverse> {
        let info = self.binance.exchange_info(None).await?;
        let tickers = self.binance.tickers_24h().await?;
        Ok(BinanceUniverse::from_market(&info, &tickers, now_ms))
    }

    async fn binance_prices(&self) -> anyhow::Result<BTreeMap<String, f64>> {
        Ok(self
            .binance
            .prices()
            .await?
            .into_iter()
            .map(|p| (p.symbol, dec(p.price)))
            .collect())
    }

    async fn binance_klines_1m(&self, symbol: &str, limit: u32) -> anyhow::Result<Vec<Candle>> {
        Ok(self
            .binance
            .klines(symbol, "1m", None, None, Some(limit))
            .await?
            .into_iter()
            .map(|k| Candle {
                open_time: k.open_time,
                open: dec(k.open),
                close: dec(k.close),
                quote_volume: k.quote_volume.map_or(0.0, dec),
            })
            .collect())
    }

    async fn binance_book(&self, symbol: &str) -> anyhow::Result<(f64, f64)> {
        let book = self.binance.book_ticker(symbol).await?;
        Ok((dec(book.bid_price), dec(book.ask_price)))
    }

    async fn binance_symbol_status(
        &self,
        symbols: &[String],
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let refs: Vec<&str> = symbols.iter().map(String::as_str).collect();
        let collect = |info: bb_binance::spot::ExchangeInfo| -> BTreeMap<String, String> {
            info.symbols
                .into_iter()
                .map(|s| (s.symbol, s.status))
                .collect()
        };
        match self.binance.exchange_info(Some(&refs)).await {
            Ok(info) => Ok(collect(info)),
            Err(batch_err) => {
                // One delisted symbol fails the whole batch; fall back per symbol.
                tracing::debug!(%batch_err, "exchangeInfo batch failed; retrying per symbol");
                let mut out = BTreeMap::new();
                for symbol in &refs {
                    if let Ok(info) = self.binance.exchange_info(Some(&[symbol])).await {
                        out.extend(collect(info));
                    }
                }
                Ok(out)
            }
        }
    }

    async fn dex_token_pairs(&self, mints: &[String]) -> anyhow::Result<Vec<Object>> {
        Ok(self.dex.token_pairs(&mints.join(",")).await?)
    }

    async fn dex_search(&self, query: &str) -> anyhow::Result<Vec<Object>> {
        Ok(self.dex.search_pairs(query).await?)
    }
}

/// One past market observation of a token (kept in engine state).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarketSample {
    pub ts_ms: i64,
    pub price_usd: Option<f64>,
    pub volume_5m_usd: Option<f64>,
}

/// Market state of one token at a cycle. Price changes are percentages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarketSnapshot {
    pub price_usd: Option<f64>,
    /// Price 60 min before (Binance: open of the 1m kline 60 min ago; Solana:
    /// `price / (1 + priceChange.h1 / 100)`).
    pub price_ref_1h_usd: Option<f64>,
    pub price_change_5m: Option<f64>,
    pub price_change_15m: Option<f64>,
    pub price_change_1h: Option<f64>,
    pub volume_5m_usd: Option<f64>,
    pub volume_1h_usd: Option<f64>,
    /// Mean 5m volume over the last 15 min (exhaustion `price_vertical_volume_weak`).
    pub volume_5m_mean_15m_usd: Option<f64>,
    pub liquidity_usd: Option<f64>,
    pub fdv_usd: Option<f64>,
    pub pair_age_minutes: Option<f64>,
    pub spread_bps: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub symbol_status: Option<String>,
    pub pair_address: Option<String>,
    /// Solana pair listed / Binance price present.
    pub pair_exists: bool,
    /// Solana pair matches the stablecoin rule (excluded from scoring).
    pub stablecoin: bool,
}

impl MarketSnapshot {
    /// Copy the market fields into `features`.
    pub fn fill_features(&self, features: &mut Features) {
        features.price_usd = self.price_usd;
        features.price_change_5m = self.price_change_5m;
        features.price_change_15m = self.price_change_15m;
        features.price_change_1h = self.price_change_1h;
        features.volume_5m_usd = self.volume_5m_usd;
        features.volume_1h_usd = self.volume_1h_usd;
        features.liquidity_usd = self.liquidity_usd;
        features.fdv_usd = self.fdv_usd;
        features.pair_age_minutes = self.pair_age_minutes;
        features.spread_bps = self.spread_bps;
    }

    /// The `XSignal.market` / Haiku market map (fixed keys, null when unknown).
    pub fn to_map(&self) -> BTreeMap<String, Value> {
        let entries: [(&str, Value); 13] = [
            ("price_usd", json!(self.price_usd)),
            ("price_ref_1h_usd", json!(self.price_ref_1h_usd)),
            ("liquidity_usd", json!(self.liquidity_usd)),
            ("volume_5m_usd", json!(self.volume_5m_usd)),
            ("volume_1h_usd", json!(self.volume_1h_usd)),
            ("fdv_usd", json!(self.fdv_usd)),
            ("pair_age_minutes", json!(self.pair_age_minutes)),
            ("spread_bps", json!(self.spread_bps)),
            ("bid", json!(self.bid)),
            ("ask", json!(self.ask)),
            ("symbol_status", json!(self.symbol_status)),
            ("price_change_5m", json!(self.price_change_5m)),
            ("price_change_1h", json!(self.price_change_1h)),
        ];
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    }

    pub fn sample(&self, now_ms: i64) -> MarketSample {
        MarketSample {
            ts_ms: now_ms,
            price_usd: self.price_usd,
            volume_5m_usd: self.volume_5m_usd,
        }
    }
}

fn pct_change(price: Option<f64>, reference: Option<f64>) -> Option<f64> {
    match (price, reference) {
        (Some(p), Some(r)) if r > 0.0 => Some((p / r - 1.0) * 100.0),
        _ => None,
    }
}

fn positive(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Binance snapshot from `ticker/price`, 1m klines and `bookTicker`. Klines are
/// optional (fetched only for focus/hot tokens and open signals).
pub fn binance_snapshot(
    price: Option<f64>,
    klines: Option<&[Candle]>,
    book: Option<(f64, f64)>,
    symbol_status: Option<String>,
    now_ms: i64,
) -> MarketSnapshot {
    let price_usd = price.and_then(positive);
    let mut snap = MarketSnapshot {
        price_usd,
        pair_exists: price_usd.is_some() && symbol_status.as_deref().is_none_or(|s| s == "TRADING"),
        symbol_status,
        ..MarketSnapshot::default()
    };
    if let Some(klines) = klines.filter(|k| !k.is_empty()) {
        let current_minute = now_ms.div_euclid(MINUTE_MS) * MINUTE_MS;
        // Price N minutes ago = open of the 1m kline starting N minutes before
        // the current one (latest kline at or before that instant).
        let open_at = |minutes: i64| -> Option<f64> {
            let target = current_minute - minutes * MINUTE_MS;
            klines
                .iter()
                .filter(|k| k.open_time <= target)
                .max_by_key(|k| k.open_time)
                .filter(|k| target - k.open_time < 5 * MINUTE_MS)
                .and_then(|k| positive(k.open))
        };
        let volume_since = |minutes: i64| -> f64 {
            let cutoff = current_minute - minutes * MINUTE_MS;
            klines
                .iter()
                .filter(|k| k.open_time > cutoff && k.open_time <= current_minute)
                .map(|k| k.quote_volume)
                .sum()
        };
        let last_close = klines
            .iter()
            .max_by_key(|k| k.open_time)
            .and_then(|k| positive(k.close));
        let now_price = price_usd.or(last_close);
        snap.price_ref_1h_usd = open_at(60);
        snap.price_change_5m = pct_change(now_price, open_at(5));
        snap.price_change_15m = pct_change(now_price, open_at(15));
        snap.price_change_1h = pct_change(now_price, snap.price_ref_1h_usd);
        snap.volume_5m_usd = Some(volume_since(5));
        snap.volume_1h_usd = Some(volume_since(60));
        snap.volume_5m_mean_15m_usd = Some(volume_since(15) / 3.0);
    }
    if let Some((bid, ask)) = book
        && bid > 0.0
        && ask >= bid
    {
        snap.bid = Some(bid);
        snap.ask = Some(ask);
        snap.spread_bps = Some((ask - bid) / f64::midpoint(bid, ask) * 10_000.0);
    }
    snap
}

/// History sample closest to `target` within the tolerance.
fn sample_near(history: &[MarketSample], target: i64) -> Option<&MarketSample> {
    history
        .iter()
        .filter(|s| (s.ts_ms - target).abs() <= SAMPLE_TOLERANCE_MS)
        .min_by_key(|s| (s.ts_ms - target).abs())
}

/// Solana snapshot from the DexScreener pairs of one mint. `preferred_pair` is
/// used when still listed, else the most liquid Solana pair. `history` holds the
/// token's earlier samples (strictly before `now_ms`).
pub fn solana_snapshot(
    pairs: &[&Object],
    preferred_pair: Option<&str>,
    history: &[MarketSample],
    now_ms: i64,
) -> MarketSnapshot {
    let solana: Vec<&Object> = pairs
        .iter()
        .copied()
        .filter(|p| py_str(p.get("chainId")).eq_ignore_ascii_case("solana"))
        .collect();
    let pair = preferred_pair
        .and_then(|address| {
            solana
                .iter()
                .copied()
                .find(|p| py_str(p.get("pairAddress")) == address)
        })
        .or_else(|| {
            first_max_by(solana.iter().copied(), |p| {
                float_or_zero(nested(p, "liquidity", "usd"))
            })
        });
    let Some(pair) = pair else {
        return MarketSnapshot::default();
    };
    let price_usd = float_or_none(pair.get("priceUsd")).and_then(positive);
    let change_h1 = float_or_none(nested(pair, "priceChange", "h1"));
    let volume_5m_usd = float_or_none(nested(pair, "volume", "m5"));
    let price_ref_1h_usd = match (price_usd, change_h1) {
        (Some(p), Some(h1)) if h1 > -100.0 => Some(p / (1.0 + h1 / 100.0)),
        _ => None,
    };
    let price_change_15m = pct_change(
        price_usd,
        sample_near(history, now_ms - 15 * MINUTE_MS).and_then(|s| s.price_usd),
    );
    let volume_5m_mean_15m_usd = volume_5m_usd.and_then(|current| {
        let v5 = sample_near(history, now_ms - 5 * MINUTE_MS)?.volume_5m_usd?;
        let v10 = sample_near(history, now_ms - 10 * MINUTE_MS)?.volume_5m_usd?;
        Some((current + v5 + v10) / 3.0)
    });
    let pair_age_minutes = float_or_none(pair.get("pairCreatedAt"))
        .filter(|created| *created > 0.0)
        // Epoch ms fit losslessly in f64 for any realistic date.
        .map(|created| ((now_ms as f64 - created) / 60_000.0).max(0.0));
    let pair_address = py_str(pair.get("pairAddress"));
    MarketSnapshot {
        price_usd,
        price_ref_1h_usd,
        price_change_5m: float_or_none(nested(pair, "priceChange", "m5")),
        price_change_15m,
        price_change_1h: change_h1,
        volume_5m_usd,
        volume_1h_usd: float_or_none(nested(pair, "volume", "h1")),
        volume_5m_mean_15m_usd,
        liquidity_usd: float_or_none(nested(pair, "liquidity", "usd")),
        fdv_usd: float_or_none(pair.get("fdv")),
        pair_age_minutes,
        pair_address: (!pair_address.is_empty()).then_some(pair_address),
        pair_exists: true,
        stablecoin: is_stablecoin_pair(pair),
        ..MarketSnapshot::default()
    }
}

/// Keep samples from the last [`MARKET_HISTORY_MS`] and append the new one.
pub fn push_sample(history: &mut Vec<MarketSample>, sample: MarketSample) {
    history.retain(|s| sample.ts_ms - s.ts_ms <= MARKET_HISTORY_MS && s.ts_ms < sample.ts_ms);
    history.push(sample);
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_900_030_000; // 30s into a minute

    fn klines() -> Vec<Candle> {
        let current = NOW.div_euclid(MINUTE_MS) * MINUTE_MS;
        (0..61)
            .rev()
            .map(|age| Candle {
                open_time: current - age * MINUTE_MS,
                open: 1.0 + (60 - age) as f64 * 0.01,
                close: 1.0 + (61 - age) as f64 * 0.01,
                quote_volume: if age < 5 { 100.0 } else { 10.0 },
            })
            .collect()
    }

    #[test]
    fn binance_snapshot_uses_kline_60_min_before_and_book_spread() {
        let k = klines();
        let snap = binance_snapshot(
            Some(2.0),
            Some(&k),
            Some((1.99, 2.01)),
            Some("TRADING".into()),
            NOW,
        );
        assert_eq!(snap.price_ref_1h_usd, Some(1.0));
        assert!((snap.price_change_1h.unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(snap.volume_5m_usd, Some(500.0));
        assert_eq!(snap.volume_1h_usd, Some(500.0 + 55.0 * 10.0));
        assert!((snap.volume_5m_mean_15m_usd.unwrap() - (500.0 + 100.0) / 3.0).abs() < 1e-9);
        assert!((snap.spread_bps.unwrap() - 100.0).abs() < 1e-9);
        assert!(snap.pair_exists);
        let gone = binance_snapshot(Some(2.0), None, None, Some("BREAK".into()), NOW);
        assert!(!gone.pair_exists);
    }

    #[test]
    fn solana_snapshot_reads_real_shaped_pair_and_history() {
        let pair = json!({
            "chainId": "solana", "dexId": "raydium", "pairAddress": "PAIR1",
            "baseToken": {"address": "MINT", "name": "Pepe", "symbol": "PEPE"},
            "priceUsd": "0.0130", "priceChange": {"m5": 5.5, "h1": 30.0, "h6": 80, "h24": 120},
            "volume": {"m5": 900.0, "h1": 6000.0, "h6": 9000, "h24": 20000},
            "liquidity": {"usd": 25000.5, "base": 1, "quote": 2}, "fdv": 130000,
            "pairCreatedAt": NOW - 90 * 60_000
        });
        let obj = pair.as_object().unwrap();
        let history = vec![
            MarketSample {
                ts_ms: NOW - 15 * MINUTE_MS + 30_000,
                price_usd: Some(0.01),
                volume_5m_usd: Some(300.0),
            },
            MarketSample {
                ts_ms: NOW - 10 * MINUTE_MS,
                price_usd: Some(0.011),
                volume_5m_usd: Some(600.0),
            },
            MarketSample {
                ts_ms: NOW - 5 * MINUTE_MS,
                price_usd: Some(0.012),
                volume_5m_usd: Some(1500.0),
            },
        ];
        let snap = solana_snapshot(&[obj], None, &history, NOW);
        assert!((snap.price_change_15m.unwrap() - 30.0).abs() < 1e-6);
        assert!((snap.price_ref_1h_usd.unwrap() - 0.01).abs() < 1e-12);
        assert_eq!(snap.volume_5m_mean_15m_usd, Some(1000.0));
        assert_eq!(snap.pair_age_minutes, Some(90.0));
        assert_eq!(snap.liquidity_usd, Some(25000.5));
        assert_eq!(snap.pair_address.as_deref(), Some("PAIR1"));
        assert!(snap.pair_exists && !snap.stablecoin);
        assert!(!solana_snapshot(&[], None, &history, NOW).pair_exists);
    }
}

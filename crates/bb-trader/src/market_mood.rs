//! Market mood: how many of Binance's most-traded USDT crypto markets are rising or falling
//! over 15m, 1h, 4h and 24h, folded into one bearish (-100) to bullish (+100) score, plus the
//! same score over the last 24 hours. Public market data only (`ticker/24hr`,
//! `exchangeInfo`, `klines`).
//!
//! Shared by the trader (cycle prompt `## Market breadth`) and the dashboard (`cryptobot-gui`
//! Overview `/api/market`).

use std::collections::HashMap;

use bb_binance::BinanceClient;
use bb_binance::fiat::is_fiat;
use bb_binance::models::Kline;
use bb_binance::spot::{SpotSymbolInfo, Ticker24h};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;

/// Coins in the basket: the most-traded USDT crypto markets by 24h quote volume.
pub const BASKET_SIZE: usize = 20;
/// The permission group Binance puts its tokenized stocks and ETFs in (AAPLB, NVDAB, SOXLB,
/// …); they are left out of a crypto mood. Observed in `exchangeInfo` on 2026-10-17, not
/// documented: if Binance renames it, those markets simply join the basket again.
pub const TOKENIZED_STOCK_GROUP: &str = "TRD_GRP_261";
/// Top-volume pairs checked in `exchangeInfo` to fill the basket after leaving stocks out.
const CANDIDATES: usize = 3 * BASKET_SIZE;
/// Moves smaller than this (in percent) count as flat, so noise doesn't tip the breadth.
pub const FLAT_BAND_PCT: f64 = 0.1;
/// Score at or beyond which the mood reads bullish (or, negated, bearish).
pub const LEAN_SCORE: f64 = 15.0;
/// Score at or beyond which the mood reads strongly bullish (or strongly bearish).
pub const STRONG_SCORE: f64 = 50.0;
/// Stablecoin rule: last price within 2% of 1.0 and 24h range under 2%.
pub const STABLECOIN_TOLERANCE: f64 = 0.02;

const QUOTE: &str = "USDT";
const INTERVAL: &str = "15m";
const BAR_MS: i64 = 15 * 60 * 1000;
/// 48h of 15m bars: 24h of score history, each point with its own 24h lookback.
const KLINE_LIMIT: u32 = 193;
/// Points in the score history (24h of 15m bars, the newest one still forming).
const HISTORY_POINTS: usize = 97;

/// `(name, 15m bars back, weight)`; the weights sum to 1.
pub const HORIZONS: [(&str, i64, f64); 4] = [
    ("15m", 1, 0.15),
    ("1h", 4, 0.25),
    ("4h", 16, 0.30),
    ("24h", 96, 0.30),
];

/// Stablecoin rule: last price within 2% of 1.0 and 24h `(high, low)` range under 2%.
pub fn is_stablecoin(last_price: f64, range: Option<(f64, f64)>) -> bool {
    if (last_price - 1.0).abs() > STABLECOIN_TOLERANCE {
        return false;
    }
    match range {
        Some((high, low)) if low > 0.0 => high / low - 1.0 < STABLECOIN_TOLERANCE,
        _ => false,
    }
}

/// Breadth over one horizon.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HorizonBreadth {
    pub name: &'static str,
    pub weight: f64,
    pub up: usize,
    pub down: usize,
    pub flat: usize,
    /// `(up - down) / coins`, -1 to 1.
    pub breadth: f64,
    pub median_return_pct: Option<f64>,
}

/// One coin's returns, newest bar against each horizon's lookback.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoinMove {
    pub asset: String,
    pub returns_pct: Vec<Option<f64>>,
    /// 24h quote volume from the `ticker/24hr` read that ranked the basket (`None` when
    /// built from candles alone).
    pub quote_volume_24h: Option<f64>,
}

/// The score at one 15m bar close (the newest bar is still forming).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MoodPoint {
    pub t: i64,
    pub score: f64,
}

/// Everything the Overview's market card and the trader's breadth section need.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MarketView {
    pub fetched_at: String,
    pub error: Option<String>,
    pub score: Option<f64>,
    pub label: Option<&'static str>,
    pub horizons: Vec<HorizonBreadth>,
    /// Oldest first.
    pub history: Vec<MoodPoint>,
    /// In basket order (largest 24h quote volume first).
    pub coins: Vec<CoinMove>,
    pub flat_band_pct: f64,
    /// Coins whose candles could not be read (left out of the score).
    pub missing: Vec<String>,
}

impl MarketView {
    fn failed(error: String) -> Self {
        Self {
            fetched_at: bb_core::time::now().to_rfc3339(),
            error: Some(error),
            flat_band_pct: FLAT_BAND_PCT,
            ..Self::default()
        }
    }

    /// Top `n` coins by return at `horizon_index` (`0 = 15m`, `1 = 1h`, `2 = 4h`, `3 = 24h`),
    /// highest first when `best` is true, lowest first when false.
    pub fn ranked_at(&self, horizon_index: usize, best: bool, n: usize) -> Vec<(&str, f64)> {
        let mut rows: Vec<(&str, f64)> = self
            .coins
            .iter()
            .filter_map(|c| {
                let ret = (*c.returns_pct.get(horizon_index)?)?;
                Some((c.asset.as_str(), ret))
            })
            .collect();
        rows.sort_by(|a, b| {
            if best {
                b.1.total_cmp(&a.1)
            } else {
                a.1.total_cmp(&b.1)
            }
            .then_with(|| a.0.cmp(b.0))
        });
        rows.truncate(n);
        rows
    }
}

/// Mood name for a score.
pub fn label(score: f64) -> &'static str {
    if score >= STRONG_SCORE {
        "Strongly bullish"
    } else if score >= LEAN_SCORE {
        "Bullish"
    } else if score > -LEAN_SCORE {
        "Neutral"
    } else if score > -STRONG_SCORE {
        "Bearish"
    } else {
        "Strongly bearish"
    }
}

fn float(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

/// USDT pairs that traded in the last 24h, largest 24h quote volume first (base assets),
/// leaving out fiat and stablecoins (price within 2% of 1 and a 24h range under 2%).
pub fn rank_by_volume(tickers: &[Ticker24h]) -> Vec<String> {
    let mut rows: Vec<(&str, f64)> = tickers
        .iter()
        .filter(|t| t.count > 0 && t.quote_volume > Decimal::ZERO)
        .filter_map(|t| {
            let base = t.symbol.strip_suffix(QUOTE)?;
            let stable = is_stablecoin(
                float(t.last_price),
                Some((float(t.high_price), float(t.low_price))),
            );
            (!base.is_empty() && !is_fiat(base) && !stable).then(|| (base, float(t.quote_volume)))
        })
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    rows.into_iter().map(|(base, _)| base.to_owned()).collect()
}

/// A trading crypto market: status `TRADING` and not one of Binance's tokenized stocks/ETFs.
pub fn is_crypto_market(info: &SpotSymbolInfo) -> bool {
    info.status == "TRADING"
        && !info
            .permission_sets
            .iter()
            .flatten()
            .any(|group| group == TOKENIZED_STOCK_GROUP)
}

/// The first `n` of `ranked` whose `<base>USDT` market is a trading crypto market.
pub fn pick_basket(ranked: &[String], markets: &[SpotSymbolInfo], n: usize) -> Vec<String> {
    ranked
        .iter()
        .filter(|base| {
            markets
                .iter()
                .any(|m| m.base_asset == **base && m.quote_asset == QUOTE && is_crypto_market(m))
        })
        .take(n)
        .cloned()
        .collect()
}

fn percent_change(now: f64, then: f64) -> Option<f64> {
    (then > 0.0 && now > 0.0).then(|| (now / then - 1.0) * 100.0)
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    })
}

/// Breadth of every horizon at bar open time `t`, from each coin's closes by open time.
fn breadth_at(closes: &[HashMap<i64, f64>], t: i64) -> Vec<HorizonBreadth> {
    HORIZONS
        .iter()
        .map(|&(name, bars, weight)| {
            let mut returns: Vec<f64> = closes
                .iter()
                .filter_map(|c| percent_change(*c.get(&t)?, *c.get(&(t - bars * BAR_MS))?))
                .collect();
            let up = returns.iter().filter(|r| **r >= FLAT_BAND_PCT).count();
            let down = returns.iter().filter(|r| **r <= -FLAT_BAND_PCT).count();
            let coins = returns.len();
            HorizonBreadth {
                name,
                weight,
                up,
                down,
                flat: coins - up - down,
                breadth: if coins == 0 {
                    0.0
                } else {
                    (up as f64 - down as f64) / coins as f64
                },
                median_return_pct: median(&mut returns),
            }
        })
        .collect()
}

/// Weighted breadth in -100..=100 (one decimal); `None` until every horizon has a coin.
fn score(horizons: &[HorizonBreadth]) -> Option<f64> {
    horizons
        .iter()
        .all(|h| h.up + h.down + h.flat > 0)
        .then(|| {
            let score = horizons.iter().map(|h| h.weight * h.breadth).sum::<f64>() * 100.0;
            (score * 10.0).round() / 10.0
        })
}

/// Every horizon's breadth and the weighted score at 15m bar open time `t`, from each
/// coin's 15m closes keyed by open time: the same numbers [`mood`] reports for its newest
/// bar, at any time. The Phase 48 persona backtest scores past times with it.
pub fn breadth_score_at(
    closes: &[HashMap<i64, f64>],
    t: i64,
) -> (Vec<HorizonBreadth>, Option<f64>) {
    let horizons = breadth_at(closes, t);
    let score = score(&horizons);
    (horizons, score)
}

/// The mood from each basket coin's 15m candles (oldest first). The newest open time across
/// the basket is "now"; coins without a candle at a time are left out of that time.
pub fn mood(series: &[(String, Vec<Kline>)], fetched_at: String) -> MarketView {
    let closes: Vec<HashMap<i64, f64>> = series
        .iter()
        .map(|(_, klines)| {
            klines
                .iter()
                .map(|k| (k.open_time, float(k.close)))
                .collect()
        })
        .collect();
    let Some(now) = series
        .iter()
        .filter_map(|(_, klines)| klines.last().map(|k| k.open_time))
        .max()
    else {
        return MarketView::failed("no candles for any basket coin".into());
    };
    let horizons = breadth_at(&closes, now);
    let history = (0..HISTORY_POINTS as i64)
        .rev()
        .map(|back| now - back * BAR_MS)
        .filter_map(|t| score(&breadth_at(&closes, t)).map(|score| MoodPoint { t, score }))
        .collect();
    let coins = series
        .iter()
        .zip(&closes)
        .map(|((asset, _), c)| CoinMove {
            asset: asset.clone(),
            returns_pct: HORIZONS
                .iter()
                .map(|&(_, bars, _)| percent_change(*c.get(&now)?, *c.get(&(now - bars * BAR_MS))?))
                .collect(),
            quote_volume_24h: None,
        })
        .collect();
    let score = score(&horizons);
    MarketView {
        fetched_at,
        error: None,
        score,
        label: score.map(label),
        horizons,
        history,
        coins,
        flat_band_pct: FLAT_BAND_PCT,
        missing: Vec::new(),
    }
}

/// Read the basket and its candles (public GET endpoints) and score them.
pub async fn fetch(client: Option<&BinanceClient>) -> MarketView {
    let Some(client) = client else {
        return MarketView::failed("no Binance client".into());
    };
    let tickers = match client.tickers_24h().await {
        Ok(tickers) => tickers,
        Err(error) => return MarketView::failed(format!("24h tickers unavailable: {error}")),
    };
    let mut ranked = rank_by_volume(&tickers);
    ranked.truncate(CANDIDATES);
    let symbols: Vec<String> = ranked
        .iter()
        .map(|base| format!("{base}/{QUOTE}"))
        .collect();
    let symbols: Vec<&str> = symbols.iter().map(String::as_str).collect();
    let markets = match client.exchange_info(Some(&symbols)).await {
        Ok(info) => info.symbols,
        Err(error) => return MarketView::failed(format!("exchange info unavailable: {error}")),
    };
    let basket = pick_basket(&ranked, &markets, BASKET_SIZE);
    let reads = futures::future::join_all(basket.iter().map(|asset| {
        let symbol = format!("{asset}/{QUOTE}");
        async move {
            client
                .klines(&symbol, INTERVAL, None, None, Some(KLINE_LIMIT))
                .await
        }
    }))
    .await;
    let mut series = Vec::with_capacity(basket.len());
    let mut missing = Vec::new();
    for (asset, read) in basket.into_iter().zip(reads) {
        match read {
            Ok(klines) if !klines.is_empty() => series.push((asset, klines)),
            Ok(_) => missing.push(asset),
            Err(error) => {
                tracing::warn!(%error, %asset, "market mood: klines unavailable");
                missing.push(asset);
            }
        }
    }
    let mut view = mood(&series, bb_core::time::now().to_rfc3339());
    view.missing = missing;
    for coin in &mut view.coins {
        let symbol = format!("{}{QUOTE}", coin.asset);
        coin.quote_volume_24h = tickers
            .iter()
            .find(|t| t.symbol == symbol)
            .map(|t| float(t.quote_volume));
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticker(symbol: &str, last: f64, high: f64, low: f64, quote_volume: i64) -> Ticker24h {
        let d = |v: f64| Decimal::try_from(v).unwrap();
        Ticker24h {
            symbol: symbol.into(),
            last_price: d(last),
            high_price: d(high),
            low_price: d(low),
            quote_volume: Decimal::from(quote_volume),
            count: 10,
            ..Ticker24h::default()
        }
    }

    /// Closes on consecutive 15m bars, the last one at open time `end`.
    fn candles(closes: &[f64], end: i64) -> Vec<Kline> {
        let start = end - (closes.len() as i64 - 1) * BAR_MS;
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let price = Decimal::try_from(c).unwrap();
                Kline {
                    open_time: start + i as i64 * BAR_MS,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume: Decimal::ONE,
                    close_time: None,
                    quote_volume: None,
                    trades: None,
                    taker_buy_base_volume: None,
                    taker_buy_quote_volume: None,
                }
            })
            .collect()
    }

    /// 193 closes rising (or falling) by `step` per bar from 100.
    fn trend(step: f64) -> Vec<f64> {
        (0..193).map(|i| 100.0 + step * f64::from(i)).collect()
    }

    const END: i64 = 1_800_000_000_000 - 1_800_000_000_000 % BAR_MS;

    fn market(base: &str, status: &str, groups: &[&str]) -> SpotSymbolInfo {
        SpotSymbolInfo {
            symbol: format!("{base}USDT"),
            status: status.into(),
            base_asset: base.into(),
            quote_asset: "USDT".into(),
            permission_sets: vec![groups.iter().map(|g| (*g).to_owned()).collect()],
            ..SpotSymbolInfo::default()
        }
    }

    #[test]
    fn ranking_is_by_volume_without_fiat_stables_idle_or_other_quotes() {
        let mut idle = ticker("DEADUSDT", 1.5, 1.6, 1.4, 10_000);
        idle.count = 0;
        let tickers = vec![
            ticker("BTCUSDT", 60_000.0, 61_000.0, 59_000.0, 900),
            ticker("USDCUSDT", 1.0, 1.001, 0.999, 2_000),
            ticker("EURUSDT", 1.08, 1.09, 1.07, 800),
            ticker("ETHBTC", 0.05, 0.051, 0.049, 5_000),
            ticker("SOLUSDT", 150.0, 155.0, 145.0, 700),
            ticker("DOGEUSDT", 0.2, 0.21, 0.19, 950),
            idle,
        ];
        assert_eq!(rank_by_volume(&tickers), vec!["DOGE", "BTC", "SOL"]);
    }

    #[test]
    fn a_coin_near_one_dollar_that_moves_is_not_a_stablecoin() {
        let tickers = vec![ticker("ADAUSDT", 1.0, 1.08, 0.95, 100)];
        assert_eq!(rank_by_volume(&tickers), vec!["ADA"]);
    }

    #[test]
    fn basket_skips_tokenized_stocks_halted_and_unknown_markets() {
        let ranked: Vec<String> = ["NVDAB", "BTC", "HALT", "GONE", "ETH", "SOL"]
            .map(String::from)
            .to_vec();
        let markets = vec![
            market("NVDAB", "TRADING", &["SPOT", TOKENIZED_STOCK_GROUP]),
            market("BTC", "TRADING", &["SPOT", "TRD_GRP_015"]),
            market("HALT", "BREAK", &["SPOT"]),
            market("ETH", "TRADING", &["SPOT"]),
            market("SOL", "TRADING", &["SPOT"]),
        ];
        assert_eq!(pick_basket(&ranked, &markets, 2), vec!["BTC", "ETH"]);
        assert_eq!(pick_basket(&ranked, &markets, 9), vec!["BTC", "ETH", "SOL"]);
    }

    #[test]
    fn all_rising_is_plus_100_and_all_falling_is_minus_100() {
        let up = vec![
            ("A".into(), candles(&trend(0.5), END)),
            ("B".into(), candles(&trend(0.3), END)),
        ];
        let view = mood(&up, String::new());
        assert_eq!(view.score, Some(100.0));
        assert_eq!(view.label, Some("Strongly bullish"));
        assert!(view.horizons.iter().all(|h| h.up == 2 && h.down == 0));

        let down = vec![("A".into(), candles(&trend(-0.2), END))];
        let view = mood(&down, String::new());
        assert_eq!(view.score, Some(-100.0));
        assert_eq!(view.label, Some("Strongly bearish"));
    }

    #[test]
    fn moves_inside_the_flat_band_do_not_count() {
        // +0.05% per bar: the 15m move (0.05%) is flat, 1h (~0.2%) and longer are up.
        let series = vec![("A".into(), candles(&trend(0.05), END))];
        let view = mood(&series, String::new());
        assert_eq!(view.horizons[0].flat, 1);
        assert_eq!(view.horizons[0].breadth, 0.0);
        assert!(view.horizons[1..].iter().all(|h| h.up == 1));
        assert_eq!(view.score, Some(85.0));
    }

    #[test]
    fn short_dip_inside_a_long_rally_weighs_each_horizon() {
        // Rising for 47h, then down 2% over the last hour: 15m and 1h bearish, 4h and 24h bullish.
        let mut closes = trend(0.5);
        let n = closes.len();
        let peak = closes[n - 5];
        for (k, close) in closes[n - 4..].iter_mut().enumerate() {
            *close = peak * (1.0 - 0.005 * (k as f64 + 1.0));
        }
        let view = mood(&[("A".into(), candles(&closes, END))], String::new());
        let breadth: Vec<f64> = view.horizons.iter().map(|h| h.breadth).collect();
        assert_eq!(breadth, vec![-1.0, -1.0, 1.0, 1.0]);
        assert!((view.score.unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(view.label, Some("Bullish"));
    }

    #[test]
    fn history_covers_24h_and_ends_now() {
        let series = vec![("A".into(), candles(&trend(0.5), END))];
        let view = mood(&series, String::new());
        assert_eq!(view.history.len(), HISTORY_POINTS);
        assert_eq!(view.history.last().unwrap().t, END);
        assert_eq!(view.history[0].t, END - 96 * BAR_MS);
    }

    #[test]
    fn coin_without_a_lookback_candle_is_left_out_of_that_horizon() {
        // B was listed 2h ago: it counts for 15m and 1h only.
        let series = vec![
            ("A".into(), candles(&trend(0.5), END)),
            (
                "B".into(),
                candles(
                    &[100.0, 99.0, 98.0, 97.0, 96.0, 95.0, 94.0, 93.0, 92.0],
                    END,
                ),
            ),
        ];
        let view = mood(&series, String::new());
        let coins: Vec<usize> = view
            .horizons
            .iter()
            .map(|h| h.up + h.down + h.flat)
            .collect();
        assert_eq!(coins, vec![2, 2, 1, 1]);
        assert_eq!(view.coins[1].returns_pct[2], None);
    }

    #[test]
    fn breadth_score_at_matches_mood_now_and_scores_past_times() {
        // A dip at the end of a rally, plus a coin listed 2h ago.
        let mut dip = trend(0.5);
        let n = dip.len();
        let peak = dip[n - 5];
        for (k, close) in dip[n - 4..].iter_mut().enumerate() {
            *close = peak * (1.0 - 0.005 * (k as f64 + 1.0));
        }
        let series = vec![
            ("A".into(), candles(&dip, END)),
            ("B".into(), candles(&trend(-0.2)[184..], END)),
        ];
        let view = mood(&series, String::new());
        let closes: Vec<HashMap<i64, f64>> = series
            .iter()
            .map(|(_, k)| k.iter().map(|k| (k.open_time, float(k.close))).collect())
            .collect();

        let (horizons, score) = breadth_score_at(&closes, END);
        assert_eq!(horizons, view.horizons);
        assert_eq!(score, view.score);

        // Every point of mood's 24h history is the score at that past time.
        for point in &view.history {
            assert_eq!(breadth_score_at(&closes, point.t).1, Some(point.score));
        }
        // Before the dip, A was rising on every horizon.
        let (before, _) = breadth_score_at(&closes, END - 4 * BAR_MS);
        assert_eq!(before.iter().map(|h| h.up).sum::<usize>(), 4);
    }

    #[test]
    fn labels_follow_the_thresholds() {
        assert_eq!(label(50.0), "Strongly bullish");
        assert_eq!(label(15.0), "Bullish");
        assert_eq!(label(14.9), "Neutral");
        assert_eq!(label(-14.9), "Neutral");
        assert_eq!(label(-15.0), "Bearish");
        assert_eq!(label(-50.0), "Strongly bearish");
    }
}

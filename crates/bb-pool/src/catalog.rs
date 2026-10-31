//! Dynamic Binance spot-market catalog and conservative eligibility gates.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use rust_decimal::Decimal;

use crate::error::Result;
use crate::models::MarketSnapshot;
use crate::source::{MarketRules, MarketSource, TickerQuote};

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
const VOLUME_RATE_MAX_LEGS: usize = 3;
const VOLUME_RATE_MAX_EXPANSIONS: usize = 20_000;

/// Eligibility thresholds.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogConfig {
    /// Minimum 24h quote volume valued in USDT.
    pub min_quote_volume_usdt: Decimal,
    /// Maximum `(ask - bid) / mid` in basis points.
    pub max_spread_bps: Decimal,
    /// Maximum ticker age; `0` disables the gate.
    pub ticker_stale_seconds: f64,
    /// Bound on [`MarketCatalog::scoring_assets`]; `0` = unbounded.
    pub max_scoring_assets: usize,
}

/// Classified Binance spot-market universe, in exchange order.
#[derive(Debug, Clone)]
pub struct MarketCatalog {
    config: CatalogConfig,
    markets: Vec<MarketSnapshot>,
    index: HashMap<String, usize>,
}

impl MarketCatalog {
    /// Empty catalog.
    pub fn new(config: CatalogConfig) -> Self {
        Self {
            config,
            markets: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Catalog over already-classified snapshots (replays, tests, cached state).
    pub fn from_snapshots(
        config: CatalogConfig,
        snapshots: impl IntoIterator<Item = MarketSnapshot>,
    ) -> Self {
        let mut catalog = Self::new(config);
        catalog.replace(snapshots);
        catalog
    }

    /// Fetch every market and ticker from `source` and reclassify (wall clock for staleness).
    pub async fn refresh(
        &mut self,
        source: &(impl MarketSource + ?Sized),
    ) -> Result<&[MarketSnapshot]> {
        let markets = source.load_markets().await?;
        let tickers = source.fetch_tickers().await?;
        Ok(self.classify(markets, &tickers, bb_core::time::now_ms()))
    }

    /// Classify in-memory market rules and tickers at `now_ms`.
    pub fn classify(
        &mut self,
        markets: impl IntoIterator<Item = MarketRules>,
        tickers: &HashMap<String, TickerQuote>,
        now_ms: i64,
    ) -> &[MarketSnapshot] {
        let empty = TickerQuote::default();
        let candidates: Vec<(MarketRules, TickerQuote, Vec<String>)> = markets
            .into_iter()
            .filter(|market| market.spot)
            .map(|market| {
                let ticker = tickers.get(market.symbol.trim()).copied().unwrap_or(empty);
                let reasons = self.basic_rejections(&market, &ticker, now_ms);
                (market, ticker, reasons)
            })
            .collect();

        let provisional: Vec<MarketSnapshot> = candidates
            .iter()
            .filter(|(_, _, reasons)| reasons.is_empty())
            .map(|(market, ticker, _)| snapshot(market, ticker, Vec::new(), None))
            .collect();
        let graph = price_edges(&provisional);
        let mut quote_rates: HashMap<String, Option<Decimal>> =
            HashMap::from([("USDT".into(), Some(Decimal::ONE))]);

        let mut snapshots = Vec::with_capacity(candidates.len());
        for (market, ticker, mut reasons) in candidates {
            let quote = market.quote.trim().to_uppercase();
            let quote_rate = *quote_rates
                .entry(quote.clone())
                .or_insert_with(|| best_rate(&graph, &quote, "USDT", VOLUME_RATE_MAX_LEGS));
            let volume_usdt = match (non_negative(ticker.quote_volume), quote_rate) {
                (Some(volume), Some(rate)) => volume.checked_mul(rate),
                _ => None,
            };
            if reasons.is_empty() {
                match volume_usdt {
                    None => reasons.push("quote volume cannot be valued in USDT".into()),
                    Some(volume) if volume < self.config.min_quote_volume_usdt => {
                        reasons.push("quote volume below configured USDT threshold".into());
                    }
                    Some(_) => {}
                }
            }
            snapshots.push(snapshot(&market, &ticker, reasons, volume_usdt));
        }
        self.replace(snapshots);
        &self.markets
    }

    fn replace(&mut self, snapshots: impl IntoIterator<Item = MarketSnapshot>) {
        self.markets.clear();
        self.index.clear();
        for snapshot in snapshots {
            match self.index.get(&snapshot.symbol) {
                Some(&position) => self.markets[position] = snapshot,
                None => {
                    self.index
                        .insert(snapshot.symbol.clone(), self.markets.len());
                    self.markets.push(snapshot);
                }
            }
        }
    }

    /// Catalog thresholds.
    pub fn config(&self) -> &CatalogConfig {
        &self.config
    }

    /// Every classified market in exchange order.
    pub fn markets(&self) -> &[MarketSnapshot] {
        &self.markets
    }

    /// One market by unified symbol.
    pub fn get(&self, symbol: &str) -> Option<&MarketSnapshot> {
        self.index
            .get(symbol)
            .map(|&position| &self.markets[position])
    }

    /// Currently eligible market edges.
    pub fn eligible(&self) -> impl Iterator<Item = &MarketSnapshot> {
        self.markets.iter().filter(|market| market.eligible)
    }

    /// Liquid base assets (by best USDT-valued volume, descending), bounded by
    /// `max_scoring_assets`.
    pub fn scoring_assets(&self) -> Vec<String> {
        let mut sorted: Vec<&MarketSnapshot> = self.eligible().collect();
        sorted.sort_by(|a, b| {
            b.quote_volume_usdt
                .unwrap_or_default()
                .cmp(&a.quote_volume_usdt.unwrap_or_default())
        });
        let mut assets: Vec<String> = Vec::new();
        for market in sorted {
            if !assets.contains(&market.base) {
                assets.push(market.base.clone());
            }
            if self.config.max_scoring_assets > 0 && assets.len() >= self.config.max_scoring_assets
            {
                break;
            }
        }
        assets
    }

    fn basic_rejections(
        &self,
        market: &MarketRules,
        ticker: &TickerQuote,
        now_ms: i64,
    ) -> Vec<String> {
        let mut reasons = Vec::new();
        if !market.active {
            reasons.push("market is not active".into());
        }
        let bid = positive(ticker.bid);
        let ask = positive(ticker.ask);
        let last = positive(ticker.last);
        let complete = matches!((bid, ask, last), (Some(b), Some(a), Some(_)) if a >= b);
        if !complete {
            reasons.push("ticker bid/ask/last is incomplete".into());
        }
        match ticker.timestamp_ms {
            None => reasons.push("ticker timestamp is missing".into()),
            Some(timestamp) => {
                let stale_ms = self.config.ticker_stale_seconds * 1000.0;
                #[allow(clippy::cast_precision_loss)]
                let age_ms = (now_ms - timestamp).max(0) as f64;
                if self.config.ticker_stale_seconds > 0.0 && age_ms > stale_ms {
                    reasons.push("ticker is stale".into());
                }
            }
        }
        if positive(market.min_amount).is_none() {
            reasons.push("minimum amount rule is missing".into());
        }
        if positive(market.min_notional).is_none() {
            reasons.push("minimum notional rule is missing".into());
        }
        if let (Some(bid), Some(ask)) = (bid, ask)
            && spread_bps(bid, ask).is_some_and(|spread| spread > self.config.max_spread_bps)
        {
            reasons.push("spread exceeds configured threshold".into());
        }
        reasons
    }
}

fn snapshot(
    market: &MarketRules,
    ticker: &TickerQuote,
    reasons: Vec<String>,
    quote_volume_usdt: Option<Decimal>,
) -> MarketSnapshot {
    let bid = positive(ticker.bid).unwrap_or_default();
    let ask = positive(ticker.ask).unwrap_or_default();
    let spread = if bid > Decimal::ZERO && ask > Decimal::ZERO {
        spread_bps(bid, ask)
    } else {
        None
    };
    MarketSnapshot {
        symbol: market.symbol.trim().to_owned(),
        base: market.base.trim().to_uppercase(),
        quote: market.quote.trim().to_uppercase(),
        bid,
        ask,
        last: positive(ticker.last).unwrap_or_default(),
        quote_volume: non_negative(ticker.quote_volume).unwrap_or_default(),
        quote_volume_usdt,
        price_change_pct_24h: ticker.price_change_pct,
        min_amount: positive(market.min_amount).unwrap_or_default(),
        min_notional: positive(market.min_notional).unwrap_or_default(),
        amount_precision: non_negative(market.amount_precision),
        timestamp_ms: ticker.timestamp_ms,
        spread_bps: spread,
        eligible: reasons.is_empty(),
        amount_step: positive(market.amount_step),
        rejection_reasons: reasons,
    }
}

/// `(ask - bid) / ((ask + bid) / 2) * 10_000`.
pub fn spread_bps(bid: Decimal, ask: Decimal) -> Option<Decimal> {
    let mid = (ask + bid) / Decimal::TWO;
    (ask - bid)
        .checked_div(mid)
        .and_then(|ratio| ratio.checked_mul(BPS))
}

fn positive(value: Option<Decimal>) -> Option<Decimal> {
    value.filter(|v| *v > Decimal::ZERO)
}

fn non_negative(value: Option<Decimal>) -> Option<Decimal> {
    value.filter(|v| !v.is_sign_negative() || v.is_zero())
}

type PriceGraph = HashMap<String, Vec<(String, Decimal)>>;

fn price_edges(markets: &[MarketSnapshot]) -> PriceGraph {
    let mut edges: PriceGraph = HashMap::new();
    for market in markets {
        if market.bid <= Decimal::ZERO || market.ask <= Decimal::ZERO {
            continue;
        }
        edges
            .entry(market.base.clone())
            .or_default()
            .push((market.quote.clone(), market.bid));
        if let Some(inverse) = Decimal::ONE.checked_div(market.ask) {
            edges
                .entry(market.quote.clone())
                .or_default()
                .push((market.base.clone(), inverse));
        }
    }
    edges
}

/// Heap entry ordered like Python's `(-amount, depth, asset, visited)` min-heap tuple.
#[derive(PartialEq, Eq)]
struct RateNode {
    amount: Decimal,
    depth: usize,
    asset: String,
    visited: Vec<String>,
}

impl Ord for RateNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.amount
            .cmp(&other.amount)
            .then_with(|| other.depth.cmp(&self.depth))
            .then_with(|| other.asset.cmp(&self.asset))
            .then_with(|| other.visited.cmp(&self.visited))
    }
}

impl PartialOrd for RateNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Best mark-to-market conversion rate `source -> destination` within `max_legs` hops.
fn best_rate(
    edges: &PriceGraph,
    source: &str,
    destination: &str,
    max_legs: usize,
) -> Option<Decimal> {
    if source == destination {
        return Some(Decimal::ONE);
    }
    let mut queue = BinaryHeap::from([RateNode {
        amount: Decimal::ONE,
        depth: 0,
        asset: source.to_owned(),
        visited: vec![source.to_owned()],
    }]);
    let mut best: Option<Decimal> = None;
    let mut expansions = 0usize;
    while expansions < VOLUME_RATE_MAX_EXPANSIONS {
        let Some(node) = queue.pop() else { break };
        if node.asset == destination {
            best = Some(best.map_or(node.amount, |b| b.max(node.amount)));
            continue;
        }
        if node.depth >= max_legs {
            continue;
        }
        for (next_asset, rate) in edges
            .get(&node.asset)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if node.visited.contains(next_asset) {
                continue;
            }
            let Some(amount) = node.amount.checked_mul(*rate) else {
                continue;
            };
            let mut visited = node.visited.clone();
            visited.push(next_asset.clone());
            queue.push(RateNode {
                amount,
                depth: node.depth + 1,
                asset: next_asset.clone(),
                visited,
            });
            expansions += 1;
        }
    }
    best
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn rules(symbol: &str, base: &str, quote: &str) -> MarketRules {
        MarketRules {
            symbol: symbol.into(),
            base: base.into(),
            quote: quote.into(),
            spot: true,
            active: true,
            min_amount: Some(dec!(0.00001)),
            min_notional: Some(dec!(5)),
            amount_precision: Some(dec!(0.00001)),
            amount_step: Some(dec!(0.00001)),
        }
    }

    fn ticker(bid: Decimal, ask: Decimal, volume: Decimal, now: i64) -> TickerQuote {
        TickerQuote {
            bid: Some(bid),
            ask: Some(ask),
            last: Some(bid),
            quote_volume: Some(volume),
            price_change_pct: Some(dec!(-2.5)),
            timestamp_ms: Some(now),
        }
    }

    fn config() -> CatalogConfig {
        CatalogConfig {
            min_quote_volume_usdt: dec!(100),
            max_spread_bps: dec!(20),
            ticker_stale_seconds: 300.0,
            max_scoring_assets: 0,
        }
    }

    #[test]
    fn discovers_all_spot_markets_and_normalizes_quote_volume() {
        let now = 1_780_000_000_000;
        let mut old = rules("OLD/USDT", "OLD", "USDT");
        old.active = false;
        let mut futures = rules("BTC/USDT:USDT", "BTC", "USDT");
        futures.spot = false;
        let tickers = HashMap::from([
            (
                "BNB/USDT".to_owned(),
                ticker(dec!(600), dec!(600.1), dec!(10000), now),
            ),
            (
                "COIN/BNB".to_owned(),
                ticker(dec!(0.01), dec!(0.010001), dec!(1), now),
            ),
            (
                "OLD/USDT".to_owned(),
                ticker(dec!(1), dec!(1.0001), dec!(10000), now),
            ),
        ]);
        let mut catalog = MarketCatalog::new(config());
        catalog.classify(
            [
                rules("BNB/USDT", "BNB", "USDT"),
                rules("COIN/BNB", "COIN", "BNB"),
                old,
                futures,
            ],
            &tickers,
            now,
        );

        let symbols: Vec<&str> = catalog
            .markets()
            .iter()
            .map(|m| m.symbol.as_str())
            .collect();
        assert_eq!(symbols, ["BNB/USDT", "COIN/BNB", "OLD/USDT"]);
        let bnb = catalog.get("BNB/USDT").unwrap();
        assert!(bnb.eligible);
        assert_eq!(bnb.amount_step, Some(dec!(0.00001)));
        assert_eq!(bnb.price_change_pct_24h, Some(dec!(-2.5)));
        let coin = catalog.get("COIN/BNB").unwrap();
        assert_eq!(coin.quote_volume_usdt, Some(dec!(600)));
        assert!(coin.eligible);
        let old = catalog.get("OLD/USDT").unwrap();
        assert!(!old.eligible);
        assert!(
            old.rejection_reasons
                .contains(&"market is not active".to_owned())
        );
        assert_eq!(catalog.scoring_assets(), ["BNB", "COIN"]);
    }

    #[test]
    fn rejects_stale_wide_incomplete_and_thin_markets() {
        let now = 1_780_000_000_000;
        let tickers = HashMap::from([
            (
                "STALE/USDT".to_owned(),
                ticker(dec!(1), dec!(1.0001), dec!(1000), now - 301_000),
            ),
            (
                "WIDE/USDT".to_owned(),
                ticker(dec!(1), dec!(1.1), dec!(1000), now),
            ),
            (
                "THIN/USDT".to_owned(),
                ticker(dec!(1), dec!(1.0001), dec!(1), now),
            ),
            (
                "NOMIN/USDT".to_owned(),
                ticker(dec!(1), dec!(1.0001), dec!(1000), now),
            ),
        ]);
        let mut nomin = rules("NOMIN/USDT", "NOMIN", "USDT");
        nomin.min_notional = None;
        let mut catalog = MarketCatalog::new(config());
        catalog.classify(
            [
                rules("STALE/USDT", "STALE", "USDT"),
                rules("WIDE/USDT", "WIDE", "USDT"),
                rules("THIN/USDT", "THIN", "USDT"),
                nomin,
                rules("NOTICK/USDT", "NOTICK", "USDT"),
            ],
            &tickers,
            now,
        );
        let reasons = |s: &str| catalog.get(s).unwrap().rejection_reasons.clone();
        assert_eq!(reasons("STALE/USDT"), ["ticker is stale"]);
        assert_eq!(
            reasons("WIDE/USDT"),
            ["spread exceeds configured threshold"]
        );
        assert_eq!(
            reasons("THIN/USDT"),
            ["quote volume below configured USDT threshold"]
        );
        assert_eq!(reasons("NOMIN/USDT"), ["minimum notional rule is missing"]);
        assert_eq!(
            reasons("NOTICK/USDT"),
            [
                "ticker bid/ask/last is incomplete",
                "ticker timestamp is missing"
            ]
        );
        assert_eq!(catalog.get("NOTICK/USDT").unwrap().spread_bps, None);
        assert_eq!(catalog.get("NOTICK/USDT").unwrap().price_change_pct_24h, None);
    }

    #[test]
    fn best_rate_prefers_highest_multi_hop_value() {
        let mut graph = PriceGraph::new();
        graph
            .entry("A".into())
            .or_default()
            .push(("B".into(), dec!(2)));
        graph
            .entry("B".into())
            .or_default()
            .push(("USDT".into(), dec!(3)));
        graph
            .entry("A".into())
            .or_default()
            .push(("USDT".into(), dec!(5)));
        assert_eq!(best_rate(&graph, "A", "USDT", 3), Some(dec!(6)));
        assert_eq!(best_rate(&graph, "A", "USDT", 1), Some(dec!(5)));
        assert_eq!(best_rate(&graph, "Z", "USDT", 3), None);
    }
}

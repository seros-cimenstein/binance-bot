//! Market universe, balances and route candidates (bb-pool catalog/routes + operator scope).

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;

use bb_binance::BinanceClient;
use bb_binance::spot::AccountInfo;
use bb_core::money::{BPS_PER_UNIT, to_f64};
use bb_pool::{CommissionService, MarketCatalog, MarketSnapshot, RouteEngine, RouteQuote};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{MarketPickMode, TraderConfig};
use crate::context::{decimal_json, opt_decimal_json, route_id};
use crate::error::Result;
use crate::evidence::{RouteEvidence, is_low_upside};

/// Quote assets that earn the small-market quote bonus.
const MAJOR_QUOTES: [&str; 6] = ["USDT", "FDUSD", "USDC", "BNB", "BTC", "ETH"];

/// Scoped market snapshot + route engine for one point in time.
#[derive(Debug)]
pub struct MarketView {
    /// In-scope markets by unified symbol (eligible or not).
    pub markets: BTreeMap<String, MarketSnapshot>,
    pub routes: RouteEngine,
    pub fees: Arc<CommissionService>,
    pub refreshed_at: DateTime<Utc>,
}

impl MarketView {
    /// Refresh the catalog from Binance and apply the operator scope.
    pub async fn load(
        client: &BinanceClient,
        fees: Arc<CommissionService>,
        config: &TraderConfig,
    ) -> Result<Self> {
        let mut catalog = MarketCatalog::new(config.catalog_config());
        let snapshots = catalog.refresh(client).await?.to_vec();
        Ok(Self::from_snapshots(snapshots, fees, config))
    }

    /// View over already-classified snapshots.
    pub fn from_snapshots(
        snapshots: impl IntoIterator<Item = MarketSnapshot>,
        fees: Arc<CommissionService>,
        config: &TraderConfig,
    ) -> Self {
        let markets: BTreeMap<String, MarketSnapshot> = snapshots
            .into_iter()
            .filter(|market| config.scope.market_block_reason(market).is_none())
            .map(|market| (market.symbol.clone(), market))
            .collect();
        let routes = RouteEngine::new(markets.values(), Arc::clone(&fees), config.route_config());
        Self {
            markets,
            routes,
            fees,
            refreshed_at: bb_core::time::now(),
        }
    }

    /// Indicative USDT value of `amount` of `asset`.
    pub fn usdt_value(&self, asset: &str, amount: Decimal) -> Option<Decimal> {
        if asset == "USDT" {
            return Some(amount);
        }
        self.routes
            .indicative_rate(asset, "USDT")
            .map(|rate| amount * rate)
    }

    pub fn age_seconds(&self) -> f64 {
        (bb_core::time::now() - self.refreshed_at).num_milliseconds() as f64 / 1_000.0
    }
}

/// Positive free / locked / total balances (Python `_sanitize_balance`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Balances {
    #[serde(serialize_with = "decimal_map")]
    pub free: BTreeMap<String, Decimal>,
    #[serde(serialize_with = "decimal_map")]
    pub used: BTreeMap<String, Decimal>,
    #[serde(serialize_with = "decimal_map")]
    pub total: BTreeMap<String, Decimal>,
}

fn decimal_map<S: serde::Serializer>(
    map: &BTreeMap<String, Decimal>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut out = serializer.serialize_map(Some(map.len()))?;
    for (key, value) in map {
        out.serialize_entry(key, &decimal_json(*value))?;
    }
    out.end()
}

impl Balances {
    pub fn from_account(account: &AccountInfo) -> Self {
        let mut balances = Self::default();
        for row in &account.balances {
            let asset = row.asset.trim().to_uppercase();
            if row.free > Decimal::ZERO {
                balances.free.insert(asset.clone(), row.free);
            }
            if row.locked > Decimal::ZERO {
                balances.used.insert(asset.clone(), row.locked);
            }
            if row.total() > Decimal::ZERO {
                balances.total.insert(asset, row.total());
            }
        }
        balances
    }

    pub fn free(&self, asset: &str) -> Decimal {
        self.free.get(asset).copied().unwrap_or_default()
    }

    /// Sum of indicative USDT values of free balances (Python `_portfolio_value_usdt`).
    pub fn free_value_usdt(&self, view: &MarketView) -> Decimal {
        self.free
            .iter()
            .filter_map(|(asset, amount)| view.usdt_value(asset, *amount))
            .sum()
    }

    /// Same over free + locked.
    pub fn total_value_usdt(&self, view: &MarketView) -> Decimal {
        self.total
            .iter()
            .filter_map(|(asset, amount)| view.usdt_value(asset, *amount))
            .sum()
    }
}

/// One executable route quote offered to the bot.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCandidate {
    pub route_id: String,
    pub route: RouteQuote,
    pub estimated_output_usdt: Option<Decimal>,
    pub source_value_usdt: Option<Decimal>,
    pub immediate_friction_bps: Option<f64>,
    /// Expected one-way cost of the route in bps: per leg, the account's effective taker
    /// commission (BNB discount when the wallet holds BNB) plus half the market's spread.
    /// Information only; the quote and order sizing keep their safety buffers.
    pub expected_cost_bps: Option<f64>,
    pub market_pick_score: f64,
    pub market_pick_reason: String,
    pub quote_volume_usdt: Option<Decimal>,
    pub spread_bps: Option<Decimal>,
    pub liquidity_tier: &'static str,
    pub market_evidence: Option<RouteEvidence>,
    pub social_signal_evidence: Option<Value>,
}

impl RouteCandidate {
    /// Python `BotRouteCandidate.as_context` (audit `candidate_routes[]` / `selected_route`).
    pub fn as_context(&self) -> Value {
        let legs: Vec<Value> = self
            .route
            .legs
            .iter()
            .map(|leg| {
                json!({
                    "symbol": leg.symbol,
                    "side": leg.side.as_str(),
                    "source_asset": leg.source_asset,
                    "destination_asset": leg.destination_asset,
                    "input_amount": decimal_json(leg.input_amount),
                    "spent_amount": decimal_json(leg.spent_amount),
                    "gross_output_amount": decimal_json(leg.gross_output_amount),
                    "fee_rate": decimal_json(leg.fee_rate),
                    "fee_output_amount": decimal_json(leg.fee_output_amount),
                    "net_output_amount": decimal_json(leg.net_output_amount),
                    "commission_discount_asset": leg.commission_discount_asset,
                })
            })
            .collect();
        json!({
            "route_id": self.route_id,
            "source_asset": self.route.source_asset,
            "destination_asset": self.route.destination_asset,
            "input_amount": decimal_json(self.route.input_amount),
            "output_amount": decimal_json(self.route.output_amount),
            "estimated_output_usdt": opt_decimal_json(self.estimated_output_usdt),
            "source_value_usdt": opt_decimal_json(self.source_value_usdt),
            "immediate_friction_bps": self.immediate_friction_bps,
            "expected_cost_bps": self.expected_cost_bps,
            "market_pick_score": finite(self.market_pick_score),
            "market_pick_reason": self.market_pick_reason,
            "quote_volume_usdt": opt_decimal_json(self.quote_volume_usdt),
            "spread_bps": opt_decimal_json(self.spread_bps),
            "liquidity_tier": self.liquidity_tier,
            "market_evidence": self.market_evidence.as_ref().map_or_else(|| json!({}), |e| json!(e)),
            "social_signal_evidence": self.social_signal_evidence.clone().unwrap_or_else(|| json!({})),
            "slippage_bps": decimal_json(self.route.slippage_bps),
            "legs": legs,
        })
    }

    /// `"BTC/USDT sell -> ETH/USDT buy"`.
    pub fn legs_label(&self) -> String {
        self.route
            .legs
            .iter()
            .map(|leg| format!("{} {}", leg.symbol, leg.side))
            .collect::<Vec<_>>()
            .join(" -> ")
    }
}

fn finite(value: f64) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

/// Destinations to quote (Python `_candidate_assets`).
pub fn candidate_assets(
    view: &MarketView,
    free: &BTreeMap<String, Decimal>,
    config: &TraderConfig,
) -> Vec<String> {
    if config.market_pick_mode == MarketPickMode::SmallMarketAggressive {
        return small_market_candidate_assets(view, free, config);
    }
    let mut eligible: Vec<&MarketSnapshot> = view.markets.values().filter(|m| m.eligible).collect();
    eligible.sort_by_key(|market| Reverse(market.quote_volume_usdt.unwrap_or_default()));
    let mut assets: Vec<String> = Vec::new();
    for market in eligible {
        let base = market.base.trim().to_uppercase();
        if !base.is_empty()
            && !is_low_upside(&base)
            && !assets.contains(&base)
            && config.scope.asset_block_reason(&base).is_none()
        {
            assets.push(base);
        }
        if config.candidate_limit > 0 && assets.len() >= config.candidate_limit {
            break;
        }
    }
    if !assets.iter().any(|a| a == "USDT") && config.scope.asset_block_reason("USDT").is_none() {
        assets.push("USDT".into());
    }
    assets
}

fn small_market_candidate_assets(
    view: &MarketView,
    free: &BTreeMap<String, Decimal>,
    config: &TraderConfig,
) -> Vec<String> {
    let mut best: BTreeMap<String, (f64, &MarketSnapshot)> = BTreeMap::new();
    for market in view.markets.values().filter(|m| m.eligible) {
        let asset = market.base.trim().to_uppercase();
        if asset.is_empty()
            || free.contains_key(&asset)
            || is_low_upside(&asset)
            || config.scope.asset_block_reason(&asset).is_some()
        {
            continue;
        }
        let score = small_market_asset_score(market, config);
        if best.get(&asset).is_none_or(|(current, _)| score > *current) {
            best.insert(asset, (score, market));
        }
    }
    let mut ranked: Vec<(String, f64, f64)> = best
        .into_iter()
        .map(|(asset, (score, market))| (asset, score, spread_f64(market, config)))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.2.total_cmp(&b.2))
            .then_with(|| b.0.cmp(&a.0))
    });
    let limit = config.small_candidate_limit;
    let mut assets: Vec<String> = ranked
        .into_iter()
        .take(if limit == 0 { usize::MAX } else { limit })
        .map(|(asset, _, _)| asset)
        .collect();
    if !assets.iter().any(|a| a == "USDT") && (limit == 0 || assets.len() < limit) {
        assets.push("USDT".into());
    }
    assets
}

/// Spread in bps; an unknown spread (Python `inf`) counts as the small-mode maximum.
fn spread_f64(market: &MarketSnapshot, config: &TraderConfig) -> f64 {
    market
        .spread_bps
        .map_or(to_f64(config.small_max_spread_bps), to_f64)
}

/// Python `_small_market_asset_score`.
pub fn small_market_asset_score(market: &MarketSnapshot, config: &TraderConfig) -> f64 {
    let volume = market.quote_volume_usdt.map_or(0.0, to_f64);
    if volume <= 0.0 {
        return f64::NEG_INFINITY;
    }
    let small_bonus = if volume < to_f64(config.min_quote_volume_usdt) {
        100.0
    } else {
        0.0
    };
    let volume_score = -volume.max(1.0).log10() * 5.0;
    let spread_penalty = spread_f64(market, config)
        .max(0.0)
        .min(to_f64(config.small_max_spread_bps))
        * 0.08;
    let notional_bonus = (10.0 - to_f64(market.min_notional)).max(0.0);
    let quote_bonus = if MAJOR_QUOTES.contains(&market.quote.as_str()) {
        5.0
    } else {
        0.0
    };
    small_bonus + volume_score + notional_bonus + quote_bonus - spread_penalty
}

/// Last leg market touching the destination (Python `_route_focus_market`).
pub fn focus_market<'a>(
    route: &RouteQuote,
    markets: &'a BTreeMap<String, MarketSnapshot>,
) -> Option<&'a MarketSnapshot> {
    route
        .legs
        .iter()
        .rev()
        .filter_map(|leg| markets.get(&leg.symbol))
        .find(|market| {
            route.destination_asset == market.base || route.destination_asset == market.quote
        })
        .or_else(|| route.legs.last().and_then(|leg| markets.get(&leg.symbol)))
}

/// Python `_route_market_pick_score`.
pub fn pick_score(
    route: &RouteQuote,
    market: Option<&MarketSnapshot>,
    source_value_usdt: Option<Decimal>,
    immediate_friction_bps: Option<f64>,
    config: &TraderConfig,
) -> (f64, String) {
    let Some(market) = market else {
        return (0.0, "no focus market metadata".into());
    };
    let asset_score = small_market_asset_score(market, config);
    let friction_penalty = immediate_friction_bps.unwrap_or(0.0).clamp(0.0, 2_000.0) * 0.04;
    let leg_penalty = route.legs.len().saturating_sub(1) as f64 * 4.0;
    let min_notional = to_f64(market.min_notional);
    let source_fit_bonus = match source_value_usdt {
        Some(value) if min_notional > 0.0 => (to_f64(value) / min_notional).min(4.0) * 2.0,
        _ => 0.0,
    };
    let reason = format!(
        "{} market volume={} USDT, spread={} bps, min_notional={}, legs={}, immediate_friction={} bps",
        liquidity_tier(market.quote_volume_usdt),
        fmt_g6(market.quote_volume_usdt.map(to_f64)),
        fmt_g6(Some(market.spread_bps.map_or(f64::INFINITY, to_f64))),
        fmt_g6(Some(min_notional)),
        route.legs.len(),
        fmt_g6(immediate_friction_bps),
    );
    (
        asset_score + source_fit_bonus - friction_penalty - leg_penalty,
        reason,
    )
}

/// `micro < 25k <= small < 100k <= mid < 1M <= large`.
pub fn liquidity_tier(volume_usdt: Option<Decimal>) -> &'static str {
    match volume_usdt {
        None => "unknown",
        Some(v) if v < Decimal::from(25_000) => "micro",
        Some(v) if v < Decimal::from(100_000) => "small",
        Some(v) if v < Decimal::from(1_000_000) => "mid",
        Some(_) => "large",
    }
}

/// Python `f"{value:.6g}"` (`unknown` for `None`).
pub fn fmt_g6(value: Option<f64>) -> String {
    let Some(value) = value else {
        return "unknown".into();
    };
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    if value == 0.0 {
        return "0".into();
    }
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let trim = |text: &str| -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text.to_owned()
        }
    };
    if !(-4..6).contains(&exponent) {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exponent.abs())
    } else {
        let decimals = usize::try_from(5 - exponent).unwrap_or(0);
        trim(&format!("{value:.decimals$}"))
    }
}

/// `(spent - output) / spent` in bps, where `spent` is the source value minus `kept_usdt`:
/// input a route leaves unspent (lot-size rounding, sizing reserve) stays in the wallet and
/// is no cost of the trade.
pub fn immediate_friction_bps(
    source_value_usdt: Option<Decimal>,
    kept_usdt: Decimal,
    estimated_output_usdt: Option<Decimal>,
) -> Option<f64> {
    let spent = source_value_usdt? - kept_usdt;
    let output = estimated_output_usdt?;
    (spent > Decimal::ZERO).then(|| to_f64((spent - output) / spent * BPS_PER_UNIT))
}

/// Expected one-way cost of `route` in bps: per leg the account's taker commission for that
/// side (BNB-discounted when `bnb_discount` and the account enables it) plus half the leg
/// market's spread. `None` when a leg's market or spread is unknown.
pub async fn expected_cost_bps(view: &MarketView, route: &RouteQuote, bnb_discount: bool) -> Result<Option<f64>> {
    let mut total = Decimal::ZERO;
    for leg in &route.legs {
        let Some(spread) = view.markets.get(&leg.symbol).and_then(|m| m.spread_bps) else {
            return Ok(None);
        };
        let commission = view.fees.get(&leg.symbol).await?;
        total += commission.taker_rate(leg.side, bnb_discount) * BPS_PER_UNIT + spread / Decimal::TWO;
    }
    Ok(Some(to_f64(total)))
}

/// Quote `amount` of `source` into `destination` and decorate it as a candidate.
/// `None` when no rule-valid, in-scope route exists. `bnb_discount`: the wallet holds free
/// BNB, so Binance can take fees at the discounted rate (only the expected-cost figure uses it).
pub async fn quote_candidate(
    view: &MarketView,
    config: &TraderConfig,
    source: &str,
    destination: &str,
    amount: Decimal,
    bnb_discount: bool,
) -> Result<Option<RouteCandidate>> {
    let Some(route) = view
        .routes
        .quote(source, destination, amount, false)
        .await?
    else {
        return Ok(None);
    };
    if route.legs.is_empty() || config.scope.route_block_reason(&route).is_some() {
        return Ok(None);
    }
    let estimated_output_usdt = view.usdt_value(destination, route.output_amount);
    let source_value_usdt = view.usdt_value(source, amount);
    let kept_usdt: Decimal = route
        .legs
        .iter()
        .filter(|leg| leg.input_amount > leg.spent_amount)
        .filter_map(|leg| view.usdt_value(&leg.source_asset, leg.input_amount - leg.spent_amount))
        .sum();
    let friction = immediate_friction_bps(source_value_usdt, kept_usdt, estimated_output_usdt);
    let expected_cost = expected_cost_bps(view, &route, bnb_discount).await?;
    let market = focus_market(&route, &view.markets);
    let (score, reason) = pick_score(&route, market, source_value_usdt, friction, config);
    Ok(Some(RouteCandidate {
        route_id: route_id(&route),
        estimated_output_usdt,
        source_value_usdt,
        immediate_friction_bps: friction,
        expected_cost_bps: expected_cost,
        market_pick_score: score,
        market_pick_reason: reason,
        quote_volume_usdt: market.and_then(|m| m.quote_volume_usdt),
        spread_bps: market.and_then(|m| m.spread_bps),
        liquidity_tier: liquidity_tier(market.and_then(|m| m.quote_volume_usdt)),
        market_evidence: None,
        social_signal_evidence: None,
        route,
    }))
}

/// Quote every (held source, destination) pair. Sources are positive free balances in scope;
/// destinations default to [`candidate_assets`]. No friction ceiling: costs are information.
pub async fn quote_routes(
    view: &MarketView,
    config: &TraderConfig,
    free: &BTreeMap<String, Decimal>,
    sources: Option<&[String]>,
    destinations: &[String],
) -> Result<Vec<RouteCandidate>> {
    let mut candidates = Vec::new();
    let bnb_discount = free.get("BNB").is_some_and(|bnb| *bnb > Decimal::ZERO);
    for (source, amount) in free {
        if *amount <= Decimal::ZERO
            || config.scope.asset_block_reason(source).is_some()
            || sources.is_some_and(|wanted| !wanted.contains(source))
        {
            continue;
        }
        for destination in destinations.iter().filter(|d| *d != source) {
            if let Some(candidate) =
                quote_candidate(view, config, source, destination, *amount, bnb_discount).await?
            {
                candidates.push(candidate);
            }
        }
    }
    sort_candidates(&mut candidates, config.market_pick_mode);
    Ok(candidates)
}

/// Python candidate ordering for the pick mode.
pub fn sort_candidates(candidates: &mut [RouteCandidate], mode: MarketPickMode) {
    match mode {
        MarketPickMode::SmallMarketAggressive => candidates.sort_by(|a, b| {
            b.market_pick_score
                .total_cmp(&a.market_pick_score)
                .then_with(|| {
                    a.immediate_friction_bps
                        .unwrap_or(0.0)
                        .total_cmp(&b.immediate_friction_bps.unwrap_or(0.0))
                })
                .then_with(|| a.route.legs.len().cmp(&b.route.legs.len()))
                .then_with(|| b.route.destination_asset.cmp(&a.route.destination_asset))
        }),
        MarketPickMode::LiquidTopValue => candidates.sort_by(|a, b| {
            let key = |c: &RouteCandidate| {
                (
                    c.estimated_output_usdt.is_some(),
                    c.estimated_output_usdt.unwrap_or_default(),
                    Reverse(c.route.legs.len()),
                )
            };
            key(b)
                .cmp(&key(a))
                .then_with(|| b.route.destination_asset.cmp(&a.route.destination_asset))
        }),
    }
}

/// The cycle's candidate table: quote everything, sort, keep the configured top N.
pub async fn build_route_candidates(
    view: &MarketView,
    config: &TraderConfig,
    free: &BTreeMap<String, Decimal>,
) -> Result<Vec<RouteCandidate>> {
    let destinations = candidate_assets(view, free, config);
    let mut candidates = quote_routes(view, config, free, None, &destinations).await?;
    let limit = config.effective_candidate_limit();
    if limit > 0 {
        candidates.truncate(limit);
    }
    Ok(candidates)
}

#[cfg(test)]
pub(crate) mod tests {
    use bb_pool::{CommissionSnapshot, CommissionSource};
    use rust_decimal_macros::dec;

    use super::*;
    use crate::config::tests::lookup;

    pub(crate) struct NoCommissions;

    #[async_trait::async_trait]
    impl CommissionSource for NoCommissions {
        async fn account_commission(
            &self,
            symbol: &str,
        ) -> bb_pool::Result<bb_binance::spot::AccountCommission> {
            Err(bb_pool::PoolError::Config(format!(
                "unexpected commission fetch for {symbol}"
            )))
        }
    }

    pub(crate) fn market(
        symbol: &str,
        bid: Decimal,
        ask: Decimal,
        volume_usdt: Decimal,
    ) -> MarketSnapshot {
        let (base, quote) = symbol.split_once('/').unwrap();
        MarketSnapshot {
            symbol: symbol.into(),
            base: base.into(),
            quote: quote.into(),
            bid,
            ask,
            last: (bid + ask) / dec!(2),
            quote_volume: volume_usdt,
            quote_volume_usdt: Some(volume_usdt),
            min_amount: dec!(0.0001),
            min_notional: dec!(0.0001),
            amount_precision: None,
            timestamp_ms: Some(bb_core::time::now_ms()),
            spread_bps: Some((ask - bid) / bid * dec!(10000)),
            eligible: true,
            amount_step: Some(dec!(0.0001)),
            rejection_reasons: Vec::new(),
        }
    }

    pub(crate) fn fees(symbols: &[&str]) -> Arc<CommissionService> {
        let service = CommissionService::new(Arc::new(NoCommissions), 3_600.0);
        for symbol in symbols {
            service.insert(CommissionSnapshot::flat(*symbol, dec!(0.001), dec!(0.001)));
        }
        Arc::new(service)
    }

    pub(crate) fn config(pairs: &[(&str, &str)]) -> TraderConfig {
        TraderConfig::from_lookup(lookup(pairs)).unwrap()
    }

    /// BTC/USDT, ETH/USDT, ETH/BTC, SOL/ETH (SOL only reachable through ETH), EUR/USDT.
    pub(crate) fn view(config: &TraderConfig) -> MarketView {
        let markets = vec![
            market("BTC/USDT", dec!(60000), dec!(60010), dec!(5000000)),
            market("ETH/USDT", dec!(3000), dec!(3001), dec!(3000000)),
            market("ETH/BTC", dec!(0.05), dec!(0.05001), dec!(2000000)),
            market("SOL/ETH", dec!(0.05), dec!(0.05002), dec!(40000)),
            market("EUR/USDT", dec!(1.08), dec!(1.081), dec!(9000000)),
        ];
        MarketView::from_snapshots(
            markets,
            fees(&["BTC/USDT", "ETH/USDT", "ETH/BTC", "SOL/ETH", "EUR/USDT"]),
            config,
        )
    }

    #[tokio::test]
    async fn build_context_offers_dynamic_multi_hop_route_and_blocks_fiat() {
        let config = config(&[("BOT_SPOT_CANDIDATE_LIMIT", "0")]);
        let view = view(&config);
        assert!(
            !view.markets.contains_key("EUR/USDT"),
            "fiat market scoped out"
        );
        let free = BTreeMap::from([("USDT".to_owned(), dec!(100))]);
        let destinations = candidate_assets(&view, &free, &config);
        assert!(destinations.contains(&"SOL".to_owned()));
        assert!(!destinations.contains(&"EUR".to_owned()));
        let candidates = build_route_candidates(&view, &config, &free).await.unwrap();
        let sol = candidates
            .iter()
            .find(|c| c.route.destination_asset == "SOL")
            .unwrap();
        assert_eq!(sol.route.legs.len(), 2);
        assert_eq!(sol.route_id, route_id(&sol.route));
        assert!(sol.route_id.starts_with("USDT->SOL:"));
        assert!(sol.immediate_friction_bps.unwrap() > 0.0);
        assert_eq!(sol.liquidity_tier, "small");
        // Liquid ordering: highest estimated USDT output first.
        let outputs: Vec<Decimal> = candidates
            .iter()
            .map(|c| c.estimated_output_usdt.unwrap())
            .collect();
        assert!(outputs.windows(2).all(|pair| pair[0] >= pair[1]));
    }

    #[tokio::test]
    async fn high_friction_routes_are_kept_as_information() {
        // The Python friction ceiling dropped these; now the bot sees them with their cost.
        let config = config(&[("BOT_SPOT_CANDIDATE_LIMIT", "0")]);
        let mut markets = vec![market("BTC/USDT", dec!(60000), dec!(66000), dec!(5000000))];
        markets[0].spread_bps = Some(dec!(10));
        let view = MarketView::from_snapshots(markets, fees(&["BTC/USDT"]), &config);
        let free = BTreeMap::from([("USDT".to_owned(), dec!(100))]);
        let candidates = build_route_candidates(&view, &config, &free).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].immediate_friction_bps.unwrap() > 900.0);
    }

    #[tokio::test]
    async fn expected_cost_is_discounted_fee_plus_half_spread_and_leaves_the_quote_alone() {
        let config = config(&[]);
        let mut btc = market("BTC/USDT", dec!(60000), dec!(60012), dec!(5000000));
        btc.spread_bps = Some(dec!(2));
        let fees = CommissionService::new(Arc::new(NoCommissions), 3_600.0);
        let mut schedule = CommissionSnapshot::flat("BTC/USDT", dec!(0.001), dec!(0.001));
        schedule.discount_enabled = true;
        schedule.discount_asset = "BNB".into();
        schedule.discount_multiplier = dec!(0.75);
        fees.insert(schedule);
        let view = MarketView::from_snapshots(vec![btc], Arc::new(fees), &config);

        let with_bnb = quote_candidate(&view, &config, "USDT", "BTC", dec!(100), true)
            .await
            .unwrap()
            .unwrap();
        let without_bnb = quote_candidate(&view, &config, "USDT", "BTC", dec!(100), false)
            .await
            .unwrap()
            .unwrap();
        // 10 bps taker x 0.75 BNB discount + 2 bps spread / 2.
        assert!((with_bnb.expected_cost_bps.unwrap() - 8.5).abs() < 1e-9);
        assert!((without_bnb.expected_cost_bps.unwrap() - 11.0).abs() < 1e-9);
        // Sizing, the slippage buffer and the quoted friction do not depend on the figure.
        assert_eq!(with_bnb.route.legs, without_bnb.route.legs);
        assert_eq!(with_bnb.route.output_amount, without_bnb.route.output_amount);
        assert_eq!(with_bnb.immediate_friction_bps, without_bnb.immediate_friction_bps);
        assert!(with_bnb.immediate_friction_bps.unwrap() > with_bnb.expected_cost_bps.unwrap());
    }

    #[tokio::test]
    async fn aggressive_mode_prefers_small_markets_and_limits() {
        let config = config(&[
            ("BOT_SPOT_MARKET_PICK_MODE", "small_market_aggressive"),
            ("BOT_SPOT_SMALL_CANDIDATE_LIMIT", "2"),
        ]);
        let view = view(&config);
        let free = BTreeMap::from([("USDT".to_owned(), dec!(100))]);
        let assets = candidate_assets(&view, &free, &config);
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0], "SOL", "smallest market ranks first: {assets:?}");
        let candidates = build_route_candidates(&view, &config, &free).await.unwrap();
        assert!(candidates.len() <= 2);
        assert!(
            candidates
                .windows(2)
                .all(|pair| pair[0].market_pick_score >= pair[1].market_pick_score)
        );
    }
    #[test]
    fn liquid_top_value_skips_stablecoins_and_dust_as_buy_targets_while_keeping_usdt_exit() {
        let config = config(&[
            ("BOT_SPOT_MARKET_PICK_MODE", "liquid_top_value"),
            ("BOT_SPOT_CANDIDATE_LIMIT", "2"),
        ]);
        let markets = vec![
            market("USDC/USDT", dec!(1), dec!(1.0001), dec!(90_000_000)),
            market("FDUSD/USDT", dec!(1), dec!(1.0001), dec!(80_000_000)),
            market("BTC/USDT", dec!(60000), dec!(60010), dec!(50_000_000)),
            market("SOL/USDT", dec!(150), dec!(150.05), dec!(30_000_000)),
            market("ETH/USDT", dec!(3000), dec!(3001), dec!(20_000_000)),
        ];
        let view = MarketView::from_snapshots(
            markets,
            fees(&["USDC/USDT", "FDUSD/USDT", "BTC/USDT", "SOL/USDT", "ETH/USDT"]),
            &config,
        );
        let free = BTreeMap::from([("ETH".to_owned(), dec!(0.1))]);
        let assets = candidate_assets(&view, &free, &config);
        assert_eq!(assets, vec!["BTC", "SOL", "USDT"]);
    }


    #[test]
    fn friction_counts_only_the_spent_part() {
        // 15 USDT quoted, 1.5 USDT left unspent by lot rounding, 13.4 USDT of coin out:
        // the cost is 0.1 on the 13.5 spent (74 bps), not 1.6 on 15 (1067 bps).
        let friction = immediate_friction_bps(Some(dec!(15)), dec!(1.5), Some(dec!(13.4))).unwrap();
        assert!((friction - 74.074).abs() < 0.01, "{friction}");
        assert_eq!(immediate_friction_bps(Some(dec!(1)), dec!(1), Some(dec!(0))), None);
    }

    #[test]
    fn liquidity_tiers_and_g_format() {
        assert_eq!(liquidity_tier(Some(dec!(24999))), "micro");
        assert_eq!(liquidity_tier(Some(dec!(25000))), "small");
        assert_eq!(liquidity_tier(Some(dec!(100000))), "mid");
        assert_eq!(liquidity_tier(Some(dec!(1000000))), "large");
        assert_eq!(liquidity_tier(None), "unknown");
        assert_eq!(fmt_g6(Some(5_000_000.0)), "5e+06");
        assert_eq!(fmt_g6(Some(123_456.0)), "123456");
        assert_eq!(fmt_g6(Some(1.5)), "1.5");
        assert_eq!(fmt_g6(Some(0.0001)), "0.0001");
        assert_eq!(fmt_g6(Some(0.00001234)), "1.234e-05");
        assert_eq!(fmt_g6(Some(16.6667)), "16.6667");
        assert_eq!(fmt_g6(None), "unknown");
        assert_eq!(fmt_g6(Some(f64::INFINITY)), "inf");
    }

    #[test]
    fn balances_keep_positive_rows_only() {
        let account = AccountInfo {
            balances: vec![
                bb_binance::spot::Balance {
                    asset: "btc".into(),
                    free: dec!(0.5),
                    locked: dec!(0.1),
                },
                bb_binance::spot::Balance {
                    asset: "ETH".into(),
                    free: dec!(0),
                    locked: dec!(0),
                },
            ],
            ..AccountInfo::default()
        };
        let balances = Balances::from_account(&account);
        assert_eq!(
            balances.free,
            BTreeMap::from([("BTC".to_owned(), dec!(0.5))])
        );
        assert_eq!(
            balances.used,
            BTreeMap::from([("BTC".to_owned(), dec!(0.1))])
        );
        assert_eq!(
            balances.total,
            BTreeMap::from([("BTC".to_owned(), dec!(0.6))])
        );
    }
}

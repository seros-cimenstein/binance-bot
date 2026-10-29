//! Bounded conversion-graph search with dynamic rule and fee costing.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use bb_core::money::floor_to_step;
use rust_decimal::Decimal;

use crate::error::Result;
use crate::fees::CommissionService;
use crate::models::{LegSide, MarketSnapshot, RouteLeg, RouteQuote};

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// Search and costing bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteConfig {
    /// Maximum legs per route (1..=3).
    pub max_route_legs: usize,
    /// Maximum heap pushes per search.
    pub max_route_expansions: usize,
    /// Top-N gross candidates that are fee-costed (plus the best path of each leg count).
    pub max_candidate_routes: usize,
    /// Adverse price buffer applied to every leg.
    pub slippage_buffer_bps: Decimal,
}

/// One hop of a candidate path: market index, side, and the asset received.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    market: usize,
    side: LegSide,
    next_asset: String,
}

/// Gross (pre-fee) candidate path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    output: Decimal,
    path: Vec<Step>,
}

/// Search node ordered like Python's `(-output, depth, sequence)` min-heap key.
struct Node {
    output: Decimal,
    depth: usize,
    sequence: usize,
    asset: String,
    visited: Vec<String>,
    path: Vec<Step>,
}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Node {}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        self.output
            .cmp(&other.output)
            .then_with(|| other.depth.cmp(&self.depth))
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Finds the best valid simulated route between two Binance assets over eligible markets.
#[derive(Debug)]
pub struct RouteEngine {
    config: RouteConfig,
    fees: Arc<CommissionService>,
    markets: Vec<MarketSnapshot>,
    adjacency: HashMap<String, Vec<(usize, LegSide, String)>>,
}

impl RouteEngine {
    /// Engine over the eligible subset of `markets`.
    pub fn new<'a>(
        markets: impl IntoIterator<Item = &'a MarketSnapshot>,
        fees: Arc<CommissionService>,
        config: RouteConfig,
    ) -> Self {
        let markets: Vec<MarketSnapshot> = markets
            .into_iter()
            .filter(|m| m.eligible)
            .cloned()
            .collect();
        let mut adjacency: HashMap<String, Vec<(usize, LegSide, String)>> = HashMap::new();
        for (index, market) in markets.iter().enumerate() {
            adjacency.entry(market.base.clone()).or_default().push((
                index,
                LegSide::Sell,
                market.quote.clone(),
            ));
            adjacency.entry(market.quote.clone()).or_default().push((
                index,
                LegSide::Buy,
                market.base.clone(),
            ));
        }
        Self {
            config,
            fees,
            markets,
            adjacency,
        }
    }

    /// Search bounds.
    pub fn config(&self) -> &RouteConfig {
        &self.config
    }

    /// Best rule-valid output before commissions.
    pub fn gross_quote(
        &self,
        source_asset: &str,
        destination_asset: &str,
        amount: Decimal,
    ) -> Option<Decimal> {
        self.candidate_paths(source_asset, destination_asset, amount, true)
            .into_iter()
            .map(|c| c.output)
            .max()
    }

    /// Order-rule-independent mark rate (one unit of `source_asset`) for portfolio reporting.
    pub fn indicative_rate(&self, source_asset: &str, destination_asset: &str) -> Option<Decimal> {
        self.candidate_paths(source_asset, destination_asset, Decimal::ONE, false)
            .into_iter()
            .map(|c| c.output)
            .max()
    }

    /// Best post-fee route using authenticated commissions. Costs the top
    /// `max_candidate_routes` gross candidates plus the best candidate of every leg count.
    pub async fn quote(
        &self,
        source_asset: &str,
        destination_asset: &str,
        amount: Decimal,
        apply_bnb_discount: bool,
    ) -> Result<Option<RouteQuote>> {
        let candidates = self.candidate_paths(source_asset, destination_asset, amount, true);
        let top = self.config.max_candidate_routes.min(candidates.len());
        let mut selected: Vec<usize> = (0..top).collect();
        for leg_count in 1..=self.config.max_route_legs {
            if let Some(index) = candidates.iter().position(|c| c.path.len() == leg_count)
                && !selected.contains(&index)
            {
                selected.push(index);
            }
        }
        let mut best: Option<RouteQuote> = None;
        for index in selected {
            let route = self
                .cost_path(
                    source_asset,
                    destination_asset,
                    amount,
                    &candidates[index].path,
                    apply_bnb_discount,
                )
                .await?;
            if let Some(route) = route
                && best
                    .as_ref()
                    .is_none_or(|b| route.output_amount > b.output_amount)
            {
                best = Some(route);
            }
        }
        Ok(best)
    }

    fn candidate_paths(
        &self,
        source_asset: &str,
        destination_asset: &str,
        amount: Decimal,
        enforce_order_rules: bool,
    ) -> Vec<Candidate> {
        let source = source_asset.trim().to_uppercase();
        let destination = destination_asset.trim().to_uppercase();
        if source == destination {
            return vec![Candidate {
                output: amount,
                path: Vec::new(),
            }];
        }
        if amount <= Decimal::ZERO {
            return Vec::new();
        }
        let mut sequence = 0usize;
        let mut queue = BinaryHeap::from([Node {
            output: amount,
            depth: 0,
            sequence,
            asset: source.clone(),
            visited: vec![source],
            path: Vec::new(),
        }]);
        let mut completed: Vec<Candidate> = Vec::new();
        let mut expansions = 0usize;
        while expansions < self.config.max_route_expansions {
            let Some(node) = queue.pop() else { break };
            if node.asset == destination {
                completed.push(Candidate {
                    output: node.output,
                    path: node.path,
                });
                continue;
            }
            if node.depth >= self.config.max_route_legs {
                continue;
            }
            let Some(edges) = self.adjacency.get(&node.asset) else {
                continue;
            };
            for (market, side, next_asset) in edges {
                if node.visited.contains(next_asset) {
                    continue;
                }
                let Some((output, _)) = gross_leg(
                    &self.markets[*market],
                    *side,
                    node.output,
                    self.config.slippage_buffer_bps,
                    enforce_order_rules,
                ) else {
                    continue;
                };
                let mut path = node.path.clone();
                path.push(Step {
                    market: *market,
                    side: *side,
                    next_asset: next_asset.clone(),
                });
                let mut visited = node.visited.clone();
                visited.push(next_asset.clone());
                sequence += 1;
                queue.push(Node {
                    output,
                    depth: node.depth + 1,
                    sequence,
                    asset: next_asset.clone(),
                    visited,
                    path,
                });
                expansions += 1;
            }
        }
        completed.sort_by_key(|candidate| std::cmp::Reverse(candidate.output));
        completed
    }

    async fn cost_path(
        &self,
        source: &str,
        destination: &str,
        amount: Decimal,
        path: &[Step],
        apply_bnb_discount: bool,
    ) -> Result<Option<RouteQuote>> {
        let source = source.trim().to_uppercase();
        let destination = destination.trim().to_uppercase();
        let mut current_asset = source.clone();
        let mut current_amount = amount;
        let mut legs = Vec::with_capacity(path.len());
        for step in path {
            let market = &self.markets[step.market];
            let Some((gross_output, spent)) = gross_leg(
                market,
                step.side,
                current_amount,
                self.config.slippage_buffer_bps,
                true,
            ) else {
                return Ok(None);
            };
            let commission = self.fees.get(&market.symbol).await?;
            let fee_rate = commission.taker_rate(step.side, apply_bnb_discount);
            let fee_amount = gross_output * fee_rate;
            let net_output = gross_output - fee_amount;
            if net_output <= Decimal::ZERO {
                return Ok(None);
            }
            legs.push(RouteLeg {
                symbol: market.symbol.clone(),
                side: step.side,
                source_asset: current_asset.clone(),
                destination_asset: step.next_asset.clone(),
                input_amount: current_amount,
                spent_amount: spent,
                gross_output_amount: gross_output,
                fee_rate,
                fee_output_amount: fee_amount,
                net_output_amount: net_output,
                commission_discount_asset: if commission.discount_enabled && apply_bnb_discount {
                    commission.discount_asset.clone()
                } else {
                    String::new()
                },
            });
            current_asset.clone_from(&step.next_asset);
            current_amount = net_output;
        }
        Ok(Some(RouteQuote {
            source_asset: source,
            destination_asset: destination,
            input_amount: amount,
            output_amount: current_amount,
            legs,
            slippage_bps: self.config.slippage_buffer_bps,
            generated_at: bb_core::time::now(),
        }))
    }
}

/// Simulate one market order: returns `(gross_output, spent)`.
///
/// Sells round the base input down to the lot step and receive `base * bid * (1 - slip)`;
/// buys spend quote at `ask * (1 + slip)` for a step-rounded base amount. With
/// `enforce_order_rules`, results below the market's minimum amount or notional are `None`.
pub fn gross_leg(
    market: &MarketSnapshot,
    side: LegSide,
    input_amount: Decimal,
    slippage_bps: Decimal,
    enforce_order_rules: bool,
) -> Option<(Decimal, Decimal)> {
    let slip = slippage_bps / BPS;
    let step = market.order_step();
    let round = |value: Decimal| match (enforce_order_rules, step) {
        (true, Some(step)) => floor_to_step(value, step),
        _ => value,
    };
    match side {
        LegSide::Sell => {
            let base_amount = round(input_amount);
            let price = market.bid * (Decimal::ONE - slip);
            let notional = base_amount.checked_mul(price)?;
            if enforce_order_rules
                && (base_amount < market.min_amount || notional < market.min_notional)
            {
                return None;
            }
            Some((notional, base_amount))
        }
        LegSide::Buy => {
            let price = market.ask * (Decimal::ONE + slip);
            let base_amount = round(input_amount.checked_div(price)?);
            let spent = base_amount.checked_mul(price)?;
            if enforce_order_rules
                && (base_amount < market.min_amount || spent < market.min_notional)
            {
                return None;
            }
            Some((base_amount, spent))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use parking_lot::Mutex;

    use async_trait::async_trait;
    use bb_binance::spot::{AccountCommission, CommissionDiscount, CommissionRates};
    use rust_decimal_macros::dec;

    use super::*;
    use crate::fees::estimate_bnb_reserve;
    use crate::source::CommissionSource;

    struct FakeCommission {
        taker: Decimal,
        discount: bool,
        calls: Mutex<Vec<String>>,
    }

    impl FakeCommission {
        fn new(taker: Decimal, discount: bool) -> Arc<Self> {
            Arc::new(Self {
                taker,
                discount,
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl CommissionSource for FakeCommission {
        async fn account_commission(&self, symbol: &str) -> Result<AccountCommission> {
            self.calls.lock().push(symbol.to_owned());
            Ok(AccountCommission {
                symbol: symbol.to_owned(),
                standard_commission: CommissionRates {
                    maker: dec!(0.001),
                    taker: self.taker,
                    buyer: Decimal::ZERO,
                    seller: Decimal::ZERO,
                },
                tax_commission: CommissionRates::default(),
                special_commission: None,
                discount: Some(CommissionDiscount {
                    enabled_for_account: self.discount,
                    enabled_for_symbol: self.discount,
                    discount_asset: "BNB".into(),
                    discount: dec!(0.75),
                }),
            })
        }
    }

    fn snapshot(symbol: &str, bid: Decimal, ask: Decimal) -> MarketSnapshot {
        let (base, quote) = symbol.split_once('/').unwrap();
        MarketSnapshot {
            symbol: symbol.into(),
            base: base.into(),
            quote: quote.into(),
            bid,
            ask,
            last: bid,
            quote_volume: dec!(1000000),
            quote_volume_usdt: Some(dec!(1000000)),
            min_amount: dec!(0.00001),
            min_notional: dec!(5),
            amount_precision: Some(dec!(0.00001)),
            timestamp_ms: Some(0),
            spread_bps: Some(dec!(1)),
            eligible: true,
            amount_step: None,
            rejection_reasons: Vec::new(),
        }
    }

    fn config(max_route_legs: usize, max_candidate_routes: usize) -> RouteConfig {
        RouteConfig {
            max_route_legs,
            max_route_expansions: 10_000,
            max_candidate_routes,
            slippage_buffer_bps: Decimal::ZERO,
        }
    }

    fn engine(
        markets: &[MarketSnapshot],
        source: Arc<FakeCommission>,
        config: RouteConfig,
    ) -> RouteEngine {
        RouteEngine::new(
            markets,
            Arc::new(CommissionService::new(source, 60.0)),
            config,
        )
    }

    #[tokio::test]
    async fn costs_multileg_route_with_dynamic_discounted_fees() {
        let source = FakeCommission::new(dec!(0.001), true);
        let markets = [
            snapshot("BNB/USDT", dec!(600), dec!(600.1)),
            snapshot("ETH/USDT", dec!(3000), dec!(3000.1)),
        ];
        let engine = engine(&markets, source.clone(), config(3, 8));

        let quote = engine
            .quote("BNB", "ETH", dec!(0.02), true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(quote.symbols(), ["BNB/USDT", "ETH/USDT"]);
        assert_eq!(quote.legs[0].fee_rate, dec!(0.00075));
        assert_eq!(quote.legs[0].commission_discount_asset, "BNB");
        assert!(quote.output_amount < dec!(0.02) * dec!(600) / dec!(3000.1));
        let mut calls = source.calls.lock().clone();
        calls.sort();
        calls.dedup();
        assert_eq!(calls, ["BNB/USDT", "ETH/USDT"]);
        assert!(
            engine
                .quote("BNB", "ETH", dec!(0.001), true)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn always_costs_direct_path_before_choosing_multihop() {
        let markets = [
            snapshot("BNB/ETH", dec!(100.0), dec!(100.1)),
            snapshot("BNB/USDT", dec!(102.0), dec!(102.1)),
            snapshot("ETH/USDT", dec!(1.0), dec!(1.0)),
        ];
        let engine = engine(
            &markets,
            FakeCommission::new(dec!(0.10), false),
            config(3, 1),
        );
        let quote = engine
            .quote("BNB", "ETH", dec!(10), true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(quote.symbols(), ["BNB/ETH"]);
    }

    #[tokio::test]
    async fn uses_lot_step_over_precision_for_market_amounts() {
        let mut hbar = snapshot("HBAR/BNB", dec!(0.000132), dec!(0.0001325));
        hbar.min_amount = dec!(1);
        hbar.min_notional = dec!(0.000001);
        hbar.amount_precision = Some(dec!(0.1));
        hbar.amount_step = Some(dec!(1));
        let engine = engine(
            &[hbar],
            FakeCommission::new(Decimal::ZERO, false),
            config(1, 8),
        );
        let quote = engine
            .quote("BNB", "HBAR", dec!(0.017013), true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(quote.legs[0].gross_output_amount, dec!(128));
        assert_eq!(quote.legs[0].spent_amount, dec!(128) * dec!(0.0001325));
    }

    #[tokio::test]
    async fn identity_route_and_indicative_rates() {
        let markets = [snapshot("BNB/USDT", dec!(600), dec!(600.1))];
        let engine = engine(
            &markets,
            FakeCommission::new(dec!(0.001), true),
            config(3, 8),
        );
        assert_eq!(
            engine.indicative_rate("USDT", "BNB"),
            Some(Decimal::ONE / dec!(600.1))
        );
        assert_eq!(engine.indicative_rate("bnb", "USDT"), Some(dec!(600)));
        assert_eq!(engine.gross_quote("BNB", "USDT", dec!(0.001)), None);
        let identity = engine
            .quote("BNB", "bnb", dec!(1), true)
            .await
            .unwrap()
            .unwrap();
        assert!(identity.legs.is_empty());
        assert_eq!(identity.output_amount, dec!(1));
        assert_eq!(
            estimate_bnb_reserve([], &HashMap::new(), dec!(3), dec!(0.00005)),
            dec!(0.00005)
        );
    }

    #[test]
    fn gross_leg_applies_slippage_and_minimums() {
        let market = snapshot("BNB/USDT", dec!(600), dec!(600.1));
        let (notional, base) =
            gross_leg(&market, LegSide::Sell, dec!(0.0123456), dec!(25), true).unwrap();
        assert_eq!(base, dec!(0.01234));
        assert_eq!(notional, dec!(0.01234) * dec!(600) * dec!(0.9975));
        assert!(gross_leg(&market, LegSide::Buy, dec!(4), Decimal::ZERO, true).is_none());
        assert!(gross_leg(&market, LegSide::Buy, dec!(4), Decimal::ZERO, false).is_some());
    }
}

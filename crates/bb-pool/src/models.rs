//! Domain models for market discovery, routing, and paper portfolios.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Direction of one simulated market conversion. Persisted lowercase (`buy`/`sell`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegSide {
    /// Spend the quote asset to receive the base asset (pay the ask).
    Buy,
    /// Spend the base asset to receive the quote asset (hit the bid).
    Sell,
}

impl LegSide {
    /// Lowercase spelling used in the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }

    /// The equivalent Binance order side.
    pub fn order_side(self) -> bb_binance::Side {
        match self {
            Self::Buy => bb_binance::Side::Buy,
            Self::Sell => bb_binance::Side::Sell,
        }
    }
}

impl fmt::Display for LegSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One current Binance spot-market edge and its eligibility decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSnapshot {
    /// Unified symbol (`BTC/USDT`).
    pub symbol: String,
    pub base: String,
    pub quote: String,
    /// Best bid (zero when the ticker had none).
    pub bid: Decimal,
    /// Best ask (zero when the ticker had none).
    pub ask: Decimal,
    pub last: Decimal,
    /// 24h quote-asset volume.
    pub quote_volume: Decimal,
    /// 24h quote volume valued in USDT (`None` when no USDT path exists).
    pub quote_volume_usdt: Option<Decimal>,
    /// LOT_SIZE `minQty` (zero when missing).
    pub min_amount: Decimal,
    /// NOTIONAL / MIN_NOTIONAL `minNotional` (zero when missing).
    pub min_notional: Decimal,
    /// Amount precision expressed as a step (ccxt `precision.amount`).
    pub amount_precision: Option<Decimal>,
    /// Ticker timestamp (ms).
    pub timestamp_ms: Option<i64>,
    /// `(ask - bid) / mid * 10_000`; `None` (Python `inf`) when bid or ask is missing.
    pub spread_bps: Option<Decimal>,
    pub eligible: bool,
    /// Largest positive LOT_SIZE / MARKET_LOT_SIZE `stepSize`.
    pub amount_step: Option<Decimal>,
    pub rejection_reasons: Vec<String>,
}

impl MarketSnapshot {
    /// Step used to round market-order amounts: the lot step, else the precision step.
    pub fn order_step(&self) -> Option<Decimal> {
        self.amount_step.or(self.amount_precision)
    }
}

/// Authenticated commission components for one Binance symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionSnapshot {
    pub symbol: String,
    pub maker_standard: Decimal,
    pub taker_standard: Decimal,
    pub buyer_standard: Decimal,
    pub seller_standard: Decimal,
    pub maker_tax: Decimal,
    pub taker_tax: Decimal,
    pub buyer_tax: Decimal,
    pub seller_tax: Decimal,
    pub maker_special: Decimal,
    pub taker_special: Decimal,
    pub buyer_special: Decimal,
    pub seller_special: Decimal,
    pub discount_enabled: bool,
    /// Asset that pays discounted commissions (`BNB`), upper-case; empty when unknown.
    pub discount_asset: String,
    /// Multiplier applied to the standard component when the discount is enabled.
    pub discount_multiplier: Decimal,
    pub retrieved_at: DateTime<Utc>,
}

impl CommissionSnapshot {
    /// Flat schedule with only standard maker/taker components (tests, fixtures).
    pub fn flat(symbol: impl Into<String>, maker: Decimal, taker: Decimal) -> Self {
        Self {
            symbol: symbol.into(),
            maker_standard: maker,
            taker_standard: taker,
            buyer_standard: Decimal::ZERO,
            seller_standard: Decimal::ZERO,
            maker_tax: Decimal::ZERO,
            taker_tax: Decimal::ZERO,
            buyer_tax: Decimal::ZERO,
            seller_tax: Decimal::ZERO,
            maker_special: Decimal::ZERO,
            taker_special: Decimal::ZERO,
            buyer_special: Decimal::ZERO,
            seller_special: Decimal::ZERO,
            discount_enabled: false,
            discount_asset: String::new(),
            discount_multiplier: Decimal::ONE,
            retrieved_at: bb_core::time::now(),
        }
    }

    /// Conservative market-order commission fraction:
    /// `standard(taker + side) [* discount] + tax(taker + side) + special(taker + side)`.
    pub fn taker_rate(&self, side: LegSide, apply_discount: bool) -> Decimal {
        let is_buy = side == LegSide::Buy;
        let mut standard = self.taker_standard
            + if is_buy {
                self.buyer_standard
            } else {
                self.seller_standard
            };
        let tax = self.taker_tax
            + if is_buy {
                self.buyer_tax
            } else {
                self.seller_tax
            };
        let special = self.taker_special
            + if is_buy {
                self.buyer_special
            } else {
                self.seller_special
            };
        if apply_discount && self.discount_enabled {
            standard *= self.discount_multiplier;
        }
        standard + tax + special
    }
}

/// One simulated market conversion in an asset-to-asset route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteLeg {
    pub symbol: String,
    pub side: LegSide,
    pub source_asset: String,
    pub destination_asset: String,
    /// Amount of `source_asset` entering the leg.
    pub input_amount: Decimal,
    /// Amount actually spent after lot rounding (base for sells, quote for buys).
    pub spent_amount: Decimal,
    pub gross_output_amount: Decimal,
    pub fee_rate: Decimal,
    pub fee_output_amount: Decimal,
    pub net_output_amount: Decimal,
    /// `BNB` when the leg's commission is modeled as discounted (paid in BNB).
    pub commission_discount_asset: String,
}

/// Costed candidate conversion path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteQuote {
    pub source_asset: String,
    pub destination_asset: String,
    pub input_amount: Decimal,
    pub output_amount: Decimal,
    pub legs: Vec<RouteLeg>,
    pub slippage_bps: Decimal,
    pub generated_at: DateTime<Utc>,
}

impl RouteQuote {
    /// Symbols traversed, in order.
    pub fn symbols(&self) -> Vec<&str> {
        self.legs.iter().map(|leg| leg.symbol.as_str()).collect()
    }
}

/// Value of a holding measured in one configured benchmark asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetValuation {
    pub asset: String,
    pub amount: Decimal,
    pub benchmark: String,
    pub benchmark_amount: Option<Decimal>,
    pub route: Option<RouteQuote>,
}

/// Paper holdings owned by one allocation/scoring experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualPortfolio {
    pub portfolio_id: String,
    pub scorer_name: String,
    pub balances: BTreeMap<String, Decimal>,
    pub seeded_at: DateTime<Utc>,
}

/// Outcome of one scorer decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionAction {
    Hold,
    Rebalance,
}

impl DecisionAction {
    /// Ledger spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Rebalance => "rebalance",
        }
    }
}

impl fmt::Display for DecisionAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A scorer's proposed or rejected paper reallocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperDecision {
    pub portfolio_id: String,
    pub scorer_name: String,
    pub source_asset: Option<String>,
    pub destination_asset: Option<String>,
    pub input_amount: Option<Decimal>,
    pub route: Option<RouteQuote>,
    pub action: DecisionAction,
    pub reason: String,
    pub score: Option<f64>,
    pub decided_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn taker_rate_adds_side_tax_special_and_discounts_standard_only() {
        let mut snapshot = CommissionSnapshot::flat("BNB/USDT", dec!(0.001), dec!(0.001));
        snapshot.buyer_standard = dec!(0.0001);
        snapshot.taker_tax = dec!(0.0002);
        snapshot.seller_special = dec!(0.0003);
        snapshot.discount_enabled = true;
        snapshot.discount_multiplier = dec!(0.75);
        assert_eq!(
            snapshot.taker_rate(LegSide::Buy, true),
            dec!(0.0011) * dec!(0.75) + dec!(0.0002)
        );
        assert_eq!(
            snapshot.taker_rate(LegSide::Sell, false),
            dec!(0.001) + dec!(0.0002) + dec!(0.0003)
        );
    }
}

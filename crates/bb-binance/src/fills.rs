//! Execution summaries from FULL order responses or `myTrades`.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::futures::FuturesTrade;
use crate::spot::{MyTrade, Order, OrderFill};

/// Aggregated execution of one order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillSummary {
    /// Total base quantity executed.
    pub executed_qty: Decimal,
    /// Total quote quantity (Σ price × qty, or `cummulativeQuoteQty`).
    pub quote_qty: Decimal,
    /// Volume-weighted average price (`None` when nothing executed).
    pub avg_price: Option<Decimal>,
    /// Commission paid per asset.
    pub commissions: BTreeMap<String, Decimal>,
    /// Number of fills aggregated.
    pub fill_count: usize,
}

impl FillSummary {
    fn finish(mut self) -> Self {
        self.avg_price =
            (self.executed_qty > Decimal::ZERO).then(|| self.quote_qty / self.executed_qty);
        self
    }

    fn add(&mut self, price: Decimal, qty: Decimal, commission: Decimal, asset: &str) {
        self.executed_qty += qty;
        self.quote_qty += price * qty;
        if !asset.is_empty() && !commission.is_zero() {
            *self.commissions.entry(asset.to_owned()).or_default() += commission;
        }
        self.fill_count += 1;
    }

    /// Summarise explicit fills.
    pub fn from_fills(fills: &[OrderFill]) -> Self {
        let mut summary = Self::default();
        for fill in fills {
            summary.add(
                fill.price,
                fill.qty,
                fill.commission,
                &fill.commission_asset,
            );
        }
        summary.finish()
    }

    /// Summarise an order: its FULL-response `fills` when present, otherwise
    /// `executedQty` / `cummulativeQuoteQty` (no commission information).
    pub fn from_order(order: &Order) -> Self {
        if !order.fills.is_empty() {
            return Self::from_fills(&order.fills);
        }
        Self {
            executed_qty: order.executed_qty,
            quote_qty: order.cummulative_quote_qty,
            ..Self::default()
        }
        .finish()
    }

    /// Summarise spot `myTrades` rows (optionally only those of `order_id`).
    pub fn from_trades(trades: &[MyTrade], order_id: Option<i64>) -> Self {
        let mut summary = Self::default();
        for t in trades
            .iter()
            .filter(|t| order_id.is_none_or(|id| t.order_id == id))
        {
            summary.executed_qty += t.qty;
            summary.quote_qty += t.quote_qty;
            if !t.commission_asset.is_empty() && !t.commission.is_zero() {
                *summary
                    .commissions
                    .entry(t.commission_asset.clone())
                    .or_default() += t.commission;
            }
            summary.fill_count += 1;
        }
        summary.finish()
    }

    /// Summarise futures `userTrades` rows (optionally only those of `order_id`).
    pub fn from_futures_trades(trades: &[FuturesTrade], order_id: Option<i64>) -> Self {
        let mut summary = Self::default();
        for t in trades
            .iter()
            .filter(|t| order_id.is_none_or(|id| t.order_id == id))
        {
            summary.executed_qty += t.qty;
            summary.quote_qty += t.quote_qty;
            if !t.commission_asset.is_empty() && !t.commission.is_zero() {
                *summary
                    .commissions
                    .entry(t.commission_asset.clone())
                    .or_default() += t.commission;
            }
            summary.fill_count += 1;
        }
        summary.finish()
    }

    /// Commission paid in `asset` (zero when none).
    pub fn commission_in(&self, asset: &str) -> Decimal {
        self.commissions.get(asset).copied().unwrap_or_default()
    }

    /// Base quantity actually received on a BUY (executed minus base-asset commission).
    pub fn net_base_received(&self, base_asset: &str) -> Decimal {
        self.executed_qty - self.commission_in(base_asset)
    }

    /// Quote actually received on a SELL (quote minus quote-asset commission).
    pub fn net_quote_received(&self, quote_asset: &str) -> Decimal {
        self.quote_qty - self.commission_in(quote_asset)
    }

    /// Quote actually spent on a BUY (quote plus quote-asset commission).
    pub fn gross_quote_spent(&self, quote_asset: &str) -> Decimal {
        self.quote_qty + self.commission_in(quote_asset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn fill(price: Decimal, qty: Decimal, commission: Decimal, asset: &str) -> OrderFill {
        OrderFill {
            price,
            qty,
            commission,
            commission_asset: asset.into(),
            trade_id: None,
        }
    }

    #[test]
    fn full_response_fills_are_volume_weighted() {
        let summary = FillSummary::from_fills(&[
            fill(dec!(4000), dec!(1), dec!(4), "USDT"),
            fill(dec!(3999), dec!(5), dec!(19.995), "USDT"),
            fill(dec!(3998), dec!(2), dec!(0.0001), "BNB"),
        ]);
        assert_eq!(summary.executed_qty, dec!(8));
        assert_eq!(summary.quote_qty, dec!(31991));
        assert_eq!(summary.avg_price, Some(dec!(3998.875)));
        assert_eq!(summary.commission_in("USDT"), dec!(23.995));
        assert_eq!(summary.commission_in("BNB"), dec!(0.0001));
        assert_eq!(summary.net_quote_received("USDT"), dec!(31967.005));
        assert_eq!(summary.fill_count, 3);
    }

    #[test]
    fn buy_net_base_and_order_fallback() {
        let summary = FillSummary::from_fills(&[fill(dec!(100), dec!(0.06), dec!(0.00006), "ETH")]);
        assert_eq!(summary.net_base_received("ETH"), dec!(0.05994));
        assert_eq!(summary.gross_quote_spent("USDT"), dec!(6));

        let order = Order {
            executed_qty: dec!(2),
            cummulative_quote_qty: dec!(9),
            ..Order::default()
        };
        let from_order = FillSummary::from_order(&order);
        assert_eq!(from_order.avg_price, Some(dec!(4.5)));
        assert!(from_order.commissions.is_empty());
        assert_eq!(FillSummary::from_fills(&[]).avg_price, None);
    }

    #[test]
    fn my_trades_filter_by_order() {
        let trade = |order_id, qty, quote| MyTrade {
            order_id,
            qty,
            quote_qty: quote,
            commission: dec!(0.1),
            commission_asset: "USDT".into(),
            ..MyTrade::default()
        };
        let trades = [
            trade(1, dec!(1), dec!(10)),
            trade(2, dec!(5), dec!(60)),
            trade(1, dec!(1), dec!(12)),
        ];
        let summary = FillSummary::from_trades(&trades, Some(1));
        assert_eq!(summary.executed_qty, dec!(2));
        assert_eq!(summary.avg_price, Some(dec!(11)));
        assert_eq!(summary.commission_in("USDT"), dec!(0.2));
    }
}

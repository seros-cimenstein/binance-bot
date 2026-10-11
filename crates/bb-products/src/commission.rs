//! Authenticated per-symbol commission schedule (port of the parts of
//! `src/pool/fees.CommissionService` / `CommissionSnapshot` the canaries use).

use bb_binance::spot::AccountCommission;
use bb_binance::{BinanceClient, BinanceError};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;

/// Commission components for one Binance symbol (fractions, e.g. `0.001`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    pub discount_asset: String,
    pub discount_multiplier: Decimal,
    pub retrieved_at: DateTime<Utc>,
}

impl CommissionSnapshot {
    /// Flat schedule, used by tests and as a building block.
    pub fn flat(symbol: &str, taker: Decimal) -> Self {
        Self {
            symbol: symbol.to_owned(),
            maker_standard: taker,
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

    /// Port of `_parse_commission`: negative rates are treated as zero and the
    /// discount multiplier only applies when enabled for account and symbol.
    pub fn from_account_commission(
        payload: &AccountCommission,
        fallback_symbol: &str,
        retrieved_at: DateTime<Utc>,
    ) -> Self {
        let standard = &payload.standard_commission;
        let tax = &payload.tax_commission;
        let special = payload.special_commission.clone().unwrap_or_default();
        let discount = payload.discount.clone().unwrap_or_default();
        let enabled = discount.enabled_for_account && discount.enabled_for_symbol;
        let multiplier = if enabled && discount.discount >= Decimal::ZERO {
            discount.discount
        } else {
            Decimal::ONE
        };
        let symbol = if payload.symbol.is_empty() {
            fallback_symbol.to_owned()
        } else {
            payload.symbol.clone()
        };
        Self {
            symbol,
            maker_standard: rate(standard.maker),
            taker_standard: rate(standard.taker),
            buyer_standard: rate(standard.buyer),
            seller_standard: rate(standard.seller),
            maker_tax: rate(tax.maker),
            taker_tax: rate(tax.taker),
            buyer_tax: rate(tax.buyer),
            seller_tax: rate(tax.seller),
            maker_special: rate(special.maker),
            taker_special: rate(special.taker),
            buyer_special: rate(special.buyer),
            seller_special: rate(special.seller),
            discount_enabled: enabled,
            discount_asset: discount.discount_asset.to_uppercase(),
            discount_multiplier: multiplier,
            retrieved_at,
        }
    }

    /// Conservative market-order commission fraction for `side` (`buy`/`sell`).
    pub fn taker_rate(&self, side: &str, apply_discount: bool) -> Decimal {
        let is_buy = side.eq_ignore_ascii_case("buy");
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

fn rate(value: Decimal) -> Decimal {
    value.max(Decimal::ZERO)
}

/// Read the symbol's commission schedule (`GET /api/v3/account/commission`).
pub async fn fetch_commission(
    client: &BinanceClient,
    symbol: &str,
) -> Result<CommissionSnapshot, BinanceError> {
    let payload = client.account_commission(symbol).await?;
    Ok(CommissionSnapshot::from_account_commission(
        &payload,
        &symbol.trim().to_uppercase(),
        bb_core::time::now(),
    ))
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use bb_binance::spot::{CommissionDiscount, CommissionRates};

    #[test]
    fn taker_rate_applies_discount_to_standard_only() {
        let payload = AccountCommission {
            symbol: "BNBUSDT".into(),
            standard_commission: CommissionRates {
                maker: dec!(0.001),
                taker: dec!(0.001),
                buyer: dec!(0),
                seller: dec!(0.0001),
            },
            tax_commission: CommissionRates {
                maker: dec!(0),
                taker: dec!(0.0002),
                buyer: dec!(0),
                seller: dec!(0),
            },
            special_commission: None,
            discount: Some(CommissionDiscount {
                enabled_for_account: true,
                enabled_for_symbol: true,
                discount_asset: "bnb".into(),
                discount: dec!(0.75),
            }),
        };
        let snapshot =
            CommissionSnapshot::from_account_commission(&payload, "X", bb_core::time::now());
        assert_eq!(snapshot.discount_asset, "BNB");
        assert_eq!(
            snapshot.taker_rate("sell", true),
            dec!(0.0011) * dec!(0.75) + dec!(0.0002)
        );
        assert_eq!(snapshot.taker_rate("buy", false), dec!(0.0012));
    }
}

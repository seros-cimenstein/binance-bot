//! Read-only Binance commission retrieval and dynamic BNB reserve estimates.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use bb_binance::spot::{AccountCommission, CommissionRates};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::error::Result;
use crate::models::{CommissionSnapshot, RouteQuote};
use crate::source::CommissionSource;

/// Caches authenticated per-symbol commission schedules for paper costing.
pub struct CommissionService {
    source: Arc<dyn CommissionSource>,
    cache_ttl: chrono::Duration,
    cache: Mutex<HashMap<String, CommissionSnapshot>>,
}

impl std::fmt::Debug for CommissionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommissionService")
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl CommissionService {
    /// Service reading from `source`, caching each symbol for `cache_seconds`.
    pub fn new(source: Arc<dyn CommissionSource>, cache_seconds: f64) -> Self {
        Self {
            source,
            cache_ttl: seconds(cache_seconds),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Commission schedule for `symbol`; re-read when the cached copy is older than the TTL.
    pub async fn get(&self, symbol: &str) -> Result<CommissionSnapshot> {
        let normalized = symbol.trim().to_uppercase();
        let now = bb_core::time::now();
        if let Some(cached) = self.cached(&normalized)
            && now - cached.retrieved_at <= self.cache_ttl
        {
            return Ok(cached);
        }
        let payload = self.source.account_commission(symbol).await?;
        let snapshot = parse_commission(&payload, &normalized, now);
        self.cache.lock().insert(normalized, snapshot.clone());
        Ok(snapshot)
    }

    /// Seed the cache (tests, fixtures, schedules obtained elsewhere).
    pub fn insert(&self, snapshot: CommissionSnapshot) {
        self.cache
            .lock()
            .insert(snapshot.symbol.trim().to_uppercase(), snapshot);
    }

    fn cached(&self, symbol: &str) -> Option<CommissionSnapshot> {
        self.cache.lock().get(symbol).cloned()
    }
}

/// Fractional seconds as a chrono duration (microsecond resolution, like `timedelta`).
pub(crate) fn seconds(value: f64) -> chrono::Duration {
    #[allow(clippy::cast_possible_truncation)]
    let micros = (value * 1_000_000.0).round() as i64;
    chrono::Duration::microseconds(micros)
}

/// Fee reserve (in BNB) needed to pay every BNB-discounted simulated leg, scaled by
/// `multiplier` and never below `floor_bnb`.
pub fn estimate_bnb_reserve<'a>(
    routes: impl IntoIterator<Item = &'a RouteQuote>,
    destination_to_bnb: &HashMap<String, Decimal>,
    multiplier: Decimal,
    floor_bnb: Decimal,
) -> Decimal {
    let mut required = Decimal::ZERO;
    for route in routes {
        for leg in &route.legs {
            if leg.commission_discount_asset != "BNB" {
                continue;
            }
            let Some(rate) = destination_to_bnb.get(&leg.destination_asset) else {
                continue;
            };
            if *rate <= Decimal::ZERO {
                continue;
            }
            required += leg.fee_output_amount * rate;
        }
    }
    floor_bnb.max(required * multiplier)
}

/// Normalize `GET /api/v3/account/commission` into a [`CommissionSnapshot`].
/// Negative components count as zero; the discount multiplier applies only when enabled
/// for both account and symbol and is positive (otherwise `1`).
pub fn parse_commission(
    payload: &AccountCommission,
    fallback_symbol: &str,
    retrieved_at: DateTime<Utc>,
) -> CommissionSnapshot {
    let rate = |value: Decimal| value.max(Decimal::ZERO);
    let standard = &payload.standard_commission;
    let tax = &payload.tax_commission;
    let zero = CommissionRates::default();
    let special = payload.special_commission.as_ref().unwrap_or(&zero);
    let discount = payload.discount.clone().unwrap_or_default();
    let enabled = discount.enabled_for_account && discount.enabled_for_symbol;
    let multiplier = if enabled && discount.discount > Decimal::ZERO {
        discount.discount
    } else {
        Decimal::ONE
    };
    let symbol = if payload.symbol.is_empty() {
        fallback_symbol.to_owned()
    } else {
        payload.symbol.clone()
    };
    CommissionSnapshot {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use bb_binance::spot::CommissionDiscount;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::models::LegSide;

    struct Counting(AtomicUsize);

    #[async_trait]
    impl CommissionSource for Counting {
        async fn account_commission(&self, symbol: &str) -> Result<AccountCommission> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(AccountCommission {
                symbol: bb_binance::exchange_id(symbol)?,
                standard_commission: CommissionRates {
                    maker: dec!(0.001),
                    taker: dec!(0.001),
                    buyer: Decimal::ZERO,
                    seller: dec!(-1),
                },
                tax_commission: CommissionRates::default(),
                special_commission: None,
                discount: Some(CommissionDiscount {
                    enabled_for_account: true,
                    enabled_for_symbol: true,
                    discount_asset: "bnb".into(),
                    discount: dec!(0.75),
                }),
            })
        }
    }

    #[tokio::test]
    async fn caches_within_ttl_and_parses_discount() {
        let source = Arc::new(Counting(AtomicUsize::new(0)));
        let service = CommissionService::new(source.clone(), 60.0);
        let first = service.get("BNB/USDT").await.unwrap();
        let second = service.get(" bnb/usdt ").await.unwrap();
        assert_eq!(source.0.load(Ordering::SeqCst), 1);
        assert_eq!(first, second);
        assert_eq!(first.symbol, "BNBUSDT");
        assert_eq!(first.discount_asset, "BNB");
        assert_eq!(first.seller_standard, Decimal::ZERO);
        assert_eq!(first.taker_rate(LegSide::Sell, true), dec!(0.00075));

        let uncached = CommissionService::new(source.clone(), 0.0);
        uncached.get("BNB/USDT").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        uncached.get("BNB/USDT").await.unwrap();
        assert_eq!(source.0.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn reserve_uses_floor_without_discounted_legs() {
        assert_eq!(
            estimate_bnb_reserve([], &HashMap::new(), dec!(3), dec!(0.00005)),
            dec!(0.00005)
        );
    }
}

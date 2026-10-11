//! Quote funding for buy orders (refactor `src/account/auto_conversion.py` + the balance snapshot of
//! `dynamic_balance.py`): when free quote cannot fund a buy, market-sell one allowed asset into the
//! quote. Quantities respect `LOT_SIZE`/`MARKET_LOT_SIZE` and min notional, and every conversion
//! passes Binance `order/test` before (optionally) being submitted.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bb_binance::spot::{NewOrder, Order};
use bb_binance::{BinanceClient, OrderCheck, OrderType, Side, SymbolRules};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::AutoConvertConfig;
use crate::error::{CapitalError, Result};

/// Treat smaller shortfalls as funded (the refactor's `1e-12`).
const DUST: Decimal = Decimal::from_parts(1, 0, 0, false, 12);

/// The exchange calls auto-convert needs. Implemented by [`BinanceClient`].
#[async_trait]
pub trait ConvertExchange: Send + Sync {
    /// Spot balances `(asset, free, locked)`.
    async fn spot_balances(&self) -> Result<Vec<(String, Decimal, Decimal)>>;
    /// Last price per exchange symbol id (`BNBUSDT`).
    async fn last_prices(&self) -> Result<BTreeMap<String, Decimal>>;
    async fn rules(&self, symbol: &str) -> Result<SymbolRules>;
    /// `POST /api/v3/order/test`.
    async fn test_order(&self, order: &NewOrder) -> Result<()>;
    /// `POST /api/v3/order` (LIVE).
    async fn create_order(&self, order: &NewOrder) -> Result<Order>;
}

#[async_trait]
impl ConvertExchange for BinanceClient {
    async fn spot_balances(&self) -> Result<Vec<(String, Decimal, Decimal)>> {
        let account = self.account().await?;
        Ok(account
            .balances
            .into_iter()
            .map(|b| (b.asset.to_uppercase(), b.free, b.locked))
            .collect())
    }

    async fn last_prices(&self) -> Result<BTreeMap<String, Decimal>> {
        Ok(self
            .prices()
            .await?
            .into_iter()
            .map(|p| (p.symbol, p.price))
            .collect())
    }

    async fn rules(&self, symbol: &str) -> Result<SymbolRules> {
        Ok(self.symbol_rules(symbol).await?)
    }

    async fn test_order(&self, order: &NewOrder) -> Result<()> {
        Ok(BinanceClient::test_order(self, order).await?)
    }

    async fn create_order(&self, order: &NewOrder) -> Result<Order> {
        Ok(BinanceClient::create_order(self, order).await?)
    }
}

/// Whether a planned conversion is only checked or also submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertMode {
    /// `order/test` only.
    PlanOnly,
    /// `order/test`, then a real MARKET sell.
    Live,
}

/// A buy order that may need quote funding.
#[derive(Debug, Clone, PartialEq)]
pub struct FundingRequest {
    /// Target order symbol (`BTC/USDT`); its base asset is never sold to fund it.
    pub symbol: Option<String>,
    pub side: String,
    pub quote_asset: String,
    pub requested_notional: Decimal,
    /// Quote that must stay free after the buy.
    pub min_quote_balance: Decimal,
    /// Target order metadata, echoed in the conversion audit.
    pub metadata: Value,
}

impl FundingRequest {
    /// From an order (`amount` base units at `price`); the quote comes from the symbol.
    pub fn for_order(symbol: &str, side: &str, amount: Decimal, price: Decimal) -> Result<Self> {
        if amount <= Decimal::ZERO {
            return Err(CapitalError::invalid(
                "requested_amount must be greater than zero",
            ));
        }
        if price <= Decimal::ZERO {
            return Err(CapitalError::invalid("price must be greater than zero"));
        }
        let unified = bb_binance::normalize_symbol(symbol)?;
        let quote = unified
            .rsplit_once('/')
            .map(|(_, q)| q.to_owned())
            .unwrap_or_default();
        if quote.is_empty() {
            return Err(CapitalError::invalid(format!(
                "cannot infer the quote asset of {symbol}"
            )));
        }
        Ok(Self {
            symbol: Some(unified),
            side: side.to_owned(),
            quote_asset: quote,
            requested_notional: amount * price,
            min_quote_balance: Decimal::ZERO,
            metadata: Value::Null,
        })
    }

    fn base_asset(&self) -> Option<String> {
        self.symbol
            .as_deref()
            .and_then(|s| s.split_once('/'))
            .map(|(base, _)| base.trim().to_uppercase())
            .filter(|b| !b.is_empty())
    }
}

/// An asset that can be sold into the quote.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConversionCandidate {
    pub asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub free: Decimal,
    pub quote_asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub quote_value: Decimal,
    pub conversion_symbol: String,
}

/// Free quote plus sellable assets valued in the quote, largest first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteSnapshot {
    pub quote_asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub free_quote: Decimal,
    pub candidates: Vec<ConversionCandidate>,
}

impl QuoteSnapshot {
    /// Value every asset with a free balance at its direct (`ASSETQUOTE`) or inverse price.
    pub fn build(
        quote_asset: &str,
        balances: &[(String, Decimal, Decimal)],
        prices: &BTreeMap<String, Decimal>,
    ) -> Self {
        let quote = quote_asset.trim().to_uppercase();
        let price_of = |asset: &str| -> Option<Decimal> {
            if let Some(p) = prices
                .get(&format!("{asset}{quote}"))
                .filter(|p| **p > Decimal::ZERO)
            {
                return Some(*p);
            }
            prices
                .get(&format!("{quote}{asset}"))
                .filter(|p| **p > Decimal::ZERO)
                .map(|p| Decimal::ONE / *p)
        };
        let mut free_quote = Decimal::ZERO;
        let mut candidates = Vec::new();
        for (asset, free, _) in balances {
            let asset = asset.trim().to_uppercase();
            if asset == quote {
                free_quote = *free;
                continue;
            }
            if *free <= Decimal::ZERO {
                continue;
            }
            let Some(price) = price_of(&asset) else {
                continue;
            };
            let quote_value = *free * price;
            if quote_value > Decimal::ZERO {
                candidates.push(ConversionCandidate {
                    conversion_symbol: format!("{asset}/{quote}"),
                    asset,
                    free: *free,
                    quote_asset: quote.clone(),
                    quote_value,
                });
            }
        }
        candidates.sort_by_key(|c| std::cmp::Reverse(c.quote_value));
        Self {
            quote_asset: quote,
            free_quote,
            candidates,
        }
    }
}

/// Outcome of one funding check (the refactor `AutoConversionResult` plus plan/skip detail).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutoConversionResult {
    pub attempted: bool,
    pub executed: bool,
    /// `disabled`, `not_buy_order`, `direct_quote_sufficient`, `no_allowed_conversion_candidate`,
    /// `order_test_passed` (plan-only) or `converted`.
    pub reason: String,
    pub quote_asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub needed_quote_value: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub converted_quote_value: Decimal,
    pub asset: Option<String>,
    pub conversion_symbol: Option<String>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub sold_amount: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub quote_price: Option<Decimal>,
    pub order_id: Option<String>,
    /// Candidates passed over and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedCandidate>,
}

/// A sellable asset that could not fund the buy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedCandidate {
    pub asset: String,
    pub reason: String,
}

impl SkippedCandidate {
    fn new(asset: &str, reason: String) -> Self {
        Self {
            asset: asset.to_owned(),
            reason,
        }
    }
}

impl AutoConversionResult {
    fn skipped(reason: &str, quote_asset: &str) -> Self {
        Self {
            attempted: false,
            executed: false,
            reason: reason.into(),
            quote_asset: quote_asset.into(),
            needed_quote_value: Decimal::ZERO,
            converted_quote_value: Decimal::ZERO,
            asset: None,
            conversion_symbol: None,
            sold_amount: Decimal::ZERO,
            quote_price: None,
            order_id: None,
            skipped: Vec::new(),
        }
    }
}

/// Convert enough approved asset value to fund a quote-denominated buy.
pub async fn ensure_quote_for_order(
    exchange: &dyn ConvertExchange,
    config: &AutoConvertConfig,
    request: &FundingRequest,
    mode: ConvertMode,
) -> Result<AutoConversionResult> {
    config.validate()?;
    let quote = if request.quote_asset.trim().is_empty() {
        config.quote_asset.trim().to_uppercase()
    } else {
        request.quote_asset.trim().to_uppercase()
    };
    if !config.enabled {
        return Ok(AutoConversionResult::skipped("disabled", &quote));
    }
    if !request.side.trim().eq_ignore_ascii_case("buy") {
        return Ok(AutoConversionResult::skipped("not_buy_order", &quote));
    }
    if request.requested_notional <= Decimal::ZERO {
        return Err(CapitalError::invalid(
            "requested notional must be greater than zero",
        ));
    }
    if request.min_quote_balance < Decimal::ZERO {
        return Err(CapitalError::invalid(
            "min_quote_balance must be greater than or equal to zero",
        ));
    }

    let balances = exchange.spot_balances().await?;
    let prices = exchange.last_prices().await?;
    let snapshot = QuoteSnapshot::build(&quote, &balances, &prices);
    let direct_budget = (snapshot.free_quote - request.min_quote_balance).max(Decimal::ZERO);
    let needed = (request.requested_notional - direct_budget).max(Decimal::ZERO);
    if needed <= DUST {
        return Ok(AutoConversionResult::skipped(
            "direct_quote_sufficient",
            &quote,
        ));
    }

    let allowed: Vec<String> = config
        .allowed_assets
        .iter()
        .map(|a| a.trim().to_uppercase())
        .collect();
    let base = request.base_asset();
    let buffer = Decimal::ONE + config.buffer_pct / Decimal::ONE_HUNDRED;
    let mut skipped = Vec::new();
    for candidate in &snapshot.candidates {
        if !allowed.is_empty() && !allowed.contains(&candidate.asset) {
            continue;
        }
        if base.as_deref() == Some(candidate.asset.as_str()) || candidate.free <= Decimal::ZERO {
            continue;
        }
        let price = candidate.quote_value / candidate.free;
        if price <= Decimal::ZERO {
            continue;
        }
        let mut target = needed * buffer;
        if config.min_quote_value > Decimal::ZERO {
            target = target.max(config.min_quote_value);
        }
        let quote_to_convert = target
            .min(config.max_quote_value)
            .min(candidate.quote_value);
        if (config.min_quote_value > Decimal::ZERO
            && quote_to_convert + DUST < config.min_quote_value)
            || quote_to_convert <= Decimal::ZERO
        {
            skipped.push(SkippedCandidate::new(
                &candidate.asset,
                format!(
                    "{} {quote} below the minimum conversion",
                    quote_to_convert.round_dp(8).normalize()
                ),
            ));
            continue;
        }

        let rules = match exchange.rules(&candidate.conversion_symbol).await {
            Ok(rules) => rules,
            Err(error) => {
                skipped.push(SkippedCandidate::new(
                    &candidate.asset,
                    format!("{}: {error}", candidate.conversion_symbol),
                ));
                continue;
            }
        };
        let quantity = rules.round_market_qty(candidate.free.min(quote_to_convert / price));
        let violations = rules.validate(&OrderCheck {
            side: Side::Sell,
            order_type: OrderType::Market,
            quantity: Some(quantity),
            quote_quantity: None,
            price: None,
            reference_price: Some(price),
        });
        if quantity <= Decimal::ZERO || !violations.is_empty() {
            let detail: Vec<String> = violations.iter().map(ToString::to_string).collect();
            skipped.push(SkippedCandidate::new(
                &candidate.asset,
                format!(
                    "sell {quantity} {}: {}",
                    candidate.conversion_symbol,
                    detail.join("; ")
                ),
            ));
            continue;
        }

        let order = NewOrder::market(&candidate.conversion_symbol, Side::Sell, quantity);
        exchange.test_order(&order).await?;
        let mut result = AutoConversionResult {
            attempted: true,
            executed: false,
            reason: "order_test_passed".into(),
            quote_asset: quote.clone(),
            needed_quote_value: needed,
            converted_quote_value: quote_to_convert,
            asset: Some(candidate.asset.clone()),
            conversion_symbol: Some(candidate.conversion_symbol.clone()),
            sold_amount: quantity,
            quote_price: Some(price),
            order_id: None,
            skipped,
        };
        if mode == ConvertMode::Live {
            let placed = exchange.create_order(&order).await?;
            result.executed = true;
            result.reason = "converted".into();
            result.order_id = Some(placed.order_id.to_string());
            if placed.cummulative_quote_qty > Decimal::ZERO {
                result.converted_quote_value = placed.cummulative_quote_qty;
            }
            if placed.executed_qty > Decimal::ZERO {
                result.sold_amount = placed.executed_qty;
            }
        }
        return Ok(result);
    }

    Ok(AutoConversionResult {
        attempted: true,
        reason: "no_allowed_conversion_candidate".into(),
        needed_quote_value: needed,
        skipped,
        ..AutoConversionResult::skipped("", &quote)
    })
}

/// Audit row for one funding check.
pub fn audit_row(
    request: &FundingRequest,
    result: &AutoConversionResult,
    mode: ConvertMode,
) -> Value {
    json!({
        "timestamp": bb_core::time::iso(bb_core::time::now()),
        "mode": match mode { ConvertMode::PlanOnly => "plan_only", ConvertMode::Live => "live" },
        "target_symbol": request.symbol,
        "requested_notional": crate::scorecard::decimal_value(request.requested_notional),
        "min_quote_balance": crate::scorecard::decimal_value(request.min_quote_balance),
        "target_metadata": request.metadata,
        "result": result,
    })
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use bb_binance::spot::SpotSymbolInfo;
    use rust_decimal_macros::dec;

    use super::*;

    struct FakeExchange {
        balances: Mutex<BTreeMap<String, Decimal>>,
        prices: BTreeMap<String, Decimal>,
        tested: Mutex<Vec<NewOrder>>,
        placed: Mutex<Vec<NewOrder>>,
    }

    impl FakeExchange {
        fn new(usdt: Decimal, bnb: Decimal) -> Self {
            Self {
                balances: Mutex::new(BTreeMap::from([("USDT".into(), usdt), ("BNB".into(), bnb)])),
                prices: BTreeMap::from([
                    ("BNBUSDT".into(), dec!(300)),
                    ("BTCUSDT".into(), dec!(100000)),
                ]),
                tested: Mutex::new(Vec::new()),
                placed: Mutex::new(Vec::new()),
            }
        }
    }

    fn rules(symbol: &str, base: &str) -> SymbolRules {
        let info: SpotSymbolInfo = serde_json::from_value(json!({
            "symbol": symbol, "status": "TRADING", "baseAsset": base, "quoteAsset": "USDT",
            "orderTypes": ["MARKET", "LIMIT"], "isSpotTradingAllowed": true,
            "filters": [
                {"filterType": "LOT_SIZE", "minQty": "0.001", "maxQty": "9000", "stepSize": "0.001"},
                {"filterType": "NOTIONAL", "minNotional": "5", "applyMinToMarket": true, "maxNotional": "9000000",
                 "applyMaxToMarket": false, "avgPriceMins": 5}
            ]
        }))
        .unwrap();
        SymbolRules::from_spot(&info)
    }

    #[async_trait]
    impl ConvertExchange for FakeExchange {
        async fn spot_balances(&self) -> Result<Vec<(String, Decimal, Decimal)>> {
            Ok(self
                .balances
                .lock()
                .iter()
                .map(|(a, f)| (a.clone(), *f, Decimal::ZERO))
                .collect())
        }
        async fn last_prices(&self) -> Result<BTreeMap<String, Decimal>> {
            Ok(self.prices.clone())
        }
        async fn rules(&self, symbol: &str) -> Result<SymbolRules> {
            match symbol {
                "BNB/USDT" => Ok(rules("BNBUSDT", "BNB")),
                _ => Err(CapitalError::invalid(format!("{symbol} is not listed"))),
            }
        }
        async fn test_order(&self, order: &NewOrder) -> Result<()> {
            self.tested.lock().push(order.clone());
            Ok(())
        }
        async fn create_order(&self, order: &NewOrder) -> Result<Order> {
            let qty = order.quantity.unwrap_or_default();
            let mut balances = self.balances.lock();
            *balances.entry("BNB".into()).or_default() -= qty;
            *balances.entry("USDT".into()).or_default() += qty * dec!(300);
            self.placed.lock().push(order.clone());
            Ok(Order {
                order_id: 42,
                executed_qty: qty,
                cummulative_quote_qty: qty * dec!(300),
                ..Order::default()
            })
        }
    }

    fn config() -> AutoConvertConfig {
        AutoConvertConfig {
            enabled: true,
            ..AutoConvertConfig::default()
        }
    }

    #[tokio::test]
    async fn auto_conversion_sells_allowed_asset_to_fund_quote() {
        let exchange = FakeExchange::new(Decimal::ZERO, dec!(1));
        let request =
            FundingRequest::for_order("BTC/USDT", "buy", dec!(0.001), dec!(100000)).unwrap();
        let result = ensure_quote_for_order(&exchange, &config(), &request, ConvertMode::Live)
            .await
            .unwrap();
        assert!(result.executed);
        assert_eq!(result.reason, "converted");
        assert_eq!(result.asset.as_deref(), Some("BNB"));
        assert_eq!(result.conversion_symbol.as_deref(), Some("BNB/USDT"));
        // min(100 * 1.01, max 100) / 300 = 0.3333.. floored to the 0.001 step.
        assert_eq!(result.sold_amount, dec!(0.333));
        assert_eq!(result.order_id.as_deref(), Some("42"));
        assert_eq!(exchange.tested.lock().len(), 1);
        let placed = exchange.placed.lock();
        assert_eq!(placed[0].symbol, "BNB/USDT");
        assert_eq!(placed[0].side, Side::Sell);
        assert_eq!(exchange.balances.lock()["USDT"], dec!(99.9));
    }

    #[tokio::test]
    async fn plan_only_runs_order_test_without_submitting() {
        let exchange = FakeExchange::new(dec!(1), dec!(1));
        let mut request =
            FundingRequest::for_order("BTC/USDT", "BUY", dec!(0.00006), dec!(100000)).unwrap();
        request.min_quote_balance = dec!(1);
        let result = ensure_quote_for_order(&exchange, &config(), &request, ConvertMode::PlanOnly)
            .await
            .unwrap();
        assert_eq!(result.reason, "order_test_passed");
        assert!(result.attempted && !result.executed);
        assert_eq!(result.needed_quote_value, dec!(6));
        // max(6 * 1.01, 5) = 6.06 USDT → 0.0202 BNB → 0.020 after LOT_SIZE.
        assert_eq!(result.sold_amount, dec!(0.020));
        assert_eq!(exchange.tested.lock().len(), 1);
        assert!(exchange.placed.lock().is_empty());
    }

    #[tokio::test]
    async fn auto_conversion_does_not_sell_base_asset_to_buy_same_asset() {
        let exchange = FakeExchange::new(Decimal::ZERO, dec!(1));
        let request = FundingRequest::for_order("BNB/USDT", "buy", dec!(0.1), dec!(300)).unwrap();
        let result = ensure_quote_for_order(&exchange, &config(), &request, ConvertMode::Live)
            .await
            .unwrap();
        assert!(result.attempted && !result.executed);
        assert_eq!(result.reason, "no_allowed_conversion_candidate");
        assert!(exchange.placed.lock().is_empty());
    }

    #[tokio::test]
    async fn skips_disabled_sells_funded_and_too_small_conversions() {
        let exchange = FakeExchange::new(dec!(50), dec!(0.01));
        let request =
            FundingRequest::for_order("BTC/USDT", "buy", dec!(0.0001), dec!(100000)).unwrap();
        let disabled = AutoConvertConfig::default();
        assert_eq!(
            ensure_quote_for_order(&exchange, &disabled, &request, ConvertMode::Live)
                .await
                .unwrap()
                .reason,
            "disabled"
        );
        let sell = FundingRequest {
            side: "sell".into(),
            ..request.clone()
        };
        assert_eq!(
            ensure_quote_for_order(&exchange, &config(), &sell, ConvertMode::Live)
                .await
                .unwrap()
                .reason,
            "not_buy_order"
        );
        assert_eq!(
            ensure_quote_for_order(&exchange, &config(), &request, ConvertMode::Live)
                .await
                .unwrap()
                .reason,
            "direct_quote_sufficient"
        );
        // 0.01 BNB = 3 USDT < the 5 USDT minimum conversion.
        let big = FundingRequest::for_order("BTC/USDT", "buy", dec!(0.001), dec!(100000)).unwrap();
        let result = ensure_quote_for_order(&exchange, &config(), &big, ConvertMode::Live)
            .await
            .unwrap();
        assert_eq!(result.reason, "no_allowed_conversion_candidate");
        assert_eq!(result.needed_quote_value, dec!(50));
        assert_eq!(result.skipped.len(), 1);
        assert!(exchange.tested.lock().is_empty());
    }

    #[tokio::test]
    async fn lot_and_notional_filters_reject_unsellable_amounts() {
        // 0.0175 BNB × 300 = 5.25 USDT but LOT_SIZE floors to 0.017 → 5.1 notional ≥ 5 passes;
        // with 0.0166 BNB the floor (0.016 → 4.8) breaks NOTIONAL and the candidate is skipped.
        let exchange = FakeExchange::new(Decimal::ZERO, dec!(0.0166));
        let mut cfg = config();
        cfg.min_quote_value = dec!(4);
        let request =
            FundingRequest::for_order("BTC/USDT", "buy", dec!(0.0001), dec!(100000)).unwrap();
        let result = ensure_quote_for_order(&exchange, &cfg, &request, ConvertMode::PlanOnly)
            .await
            .unwrap();
        assert_eq!(result.reason, "no_allowed_conversion_candidate");
        assert!(result.skipped[0].reason.contains("BNB/USDT"));
        assert!(exchange.tested.lock().is_empty());
    }

    #[test]
    fn snapshot_values_assets_directly_or_inversely_and_sorts_by_value() {
        let prices = BTreeMap::from([
            ("ETHUSDT".to_owned(), dec!(2000)),
            ("USDTTRY".to_owned(), dec!(40)),
        ]);
        let balances = vec![
            ("USDT".to_owned(), dec!(3), Decimal::ZERO),
            ("TRY".to_owned(), dec!(400), Decimal::ZERO),
            ("ETH".to_owned(), dec!(0.01), Decimal::ZERO),
            ("XYZ".to_owned(), dec!(5), Decimal::ZERO),
        ];
        let snapshot = QuoteSnapshot::build("usdt", &balances, &prices);
        assert_eq!(snapshot.free_quote, dec!(3));
        let assets: Vec<&str> = snapshot
            .candidates
            .iter()
            .map(|c| c.asset.as_str())
            .collect();
        assert_eq!(assets, ["ETH", "TRY"]);
        assert_eq!(snapshot.candidates[1].quote_value, dec!(10));
    }
}

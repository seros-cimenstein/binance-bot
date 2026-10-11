//! Exchange boundary used by the runtime (the subset of the Python
//! `BinanceClient` the order manager, sizer, portfolio service and ops-check
//! call), plus the production [`BinanceExchange`] adapter.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bb_binance::spot::{NewOrder, Order as BinanceOrder, OrderRef, OrderStatus as BinanceStatus};
use bb_binance::{BinanceClient, BinanceError, Side};
use bb_core::Decimal;

use crate::orders::models::{OrderSide, OrderType};
use crate::orders::payload::{FillPayload, OrderPayload, datetime_from_ms};

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error(transparent)]
    Binance(#[from] BinanceError),
    #[error("{0}")]
    Other(String),
}

/// Order request forwarded to the exchange (already validated by the manager).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeOrderRequest {
    pub symbol: String,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub amount: Decimal,
    pub price: Option<Decimal>,
}

/// Account balances grouped like ccxt (`free` / `used` / `total`), upper-case assets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Balances {
    pub free: BTreeMap<String, Decimal>,
    pub used: BTreeMap<String, Decimal>,
    pub total: BTreeMap<String, Decimal>,
}

/// How base quantities are rounded down for a market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountPrecision {
    /// ccxt `DECIMAL_PLACES` precision.
    Places(u32),
    /// Step size (Binance `LOT_SIZE.stepSize`).
    Step(Decimal),
}

/// Exchange precision and minimum-size constraints for one market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MarketRules {
    pub min_amount: Decimal,
    pub min_notional: Decimal,
    pub amount_precision: Option<AmountPrecision>,
}

#[async_trait]
pub trait Exchange: Send + Sync {
    /// Submit an order (LIVE on a real exchange).
    async fn create_order(
        &self,
        request: &ExchangeOrderRequest,
    ) -> Result<OrderPayload, ExchangeError>;

    async fn cancel_order(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> Result<OrderPayload, ExchangeError>;

    async fn fetch_order(
        &self,
        order_id: &str,
        symbol: Option<&str>,
    ) -> Result<OrderPayload, ExchangeError>;

    /// Account balances (`get_balance()`).
    async fn balances(&self) -> Result<Balances, ExchangeError>;

    /// Last traded price for ops-check (`fetch_ticker(...)["last"]`).
    async fn last_price(&self, symbol: &str) -> Result<Option<Decimal>, ExchangeError>;

    /// Market rules for sizing; `None` when the market is unknown.
    async fn market_rules(&self, _symbol: &str) -> Option<MarketRules> {
        None
    }

    /// Release resources (`rest_client.close()`).
    async fn close(&self) {}
}

/// Production adapter over the native Binance spot client.
#[derive(Debug, Clone)]
pub struct BinanceExchange {
    client: BinanceClient,
}

impl BinanceExchange {
    pub fn new(client: BinanceClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &BinanceClient {
        &self.client
    }
}

#[async_trait]
impl Exchange for BinanceExchange {
    async fn create_order(
        &self,
        request: &ExchangeOrderRequest,
    ) -> Result<OrderPayload, ExchangeError> {
        let side = match request.side {
            OrderSide::Buy => Side::Buy,
            OrderSide::Sell => Side::Sell,
        };
        // ccxt applied amount/price precision inside create_order.
        let rules = self.client.symbol_rules(&request.symbol).await?;
        let new_order = match (request.order_type, request.price) {
            (OrderType::Limit, Some(price)) => NewOrder::limit(
                &request.symbol,
                side,
                rules.round_qty(request.amount),
                rules.round_price(price),
            ),
            (OrderType::Limit, None) => {
                return Err(ExchangeError::Other(
                    "price is required for limit orders".into(),
                ));
            }
            (OrderType::Market, _) => NewOrder::market(
                &request.symbol,
                side,
                rules.round_market_qty(request.amount),
            ),
        };
        let order = self.client.create_order(&new_order).await?;
        Ok(binance_order_payload(&order, &request.symbol))
    }

    async fn cancel_order(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> Result<OrderPayload, ExchangeError> {
        let order = self
            .client
            .cancel_order(symbol, &order_ref(order_id))
            .await?;
        Ok(binance_order_payload(&order, symbol))
    }

    async fn fetch_order(
        &self,
        order_id: &str,
        symbol: Option<&str>,
    ) -> Result<OrderPayload, ExchangeError> {
        let Some(symbol) = symbol else {
            return Err(ExchangeError::Other(
                "fetch_order requires a symbol on Binance".into(),
            ));
        };
        let order = self
            .client
            .query_order(symbol, &order_ref(order_id))
            .await?;
        Ok(binance_order_payload(&order, symbol))
    }

    async fn balances(&self) -> Result<Balances, ExchangeError> {
        let account = self.client.account().await?;
        let mut balances = Balances::default();
        for row in &account.balances {
            let asset = row.asset.trim().to_uppercase();
            balances.free.insert(asset.clone(), row.free);
            balances.used.insert(asset.clone(), row.locked);
            balances.total.insert(asset, row.total());
        }
        Ok(balances)
    }

    async fn last_price(&self, symbol: &str) -> Result<Option<Decimal>, ExchangeError> {
        let ticker = self.client.ticker_24h(symbol).await?;
        Ok(Some(ticker.last_price).filter(|p| *p > Decimal::ZERO))
    }

    async fn market_rules(&self, symbol: &str) -> Option<MarketRules> {
        let rules = self.client.symbol_rules(symbol).await.ok()?;
        let step = rules.step_size();
        Some(MarketRules {
            min_amount: rules.min_qty().unwrap_or_default(),
            min_notional: rules.min_notional().unwrap_or_default(),
            amount_precision: (step > Decimal::ZERO).then_some(AmountPrecision::Step(step)),
        })
    }
}

/// Numeric ids are exchange order ids; anything else is a client order id.
fn order_ref(order_id: &str) -> OrderRef {
    order_id
        .parse::<i64>()
        .map_or_else(|_| OrderRef::ClientId(order_id.to_owned()), OrderRef::Id)
}

/// Typed Binance order → manager payload, keeping the caller's (unified)
/// symbol spelling as ccxt did.
pub fn binance_order_payload(order: &BinanceOrder, symbol: &str) -> OrderPayload {
    let average = order.average_price();
    let timestamp = order
        .transact_time
        .or(order.time)
        .and_then(datetime_from_ms);
    let fills = order
        .fills
        .iter()
        .map(|fill| FillPayload {
            id: fill.trade_id.map(|id| id.to_string()),
            symbol: None,
            side: None,
            price: Some(fill.price),
            amount: Some(fill.qty),
            fee: fill.commission,
            fee_currency: fill.commission_asset.clone(),
            timestamp,
        })
        .collect();
    OrderPayload {
        id: Some(order.order_id.to_string()),
        symbol: Some(symbol.to_owned()),
        order_type: Some(order.order_type.to_lowercase()).filter(|t| !t.is_empty()),
        side: Some(order.side.to_lowercase()).filter(|s| !s.is_empty()),
        amount: Some(order.orig_qty).filter(|q| *q > Decimal::ZERO),
        // ccxt reports the average as the price of market orders.
        price: Some(order.price).filter(|p| *p > Decimal::ZERO).or(average),
        filled: Some(order.executed_qty),
        remaining: None,
        average,
        cost: Some(order.cummulative_quote_qty).filter(|c| !c.is_zero()),
        status: (order.status != BinanceStatus::Unknown).then(|| order.status.ccxt().to_owned()),
        timestamp,
        fills,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bb_binance::spot::OrderFill;
    use rust_decimal_macros::dec;

    #[test]
    fn binance_market_order_maps_to_ccxt_like_payload() {
        let order = BinanceOrder {
            symbol: "BTCUSDT".into(),
            order_id: 2_320_471,
            transact_time: Some(1_700_000_000_000),
            price: dec!(0),
            orig_qty: dec!(0.001),
            executed_qty: dec!(0.001),
            cummulative_quote_qty: dec!(82.39684),
            status: BinanceStatus::Filled,
            order_type: "MARKET".into(),
            side: "BUY".into(),
            fills: vec![OrderFill {
                price: dec!(82396.84),
                qty: dec!(0.001),
                commission: dec!(0),
                commission_asset: "BTC".into(),
                trade_id: Some(907_115),
            }],
            ..BinanceOrder::default()
        };
        let payload = binance_order_payload(&order, "BTC/USDT");
        assert_eq!(payload.id.as_deref(), Some("2320471"));
        assert_eq!(payload.symbol.as_deref(), Some("BTC/USDT"));
        assert_eq!(payload.status.as_deref(), Some("closed"));
        assert_eq!(payload.price, Some(dec!(82396.84)));
        assert_eq!(payload.average, Some(dec!(82396.84)));
        assert_eq!(payload.fills[0].id.as_deref(), Some("907115"));
        assert_eq!(order_ref("123"), OrderRef::Id(123));
        assert_eq!(order_ref("local-x"), OrderRef::ClientId("local-x".into()));
    }
}

//! Shared test doubles: a scripted/auto-filling exchange and a capturing
//! alert transport.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use bb_core::Decimal;
use bb_runtime::alerts::{AlertTransport, TelegramAlerter};
use bb_runtime::exchange::{Balances, Exchange, ExchangeError, ExchangeOrderRequest, MarketRules};
use bb_runtime::orders::OrderPayload;
use bb_runtime::orders::models::OrderStatus;
use parking_lot::Mutex;
use rust_decimal_macros::dec;
use serde_json::{Value, json};

#[derive(Default)]
pub struct FakeExchange {
    pub create_payloads: Mutex<VecDeque<Value>>,
    pub cancel_payloads: Mutex<VecDeque<Value>>,
    pub fetch_payloads: Mutex<VecDeque<Value>>,
    pub create_calls: Mutex<Vec<ExchangeOrderRequest>>,
    pub cancel_calls: Mutex<Vec<(String, String)>>,
    pub fetch_calls: Mutex<Vec<(String, Option<String>)>>,
    /// Orders filled automatically (when no scripted payload is queued).
    pub filled: Mutex<Vec<Value>>,
    pub balances: Mutex<Balances>,
    pub market_rules: Mutex<Option<MarketRules>>,
    pub closed: Mutex<bool>,
    counter: Mutex<u64>,
}

impl FakeExchange {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn with_usdt(free: Decimal) -> Arc<Self> {
        let exchange = Self::default();
        {
            let mut balances = exchange.balances.lock();
            balances.free.insert("USDT".into(), free);
            balances.used.insert("USDT".into(), Decimal::ZERO);
            balances.total.insert("USDT".into(), free);
        }
        Arc::new(exchange)
    }

    pub fn sides(&self) -> Vec<String> {
        self.filled
            .lock()
            .iter()
            .map(|o| o["side"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

#[async_trait]
impl Exchange for FakeExchange {
    async fn create_order(
        &self,
        request: &ExchangeOrderRequest,
    ) -> Result<OrderPayload, ExchangeError> {
        self.create_calls.lock().push(request.clone());
        if let Some(payload) = self.create_payloads.lock().pop_front() {
            return Ok(OrderPayload::from_json(&payload));
        }
        let id = {
            let mut counter = self.counter.lock();
            *counter += 1;
            *counter
        };
        // test_pipeline.py FakeExchange: fill at the limit price or 82,000.
        let price = request.price.unwrap_or(dec!(82000));
        let order = json!({
            "id": id.to_string(),
            "symbol": request.symbol,
            "type": request.order_type.as_str(),
            "side": request.side.as_str(),
            "amount": request.amount.to_string(),
            "price": price.to_string(),
            "status": OrderStatus::Closed.as_str(),
            "filled": request.amount.to_string(),
            "remaining": "0",
            "average": price.to_string(),
        });
        self.filled.lock().push(order.clone());
        Ok(OrderPayload::from_json(&order))
    }

    async fn cancel_order(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> Result<OrderPayload, ExchangeError> {
        self.cancel_calls
            .lock()
            .push((order_id.to_owned(), symbol.to_owned()));
        let payload = self
            .cancel_payloads
            .lock()
            .pop_front()
            .unwrap_or(Value::Null);
        Ok(OrderPayload::from_json(&payload))
    }

    async fn fetch_order(
        &self,
        order_id: &str,
        symbol: Option<&str>,
    ) -> Result<OrderPayload, ExchangeError> {
        self.fetch_calls
            .lock()
            .push((order_id.to_owned(), symbol.map(str::to_owned)));
        let payload = self
            .fetch_payloads
            .lock()
            .pop_front()
            .unwrap_or(Value::Null);
        Ok(OrderPayload::from_json(&payload))
    }

    async fn balances(&self) -> Result<Balances, ExchangeError> {
        Ok(self.balances.lock().clone())
    }

    async fn last_price(&self, _symbol: &str) -> Result<Option<Decimal>, ExchangeError> {
        Ok(Some(dec!(82000)))
    }

    async fn market_rules(&self, _symbol: &str) -> Option<MarketRules> {
        *self.market_rules.lock()
    }

    async fn close(&self) {
        *self.closed.lock() = true;
    }
}

/// Records every alert message instead of sending it.
#[derive(Default)]
pub struct CapturingTransport {
    pub messages: Mutex<Vec<String>>,
    pub deliver: bool,
}

impl CapturingTransport {
    pub fn alerter() -> (Arc<Self>, Arc<TelegramAlerter>) {
        let transport = Arc::new(Self {
            messages: Mutex::new(Vec::new()),
            deliver: true,
        });
        let alerter = Arc::new(TelegramAlerter::with_transport(transport.clone()));
        (transport, alerter)
    }

    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().clone()
    }
}

#[async_trait]
impl AlertTransport for CapturingTransport {
    async fn send(&self, html: &str) -> bool {
        self.messages.lock().push(html.to_owned());
        self.deliver
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Port of `tests/test_execution_router.py` and `tests/test_ops_check.py`.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bb_binance::ws::ExecutionReport;
use bb_core::Decimal;
use bb_runtime::alerts::TelegramAlerter;
use bb_runtime::config::Settings;
use bb_runtime::execution::{ExecutionUpdateRouter, execution_report_to_order_payload};
use bb_runtime::ops_check::{OperationsCheckError, run_checks};
use bb_runtime::orders::OrderManager;
use bb_runtime::orders::models::{OrderSide, OrderStatus};
use bb_runtime::strategies::{Candle, FillData, OrderClient, Strategy, StrategyError, TickData};
use parking_lot::Mutex;
use rust_decimal_macros::dec;
use support::{CapturingTransport, FakeExchange};

fn report(trade_id: i64, last_qty: Decimal, cumulative: Decimal, status: &str) -> ExecutionReport {
    ExecutionReport {
        event_time: 1_700_000_000_500,
        symbol: "BTCUSDT".into(),
        client_order_id: "client-1".into(),
        side: "BUY".into(),
        order_type: "LIMIT".into(),
        time_in_force: Some("GTC".into()),
        order_quantity: Some(dec!(1)),
        order_price: Some(dec!(100)),
        stop_price: None,
        iceberg_quantity: None,
        order_list_id: None,
        original_client_order_id: None,
        execution_type: "TRADE".into(),
        order_status: status.into(),
        reject_reason: None,
        order_id: 42,
        last_executed_quantity: Some(last_qty),
        cumulative_filled_quantity: Some(cumulative),
        last_executed_price: Some(dec!(100)),
        commission_amount: Some(dec!(0.001)),
        commission_asset: Some("BNB".into()),
        trade_time: Some(1_700_000_000_400),
        trade_id: Some(trade_id),
        order_creation_time: Some(1_700_000_000_000),
        quote_order_quantity: None,
        last_quote_transacted: None,
        cumulative_quote_transacted: Some(cumulative * dec!(100)),
        is_maker: Some(false),
        is_buyer: None,
        raw: serde_json::Value::Null,
    }
}

#[test]
fn execution_report_to_order_payload_matches_order_manager_shape() {
    let payload =
        execution_report_to_order_payload(&report(7, dec!(0.4), dec!(0.4), "PARTIALLY_FILLED"));
    assert_eq!(payload.id.as_deref(), Some("42"));
    assert_eq!(payload.symbol.as_deref(), Some("BTC/USDT"));
    assert_eq!(payload.order_type.as_deref(), Some("limit"));
    assert_eq!(payload.side.as_deref(), Some("buy"));
    assert_eq!(payload.status.as_deref(), Some("partially_filled"));
    assert_eq!(
        (payload.amount, payload.filled, payload.remaining),
        (Some(dec!(1)), Some(dec!(0.4)), Some(dec!(0.6)))
    );
    assert_eq!(payload.average, Some(dec!(100)));
    assert_eq!(
        payload.timestamp.map(|t| t.and_utc().timestamp_millis()),
        Some(1_700_000_000_000)
    );
    let fill = &payload.fills[0];
    assert_eq!(fill.id.as_deref(), Some("7"));
    assert_eq!(
        (fill.amount, fill.price, fill.fee),
        (Some(dec!(0.4)), Some(dec!(100)), dec!(0.001))
    );
    assert_eq!(fill.fee_currency, "BNB");

    let no_trade = execution_report_to_order_payload(&ExecutionReport {
        trade_id: Some(-1),
        ..report(-1, dec!(0.1), dec!(0.1), "PARTIALLY_FILLED")
    });
    assert_eq!(no_trade.fills[0].id.as_deref(), Some("42:1700000000500"));
}

struct RecordingStrategy {
    client: Arc<dyn OrderClient>,
    fills: Mutex<Vec<FillData>>,
}

#[async_trait]
impl Strategy for RecordingStrategy {
    fn name(&self) -> &str {
        "recording"
    }
    fn client(&self) -> &Arc<dyn OrderClient> {
        &self.client
    }
    fn is_running(&self) -> bool {
        true
    }
    async fn start(&self) {}
    async fn stop(&self) {}
    async fn on_tick(&self, _tick: TickData) {}
    async fn on_candle(&self, _candle: &Candle) -> Result<(), StrategyError> {
        Ok(())
    }
    async fn on_fill(&self, fill: FillData) {
        self.fills.lock().push(fill);
    }
    async fn on_error(&self, _error: &StrategyError) {}
}

fn router(
    symbol: &str,
) -> (
    Arc<OrderManager>,
    Arc<RecordingStrategy>,
    ExecutionUpdateRouter,
) {
    let manager = Arc::new(OrderManager::new(FakeExchange::new(), None));
    let client = Arc::new(bb_runtime::runtime::StrategyOrderClient::new(
        manager.clone(),
    ));
    let strategy = Arc::new(RecordingStrategy {
        client,
        fills: Mutex::new(Vec::new()),
    });
    let strategies: HashMap<String, Arc<dyn Strategy>> =
        HashMap::from([(symbol.to_owned(), strategy.clone() as Arc<dyn Strategy>)]);
    let router = ExecutionUpdateRouter::new(manager.clone(), strategies);
    (manager, strategy, router)
}

#[tokio::test]
async fn router_applies_order_update_and_notifies_strategy_once_per_fill() {
    let (manager, strategy, router) = router("BTC/USDT");
    let order = router
        .route(&report(7, dec!(0.4), dec!(0.4), "PARTIALLY_FILLED"))
        .await
        .unwrap();
    assert_eq!(order.status, OrderStatus::Open);
    assert_eq!(order.symbol, "BTC/USDT");
    router
        .route(&report(7, dec!(0.4), dec!(0.4), "PARTIALLY_FILLED"))
        .await
        .unwrap();
    let closed = router
        .route(&report(8, dec!(0.6), dec!(1), "FILLED"))
        .await
        .unwrap();
    assert_eq!(closed.status, OrderStatus::Closed);

    let fills = strategy.fills.lock().clone();
    assert_eq!(fills.len(), 2, "duplicate fills are not re-notified");
    assert_eq!(fills[0].order_id, "42");
    assert_eq!(fills[0].side, OrderSide::Buy);
    assert_eq!(
        (fills[0].quantity, fills[0].price, fills[0].commission),
        (dec!(0.4), dec!(100), dec!(0.001))
    );
    assert_eq!(fills[0].timestamp, 1_700_000_000_400);
    let position = manager.get_position("BTC/USDT").unwrap().unwrap();
    assert_eq!(
        (position.amount, position.entry_price),
        (dec!(1), dec!(100))
    );
}

#[tokio::test]
async fn router_ignores_unmatched_strategy_symbol() {
    let (manager, strategy, router) = router("ETH/USDT");
    router
        .route(&report(7, dec!(0.4), dec!(0.4), "PARTIALLY_FILLED"))
        .await
        .unwrap();
    assert!(strategy.fills.lock().is_empty());
    assert_eq!(manager.list_orders(None).unwrap().len(), 1);
}

// ------------------------------------------------------------------ ops

fn settings() -> Settings {
    Settings::new("key", "secret").unwrap()
}

#[tokio::test]
async fn ops_check_is_read_only_and_can_confirm_telegram_delivery() {
    let exchange = FakeExchange::new();
    let (transport, alerter) = CapturingTransport::alerter();
    let lines = run_checks(&settings(), exchange.as_ref(), Some(&alerter))
        .await
        .unwrap();
    assert_eq!(
        lines,
        vec![
            "Mode: testnet",
            "Exchange ticker OK: BTC/USDT last=82000",
            "Account access OK: balance request succeeded",
            "Telegram delivery OK: verification message sent",
        ]
    );
    assert!(exchange.create_calls.lock().is_empty());
    assert!(*exchange.closed.lock());
    assert!(transport.messages()[0].contains("Operational verification"));
}

#[tokio::test]
async fn ops_check_fails_when_telegram_verification_is_not_delivered() {
    let exchange = FakeExchange::new();
    let alerter = TelegramAlerter::disabled();
    let err = run_checks(&settings(), exchange.as_ref(), Some(&alerter))
        .await
        .unwrap_err();
    assert!(matches!(err, OperationsCheckError::TelegramDelivery));
    assert!(*exchange.closed.lock());
}

#[tokio::test]
async fn ops_check_rejects_unconfirmed_live_configuration() {
    let exchange = FakeExchange::new();
    let live = Settings {
        use_testnet: false,
        ..settings()
    };
    let err = run_checks(&live, exchange.as_ref(), None)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "live configuration requires CONFIRM_LIVE_TRADING=yes"
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Port of `tests/test_order_manager.py` and `tests/test_db.py` round trips.

mod support;

use std::sync::Arc;

use bb_core::Decimal;
use bb_runtime::db::OrderDb;
use bb_runtime::orders::models::{Fill, Order, OrderSide, OrderStatus, OrderType, utc_now};
use bb_runtime::orders::{OrderManager, OrderManagerError, OrderPayload, PlaceOrder};
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use support::FakeExchange;

fn manager(exchange: &Arc<FakeExchange>, db: Option<Arc<OrderDb>>) -> OrderManager {
    OrderManager::new(exchange.clone(), db)
}

fn update(value: Value) -> OrderPayload {
    OrderPayload::from_json(&value)
}

fn stored_order(
    id: &str,
    side: OrderSide,
    status: OrderStatus,
    amount: Decimal,
    filled: Decimal,
    price: Decimal,
) -> Order {
    let timestamp = utc_now();
    let fills = if filled > Decimal::ZERO {
        vec![Fill {
            id: format!("fill-{id}"),
            order_id: id.into(),
            symbol: "BTC/USDT".into(),
            side,
            price,
            amount: filled,
            fee: Decimal::ZERO,
            fee_currency: String::new(),
            timestamp,
        }]
    } else {
        Vec::new()
    };
    Order {
        status,
        filled_amount: filled,
        remaining_amount: (amount - filled).max(Decimal::ZERO),
        avg_fill_price: Some(price),
        fills,
        timestamp,
        ..Order::new(id, "BTC/USDT", OrderType::Market, side, amount)
    }
}

fn temp_db(orders: &[Order]) -> (tempfile::TempDir, Arc<OrderDb>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(OrderDb::open(dir.path().join("bot.db")).unwrap());
    for order in orders {
        db.upsert_order(order).unwrap();
        // Distinct timestamps keep the restore order deterministic.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    (dir, db)
}

#[tokio::test]
async fn place_order_tracks_active_and_historical_state() {
    let exchange = FakeExchange::new();
    exchange.create_payloads.lock().push_back(json!({
        "id": "order-1", "symbol": "BTC/USDT", "type": "limit", "side": "buy",
        "amount": "0.5", "price": "30000", "status": "open", "filled": "0", "remaining": "0.5",
    }));
    let manager = manager(&exchange, None);
    let order = manager
        .place_order(PlaceOrder::limit(
            " BTC/USDT ",
            OrderSide::Buy,
            dec!(0.5),
            dec!(30000),
        ))
        .await
        .unwrap();
    assert_eq!(order.id, "order-1");
    assert_eq!(order.order_type, OrderType::Limit);
    assert_eq!(order.status, OrderStatus::Open);
    assert_eq!(order.remaining_amount, dec!(0.5));
    let call = exchange.create_calls.lock()[0].clone();
    assert_eq!(
        (call.symbol.as_str(), call.amount, call.price),
        ("BTC/USDT", dec!(0.5), Some(dec!(30000)))
    );
    let ids = |orders: Vec<Order>| orders.into_iter().map(|o| o.id).collect::<Vec<_>>();
    assert_eq!(
        ids(manager.list_active_orders(None).unwrap()),
        vec!["order-1"]
    );
    assert_eq!(ids(manager.list_orders(None).unwrap()), vec!["order-1"]);
}

#[tokio::test]
async fn cancel_order_moves_active_order_to_historical_terminal_state() {
    let exchange = FakeExchange::new();
    let base = json!({"id": "order-2", "symbol": "ETH/USDT", "type": "limit", "side": "sell", "amount": "2", "price": "2200"});
    let mut open = base.clone();
    open["status"] = json!("open");
    let mut canceled = base;
    canceled["status"] = json!("canceled");
    exchange.create_payloads.lock().push_back(open);
    exchange.cancel_payloads.lock().push_back(canceled);
    let manager = manager(&exchange, None);
    manager
        .place_order(PlaceOrder::limit(
            "ETH/USDT",
            OrderSide::Sell,
            dec!(2),
            dec!(2200),
        ))
        .await
        .unwrap();

    let order = manager.cancel_order("order-2", None).await.unwrap();
    assert_eq!(order.status, OrderStatus::Canceled);
    assert_eq!(
        *exchange.cancel_calls.lock(),
        vec![("order-2".to_owned(), "ETH/USDT".to_owned())]
    );
    assert!(manager.list_active_orders(None).unwrap().is_empty());
    assert_eq!(
        manager.list_orders(None).unwrap()[0].status,
        OrderStatus::Canceled
    );
    assert!(matches!(
        manager.cancel_order("order-2", None).await,
        Err(OrderManagerError::InvalidTransition { .. })
    ));
}

#[tokio::test]
async fn status_refresh_fetches_and_updates_order() {
    let exchange = FakeExchange::new();
    exchange.create_payloads.lock().push_back(json!({
        "id": "order-3", "symbol": "BTC/USDT", "type": "limit", "side": "buy",
        "amount": "1", "price": "100", "status": "open", "filled": "0",
    }));
    exchange.fetch_payloads.lock().push_back(json!({
        "id": "order-3", "symbol": "BTC/USDT", "type": "limit", "side": "buy", "amount": "1", "price": "100",
        "status": "filled", "filled": "1", "remaining": "0", "average": "100",
    }));
    let manager = manager(&exchange, None);
    manager
        .place_order(PlaceOrder::limit(
            "BTC/USDT",
            OrderSide::Buy,
            dec!(1),
            dec!(100),
        ))
        .await
        .unwrap();

    let status = manager
        .get_order_status("order-3", None, true)
        .await
        .unwrap();
    assert_eq!(status, OrderStatus::Closed);
    assert_eq!(
        *exchange.fetch_calls.lock(),
        vec![("order-3".to_owned(), Some("BTC/USDT".to_owned()))]
    );
    assert!(manager.list_active_orders(None).unwrap().is_empty());
    assert_eq!(
        manager.get_position("BTC/USDT").unwrap().unwrap().amount,
        dec!(1)
    );
}

#[tokio::test]
async fn apply_order_updates_derive_positions_from_fill_deltas() {
    let exchange = FakeExchange::new();
    exchange.create_payloads.lock().push_back(json!({
        "id": "order-4", "symbol": "BTC/USDT", "type": "limit", "side": "buy",
        "amount": "1", "price": "100", "status": "open", "filled": "0",
    }));
    let manager = manager(&exchange, None);
    manager
        .place_order(PlaceOrder::limit(
            "BTC/USDT",
            OrderSide::Buy,
            dec!(1),
            dec!(100),
        ))
        .await
        .unwrap();

    let partial = manager
        .apply_order_update(&update(json!({
            "id": "order-4", "symbol": "BTC/USDT", "type": "limit", "side": "buy", "amount": "1", "price": "100",
            "status": "PARTIALLY_FILLED", "filled": "0.4", "remaining": "0.6", "average": "100",
            "trades": [{"id": "fill-1", "amount": "0.4", "price": "100"}],
        })))
        .unwrap();
    assert_eq!(partial.status, OrderStatus::Open);
    let position = manager.get_position("BTC/USDT").unwrap().unwrap();
    assert_eq!(
        (position.amount, position.entry_price),
        (dec!(0.4), dec!(100))
    );

    let closed = manager
        .apply_order_update(&update(json!({
            "id": "order-4", "symbol": "BTC/USDT", "type": "limit", "side": "buy", "amount": "1", "price": "100",
            "status": "FILLED", "filled": "1", "remaining": "0", "average": "112",
            "trades": [
                {"id": "fill-1", "amount": "0.4", "price": "100"},
                {"id": "fill-2", "amount": "0.6", "price": "120"},
            ],
        })))
        .unwrap();
    let position = manager.get_position("BTC/USDT").unwrap().unwrap();
    assert_eq!(closed.status, OrderStatus::Closed);
    assert_eq!(closed.fills.len(), 2);
    assert_eq!(
        (position.amount, position.entry_price),
        (dec!(1), dec!(112))
    );
    assert!(manager.list_active_orders(None).unwrap().is_empty());
}

#[tokio::test]
async fn restore_from_db_rebuilds_orders_positions_and_active_index() {
    let (_dir, db) = temp_db(&[
        stored_order(
            "restore-buy",
            OrderSide::Buy,
            OrderStatus::Closed,
            dec!(1),
            dec!(1),
            dec!(100),
        ),
        stored_order(
            "restore-sell",
            OrderSide::Sell,
            OrderStatus::Closed,
            dec!(0.4),
            dec!(0.4),
            dec!(110),
        ),
        stored_order(
            "restore-open",
            OrderSide::Buy,
            OrderStatus::Open,
            dec!(0.5),
            dec!(0.1),
            dec!(120),
        ),
    ]);
    let exchange = FakeExchange::new();
    let manager = manager(&exchange, Some(db));
    manager.restore_from_db().unwrap();

    let ids: Vec<_> = manager
        .list_orders(None)
        .unwrap()
        .into_iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(ids, vec!["restore-buy", "restore-sell", "restore-open"]);
    let active: Vec<_> = manager
        .list_active_orders(None)
        .unwrap()
        .into_iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(active, vec!["restore-open"]);
    let position = manager.get_position("BTC/USDT").unwrap().unwrap();
    assert_eq!(position.amount, dec!(0.7));
    assert_eq!(position.entry_price.round_dp(10), dec!(102.8571428571));

    manager
        .apply_order_update(&update(json!({
            "id": "restore-buy", "symbol": "BTC/USDT", "type": "market", "side": "buy",
            "amount": "1", "status": "filled", "filled": "1", "average": "100",
        })))
        .unwrap();
    assert_eq!(
        manager.get_position("BTC/USDT").unwrap().unwrap().amount,
        dec!(0.7)
    );
}

#[tokio::test]
async fn reconcile_active_orders_refreshes_restored_orders() {
    let (_dir, db) = temp_db(&[stored_order(
        "restore-open",
        OrderSide::Buy,
        OrderStatus::Open,
        dec!(0.5),
        dec!(0.1),
        dec!(120),
    )]);
    let exchange = FakeExchange::new();
    exchange.fetch_payloads.lock().push_back(json!({
        "id": "restore-open", "symbol": "BTC/USDT", "type": "market", "side": "buy", "amount": "0.5",
        "status": "filled", "filled": "0.5", "remaining": "0", "average": "120",
    }));
    let manager = manager(&exchange, Some(db.clone()));
    manager.restore_from_db().unwrap();

    let reconciled = manager.reconcile_active_orders().await.unwrap();
    assert_eq!(
        reconciled.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        vec!["restore-open"]
    );
    assert_eq!(
        *exchange.fetch_calls.lock(),
        vec![("restore-open".to_owned(), Some("BTC/USDT".to_owned()))]
    );
    assert!(manager.list_active_orders(None).unwrap().is_empty());
    let position = manager.get_position("BTC/USDT").unwrap().unwrap();
    assert_eq!(
        (position.amount, position.entry_price),
        (dec!(0.5), dec!(120))
    );
    assert_eq!(db.load_orders().unwrap()[0].status, OrderStatus::Closed);
}

#[test]
fn terminal_orders_reject_invalid_lifecycle_regressions() {
    let exchange = FakeExchange::new();
    let manager = manager(&exchange, None);
    manager
        .apply_order_update(&update(json!({
            "id": "order-5", "symbol": "ETH/USDT", "type": "limit", "side": "buy",
            "amount": "1", "price": "2000", "status": "filled", "filled": "1", "average": "2000",
        })))
        .unwrap();
    let err = manager
        .apply_order_update(&update(json!({
            "id": "order-5", "symbol": "ETH/USDT", "type": "limit", "side": "buy",
            "amount": "1", "price": "2000", "status": "canceled",
        })))
        .unwrap_err();
    assert!(
        matches!(err, OrderManagerError::InvalidTransition { .. }),
        "{err}"
    );
    assert_eq!(
        manager.list_orders(None).unwrap()[0].status,
        OrderStatus::Closed
    );
}

#[tokio::test]
async fn place_order_validates_requests_before_exchange_call() {
    let exchange = FakeExchange::new();
    let manager = manager(&exchange, None);
    let limit_without_price = PlaceOrder {
        order_type: OrderType::Limit,
        ..PlaceOrder::market("BTC/USDT", OrderSide::Buy, dec!(1))
    };
    let zero_price = PlaceOrder {
        price: Some(dec!(0)),
        ..PlaceOrder::market("BTC/USDT", OrderSide::Buy, dec!(1))
    };
    for (request, message) in [
        (limit_without_price, "price is required"),
        (zero_price, "price must be greater"),
    ] {
        let err = manager.place_order(request).await.unwrap_err();
        assert!(err.to_string().contains(message), "{err}");
    }
    assert!(
        "stop"
            .parse::<OrderType>()
            .unwrap_err()
            .contains("order_type")
    );
    assert!(exchange.create_calls.lock().is_empty());
}

#[tokio::test]
async fn unknown_local_status_query_raises_order_not_found() {
    let manager = manager(&FakeExchange::new(), None);
    assert!(matches!(
        manager.get_order_status("missing", None, false).await,
        Err(OrderManagerError::NotFound(id)) if id == "missing"
    ));
}

#[tokio::test]
async fn local_order_ids_are_rekeyed_to_exchange_ids_in_the_database() {
    let (_dir, db) = temp_db(&[]);
    let exchange = FakeExchange::new();
    let manager = manager(&exchange, Some(db.clone()));
    let order = manager
        .place_order(PlaceOrder::market("BTC/USDT", OrderSide::Buy, dec!(0.001)))
        .await
        .unwrap();
    assert_eq!(order.id, "1");
    let stored = db.load_orders().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, "1");
    assert_eq!(stored[0].fills.len(), 1);
    assert_eq!(stored[0].fills[0].id, "1:synthetic:0.001");
}

#[test]
fn existing_python_database_opens_and_restores() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bot.db");
    if !source.exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("bot.db");
    std::fs::copy(&source, &copy).unwrap();
    let db = Arc::new(OrderDb::open(&copy).unwrap());
    let orders = db.load_orders().unwrap();
    assert!(orders.iter().all(|o| !o.id.is_empty()));
    let manager = manager(&FakeExchange::new(), Some(db));
    manager.restore_from_db().unwrap();
    assert_eq!(manager.list_orders(None).unwrap().len(), orders.len());
}

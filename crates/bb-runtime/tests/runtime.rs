#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Port of `tests/test_runtime.py` (StrategyOrderClient, BotRuntime and the
//! crash-restart supervisor).

mod support;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bb_binance::BinanceError;
use bb_core::Decimal;
use bb_runtime::config::{OrderSizeMode, Settings};
use bb_runtime::db::OrderDb;
use bb_runtime::orders::OrderManager;
use bb_runtime::orders::models::{Order, OrderSide, OrderStatus, OrderType};
use bb_runtime::orders::sizing::OrderSizingConfig;
use bb_runtime::portfolio::{PortfolioError, PortfolioSnapshot, PortfolioSource};
use bb_runtime::risk::{RiskConfig, RiskGuard};
use bb_runtime::runtime::{
    BotRuntime, CandleSource, CandleStream, ClientError, ComponentFactory, Components,
    RuntimeError, Signal, StrategyOrderClient, supervise,
};
use bb_runtime::strategies::{OrderClient, StrategyOrder, StrategyRegistry};
use futures::StreamExt;
use parking_lot::Mutex;
use rust_decimal_macros::dec;
use support::{CapturingTransport, FakeExchange};

struct StaticPortfolio {
    snapshot: PortfolioSnapshot,
    marks: Mutex<Vec<HashMap<String, Decimal>>>,
}

impl StaticPortfolio {
    fn new(snapshot: PortfolioSnapshot) -> Arc<Self> {
        Arc::new(Self {
            snapshot,
            marks: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl PortfolioSource for StaticPortfolio {
    async fn snapshot(
        &self,
        marks: &HashMap<String, Decimal>,
    ) -> Result<PortfolioSnapshot, PortfolioError> {
        self.marks.lock().push(marks.clone());
        Ok(self.snapshot.clone())
    }
}

fn cash(available: Decimal, equity: Decimal) -> PortfolioSnapshot {
    PortfolioSnapshot {
        available_balance_usdt: available,
        total_balance_usdt: available,
        total_equity_usdt: equity,
        ..PortfolioSnapshot::default()
    }
}

fn manager(exchange: &Arc<FakeExchange>) -> Arc<OrderManager> {
    Arc::new(OrderManager::new(exchange.clone(), None))
}

#[tokio::test]
async fn strategy_order_client_applies_dynamic_sizing_in_dry_run() {
    let exchange = FakeExchange::new();
    let (transport, alerter) = CapturingTransport::alerter();
    let portfolio = StaticPortfolio::new(cash(dec!(1000), dec!(1000)));
    let client = StrategyOrderClient::new(manager(&exchange))
        .with_alerter(Some(alerter))
        .with_dry_run(true)
        .with_sizing_config(OrderSizingConfig {
            mode: OrderSizeMode::QuotePercent,
            order_size_percent: dec!(25),
            ..OrderSizingConfig::default()
        })
        .with_portfolio(portfolio.clone());
    client.update_last_price(dec!(100));

    let order = client
        .submit(StrategyOrder::market(
            "BTC/USDT",
            OrderSide::Buy,
            dec!(0.001),
        ))
        .await
        .unwrap();
    assert_eq!(order.status, OrderStatus::Closed);
    assert_eq!((order.amount, order.filled), (dec!(2.5), dec!(2.5)));
    assert!(
        order.id.starts_with("dry-") && order.id.len() == 12,
        "{}",
        order.id
    );
    assert_eq!(
        *portfolio.marks.lock(),
        vec![HashMap::from([("BTC/USDT".to_owned(), dec!(100))])]
    );
    assert!(
        exchange.create_calls.lock().is_empty(),
        "dry run must not reach the exchange"
    );
    let messages = transport.messages();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("<i>(dry run)</i>") && messages[0].contains("<code>2.5</code>"));
}

#[tokio::test]
async fn strategy_order_client_can_bypass_sizing_for_position_closes() {
    let exchange = FakeExchange::new();
    let portfolio = StaticPortfolio::new(cash(dec!(1000), dec!(1000)));
    let client = StrategyOrderClient::new(manager(&exchange))
        .with_dry_run(true)
        .with_sizing_config(OrderSizingConfig {
            mode: OrderSizeMode::QuotePercent,
            order_size_percent: dec!(50),
            ..OrderSizingConfig::default()
        })
        .with_portfolio(portfolio.clone());
    client.update_last_price(dec!(100));
    let order = client
        .submit(StrategyOrder {
            size_order: false,
            ..StrategyOrder::market("BTC/USDT", OrderSide::Sell, dec!(0.25))
        })
        .await
        .unwrap();
    assert_eq!(order.amount, dec!(0.25));
    assert!(portfolio.marks.lock().is_empty());
}

#[tokio::test]
async fn strategy_order_client_applies_expanded_risk_rules_and_alerts() {
    let base = RiskConfig::new(dec!(10000), dec!(10000));
    let buy = |amount| StrategyOrder::market("BTC/USDT", OrderSide::Buy, amount);
    let cases = [
        (
            RiskConfig {
                max_total_exposure_usd: Some(dec!(600)),
                ..base.clone()
            },
            PortfolioSnapshot {
                positions: BTreeMap::from([("ETH/USDT".to_owned(), dec!(590))]),
                total_exposure_usdt: dec!(590),
                ..cash(dec!(1000), dec!(2000))
            },
            buy(dec!(0.2)),
            "maximum total exposure breached",
        ),
        (
            RiskConfig {
                max_open_orders: Some(1),
                ..base.clone()
            },
            PortfolioSnapshot {
                open_orders_count: 1,
                ..cash(dec!(1000), dec!(1000))
            },
            buy(dec!(0.1)),
            "maximum open orders breached",
        ),
        (
            RiskConfig {
                min_balance_usdt: dec!(950),
                ..base.clone()
            },
            cash(dec!(1000), dec!(1000)),
            buy(dec!(1)),
            "minimum balance breached",
        ),
        (
            RiskConfig {
                allowed_leverage: Some(dec!(2)),
                ..base.clone()
            },
            cash(dec!(1000), dec!(1000)),
            buy(dec!(0.1)),
            "leverage check breached",
        ),
        (
            RiskConfig {
                daily_drawdown_limit_pct: Some(dec!(5)),
                ..base.clone()
            },
            PortfolioSnapshot {
                realized_pnl_today_usdt: dec!(-60),
                ..cash(dec!(1000), dec!(1000))
            },
            buy(dec!(0.1)),
            "daily drawdown breached",
        ),
        (
            RiskConfig {
                max_price_deviation_pct: Some(dec!(5)),
                ..base.clone()
            },
            cash(dec!(1000), dec!(1000)),
            StrategyOrder {
                order_type: OrderType::Limit,
                price: Some(dec!(120)),
                ..buy(dec!(0.1))
            },
            "price deviation breached",
        ),
    ];
    for (config, snapshot, order, expected) in cases {
        let exchange = FakeExchange::new();
        let (transport, alerter) = CapturingTransport::alerter();
        let client = StrategyOrderClient::new(manager(&exchange))
            .with_risk_guard(Arc::new(RiskGuard::new(config, None).unwrap()))
            .with_alerter(Some(alerter))
            .with_portfolio(StaticPortfolio::new(snapshot));
        client.update_last_price(dec!(100));
        let err = client.submit(order).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::Risk(breach) if breach.message.starts_with(expected)),
            "{expected}: {err}"
        );
        let messages = transport.messages();
        assert_eq!(messages.len(), 1, "{expected}");
        assert!(messages[0].contains(expected), "{}", messages[0]);
        assert!(exchange.create_calls.lock().is_empty());
    }
}

#[tokio::test]
async fn sizing_errors_alert_and_block_the_order() {
    let exchange = FakeExchange::new();
    let (transport, alerter) = CapturingTransport::alerter();
    let client = StrategyOrderClient::new(manager(&exchange))
        .with_alerter(Some(alerter))
        .with_sizing_config(OrderSizingConfig {
            min_order_notional_usd: dec!(10),
            ..OrderSizingConfig::default()
        });
    client.update_last_price(dec!(100));
    let err = client
        .submit(StrategyOrder::market(
            "BTC/USDT",
            OrderSide::Buy,
            dec!(0.001),
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::Sizing(_)), "{err}");
    assert!(transport.messages()[0].contains("order notional below minimum"));
    assert!(exchange.create_calls.lock().is_empty());
}

// ---------------------------------------------------------------- runtime

struct PendingFeed {
    subscriptions: Mutex<Vec<String>>,
}

#[async_trait]
impl CandleSource for PendingFeed {
    async fn subscribe_ohlcv(
        &self,
        symbol: &str,
        _timeframe: &str,
    ) -> Result<CandleStream, BinanceError> {
        self.subscriptions.lock().push(symbol.to_owned());
        Ok(futures::stream::pending().boxed())
    }
}

struct Factory {
    exchange: Arc<FakeExchange>,
    feed: Arc<PendingFeed>,
}

impl Factory {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            exchange: FakeExchange::new(),
            feed: Arc::new(PendingFeed {
                subscriptions: Mutex::new(Vec::new()),
            }),
        })
    }
}

impl ComponentFactory for Factory {
    fn build(&self, _settings: &Settings) -> Result<Components, RuntimeError> {
        Ok(Components {
            exchange: self.exchange.clone(),
            feed: self.feed.clone(),
            execution: None,
            capital: None,
        })
    }
}

fn settings(dir: &tempfile::TempDir) -> Settings {
    let mut s = Settings::defaults("key", "secret");
    s.db_path = dir.path().join("bot.db").display().to_string();
    s.validate().unwrap()
}

#[tokio::test]
async fn runtime_rejects_unconfirmed_and_fixed_size_live_trading_before_setup() {
    let dir = tempfile::tempdir().unwrap();
    let mut live = settings(&dir);
    live.use_testnet = false;
    live.order_size_mode = OrderSizeMode::QuotePercent;
    let runtime = BotRuntime::new(
        live.clone(),
        Signal::default(),
        Signal::default(),
        Factory::new(),
    );
    let err = runtime.run().await.unwrap_err();
    assert!(err.is_fatal());
    assert_eq!(
        err.to_string(),
        "Live trading requires CONFIRM_LIVE_TRADING=yes in environment"
    );
    assert!(
        !dir.path().join("bot.db").exists(),
        "nothing is built before the check"
    );

    let fixed = Settings {
        order_size_mode: OrderSizeMode::Fixed,
        confirm_live_trading: "yes".into(),
        ..live
    };
    let err = BotRuntime::new(fixed, Signal::default(), Signal::default(), Factory::new())
        .run()
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("Fixed-size live execution is disabled")
    );
}

#[tokio::test]
async fn runtime_cleans_up_components_when_setup_fails() {
    let dir = tempfile::tempdir().unwrap();
    let factory = Factory::new();
    let s = Settings {
        strategy_name: "missing".into(),
        ..settings(&dir)
    };
    let err = BotRuntime::new(s, Signal::default(), Signal::default(), factory.clone())
        .run()
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("strategy plugin is not registered"),
        "{err}"
    );
    assert!(*factory.exchange.closed.lock());
}

#[tokio::test]
async fn runtime_starts_one_strategy_per_symbol_and_stops_on_signal() {
    let dir = tempfile::tempdir().unwrap();
    let factory = Factory::new();
    let mut s = settings(&dir);
    s.trading_symbols = vec!["BTC/USDT".into(), "ethusdt".into()];
    let s = s.validate().unwrap();
    let stop = Signal::default();
    let (transport, alerter) = CapturingTransport::alerter();
    let runtime = BotRuntime::new(s, stop.clone(), Signal::default(), factory.clone())
        .with_alerter(Some(alerter));
    let handle = tokio::spawn(async move { runtime.run().await });
    while factory.feed.subscriptions.lock().len() < 2 {
        tokio::task::yield_now().await;
    }
    stop.set();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut subscribed = factory.feed.subscriptions.lock().clone();
    subscribed.sort();
    assert_eq!(subscribed, vec!["BTC/USDT", "ETH/USDT"]);
    assert!(*factory.exchange.closed.lock());
    assert_eq!(
        transport.messages(),
        vec!["\u{1f7e1} <b>Bot started</b> — mode: <code>testnet</code>".to_owned()]
    );
}

fn closed_order(id: &str, symbol: &str, side: OrderSide, amount: Decimal, price: Decimal) -> Order {
    Order {
        status: OrderStatus::Closed,
        filled_amount: amount,
        avg_fill_price: Some(price),
        price: Some(price),
        ..Order::new(id, symbol, OrderType::Market, side, amount)
    }
}

#[tokio::test]
async fn runtime_rolls_back_positions_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let s = settings(&dir);
    {
        let db = OrderDb::open(&s.db_path).unwrap();
        db.upsert_order(&closed_order(
            "a",
            "BTC/USDT",
            OrderSide::Buy,
            dec!(0.25),
            dec!(100),
        ))
        .unwrap();
        db.upsert_order(&closed_order(
            "b",
            "ETH/USDT",
            OrderSide::Sell,
            dec!(1.5),
            dec!(10),
        ))
        .unwrap();
        db.upsert_order(&closed_order(
            "c",
            "BNB/USDT",
            OrderSide::Buy,
            dec!(1),
            dec!(1),
        ))
        .unwrap();
        db.upsert_order(&closed_order(
            "d",
            "BNB/USDT",
            OrderSide::Sell,
            dec!(1),
            dec!(1),
        ))
        .unwrap();
    }
    let factory = Factory::new();
    let rollback = Signal::default();
    rollback.set();
    let stop = Signal::default();
    BotRuntime::new(s, stop.clone(), rollback, factory.clone())
        .run()
        .await
        .unwrap();

    assert!(stop.is_set());
    assert!(*factory.exchange.closed.lock());
    let calls: Vec<_> = factory
        .exchange
        .create_calls
        .lock()
        .iter()
        .map(|c| (c.symbol.clone(), c.side, c.amount, c.order_type, c.price))
        .collect();
    assert_eq!(
        calls,
        vec![
            (
                "BTC/USDT".to_owned(),
                OrderSide::Sell,
                dec!(0.25),
                OrderType::Market,
                None
            ),
            (
                "ETH/USDT".to_owned(),
                OrderSide::Buy,
                dec!(1.5),
                OrderType::Market,
                None
            ),
        ]
    );
}

#[tokio::test]
async fn dry_run_runtime_reports_mode_and_places_no_orders() {
    let dir = tempfile::tempdir().unwrap();
    let s = Settings {
        dry_run: true,
        ..settings(&dir)
    };
    let factory = Factory::new();
    let rollback = Signal::default();
    rollback.set();
    let (transport, alerter) = CapturingTransport::alerter();
    {
        let db = OrderDb::open(&s.db_path).unwrap();
        db.upsert_order(&closed_order(
            "a",
            "BTC/USDT",
            OrderSide::Buy,
            dec!(0.25),
            dec!(100),
        ))
        .unwrap();
    }
    BotRuntime::new(s, Signal::default(), rollback, factory.clone())
        .with_alerter(Some(alerter))
        .run()
        .await
        .unwrap();
    assert!(factory.exchange.create_calls.lock().is_empty());
    let messages = transport.messages();
    assert!(messages[0].contains("mode: <code>DRY_RUN</code>"));
    assert!(messages[1].contains("<b>SELL</b> <i>(dry run)</i>"));
}

#[tokio::test]
async fn runtime_reconciles_active_orders_during_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let s = settings(&dir);
    {
        let db = OrderDb::open(&s.db_path).unwrap();
        let open = Order {
            status: OrderStatus::Open,
            price: Some(dec!(100)),
            ..Order::new("77", "BTC/USDT", OrderType::Limit, OrderSide::Buy, dec!(1))
        };
        db.upsert_order(&open).unwrap();
    }
    let factory = Factory::new();
    factory.exchange.fetch_payloads.lock().push_back(serde_json::json!({
        "id": "77", "symbol": "BTC/USDT", "type": "limit", "side": "buy", "amount": "1", "price": "100",
        "status": "canceled", "filled": "0",
    }));
    let stop = Signal::default();
    stop.set();
    BotRuntime::new(s.clone(), stop, Signal::default(), factory.clone())
        .run()
        .await
        .unwrap();
    assert_eq!(
        *factory.exchange.fetch_calls.lock(),
        vec![("77".to_owned(), Some("BTC/USDT".to_owned()))]
    );
    let db = OrderDb::open(&s.db_path).unwrap();
    assert_eq!(db.load_orders().unwrap()[0].status, OrderStatus::Canceled);
}

#[tokio::test]
async fn custom_strategy_registry_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let s = Settings {
        strategy_name: "none".into(),
        ..settings(&dir)
    };
    let mut registry = StrategyRegistry::with_builtin();
    let builtin = StrategyRegistry::with_builtin();
    registry
        .register(
            "none",
            Arc::new(move |client, settings| builtin.create("demo-sma", client, settings)),
        )
        .unwrap();
    let stop = Signal::default();
    stop.set();
    BotRuntime::new(s, stop, Signal::default(), Factory::new())
        .with_registry(registry)
        .run()
        .await
        .unwrap();
}

// ------------------------------------------------------------- supervisor

#[tokio::test(start_paused = true)]
async fn main_restarts_after_runtime_crash() {
    let (transport, alerter) = CapturingTransport::alerter();
    let attempts = Mutex::new(0_u32);
    let result = supervise(Some(&alerter), &Signal::default(), || {
        let attempt = {
            let mut n = attempts.lock();
            *n += 1;
            *n
        };
        async move {
            if attempt == 1 {
                Err(RuntimeError::Task("boom".into()))
            } else {
                Ok(())
            }
        }
    })
    .await;
    assert!(result.is_ok());
    assert_eq!(*attempts.lock(), 2);
    let messages = transport.messages();
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0].contains("(attempt 1/5)") && messages[0].contains("Restarting in 5s"),
        "{}",
        messages[0]
    );
}

#[tokio::test(start_paused = true)]
async fn main_alerts_after_terminal_runtime_failure() {
    let (transport, alerter) = CapturingTransport::alerter();
    let started = tokio::time::Instant::now();
    let result = supervise(Some(&alerter), &Signal::default(), || async {
        Err(RuntimeError::Task("still broken".into()))
    })
    .await;
    assert!(result.is_err());
    // 5 + 10 + 20 + 40 seconds of backoff between the five attempts.
    assert_eq!(started.elapsed(), Duration::from_secs(75));
    let messages = transport.messages();
    assert_eq!(messages.len(), 5);
    assert!(messages[3].contains("Restarting in 40s"));
    assert!(messages[4].starts_with("\u{1f6d1} <b>Bot stopped after repeated failures</b>"));
    assert!(messages[4].contains("runtime task failed: still broken"));
}

#[tokio::test]
async fn fatal_errors_are_not_retried() {
    let calls = Mutex::new(0);
    let result = supervise(None, &Signal::default(), || {
        *calls.lock() += 1;
        async { Err(RuntimeError::LiveTradingRefused("no")) }
    })
    .await;
    assert!(result.unwrap_err().is_fatal());
    assert_eq!(*calls.lock(), 1);
}

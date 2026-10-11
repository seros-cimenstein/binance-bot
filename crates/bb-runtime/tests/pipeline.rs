#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `test_pipeline.py` (end-to-end StrategyOrderClient → OrderManager →
//! RiskGuard → DemoSMA) and `tests/test_integration_runtime.py` (full
//! BotRuntime with fake exchange, feed, execution stream, alerts and SQLite).

mod support;

use std::sync::Arc;

use async_trait::async_trait;
use bb_binance::BinanceError;
use bb_binance::feeds::CandleData;
use bb_binance::ws::ExecutionReport;
use bb_core::Decimal;
use bb_runtime::config::{OrderUpdateMode, Settings};
use bb_runtime::db::OrderDb;
use bb_runtime::orders::OrderManager;
use bb_runtime::orders::models::OrderSide;
use bb_runtime::risk::{RiskConfig, RiskGuard};
use bb_runtime::runtime::{
    BotRuntime, CandleSource, CandleStream, ComponentFactory, Components, ExecutionSource,
    ExecutionStream, RuntimeError, Signal, StrategyOrderClient,
};
use bb_runtime::strategies::{Candle, DemoSmaStrategy, OrderClient, SmaSettings, Strategy};
use futures::StreamExt;
use parking_lot::Mutex;
use rust_decimal_macros::dec;
use support::{CapturingTransport, FakeExchange};

fn candle(symbol: &str, price: Decimal, timestamp: i64) -> CandleData {
    CandleData {
        symbol: symbol.into(),
        timestamp,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: dec!(1),
    }
}

#[tokio::test]
async fn test_pipeline_buy_and_sell_at_82000_with_two_alerts() {
    let exchange = FakeExchange::new();
    let (transport, alerter) = CapturingTransport::alerter();
    let order_manager = Arc::new(OrderManager::new(exchange.clone(), None));
    let risk_guard =
        Arc::new(RiskGuard::new(RiskConfig::new(dec!(50000), dec!(10000)), None).unwrap());
    let client = Arc::new(
        StrategyOrderClient::new(order_manager.clone())
            .with_risk_guard(risk_guard)
            .with_alerter(Some(alerter))
            .with_dry_run(false),
    );
    let settings = SmaSettings {
        fast_period: 3,
        slow_period: 6,
        order_size: dec!(0.001),
        ..SmaSettings::new("BTC/USDT").unwrap()
    };
    let strategy = DemoSmaStrategy::new(client.clone(), settings);
    strategy.start().await;

    let prices = std::iter::repeat_n(dec!(82000), 8)
        .chain(std::iter::repeat_n(dec!(86000), 4))
        .chain(std::iter::repeat_n(dec!(76000), 5));
    for (i, price) in prices.enumerate() {
        client.update_last_price(price);
        strategy
            .on_candle(&Candle::from(candle("BTC/USDT", price, i as i64 + 1)))
            .await
            .unwrap();
    }

    let filled = exchange.filled.lock().clone();
    assert_eq!(exchange.sides(), vec!["buy", "sell"]);
    for order in &filled {
        assert_eq!(order["average"], "82000");
        assert_eq!(order["amount"], "0.001");
    }
    let messages = transport.messages();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages[0].contains("<b>BUY</b>") && messages[0].contains("$82,000.0000"),
        "{}",
        messages[0]
    );
    assert!(
        messages[1].contains("<b>SELL</b>") && messages[1].contains("$82,000.0000"),
        "{}",
        messages[1]
    );
    assert!(
        order_manager.get_position("BTC/USDT").unwrap().is_none(),
        "round trip leaves no position"
    );
}

struct FiniteFeed {
    subscriptions: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl CandleSource for FiniteFeed {
    async fn subscribe_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
    ) -> Result<CandleStream, BinanceError> {
        self.subscriptions
            .lock()
            .push((symbol.to_owned(), timeframe.to_owned()));
        let symbol = symbol.to_owned();
        let prices: Vec<Decimal> = std::iter::repeat_n(dec!(82000), 8)
            .chain(std::iter::repeat_n(dec!(86000), 4))
            .chain(std::iter::repeat_n(dec!(78000), 5))
            .collect();
        Ok(futures::stream::iter(prices.into_iter().enumerate())
            .then(move |(i, price)| {
                let symbol = symbol.clone();
                async move {
                    tokio::task::yield_now().await;
                    Ok(candle(&symbol, price, i as i64 + 1))
                }
            })
            .boxed())
    }
}

#[derive(Default)]
struct FakeExecution {
    started: Mutex<u32>,
    stopped: Arc<Mutex<bool>>,
}

struct StopOnDrop(Arc<Mutex<bool>>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        *self.0.lock() = true;
    }
}

#[async_trait]
impl ExecutionSource for FakeExecution {
    async fn open(&self) -> Result<ExecutionStream, BinanceError> {
        *self.started.lock() += 1;
        let guard = StopOnDrop(self.stopped.clone());
        Ok(futures::stream::pending::<ExecutionReport>()
            .map(move |report| {
                let _keep = &guard;
                report
            })
            .boxed())
    }
}

struct FakeFactory {
    exchange: Arc<FakeExchange>,
    feed: Arc<FiniteFeed>,
    execution: Arc<FakeExecution>,
}

impl ComponentFactory for FakeFactory {
    fn build(&self, _settings: &Settings) -> Result<Components, RuntimeError> {
        Ok(Components {
            exchange: self.exchange.clone(),
            feed: self.feed.clone(),
            execution: Some(self.execution.clone()),
            capital: None,
        })
    }
}

#[tokio::test]
async fn runtime_pipeline_with_fake_exchange_websocket_telegram_and_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("bot.db");
    let mut settings = Settings::defaults("key", "secret");
    settings.db_path = db_path.display().to_string();
    settings.fast_period = 3;
    settings.slow_period = 6;
    settings.order_size = dec!(0.001);
    settings.max_position_size_usd = dec!(50000);
    settings.daily_loss_limit_usd = dec!(10000);
    settings.order_update_mode = OrderUpdateMode::Websocket;
    let settings = settings.validate().unwrap();

    let factory = Arc::new(FakeFactory {
        exchange: FakeExchange::with_usdt(dec!(10000)),
        feed: Arc::new(FiniteFeed {
            subscriptions: Mutex::new(Vec::new()),
        }),
        execution: Arc::new(FakeExecution::default()),
    });
    let (transport, alerter) = CapturingTransport::alerter();
    let runtime = BotRuntime::new(
        settings,
        Signal::default(),
        Signal::default(),
        factory.clone(),
    )
    .with_alerter(Some(alerter));

    tokio::time::timeout(std::time::Duration::from_secs(5), runtime.run())
        .await
        .expect("runtime finished")
        .unwrap();

    assert_eq!(
        *factory.feed.subscriptions.lock(),
        vec![("BTC/USDT".to_owned(), "1m".to_owned())]
    );
    assert_eq!(factory.exchange.sides(), vec!["buy", "sell"]);
    let messages = transport.messages();
    assert_eq!(
        messages[0],
        "\u{1f7e1} <b>Bot started</b> — mode: <code>testnet</code>"
    );
    let trades: Vec<_> = messages[1..]
        .iter()
        .map(|m| m.contains("<b>BUY</b>"))
        .collect();
    assert_eq!(trades, vec![true, false]);
    assert!(!messages.iter().any(|m| m.contains("Risk breach")));
    assert_eq!(*factory.execution.started.lock(), 1);
    assert!(*factory.execution.stopped.lock());
    assert!(*factory.exchange.closed.lock());

    let db = OrderDb::open(&db_path).unwrap();
    let sides: Vec<_> = db
        .load_orders()
        .unwrap()
        .into_iter()
        .map(|o| o.side)
        .collect();
    assert_eq!(sides, vec![OrderSide::Buy, OrderSide::Sell]);
}

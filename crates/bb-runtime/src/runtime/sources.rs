//! Market-data and execution-update sources the runtime consumes, with the
//! production Binance implementations (port of `_default_data_feed`,
//! `_default_execution_manager` and `src/data/websocket_feed.py`, whose
//! closed-candle feed now lives in `bb_binance::feeds`).

use std::sync::Arc;

use async_trait::async_trait;
use bb_binance::feeds::{
    CandleData, FeedBackoff, MarketDataFeed, PollingCandleFeed, WebSocketCandleFeed,
};
use bb_binance::ws::{EventQueue, EventType, ExecutionReport, WsConfig, WsManager, WsPayload};
use bb_binance::{BinanceClient, BinanceConfig, BinanceError, Credentials};
use futures::StreamExt;
use futures::stream::{self, BoxStream};

use super::RuntimeError;
use crate::capital::CapitalGate;
use crate::config::{MarketDataMode, OrderUpdateMode, Settings};
use crate::exchange::{BinanceExchange, Exchange};

pub type CandleStream = BoxStream<'static, Result<CandleData, BinanceError>>;
pub type ExecutionStream = BoxStream<'static, ExecutionReport>;

/// Closed-candle source (`feeds.subscribe_ohlcv`).
#[async_trait]
pub trait CandleSource: Send + Sync {
    async fn subscribe_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
    ) -> Result<CandleStream, BinanceError>;

    /// WebSocket manager to attach stream-failure alerts to, if any.
    fn ws_manager(&self) -> Option<WsManager> {
        None
    }
}

#[async_trait]
impl CandleSource for MarketDataFeed {
    async fn subscribe_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
    ) -> Result<CandleStream, BinanceError> {
        MarketDataFeed::subscribe_ohlcv(self, symbol, timeframe).await
    }

    fn ws_manager(&self) -> Option<WsManager> {
        self.manager().cloned()
    }
}

/// Private execution-report source (`_stream_order_updates`). Dropping the
/// returned stream must unsubscribe and stop the underlying connection.
#[async_trait]
pub trait ExecutionSource: Send + Sync {
    async fn open(&self) -> Result<ExecutionStream, BinanceError>;

    fn ws_manager(&self) -> Option<WsManager> {
        None
    }
}

/// User-data stream over a [`WsManager`].
#[derive(Debug, Clone)]
pub struct WsExecutionSource {
    manager: WsManager,
}

impl WsExecutionSource {
    pub fn new(manager: WsManager) -> Self {
        Self { manager }
    }
}

/// Unsubscribes the queue and stops the manager when the stream is dropped.
struct QueueGuard {
    manager: WsManager,
    queue: EventQueue,
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.manager.unsubscribe_queue(&self.queue);
        let manager = self.manager.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { manager.stop().await });
        }
    }
}

#[async_trait]
impl ExecutionSource for WsExecutionSource {
    async fn open(&self) -> Result<ExecutionStream, BinanceError> {
        let queue = self.manager.subscribe_queue(EventType::OrderUpdate, None);
        let guard = QueueGuard {
            manager: self.manager.clone(),
            queue: queue.clone(),
        };
        self.manager.start(true).await?;
        Ok(stream::unfold((guard, queue), |(guard, queue)| async move {
            loop {
                let event = queue.pop().await;
                if let WsPayload::Order(report) = &event.payload {
                    let report = ExecutionReport::clone(report);
                    return Some((report, (guard, queue)));
                }
            }
        })
        .boxed())
    }

    fn ws_manager(&self) -> Option<WsManager> {
        Some(self.manager.clone())
    }
}

/// The exchange-facing parts of one runtime attempt.
pub struct Components {
    pub exchange: Arc<dyn Exchange>,
    pub feed: Arc<dyn CandleSource>,
    /// Present when `ORDER_UPDATE_MODE=websocket`.
    pub execution: Option<Arc<dyn ExecutionSource>>,
    /// Capital pool leases / quote auto-convert (`CAPITAL_POOL_ENABLED`,
    /// `AUTO_CONVERT_TO_QUOTE`); `None` when both are off.
    pub capital: Option<Arc<CapitalGate>>,
}

/// Builds [`Components`] for each runtime attempt (test seam for the
/// Python `*_factory` keyword arguments).
pub trait ComponentFactory: Send + Sync {
    fn build(&self, settings: &Settings) -> Result<Components, RuntimeError>;
}

/// Production factory: native Binance REST client, polling or WebSocket
/// candle feed, and the user-data stream for order updates.
#[derive(Debug, Clone, Copy, Default)]
pub struct BinanceComponentFactory;

impl ComponentFactory for BinanceComponentFactory {
    fn build(&self, settings: &Settings) -> Result<Components, RuntimeError> {
        let credentials = Credentials::new(settings.api_key.clone(), &settings.api_secret)?;
        let client = BinanceClient::new(
            BinanceConfig::new(settings.use_testnet).with_credentials(credentials),
        )?;
        let feed = match settings.market_data_mode {
            MarketDataMode::Websocket => MarketDataFeed::WebSocket(WebSocketCandleFeed::new(
                WsManager::from_client(&client, WsConfig::default()),
                usize::try_from(settings.feed_queue_maxsize).unwrap_or(0),
            )),
            MarketDataMode::Polling => MarketDataFeed::Polling(PollingCandleFeed::new(
                client.clone(),
                FeedBackoff::default(),
            )?),
        };
        let execution: Option<Arc<dyn ExecutionSource>> = match settings.order_update_mode {
            OrderUpdateMode::Websocket => Some(Arc::new(WsExecutionSource::new(
                WsManager::from_client(&client, WsConfig::default()),
            ))),
            OrderUpdateMode::Disabled => None,
        };
        let capital = CapitalGate::from_env(settings, client.clone())
            .map_err(|err| RuntimeError::Capital(err.to_string()))?
            .map(Arc::new);
        Ok(Components {
            exchange: Arc::new(BinanceExchange::new(client)),
            feed: Arc::new(feed),
            execution,
            capital,
        })
    }
}

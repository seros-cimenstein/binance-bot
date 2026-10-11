//! Binance WebSocket manager (port of `src/exchange/websocket.py`).
//!
//! - One combined public market-stream connection (`/stream?streams=a/b/...`);
//!   live `SUBSCRIBE`/`UNSUBSCRIBE` control messages while connected; the
//!   subscription set is reconciled on every (re)connect.
//! - Optional user-data stream over the WebSocket API
//!   (`userDataStream.subscribe.signature`, HMAC or Ed25519 signed).
//! - Reconnect with exponential backoff + jitter, ping heartbeat, stale-stream
//!   detection (no message within `message_timeout` → reconnect), and
//!   `serverShutdown` handling.
//! - Fan-out to drop-oldest subscriber queues and async callbacks. Disconnects
//!   and malformed payloads surface as [`EventType::StreamError`] events (the
//!   runtime's failure-alert hook); terminated user streams as
//!   [`EventType::UserStreamExpired`].

pub mod events;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use crate::auth::Credentials;
use crate::config::BinanceConfig;
use crate::error::{BinanceError, Result};
use crate::queue::DropOldestQueue;
use crate::ratelimit::backoff_delay;
use crate::rest::{Api, BinanceClient};
use crate::symbols::stream_symbol;

pub use events::{
    AccountBalance, AccountPositionUpdate, BalanceUpdate, BookTickerUpdate, EventType,
    ExecutionReport, KlineUpdate, MarkPriceUpdate, MiniTickerUpdate, OrderBookUpdate, RawMessage,
    StreamError, TickerUpdate, TradeUpdate, UserStreamExpired, WsEvent, WsPayload,
};
use events::{SymbolLookup, UserParse};

/// Subscriber queue handle.
pub type EventQueue = Arc<DropOldestQueue<Arc<WsEvent>>>;

/// Async event callback (spawned per event, never blocks the receiver).
pub type EventCallback = Arc<dyn Fn(Arc<WsEvent>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Which market-stream host the manager connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMarket {
    Spot,
    UsdmFutures,
}

/// Manager tuning (Python `BinanceWebSocketConfig` defaults).
#[derive(Debug, Clone)]
pub struct WsConfig {
    pub subscriber_queue_size: usize,
    pub heartbeat_interval: Duration,
    pub message_timeout: Duration,
    /// Timeout for the user-stream subscription response.
    pub subscribe_timeout: Duration,
    pub connect_timeout: Duration,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            subscriber_queue_size: 1_000,
            heartbeat_interval: Duration::from_secs(30),
            message_timeout: Duration::from_secs(90),
            subscribe_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(20),
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

struct Subscriber {
    event_type: EventType,
    queue: EventQueue,
}

struct CallbackEntry {
    id: u64,
    event_type: EventType,
    callback: EventCallback,
}

struct Inner {
    config: WsConfig,
    stream_base: String,
    ws_api_url: String,
    credentials: Option<Credentials>,
    clock: Option<BinanceClient>,
    /// stream name → symbol as given by the caller.
    subscriptions: Mutex<BTreeMap<String, String>>,
    lookup: Mutex<SymbolLookup>,
    subs_version: watch::Sender<u64>,
    control: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    subscribers: Mutex<Vec<Subscriber>>,
    callbacks: Mutex<Vec<CallbackEntry>>,
    next_id: AtomicU64,
    running: AtomicBool,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    public_connected: AtomicBool,
    user_connected: AtomicBool,
    dropped_events: AtomicU64,
    public_connections: AtomicU64,
}

/// Market-data + user-data WebSocket manager. Cheap to clone.
#[derive(Clone)]
pub struct WsManager {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for WsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsManager")
            .field("stream_base", &self.inner.stream_base)
            .field("running", &self.is_running())
            .finish()
    }
}

enum Exit {
    /// We closed on purpose (stale, serverShutdown, no subscriptions, stop): reconnect at once.
    Planned,
    /// Server sent a close frame: reconnect after backoff, no alert.
    ServerClosed,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl WsManager {
    /// Manager for `market` streams using the endpoints/credentials of `config`.
    pub fn new(config: &BinanceConfig, market: StreamMarket, ws_config: WsConfig) -> Self {
        let stream_base = match market {
            StreamMarket::Spot => config.endpoints.spot_ws_stream.clone(),
            StreamMarket::UsdmFutures => config.endpoints.futures_ws_stream.clone(),
        };
        Self {
            inner: Arc::new(Inner {
                config: ws_config,
                stream_base,
                ws_api_url: config.endpoints.spot_ws_api.clone(),
                credentials: config.credentials.clone(),
                clock: None,
                subscriptions: Mutex::new(BTreeMap::new()),
                lookup: Mutex::new(SymbolLookup::default()),
                subs_version: watch::channel(0).0,
                control: Mutex::new(None),
                subscribers: Mutex::new(Vec::new()),
                callbacks: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                running: AtomicBool::new(false),
                stop: watch::channel(false).0,
                tasks: Mutex::new(Vec::new()),
                public_connected: AtomicBool::new(false),
                user_connected: AtomicBool::new(false),
                dropped_events: AtomicU64::new(0),
                public_connections: AtomicU64::new(0),
            }),
        }
    }

    /// Spot manager sharing a REST client's config and server-time offset
    /// (used for the user-stream signature timestamp).
    pub fn from_client(client: &BinanceClient, ws_config: WsConfig) -> Self {
        let mut manager = Self::new(client.config(), StreamMarket::Spot, ws_config);
        if let Some(inner) = Arc::get_mut(&mut manager.inner) {
            inner.clock = Some(client.clone());
        }
        manager
    }

    /// Whether `start` has been called and `stop` has not.
    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::SeqCst)
    }

    /// Whether the public market-stream socket is currently open.
    pub fn public_connected(&self) -> bool {
        self.inner.public_connected.load(Ordering::SeqCst)
    }

    /// Whether the user-data socket is currently subscribed.
    pub fn user_connected(&self) -> bool {
        self.inner.user_connected.load(Ordering::SeqCst)
    }

    /// Successful public connections so far (reconnect observability).
    pub fn public_connection_count(&self) -> u64 {
        self.inner.public_connections.load(Ordering::SeqCst)
    }

    /// Events evicted from full subscriber queues.
    pub fn dropped_events(&self) -> u64 {
        self.inner.dropped_events.load(Ordering::Relaxed)
    }

    /// Current stream names, sorted.
    pub fn subscriptions(&self) -> Vec<String> {
        lock(&self.inner.subscriptions).keys().cloned().collect()
    }

    // ----- subscriber registry -------------------------------------------------

    /// Register a drop-oldest queue for `event_type` (`maxsize` defaults to the config).
    pub fn subscribe_queue(&self, event_type: EventType, maxsize: Option<usize>) -> EventQueue {
        let queue = Arc::new(DropOldestQueue::new(
            maxsize.unwrap_or(self.inner.config.subscriber_queue_size),
        ));
        lock(&self.inner.subscribers).push(Subscriber {
            event_type,
            queue: queue.clone(),
        });
        queue
    }

    /// Remove a queue registered with [`Self::subscribe_queue`].
    pub fn unsubscribe_queue(&self, queue: &EventQueue) {
        lock(&self.inner.subscribers).retain(|s| !Arc::ptr_eq(&s.queue, queue));
    }

    /// Register an async callback; returns an id for [`Self::unsubscribe`].
    pub fn subscribe<F, Fut>(&self, event_type: EventType, callback: F) -> u64
    where
        F: Fn(Arc<WsEvent>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let callback: EventCallback = Arc::new(move |event| Box::pin(callback(event)));
        lock(&self.inner.callbacks).push(CallbackEntry {
            id,
            event_type,
            callback,
        });
        id
    }

    /// Remove a callback.
    pub fn unsubscribe(&self, id: u64) {
        lock(&self.inner.callbacks).retain(|c| c.id != id);
    }

    /// Deliver an event to matching queues and callbacks.
    pub fn emit(&self, event: WsEvent) {
        let event = Arc::new(event);
        let kind = event.event_type();
        for sub in lock(&self.inner.subscribers)
            .iter()
            .filter(|s| s.event_type == kind)
        {
            if sub.queue.push(event.clone()) {
                let dropped = self.inner.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped.is_multiple_of(100) {
                    tracing::warn!(
                        dropped_events = dropped,
                        "Binance WebSocket event queue is dropping events"
                    );
                }
            }
        }
        let callbacks: Vec<EventCallback> = lock(&self.inner.callbacks)
            .iter()
            .filter(|c| c.event_type == kind)
            .map(|c| c.callback.clone())
            .collect();
        for callback in callbacks {
            let event = event.clone();
            tokio::spawn(async move { callback(event).await });
        }
    }

    // ----- public stream subscriptions ----------------------------------------

    fn clean(symbol: &str) -> Result<String> {
        let cleaned = symbol.trim().to_uppercase();
        if cleaned.is_empty() {
            return Err(BinanceError::InvalidSymbol(
                "symbol must not be blank".into(),
            ));
        }
        Ok(cleaned)
    }

    fn streams_for(symbols: &[&str], suffix: &str) -> Result<Vec<(String, String)>> {
        symbols
            .iter()
            .map(|s| {
                Ok((
                    format!("{}@{suffix}", stream_symbol(&Self::clean(s)?)?),
                    Self::clean(s)?,
                ))
            })
            .collect()
    }

    /// `<symbol>@ticker` (24h rolling ticker).
    pub async fn subscribe_ticker(&self, symbols: &[&str]) -> Result<()> {
        self.add_streams(Self::streams_for(symbols, "ticker")?)
    }

    /// `<symbol>@miniTicker`.
    pub async fn subscribe_mini_ticker(&self, symbols: &[&str]) -> Result<()> {
        self.add_streams(Self::streams_for(symbols, "miniTicker")?)
    }

    /// `<symbol>@bookTicker`.
    pub async fn subscribe_book_ticker(&self, symbols: &[&str]) -> Result<()> {
        self.add_streams(Self::streams_for(symbols, "bookTicker")?)
    }

    /// `<symbol>@trade`.
    pub async fn subscribe_trades(&self, symbols: &[&str]) -> Result<()> {
        self.add_streams(Self::streams_for(symbols, "trade")?)
    }

    /// `<symbol>@aggTrade`.
    pub async fn subscribe_agg_trades(&self, symbols: &[&str]) -> Result<()> {
        self.add_streams(Self::streams_for(symbols, "aggTrade")?)
    }

    /// Futures `<symbol>@markPrice@1s`.
    pub async fn subscribe_mark_price(&self, symbols: &[&str]) -> Result<()> {
        self.add_streams(Self::streams_for(symbols, "markPrice@1s")?)
    }

    fn depth_suffix(depth: u32, update_interval_ms: u32) -> Result<String> {
        if ![5, 10, 20].contains(&depth) {
            return Err(BinanceError::InvalidRequest(
                "depth must be one of 5, 10, or 20".into(),
            ));
        }
        if ![100, 1000].contains(&update_interval_ms) {
            return Err(BinanceError::InvalidRequest(
                "update_interval_ms must be 100 or 1000".into(),
            ));
        }
        Ok(if update_interval_ms == 100 {
            format!("depth{depth}@100ms")
        } else {
            format!("depth{depth}")
        })
    }

    /// `<symbol>@depth<N>[@100ms]` partial book (depth 5/10/20, 100 or 1000 ms).
    pub async fn subscribe_order_book(
        &self,
        symbols: &[&str],
        depth: u32,
        update_interval_ms: u32,
    ) -> Result<()> {
        self.add_streams(Self::streams_for(
            symbols,
            &Self::depth_suffix(depth, update_interval_ms)?,
        )?)
    }

    fn interval_suffix(interval: &str) -> Result<String> {
        let interval = interval.trim();
        if interval.is_empty() {
            return Err(BinanceError::InvalidRequest(
                "interval must not be blank".into(),
            ));
        }
        Ok(format!("kline_{interval}"))
    }

    /// `<symbol>@kline_<interval>`.
    pub async fn subscribe_klines(&self, symbols: &[&str], interval: &str) -> Result<()> {
        self.add_streams(Self::streams_for(
            symbols,
            &Self::interval_suffix(interval)?,
        )?)
    }

    /// Ticker and/or partial book for each symbol.
    pub async fn subscribe_market_data(
        &self,
        symbols: &[&str],
        ticker: bool,
        order_book: bool,
        order_book_depth: u32,
        order_book_update_interval_ms: u32,
    ) -> Result<()> {
        if ticker {
            self.subscribe_ticker(symbols).await?;
        }
        if order_book {
            self.subscribe_order_book(symbols, order_book_depth, order_book_update_interval_ms)
                .await?;
        }
        Ok(())
    }

    /// Subscribe raw stream names (e.g. `btcusdt@depth@100ms`, `!miniTicker@arr`).
    pub async fn subscribe_streams(&self, streams: &[&str]) -> Result<()> {
        self.add_streams(
            streams
                .iter()
                .map(|s| {
                    let prefix = s.split('@').next().unwrap_or_default().to_uppercase();
                    (s.to_string(), prefix)
                })
                .collect(),
        )
    }

    /// Undo [`Self::subscribe_ticker`].
    pub async fn unsubscribe_ticker(&self, symbols: &[&str]) -> Result<()> {
        self.remove_streams(Self::streams_for(symbols, "ticker")?)
    }

    /// Undo [`Self::subscribe_book_ticker`].
    pub async fn unsubscribe_book_ticker(&self, symbols: &[&str]) -> Result<()> {
        self.remove_streams(Self::streams_for(symbols, "bookTicker")?)
    }

    /// Undo [`Self::subscribe_order_book`].
    pub async fn unsubscribe_order_book(
        &self,
        symbols: &[&str],
        depth: u32,
        update_interval_ms: u32,
    ) -> Result<()> {
        let suffix = if update_interval_ms == 100 {
            format!("depth{depth}@100ms")
        } else {
            format!("depth{depth}")
        };
        self.remove_streams(Self::streams_for(symbols, &suffix)?)
    }

    /// Undo [`Self::subscribe_klines`].
    pub async fn unsubscribe_klines(&self, symbols: &[&str], interval: &str) -> Result<()> {
        self.remove_streams(Self::streams_for(
            symbols,
            &Self::interval_suffix(interval)?,
        )?)
    }

    /// Undo [`Self::subscribe_streams`].
    pub async fn unsubscribe_streams(&self, streams: &[&str]) -> Result<()> {
        self.remove_streams(
            streams
                .iter()
                .map(|s| (s.to_string(), String::new()))
                .collect(),
        )
    }

    fn add_streams(&self, streams: Vec<(String, String)>) -> Result<()> {
        let mut added = Vec::new();
        {
            let mut subs = lock(&self.inner.subscriptions);
            let mut lookup = lock(&self.inner.lookup);
            for (stream, symbol) in streams {
                if subs.contains_key(&stream) {
                    continue;
                }
                let id = stream.split('@').next().unwrap_or_default().to_uppercase();
                lookup.0.insert(id, symbol.clone());
                subs.insert(stream.clone(), symbol);
                added.push(stream);
            }
        }
        if added.is_empty() {
            return Ok(());
        }
        tracing::info!(streams = ?added, "Binance public streams subscribed");
        self.send_control("SUBSCRIBE", &added);
        self.inner.subs_version.send_modify(|v| *v += 1);
        Ok(())
    }

    fn remove_streams(&self, streams: Vec<(String, String)>) -> Result<()> {
        let removed: Vec<String> = {
            let mut subs = lock(&self.inner.subscriptions);
            streams
                .into_iter()
                .filter_map(|(stream, _)| subs.remove(&stream).map(|_| stream))
                .collect()
        };
        if removed.is_empty() {
            return Ok(());
        }
        tracing::info!(streams = ?removed, "Binance public streams unsubscribed");
        self.send_control("UNSUBSCRIBE", &removed);
        self.inner.subs_version.send_modify(|v| *v += 1);
        Ok(())
    }

    fn send_control(&self, method: &str, streams: &[String]) {
        if let Some(tx) = lock(&self.inner.control).as_ref() {
            let _ = tx.send(control_message(method, streams));
        }
    }

    // ----- lifecycle ------------------------------------------------------------

    /// Start the public-stream task (and the user-data task when `user_stream`).
    /// Subscriptions may be added before or after. Idempotent while running.
    pub async fn start(&self, user_stream: bool) -> Result<()> {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if user_stream && self.inner.credentials.is_none() {
            self.inner.running.store(false, Ordering::SeqCst);
            return Err(BinanceError::Config(
                "api_key and api_secret are required for Binance user data stream".into(),
            ));
        }
        self.inner.stop.send_replace(false);
        let mut tasks = lock(&self.inner.tasks);
        let public = self.clone();
        tasks.push(tokio::spawn(async move { public.public_loop().await }));
        if user_stream {
            let user = self.clone();
            tasks.push(tokio::spawn(async move { user.user_loop().await }));
        }
        tracing::info!(
            user_stream,
            public_subscriptions = self.subscriptions().len(),
            "Binance WebSocket manager started"
        );
        Ok(())
    }

    /// Stop all loops and close sockets.
    pub async fn stop(&self) {
        self.inner.running.store(false, Ordering::SeqCst);
        self.inner.stop.send_replace(true);
        let tasks: Vec<JoinHandle<()>> = lock(&self.inner.tasks).drain(..).collect();
        for task in tasks {
            if tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
            {
                tracing::warn!("Binance WebSocket task did not stop in time");
            }
        }
        *lock(&self.inner.control) = None;
        tracing::info!("Binance WebSocket manager stopped");
    }

    fn stopping(&self) -> bool {
        *self.inner.stop.borrow()
    }

    async fn sleep_or_stop(&self, delay: Duration) {
        let mut stop = self.inner.stop.subscribe();
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stopped(&mut stop) => {}
        }
    }

    fn next_delay(&self, attempt: &mut u32) -> Duration {
        *attempt += 1;
        backoff_delay(
            *attempt,
            self.inner.config.base_backoff,
            self.inner.config.max_backoff,
            rand::random::<f64>(),
        )
    }

    async fn public_loop(self) {
        let mut attempt = 0u32;
        let mut versions = self.inner.subs_version.subscribe();
        let mut stop = self.inner.stop.subscribe();
        while !self.stopping() {
            while lock(&self.inner.subscriptions).is_empty() {
                tokio::select! {
                    _ = versions.changed() => {}
                    _ = stopped(&mut stop) => return,
                }
            }
            let streams = self.subscriptions();
            let url = format!(
                "{}/stream?streams={}",
                self.inner.stream_base,
                streams.join("/")
            );
            tracing::info!(
                stream_count = streams.len(),
                "Connecting Binance public WebSocket"
            );
            let result = match tokio::time::timeout(
                self.inner.config.connect_timeout,
                tokio_tungstenite::connect_async(url.as_str()),
            )
            .await
            {
                Err(_) => Err(("TimeoutError".to_owned(), "connect timed out".to_owned())),
                Ok(Err(err)) => Err(("ConnectError".to_owned(), err.to_string())),
                Ok(Ok((socket, _))) => {
                    attempt = 0;
                    self.inner.public_connections.fetch_add(1, Ordering::SeqCst);
                    self.inner.public_connected.store(true, Ordering::SeqCst);
                    tracing::info!(
                        stream_count = streams.len(),
                        "Binance public WebSocket connected"
                    );
                    let outcome = self.run_public(socket, &streams).await;
                    self.inner.public_connected.store(false, Ordering::SeqCst);
                    *lock(&self.inner.control) = None;
                    outcome
                }
            };
            if self.stopping() {
                break;
            }
            match result {
                Ok(Exit::Planned) => {}
                Ok(Exit::ServerClosed) => {
                    let delay = self.next_delay(&mut attempt);
                    tracing::info!(
                        sleep_ms = delay.as_millis() as u64,
                        "Binance public WebSocket closed by server; reconnecting"
                    );
                    self.sleep_or_stop(delay).await;
                }
                Err((error_type, message)) => {
                    let delay = self.next_delay(&mut attempt);
                    tracing::warn!(sleep_ms = delay.as_millis() as u64, error_type = %error_type, error = %message, "Binance public WebSocket disconnected; reconnecting");
                    self.emit(WsEvent::stream_error(
                        "public",
                        "public WebSocket disconnected",
                        Some(&error_type),
                        Value::Null,
                    ));
                    self.sleep_or_stop(delay).await;
                }
            }
        }
        tracing::debug!("Binance public stream loop exited");
    }

    async fn run_public<S>(
        &self,
        socket: S,
        connected: &[String],
    ) -> std::result::Result<Exit, (String, String)>
    where
        S: futures::Stream<
                Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
            > + futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
            + Unpin,
    {
        let ws_err =
            |e: tokio_tungstenite::tungstenite::Error| ("WebSocketError".to_owned(), e.to_string());
        let (mut sink, mut stream) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel();
        *lock(&self.inner.control) = Some(tx);

        // Reconcile subscription changes that happened while connecting.
        let current = self.subscriptions();
        if current.is_empty() {
            let _ = sink.send(Message::Close(None)).await;
            return Ok(Exit::Planned);
        }
        let stale: Vec<String> = connected
            .iter()
            .filter(|s| !current.contains(s))
            .cloned()
            .collect();
        let fresh: Vec<String> = current
            .iter()
            .filter(|s| !connected.contains(s))
            .cloned()
            .collect();
        if !stale.is_empty() {
            sink.send(control_message("UNSUBSCRIBE", &stale))
                .await
                .map_err(ws_err)?;
        }
        if !fresh.is_empty() {
            sink.send(control_message("SUBSCRIBE", &fresh))
                .await
                .map_err(ws_err)?;
        }

        let mut stop = self.inner.stop.subscribe();
        let mut versions = self.inner.subs_version.subscribe();
        let mut heartbeat = tokio::time::interval_at(
            Instant::now() + self.inner.config.heartbeat_interval,
            self.inner.config.heartbeat_interval,
        );
        let mut last_message = Instant::now();
        loop {
            tokio::select! {
                _ = stopped(&mut stop) => {
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(Exit::Planned);
                }
                Some(msg) = rx.recv() => {
                    sink.send(msg).await.map_err(ws_err)?;
                }
                changed = versions.changed() => {
                    if changed.is_ok() && lock(&self.inner.subscriptions).is_empty() {
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(Exit::Planned);
                    }
                }
                _ = heartbeat.tick() => {
                    let idle = last_message.elapsed();
                    tracing::debug!(public_last_message_ms = idle.as_millis() as u64, "Binance WebSocket heartbeat");
                    if idle > self.inner.config.message_timeout {
                        tracing::warn!(idle_ms = idle.as_millis() as u64, "Binance public WebSocket heartbeat timeout");
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(Exit::Planned);
                    }
                    sink.send(Message::Ping(Vec::new().into())).await.map_err(ws_err)?;
                }
                next = stream.next() => {
                    match next {
                        None => return Err(("ConnectionClosed".into(), "public WebSocket ended without close frame".into())),
                        Some(Err(err)) => return Err(ws_err(err)),
                        Some(Ok(Message::Text(text))) => {
                            last_message = Instant::now();
                            if self.handle_public_message(text.as_str()) {
                                let _ = sink.send(Message::Close(None)).await;
                                return Ok(Exit::Planned);
                            }
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            last_message = Instant::now();
                            if self.handle_public_message(&String::from_utf8_lossy(&bytes)) {
                                let _ = sink.send(Message::Close(None)).await;
                                return Ok(Exit::Planned);
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            sink.send(Message::Pong(payload)).await.map_err(ws_err)?;
                        }
                        Some(Ok(Message::Close(_))) => return Ok(Exit::ServerClosed),
                        Some(Ok(_)) => {}
                    }
                }
            }
        }
    }

    /// Parse and dispatch one public message. Returns `true` when the
    /// connection should be recycled (`serverShutdown`).
    pub fn handle_public_message(&self, text: &str) -> bool {
        let message: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                self.emit(WsEvent::stream_error(
                    "public",
                    "invalid JSON message",
                    Some("JSONDecodeError"),
                    Value::Null,
                ));
                return false;
            }
        };
        if let Value::Object(obj) = &message
            && obj.contains_key("id")
            && (obj.contains_key("result") || obj.contains_key("error"))
        {
            tracing::debug!(response = %message, "Binance public WebSocket control response received");
            return false;
        }
        let stream = message
            .get("stream")
            .and_then(Value::as_str)
            .unwrap_or("public")
            .to_owned();
        let data = message.get("data").unwrap_or(&message);
        let Value::Object(data) = data else {
            self.emit(WsEvent::raw("public", message.clone()));
            return false;
        };
        if data.get("e").and_then(Value::as_str) == Some("serverShutdown") {
            self.emit(WsEvent::raw(&stream, Value::Object(data.clone())));
            return true;
        }
        let lookup = lock(&self.inner.lookup).clone();
        match events::parse_public(&stream, data, &lookup) {
            Ok(payload) => self.emit(WsEvent::new(payload)),
            Err(err) => {
                tracing::error!(stream = %stream, error = %err.0, "Failed to validate public WebSocket payload");
                self.emit(WsEvent::stream_error(
                    &stream,
                    "failed to validate public WebSocket payload",
                    Some("ValueError"),
                    Value::Object(data.clone()),
                ));
            }
        }
        false
    }

    /// Parse and dispatch one user-data message. Returns `true` when the
    /// stream was terminated and must be re-subscribed.
    pub fn handle_user_message(&self, text: &str) -> bool {
        let message: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                self.emit(WsEvent::stream_error(
                    "user",
                    "invalid JSON message",
                    Some("JSONDecodeError"),
                    Value::Null,
                ));
                return false;
            }
        };
        let Value::Object(obj) = &message else {
            self.emit(WsEvent::raw("user", json!({ "message": message })));
            return false;
        };
        let lookup = lock(&self.inner.lookup).clone();
        match events::parse_user(obj, &lookup) {
            Ok(UserParse::Event(payload)) => {
                self.emit(WsEvent::new(payload));
                false
            }
            Ok(UserParse::Terminated(payload)) => {
                tracing::warn!("Binance user-data stream terminated");
                self.emit(WsEvent::new(payload));
                true
            }
            Err(err) => {
                tracing::error!(error = %err.0, "Failed to validate user-data payload");
                self.emit(WsEvent::stream_error(
                    "user",
                    "failed to validate user-data payload",
                    Some("ValueError"),
                    message.clone(),
                ));
                false
            }
        }
    }

    /// `userDataStream.subscribe.signature` request (params sorted, signed).
    pub fn user_subscribe_request(&self) -> Result<Value> {
        let credentials = self.inner.credentials.as_ref().ok_or_else(|| {
            BinanceError::Config("api_key is required for Binance user data stream".into())
        })?;
        let timestamp = match &self.inner.clock {
            Some(client) => client.server_now_ms(Api::Spot),
            None => bb_core::time::now_ms(),
        };
        Ok(user_subscribe_payload(credentials, timestamp))
    }

    async fn user_loop(self) {
        let mut attempt = 0u32;
        while !self.stopping() {
            tracing::info!("Connecting Binance WebSocket API user-data stream");
            let result = self.run_user().await;
            self.inner.user_connected.store(false, Ordering::SeqCst);
            if self.stopping() {
                break;
            }
            match result {
                Ok(()) => attempt = 0,
                Err((error_type, message)) => {
                    let delay = self.next_delay(&mut attempt);
                    tracing::warn!(sleep_ms = delay.as_millis() as u64, error_type = %error_type, error = %message, "Binance user-data WebSocket disconnected; reconnecting");
                    self.emit(WsEvent::stream_error(
                        "user",
                        "user-data WebSocket disconnected",
                        Some(&error_type),
                        Value::Null,
                    ));
                    self.sleep_or_stop(delay).await;
                }
            }
        }
        tracing::debug!("Binance user stream loop exited");
    }

    async fn run_user(&self) -> std::result::Result<(), (String, String)> {
        let ws_err =
            |e: tokio_tungstenite::tungstenite::Error| ("WebSocketError".to_owned(), e.to_string());
        let (socket, _) = tokio::time::timeout(
            self.inner.config.connect_timeout,
            tokio_tungstenite::connect_async(self.inner.ws_api_url.as_str()),
        )
        .await
        .map_err(|_| ("TimeoutError".to_owned(), "connect timed out".to_owned()))?
        .map_err(|e| ("ConnectError".to_owned(), e.to_string()))?;
        let (mut sink, mut stream) = socket.split();
        let request = self
            .user_subscribe_request()
            .map_err(|e| ("ConfigError".to_owned(), e.to_string()))?;
        sink.send(Message::Text(request.to_string().into()))
            .await
            .map_err(ws_err)?;

        let deadline = Instant::now() + self.inner.config.subscribe_timeout;
        let response = loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => {
                    return Err((
                        "TimeoutError".into(),
                        "user-data subscription timed out".into(),
                    ));
                }
                Ok(None) => {
                    return Err((
                        "ConnectionClosed".into(),
                        "closed before subscription response".into(),
                    ));
                }
                Ok(Some(Err(e))) => return Err(ws_err(e)),
                Ok(Some(Ok(Message::Text(text)))) => break text.to_string(),
                Ok(Some(Ok(Message::Ping(p)))) => {
                    sink.send(Message::Pong(p)).await.map_err(ws_err)?
                }
                Ok(Some(Ok(_))) => {
                    return Err((
                        "BinanceWebSocketError".into(),
                        "Binance user-data subscription did not return a text response".into(),
                    ));
                }
            }
        };
        check_subscribe_response(&response)?;
        self.inner.user_connected.store(true, Ordering::SeqCst);
        tracing::info!("Binance WebSocket API user-data stream subscribed");

        let mut stop = self.inner.stop.subscribe();
        let mut heartbeat = tokio::time::interval_at(
            Instant::now() + self.inner.config.heartbeat_interval,
            self.inner.config.heartbeat_interval,
        );
        loop {
            tokio::select! {
                _ = stopped(&mut stop) => {
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = heartbeat.tick() => {
                    sink.send(Message::Ping(Vec::new().into())).await.map_err(ws_err)?;
                }
                next = stream.next() => match next {
                    None => return Err(("ConnectionClosed".into(), "user-data WebSocket ended".into())),
                    Some(Err(e)) => return Err(ws_err(e)),
                    Some(Ok(Message::Text(text))) => {
                        if self.handle_user_message(text.as_str()) {
                            let _ = sink.send(Message::Close(None)).await;
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if self.handle_user_message(&String::from_utf8_lossy(&bytes)) {
                            let _ = sink.send(Message::Close(None)).await;
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Ping(p))) => sink.send(Message::Pong(p)).await.map_err(ws_err)?,
                    Some(Ok(Message::Close(_))) => {
                        return Err(("ConnectionClosed".into(), "user-data WebSocket closed by server".into()));
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Resolves once the stop flag is set (drops the non-`Send` watch guard immediately).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

fn control_message(method: &str, streams: &[String]) -> Message {
    let id = bb_core::time::now_ms() % 1_000_000_000;
    Message::Text(
        json!({ "method": method, "params": streams, "id": id })
            .to_string()
            .into(),
    )
}

/// Build the signed `userDataStream.subscribe.signature` request.
pub fn user_subscribe_payload(credentials: &Credentials, timestamp_ms: i64) -> Value {
    let payload = format!("apiKey={}&timestamp={timestamp_ms}", credentials.api_key());
    json!({
        "id": format!("user-stream-{timestamp_ms}"),
        "method": "userDataStream.subscribe.signature",
        "params": {
            "apiKey": credentials.api_key(),
            "timestamp": timestamp_ms,
            "signature": credentials.sign(&payload),
        }
    })
}

fn check_subscribe_response(text: &str) -> std::result::Result<(), (String, String)> {
    let response: Value = serde_json::from_str(text).map_err(|_| {
        (
            "BinanceWebSocketError".to_owned(),
            "Binance user-data subscription response was not valid JSON".to_owned(),
        )
    })?;
    if response.get("status").and_then(Value::as_i64) != Some(200) {
        let error = response.get("error").cloned().unwrap_or(Value::Null);
        return Err((
            "BinanceWebSocketError".into(),
            format!("Binance user-data subscription failed: {error}"),
        ));
    }
    if response
        .get("result")
        .and_then(|r| r.get("subscriptionId"))
        .is_none()
    {
        return Err((
            "BinanceWebSocketError".into(),
            "Binance user-data subscription response omitted subscriptionId".into(),
        ));
    }
    Ok(())
}

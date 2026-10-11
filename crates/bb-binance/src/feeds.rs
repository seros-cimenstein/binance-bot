//! Market-data feeds (port of `src/data/feeds.py` and `src/data/websocket_feed.py`).
//!
//! - [`PollingCandleFeed`]: REST kline polling (`MARKET_DATA_MODE=polling`).
//! - [`WebSocketCandleFeed`]: closed candles from `@kline` streams (`MARKET_DATA_MODE=websocket`).
//! - [`subscribe_tickers`]: normalised best bid/ask/last updates from `@ticker` streams.
//! - [`MarketDataFeed`]: either candle feed behind one `subscribe_ohlcv`.

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{BinanceError, ErrorKind, Result};
use crate::models::Kline;
use crate::ratelimit::backoff_delay;
use crate::rest::{Api, BinanceClient};
use crate::symbols::normalize_symbol;
use crate::ws::{EventQueue, EventType, WsManager, WsPayload};

/// Normalised best bid/ask and last-trade data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickerData {
    pub symbol: String,
    pub timestamp: i64,
    pub bid: Decimal,
    pub ask: Decimal,
    pub last: Decimal,
    pub volume: Decimal,
}

/// Normalised OHLCV candle (timestamp = open time, ms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandleData {
    pub symbol: String,
    pub timestamp: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
}

/// Seconds per timeframe (Python `_TIMEFRAME_SECONDS`); `None` for unknown timeframes.
pub fn timeframe_seconds(timeframe: &str) -> Option<u64> {
    Some(match timeframe {
        "1m" => 60,
        "3m" => 180,
        "5m" => 300,
        "15m" => 900,
        "30m" => 1_800,
        "1h" => 3_600,
        "2h" => 7_200,
        "4h" => 14_400,
        "6h" => 21_600,
        "8h" => 28_800,
        "12h" => 43_200,
        "1d" => 86_400,
        "3d" => 259_200,
        "1w" => 604_800,
        _ => return None,
    })
}

fn checked_timeframe(timeframe: &str) -> Result<String> {
    let tf = timeframe.trim();
    if tf.is_empty() {
        return Err(BinanceError::InvalidRequest(
            "timeframe must not be blank".into(),
        ));
    }
    Ok(tf.to_owned())
}

/// Reconnect/retry policy shared by the feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedBackoff {
    pub base: Duration,
    pub max: Duration,
    /// Consecutive retry limit; `None` retries forever.
    pub max_retries: Option<u32>,
}

impl Default for FeedBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(60),
            max_retries: None,
        }
    }
}

impl FeedBackoff {
    /// Validate like Python `_ExponentialBackoff.__init__`.
    pub fn validate(&self) -> Result<()> {
        if self.base.is_zero() || self.max.is_zero() {
            return Err(BinanceError::InvalidRequest(
                "backoff durations must be greater than zero".into(),
            ));
        }
        if self.max < self.base {
            return Err(BinanceError::InvalidRequest(
                "max_backoff_seconds must be greater than or equal to base_backoff_seconds".into(),
            ));
        }
        Ok(())
    }
}

/// Errors the polling feed retries: transport, 5xx, rate limits and other
/// exchange-side failures (Python retried ccxt `NetworkError`/`ExchangeError`);
/// never configuration, authentication or bad-request errors.
fn feed_retryable(err: &BinanceError) -> bool {
    !matches!(
        err.kind(),
        ErrorKind::Authentication | ErrorKind::BadRequest | ErrorKind::InvalidOrder
    ) && !matches!(err, BinanceError::Config(_))
}

/// Pick the most recent kline whose close time has passed, and how long to wait
/// until the next one closes (plus a 1 s grace, clamped to `[1 s, interval]`).
pub fn latest_closed(
    symbol: &str,
    rows: &[Kline],
    now_ms: i64,
    interval: Duration,
) -> (Option<CandleData>, Duration) {
    let interval_ms = i64::try_from(interval.as_millis()).unwrap_or(i64::MAX);
    let closed = rows
        .iter()
        .filter(|k| k.close_time.unwrap_or(k.open_time + interval_ms - 1) < now_ms)
        .max_by_key(|k| k.open_time);
    let candle = closed.map(|k| CandleData {
        symbol: symbol.to_owned(),
        timestamp: k.open_time,
        open: k.open,
        high: k.high,
        low: k.low,
        close: k.close,
        volume: k.volume,
    });
    let next_close = closed
        .map(|k| k.close_time.unwrap_or(k.open_time + interval_ms - 1) + interval_ms)
        .unwrap_or(now_ms + interval_ms);
    let wait_ms = (next_close + 1_000 - now_ms).clamp(1_000, interval_ms.max(1_000));
    (
        candle,
        Duration::from_millis(u64::try_from(wait_ms).unwrap_or(1_000)),
    )
}

/// REST-polling candle feed (port of `BinanceDataFeed.subscribe_ohlcv`).
#[derive(Debug, Clone)]
pub struct PollingCandleFeed {
    client: BinanceClient,
    backoff: FeedBackoff,
}

struct PollState {
    client: BinanceClient,
    symbol: String,
    timeframe: String,
    interval: Duration,
    backoff: FeedBackoff,
    attempt: u32,
    last_timestamp: Option<i64>,
    pending_sleep: Option<Duration>,
    done: bool,
}

impl PollingCandleFeed {
    /// Feed over `client` with the given retry policy.
    pub fn new(client: BinanceClient, backoff: FeedBackoff) -> Result<Self> {
        backoff.validate()?;
        Ok(Self { client, backoff })
    }

    /// Yield each newly **closed** candle for `symbol`/`timeframe`.
    ///
    /// Polls the last two klines, picks the newest closed one (server clock)
    /// and sleeps until just after the next close. Retryable failures back off
    /// exponentially; after `max_retries` consecutive failures the error is
    /// yielded and the stream ends.
    pub fn subscribe_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
    ) -> Result<BoxStream<'static, Result<CandleData>>> {
        let symbol = normalize_symbol(symbol)?;
        let timeframe = checked_timeframe(timeframe)?;
        tracing::info!(symbol = %symbol, timeframe = %timeframe, stream = "ohlcv", "Binance OHLCV stream subscribed");
        let state = PollState {
            client: self.client.clone(),
            interval: Duration::from_secs(timeframe_seconds(&timeframe).unwrap_or(60)),
            symbol,
            timeframe,
            backoff: self.backoff,
            attempt: 0,
            last_timestamp: None,
            pending_sleep: None,
            done: false,
        };
        Ok(stream::unfold(state, poll_next).boxed())
    }
}

async fn poll_next(mut st: PollState) -> Option<(Result<CandleData>, PollState)> {
    loop {
        if st.done {
            return None;
        }
        if let Some(pause) = st.pending_sleep.take() {
            tokio::time::sleep(pause).await;
        }
        let rows = match st
            .client
            .klines(&st.symbol, &st.timeframe, None, None, Some(2))
            .await
        {
            Ok(rows) => rows,
            Err(err) if feed_retryable(&err) => {
                if st.backoff.max_retries.is_some_and(|max| st.attempt >= max) {
                    tracing::error!(symbol = %st.symbol, attempts = st.attempt, error = %err, "Binance stream retries exhausted");
                    st.done = true;
                    return Some((Err(err), st));
                }
                st.attempt += 1;
                let delay = backoff_delay(
                    st.attempt,
                    st.backoff.base,
                    st.backoff.max,
                    rand::random::<f64>(),
                );
                tracing::warn!(symbol = %st.symbol, attempt = st.attempt, sleep_ms = delay.as_millis() as u64, error = %err, "Binance stream dropped; reconnecting");
                st.pending_sleep = Some(delay);
                continue;
            }
            Err(err) => {
                st.done = true;
                return Some((Err(err), st));
            }
        };
        if st.attempt > 0 {
            tracing::info!(symbol = %st.symbol, attempts = st.attempt, "Binance stream reconnected");
        }
        st.attempt = 0;
        let now = st.client.server_now_ms(Api::Spot);
        let (candle, wait) = latest_closed(&st.symbol, &rows, now, st.interval);
        st.pending_sleep = Some(wait);
        if let Some(candle) = candle
            && Some(candle.timestamp) != st.last_timestamp
        {
            st.last_timestamp = Some(candle.timestamp);
            return Some((Ok(candle), st));
        }
    }
}

/// Unsubscribes and stops the manager when a WebSocket feed stream is dropped.
struct WsFeedGuard {
    manager: WsManager,
    queue: EventQueue,
    symbols: Vec<String>,
    kind: GuardKind,
}

enum GuardKind {
    Klines(String),
    Ticker,
}

impl Drop for WsFeedGuard {
    fn drop(&mut self) {
        self.manager.unsubscribe_queue(&self.queue);
        let manager = self.manager.clone();
        let symbols = std::mem::take(&mut self.symbols);
        let kind = std::mem::replace(&mut self.kind, GuardKind::Ticker);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let refs: Vec<&str> = symbols.iter().map(String::as_str).collect();
                let result = match &kind {
                    GuardKind::Klines(tf) => manager.unsubscribe_klines(&refs, tf).await,
                    GuardKind::Ticker => manager.unsubscribe_ticker(&refs).await,
                };
                if let Err(err) = result {
                    tracing::warn!(error = %err, "Binance feed unsubscribe failed");
                }
                manager.stop().await;
            });
        }
    }
}

/// Closed candles from `@kline` WebSocket streams (port of `BinanceWebSocketCandleFeed`).
#[derive(Debug, Clone)]
pub struct WebSocketCandleFeed {
    manager: WsManager,
    queue_maxsize: usize,
}

impl WebSocketCandleFeed {
    /// Feed over `manager`; per-subscription queue bound `queue_maxsize` (0 = unbounded).
    pub fn new(manager: WsManager, queue_maxsize: usize) -> Self {
        Self {
            manager,
            queue_maxsize,
        }
    }

    /// The underlying manager (to attach stream-failure alert callbacks).
    pub fn manager(&self) -> &WsManager {
        &self.manager
    }

    /// Subscribe, start the manager (public streams only) and yield closed,
    /// de-duplicated candles for exactly this symbol/timeframe. Dropping the
    /// stream unsubscribes and stops the manager.
    pub async fn subscribe_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
    ) -> Result<BoxStream<'static, Result<CandleData>>> {
        let timeframe = checked_timeframe(timeframe)?;
        let symbol = normalize_symbol(symbol)?;
        let queue = self
            .manager
            .subscribe_queue(EventType::KlineUpdate, Some(self.queue_maxsize));
        let guard = WsFeedGuard {
            manager: self.manager.clone(),
            queue: queue.clone(),
            symbols: vec![symbol.clone()],
            kind: GuardKind::Klines(timeframe.clone()),
        };
        self.manager
            .subscribe_klines(&[symbol.as_str()], &timeframe)
            .await?;
        self.manager.start(false).await?;
        let state = (guard, queue, symbol, timeframe, None::<i64>);
        Ok(stream::unfold(
            state,
            |(guard, queue, symbol, timeframe, mut last)| async move {
                loop {
                    let event = queue.pop().await;
                    let WsPayload::Kline(k) = &event.payload else {
                        continue;
                    };
                    if normalize_symbol(&k.symbol).ok().as_deref() != Some(symbol.as_str())
                        || k.interval != timeframe
                        || !k.is_closed
                        || last == Some(k.open_time)
                    {
                        continue;
                    }
                    last = Some(k.open_time);
                    let candle = CandleData {
                        symbol: symbol.clone(),
                        timestamp: k.open_time,
                        open: k.open,
                        high: k.high,
                        low: k.low,
                        close: k.close,
                        volume: k.volume,
                    };
                    return Some((Ok(candle), (guard, queue, symbol, timeframe, last)));
                }
            },
        )
        .boxed())
    }
}

/// Stream normalised tickers for `symbols` from `@ticker` streams (port of
/// `BinanceDataFeed.subscribe_tickers`). Symbols are normalised and de-duplicated;
/// payloads without bid/ask are dropped with a warning; the queue drops the
/// oldest update when full.
pub async fn subscribe_tickers(
    manager: &WsManager,
    symbols: &[&str],
    queue_maxsize: usize,
) -> Result<BoxStream<'static, TickerData>> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for symbol in symbols {
        let n = normalize_symbol(symbol)?;
        if seen.insert(n.clone()) {
            normalized.push(n);
        }
    }
    if normalized.is_empty() {
        return Err(BinanceError::InvalidRequest(
            "symbols must not be empty".into(),
        ));
    }
    let queue = manager.subscribe_queue(EventType::TickerUpdate, Some(queue_maxsize));
    let guard = WsFeedGuard {
        manager: manager.clone(),
        queue: queue.clone(),
        symbols: normalized.clone(),
        kind: GuardKind::Ticker,
    };
    let refs: Vec<&str> = normalized.iter().map(String::as_str).collect();
    manager.subscribe_ticker(&refs).await?;
    manager.start(false).await?;
    tracing::info!(symbols = ?normalized, stream = "ticker", "Binance ticker streams subscribed");
    let wanted: HashSet<String> = normalized.into_iter().collect();
    Ok(stream::unfold(
        (guard, queue, wanted),
        |(guard, queue, wanted)| async move {
            loop {
                let event = queue.pop().await;
                let WsPayload::Ticker(t) = &event.payload else {
                    continue;
                };
                let Ok(symbol) = normalize_symbol(&t.symbol) else {
                    continue;
                };
                if !wanted.contains(&symbol) {
                    continue;
                }
                let (Some(bid), Some(ask)) = (t.bid, t.ask) else {
                    tracing::warn!(symbol = %symbol, "Dropped malformed Binance ticker payload");
                    continue;
                };
                let data = TickerData {
                    symbol,
                    timestamp: t.event_time.unwrap_or_else(bb_core::time::now_ms),
                    bid,
                    ask,
                    last: t.price,
                    volume: t.base_volume.unwrap_or_default(),
                };
                return Some((data, (guard, queue, wanted)));
            }
        },
    )
    .boxed())
}

/// Either candle feed (`MARKET_DATA_MODE=polling|websocket`).
#[derive(Debug, Clone)]
pub enum MarketDataFeed {
    Polling(PollingCandleFeed),
    WebSocket(WebSocketCandleFeed),
}

impl MarketDataFeed {
    /// Stream closed candles from whichever source is configured.
    pub async fn subscribe_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
    ) -> Result<BoxStream<'static, Result<CandleData>>> {
        match self {
            Self::Polling(feed) => feed.subscribe_ohlcv(symbol, timeframe),
            Self::WebSocket(feed) => feed.subscribe_ohlcv(symbol, timeframe).await,
        }
    }

    /// The WebSocket manager when in WebSocket mode (for stream-error alerts).
    pub fn manager(&self) -> Option<&WsManager> {
        match self {
            Self::Polling(_) => None,
            Self::WebSocket(feed) => Some(feed.manager()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn kline(open: i64) -> Kline {
        Kline {
            open_time: open,
            open: dec!(1),
            high: dec!(2),
            low: dec!(0.5),
            close: dec!(1.5),
            volume: dec!(10),
            close_time: Some(open + 59_999),
            quote_volume: None,
            trades: None,
            taker_buy_base_volume: None,
            taker_buy_quote_volume: None,
        }
    }

    #[test]
    fn latest_closed_skips_the_forming_candle() {
        let minute = Duration::from_secs(60);
        let rows = [kline(0), kline(60_000)];
        let (candle, wait) = latest_closed("BTC/USDT", &rows, 90_000, minute);
        assert_eq!(candle.map(|c| c.timestamp), Some(0));
        assert_eq!(wait, Duration::from_millis(31_000 - 1));
        let (candle, wait) = latest_closed("BTC/USDT", &rows, 120_500, minute);
        assert_eq!(candle.map(|c| c.timestamp), Some(60_000));
        assert_eq!(wait, Duration::from_millis(60_000));
        let (none, _) = latest_closed("BTC/USDT", &rows[..1], 30_000, minute);
        assert!(none.is_none());
    }

    #[test]
    fn timeframe_table_and_backoff_validation() {
        assert_eq!(timeframe_seconds("4h"), Some(14_400));
        assert_eq!(timeframe_seconds("2w"), None);
        assert!(checked_timeframe(" ").is_err());
        let bad = FeedBackoff {
            base: Duration::from_secs(5),
            max: Duration::from_secs(1),
            max_retries: None,
        };
        assert!(bad.validate().is_err());
    }
}

//! WebSocket manager + feed tests against a local tokio WebSocket server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bb_binance::feeds::WebSocketCandleFeed;
use bb_binance::ws::{
    EventQueue, EventType, StreamMarket, WsConfig, WsEvent, WsManager, WsPayload,
};
use bb_binance::{BinanceConfig, Credentials};
use futures::{SinkExt, StreamExt};
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Spawn a WS server; each accepted connection (with its request URI) goes to `handler`.
async fn serve<F, Fut>(handler: F) -> Result<(String, Arc<AtomicUsize>), Box<dyn std::error::Error>>
where
    F: Fn(usize, String, WebSocketStream<TcpStream>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let handler = handler.clone();
            tokio::spawn(async move {
                let (uri_tx, mut uri_rx) = mpsc::unbounded_channel();
                // Signature dictated by tungstenite's `Callback` trait.
                #[allow(clippy::result_large_err)]
                let callback = move |req: &Request, resp: Response| {
                    let _ = uri_tx.send(req.uri().to_string());
                    Ok(resp)
                };
                if let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, callback).await {
                    let uri = uri_rx.recv().await.unwrap_or_default();
                    handler(index, uri, ws).await;
                }
            });
        }
    });
    Ok((format!("ws://{addr}"), count))
}

fn ws_config() -> WsConfig {
    WsConfig {
        subscriber_queue_size: 100,
        heartbeat_interval: Duration::from_millis(50),
        message_timeout: Duration::from_secs(5),
        subscribe_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(2),
        base_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(50),
    }
}

fn manager(url: &str, ws: WsConfig) -> WsManager {
    let config = BinanceConfig::new(false)
        .with_spot_ws_stream_url(url)
        .with_spot_ws_api_url(url);
    WsManager::new(&config, StreamMarket::Spot, ws)
}

async fn next(queue: &EventQueue) -> Result<Arc<WsEvent>, Box<dyn std::error::Error>> {
    Ok(tokio::time::timeout(Duration::from_secs(5), queue.pop()).await?)
}

fn kline_msg(symbol: &str, open_time: i64, closed: bool, close: &str) -> String {
    json!({
        "stream": format!("{}@kline_1m", symbol.to_lowercase()),
        "data": {"e": "kline", "E": open_time + 1, "s": symbol, "k": {
            "t": open_time, "T": open_time + 59_999, "s": symbol, "i": "1m",
            "o": "100.0", "h": "102.0", "l": "99.0", "c": close, "v": "12.5", "x": closed
        }}
    })
    .to_string()
}

#[tokio::test]
async fn public_stream_reconnects_after_drop_and_reports_stream_error() -> TestResult {
    let (url, count) = serve(|index, uri, mut ws| async move {
        assert_eq!(uri, "/stream?streams=btcusdt@kline_1m/btcusdt@ticker");
        if index == 0 {
            let _ = ws.send(Message::Text(kline_msg("BTCUSDT", 60_000, true, "101.0").into())).await;
            // Drop the TCP stream without a close frame.
            drop(ws);
        } else {
            let ticker = json!({"stream": "btcusdt@ticker", "data": {"e": "24hrTicker", "E": 1, "s": "BTCUSDT", "c": "62000.50", "b": "61999.90", "a": "62001.10", "v": "1200.0"}});
            let _ = ws.send(Message::Text(ticker.to_string().into())).await;
            while ws.next().await.is_some() {}
        }
    })
    .await?;
    let manager = manager(&url, ws_config());
    let klines = manager.subscribe_queue(EventType::KlineUpdate, None);
    let tickers = manager.subscribe_queue(EventType::TickerUpdate, None);
    let errors = manager.subscribe_queue(EventType::StreamError, None);
    manager.subscribe_klines(&["BTC/USDT"], "1m").await?;
    manager.subscribe_ticker(&["BTC/USDT"]).await?;
    manager.start(false).await?;

    let WsPayload::Kline(kline) = &next(&klines).await?.payload else {
        return Err("expected kline".into());
    };
    assert_eq!(kline.symbol, "BTC/USDT");
    assert_eq!(kline.close, dec!(101.0));
    let WsPayload::StreamError(err) = &next(&errors).await?.payload else {
        return Err("expected stream error".into());
    };
    assert_eq!(err.stream, "public");
    assert_eq!(err.message, "public WebSocket disconnected");
    let WsPayload::Ticker(ticker) = &next(&tickers).await?.payload else {
        return Err("expected ticker".into());
    };
    assert_eq!(ticker.price, dec!(62000.50));
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(manager.public_connection_count(), 2);
    manager.stop().await;
    Ok(())
}

#[tokio::test]
async fn live_subscribe_sends_control_message_and_book_ticker_parses() -> TestResult {
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Value>();
    let (url, _) = serve(move |_, _, mut ws| {
        let control_tx = control_tx.clone();
        async move {
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(text) = msg {
                    let value: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::Null);
                    let _ = ws.send(Message::Text(json!({"result": null, "id": value["id"]}).to_string().into())).await;
                    let book = json!({"stream": "ethusdt@bookTicker", "data": {"u": 400900217, "s": "ETHUSDT", "b": "25.35190000", "B": "31.21000000", "a": "25.36520000", "A": "40.66000000"}});
                    let _ = ws.send(Message::Text(book.to_string().into())).await;
                    let _ = control_tx.send(value);
                }
            }
        }
    })
    .await?;
    let manager = manager(&url, ws_config());
    let books = manager.subscribe_queue(EventType::BookTickerUpdate, None);
    let raw = manager.subscribe_queue(EventType::RawMessage, None);
    manager.subscribe_ticker(&["BTCUSDT"]).await?;
    manager.start(false).await?;
    while !manager.public_connected() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    manager.subscribe_book_ticker(&["eth/usdt"]).await?;
    let control = tokio::time::timeout(Duration::from_secs(5), control_rx.recv())
        .await?
        .ok_or("no control")?;
    assert_eq!(control["method"], "SUBSCRIBE");
    assert_eq!(control["params"], json!(["ethusdt@bookTicker"]));
    let WsPayload::BookTicker(book) = &next(&books).await?.payload else {
        return Err("expected book ticker".into());
    };
    assert_eq!(book.symbol, "ETH/USDT");
    assert_eq!(book.ask_qty, dec!(40.66));
    assert!(
        raw.is_empty(),
        "control responses must not surface as events"
    );
    manager.stop().await;
    Ok(())
}

#[tokio::test]
async fn stale_stream_is_recycled_and_server_shutdown_reconnects_without_error() -> TestResult {
    let (url, count) = serve(|index, _, mut ws| async move {
        if index == 1 {
            let shutdown =
                json!({"stream": "!serverShutdown", "data": {"e": "serverShutdown", "E": 1}});
            let _ = ws.send(Message::Text(shutdown.to_string().into())).await;
        }
        // Answer pings (auto) but never send data.
        while ws.next().await.is_some() {}
    })
    .await?;
    let mut config = ws_config();
    config.message_timeout = Duration::from_millis(150);
    let manager = manager(&url, config);
    let errors = manager.subscribe_queue(EventType::StreamError, None);
    let raw = manager.subscribe_queue(EventType::RawMessage, None);
    manager.subscribe_trades(&["BTCUSDT"]).await?;
    manager.start(false).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while count.load(Ordering::SeqCst) < 3 {
        if tokio::time::Instant::now() > deadline {
            return Err("manager did not reconnect".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let WsPayload::Raw(shutdown) = &next(&raw).await?.payload else {
        return Err("expected raw serverShutdown".into());
    };
    assert_eq!(shutdown.raw["e"], "serverShutdown");
    assert!(
        errors.is_empty(),
        "planned reconnects must not raise alerts"
    );
    manager.stop().await;
    Ok(())
}

#[tokio::test]
async fn user_stream_subscribes_with_signature_and_resubscribes_after_termination() -> TestResult {
    let (url, count) = serve(|index, _, mut ws| async move {
        let Some(Ok(Message::Text(request))) = ws.next().await else { return };
        let request: Value = serde_json::from_str(request.as_str()).unwrap_or(Value::Null);
        let params = &request["params"];
        let payload = format!("apiKey={}&timestamp={}", params["apiKey"].as_str().unwrap_or_default(), params["timestamp"]);
        let valid = request["method"] == "userDataStream.subscribe.signature"
            && params["signature"] == bb_binance::auth::hmac_sha256_hex("secret", &payload);
        let status = if valid { 200 } else { 400 };
        let _ = ws
            .send(Message::Text(json!({"id": request["id"], "status": status, "result": {"subscriptionId": 0}}).to_string().into()))
            .await;
        if index == 0 {
            let report = json!({"subscriptionId": 0, "event": {
                "e": "executionReport", "E": 1499405658658_i64, "s": "ETHBTC", "c": "mUvoqJxFIILMdfAW5iGSOW", "S": "BUY",
                "o": "LIMIT", "f": "GTC", "q": "1.00000000", "p": "0.10264410", "P": "0.00000000", "F": "0.00000000",
                "g": -1, "C": "", "x": "TRADE", "X": "FILLED", "r": "NONE", "i": 4293153, "l": "1.00000000",
                "z": "1.00000000", "L": "0.10264410", "n": "0.00100000", "N": "ETH", "T": 1499405658657_i64, "t": 7,
                "w": false, "m": false, "M": true, "O": 1499405658657_i64, "Z": "0.10264410", "Y": "0.10264410", "Q": "0.00000000"
            }});
            let _ = ws.send(Message::Text(report.to_string().into())).await;
            let terminated = json!({"subscriptionId": 0, "event": {"e": "eventStreamTerminated", "E": 1728973001334_i64}});
            let _ = ws.send(Message::Text(terminated.to_string().into())).await;
        }
        while ws.next().await.is_some() {}
    })
    .await?;
    let config = BinanceConfig::new(false)
        .with_spot_ws_stream_url(&url)
        .with_spot_ws_api_url(&url)
        .with_credentials(Credentials::new("key", "secret")?);
    let manager = WsManager::new(&config, StreamMarket::Spot, ws_config());
    let orders = manager.subscribe_queue(EventType::OrderUpdate, None);
    let expired = manager.subscribe_queue(EventType::UserStreamExpired, None);
    let errors = manager.subscribe_queue(EventType::StreamError, None);
    manager.start(true).await?;

    let WsPayload::Order(report) = &next(&orders).await?.payload else {
        return Err("expected order update".into());
    };
    assert_eq!(report.order_id, 4_293_153);
    assert_eq!(report.order_status, "FILLED");
    assert_eq!(report.commission_amount, Some(dec!(0.001)));
    assert_eq!(report.original_client_order_id, None);
    assert_eq!(report.is_buyer, Some(true));
    assert!(matches!(
        next(&expired).await?.payload,
        WsPayload::UserStreamExpired(_)
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while count.load(Ordering::SeqCst) < 2 || !manager.user_connected() {
        if tokio::time::Instant::now() > deadline {
            return Err("user stream did not resubscribe".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(errors.is_empty());
    manager.stop().await;
    Ok(())
}

#[tokio::test]
async fn user_stream_requires_credentials_and_signed_payload_is_deterministic() -> TestResult {
    let manager = manager("ws://127.0.0.1:9", ws_config());
    assert!(manager.start(true).await.is_err());
    assert!(!manager.is_running());
    let payload = bb_binance::ws::user_subscribe_payload(
        &Credentials::new("key", "secret")?,
        1_747_385_641_636,
    );
    assert_eq!(payload["method"], "userDataStream.subscribe.signature");
    assert_eq!(payload["params"]["timestamp"], 1_747_385_641_636_i64);
    assert_eq!(
        payload["params"]["signature"],
        bb_binance::auth::hmac_sha256_hex("secret", "apiKey=key&timestamp=1747385641636")
    );
    Ok(())
}

#[tokio::test]
async fn message_parsing_matches_python_manager() -> TestResult {
    let manager = manager("ws://127.0.0.1:9", ws_config());
    manager
        .subscribe_market_data(&["BTC/USDT"], true, true, 20, 100)
        .await?;
    assert_eq!(
        manager.subscriptions(),
        vec!["btcusdt@depth20@100ms", "btcusdt@ticker"]
    );
    manager.subscribe_klines(&["BTC/USDT"], "5m").await?;
    assert!(
        manager
            .subscriptions()
            .contains(&"btcusdt@kline_5m".to_owned())
    );
    assert!(
        manager
            .subscribe_order_book(&["BTC/USDT"], 7, 100)
            .await
            .is_err()
    );

    let books = manager.subscribe_queue(EventType::OrderBookUpdate, None);
    let errors = manager.subscribe_queue(EventType::StreamError, None);
    let accounts = manager.subscribe_queue(EventType::AccountPosition, None);
    let depth = json!({"stream": "btcusdt@depth20@100ms", "data": {"lastUpdateId": 123, "bids": [["61999.00", "0.5"]], "asks": [["62001.00", "0.4"]]}});
    manager.handle_public_message(&depth.to_string());
    let WsPayload::OrderBook(book) = &next(&books).await?.payload else {
        return Err("expected book".into());
    };
    assert_eq!(book.symbol, "BTC/USDT");
    assert_eq!(book.last_update_id, Some(123));
    assert_eq!(book.bids[0].price, dec!(61999));
    assert_eq!(book.asks[0].quantity, dec!(0.4));

    manager.handle_public_message("{not-json");
    let WsPayload::StreamError(err) = &next(&errors).await?.payload else {
        return Err("expected error".into());
    };
    assert_eq!(
        (err.stream.as_str(), err.message.as_str()),
        ("public", "invalid JSON message")
    );

    let bad_kline = json!({"stream": "btcusdt@kline_1m", "data": {"e": "kline", "k": {"t": 1, "T": 2, "o": "x"}}});
    manager.handle_public_message(&bad_kline.to_string());
    let WsPayload::StreamError(err) = &next(&errors).await?.payload else {
        return Err("expected validation error".into());
    };
    assert_eq!(err.message, "failed to validate public WebSocket payload");

    let position = json!({"subscriptionId": 0, "event": {"e": "outboundAccountPosition", "E": 1564034571105_i64, "u": 1564034571073_i64, "B": [{"a": "ETH", "f": "10000.000000", "l": "0.000000"}]}});
    manager.handle_user_message(&position.to_string());
    let WsPayload::AccountPosition(update) = &next(&accounts).await?.payload else {
        return Err("expected account position".into());
    };
    assert_eq!(update.event_time, 1_564_034_571_105);
    assert_eq!(update.balances[0].free, dec!(10000));
    Ok(())
}

#[tokio::test]
async fn callbacks_and_queues_both_receive_events() -> TestResult {
    let manager = manager("ws://127.0.0.1:9", ws_config());
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager.subscribe(EventType::TickerUpdate, move |event: Arc<WsEvent>| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(event);
        }
    });
    let queue = manager.subscribe_queue(EventType::TickerUpdate, Some(1));
    let ticker = json!({"stream": "ethusdt@ticker", "data": {"e": "24hrTicker", "s": "ETHUSDT", "c": "3000"}});
    manager.handle_public_message(&ticker.to_string());
    manager.handle_public_message(&ticker.to_string());
    let from_callback = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await?
        .ok_or("no callback")?;
    let from_queue = next(&queue).await?;
    assert_eq!(from_callback.payload, from_queue.payload);
    assert_eq!(queue.dropped(), 1);
    assert_eq!(manager.dropped_events(), 1);
    Ok(())
}

#[tokio::test]
async fn websocket_candle_feed_yields_matching_closed_deduplicated_candles() -> TestResult {
    let (url, _) = serve(|_, uri, mut ws| async move {
        assert_eq!(uri, "/stream?streams=btcusdt@kline_1m");
        for msg in [
            kline_msg("BTCUSDT", 1_000, false, "100.5"),
            kline_msg("ETHUSDT", 1_000, true, "50.0"),
            kline_msg("BTCUSDT", 1_000, true, "103.5"),
            kline_msg("BTCUSDT", 1_000, true, "999.0"),
            kline_msg("BTCUSDT", 61_000, true, "102.0"),
        ] {
            let _ = ws.send(Message::Text(msg.into())).await;
        }
        while ws.next().await.is_some() {}
    })
    .await?;
    let manager = manager(&url, ws_config());
    let feed = WebSocketCandleFeed::new(manager.clone(), 10);
    assert!(feed.subscribe_ohlcv("BTC/USDT", " ").await.is_err());
    let mut stream = feed.subscribe_ohlcv("btcusdt", "1m").await?;
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await?
        .ok_or("ended")??;
    assert_eq!(first.symbol, "BTC/USDT");
    assert_eq!(first.timestamp, 1_000);
    assert_eq!(first.close, dec!(103.5));
    assert_eq!(first.volume, dec!(12.5));
    let second = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await?
        .ok_or("ended")??;
    assert_eq!(second.timestamp, 61_000);
    assert_eq!(second.close, dec!(102.0));
    drop(stream);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while manager.is_running() || !manager.subscriptions().is_empty() {
        if tokio::time::Instant::now() > deadline {
            return Err("feed drop did not clean up".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

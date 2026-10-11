use super::*;
use async_trait::async_trait;
use bb_binance::ws::{StreamError, UserStreamExpired};

use crate::alerts::AlertTransport;

#[derive(Default)]
struct Capture(Mutex<Vec<String>>);

#[async_trait]
impl AlertTransport for Capture {
    async fn send(&self, html: &str) -> bool {
        self.0.lock().push(html.to_owned());
        true
    }
}

fn stream_error(stream: &str, kind: Option<&str>) -> WsEvent {
    WsEvent::new(WsPayload::StreamError(StreamError {
        stream: stream.into(),
        message: "connection lost".into(),
        error_type: kind.map(str::to_owned),
        raw: serde_json::Value::Null,
    }))
}

#[tokio::test(start_paused = true)]
async fn stream_alerts_respect_per_stream_cooldown() {
    let capture = Arc::new(Capture::default());
    let alerts = StreamAlerts {
        alerter: Arc::new(TelegramAlerter::with_transport(capture.clone())),
        cooldown: Duration::from_secs(300),
        last_sent: Mutex::new(HashMap::new()),
    };
    alerts
        .handle("market-data", &stream_error("public", Some("TimeoutError")))
        .await;
    alerts
        .handle("market-data", &stream_error("public", None))
        .await;
    alerts
        .handle(
            "execution",
            &WsEvent::new(WsPayload::UserStreamExpired(UserStreamExpired {
                event_time: None,
                raw: serde_json::Value::Null,
            })),
        )
        .await;
    tokio::time::advance(Duration::from_secs(301)).await;
    alerts
        .handle("market-data", &stream_error("public", None))
        .await;

    let messages = capture.0.lock().clone();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert!(messages[0].contains("<code>market-data/public</code>"));
    assert!(messages[0].contains("connection lost (TimeoutError)"));
    assert!(messages[1].contains("<code>execution/user</code>"));
    assert!(messages[1].contains("Binance user-data stream expired"));
}

#[tokio::test]
async fn signal_is_set_once_and_wakes_waiters() {
    let signal = Signal::default();
    assert!(!signal.is_set());
    let waiter = tokio::spawn({
        let signal = signal.clone();
        async move { signal.wait().await }
    });
    signal.set();
    waiter.await.unwrap();
    assert!(signal.is_set());
    signal.wait().await;
}

//! Detector alert sink. Alerts are never fatal.

use async_trait::async_trait;

#[async_trait]
pub trait Alerter: Send + Sync {
    /// Deliver a plain-text alert; returns whether it was sent.
    async fn alert(&self, text: &str) -> bool;
}

/// Telegram Bot API alerts from `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID`
/// (no-op when unset). Text is HTML-escaped like Python `maybe_telegram`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TelegramAlerter;

#[async_trait]
impl Alerter for TelegramAlerter {
    async fn alert(&self, text: &str) -> bool {
        bb_core::telegram::maybe_send(&bb_core::telegram::escape(text)).await
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use super::*;

    /// Records every alert instead of sending it.
    #[derive(Debug, Default)]
    pub struct RecordingAlerter(pub Mutex<Vec<String>>);

    impl RecordingAlerter {
        pub fn sent(&self) -> Vec<String> {
            self.0.lock().map(|v| v.clone()).unwrap_or_default()
        }
    }

    #[async_trait]
    impl Alerter for RecordingAlerter {
        async fn alert(&self, text: &str) -> bool {
            if let Ok(mut v) = self.0.lock() {
                v.push(text.to_owned());
            }
            true
        }
    }
}

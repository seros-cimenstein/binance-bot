//! Never-fatal Telegram alerts (port of `src/telegram_alerts.TelegramAlerter.send`).

use async_trait::async_trait;
use bb_core::telegram::Telegram;

use crate::settings::Settings;

/// Anything that can deliver an HTML alert; returns whether it was sent.
#[async_trait]
pub trait Alerter: Send + Sync {
    async fn send(&self, html: &str) -> bool;
}

/// Telegram Bot API alerter; disabled (always `false`) without token + chat id.
#[derive(Debug, Clone)]
pub struct TelegramAlerter {
    telegram: Option<Telegram>,
}

impl TelegramAlerter {
    pub fn new(bot_token: &str, chat_id: &str) -> Self {
        let telegram = if bot_token.is_empty() || chat_id.is_empty() {
            tracing::warn!(
                "Telegram alerts disabled: set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID to enable"
            );
            None
        } else {
            Some(Telegram::new(bot_token, chat_id))
        };
        Self { telegram }
    }

    pub fn from_settings(settings: &Settings) -> Self {
        Self::new(&settings.telegram_bot_token, &settings.telegram_chat_id)
    }
}

#[async_trait]
impl Alerter for TelegramAlerter {
    async fn send(&self, html: &str) -> bool {
        match &self.telegram {
            Some(telegram) => telegram.send(html).await,
            None => false,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use parking_lot::Mutex;

    use super::*;

    /// Records every message; returns `result` from `send`.
    #[derive(Debug, Default)]
    pub struct RecordingAlerter {
        pub messages: Mutex<Vec<String>>,
        pub fail: bool,
    }

    impl RecordingAlerter {
        pub fn messages(&self) -> Vec<String> {
            self.messages.lock().clone()
        }
    }

    #[async_trait]
    impl Alerter for RecordingAlerter {
        async fn send(&self, html: &str) -> bool {
            self.messages.lock().push(html.to_owned());
            !self.fail
        }
    }
}

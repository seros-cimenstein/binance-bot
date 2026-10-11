//! Never-fatal Telegram alerts.

use bb_core::telegram::{Telegram, escape};

/// Telegram sink handed to the runtimes; disabled when credentials are absent.
#[derive(Debug, Clone, Default)]
pub struct Alerter(Option<Telegram>);

impl Alerter {
    /// From `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` (no-op when either is unset).
    pub fn from_env() -> Self {
        Self(Telegram::from_env())
    }

    pub fn disabled() -> Self {
        Self(None)
    }

    pub fn new(telegram: Telegram) -> Self {
        Self(Some(telegram))
    }

    /// Send plain text (HTML-escaped); failures are logged by bb-core and ignored.
    pub async fn send(&self, text: &str) {
        if let Some(tg) = &self.0 {
            tg.send(&escape(text)).await;
        }
    }
}

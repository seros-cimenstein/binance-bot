//! Port of the runtime alerts in `src/telegram_alerts.py`, on top of
//! [`bb_core::telegram::Telegram`] (never-fatal sends, persisted 429 cooldown).
//!
//! `check_for_go_off_command` (`/go_off` / `/go_on` and `data/go_off.flag`) is
//! intentionally not ported: it silently disabled safety limits.

use std::sync::Arc;

use async_trait::async_trait;
use bb_core::Decimal;
use bb_core::telegram::{Telegram, escape};

use crate::orders::models::OrderSide;
use crate::orders::payload::format_g;

/// Delivery channel for alert messages (HTML).
#[async_trait]
pub trait AlertTransport: Send + Sync {
    async fn send(&self, html: &str) -> bool;
}

#[async_trait]
impl AlertTransport for Telegram {
    async fn send(&self, html: &str) -> bool {
        Telegram::send(self, html).await
    }
}

/// Fire-and-forget runtime alerts; disabled without credentials.
#[derive(Clone)]
pub struct TelegramAlerter {
    transport: Option<Arc<dyn AlertTransport>>,
}

impl std::fmt::Debug for TelegramAlerter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramAlerter")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl TelegramAlerter {
    /// Enabled only when both the token and chat id are non-blank.
    pub fn new(bot_token: &str, chat_id: &str) -> Self {
        if bot_token.trim().is_empty() || chat_id.trim().is_empty() {
            tracing::warn!(
                "Telegram alerts disabled: set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID to enable"
            );
            return Self { transport: None };
        }
        let mut telegram = Telegram::new(bot_token.trim(), chat_id.trim());
        if let Some(path) = bb_core::env::optional_string(&["TELEGRAM_COOLDOWN_PATH"]) {
            telegram = telegram.with_cooldown_path(path);
        }
        Self {
            transport: Some(Arc::new(telegram)),
        }
    }

    /// Alerter over a custom transport (tests, alternative channels).
    pub fn with_transport(transport: Arc<dyn AlertTransport>) -> Self {
        Self {
            transport: Some(transport),
        }
    }

    pub fn disabled() -> Self {
        Self { transport: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.transport.is_some()
    }

    pub async fn send(&self, html: &str) -> bool {
        match &self.transport {
            Some(transport) => transport.send(html).await,
            None => false,
        }
    }

    pub async fn alert_started(&self, mode: &str) {
        let emoji = if mode == "LIVE" {
            "\u{1f534}"
        } else {
            "\u{1f7e1}"
        };
        self.send(&format!(
            "{emoji} <b>Bot started</b> — mode: <code>{}</code>",
            escape(mode)
        ))
        .await;
    }

    pub async fn alert_crash(
        &self,
        attempt: u32,
        max_attempts: u32,
        delay_seconds: f64,
        error: &str,
    ) {
        self.send(&format!(
            "\u{1f4a5} <b>Bot crashed</b> (attempt {attempt}/{max_attempts})\nRestarting in {delay_seconds:.0}s\n<code>{}</code>",
            clip(error, 300)
        ))
        .await;
    }

    pub async fn alert_terminal_failure(&self, error: &str) {
        self.send(&format!(
            "\u{1f6d1} <b>Bot stopped after repeated failures</b>\n<code>{}</code>",
            clip(error, 300)
        ))
        .await;
    }

    pub async fn alert_stream_failure(&self, stream: &str, reason: &str) {
        self.send(&format!(
            "\u{26a0}\u{fe0f} <b>WebSocket stream failure</b>\nStream: <code>{}</code>\n<code>{}</code>",
            clip(stream, 100),
            clip(reason, 300)
        ))
        .await;
    }

    pub async fn alert_operations_test(&self, mode: &str, symbol: &str) -> bool {
        self.send(&format!(
            "\u{2705} <b>Operational verification</b>\nMode: <code>{}</code>\nSymbol: <code>{}</code>\nRead-only connectivity check completed.",
            escape(mode),
            escape(symbol)
        ))
        .await
    }

    pub async fn alert_risk_breach(&self, reason: &str) {
        self.send(&format!(
            "\u{26a0}\u{fe0f} <b>Risk breach blocked an order</b>\n<code>{}</code>",
            clip(reason, 300)
        ))
        .await;
    }

    pub async fn alert_trade(
        &self,
        side: OrderSide,
        symbol: &str,
        amount: Decimal,
        price: Option<Decimal>,
        dry_run: bool,
    ) {
        let (emoji, label) = match side {
            OrderSide::Buy => ("\u{1f7e2}", "BUY"),
            OrderSide::Sell => ("\u{1f534}", "SELL"),
        };
        let suffix = if dry_run { " <i>(dry run)</i>" } else { "" };
        let price = price.map_or_else(|| "market".to_owned(), |p| format!("${}", thousands(p, 4)));
        self.send(&format!(
            "{emoji} <b>{label}</b>{suffix}\nSymbol: <code>{}</code>\nAmount: <code>{}</code>\nPrice: <code>{price}</code>",
            escape(symbol),
            format_g(amount)
        ))
        .await;
    }
}

/// First `max` characters, HTML-escaped (Python sent raw error text, which
/// made Telegram reject messages containing `<`/`&`).
fn clip(text: &str, max: usize) -> String {
    escape(&text.chars().take(max).collect::<String>())
}

/// Python `f"{value:,.{dp}f}"`.
pub fn thousands(value: Decimal, dp: u32) -> String {
    let rounded = format!("{:.*}", dp as usize, value.round_dp(dp));
    let (sign, digits) = rounded
        .strip_prefix('-')
        .map_or(("", rounded.as_str()), |d| ("-", d));
    let (int_part, frac) = digits
        .split_once('.')
        .map_or((digits, None), |(i, f)| (i, Some(f)));
    let mut grouped = String::with_capacity(int_part.len() + int_part.len() / 3);
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    match frac {
        Some(frac) => format!("{sign}{grouped}.{frac}"),
        None => format!("{sign}{grouped}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use rust_decimal_macros::dec;

    #[derive(Default)]
    pub(crate) struct Capture(Mutex<Vec<String>>);

    #[async_trait]
    impl AlertTransport for Capture {
        async fn send(&self, html: &str) -> bool {
            self.0.lock().push(html.to_owned());
            true
        }
    }

    #[test]
    fn thousands_matches_python_format() {
        assert_eq!(thousands(dec!(82000), 4), "82,000.0000");
        assert_eq!(thousands(dec!(1234567.891), 4), "1,234,567.8910");
        assert_eq!(thousands(dec!(999.5), 0), "1,000");
        assert_eq!(thousands(dec!(-1234.5), 2), "-1,234.50");
    }

    #[tokio::test]
    async fn messages_match_python_wording_and_escape_errors() {
        let capture = Arc::new(Capture::default());
        let alerter = TelegramAlerter::with_transport(capture.clone());
        alerter
            .alert_trade(
                OrderSide::Buy,
                "BTC/USDT",
                dec!(0.001),
                Some(dec!(82000)),
                true,
            )
            .await;
        alerter.alert_crash(2, 5, 10.0, "boom <x>").await;
        alerter.alert_started("LIVE").await;
        let messages = capture.0.lock().clone();
        assert_eq!(
            messages[0],
            "\u{1f7e2} <b>BUY</b> <i>(dry run)</i>\nSymbol: <code>BTC/USDT</code>\nAmount: <code>0.001</code>\nPrice: <code>$82,000.0000</code>"
        );
        assert_eq!(
            messages[1],
            "\u{1f4a5} <b>Bot crashed</b> (attempt 2/5)\nRestarting in 10s\n<code>boom &lt;x&gt;</code>"
        );
        assert_eq!(
            messages[2],
            "\u{1f534} <b>Bot started</b> — mode: <code>LIVE</code>"
        );
    }

    #[tokio::test]
    async fn disabled_alerter_sends_nothing() {
        let alerter = TelegramAlerter::new("", "chat");
        assert!(!alerter.is_enabled());
        assert!(!alerter.alert_operations_test("testnet", "BTC/USDT").await);
    }
}

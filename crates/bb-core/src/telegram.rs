//! Telegram Bot API alerts.
//!
//! Contract: alerts are never fatal. [`Telegram::send`] returns `false` on any
//! failure and logs it. A 429 response persists a cooldown to disk
//! (`data/telegram-cooldown.json` by default) so a crash-looping container
//! cannot resume a retry storm.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const DEFAULT_COOLDOWN_PATH: &str = "data/telegram-cooldown.json";
const DEFAULT_COOLDOWN_S: i64 = 60;
const COOLDOWN_CAP_S: i64 = 24 * 60 * 60;
/// Telegram's hard limit is 4096 UTF-16 units; stay below it.
pub const MAX_MESSAGE_CHARS: usize = 3900;

#[derive(Debug, Serialize, Deserialize, Default)]
struct CooldownState {
    cooldown_until_ms: i64,
    #[serde(default)]
    last_429_at_ms: i64,
    #[serde(default)]
    retry_after_observed_s: i64,
}

#[derive(Debug, Clone)]
pub struct Telegram {
    token: String,
    chat_id: String,
    cooldown_path: PathBuf,
    api_base: String,
    http: reqwest::Client,
}

impl Telegram {
    pub fn new(token: impl Into<String>, chat_id: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            chat_id: chat_id.into(),
            cooldown_path: PathBuf::from(DEFAULT_COOLDOWN_PATH),
            api_base: "https://api.telegram.org".to_owned(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }

    /// From `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID`; `None` when either is unset.
    pub fn from_env() -> Option<Self> {
        let token = crate::env::optional_string(&["TELEGRAM_BOT_TOKEN"])?;
        let chat = crate::env::optional_string(&["TELEGRAM_CHAT_ID"])?;
        let mut tg = Self::new(token, chat);
        if let Some(p) = crate::env::optional_string(&["TELEGRAM_COOLDOWN_PATH"]) {
            tg.cooldown_path = PathBuf::from(p);
        }
        Some(tg)
    }

    pub fn with_cooldown_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.cooldown_path = path.into();
        self
    }

    /// Override the API origin (tests).
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn cooled_down(&self) -> bool {
        cooldown_until_ms(&self.cooldown_path) > crate::time::now_ms()
    }

    /// Send one HTML message (caller escapes untrusted text). Long messages are
    /// split into Telegram-safe chunks; returns `true` only if every chunk sent.
    pub async fn send(&self, html: &str) -> bool {
        let mut ok = true;
        for chunk in split_message(html, MAX_MESSAGE_CHARS) {
            ok &= self.send_one(&chunk).await;
            if !ok {
                break;
            }
        }
        ok
    }

    async fn send_one(&self, text: &str) -> bool {
        if self.cooled_down() {
            tracing::debug!("telegram alert suppressed (429 cooldown active)");
            return false;
        }
        let url = format!("{}/bot{}/sendMessage", self.api_base, self.token);
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        let resp = match self.http.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err.without_url(), "telegram alert error");
                return false;
            }
        };
        let status = resp.status();
        if status.is_success() {
            return true;
        }
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 429 {
            let retry = parse_retry_after(&text);
            write_cooldown(&self.cooldown_path, retry);
            tracing::warn!(retry_after_s = retry, "telegram 429, cooling down");
        } else {
            tracing::warn!(%status, body = %truncate(&text, 200), "telegram alert failed");
        }
        false
    }

    /// Escape plain text and send; `monospace` wraps it in `<pre>`.
    pub async fn send_text(&self, text: &str, monospace: bool) -> bool {
        let escaped = escape(text);
        if monospace {
            self.send(&format!("<pre>{escaped}</pre>")).await
        } else {
            self.send(&escaped).await
        }
    }
}

/// Fire-and-forget helper used by every runtime: no-op without credentials.
pub async fn maybe_send(html: &str) -> bool {
    match Telegram::from_env() {
        Some(tg) => tg.send(html).await,
        None => false,
    }
}

pub fn escape(text: &str) -> String {
    html_escape::encode_text(text).into_owned()
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Split on line boundaries where possible, hard-splitting overlong lines.
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    if text.chars().count() <= max_chars {
        return vec![text.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for line in text.split_inclusive('\n') {
        let line_len = line.chars().count();
        if current_len + line_len > max_chars && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if line_len > max_chars {
            let chars: Vec<char> = line.chars().collect();
            for piece in chars.chunks(max_chars) {
                chunks.push(piece.iter().collect());
            }
            continue;
        }
        current.push_str(line);
        current_len += line_len;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn parse_retry_after(body: &str) -> i64 {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/parameters/retry_after")
                .and_then(serde_json::Value::as_i64)
        })
        .filter(|ra| *ra > 0)
        .unwrap_or(DEFAULT_COOLDOWN_S)
}

fn cooldown_until_ms(path: &Path) -> i64 {
    crate::fsutil::read_json::<CooldownState>(path)
        .ok()
        .flatten()
        .map_or(0, |s| s.cooldown_until_ms)
}

fn write_cooldown(path: &Path, retry_after_s: i64) {
    let retry = retry_after_s.clamp(0, COOLDOWN_CAP_S);
    let now = crate::time::now_ms();
    let state = CooldownState {
        cooldown_until_ms: now + retry * 1000,
        last_429_at_ms: now,
        retry_after_observed_s: retry,
    };
    if let Err(err) = crate::fsutil::write_json_atomic(path, &state) {
        tracing::warn!(%err, path = %path.display(), "cannot persist telegram cooldown");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn split_respects_limit_and_preserves_text() {
        let text = "a\n".repeat(50) + &"x".repeat(25);
        let chunks = split_message(&text, 10);
        assert!(chunks.iter().all(|c| c.chars().count() <= 10));
        assert_eq!(chunks.concat(), text);
    }

    #[tokio::test]
    async fn rate_limit_persists_cooldown_and_suppresses_next_send()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("^/botT/sendMessage$"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string(r#"{"ok":false,"parameters":{"retry_after":120}}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir()?;
        let tg = Telegram::new("T", "1")
            .with_api_base(server.uri())
            .with_cooldown_path(dir.path().join("cd.json"));
        assert!(!tg.send("hi").await);
        assert!(tg.cooled_down());
        assert!(!tg.send("again").await); // suppressed locally; mock expects exactly 1 call
        Ok(())
    }
}

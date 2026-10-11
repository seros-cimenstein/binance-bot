//! Telegram channel signal ingester: contract addresses posted in configured
//! channels (`data/telegram-channels.json`) become candidates weighted by the
//! channel's configured weight. Runs as a Telegram *user* account over MTProto
//! (`grammers`), authorised once with `signals-telegram-setup`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use bb_core::env;
use grammers_client::client::UpdatesConfiguration;
use grammers_client::session::types::PeerId;
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool};
use regex::Regex;
use serde_json::{Value, json};

use crate::alerts::Alerter;
use crate::candidate::SignalCandidate;
use crate::common::{
    AlertRateLimiter, Clock, append_ledger, atomic_write_json, emergency_stop_active,
    read_json_lenient,
};
use crate::error::SignalsError;
use crate::jsonv::number;
use crate::telegram_session::{DEFAULT_SESSION_PATH, JsonFileSession};
use crate::watchlist::Watchlist;

pub const DEDUP_WINDOW_MS: i64 = 3_600_000;
pub const CHANNEL_RELOAD_INTERVAL: Duration = Duration::from_secs(300);
pub const STOP_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// `--once`: stop after this long without a caught-up update.
pub const CATCH_UP_IDLE: Duration = Duration::from_secs(5);
const DEFAULT_WEIGHT: f64 = 0.5;
const DEFAULT_CHANNELS: [(&str, f64); 3] = [
    ("binance_announcements", 0.4),
    ("WatcherGuru", 0.3),
    ("coingecko", 0.2),
];
const DEFAULT_CHANNELS_NOTE: &str = "Edit this file to add private/curated alpha groups. weight \u{2208} [0,1] feeds confidence. Username = @handle without the @.";

#[allow(clippy::expect_used)] // literal patterns; covered by tests
static EVM_ADDRESS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b0x[a-fA-F0-9]{40}\b").expect("valid EVM regex"));
#[allow(clippy::expect_used)]
static SOLANA_ADDRESS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[1-9A-HJ-NP-Za-km-z]{32,44}\b").expect("valid Solana regex"));

#[derive(Debug, Clone, PartialEq)]
pub struct TelegramSettings {
    pub api_id: String,
    pub api_hash: String,
    pub session_path: PathBuf,
    pub channels_path: PathBuf,
    pub max_alerts_per_hour: usize,
    pub stop_path: PathBuf,
    pub ledger_path: PathBuf,
    pub watchlist_path: PathBuf,
    pub watchlist_ttl_hours: f64,
}

impl Default for TelegramSettings {
    fn default() -> Self {
        Self {
            api_id: String::new(),
            api_hash: String::new(),
            session_path: DEFAULT_SESSION_PATH.into(),
            channels_path: "data/telegram-channels.json".into(),
            max_alerts_per_hour: 30,
            stop_path: "data/signals-telegram.stop".into(),
            ledger_path: "data/signals-ledger.jsonl".into(),
            watchlist_path: "data/signals-watchlist.json".into(),
            watchlist_ttl_hours: 12.0,
        }
    }
}

impl TelegramSettings {
    pub fn from_env() -> Result<Self, SignalsError> {
        let d = Self::default();
        Ok(Self {
            api_id: env::string(&["TELEGRAM_API_ID"], ""),
            api_hash: env::string(&["TELEGRAM_API_HASH"], ""),
            session_path: session_path_from_env(),
            channels_path: env::string(
                &["SIGNALS_TELEGRAM_CHANNELS_PATH"],
                "data/telegram-channels.json",
            )
            .into(),
            max_alerts_per_hour: env::integer(
                &["SIGNALS_TELEGRAM_MAX_ALERTS_PER_HOUR"],
                d.max_alerts_per_hour,
            )?,
            stop_path: env::string(
                &["SIGNALS_TELEGRAM_STOP_PATH"],
                "data/signals-telegram.stop",
            )
            .into(),
            ledger_path: env::string(&["SIGNALS_LEDGER_PATH"], "data/signals-ledger.jsonl").into(),
            watchlist_path: env::string(&["SIGNALS_WATCHLIST_PATH"], "data/signals-watchlist.json")
                .into(),
            watchlist_ttl_hours: env::float(
                &["SIGNALS_WATCHLIST_TTL_HOURS"],
                d.watchlist_ttl_hours,
            )?,
        })
    }

    /// Credentials present, `api_id` numeric, session file created by setup.
    pub fn validate(&self) -> Result<i32, TelegramConfigError> {
        if self.api_id.is_empty() || self.api_hash.is_empty() {
            return Err(TelegramConfigError(
                "TELEGRAM_API_ID and TELEGRAM_API_HASH are required. Create them at my.telegram.org/apps, then run `bb signals-telegram-setup`.".into(),
            ));
        }
        let api_id = self
            .api_id
            .parse::<i32>()
            .map_err(|_| TelegramConfigError("TELEGRAM_API_ID must be an integer.".into()))?;
        if !self.session_path.exists() {
            return Err(TelegramConfigError(format!(
                "Telegram session file missing: {}. Run `bb signals-telegram-setup` (signals-telegram-setup) in an interactive shell first.",
                self.session_path.display()
            )));
        }
        Ok(api_id)
    }
}

/// `TELEGRAM_SESSION_PATH`, defaulting to the grammers session file.
pub fn session_path_from_env() -> PathBuf {
    env::string(&["TELEGRAM_SESSION_PATH"], DEFAULT_SESSION_PATH).into()
}

/// Operator-actionable configuration problem (CLI exit code 2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TelegramConfigError(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct TelegramChannel {
    pub username: String,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TelegramChannelList {
    pub channels: Vec<TelegramChannel>,
    pub mtime: Option<SystemTime>,
}

impl TelegramChannelList {
    pub fn usernames(&self) -> Vec<&str> {
        self.channels.iter().map(|c| c.username.as_str()).collect()
    }

    /// Configured weight (case-insensitive, `@` optional), default 0.5.
    pub fn weight_for(&self, username: &str) -> f64 {
        let wanted = normalize_username(username).to_lowercase();
        self.channels
            .iter()
            .find(|c| c.username.to_lowercase() == wanted)
            .map_or(DEFAULT_WEIGHT, |c| c.weight)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAddress {
    pub chain: &'static str,
    pub address: String,
}

/// EVM and Solana contract addresses in text order, de-duplicated.
pub fn extract_contract_addresses(text: &str) -> Vec<DetectedAddress> {
    let mut matches: Vec<(usize, DetectedAddress)> = EVM_ADDRESS_RE
        .find_iter(text)
        .map(|m| {
            (
                m.start(),
                DetectedAddress {
                    chain: "ethereum",
                    address: m.as_str().to_owned(),
                },
            )
        })
        .collect();
    matches.extend(
        SOLANA_ADDRESS_RE
            .find_iter(text)
            .filter(|m| looks_like_solana_pubkey(m.as_str()))
            .map(|m| {
                (
                    m.start(),
                    DetectedAddress {
                        chain: "solana",
                        address: m.as_str().to_owned(),
                    },
                )
            }),
    );
    matches.sort_by_key(|(start, _)| *start);
    let mut seen = HashSet::new();
    matches
        .into_iter()
        .map(|(_, d)| d)
        .filter(|d| seen.insert((d.chain, d.address.clone())))
        .collect()
}

/// Rejects lowercase words and short tokens that match the base58 alphabet.
fn looks_like_solana_pubkey(address: &str) -> bool {
    address.len() >= 40
        && address.chars().any(|c| c.is_uppercase())
        && address.chars().any(|c| c.is_ascii_digit())
}

fn normalize_username(username: &str) -> &str {
    username.trim().trim_start_matches('@')
}

/// Load the channel list, writing the (low-quality) defaults when missing.
pub fn load_channel_list(path: &Path) -> Result<TelegramChannelList, SignalsError> {
    if !path.exists() {
        let channels: Vec<Value> = DEFAULT_CHANNELS
            .iter()
            .map(|(u, w)| json!({"username": u, "weight": w}))
            .collect();
        atomic_write_json(
            path,
            &json!({"channels": channels, "_note": DEFAULT_CHANNELS_NOTE}),
        )?;
        tracing::warn!(
            path = %path.display(),
            "Created default Telegram channel list. Signal quality from these public news channels will be LOW; replace them with vetted alpha groups."
        );
    }
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let Some(data) = read_json_lenient(path) else {
        tracing::warn!(path = %path.display(), "Telegram channel list malformed");
        return Ok(TelegramChannelList {
            channels: Vec::new(),
            mtime,
        });
    };
    let mut seen = HashSet::new();
    let channels = data
        .get("channels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_channel)
        .filter(|c| seen.insert(c.username.to_lowercase()))
        .collect();
    Ok(TelegramChannelList { channels, mtime })
}

fn parse_channel(item: &Value) -> Option<TelegramChannel> {
    let item = item.as_object()?;
    let username = normalize_username(item.get("username")?.as_str()?);
    if username.is_empty() {
        return None;
    }
    let weight = match item.get("weight") {
        None => DEFAULT_WEIGHT,
        w => number(w)?,
    };
    (0.0..=1.0).contains(&weight).then(|| TelegramChannel {
        username: username.to_owned(),
        weight,
    })
}

/// Per-(channel, address) one-hour de-duplication window.
#[derive(Debug, Default)]
pub struct DedupWindow {
    order: VecDeque<((String, String), i64)>,
    keys: HashSet<(String, String)>,
}

impl DedupWindow {
    pub fn allows(&mut self, channel_id: &str, address: &str, now_ms: i64) -> bool {
        let cutoff = now_ms - DEDUP_WINDOW_MS;
        while let Some((key, ts)) = self.order.front() {
            if *ts > cutoff {
                break;
            }
            self.keys.remove(key);
            self.order.pop_front();
        }
        let key = (channel_id.to_owned(), address.to_owned());
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back((key, now_ms));
        true
    }
}

/// The parts of a channel post the detector needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMessage {
    pub text: String,
    pub channel_username: String,
    /// Bot-API style marked chat id (e.g. `-1001234567890`), as Telethon's `chat_id`.
    pub channel_id: String,
    pub message_id: String,
}

pub struct TelegramSignalProcessor {
    settings: TelegramSettings,
    clock: Arc<dyn Clock>,
    alerter: Arc<dyn Alerter>,
    seen: DedupWindow,
    watchlist: Watchlist,
    limiter: AlertRateLimiter,
}

impl TelegramSignalProcessor {
    pub fn new(
        settings: TelegramSettings,
        clock: Arc<dyn Clock>,
        alerter: Arc<dyn Alerter>,
    ) -> Self {
        let watchlist = Watchlist::load(&settings.watchlist_path);
        let limiter = AlertRateLimiter::new(settings.max_alerts_per_hour);
        Self {
            settings,
            clock,
            alerter,
            seen: DedupWindow::default(),
            watchlist,
            limiter,
        }
    }

    pub async fn process(
        &mut self,
        message: &IncomingMessage,
        channels: &TelegramChannelList,
    ) -> Result<Vec<SignalCandidate>, SignalsError> {
        let detected = extract_contract_addresses(&message.text);
        let mut candidates = Vec::new();
        for d in detected {
            let now_ms = self.clock.now_ms();
            if !self.seen.allows(&message.channel_id, &d.address, now_ms) {
                continue;
            }
            let raw = json!({
                "channel_username": message.channel_username,
                "channel_id": message.channel_id,
                "message_id": message.message_id,
                "message_text": message.text.chars().take(500).collect::<String>(),
                "message_url": format!("https://t.me/{}/{}", message.channel_username, message.message_id),
            });
            let candidate = SignalCandidate {
                source: "telegram".into(),
                chain: d.chain.into(),
                token_address: d.address,
                token_symbol: String::new(),
                detected_at_ms: now_ms,
                raw_signal: raw.as_object().cloned().unwrap_or_default(),
                confidence: channels.weight_for(&message.channel_username),
            };
            append_ledger(&self.settings.ledger_path, &candidate)?;
            self.watchlist
                .add(&candidate, self.settings.watchlist_ttl_hours);
            self.alert(&candidate).await;
            candidates.push(candidate);
        }
        if !candidates.is_empty() {
            self.watchlist.prune(self.clock.now_ms());
            self.watchlist.save()?;
        }
        Ok(candidates)
    }

    async fn alert(&mut self, candidate: &SignalCandidate) {
        if !self.limiter.allow(self.clock.now_ms()) {
            tracing::debug!(token = %candidate.token_address, "Telegram signal alert suppressed by detector rate limit");
            return;
        }
        let channel = candidate
            .raw_signal
            .get("channel_username")
            .and_then(Value::as_str)
            .unwrap_or("");
        let msg = format!(
            "Telegram signal\nChannel: {channel}\nChain: {}\nAddress: {}\nConfidence: {:.3}",
            candidate.chain, candidate.token_address, candidate.confidence
        );
        self.alerter.alert(&msg).await;
    }
}

/// Resolve configured usernames to chat ids (logs and skips unknown ones).
async fn resolve_channels(
    client: &Client,
    channels: &TelegramChannelList,
) -> HashMap<PeerId, String> {
    let mut resolved = HashMap::new();
    for username in channels.usernames() {
        match client.resolve_username(username).await {
            Ok(Some(peer)) => {
                resolved.insert(peer.id(), username.to_owned());
            }
            Ok(None) => tracing::warn!(%username, "Telegram channel not found"),
            Err(err) => tracing::warn!(%username, %err, "Telegram channel resolution failed"),
        }
    }
    if resolved.is_empty() {
        tracing::warn!("Telegram channel list is empty; no channels subscribed");
    } else {
        tracing::info!(channels = resolved.len(), "Listening for Telegram signals");
    }
    resolved
}

fn incoming(message: &grammers_client::message::Message, configured: &str) -> IncomingMessage {
    let peer_id = message.peer_id();
    let username = message
        .peer()
        .and_then(|p| p.username())
        .or_else(|| message.sender().and_then(|s| s.username()))
        .unwrap_or(configured);
    IncomingMessage {
        text: message.text().to_owned(),
        channel_username: normalize_username(username).to_owned(),
        channel_id: peer_id
            .bot_api_dialog_id()
            .map_or_else(String::new, |id| id.to_string()),
        message_id: message.id().to_string(),
    }
}

/// Connect, then stream channel posts until stop file / signal / disconnect
/// (`once`: catch up on missed posts and exit when idle).
pub async fn run_ingester(
    settings: TelegramSettings,
    once: bool,
    clock: Arc<dyn Clock>,
    alerter: Arc<dyn Alerter>,
) -> anyhow::Result<()> {
    let mut channels = load_channel_list(&settings.channels_path)?;
    let api_id = settings.validate()?;
    let session = Arc::new(
        JsonFileSession::open(&settings.session_path)
            .map_err(|e| TelegramConfigError(e.to_string()))?,
    );
    let SenderPool {
        runner,
        updates,
        handle,
    } = SenderPool::new(Arc::clone(&session), api_id);
    let client = Client::new(handle.clone());
    let pool_task = tokio::spawn(runner.run());

    let result = Box::pin(async {
        if !client.is_authorized().await? {
            return Err(TelegramConfigError(
                "Telegram session is not authorized. Run `bb signals-telegram-setup` in an interactive shell.".into(),
            )
            .into());
        }
        let mut subscribed = resolve_channels(&client, &channels).await;
        let mut processor = TelegramSignalProcessor::new(settings.clone(), clock, alerter);
        let mut stream = client
            .stream_updates(updates, UpdatesConfiguration { catch_up: once, ..Default::default() })
            .await
            .map_err(|e| anyhow::anyhow!("cannot start Telegram update stream: {e}"))?;

        let mut stop_tick = tokio::time::interval(STOP_CHECK_INTERVAL);
        let mut last_reload = tokio::time::Instant::now();
        let shutdown = crate::common::shutdown_signal();
        tokio::pin!(shutdown);
        let outcome: anyhow::Result<()> = loop {
            let next = async {
                if once {
                    tokio::time::timeout(CATCH_UP_IDLE, stream.next()).await.ok()
                } else {
                    Some(stream.next().await)
                }
            };
            tokio::select! {
                () = &mut shutdown => {
                    tracing::info!("Telegram signal ingester stopped by signal");
                    break Ok(());
                }
                _ = stop_tick.tick(), if !once => {
                    if emergency_stop_active(&settings.stop_path) {
                        tracing::info!(path = %settings.stop_path.display(), "Telegram signal stop file present");
                        break Ok(());
                    }
                    if last_reload.elapsed() >= CHANNEL_RELOAD_INTERVAL {
                        last_reload = tokio::time::Instant::now();
                        let next_channels = load_channel_list(&settings.channels_path)?;
                        if next_channels.mtime != channels.mtime {
                            channels = next_channels;
                            subscribed = resolve_channels(&client, &channels).await;
                            tracing::info!(channels = channels.channels.len(), "Reloaded Telegram channel list");
                        }
                    }
                }
                update = next => match update {
                    None => break Ok(()),
                    Some(Err(grammers_client::InvocationError::Dropped)) => {
                        tracing::info!("Telegram connection closed");
                        break Ok(());
                    }
                    Some(Err(err)) => break Err(anyhow::anyhow!("Telegram update stream failed: {err}")),
                    Some(Ok(Update::NewMessage(message))) => {
                        if let Some(configured) = subscribed.get(&message.peer_id()) {
                            let msg = incoming(&message, configured);
                            if let Err(err) = processor.process(&msg, &channels).await {
                                tracing::warn!(%err, "Telegram signal message processing failed");
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                },
            }
        };
        if let Err(err) = stream.sync_update_state().await {
            tracing::warn!(%err, "cannot persist Telegram update state");
        }
        outcome
    })
    .await;

    handle.quit();
    let _ = pool_task.await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::testing::RecordingAlerter;
    use crate::common::FixedClock;

    const EVM: &str = "0x1234567890abcdef1234567890ABCDEF12345678";
    const SOL: &str = "9xQeWvG816bUx9EPfT8GK8uE2LJQku3Z34Y3tjnT4W";
    const SOL2: &str = "So11111111111111111111111111111111111111112";

    fn d(chain: &'static str, address: &str) -> DetectedAddress {
        DetectedAddress {
            chain,
            address: address.into(),
        }
    }

    #[test]
    fn extracts_evm_and_solana_addresses() {
        assert_eq!(
            extract_contract_addresses(&format!("fresh pair {EVM}")),
            vec![d("ethereum", EVM)]
        );
        assert_eq!(
            extract_contract_addresses(&format!("sol ca {SOL}")),
            vec![d("solana", SOL)]
        );
    }

    #[test]
    fn solana_false_positives_ignored() {
        for text in [
            "short Abc123",
            "abcdefghijklmnopqrstuvwxyzabcdefghjkmnpqrstuv",
            "thisisaverylonglowercasestringwithoutdigits",
        ] {
            assert!(extract_contract_addresses(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn mixed_message_preserves_order_and_uniques() {
        let text = format!("{SOL} then {EVM} then {SOL2} and duplicate {EVM}");
        assert_eq!(
            extract_contract_addresses(&text),
            vec![d("solana", SOL), d("ethereum", EVM), d("solana", SOL2)]
        );
    }

    #[test]
    fn channel_list_defaults_written_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channels.json");
        let list = load_channel_list(&path).unwrap();
        assert_eq!(
            list.usernames(),
            vec!["binance_announcements", "WatcherGuru", "coingecko"]
        );
        assert_eq!(
            read_json_lenient(&path).unwrap()["channels"][0]["weight"],
            json!(0.4)
        );
    }

    #[test]
    fn channel_list_ignores_malformed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channels.json");
        std::fs::write(
            &path,
            json!({"channels": [
                {"username": "@AlphaRoom", "weight": 0.9},
                {"username": "", "weight": 0.5},
                {"username": "BadWeight", "weight": "high"},
                {"username": "TooHigh", "weight": 1.1},
                {"username": "BoolWeight", "weight": true},
                {"username": "alpharoom", "weight": 0.1},
                "not an object",
            ]})
            .to_string(),
        )
        .unwrap();
        let list = load_channel_list(&path).unwrap();
        assert_eq!(
            list.channels,
            vec![TelegramChannel {
                username: "AlphaRoom".into(),
                weight: 0.9
            }]
        );
        assert_eq!(list.weight_for("missing"), 0.5);
        assert_eq!(list.weight_for("@alpharoom"), 0.9);

        std::fs::write(&path, "{").unwrap();
        assert!(load_channel_list(&path).unwrap().channels.is_empty());
    }

    #[test]
    fn dedup_window_per_channel_address_pair() {
        let mut w = DedupWindow::default();
        assert!(w.allows("1", EVM, 1_000_000));
        assert!(!w.allows("1", EVM, 1_000_100));
        assert!(w.allows("2", EVM, 1_000_200));
        assert!(w.allows("1", EVM, 1_000_000 + 3_600_000));
    }

    fn settings(dir: &Path) -> TelegramSettings {
        TelegramSettings {
            api_id: "12345".into(),
            api_hash: "hash".into(),
            session_path: dir.join("session.json"),
            channels_path: dir.join("channels.json"),
            stop_path: dir.join("stop"),
            ledger_path: dir.join("ledger.jsonl"),
            watchlist_path: dir.join("watchlist.json"),
            ..TelegramSettings::default()
        }
    }

    fn channels(username: &str, weight: f64) -> TelegramChannelList {
        TelegramChannelList {
            channels: vec![TelegramChannel {
                username: username.into(),
                weight,
            }],
            mtime: None,
        }
    }

    #[tokio::test]
    async fn process_writes_ledger_watchlist_and_alerts() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let alerter = Arc::new(RecordingAlerter::default());
        let mut p = TelegramSignalProcessor::new(
            s.clone(),
            Arc::new(FixedClock(2_000_000)),
            alerter.clone(),
        );
        let msg = IncomingMessage {
            text: format!("new contract {EVM}"),
            channel_username: "WatcherGuru".into(),
            channel_id: "123".into(),
            message_id: "77".into(),
        };

        let candidates = p
            .process(&msg, &channels("WatcherGuru", 0.3))
            .await
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].source, "telegram");
        assert_eq!(candidates[0].confidence, 0.3);
        assert_eq!(
            Value::Object(candidates[0].raw_signal.clone()),
            json!({
                "channel_username": "WatcherGuru",
                "channel_id": "123",
                "message_id": "77",
                "message_text": format!("new contract {EVM}"),
                "message_url": "https://t.me/WatcherGuru/77",
            })
        );
        let row: Value = serde_json::from_str(
            std::fs::read_to_string(&s.ledger_path)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(row["source"], "telegram");
        assert_eq!(row["token_address"], EVM);
        let wl = read_json_lenient(&s.watchlist_path).unwrap();
        assert_eq!(wl["watching"][0]["token"], EVM);
        assert_eq!(wl["watching"][0]["source"], "telegram");
        assert_eq!(alerter.sent().len(), 1);
    }

    #[tokio::test]
    async fn process_respects_rate_limit() {
        let dir = tempfile::tempdir().unwrap();
        let s = TelegramSettings {
            max_alerts_per_hour: 1,
            ..settings(dir.path())
        };
        let alerter = Arc::new(RecordingAlerter::default());
        let mut p =
            TelegramSignalProcessor::new(s, Arc::new(FixedClock(2_000_000)), alerter.clone());
        let msg = IncomingMessage {
            text: format!("{EVM} {SOL}"),
            channel_username: "coingecko".into(),
            channel_id: "9".into(),
            message_id: "5".into(),
        };
        assert_eq!(
            p.process(&msg, &channels("coingecko", 0.2))
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(alerter.sent().len(), 1);
    }

    #[test]
    fn missing_session_is_a_config_error_pointing_at_setup() {
        let dir = tempfile::tempdir().unwrap();
        let err = settings(dir.path()).validate().unwrap_err().to_string();
        assert!(err.contains("signals-telegram-setup"));
        assert!(err.to_lowercase().contains("session file missing"));
        let bad = TelegramSettings {
            api_id: "abc".into(),
            ..settings(dir.path())
        };
        assert!(bad.validate().unwrap_err().to_string().contains("integer"));
    }
}

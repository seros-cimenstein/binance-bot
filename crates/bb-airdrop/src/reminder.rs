//! On-chain activity snapshots + Telegram digest of pending airdrop tasks.
//!
//! 1. **Activity detection** — every wallet has a monotonic counter on-chain
//!    (EVM transaction count, Solana signature count), snapshotted per ISO
//!    week. If this run's counter exceeds the most recent snapshot from a
//!    prior week, the wallet did something: every pending active opportunity
//!    for that wallet's chain is auto-marked completed.
//! 2. **Telegram digest** — a compact HTML message listing what is still
//!    pending per wallet, with the *official* URL so the operator can verify
//!    before connecting a wallet.
//!
//! `airdrop-scheduler remind` runs [`run_reminder`] once (daily timer).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use bb_core::telegram::{Telegram, escape};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::{Chain, Confidence, Status, filter_by};
use crate::error::{AirdropError, Result};
use crate::scheduler::{
    ChecklistRow, CompletionState, RowStatus, TaskKey, build_checklist, group_by_wallet,
    iso_week_label, load_state, save_state,
};
use crate::state::{load_json, save_json};
use crate::wallet::{EncryptedWallet, WalletStore, load_store};

pub const EVM_BASE_RPC_DEFAULT: &str = "https://mainnet.base.org";
pub const SOLANA_RPC_DEFAULT: &str = "https://api.mainnet-beta.solana.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Snapshots kept per wallet (~3 months of weekly history).
const HISTORY_CAP: usize = 12;
const SOLANA_SIGNATURE_LIMIT: u32 = 1000;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReminderSettings {
    pub evm_rpc_url: String,
    pub solana_rpc_url: String,
    pub min_confidence: Confidence,
    /// `None` when `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` are unset.
    pub telegram: Option<Telegram>,
}

impl Default for ReminderSettings {
    fn default() -> Self {
        Self {
            evm_rpc_url: EVM_BASE_RPC_DEFAULT.to_owned(),
            solana_rpc_url: SOLANA_RPC_DEFAULT.to_owned(),
            min_confidence: Confidence::Medium,
            telegram: None,
        }
    }
}

impl ReminderSettings {
    pub fn from_env() -> Self {
        use bb_core::env::string;
        Self {
            evm_rpc_url: string(&["AIRDROP_EVM_RPC_URL"], EVM_BASE_RPC_DEFAULT),
            solana_rpc_url: string(&["AIRDROP_SOLANA_RPC_URL"], SOLANA_RPC_DEFAULT),
            min_confidence: Confidence::parse_or_low(&string(
                &["AIRDROP_MIN_CONFIDENCE"],
                "medium",
            )),
            telegram: Telegram::from_env(),
        }
    }
}

/// Shared RPC client with the reminder's 15 s timeout.
pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?)
}

// ---------------------------------------------------------------------------
// Activity snapshot store
// ---------------------------------------------------------------------------

/// One wallet's activity counter at a point in time (alphabetical field order
/// matches the Python `sort_keys=True` writer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySnapshot {
    pub address: String,
    /// Full ISO timestamp.
    pub captured_at_iso: String,
    pub chain: Chain,
    /// EVM nonce or Solana signature count.
    pub counter: u64,
    /// e.g. `"2026-W22"`.
    pub iso_week: String,
    pub wallet_name: String,
}

/// Per-wallet history of snapshots, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Value")]
pub struct ActivityStore {
    pub snapshots: BTreeMap<String, Vec<ActivitySnapshot>>,
}

/// Lenient like the Python loader: a non-object `snapshots` is empty and
/// non-list histories are skipped, but a malformed snapshot is an error.
impl TryFrom<Value> for ActivityStore {
    type Error = AirdropError;

    fn try_from(payload: Value) -> Result<Self> {
        let Value::Object(mut obj) = payload else {
            return Ok(Self::default());
        };
        let Some(Value::Object(raw)) = obj.remove("snapshots") else {
            return Ok(Self::default());
        };
        let mut snapshots = BTreeMap::new();
        for (name, entries) in raw {
            let Value::Array(entries) = entries else {
                continue;
            };
            let history = entries
                .into_iter()
                .map(|e| {
                    serde_json::from_value(e)
                        .map_err(|e| AirdropError::MalformedSnapshot(e.to_string()))
                })
                .collect::<Result<Vec<ActivitySnapshot>>>()?;
            snapshots.insert(name, history);
        }
        Ok(Self { snapshots })
    }
}

impl ActivityStore {
    pub fn latest(&self, wallet_name: &str) -> Option<&ActivitySnapshot> {
        self.snapshots.get(wallet_name).and_then(|h| h.last())
    }

    /// Most recent snapshot from a week other than `iso_week`.
    pub fn previous_in_iso_week(
        &self,
        wallet_name: &str,
        iso_week: &str,
    ) -> Option<&ActivitySnapshot> {
        self.snapshots
            .get(wallet_name)?
            .iter()
            .rev()
            .find(|s| s.iso_week != iso_week)
    }

    /// Repeated runs within the same ISO week overwrite the tail; history is
    /// capped at [`HISTORY_CAP`] entries per wallet.
    pub fn record(&mut self, snap: ActivitySnapshot) {
        let history = self.snapshots.entry(snap.wallet_name.clone()).or_default();
        match history.last_mut() {
            Some(last) if last.iso_week == snap.iso_week => *last = snap,
            _ => history.push(snap),
        }
        let excess = history.len().saturating_sub(HISTORY_CAP);
        history.drain(..excess);
    }
}

pub fn load_activity_store(path: &Path) -> Result<ActivityStore> {
    load_json(path, "activity store")
}

pub fn save_activity_store(store: &ActivityStore, path: &Path) -> Result<()> {
    save_json(path, store)
}

// ---------------------------------------------------------------------------
// RPC counter fetchers
// ---------------------------------------------------------------------------

async fn post_rpc(http: &reqwest::Client, url: &str, payload: &Value) -> Result<Value> {
    let mut data: Value = http
        .post(url)
        .json(payload)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(error) = data.get("error") {
        return Err(AirdropError::Rpc {
            url: url.to_owned(),
            error: error.to_string(),
        });
    }
    Ok(data
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// `eth_getTransactionCount` — the wallet's outgoing-tx nonce.
pub async fn fetch_evm_nonce(http: &reqwest::Client, rpc_url: &str, address: &str) -> Result<u64> {
    let result = post_rpc(
        http,
        rpc_url,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_getTransactionCount",
            "params": [address, "latest"],
        }),
    )
    .await?;
    let unexpected = || AirdropError::UnexpectedRpcResult {
        what: "nonce",
        payload: result.to_string(),
    };
    let hex = result.as_str().ok_or_else(unexpected)?;
    let digits = hex
        .strip_prefix("0x")
        .or_else(|| hex.strip_prefix("0X"))
        .unwrap_or(hex);
    u64::from_str_radix(digits, 16).map_err(|_| unexpected())
}

/// `getSignaturesForAddress` length as a monotonic counter. Capped at 1000 by
/// the RPC; for low-activity airdrop wallets that is effectively lifetime.
pub async fn fetch_solana_signature_count(
    http: &reqwest::Client,
    rpc_url: &str,
    address: &str,
) -> Result<u64> {
    let result = post_rpc(
        http,
        rpc_url,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "getSignaturesForAddress",
            "params": [address, {"limit": SOLANA_SIGNATURE_LIMIT}],
        }),
    )
    .await?;
    match result {
        Value::Array(sigs) => Ok(sigs.len() as u64),
        other => Err(AirdropError::UnexpectedRpcResult {
            what: "sigs",
            payload: other.to_string(),
        }),
    }
}

pub async fn fetch_counter_for_wallet(
    http: &reqwest::Client,
    wallet: &EncryptedWallet,
    settings: &ReminderSettings,
) -> Result<u64> {
    match wallet.chain {
        Chain::Evm => fetch_evm_nonce(http, &settings.evm_rpc_url, &wallet.address).await,
        Chain::Solana => {
            fetch_solana_signature_count(http, &settings.solana_rpc_url, &wallet.address).await
        }
        Chain::Cex => Err(AirdropError::UnknownChain(wallet.chain.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Detection + auto-mark
// ---------------------------------------------------------------------------

/// True if the wallet ran at least one new transaction since the most recent
/// prior-week snapshot; `false` on the very first run (no baseline).
pub fn wallet_was_active_this_week(
    current_counter: u64,
    previous: Option<&ActivitySnapshot>,
) -> bool {
    previous.is_some_and(|p| current_counter > p.counter)
}

/// Mark every pending active opportunity on each active wallet's chain as
/// completed this week; returns the newly marked keys.
pub fn auto_mark_completed_from_activity(
    completion_state: &mut CompletionState,
    wallets_with_activity: &[&EncryptedWallet],
    iso_week: &str,
    min_confidence: Option<Confidence>,
) -> Vec<TaskKey> {
    let mut newly_marked = Vec::new();
    for wallet in wallets_with_activity {
        for opp in filter_by(Some(wallet.chain), Some(Status::Active), min_confidence) {
            let key = TaskKey::new(&wallet.name, opp.name);
            if completion_state.is_completed_this_week(&key, iso_week) {
                continue;
            }
            completion_state.mark(&key, iso_week);
            newly_marked.push(key);
        }
    }
    newly_marked
}

// ---------------------------------------------------------------------------
// Snapshot orchestration
// ---------------------------------------------------------------------------

/// Snapshot every wallet's counter into `activity_store` and return the
/// wallets that were active this week. RPC failures are logged and skipped
/// (the next run retries).
pub async fn snapshot_all_wallets<'a>(
    http: &reqwest::Client,
    settings: &ReminderSettings,
    store: &'a WalletStore,
    activity_store: &mut ActivityStore,
    iso_week: &str,
    now: DateTime<Utc>,
) -> Vec<&'a EncryptedWallet> {
    let mut active = Vec::new();
    for wallet in &store.wallets {
        let prior = activity_store
            .previous_in_iso_week(&wallet.name, iso_week)
            .cloned();
        let counter = match fetch_counter_for_wallet(http, wallet, settings).await {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(wallet = %wallet.name, error = %err, "airdrop reminder: failed to fetch counter");
                continue;
            }
        };
        activity_store.record(ActivitySnapshot {
            address: wallet.address.clone(),
            captured_at_iso: now.to_rfc3339_opts(SecondsFormat::Micros, false),
            chain: wallet.chain,
            counter,
            iso_week: iso_week.to_owned(),
            wallet_name: wallet.name.clone(),
        });
        if wallet_was_active_this_week(counter, prior.as_ref()) {
            active.push(wallet);
        }
    }
    active
}

// ---------------------------------------------------------------------------
// Telegram digest
// ---------------------------------------------------------------------------

/// Compact HTML digest: pending rows grouped by wallet (sorted by name), plus
/// a call-out of wallets whose activity was auto-detected.
pub fn render_telegram_digest(
    iso_week: &str,
    pending_rows: &[ChecklistRow],
    active_wallet_names: &[&str],
) -> String {
    let mut lines = vec![
        format!("<b>Airdrop check-in</b> ({})", escape(iso_week)),
        String::new(),
    ];
    if !active_wallet_names.is_empty() {
        let names: BTreeSet<&str> = active_wallet_names.iter().copied().collect();
        let names = names.into_iter().collect::<Vec<_>>().join(", ");
        lines.push(format!(
            "✅ Auto-detected on-chain activity: <code>{}</code> — their pending tasks were marked done.",
            escape(&names)
        ));
        lines.push(String::new());
    }
    if pending_rows.is_empty() {
        lines.push("Nothing pending. Nice.".to_owned());
        return lines.join("\n").trim().to_owned();
    }
    let mut groups = group_by_wallet(pending_rows);
    groups.sort_by(|a, b| a.0.cmp(b.0));
    for (wallet_name, rows) in groups {
        let label = if rows[0].chain == Chain::Cex {
            "Binance account"
        } else {
            rows[0].chain.as_str()
        };
        lines.push(format!(
            "<b>{}</b> ({label}) — {} pending:",
            escape(wallet_name),
            rows.len()
        ));
        for row in rows {
            lines.push(format!(
                "  • <a href=\"{}\">{}</a>: {} (min ${}, {} confidence)",
                html_escape::encode_double_quoted_attribute(&row.url),
                escape(&row.opportunity),
                escape(&row.activity_kind),
                row.min_size_usd.round_dp(0),
                row.confidence
            ));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim().to_owned()
}

/// Push the digest; `false` when Telegram is unconfigured or the send fails.
pub async fn send_telegram_digest(settings: &ReminderSettings, message: &str) -> bool {
    match &settings.telegram {
        Some(tg) => tg.send(message).await,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Top-level orchestrator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReminderResult {
    pub iso_week: String,
    pub active_wallets: Vec<String>,
    pub auto_marked: Vec<String>,
    pub pending_rows: usize,
    pub telegram_pushed: bool,
    /// Rendered digest (printed by the CLI when Telegram is unconfigured).
    #[serde(skip)]
    pub digest: String,
}

pub struct ReminderPaths<'a> {
    pub store: &'a Path,
    pub state: &'a Path,
    pub activity: &'a Path,
}

/// One-shot: snapshot → detect activity → auto-mark → push digest.
pub async fn run_reminder(
    http: &reqwest::Client,
    settings: &ReminderSettings,
    paths: &ReminderPaths<'_>,
    now: DateTime<Utc>,
) -> Result<ReminderResult> {
    let iso = iso_week_label(now);
    let store = load_store(paths.store)?;
    let mut completion_state = load_state(paths.state)?;
    let mut activity_store = load_activity_store(paths.activity)?;

    let active = snapshot_all_wallets(http, settings, &store, &mut activity_store, &iso, now).await;
    save_activity_store(&activity_store, paths.activity)?;

    let auto_marked = auto_mark_completed_from_activity(
        &mut completion_state,
        &active,
        &iso,
        Some(settings.min_confidence),
    );
    save_state(&completion_state, paths.state)?;

    let pending: Vec<ChecklistRow> = build_checklist(
        &store,
        &completion_state,
        &iso,
        Some(settings.min_confidence),
        Some(Status::Active),
    )
    .into_iter()
    .filter(|row| row.status == RowStatus::Pending)
    .collect();

    let active_names: Vec<&str> = active.iter().map(|w| w.name.as_str()).collect();
    let digest = render_telegram_digest(&iso, &pending, &active_names);
    let telegram_pushed = send_telegram_digest(settings, &digest).await;

    Ok(ReminderResult {
        iso_week: iso,
        active_wallets: active_names.iter().map(|&n| n.to_owned()).collect(),
        auto_marked: auto_marked.iter().map(TaskKey::as_key).collect(),
        pending_rows: pending.len(),
        telegram_pushed,
        digest,
    })
}

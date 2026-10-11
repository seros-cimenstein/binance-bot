//! Weekly activity scheduler for the airdrop farmer.
//!
//! Takes the wallet store + opportunity catalog and produces a per-wallet
//! checklist of things to do this week: one row per (wallet, opportunity on
//! the wallet's chain), plus wallet-less exchange-account (`cex`) programs
//! under the [`CEX_WALLET_NAME`] pseudo-wallet. A row is `completed` when the
//! persisted [`CompletionState`] holds the current ISO week for its
//! [`TaskKey`], otherwise `pending`. The operator marks rows done/undone via
//! the CLI; the reminder auto-marks wallets with detected on-chain activity.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use chrono::{DateTime, Datelike as _, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::catalog::{AirdropOpportunity, Chain, Confidence, Status, filter_by};
use crate::error::{AirdropError, Result};
use crate::state::{load_json, save_json};
use crate::wallet::WalletStore;

/// Pseudo-wallet that exchange-account programs are rendered under.
pub const CEX_WALLET_NAME: &str = "binance";

/// Stable identifier for one (wallet, opportunity) pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskKey {
    pub wallet_name: String,
    pub opportunity_name: String,
}

impl TaskKey {
    pub fn new(wallet_name: impl Into<String>, opportunity_name: impl Into<String>) -> Self {
        Self {
            wallet_name: wallet_name.into(),
            opportunity_name: opportunity_name.into(),
        }
    }

    pub fn as_key(&self) -> String {
        format!("{}::{}", self.wallet_name, self.opportunity_name)
    }
}

impl std::str::FromStr for TaskKey {
    type Err = AirdropError;

    fn from_str(raw: &str) -> Result<Self> {
        raw.split_once("::")
            .map(|(w, o)| Self::new(w, o))
            .ok_or_else(|| AirdropError::InvalidTaskKey(raw.to_owned()))
    }
}

/// Per-task last-completed-week marker (`"wallet::opportunity" -> "2026-W22"`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "serde_json::Value")]
pub struct CompletionState {
    pub completions: BTreeMap<String, String>,
}

/// Lenient like the Python loader: a missing/non-object `completions` is
/// empty and non-string week values are stringified.
impl From<serde_json::Value> for CompletionState {
    fn from(payload: serde_json::Value) -> Self {
        let completions = match payload {
            serde_json::Value::Object(mut obj) => match obj.remove("completions") {
                Some(serde_json::Value::Object(raw)) => raw
                    .into_iter()
                    .map(|(k, v)| match v {
                        serde_json::Value::String(s) => (k, s),
                        other => (k, other.to_string()),
                    })
                    .collect(),
                _ => BTreeMap::new(),
            },
            _ => BTreeMap::new(),
        };
        Self { completions }
    }
}

impl CompletionState {
    pub fn is_completed_this_week(&self, key: &TaskKey, current_iso_week: &str) -> bool {
        self.completions
            .get(&key.as_key())
            .is_some_and(|w| w == current_iso_week)
    }

    pub fn mark(&mut self, key: &TaskKey, iso_week: &str) {
        self.completions.insert(key.as_key(), iso_week.to_owned());
    }

    pub fn unmark(&mut self, key: &TaskKey) {
        self.completions.remove(&key.as_key());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RowStatus {
    Completed,
    Pending,
}

/// One checklist line; JSON field order matches the Python `asdict` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChecklistRow {
    pub wallet_name: String,
    pub wallet_address: String,
    pub chain: Chain,
    pub opportunity: String,
    pub url: String,
    pub activity_kind: String,
    pub min_actions_per_week: u32,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub min_size_usd: Decimal,
    pub confidence: Confidence,
    pub status: RowStatus,
    pub notes: String,
}

/// ISO-week label (e.g. `"2026-W22"`) used as the completion marker.
pub fn iso_week_label(now: DateTime<Utc>) -> String {
    let w = now.iso_week();
    format!("{}-W{:02}", w.year(), w.week())
}

pub fn current_iso_week() -> String {
    iso_week_label(bb_core::time::now())
}

fn checklist_row(
    wallet_name: &str,
    wallet_address: &str,
    chain: Chain,
    opp: &AirdropOpportunity,
    state: &CompletionState,
    iso_week: &str,
) -> ChecklistRow {
    let key = TaskKey::new(wallet_name, opp.name);
    let status = if state.is_completed_this_week(&key, iso_week) {
        RowStatus::Completed
    } else {
        RowStatus::Pending
    };
    ChecklistRow {
        wallet_name: wallet_name.to_owned(),
        wallet_address: wallet_address.to_owned(),
        chain,
        opportunity: opp.name.to_owned(),
        url: opp.url.to_owned(),
        activity_kind: opp.activity_kind.to_owned(),
        min_actions_per_week: opp.min_actions_per_week,
        min_size_usd: opp.min_size_usd,
        confidence: opp.confidence,
        status,
        notes: opp.notes.to_owned(),
    }
}

/// Rows for every stored wallet × matching opportunity, then the wallet-less
/// exchange-account programs once under [`CEX_WALLET_NAME`].
pub fn build_checklist(
    store: &WalletStore,
    state: &CompletionState,
    iso_week: &str,
    min_confidence: Option<Confidence>,
    status_filter: Option<Status>,
) -> Vec<ChecklistRow> {
    let wallet_rows = store.wallets.iter().flat_map(|wallet| {
        filter_by(Some(wallet.chain), status_filter, min_confidence)
            .into_iter()
            .map(|opp| {
                checklist_row(
                    &wallet.name,
                    &wallet.address,
                    wallet.chain,
                    opp,
                    state,
                    iso_week,
                )
            })
    });
    let cex_rows = filter_by(Some(Chain::Cex), status_filter, min_confidence)
        .into_iter()
        .map(|opp| checklist_row(CEX_WALLET_NAME, "", Chain::Cex, opp, state, iso_week));
    wallet_rows.chain(cex_rows).collect()
}

/// Empty state when `path` does not exist.
pub fn load_state(path: &Path) -> Result<CompletionState> {
    load_json(path, "scheduler state")
}

pub fn save_state(state: &CompletionState, path: &Path) -> Result<()> {
    save_json(path, state)
}

/// Group rows by wallet name, preserving first-seen order of wallets and rows.
pub(crate) fn group_by_wallet<'a>(
    rows: impl IntoIterator<Item = &'a ChecklistRow>,
) -> Vec<(&'a str, Vec<&'a ChecklistRow>)> {
    let mut groups: Vec<(&str, Vec<&ChecklistRow>)> = Vec::new();
    for row in rows {
        match groups.iter_mut().find(|(name, _)| *name == row.wallet_name) {
            Some((_, group)) => group.push(row),
            None => groups.push((&row.wallet_name, vec![row])),
        }
    }
    groups
}

/// Plain-text checklist for the terminal.
pub fn render_checklist(rows: &[ChecklistRow], iso_week: &str) -> String {
    let rule = "=".repeat(72);
    let mut out = format!("{rule}\n  AIRDROP CHECKLIST  ({iso_week})\n{rule}");
    if rows.is_empty() {
        out.push_str(
            "\n  (no tasks scheduled; either wallet store is empty or no active opportunities)",
        );
        return out;
    }
    for (wallet, wallet_rows) in group_by_wallet(rows) {
        let first = wallet_rows[0];
        let _ = write!(out, "\n\n  WALLET: {wallet}  ({})", first.chain);
        if first.chain == Chain::Cex {
            out.push_str("\n  ACCOUNT: Binance (exchange program — no wallet needed)");
        } else {
            let _ = write!(out, "\n  ADDRESS: {}", first.wallet_address);
        }
        for row in wallet_rows {
            let mark = if row.status == RowStatus::Completed {
                "[x]"
            } else {
                "[ ]"
            };
            let _ = write!(
                out,
                "\n    {mark} {:30}  {}",
                row.opportunity, row.activity_kind
            );
            let _ = write!(
                out,
                "\n        {}  ({}x/wk, min ${}, confidence={})",
                row.url,
                row.min_actions_per_week,
                row.min_size_usd.round_dp(0),
                row.confidence
            );
            if !row.notes.is_empty() {
                let _ = write!(out, "\n        note: {}", row.notes);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::add_wallet;

    fn store_with(wallets: &[(&str, Chain)]) -> WalletStore {
        let mut s = WalletStore {
            kdf_iterations: 1_000,
            ..WalletStore::default()
        };
        for (name, chain) in wallets {
            add_wallet(&mut s, name, *chain, "pp").unwrap();
        }
        s
    }

    fn active(chain: Chain) -> Vec<&'static AirdropOpportunity> {
        filter_by(Some(chain), Some(Status::Active), None)
    }

    #[test]
    fn iso_week_label_has_year_w_format() {
        let label = current_iso_week();
        assert!(label.contains("-W") && label.len() == 8);
        let ts = DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(iso_week_label(ts), "2026-W53");
    }

    #[test]
    fn task_key_round_trip() {
        let k = TaskKey::new("w1", "Kamino Finance");
        assert_eq!(k.as_key().parse::<TaskKey>().unwrap(), k);
        assert!("no-separator".parse::<TaskKey>().is_err());
    }

    #[test]
    fn completion_state_mark_then_check() {
        let mut state = CompletionState::default();
        let k = TaskKey::new("w", "o");
        state.mark(&k, "2026-W22");
        assert!(state.is_completed_this_week(&k, "2026-W22"));
        assert!(!state.is_completed_this_week(&k, "2026-W23"));
    }

    #[test]
    fn completion_state_unmark_removes_entry() {
        let mut state = CompletionState::default();
        let k = TaskKey::new("w", "o");
        state.mark(&k, "2026-W22");
        state.unmark(&k);
        assert!(!state.is_completed_this_week(&k, "2026-W22"));
    }

    #[test]
    fn completion_state_parses_leniently() {
        let s: CompletionState = serde_json::from_str(r#"{"completions": []}"#).unwrap();
        assert!(s.completions.is_empty());
        let s: CompletionState = serde_json::from_str(r#"{"completions": {"a::b": 7}}"#).unwrap();
        assert_eq!(s.completions["a::b"], "7");
    }

    #[test]
    fn build_checklist_emits_one_row_per_wallet_opp_pair() {
        let store = store_with(&[("sol-1", Chain::Solana), ("evm-1", Chain::Evm)]);
        let rows = build_checklist(
            &store,
            &CompletionState::default(),
            "2026-W22",
            None,
            Some(Status::Active),
        );
        assert_eq!(
            rows.len(),
            active(Chain::Solana).len() + active(Chain::Evm).len() + active(Chain::Cex).len()
        );
    }

    #[test]
    fn build_checklist_emits_walletless_cex_rows() {
        let rows = build_checklist(
            &WalletStore::default(),
            &CompletionState::default(),
            "2026-W22",
            None,
            Some(Status::Active),
        );
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.chain == Chain::Cex
            && r.wallet_name == CEX_WALLET_NAME
            && r.wallet_address.is_empty()));
    }

    #[test]
    fn render_checklist_labels_cex_account_without_address() {
        let rows = build_checklist(
            &WalletStore::default(),
            &CompletionState::default(),
            "2026-W22",
            None,
            Some(Status::Active),
        );
        let text = render_checklist(&rows, "2026-W22");
        assert!(text.contains("ACCOUNT: Binance"));
        assert!(!text.contains("ADDRESS:"));
    }

    #[test]
    fn build_checklist_marks_completed_rows() {
        let store = store_with(&[("sol-1", Chain::Solana)]);
        let target = active(Chain::Solana)[0];
        let mut state = CompletionState::default();
        state.mark(&TaskKey::new("sol-1", target.name), "2026-W22");
        let rows = build_checklist(&store, &state, "2026-W22", None, Some(Status::Active));
        let completed: Vec<_> = rows
            .iter()
            .filter(|r| r.status == RowStatus::Completed)
            .collect();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].opportunity, target.name);
        assert!(
            rows.iter()
                .filter(|r| r.status == RowStatus::Pending)
                .all(|r| r.opportunity != target.name)
        );
    }

    #[test]
    fn state_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = CompletionState::default();
        state.mark(&TaskKey::new("w1", "Kamino"), "2026-W22");
        save_state(&state, &path).unwrap();
        let loaded = load_state(&path).unwrap();
        assert!(loaded.is_completed_this_week(&TaskKey::new("w1", "Kamino"), "2026-W22"));
        assert!(
            load_state(&dir.path().join("missing.json"))
                .unwrap()
                .completions
                .is_empty()
        );
    }

    #[test]
    fn load_state_reports_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{").unwrap();
        let err = load_state(&path).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("could not read scheduler state at")
        );
    }

    #[test]
    fn render_checklist_includes_address_and_status_marks() {
        let store = store_with(&[("sol-1", Chain::Solana)]);
        let rows = build_checklist(
            &store,
            &CompletionState::default(),
            "2026-W22",
            None,
            Some(Status::Active),
        );
        let text = render_checklist(&rows, "2026-W22");
        assert!(text.contains("AIRDROP CHECKLIST"));
        assert!(text.contains("WALLET: sol-1"));
        assert!(text.contains(&format!("ADDRESS: {}", store.wallets[0].address)));
        assert!(text.contains("[ ]"));
        assert!(text.contains("min $20, confidence=high"));
    }

    #[test]
    fn checklist_row_json_keeps_python_shape() {
        let rows = build_checklist(
            &WalletStore::default(),
            &CompletionState::default(),
            "2026-W22",
            None,
            None,
        );
        let v = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(v["chain"], "cex");
        assert_eq!(v["status"], "pending");
        assert_eq!(serde_json::to_string(&v["min_size_usd"]).unwrap(), "200.0");
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Reminder behaviour: activity store, detection, auto-mark, digest, RPC and
//! the end-to-end run (RPC + Telegram mocked with wiremock).

use bb_airdrop::catalog::{Chain, Status, filter_by};
use bb_airdrop::reminder::{
    ActivitySnapshot, ActivityStore, ReminderPaths, ReminderSettings,
    auto_mark_completed_from_activity, fetch_counter_for_wallet, fetch_evm_nonce,
    fetch_solana_signature_count, load_activity_store, render_telegram_digest, run_reminder,
    save_activity_store, snapshot_all_wallets, wallet_was_active_this_week,
};
use bb_airdrop::scheduler::{
    CEX_WALLET_NAME, ChecklistRow, CompletionState, RowStatus, TaskKey, load_state,
};
use bb_airdrop::wallet::{EncryptedWallet, WalletStore, add_wallet, save_store};
use bb_core::telegram::Telegram;
use chrono::{DateTime, Utc};
use rust_decimal_macros::dec;
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn snap(name: &str, counter: u64, iso_week: &str, chain: Chain) -> ActivitySnapshot {
    ActivitySnapshot {
        address: "0xabc".into(),
        captured_at_iso: "2026-06-01T00:00:00+00:00".into(),
        chain,
        counter,
        iso_week: iso_week.into(),
        wallet_name: name.into(),
    }
}

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

fn settings(evm: &str, sol: &str) -> ReminderSettings {
    ReminderSettings {
        evm_rpc_url: evm.into(),
        solana_rpc_url: sol.into(),
        ..ReminderSettings::default()
    }
}

/// Thursday of ISO week 2026-W22.
fn w22() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-05-28T09:00:00Z")
        .unwrap()
        .to_utc()
}

async fn rpc_server(result: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": result})),
        )
        .mount(&server)
        .await;
    server
}

// ---------------------------------------------------------------------------
// ActivityStore plumbing
// ---------------------------------------------------------------------------

#[test]
fn activity_store_records_and_returns_latest() {
    let mut s = ActivityStore::default();
    s.record(snap("w1", 5, "2026-W22", Chain::Evm));
    assert_eq!(s.latest("w1").unwrap().counter, 5);
}

#[test]
fn activity_store_replaces_within_same_iso_week() {
    let mut s = ActivityStore::default();
    s.record(snap("w1", 5, "2026-W22", Chain::Evm));
    s.record(snap("w1", 6, "2026-W22", Chain::Evm));
    assert_eq!(s.snapshots["w1"].len(), 1);
    assert_eq!(s.latest("w1").unwrap().counter, 6);
}

#[test]
fn activity_store_appends_on_new_iso_week() {
    let mut s = ActivityStore::default();
    s.record(snap("w1", 5, "2026-W22", Chain::Evm));
    s.record(snap("w1", 8, "2026-W23", Chain::Evm));
    assert_eq!(s.snapshots["w1"].len(), 2);
}

#[test]
fn activity_store_previous_in_iso_week_skips_current() {
    let mut s = ActivityStore::default();
    s.record(snap("w1", 5, "2026-W22", Chain::Evm));
    s.record(snap("w1", 6, "2026-W23", Chain::Evm));
    let prior = s.previous_in_iso_week("w1", "2026-W23").unwrap();
    assert_eq!((prior.iso_week.as_str(), prior.counter), ("2026-W22", 5));
}

#[test]
fn activity_store_previous_returns_none_when_no_prior_week() {
    let mut s = ActivityStore::default();
    s.record(snap("w1", 5, "2026-W22", Chain::Evm));
    assert!(s.previous_in_iso_week("w1", "2026-W22").is_none());
}

#[test]
fn activity_store_caps_history_at_twelve_entries() {
    let mut s = ActivityStore::default();
    for i in 0..20 {
        s.record(snap("w1", i, &format!("2026-W{i:02}"), Chain::Evm));
    }
    let history = &s.snapshots["w1"];
    assert_eq!(history.len(), 12);
    assert_eq!(history[0].counter, 8, "oldest entries are dropped");
}

#[test]
fn activity_store_round_trips_through_json() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("activity.json");
    let mut s = ActivityStore::default();
    s.record(snap("w1", 5, "2026-W22", Chain::Solana));
    s.record(snap("w1", 6, "2026-W23", Chain::Solana));
    save_activity_store(&s, &p).unwrap();
    let loaded = load_activity_store(&p).unwrap();
    let last = loaded.snapshots["w1"].last().unwrap();
    assert_eq!((last.counter, last.chain), (6, Chain::Solana));
}

#[test]
fn load_activity_store_missing_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        load_activity_store(&dir.path().join("nope.json"))
            .unwrap()
            .snapshots
            .is_empty()
    );
}

#[test]
fn activity_store_parses_python_file_and_rejects_malformed_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("activity.json");
    // Shape written by the Python reminder (sort_keys=True, isoformat()).
    std::fs::write(
        &p,
        r#"{"snapshots": {"evm-1": [{"address": "0x1", "captured_at_iso": "2026-06-01T09:00:00.123456+00:00",
            "chain": "evm", "counter": 3, "iso_week": "2026-W23", "wallet_name": "evm-1"}], "junk": 5}}"#,
    )
    .unwrap();
    let s = load_activity_store(&p).unwrap();
    assert_eq!(s.latest("evm-1").unwrap().counter, 3);
    assert!(!s.snapshots.contains_key("junk"));

    std::fs::write(&p, r#"{"snapshots": {"w": [{"wallet_name": "w"}]}}"#).unwrap();
    let err = load_activity_store(&p).unwrap_err().to_string();
    assert!(err.contains("activity snapshot malformed"), "{err}");
}

// ---------------------------------------------------------------------------
// Detection + auto-mark
// ---------------------------------------------------------------------------

#[test]
fn wallet_activity_detection() {
    let prior = snap("w1", 5, "2026-W22", Chain::Evm);
    assert!(wallet_was_active_this_week(7, Some(&prior)));
    assert!(!wallet_was_active_this_week(5, Some(&prior)));
    // First run can't infer activity — no baseline.
    assert!(!wallet_was_active_this_week(99, None));
}

#[test]
fn auto_mark_marks_all_pending_for_active_wallets() {
    let store = store_with(&[("sol-1", Chain::Solana)]);
    let mut state = CompletionState::default();
    let marked =
        auto_mark_completed_from_activity(&mut state, &[&store.wallets[0]], "2026-W22", None);
    let sol_active = filter_by(Some(Chain::Solana), Some(Status::Active), None);
    assert_eq!(marked.len(), sol_active.len());
    for opp in sol_active {
        assert!(state.is_completed_this_week(&TaskKey::new("sol-1", opp.name), "2026-W22"));
    }
}

#[test]
fn auto_mark_skips_already_marked_tasks() {
    let store = store_with(&[("sol-1", Chain::Solana)]);
    let sol_active = filter_by(Some(Chain::Solana), Some(Status::Active), None);
    let mut state = CompletionState::default();
    state.mark(&TaskKey::new("sol-1", sol_active[0].name), "2026-W22");
    let marked =
        auto_mark_completed_from_activity(&mut state, &[&store.wallets[0]], "2026-W22", None);
    assert_eq!(marked.len(), sol_active.len() - 1);
}

#[test]
fn auto_mark_only_touches_opportunities_on_matching_chain() {
    let store = store_with(&[("sol-1", Chain::Solana)]);
    let mut state = CompletionState::default();
    auto_mark_completed_from_activity(&mut state, &[&store.wallets[0]], "2026-W22", None);
    for opp in filter_by(Some(Chain::Evm), Some(Status::Active), None) {
        assert!(!state.is_completed_this_week(&TaskKey::new("sol-1", opp.name), "2026-W22"));
    }
}

#[test]
fn auto_mark_never_marks_cex_programs() {
    let store = store_with(&[("evm-1", Chain::Evm)]);
    let mut state = CompletionState::default();
    auto_mark_completed_from_activity(&mut state, &[&store.wallets[0]], "2026-W22", None);
    let cex_active = filter_by(Some(Chain::Cex), Some(Status::Active), None);
    assert!(!cex_active.is_empty());
    for opp in cex_active {
        assert!(
            !state.is_completed_this_week(&TaskKey::new(CEX_WALLET_NAME, opp.name), "2026-W22")
        );
    }
}

// ---------------------------------------------------------------------------
// Digest rendering
// ---------------------------------------------------------------------------

fn row(wallet: &str, chain: Chain, opp: &str) -> ChecklistRow {
    ChecklistRow {
        wallet_name: wallet.into(),
        wallet_address: "0xabc".into(),
        chain,
        opportunity: opp.into(),
        url: "https://x.io".into(),
        activity_kind: "swap".into(),
        min_actions_per_week: 1,
        min_size_usd: dec!(10.0),
        confidence: bb_airdrop::catalog::Confidence::Medium,
        status: RowStatus::Pending,
        notes: String::new(),
    }
}

#[test]
fn digest_lists_pending_grouped_by_wallet() {
    let rows = [
        row("sol-1", Chain::Solana, "Kamino"),
        row("sol-1", Chain::Solana, "Drift"),
        row("evm-1", Chain::Evm, "Linea"),
    ];
    let msg = render_telegram_digest("2026-W22", &rows, &[]);
    assert!(msg.contains("<b>sol-1</b> (solana) — 2 pending"));
    assert!(msg.contains("<b>evm-1</b> (evm) — 1 pending"));
    assert!(msg.contains("Kamino") && msg.contains("Drift") && msg.contains("Linea"));
    // Wallet groups are sorted by name.
    assert!(msg.find("evm-1").unwrap() < msg.find("sol-1").unwrap());
    assert!(
        msg.contains("  • <a href=\"https://x.io\">Kamino</a>: swap (min $10, medium confidence)")
    );
    assert!(!msg.ends_with('\n'));
}

#[test]
fn digest_highlights_active_wallets() {
    let msg = render_telegram_digest("2026-W22", &[], &["sol-1", "evm-1", "sol-1"]);
    assert!(msg.contains("Auto-detected on-chain activity: <code>evm-1, sol-1</code>"));
}

#[test]
fn digest_handles_empty_pending_with_no_activity() {
    let msg = render_telegram_digest("2026-W22", &[], &[]);
    assert_eq!(
        msg,
        "<b>Airdrop check-in</b> (2026-W22)\n\nNothing pending. Nice."
    );
}

#[test]
fn digest_labels_cex_group_as_binance_account_and_escapes_html() {
    let rows = [row(
        CEX_WALLET_NAME,
        Chain::Cex,
        "Binance Alpha Points & airdrops",
    )];
    let msg = render_telegram_digest("2026-W22", &rows, &[]);
    assert!(msg.contains(&format!(
        "<b>{CEX_WALLET_NAME}</b> (Binance account) — 1 pending"
    )));
    assert!(msg.contains("Binance Alpha Points &amp; airdrops"));
}

// ---------------------------------------------------------------------------
// RPC fetchers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_evm_nonce_parses_hex_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(
            json!({"method": "eth_getTransactionCount", "params": ["0xabc", "latest"]}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": "0x2a"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let n = fetch_evm_nonce(&reqwest::Client::new(), &server.uri(), "0xabc")
        .await
        .unwrap();
    assert_eq!(n, 42);
}

#[tokio::test]
async fn fetch_solana_signature_count_returns_list_len() {
    let sigs: Vec<_> = (0..7)
        .map(|i| json!({"signature": format!("sig{i}")}))
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "getSignaturesForAddress", "params": ["abc", {"limit": 1000}]}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": sigs})),
        )
        .mount(&server)
        .await;
    let n = fetch_solana_signature_count(&reqwest::Client::new(), &server.uri(), "abc")
        .await
        .unwrap();
    assert_eq!(n, 7);
}

#[tokio::test]
async fn rpc_error_payload_and_bad_result_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000}})),
        )
        .mount(&server)
        .await;
    let err = fetch_evm_nonce(&reqwest::Client::new(), &server.uri(), "0xabc")
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("RPC error from"));

    let server = rpc_server(json!(12)).await;
    let err = fetch_evm_nonce(&reqwest::Client::new(), &server.uri(), "0xabc")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unexpected nonce response"));
}

#[tokio::test]
async fn fetch_counter_dispatches_on_chain() {
    let evm = rpc_server(json!("0x10")).await;
    let sol = MockServer::start().await;
    let wallet = EncryptedWallet {
        address: "0xabc".into(),
        chain: Chain::Evm,
        ciphertext_b64: "x".into(),
        iv_b64: "x".into(),
        mac_b64: "x".into(),
        name: "evm-1".into(),
        salt_b64: "x".into(),
    };
    let n = fetch_counter_for_wallet(
        &reqwest::Client::new(),
        &wallet,
        &settings(&evm.uri(), &sol.uri()),
    )
    .await
    .unwrap();
    assert_eq!(n, 16);
    assert_eq!(evm.received_requests().await.unwrap().len(), 1);
    assert!(sol.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Snapshot orchestration + full run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_records_first_run_but_reports_no_active() {
    let store = store_with(&[("evm-1", Chain::Evm)]);
    let server = rpc_server(json!("0x05")).await;
    let mut activity = ActivityStore::default();
    let active = snapshot_all_wallets(
        &reqwest::Client::new(),
        &settings(&server.uri(), "http://unused"),
        &store,
        &mut activity,
        "2026-W22",
        w22(),
    )
    .await;
    assert!(active.is_empty());
    let latest = activity.latest("evm-1").unwrap();
    assert_eq!(latest.counter, 5);
    assert_eq!(latest.captured_at_iso, "2026-05-28T09:00:00.000000+00:00");
}

#[tokio::test]
async fn snapshot_detects_active_wallet_on_second_week() {
    let store = store_with(&[("evm-1", Chain::Evm)]);
    let mut activity = ActivityStore::default();
    activity.record(snap("evm-1", 5, "2026-W21", Chain::Evm));
    let server = rpc_server(json!("0x07")).await;
    let active = snapshot_all_wallets(
        &reqwest::Client::new(),
        &settings(&server.uri(), "http://unused"),
        &store,
        &mut activity,
        "2026-W22",
        w22(),
    )
    .await;
    let names: Vec<_> = active.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["evm-1"]);
}

#[tokio::test]
async fn snapshot_skips_wallets_whose_rpc_fails() {
    let store = store_with(&[("evm-1", Chain::Evm)]);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let mut activity = ActivityStore::default();
    let active = snapshot_all_wallets(
        &reqwest::Client::new(),
        &settings(&server.uri(), "http://unused"),
        &store,
        &mut activity,
        "2026-W22",
        w22(),
    )
    .await;
    assert!(active.is_empty());
    assert!(activity.snapshots.is_empty());
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: std::path::PathBuf,
    state: std::path::PathBuf,
    activity: std::path::PathBuf,
}

fn seeded_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture {
        store: dir.path().join("wallets.json"),
        state: dir.path().join("state.json"),
        activity: dir.path().join("activity.json"),
        _dir: dir,
    };
    save_store(&store_with(&[("evm-1", Chain::Evm)]), &f.store).unwrap();
    let mut activity = ActivityStore::default();
    activity.record(snap("evm-1", 10, "2026-W21", Chain::Evm));
    save_activity_store(&activity, &f.activity).unwrap();
    f
}

#[tokio::test]
async fn run_reminder_end_to_end_without_telegram() {
    let f = seeded_fixture();
    let rpc = rpc_server(json!("0x14")).await;
    let result = run_reminder(
        &reqwest::Client::new(),
        &settings(&rpc.uri(), "http://unused"),
        &ReminderPaths {
            store: &f.store,
            state: &f.state,
            activity: &f.activity,
        },
        w22(),
    )
    .await
    .unwrap();

    assert_eq!(result.iso_week, "2026-W22");
    assert_eq!(result.active_wallets, ["evm-1"]);
    assert!(!result.auto_marked.is_empty());
    assert!(result.auto_marked.iter().all(|k| k.starts_with("evm-1::")));
    assert!(!result.telegram_pushed);
    // Only the wallet-less cex programs remain pending.
    assert!(result.pending_rows > 0);
    assert!(result.digest.contains("(Binance account)"));
    assert!(!result.digest.contains("<b>evm-1</b>"));

    let state = load_state(&f.state).unwrap();
    assert_eq!(state.completions.len(), result.auto_marked.len());
    let reloaded = load_activity_store(&f.activity).unwrap();
    let latest = reloaded.latest("evm-1").unwrap();
    assert_eq!(
        (latest.counter, latest.iso_week.as_str()),
        (0x14, "2026-W22")
    );
}

#[tokio::test]
async fn run_reminder_pushes_digest_to_telegram() {
    let f = seeded_fixture();
    let rpc = rpc_server(json!("0x0a")).await; // unchanged counter → nothing auto-marked
    let tg_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTOKEN/sendMessage"))
        .and(body_partial_json(
            json!({"chat_id": "42", "parse_mode": "HTML", "disable_web_page_preview": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&tg_server)
        .await;
    let cooldown = f._dir.path().join("tg-cooldown.json");
    let mut s = settings(&rpc.uri(), "http://unused");
    s.telegram = Some(
        Telegram::new("TOKEN", "42")
            .with_api_base(tg_server.uri())
            .with_cooldown_path(cooldown),
    );

    let result = run_reminder(
        &reqwest::Client::new(),
        &s,
        &ReminderPaths {
            store: &f.store,
            state: &f.state,
            activity: &f.activity,
        },
        w22(),
    )
    .await
    .unwrap();
    assert!(result.telegram_pushed);
    assert!(result.active_wallets.is_empty());
    assert!(result.digest.contains("<b>evm-1</b> (evm)"));
    let sent: serde_json::Value = tg_server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(sent["text"], result.digest);
}

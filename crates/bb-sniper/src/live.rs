//! Live signed-swap executor building blocks.
//!
//! Every state-mutating path goes through the preflight gate ladder; nothing
//! fires until *all* gates pass:
//!
//! * `SNIPER_LIVE_ENABLED=true` (settings `enabled`)
//! * `CONFIRM_LIVE_SNIPER=yes` (settings `confirm_live_sniper`)
//! * `SNIPER_PROVEN_EDGE=yes` (environment)
//! * wallet balance ≥ `min_wallet_sol_atoms`
//! * no emergency stop file
//! * today's spend + this trade ≤ `max_daily_sol_spend_atoms`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_signer::Signer as _;
use solana_transaction::versioned::VersionedTransaction;

use crate::error::{SniperError, truncate_chars};
use crate::jupiter::{DEFAULT_QUOTE_URL, QuoteProvider};

pub const DEFAULT_SOLANA_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
pub const DEFAULT_SWAP_URL: &str = "https://lite-api.jup.ag/swap/v1/swap";
pub const PROVEN_EDGE_ENV: &str = "SNIPER_PROVEN_EDGE";

const BALANCE_TIMEOUT: Duration = Duration::from_secs(15);
const SWAP_TIMEOUT: Duration = Duration::from_secs(20);
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

fn live_err(msg: impl Into<String>) -> SniperError {
    SniperError::Live(msg.into())
}

/// Runtime knobs + safety caps for the live executor.
#[derive(Debug, Clone, PartialEq)]
pub struct SniperLiveSettings {
    pub wallet_path: PathBuf,
    pub rpc_url: String,
    pub quote_url: String,
    pub swap_url: String,

    /// 0.005 SOL per snipe.
    pub entry_sol_atoms: u64,
    pub hold_seconds: u64,
    pub slippage_bps: u32,
    pub max_concurrent: usize,
    /// 0.1 SOL/day hard cap.
    pub max_daily_sol_spend_atoms: u64,
    /// Need ≥ 0.05 SOL to operate.
    pub min_wallet_sol_atoms: u64,
    pub confirmation_timeout: Duration,
    pub rpc_poll_interval: Duration,

    pub enabled: bool,
    pub confirm_live_sniper: String,
    pub emergency_stop_path: PathBuf,
    pub ledger_path: PathBuf,
    pub state_path: PathBuf,
}

impl Default for SniperLiveSettings {
    fn default() -> Self {
        Self {
            wallet_path: "data/sniper-wallet.json".into(),
            rpc_url: DEFAULT_SOLANA_RPC_URL.into(),
            quote_url: DEFAULT_QUOTE_URL.into(),
            swap_url: DEFAULT_SWAP_URL.into(),
            entry_sol_atoms: 5_000_000,
            hold_seconds: 300,
            slippage_bps: 500,
            max_concurrent: 3,
            max_daily_sol_spend_atoms: 100_000_000,
            min_wallet_sol_atoms: 50_000_000,
            confirmation_timeout: Duration::from_secs(60),
            rpc_poll_interval: Duration::from_secs(2),
            enabled: false,
            confirm_live_sniper: "no".into(),
            emergency_stop_path: "data/sniper-live.stop".into(),
            ledger_path: "data/sniper-live-ledger.jsonl".into(),
            state_path: "data/sniper-live-state.json".into(),
        }
    }
}

/// Outcome of a single signed-swap submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwapResult {
    pub signature: String,
    pub pre_balance_lamports: u64,
    pub post_balance_lamports: u64,
    pub confirmed: bool,
    pub error: String,
}

/// Wall-clock source, injectable so day rollovers are testable.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        bb_core::time::now()
    }
}

/// Persisted spend state (`data/sniper-live-state.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendState {
    pub date_utc: String,
    pub spent_sol_atoms: u64,
}

/// Total SOL atoms spent on the current UTC day.
#[derive(Clone)]
pub struct DailySpendTracker {
    state: SpendState,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for DailySpendTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DailySpendTracker")
            .field("state", &self.state)
            .finish()
    }
}

impl DailySpendTracker {
    pub fn new(date_utc: impl Into<String>, spent_sol_atoms: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            state: SpendState {
                date_utc: date_utc.into(),
                spent_sol_atoms,
            },
            clock,
        }
    }

    /// Zero spend dated today.
    pub fn empty(clock: Arc<dyn Clock>) -> Self {
        let today = bb_core::time::utc_date(clock.now());
        Self::new(today, 0, clock)
    }

    pub fn date_utc(&self) -> &str {
        &self.state.date_utc
    }

    pub fn spent_sol_atoms(&self) -> u64 {
        self.state.spent_sol_atoms
    }

    pub fn state(&self) -> &SpendState {
        &self.state
    }

    pub fn reset_if_new_day(&mut self) {
        let today = bb_core::time::utc_date(self.clock.now());
        if today != self.state.date_utc {
            self.state = SpendState {
                date_utc: today,
                spent_sol_atoms: 0,
            };
        }
    }

    pub fn add(&mut self, atoms: u64) {
        self.reset_if_new_day();
        self.state.spent_sol_atoms = self.state.spent_sol_atoms.saturating_add(atoms);
    }

    pub fn would_exceed(&mut self, atoms: u64, cap: u64) -> bool {
        self.reset_if_new_day();
        self.state.spent_sol_atoms.saturating_add(atoms) > cap
    }
}

fn ensure_no_emergency_stop(path: &Path) -> Result<(), SniperError> {
    if bb_core::fsutil::stop_requested(path) {
        return Err(live_err(format!(
            "emergency stop file present at {}; refusing to run",
            path.display()
        )));
    }
    Ok(())
}

/// Gate ladder with the `SNIPER_PROVEN_EDGE` value supplied by the caller.
pub fn check_preflight(
    settings: &SniperLiveSettings,
    proven_edge: Option<&str>,
) -> Result<(), SniperError> {
    ensure_no_emergency_stop(&settings.emergency_stop_path)?;
    if !settings.enabled {
        return Err(live_err(
            "SNIPER_LIVE_ENABLED=true is required for live submission",
        ));
    }
    if settings.confirm_live_sniper != "yes" {
        return Err(live_err(
            "CONFIRM_LIVE_SNIPER=yes is required for live submission",
        ));
    }
    if !proven_edge.is_some_and(|v| v.trim().eq_ignore_ascii_case("yes")) {
        return Err(live_err(
            "SNIPER_PROVEN_EDGE=yes is required for live submission. \
             Review the paper ledger first; set this only when paper win rate × avg win \
             exceeds avg loss × (1 - win rate).",
        ));
    }
    Ok(())
}

/// Gate ladder reading `SNIPER_PROVEN_EDGE` from the environment.
pub fn preflight_settings(settings: &SniperLiveSettings) -> Result<(), SniperError> {
    check_preflight(settings, std::env::var(PROVEN_EDGE_ENV).ok().as_deref())
}

async fn rpc_post(
    client: &reqwest::Client,
    rpc_url: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<(reqwest::StatusCode, reqwest::Response), SniperError> {
    let resp = client
        .post(rpc_url)
        .json(payload)
        .timeout(timeout)
        .send()
        .await?;
    Ok((resp.status(), resp))
}

/// SOL balance (lamports) of one pubkey.
pub async fn fetch_lamport_balance(
    client: &reqwest::Client,
    rpc_url: &str,
    pubkey: &str,
) -> Result<u64, SniperError> {
    let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "getBalance", "params": [pubkey]});
    let (status, resp) = rpc_post(client, rpc_url, &payload, BALANCE_TIMEOUT).await?;
    if status != reqwest::StatusCode::OK {
        return Err(live_err(format!("getBalance HTTP {}", status.as_u16())));
    }
    let body: Value = resp.json().await?;
    if let Some(err) = body.get("error").filter(|e| !e.is_null()) {
        return Err(live_err(format!("getBalance RPC error: {err}")));
    }
    match body.get("result").and_then(|r| r.get("value")) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| live_err(format!("getBalance returned malformed value: {n}"))),
        Some(other) => Err(live_err(format!(
            "getBalance returned malformed value: {other}"
        ))),
    }
}

/// First `getSignatureStatuses` row, or `None` when unknown / HTTP failure.
pub async fn fetch_signature_status(
    client: &reqwest::Client,
    rpc_url: &str,
    signature: &str,
) -> Result<Option<Value>, SniperError> {
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "getSignatureStatuses",
        "params": [[signature], {"searchTransactionHistory": false}],
    });
    let (status, resp) = rpc_post(client, rpc_url, &payload, BALANCE_TIMEOUT).await?;
    if status != reqwest::StatusCode::OK {
        return Ok(None);
    }
    let mut body: Value = resp.json().await?;
    let row = body
        .pointer_mut("/result/value/0")
        .map(Value::take)
        .filter(|row| !row.is_null());
    Ok(row)
}

/// POST a quote to Jupiter `/swap`; returns the base64 unsigned transaction.
pub async fn fetch_swap_transaction(
    client: &reqwest::Client,
    swap_url: &str,
    quote: &Value,
    user_pubkey: &str,
) -> Result<String, SniperError> {
    let body = json!({
        "quoteResponse": quote,
        "userPublicKey": user_pubkey,
        "wrapAndUnwrapSol": true,
        "dynamicComputeUnitLimit": true,
        "prioritizationFeeLamports": "auto",
    });
    let resp = client
        .post(swap_url)
        .json(&body)
        .timeout(SWAP_TIMEOUT)
        .send()
        .await?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        let preview = resp.text().await.unwrap_or_default();
        return Err(live_err(format!(
            "Jupiter swap HTTP {}: {}",
            status.as_u16(),
            truncate_chars(&preview, 200)
        )));
    }
    let payload: Value = resp.json().await?;
    match payload.get("swapTransaction") {
        Some(Value::String(tx)) if !tx.is_empty() => Ok(tx.clone()),
        _ => Err(live_err(format!(
            "swap response missing swapTransaction: {payload}"
        ))),
    }
}

/// Re-sign Jupiter's unsigned `VersionedTransaction` with `keypair`; returns wire bytes.
pub fn sign_swap_transaction(swap_tx_b64: &str, keypair: &Keypair) -> Result<Vec<u8>, SniperError> {
    let raw = B64
        .decode(swap_tx_b64.trim())
        .map_err(|e| live_err(format!("swap transaction is not base64: {e}")))?;
    let tx: VersionedTransaction = wincode::deserialize(&raw)
        .map_err(|e| live_err(format!("swap transaction does not decode: {e}")))?;
    let signed = VersionedTransaction::try_new(tx.message, &[keypair])
        .map_err(|e| live_err(format!("signing swap transaction failed: {e}")))?;
    wincode::serialize(&signed)
        .map_err(|e| live_err(format!("serialising signed transaction failed: {e}")))
}

/// `sendTransaction`; returns the base58 signature.
pub async fn submit_signed_transaction(
    client: &reqwest::Client,
    rpc_url: &str,
    signed_tx: &[u8],
) -> Result<String, SniperError> {
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "sendTransaction",
        "params": [B64.encode(signed_tx), {"encoding": "base64", "skipPreflight": false,
                                           "preflightCommitment": "processed"}],
    });
    let (status, resp) = rpc_post(client, rpc_url, &payload, SEND_TIMEOUT).await?;
    if status != reqwest::StatusCode::OK {
        return Err(live_err(format!(
            "sendTransaction HTTP {}",
            status.as_u16()
        )));
    }
    let body: Value = resp.json().await?;
    if let Some(err) = body.get("error").filter(|e| !e.is_null()) {
        return Err(live_err(format!("sendTransaction RPC error: {err}")));
    }
    match body.get("result") {
        Some(Value::String(sig)) if !sig.is_empty() => Ok(sig.clone()),
        _ => Err(live_err("sendTransaction returned no signature")),
    }
}

/// Parameters of one signed swap.
#[derive(Debug, Clone, Copy)]
pub struct SwapRequest<'a> {
    pub input_mint: &'a str,
    pub output_mint: &'a str,
    pub amount_atoms: u64,
    pub slippage_bps: u32,
}

/// Endpoints + polling cadence for signed swaps.
#[derive(Debug, Clone, Copy)]
pub struct SwapEndpoints<'a> {
    pub rpc_url: &'a str,
    pub swap_url: &'a str,
    pub confirmation_timeout: Duration,
    pub rpc_poll_interval: Duration,
}

/// End-to-end: quote → build swap tx → sign → submit → wait for confirmation.
///
/// MOVES REAL FUNDS. Callers must have passed [`load_runtime`]'s gate ladder,
/// [`LiveExecutorRuntime::assert_spend_allowed`] and
/// [`LiveExecutorRuntime::assert_balance_ok`] first.
pub async fn execute_signed_swap<Q: QuoteProvider>(
    client: &reqwest::Client,
    quotes: &Q,
    keypair: &Keypair,
    request: SwapRequest<'_>,
    endpoints: SwapEndpoints<'_>,
) -> Result<SwapResult, SniperError> {
    let pubkey = keypair.pubkey().to_string();
    let pre_balance = fetch_lamport_balance(client, endpoints.rpc_url, &pubkey).await?;
    let quote = quotes
        .get_quote(
            request.input_mint,
            request.output_mint,
            request.amount_atoms,
            request.slippage_bps,
        )
        .await?;
    let swap_tx = fetch_swap_transaction(client, endpoints.swap_url, &quote.raw, &pubkey).await?;
    let signed = sign_swap_transaction(&swap_tx, keypair)?;
    let signature = submit_signed_transaction(client, endpoints.rpc_url, &signed).await?;

    let deadline = tokio::time::Instant::now() + endpoints.confirmation_timeout;
    let mut confirmed = false;
    let mut last_error = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Some(status) = fetch_signature_status(client, endpoints.rpc_url, &signature).await? {
            if let Some(err) = status.get("err").filter(|e| !e.is_null()) {
                last_error = truncate_chars(&err.to_string(), 200);
                break;
            }
            let confirmation = status
                .get("confirmationStatus")
                .and_then(Value::as_str)
                .unwrap_or("");
            if matches!(confirmation, "confirmed" | "finalized") {
                confirmed = true;
                break;
            }
        }
        tokio::time::sleep(endpoints.rpc_poll_interval).await;
    }

    let post_balance = fetch_lamport_balance(client, endpoints.rpc_url, &pubkey).await?;
    Ok(SwapResult {
        signature,
        pre_balance_lamports: pre_balance,
        post_balance_lamports: post_balance,
        confirmed,
        error: last_error,
    })
}

/// Wallet + spend tracker; kept apart from immutable settings so the daily
/// cap holds across cycles.
#[derive(Debug)]
pub struct LiveExecutorRuntime {
    pub settings: SniperLiveSettings,
    pub keypair: Keypair,
    pub spend: DailySpendTracker,
}

impl LiveExecutorRuntime {
    pub fn assert_spend_allowed(&mut self, amount_atoms: u64) -> Result<(), SniperError> {
        let cap = self.settings.max_daily_sol_spend_atoms;
        if self.spend.would_exceed(amount_atoms, cap) {
            return Err(live_err(format!(
                "daily SOL spend cap reached: {} > {cap}",
                self.spend.spent_sol_atoms().saturating_add(amount_atoms)
            )));
        }
        Ok(())
    }

    pub async fn assert_balance_ok(&self, client: &reqwest::Client) -> Result<u64, SniperError> {
        let pubkey = self.keypair.pubkey().to_string();
        let balance = fetch_lamport_balance(client, &self.settings.rpc_url, &pubkey).await?;
        if balance < self.settings.min_wallet_sol_atoms {
            return Err(live_err(format!(
                "wallet balance {balance} lamports below minimum {}",
                self.settings.min_wallet_sol_atoms
            )));
        }
        Ok(balance)
    }

    pub fn record_spend(&mut self, atoms: u64) {
        self.spend.add(atoms);
    }

    /// Atomically persist the spend tracker to `settings.state_path`.
    pub fn save_spend_tracker(&self) -> Result<(), SniperError> {
        let path = &self.settings.state_path;
        bb_core::fsutil::write_json_atomic(path, self.spend.state())
            .map_err(|e| SniperError::io(path, e))
    }
}

/// Best-effort load of the persisted tracker; unreadable state starts fresh.
fn load_spend_tracker(path: &Path, clock: Arc<dyn Clock>) -> DailySpendTracker {
    let mut spend = DailySpendTracker::empty(clock.clone());
    let Ok(Some(data)) = bb_core::fsutil::read_json::<Value>(path) else {
        return spend;
    };
    let date = match data.get("date_utc") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => spend.date_utc().to_owned(),
    };
    let spent = match data.get("spent_sol_atoms") {
        None | Some(Value::Null) => Some(0),
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        Some(_) => None,
    };
    if let Some(spent) = spent {
        spend = DailySpendTracker::new(date, spent, clock);
        spend.reset_if_new_day();
    }
    spend
}

/// Gate ladder (with `SNIPER_PROVEN_EDGE` value supplied), wallet decrypt,
/// persisted spend tracker.
pub fn load_runtime_with(
    settings: SniperLiveSettings,
    proven_edge: Option<&str>,
    passphrase: &str,
    clock: Arc<dyn Clock>,
) -> Result<LiveExecutorRuntime, SniperError> {
    check_preflight(&settings, proven_edge)?;
    let keypair = crate::wallet::load_keypair_with_passphrase(&settings.wallet_path, passphrase)?;
    let spend = load_spend_tracker(&settings.state_path, clock);
    Ok(LiveExecutorRuntime {
        settings,
        keypair,
        spend,
    })
}

/// Production entry: env-sourced gates, passphrase from argument /
/// `SNIPER_WALLET_PASSPHRASE` / TTY prompt, system clock.
pub fn load_runtime(
    settings: SniperLiveSettings,
    passphrase: Option<&str>,
) -> Result<LiveExecutorRuntime, SniperError> {
    preflight_settings(&settings)?;
    let keypair = crate::wallet::load_keypair(&settings.wallet_path, passphrase)?;
    let spend = load_spend_tracker(&settings.state_path, Arc::new(SystemClock));
    Ok(LiveExecutorRuntime {
        settings,
        keypair,
        spend,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use parking_lot::Mutex;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use crate::jupiter::JupiterQuote;

    struct FixedClock(Mutex<DateTime<Utc>>);

    impl FixedClock {
        fn at(y: i32, m: u32, d: u32) -> Arc<Self> {
            Arc::new(Self(Mutex::new(
                Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap(),
            )))
        }
        fn advance_days(&self, days: i64) {
            let mut now = self.0.lock();
            *now += chrono::Duration::days(days);
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock()
        }
    }

    fn enabled_settings(dir: &Path) -> SniperLiveSettings {
        SniperLiveSettings {
            wallet_path: dir.join("wallet.json"),
            enabled: true,
            confirm_live_sniper: "yes".into(),
            emergency_stop_path: dir.join("stop"),
            ledger_path: dir.join("ledger.jsonl"),
            state_path: dir.join("state.json"),
            ..SniperLiveSettings::default()
        }
    }

    fn runtime(dir: &Path, clock: Arc<dyn Clock>) -> LiveExecutorRuntime {
        let s = enabled_settings(dir);
        crate::wallet::generate_wallet(&s.wallet_path, "pp", false).unwrap();
        load_runtime_with(s, Some("yes"), "pp", clock).unwrap()
    }

    fn err_text(r: Result<(), SniperError>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn preflight_gate_ladder() {
        let dir = tempfile::tempdir().unwrap();
        let base = enabled_settings(dir.path());
        let disabled = SniperLiveSettings {
            enabled: false,
            ..base.clone()
        };
        assert!(err_text(check_preflight(&disabled, Some("yes"))).contains("ENABLED"));
        let unconfirmed = SniperLiveSettings {
            confirm_live_sniper: "no".into(),
            ..base.clone()
        };
        assert!(
            err_text(check_preflight(&unconfirmed, Some("yes"))).contains("CONFIRM_LIVE_SNIPER")
        );
        assert!(err_text(check_preflight(&base, None)).contains("PROVEN_EDGE"));
        assert!(err_text(check_preflight(&base, Some("no"))).contains("PROVEN_EDGE"));
        check_preflight(&base, Some(" YES ")).unwrap();
        std::fs::write(&base.emergency_stop_path, "halt").unwrap();
        assert!(err_text(check_preflight(&base, Some("yes"))).contains("emergency stop"));
    }

    #[test]
    fn spend_tracker_blocks_above_cap() {
        let clock = FixedClock::at(2026, 10, 11);
        let mut s = DailySpendTracker::new("2026-10-11", 80_000_000, clock);
        assert!(s.would_exceed(30_000_000, 100_000_000));
        assert!(!s.would_exceed(20_000_000, 100_000_000));
    }

    #[test]
    fn spend_tracker_resets_on_new_day() {
        let clock = FixedClock::at(2026, 10, 11);
        let mut s = DailySpendTracker::new("1999-01-01", 99_999_999, clock.clone());
        s.reset_if_new_day();
        assert_eq!(s.date_utc(), "2026-10-11");
        assert_eq!(s.spent_sol_atoms(), 0);

        s.add(40_000_000);
        assert_eq!(s.spent_sol_atoms(), 40_000_000);
        clock.advance_days(1);
        assert!(!s.would_exceed(100_000_000, 100_000_000));
        assert_eq!(s.date_utc(), "2026-10-12");
        assert_eq!(s.spent_sol_atoms(), 0);
    }

    #[tokio::test]
    async fn fetch_lamport_balance_value_and_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "getBalance", "params": ["GOOD"]}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"value": 12345}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "getBalance", "params": ["BAD"]}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "bad"}}),
            ))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        assert_eq!(
            fetch_lamport_balance(&client, &server.uri(), "GOOD")
                .await
                .unwrap(),
            12345
        );
        let err = fetch_lamport_balance(&client, &server.uri(), "BAD")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("getBalance RPC error"));
    }

    #[tokio::test]
    async fn fetch_signature_status_row_or_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"params": [["s1"]]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"result": {"value": [{"confirmationStatus": "confirmed", "err": null}]}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"params": [["s2"]]})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"result": {"value": []}})),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let row = fetch_signature_status(&client, &server.uri(), "s1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row["confirmationStatus"], "confirmed");
        assert!(
            fetch_signature_status(&client, &server.uri(), "s2")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn fetch_swap_transaction_wraps_quote_and_validates_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"userPublicKey": "P", "quoteResponse": {"inputMint": "x"}, "wrapAndUnwrapSol": true})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"swapTransaction": "abc=="})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"oops": "no swapTransaction"})),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let tx = fetch_swap_transaction(&client, &server.uri(), &json!({"inputMint": "x"}), "P")
            .await
            .unwrap();
        assert_eq!(tx, "abc==");
        let err = fetch_swap_transaction(&client, &server.uri(), &json!({}), "Q")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("swapTransaction"));
    }

    #[tokio::test]
    async fn submit_signed_transaction_sends_base64() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": "5sigBase58"})))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let sig = submit_signed_transaction(&client, &server.uri(), &[0u8; 16])
            .await
            .unwrap();
        assert_eq!(sig, "5sigBase58");
        let requests: Vec<Request> = server.received_requests().await.unwrap();
        let body: Value = requests[0].body_json().unwrap();
        assert_eq!(body["method"], "sendTransaction");
        assert_eq!(body["params"][1]["encoding"], "base64");
        assert_eq!(
            B64.decode(body["params"][0].as_str().unwrap()).unwrap(),
            vec![0u8; 16]
        );
    }

    /// Legacy wire-format tx with one zeroed signature slot, fee payer = `payer`.
    fn unsigned_tx(payer: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
        let mut message = vec![1u8, 0, 1, 2];
        message.extend_from_slice(payer);
        message.extend_from_slice(&[7u8; 32]); // program id
        message.extend_from_slice(&[9u8; 32]); // recent blockhash
        message.extend_from_slice(&[1, 1, 1, 0, 2, 0xAA, 0xBB]); // 1 ix: prog idx 1, accts [0], data [AA BB]
        let mut tx = vec![1u8];
        tx.extend_from_slice(&[0u8; 64]);
        tx.extend_from_slice(&message);
        (tx, message)
    }

    #[test]
    fn sign_swap_transaction_produces_valid_signature() {
        let kp = Keypair::new();
        let (tx, message) = unsigned_tx(&kp.pubkey().to_bytes());
        let signed = sign_swap_transaction(&B64.encode(&tx), &kp).unwrap();
        assert_eq!(signed.len(), tx.len());
        assert_eq!(&signed[65..], &message[..]);
        let sig = solana_signature::Signature::try_from(&signed[1..65]).unwrap();
        assert!(sig.verify(&kp.pubkey().to_bytes(), &message));

        let other = Keypair::new();
        let err = sign_swap_transaction(&B64.encode(&tx), &other).unwrap_err();
        assert!(err.to_string().contains("signing swap transaction failed"));
        assert!(sign_swap_transaction("AAAA", &kp).is_err());
    }

    struct FixedQuote;

    impl QuoteProvider for FixedQuote {
        async fn get_quote(
            &self,
            input: &str,
            output: &str,
            amount: u64,
            _bps: u32,
        ) -> Result<JupiterQuote, SniperError> {
            Ok(JupiterQuote {
                input_mint: input.into(),
                output_mint: output.into(),
                in_amount: amount,
                out_amount: 42,
                price_impact_pct: 0.0,
                swap_usd_value: rust_decimal::Decimal::ONE,
                route_labels: Vec::new(),
                context_slot: 1,
                raw: json!({"inAmount": amount.to_string(), "outAmount": "42"}),
            })
        }
    }

    #[tokio::test]
    async fn execute_signed_swap_against_mocks() {
        let rpc = MockServer::start().await;
        let jup = MockServer::start().await;
        let kp = Keypair::new();
        let (tx, _) = unsigned_tx(&kp.pubkey().to_bytes());
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"quoteResponse": {"outAmount": "42"}}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"swapTransaction": B64.encode(&tx)})),
            )
            .mount(&jup)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "getBalance"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"result": {"value": 1_000_000}})),
            )
            .mount(&rpc)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "sendTransaction"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": "SIG"})))
            .mount(&rpc)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "getSignatureStatuses"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"result": {"value": [{"confirmationStatus": "finalized", "err": null}]}}),
            ))
            .mount(&rpc)
            .await;
        let client = reqwest::Client::new();
        let result = execute_signed_swap(
            &client,
            &FixedQuote,
            &kp,
            SwapRequest {
                input_mint: "A",
                output_mint: "B",
                amount_atoms: 5,
                slippage_bps: 500,
            },
            SwapEndpoints {
                rpc_url: &rpc.uri(),
                swap_url: &jup.uri(),
                confirmation_timeout: Duration::from_secs(5),
                rpc_poll_interval: Duration::from_millis(10),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            SwapResult {
                signature: "SIG".into(),
                pre_balance_lamports: 1_000_000,
                post_balance_lamports: 1_000_000,
                confirmed: true,
                error: String::new(),
            }
        );
    }

    #[test]
    fn load_runtime_blocks_when_preflight_fails() {
        let dir = tempfile::tempdir().unwrap();
        let s = SniperLiveSettings {
            enabled: false,
            ..enabled_settings(dir.path())
        };
        assert!(load_runtime_with(s, Some("yes"), "pp", Arc::new(SystemClock)).is_err());
    }

    #[test]
    fn load_runtime_restores_same_day_spend_and_resets_stale_day() {
        let dir = tempfile::tempdir().unwrap();
        let s = enabled_settings(dir.path());
        crate::wallet::generate_wallet(&s.wallet_path, "pp", false).unwrap();
        std::fs::write(
            &s.state_path,
            r#"{"date_utc": "2026-05-31", "spent_sol_atoms": 12345}"#,
        )
        .unwrap();

        let rt =
            load_runtime_with(s.clone(), Some("yes"), "pp", FixedClock::at(2026, 5, 31)).unwrap();
        assert_eq!(rt.spend.spent_sol_atoms(), 12345);
        let rt = load_runtime_with(s, Some("yes"), "pp", FixedClock::at(2026, 6, 1)).unwrap();
        assert_eq!(rt.spend.spent_sol_atoms(), 0);
        assert_eq!(rt.spend.date_utc(), "2026-06-01");
    }

    #[test]
    fn runtime_assert_spend_blocks_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = runtime(dir.path(), FixedClock::at(2026, 10, 11));
        rt.record_spend(rt.settings.max_daily_sol_spend_atoms);
        let err = rt.assert_spend_allowed(1).unwrap_err();
        assert!(err.to_string().contains("daily SOL spend cap"));
    }

    #[tokio::test]
    async fn runtime_assert_balance_ok_floor() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(SystemClock));
        let client = reqwest::Client::new();

        let low = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"result": {"value": 1_000}})),
            )
            .mount(&low)
            .await;
        let rt_low = LiveExecutorRuntime {
            settings: SniperLiveSettings {
                rpc_url: low.uri(),
                ..rt.settings.clone()
            },
            keypair: rt.keypair.insecure_clone(),
            spend: rt.spend.clone(),
        };
        let err = rt_low.assert_balance_ok(&client).await.unwrap_err();
        assert!(err.to_string().contains("below minimum"));

        let plenty = rt.settings.min_wallet_sol_atoms * 10;
        let high = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"result": {"value": plenty}})),
            )
            .mount(&high)
            .await;
        let rt_high = LiveExecutorRuntime {
            settings: SniperLiveSettings {
                rpc_url: high.uri(),
                ..rt.settings.clone()
            },
            ..rt
        };
        assert_eq!(rt_high.assert_balance_ok(&client).await.unwrap(), plenty);
    }

    #[test]
    fn save_spend_tracker_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = runtime(dir.path(), FixedClock::at(2026, 10, 11));
        rt.record_spend(99_999);
        rt.save_spend_tracker().unwrap();
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(&rt.settings.state_path).unwrap())
                .unwrap();
        assert_eq!(
            persisted,
            json!({"date_utc": "2026-10-11", "spent_sol_atoms": 99_999})
        );
    }
}

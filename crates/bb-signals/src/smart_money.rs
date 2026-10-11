//! Solana smart-money tracker: polls tracked wallets' signatures through Helius
//! RPC and emits a candidate for every non-stable SPL token the wallet received
//! (and did not also send) in a transaction.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use bb_core::{Decimal, env};
use serde_json::{Value, json};

use crate::alerts::Alerter;
use crate::candidate::SignalCandidate;
use crate::common::{
    AlertRateLimiter, Clock, append_ledger, atomic_write_json, emergency_stop_active,
    read_json_lenient, secs,
};
use crate::error::SignalsError;
use crate::jsonv::{Object, int_or_none, number, py_str, str_or_empty};
use crate::wallet_bootstrap::{BirdeyeSource, refresh_wallets_if_stale};
use crate::watchlist::Watchlist;

pub const HELIUS_RPC_URL: &str = "https://mainnet.helius-rpc.com/";
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCo1cCfZwpz35K7RV8";
const STABLE_OR_SOL_MINTS: [&str; 3] = [WSOL_MINT, USDC_MINT, USDT_MINT];
const SIGNATURE_LIMIT: u32 = 25;

#[derive(Debug, Clone, PartialEq)]
pub struct SmartMoneySettings {
    pub helius_api_key: String,
    pub poll_interval_seconds: f64,
    pub wallet_list_path: PathBuf,
    pub state_path: PathBuf,
    pub max_alerts_per_hour: usize,
    pub stop_path: PathBuf,
    pub refresh_hours: f64,
    pub birdeye_api_key: Option<String>,
    pub ledger_path: PathBuf,
    pub watchlist_path: PathBuf,
    pub watchlist_ttl_hours: f64,
}

impl Default for SmartMoneySettings {
    fn default() -> Self {
        Self {
            helius_api_key: String::new(),
            poll_interval_seconds: 60.0,
            wallet_list_path: "data/smart-money-wallets.json".into(),
            state_path: "data/smart-money-state.json".into(),
            max_alerts_per_hour: 15,
            stop_path: "data/signals-smart-money.stop".into(),
            refresh_hours: 168.0,
            birdeye_api_key: None,
            ledger_path: "data/signals-ledger.jsonl".into(),
            watchlist_path: "data/signals-watchlist.json".into(),
            watchlist_ttl_hours: 12.0,
        }
    }
}

impl SmartMoneySettings {
    pub fn from_env() -> Result<Self, SignalsError> {
        let d = Self::default();
        Ok(Self {
            helius_api_key: env::string(&["HELIUS_API_KEY"], ""),
            poll_interval_seconds: env::float(
                &["SIGNALS_SMART_MONEY_POLL_INTERVAL_SECONDS"],
                d.poll_interval_seconds,
            )?,
            wallet_list_path: env::string(
                &["SIGNALS_SMART_MONEY_WALLET_LIST_PATH"],
                "data/smart-money-wallets.json",
            )
            .into(),
            state_path: env::string(
                &["SIGNALS_SMART_MONEY_STATE_PATH"],
                "data/smart-money-state.json",
            )
            .into(),
            max_alerts_per_hour: env::integer(
                &["SIGNALS_SMART_MONEY_MAX_ALERTS_PER_HOUR"],
                d.max_alerts_per_hour,
            )?,
            stop_path: env::string(
                &["SIGNALS_SMART_MONEY_STOP_PATH"],
                "data/signals-smart-money.stop",
            )
            .into(),
            refresh_hours: env::float(&["SIGNALS_SMART_MONEY_REFRESH_HOURS"], d.refresh_hours)?,
            birdeye_api_key: env::optional_string(&["BIRDEYE_API_KEY"]),
            ledger_path: env::string(&["SIGNALS_LEDGER_PATH"], "data/signals-ledger.jsonl").into(),
            watchlist_path: env::string(&["SIGNALS_WATCHLIST_PATH"], "data/signals-watchlist.json")
                .into(),
            watchlist_ttl_hours: env::float(
                &["SIGNALS_WATCHLIST_TTL_HOURS"],
                d.watchlist_ttl_hours,
            )?,
        })
    }

    /// The tracker cannot run without a Helius key.
    pub fn validate(&self) -> Result<(), SignalsError> {
        if self.helius_api_key.is_empty() {
            return Err(SignalsError::Malformed(
                "HELIUS_API_KEY is required for the smart-money tracker. Create a free-tier key at helius.dev and set HELIUS_API_KEY.".into(),
            ));
        }
        Ok(())
    }
}

/// A token the wallet received in one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletBuy {
    pub mint: String,
    pub amount_atoms: String,
}

/// Solana JSON-RPC over Helius (`?api-key=` is redacted from errors).
#[derive(Debug, Clone)]
pub struct HeliusRpc {
    http: reqwest::Client,
    url: String,
}

impl HeliusRpc {
    pub fn new(http: reqwest::Client, api_key: &str) -> Self {
        Self::with_base_url(http, HELIUS_RPC_URL, api_key)
    }

    pub fn with_base_url(http: reqwest::Client, base_url: &str, api_key: &str) -> Self {
        Self {
            http,
            url: format!("{base_url}?api-key={api_key}"),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, SignalsError> {
        let payload = json!({"jsonrpc": "2.0", "id": method, "method": method, "params": params});
        let data: Value = self
            .http
            .post(&self.url)
            .json(&payload)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| SignalsError::http(&self.url, e))?
            .json()
            .await
            .map_err(|e| SignalsError::http(&self.url, e))?;
        let Value::Object(mut data) = data else {
            return Err(SignalsError::Malformed(format!(
                "Helius {method} response was not an object"
            )));
        };
        match data.get("error") {
            None | Some(Value::Null) => {}
            Some(err) if is_falsy(err) => {}
            Some(err) => {
                return Err(SignalsError::Rpc {
                    method: format!("Helius {method}"),
                    error: err.to_string(),
                });
            }
        }
        Ok(data.remove("result").unwrap_or(Value::Null))
    }

    /// Newest-first signatures newer than `until` (at most 25).
    pub async fn wallet_signatures(
        &self,
        wallet_pubkey: &str,
        until: Option<&str>,
    ) -> Result<Vec<String>, SignalsError> {
        let mut config = json!({"limit": SIGNATURE_LIMIT});
        // `until` returns signatures NEWER than the cursor; `before` would walk
        // older history, the opposite of tracking new activity.
        if let Some(until) = until.filter(|u| !u.is_empty()) {
            config["until"] = Value::String(until.to_owned());
        }
        let result = self
            .call("getSignaturesForAddress", json!([wallet_pubkey, config]))
            .await?;
        let Value::Array(items) = result else {
            tracing::warn!(wallet = %wallet_pubkey, "Helius signatures response malformed");
            return Ok(Vec::new());
        };
        Ok(items
            .iter()
            .filter_map(|i| i.get("signature").and_then(Value::as_str))
            .map(str::to_owned)
            .collect())
    }

    /// `{"result": <jsonParsed transaction or null>}`.
    pub async fn transaction(&self, signature: &str) -> Result<Value, SignalsError> {
        let result = self
            .call(
                "getTransaction",
                json!([signature, {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0}]),
            )
            .await?;
        Ok(json!({ "result": result }))
    }
}

fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => !b,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Null => true,
    }
}

pub fn detect_buy_mint(transaction: &Value, wallet_pubkey: &str) -> Option<String> {
    detect_wallet_buys(transaction, wallet_pubkey)
        .into_iter()
        .next()
        .map(|b| b.mint)
}

/// Incoming non-stable SPL transfers into the wallet's token accounts, unless
/// the same transaction also sent that mint out or the wallet's balance of it
/// did not grow. Parsed SPL transfer instructions are a practical buy proxy
/// that avoids decoding every DEX's swap layout.
pub fn detect_wallet_buys(transaction: &Value, wallet_pubkey: &str) -> Vec<WalletBuy> {
    let Some(tx) = transaction.get("result").unwrap_or(transaction).as_object() else {
        return Vec::new();
    };
    let (own_accounts, account_mints) = wallet_token_accounts(tx, wallet_pubkey);
    if own_accounts.is_empty() {
        return Vec::new();
    }
    let positive = positive_wallet_mints(tx, wallet_pubkey);

    let mut incoming: Vec<(String, String)> = Vec::new();
    let mut outgoing: HashSet<String> = HashSet::new();
    for instruction in token_transfer_instructions(tx) {
        let info = instruction_info(instruction);
        let destination = str_or_empty(info.and_then(|i| i.get("destination")));
        let source = str_or_empty(info.and_then(|i| i.get("source")));
        let mint = [
            str_or_empty(info.and_then(|i| i.get("mint"))),
            account_mints.get(destination).map_or("", String::as_str),
            account_mints.get(source).map_or("", String::as_str),
        ]
        .into_iter()
        .find(|m| !m.is_empty())
        .unwrap_or("");
        if mint.is_empty() {
            continue;
        }
        let amount = info.map_or_else(|| "0".to_owned(), amount_atoms);
        if own_accounts.contains(destination) {
            match incoming.iter_mut().find(|(m, _)| m == mint) {
                Some((_, total)) => *total = sum_amounts(total, &amount),
                None => incoming.push((mint.to_owned(), sum_amounts("0", &amount))),
            }
        }
        if own_accounts.contains(source) {
            outgoing.insert(mint.to_owned());
        }
    }

    incoming
        .into_iter()
        .filter(|(mint, _)| {
            !STABLE_OR_SOL_MINTS.contains(&mint.as_str())
                && !outgoing.contains(mint)
                && (positive.is_empty() || positive.contains(mint))
        })
        .map(|(mint, amount_atoms)| WalletBuy { mint, amount_atoms })
        .collect()
}

fn token_balances<'a>(tx: &'a Object, key: &str) -> impl Iterator<Item = &'a Object> {
    tx.get("meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get(key))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

fn wallet_token_accounts(tx: &Object, wallet: &str) -> (HashSet<String>, HashMap<String, String>) {
    let keys = account_keys(tx);
    let mut own = HashSet::new();
    let mut mints = HashMap::new();
    for balance in
        token_balances(tx, "preTokenBalances").chain(token_balances(tx, "postTokenBalances"))
    {
        let account = account_for_index(&keys, balance.get("accountIndex"));
        let mint = str_or_empty(balance.get("mint"));
        if !account.is_empty() && !mint.is_empty() {
            mints.insert(account.to_owned(), mint.to_owned());
        }
        if !account.is_empty() && str_or_empty(balance.get("owner")) == wallet {
            own.insert(account.to_owned());
        }
    }
    (own, mints)
}

fn positive_wallet_mints(tx: &Object, wallet: &str) -> HashSet<String> {
    let pre = wallet_mint_amounts(tx, wallet, "preTokenBalances");
    let post = wallet_mint_amounts(tx, wallet, "postTokenBalances");
    pre.keys()
        .chain(post.keys())
        .filter(|mint| {
            post.get(*mint).copied().unwrap_or_default()
                > pre.get(*mint).copied().unwrap_or_default()
        })
        .cloned()
        .collect()
}

/// Token atoms are integers; `Decimal` keeps them exact.
fn wallet_mint_amounts(tx: &Object, wallet: &str, key: &str) -> BTreeMap<String, Decimal> {
    let mut amounts = BTreeMap::new();
    for balance in token_balances(tx, key) {
        if str_or_empty(balance.get("owner")) != wallet {
            continue;
        }
        let mint = str_or_empty(balance.get("mint"));
        if mint.is_empty() {
            continue;
        }
        let amount = balance
            .get("uiTokenAmount")
            .and_then(Value::as_object)
            .map_or(Decimal::ZERO, |ta| {
                let raw = ta
                    .get("amount")
                    .map_or_else(|| "0".to_owned(), |v| py_str(Some(v)));
                Decimal::from_str(raw.trim()).unwrap_or(Decimal::ZERO)
            });
        *amounts.entry(mint.to_owned()).or_insert(Decimal::ZERO) += amount;
    }
    amounts
}

fn message(tx: &Object) -> Option<&Object> {
    tx.get("transaction")?
        .as_object()?
        .get("message")?
        .as_object()
}

fn token_transfer_instructions(tx: &Object) -> Vec<&Object> {
    let root = message(tx)
        .and_then(|m| m.get("instructions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let inner = tx
        .get("meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("innerInstructions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("instructions").and_then(Value::as_array))
        .flatten();
    root.chain(inner)
        .filter_map(Value::as_object)
        .filter(|i| is_token_transfer(i))
        .collect()
}

fn is_token_transfer(instruction: &Object) -> bool {
    let Some(parsed) = instruction.get("parsed").and_then(Value::as_object) else {
        return false;
    };
    let is_spl = str_or_empty(instruction.get("program")) == "spl-token"
        || str_or_empty(instruction.get("programId")) == TOKEN_PROGRAM_ID;
    is_spl
        && matches!(
            str_or_empty(parsed.get("type")),
            "transfer" | "transferChecked"
        )
}

fn instruction_info(instruction: &Object) -> Option<&Object> {
    instruction
        .get("parsed")?
        .as_object()?
        .get("info")?
        .as_object()
}

fn amount_atoms(info: &Object) -> String {
    if let Some(amount) = info
        .get("tokenAmount")
        .and_then(|ta| ta.get("amount"))
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())
    {
        return amount.to_owned();
    }
    match info.get("amount") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => n.to_string(),
        _ => "0".to_owned(),
    }
}

/// Integer sum of two atom strings; a non-integer side yields `right`.
fn sum_amounts(left: &str, right: &str) -> String {
    match (left.trim().parse::<i128>(), right.trim().parse::<i128>()) {
        (Ok(l), Ok(r)) => l
            .checked_add(r)
            .map_or_else(|| right.to_owned(), |s| s.to_string()),
        _ => right.to_owned(),
    }
}

fn account_keys(tx: &Object) -> Vec<String> {
    message(tx)
        .and_then(|m| m.get("accountKeys"))
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .map(|k| match k {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => str_or_empty(o.get("pubkey")).to_owned(),
                    _ => String::new(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn account_for_index<'a>(keys: &'a [String], index: Option<&Value>) -> &'a str {
    int_or_none(index)
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| keys.get(i))
        .map_or("", String::as_str)
}

fn block_time_ms(transaction: &Value) -> Option<i64> {
    let result = transaction
        .get("result")
        .unwrap_or(transaction)
        .as_object()?;
    int_or_none(result.get("blockTime")).map(|t| t * 1000)
}

fn wallet_pubkey(wallet: &Object) -> String {
    str_or_empty(wallet.get("pubkey")).trim().to_owned()
}

fn wallet_weight(wallet: &Object) -> f64 {
    match wallet.get("weight") {
        None => 0.5,
        v => number(v).map_or(0.5, |w| w.clamp(0.0, 1.0)),
    }
}

/// Last processed signature per wallet (`data/smart-money-state.json`).
fn load_state(path: &std::path::Path) -> BTreeMap<String, String> {
    match read_json_lenient(path) {
        Some(Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| match v {
                Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

pub struct SmartMoneyTracker {
    settings: SmartMoneySettings,
    rpc: HeliusRpc,
    birdeye: BirdeyeSource,
    clock: Arc<dyn Clock>,
    alerter: Arc<dyn Alerter>,
    watchlist: Watchlist,
    limiter: AlertRateLimiter,
}

impl SmartMoneyTracker {
    pub fn new(
        settings: SmartMoneySettings,
        rpc: HeliusRpc,
        birdeye: BirdeyeSource,
        clock: Arc<dyn Clock>,
        alerter: Arc<dyn Alerter>,
    ) -> Self {
        let watchlist = Watchlist::load(&settings.watchlist_path);
        let limiter = AlertRateLimiter::new(settings.max_alerts_per_hour);
        Self {
            settings,
            rpc,
            birdeye,
            clock,
            alerter,
            watchlist,
            limiter,
        }
    }

    pub async fn run_cycle(&mut self) -> Result<Vec<SignalCandidate>, SignalsError> {
        let wallets = refresh_wallets_if_stale(
            &self.settings.wallet_list_path,
            &self.birdeye,
            self.settings.refresh_hours,
            self.clock.now_ms(),
        )
        .await?;
        let mut state = load_state(&self.settings.state_path);
        let mut candidates = Vec::new();
        if wallets.is_empty() {
            tracing::warn!("Smart-money wallet list is empty; sleeping until next cycle");
        }

        for wallet in &wallets {
            let pubkey = wallet_pubkey(wallet);
            if pubkey.is_empty() {
                continue;
            }
            let signatures = match self
                .rpc
                .wallet_signatures(&pubkey, state.get(&pubkey).map(String::as_str))
                .await
            {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(%err, wallet = %pubkey, "Helius signature fetch failed");
                    continue;
                }
            };
            if let Some(newest) = signatures.first() {
                state.insert(pubkey.clone(), newest.clone());
            }
            for signature in &signatures {
                let tx = match self.rpc.transaction(signature).await {
                    Ok(tx) => tx,
                    Err(err) => {
                        tracing::warn!(%err, %signature, "Helius transaction fetch failed");
                        continue;
                    }
                };
                let block_time = block_time_ms(&tx);
                for buy in detect_wallet_buys(&tx, &pubkey) {
                    let candidate = self.candidate(wallet, buy, signature, block_time);
                    append_ledger(&self.settings.ledger_path, &candidate)?;
                    self.watchlist
                        .add(&candidate, self.settings.watchlist_ttl_hours);
                    self.alert(&candidate).await;
                    candidates.push(candidate);
                }
            }
        }

        self.watchlist.prune(self.clock.now_ms());
        self.watchlist.save()?;
        atomic_write_json(&self.settings.state_path, &state)?;
        Ok(candidates)
    }

    pub async fn run(&mut self, once: bool) {
        loop {
            if emergency_stop_active(&self.settings.stop_path) {
                tracing::info!(path = %self.settings.stop_path.display(), "Smart-money stop file present");
                return;
            }
            if let Err(err) = self.run_cycle().await {
                tracing::warn!(%err, "Smart-money cycle failed");
            }
            if once {
                return;
            }
            tokio::time::sleep(secs(self.settings.poll_interval_seconds)).await;
        }
    }

    fn candidate(
        &self,
        wallet: &Object,
        buy: WalletBuy,
        signature: &str,
        block_time_ms: Option<i64>,
    ) -> SignalCandidate {
        let weight = wallet_weight(wallet);
        let raw = json!({
            "wallet_pubkey": wallet_pubkey(wallet),
            "wallet_label": py_str(wallet.get("label")),
            "wallet_weight": weight,
            "signature": signature,
            "block_time_ms": block_time_ms,
            "amount_atoms": buy.amount_atoms,
            "helius_tx_url": format!("https://xray.helius.xyz/tx/{signature}"),
        });
        SignalCandidate {
            source: "smart_money".into(),
            chain: "solana".into(),
            token_address: buy.mint,
            token_symbol: String::new(),
            detected_at_ms: self.clock.now_ms(),
            raw_signal: raw.as_object().cloned().unwrap_or_default(),
            confidence: weight,
        }
    }

    async fn alert(&mut self, candidate: &SignalCandidate) {
        if !self.limiter.allow(self.clock.now_ms()) {
            tracing::debug!(token = %candidate.token_address, "Smart-money alert suppressed by detector rate limit");
            return;
        }
        let msg = format!(
            "Smart-money signal\nWallet: {}\nToken: {}\nSignature: {}\nConfidence: {:.3}",
            py_str(candidate.raw_signal.get("wallet_label")),
            candidate.token_address,
            py_str(candidate.raw_signal.get("signature")),
            candidate.confidence
        );
        self.alerter.alert(&msg).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::testing::RecordingAlerter;
    use crate::common::FixedClock;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WALLET: &str = "Wallet111111111111111111111111111111111111";
    const ATA: &str = "Ata111111111111111111111111111111111111111";
    const MINT_A: &str = "MintA111111111111111111111111111111111111";
    const MINT_B: &str = "MintB111111111111111111111111111111111111";
    const OTHER_ATA: &str = "OtherAta111111111111111111111111111111111";

    fn transfer(source: &str, destination: &str, mint: &str, amount: &str) -> Value {
        json!({
            "program": "spl-token",
            "programId": TOKEN_PROGRAM_ID,
            "parsed": {"type": "transferChecked", "info": {
                "source": source, "destination": destination, "mint": mint,
                "tokenAmount": {"amount": amount},
            }},
        })
    }

    fn tx(mint: &str, outgoing_same_mint: bool, block_time: i64) -> Value {
        let mut instructions = vec![transfer(OTHER_ATA, ATA, mint, "100")];
        if outgoing_same_mint {
            instructions.push(transfer(ATA, OTHER_ATA, mint, "25"));
        }
        json!({
            "blockTime": block_time,
            "transaction": {"message": {
                "accountKeys": [{"pubkey": WALLET}, {"pubkey": ATA}, {"pubkey": OTHER_ATA}],
                "instructions": instructions,
            }},
            "meta": {
                "preTokenBalances": [{"accountIndex": 1, "mint": mint, "owner": WALLET, "uiTokenAmount": {"amount": "0"}}],
                "postTokenBalances": [{"accountIndex": 1, "mint": mint, "owner": WALLET, "uiTokenAmount": {"amount": "100"}}],
                "innerInstructions": [],
            },
        })
    }

    #[test]
    fn buy_detection_rules() {
        assert_eq!(
            detect_buy_mint(&tx(MINT_A, false, 1), WALLET).as_deref(),
            Some(MINT_A)
        );
        assert_eq!(detect_buy_mint(&tx(MINT_A, true, 1), WALLET), None);
        assert_eq!(detect_buy_mint(&tx(USDC_MINT, false, 1), WALLET), None);
        let buys = detect_wallet_buys(&json!({"result": tx(MINT_A, false, 1)}), WALLET);
        assert_eq!(
            buys,
            vec![WalletBuy {
                mint: MINT_A.into(),
                amount_atoms: "100".into()
            }]
        );
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        settings: SmartMoneySettings,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let settings = SmartMoneySettings {
            helius_api_key: "helius-key".into(),
            poll_interval_seconds: 0.0,
            wallet_list_path: p.join("wallets.json"),
            state_path: p.join("state.json"),
            stop_path: p.join("stop"),
            ledger_path: p.join("ledger.jsonl"),
            watchlist_path: p.join("watchlist.json"),
            ..SmartMoneySettings::default()
        };
        std::fs::write(
            &settings.wallet_list_path,
            json!({
                "wallets": [{"pubkey": WALLET, "label": "ansem-public", "source": "manual", "weight": 0.8}],
                "last_refreshed_ms": 2_000_000,
            })
            .to_string(),
        )
        .unwrap();
        Fixture {
            _dir: dir,
            settings,
        }
    }

    async fn rpc_server(signatures: &[&str], txs: &[(&str, Value)]) -> MockServer {
        let server = MockServer::start().await;
        let sigs: Vec<Value> = signatures.iter().map(|s| json!({"signature": s})).collect();
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "getSignaturesForAddress"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": sigs})))
            .mount(&server)
            .await;
        for (sig, body) in txs {
            Mock::given(method("POST"))
                .and(body_partial_json(
                    json!({"method": "getTransaction", "params": [sig]}),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": body})))
                .mount(&server)
                .await;
        }
        server
    }

    fn tracker(
        server: &MockServer,
        settings: SmartMoneySettings,
        alerter: Arc<RecordingAlerter>,
    ) -> SmartMoneyTracker {
        let http = reqwest::Client::new();
        SmartMoneyTracker::new(
            settings,
            HeliusRpc::with_base_url(http.clone(), &format!("{}/", server.uri()), "helius-key"),
            BirdeyeSource::new(http, None),
            Arc::new(FixedClock(2_000_000)),
            alerter,
        )
    }

    #[tokio::test]
    async fn full_cycle_emits_two_candidates_and_updates_state() {
        let f = fixture();
        let server = rpc_server(
            &["sig-new", "sig-old"],
            &[
                ("sig-new", tx(MINT_A, false, 111)),
                ("sig-old", tx(MINT_B, false, 110)),
            ],
        )
        .await;
        let alerter = Arc::new(RecordingAlerter::default());
        let mut t = tracker(&server, f.settings.clone(), alerter.clone());

        let candidates = t.run_cycle().await.unwrap();

        let tokens: Vec<&str> = candidates
            .iter()
            .map(|c| c.token_address.as_str())
            .collect();
        assert_eq!(tokens, vec![MINT_A, MINT_B]);
        let rows: Vec<Value> = std::fs::read_to_string(&f.settings.ledger_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["source"], "smart_money");
        assert_eq!(rows[0]["raw_signal"]["wallet_pubkey"], WALLET);
        assert_eq!(rows[0]["raw_signal"]["block_time_ms"], 111_000);
        assert!(
            rows[0]["raw_signal"]["helius_tx_url"]
                .as_str()
                .unwrap()
                .ends_with("/sig-new")
        );
        assert_eq!(
            read_json_lenient(&f.settings.state_path),
            Some(json!({WALLET: "sig-new"}))
        );
        assert_eq!(alerter.sent().len(), 2);
        assert!((candidates[0].confidence - 0.8).abs() < 1e-12);
    }

    #[tokio::test]
    async fn second_cycle_uses_until_cursor() {
        let f = fixture();
        let first = rpc_server(&["sig-new"], &[("sig-new", tx(MINT_A, false, 1))]).await;
        assert_eq!(
            tracker(&first, f.settings.clone(), Arc::default())
                .run_cycle()
                .await
                .unwrap()
                .len(),
            1
        );

        let second = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "getSignaturesForAddress", "params": [WALLET, {"until": "sig-new", "limit": 25}]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
            .expect(1)
            .mount(&second)
            .await;
        assert!(
            tracker(&second, f.settings.clone(), Arc::default())
                .run_cycle()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_helius_key_is_rejected() {
        let err = SmartMoneySettings::default().validate().unwrap_err();
        assert!(err.to_string().contains("HELIUS_API_KEY"));
    }

    #[tokio::test]
    async fn empty_wallet_list_cycle_returns_zero_without_raising() {
        let f = fixture();
        std::fs::write(
            &f.settings.wallet_list_path,
            r#"{"wallets": [], "last_refreshed_ms": 2000000}"#,
        )
        .unwrap();
        let server = MockServer::start().await;
        let candidates = tracker(&server, f.settings.clone(), Arc::default())
            .run_cycle()
            .await
            .unwrap();
        assert!(candidates.is_empty());
        assert_eq!(
            read_json_lenient(&f.settings.watchlist_path),
            Some(json!({"watching": []}))
        );
    }

    #[tokio::test]
    async fn stop_file_present_exits_cleanly() {
        let f = fixture();
        std::fs::write(&f.settings.stop_path, b"").unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        tracker(&server, f.settings.clone(), Arc::default())
            .run(true)
            .await;
    }

    #[tokio::test]
    async fn telegram_alert_rate_limit_honored() {
        let f = fixture();
        let settings = SmartMoneySettings {
            max_alerts_per_hour: 1,
            ..f.settings.clone()
        };
        let server = rpc_server(
            &["sig-1", "sig-2"],
            &[
                ("sig-1", tx(MINT_A, false, 1)),
                ("sig-2", tx(MINT_B, false, 1)),
            ],
        )
        .await;
        let alerter = Arc::new(RecordingAlerter::default());
        assert_eq!(
            tracker(&server, settings, alerter.clone())
                .run_cycle()
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(alerter.sent().len(), 1);
    }

    #[tokio::test]
    async fn rpc_error_object_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"error": {"code": -32000, "message": "boom"}})),
            )
            .mount(&server)
            .await;
        let rpc = HeliusRpc::with_base_url(
            reqwest::Client::new(),
            &format!("{}/", server.uri()),
            "secret",
        );
        let err = rpc.transaction("sig").await.unwrap_err().to_string();
        assert!(err.contains("boom") && !err.contains("secret"));
    }
}

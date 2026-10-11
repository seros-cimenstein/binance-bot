//! Token safety scorer for Solana memecoin candidates.
//!
//! Queries Solana RPC for the on-chain state that decides whether a mint is
//! safe enough to trade:
//!
//! * mint authority not renounced → infinite dilution → hard reject;
//! * freeze authority not renounced → can't sell → hard reject;
//! * single-holder / top-10 concentration above the caps → hard reject;
//! * otherwise a 0-100 score docked proportionally to concentration.

use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::SniperError;

pub const DEFAULT_RPC_HTTP_URL: &str = "https://api.mainnet-beta.solana.com";
pub const DEFAULT_MAX_TOP_HOLDER_PCT: f64 = 30.0;
pub const DEFAULT_MAX_TOP_10_PCT: f64 = 75.0;

const RPC_TIMEOUT: Duration = Duration::from_secs(20);

/// SPL Token v3 mint account layout length:
/// `0..36` mintAuthority option, `36..44` supply, `44` decimals,
/// `45` isInitialized, `46..82` freezeAuthority option.
pub const SPL_MINT_LAYOUT_LEN: usize = 82;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopHolder {
    pub address: String,
    pub amount: u64,
    pub pct_of_supply: f64,
}

/// Scored evaluation of one token mint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSafetyScore {
    pub mint: String,
    /// `None` = could not determine.
    pub has_mint_authority: Option<bool>,
    pub has_freeze_authority: Option<bool>,
    pub supply: Option<u64>,
    pub decimals: Option<u8>,
    pub top_holder_pct: f64,
    pub top_10_pct: f64,
    pub holder_count_top_20: usize,
    pub top_holders: Vec<TopHolder>,
    /// 0..100, higher is safer.
    pub score: f64,
    /// Set when any hard check failed.
    pub rejection_reason: Option<String>,
}

impl TokenSafetyScore {
    pub fn is_tradeable(&self) -> bool {
        self.rejection_reason.is_none() && self.score > 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintInfo {
    pub has_mint_authority: bool,
    pub has_freeze_authority: bool,
    pub supply: u64,
    pub decimals: u8,
    pub is_initialized: bool,
}

fn le_u32(bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(bytes);
    u32::from_le_bytes(buf)
}

/// Decode an SPL Token mint account from its raw payload.
pub fn parse_mint_account(data: &[u8]) -> Result<MintInfo, SniperError> {
    if data.len() < SPL_MINT_LAYOUT_LEN {
        return Err(SniperError::Scorer(format!(
            "mint account data too short: {} bytes (expected {SPL_MINT_LAYOUT_LEN})",
            data.len()
        )));
    }
    let mut supply = [0u8; 8];
    supply.copy_from_slice(&data[36..44]);
    Ok(MintInfo {
        has_mint_authority: le_u32(&data[0..4]) != 0,
        has_freeze_authority: le_u32(&data[46..50]) != 0,
        supply: u64::from_le_bytes(supply),
        decimals: data[44],
        is_initialized: data[45] != 0,
    })
}

/// Binary payload of a `getAccountInfo` value (`data: [<payload>, "base64"]`).
pub fn decode_account_data(rpc_value: &Value) -> Result<Vec<u8>, SniperError> {
    let data = rpc_value
        .get("data")
        .and_then(Value::as_array)
        .filter(|d| d.len() >= 2)
        .ok_or_else(|| SniperError::Scorer("getAccountInfo result missing data array".into()))?;
    let as_text = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    let encoding = as_text(&data[1]);
    if encoding != "base64" {
        return Err(SniperError::Scorer(format!(
            "unsupported account data encoding: {encoding}"
        )));
    }
    base64::engine::general_purpose::STANDARD
        .decode(as_text(&data[0]))
        .map_err(|err| SniperError::Scorer(format!("failed to base64-decode mint account: {err}")))
}

/// One row of `getTokenLargestAccounts`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LargestAccount {
    #[serde(default)]
    pub address: String,
    /// Raw atoms as a decimal string (RPC convention); numbers tolerated.
    #[serde(default)]
    pub amount: Value,
}

impl LargestAccount {
    fn amount_atoms(&self) -> Option<u64> {
        match &self.amount {
            Value::String(s) => s.trim().parse().ok(),
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }
}

/// `(top_holder_pct, top_10_pct, holder_count, top_20)`.
pub fn compute_top_concentration(
    holders: &[LargestAccount],
    total_supply: u64,
) -> (f64, f64, usize, Vec<TopHolder>) {
    if total_supply == 0 || holders.is_empty() {
        return (0.0, 0.0, 0, Vec::new());
    }
    let mut parsed: Vec<TopHolder> = holders
        .iter()
        .filter_map(|holder| {
            let amount = holder.amount_atoms().filter(|a| *a > 0)?;
            Some(TopHolder {
                address: holder.address.clone(),
                amount,
                pct_of_supply: amount as f64 / total_supply as f64 * 100.0,
            })
        })
        .collect();
    parsed.sort_by_key(|h| std::cmp::Reverse(h.amount));
    let top = parsed.first().map_or(0.0, |h| h.pct_of_supply);
    let top_10 = parsed.iter().take(10).map(|h| h.pct_of_supply).sum();
    let count = parsed.len();
    parsed.truncate(20);
    (top, top_10, count, parsed)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SafetyThresholds {
    pub max_top_holder_pct: f64,
    pub max_top_10_pct: f64,
}

impl Default for SafetyThresholds {
    fn default() -> Self {
        Self {
            max_top_holder_pct: DEFAULT_MAX_TOP_HOLDER_PCT,
            max_top_10_pct: DEFAULT_MAX_TOP_10_PCT,
        }
    }
}

/// Combine mint info + holder list into a single safety verdict.
pub fn score_safety(
    mint: &str,
    mint_info: Option<&MintInfo>,
    holders: &[LargestAccount],
    thresholds: SafetyThresholds,
) -> TokenSafetyScore {
    let Some(info) = mint_info else {
        return TokenSafetyScore {
            mint: mint.to_owned(),
            has_mint_authority: None,
            has_freeze_authority: None,
            supply: None,
            decimals: None,
            top_holder_pct: 0.0,
            top_10_pct: 0.0,
            holder_count_top_20: 0,
            top_holders: Vec::new(),
            score: 0.0,
            rejection_reason: Some("mint account not found".to_owned()),
        };
    };
    let SafetyThresholds {
        max_top_holder_pct,
        max_top_10_pct,
    } = thresholds;

    let mut rejection = if info.has_mint_authority {
        Some("mint authority is not renounced (deployer can dilute)".to_owned())
    } else if info.has_freeze_authority {
        Some(
            "freeze authority is not renounced (deployer can freeze your token account)".to_owned(),
        )
    } else if !info.is_initialized {
        Some("mint not initialized".to_owned())
    } else if info.decimals > 18 {
        Some(format!("unreasonable decimals={}", info.decimals))
    } else {
        None
    };

    let (top_holder_pct, top_10_pct, holder_count, top_holders) =
        compute_top_concentration(holders, info.supply);

    if rejection.is_none() {
        if top_holder_pct > max_top_holder_pct {
            rejection = Some(format!(
                "top holder owns {top_holder_pct:.1}% (max {max_top_holder_pct:.1}%)"
            ));
        } else if top_10_pct > max_top_10_pct {
            rejection = Some(format!(
                "top-10 owns {top_10_pct:.1}% (max {max_top_10_pct:.1}%)"
            ));
        }
    }

    let score = if rejection.is_some() {
        0.0
    } else {
        // Start at 100 and dock points proportional to concentration.
        let docked = 100.0
            - top_holder_pct.min(max_top_holder_pct) * (40.0 / max_top_holder_pct)
            - top_10_pct.min(max_top_10_pct) * (40.0 / max_top_10_pct);
        docked.clamp(0.0, 100.0)
    };

    TokenSafetyScore {
        mint: mint.to_owned(),
        has_mint_authority: Some(info.has_mint_authority),
        has_freeze_authority: Some(info.has_freeze_authority),
        supply: Some(info.supply),
        decimals: Some(info.decimals),
        top_holder_pct,
        top_10_pct,
        holder_count_top_20: holder_count,
        top_holders,
        score,
        rejection_reason: rejection,
    }
}

/// Source of safety verdicts for the candidate pipeline.
pub trait TokenScorer: Send + Sync {
    fn score(
        &self,
        mint: &str,
    ) -> impl Future<Output = Result<TokenSafetyScore, SniperError>> + Send;
}

/// Solana JSON-RPC backed scorer.
#[derive(Debug, Clone)]
pub struct RpcScorer {
    client: reqwest::Client,
    rpc_url: String,
    thresholds: SafetyThresholds,
}

impl RpcScorer {
    pub fn new(
        client: reqwest::Client,
        rpc_url: impl Into<String>,
        thresholds: SafetyThresholds,
    ) -> Self {
        Self {
            client,
            rpc_url: rpc_url.into(),
            thresholds,
        }
    }

    /// JSON-RPC call; HTTP / RPC-level failures are [`SniperError::Scorer`],
    /// transport and body-decoding failures are [`SniperError::Http`].
    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value, SniperError> {
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp = self
            .client
            .post(&self.rpc_url)
            .json(&payload)
            .timeout(RPC_TIMEOUT)
            .send()
            .await?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            return Err(SniperError::Scorer(format!(
                "RPC {method} returned HTTP {}",
                status.as_u16()
            )));
        }
        let mut body: Value = resp.json().await?;
        if let Some(err) = body.get("error").filter(|e| !e.is_null()) {
            return Err(SniperError::Scorer(format!("RPC {method} error: {err}")));
        }
        Ok(body
            .get_mut("result")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    pub async fn fetch_mint_info(&self, mint: &str) -> Result<MintInfo, SniperError> {
        let result = self
            .rpc_call(
                "getAccountInfo",
                json!([mint, {"encoding": "base64", "commitment": "confirmed"}]),
            )
            .await?;
        let value = result
            .get("value")
            .filter(|v| v.is_object())
            .ok_or_else(|| SniperError::Scorer(format!("mint {mint} not found on-chain")))?;
        parse_mint_account(&decode_account_data(value)?)
    }

    pub async fn fetch_largest_holders(
        &self,
        mint: &str,
    ) -> Result<Vec<LargestAccount>, SniperError> {
        let mut result = self
            .rpc_call(
                "getTokenLargestAccounts",
                json!([mint, {"commitment": "confirmed"}]),
            )
            .await?;
        let Some(Value::Array(rows)) = result.get_mut("value").map(Value::take) else {
            return Ok(Vec::new());
        };
        Ok(rows
            .into_iter()
            .filter_map(|row| serde_json::from_value(row).ok())
            .collect())
    }

    /// End-to-end: fetch mint info + holders, run the rules.
    ///
    /// A mint lookup domain failure becomes a "mint account not found"
    /// verdict; a holder lookup domain failure is non-fatal. Transport
    /// failures propagate.
    pub async fn score_token(&self, mint: &str) -> Result<TokenSafetyScore, SniperError> {
        let info = match self.fetch_mint_info(mint).await {
            Ok(info) => info,
            Err(SniperError::Scorer(_)) => {
                return Ok(score_safety(mint, None, &[], SafetyThresholds::default()));
            }
            Err(err) => return Err(err),
        };
        let holders = match self.fetch_largest_holders(mint).await {
            Ok(holders) => holders,
            Err(SniperError::Scorer(_)) => Vec::new(),
            Err(err) => return Err(err),
        };
        Ok(score_safety(mint, Some(&info), &holders, self.thresholds))
    }
}

impl TokenScorer for RpcScorer {
    async fn score(&self, mint: &str) -> Result<TokenSafetyScore, SniperError> {
        self.score_token(mint).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mint_payload(
        mint_auth: bool,
        freeze_auth: bool,
        supply: u64,
        decimals: u8,
        init: bool,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(SPL_MINT_LAYOUT_LEN);
        out.extend_from_slice(&u32::from(mint_auth).to_le_bytes());
        out.extend_from_slice(&[u8::from(mint_auth); 32]);
        out.extend_from_slice(&supply.to_le_bytes());
        out.push(decimals);
        out.push(u8::from(init));
        out.extend_from_slice(&u32::from(freeze_auth).to_le_bytes());
        out.extend_from_slice(&[2 * u8::from(freeze_auth); 32]);
        out
    }

    fn info(mint_auth: bool, freeze_auth: bool, supply: u64, decimals: u8) -> MintInfo {
        MintInfo {
            has_mint_authority: mint_auth,
            has_freeze_authority: freeze_auth,
            supply,
            decimals,
            is_initialized: true,
        }
    }

    fn holders(rows: impl IntoIterator<Item = (String, u64)>) -> Vec<LargestAccount> {
        rows.into_iter()
            .map(|(address, amount)| LargestAccount {
                address,
                amount: Value::String(amount.to_string()),
            })
            .collect()
    }

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn parse_safe_mint() {
        let parsed =
            parse_mint_account(&mint_payload(false, false, 1_000_000_000_000, 9, true)).unwrap();
        assert_eq!(parsed, info(false, false, 1_000_000_000_000, 9));
    }

    #[test]
    fn parse_unsafe_mint_with_authorities() {
        let parsed = parse_mint_account(&mint_payload(true, true, 1_000_000_000, 6, true)).unwrap();
        assert!(parsed.has_mint_authority && parsed.has_freeze_authority);
    }

    #[test]
    fn parse_mint_account_rejects_short_payload() {
        let err = parse_mint_account(&[0u8; 50]).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn decode_account_data_extracts_base64() {
        let payload = mint_payload(false, false, 1000, 6, true);
        let raw = decode_account_data(&json!({"data": [b64(&payload), "base64"]})).unwrap();
        assert_eq!(raw, payload);
    }

    #[test]
    fn decode_account_data_rejects_unsupported_encoding() {
        let err = decode_account_data(&json!({"data": ["abc", "base58"]})).unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn decode_account_data_rejects_missing_array() {
        let err = decode_account_data(&json!({"data": "not-a-list"})).unwrap_err();
        assert!(err.to_string().contains("missing data array"));
    }

    #[test]
    fn concentration_with_no_holders_or_zero_supply() {
        assert_eq!(
            compute_top_concentration(&[], 1000),
            (0.0, 0.0, 0, Vec::new())
        );
        let (top, _, _, parsed) = compute_top_concentration(&holders([("A".into(), 10)]), 0);
        assert!(top.abs() < f64::EPSILON && parsed.is_empty());
    }

    #[test]
    fn concentration_sorts_and_caps_top_20() {
        let rows = holders((0..30u64).map(|i| (format!("acct{i}"), i + 1)));
        let total: u64 = (1..=30).sum();
        let (_, top10, count, parsed) = compute_top_concentration(&rows, total);
        assert_eq!(parsed.len(), 20);
        assert_eq!(count, 30);
        assert_eq!(parsed[0].amount, 30);
        let expected = (21..=30u64).sum::<u64>() as f64 / total as f64 * 100.0;
        assert!((top10 - expected).abs() < 1e-9);
    }

    #[test]
    fn score_rejects_when_mint_account_missing() {
        let s = score_safety("M1", None, &[], SafetyThresholds::default());
        assert_eq!(
            s.rejection_reason.as_deref(),
            Some("mint account not found")
        );
        assert!(s.score.abs() < f64::EPSILON);
        assert!(!s.is_tradeable());
    }

    #[test]
    fn score_rejects_unrevoked_authorities() {
        let s = score_safety(
            "M1",
            Some(&info(true, false, 1_000_000, 6)),
            &[],
            SafetyThresholds::default(),
        );
        assert!(s.rejection_reason.unwrap().contains("mint authority"));
        let s = score_safety(
            "M1",
            Some(&info(false, true, 1_000_000, 6)),
            &[],
            SafetyThresholds::default(),
        );
        assert!(s.rejection_reason.unwrap().contains("freeze authority"));
    }

    #[test]
    fn score_rejects_concentrated_top_holder() {
        let rows = holders([("whale".into(), 600_000), ("small".into(), 1000)]);
        let s = score_safety(
            "M1",
            Some(&info(false, false, 1_000_000, 6)),
            &rows,
            SafetyThresholds::default(),
        );
        assert_eq!(
            s.rejection_reason.as_deref(),
            Some("top holder owns 60.0% (max 30.0%)")
        );
    }

    #[test]
    fn score_rejects_concentrated_top_10() {
        let rows = holders((0..12).map(|i| (format!("h{i}"), 80_000)));
        let s = score_safety(
            "M1",
            Some(&info(false, false, 1_000_000, 6)),
            &rows,
            SafetyThresholds::default(),
        );
        assert!(s.rejection_reason.unwrap().contains("top-10"));
    }

    #[test]
    fn score_passes_with_distributed_supply() {
        let rows = holders((0..50).map(|i| (format!("h{i}"), 20_000)));
        let s = score_safety(
            "M1",
            Some(&info(false, false, 1_000_000, 6)),
            &rows,
            SafetyThresholds::default(),
        );
        assert!(s.rejection_reason.is_none());
        assert!(s.is_tradeable());
        assert!((s.top_holder_pct - 2.0).abs() < 1e-9);
        assert!((s.top_10_pct - 20.0).abs() < 1e-9);
        // 100 - 2*(40/30) - 20*(40/75)
        assert!((s.score - (100.0 - 2.0 * 40.0 / 30.0 - 20.0 * 40.0 / 75.0)).abs() < 1e-9);
        let decoded: Value = serde_json::to_value(&s).unwrap();
        assert_eq!(decoded["mint"], "M1");
        assert_eq!(decoded["top_holders"].as_array().unwrap().len(), 20);
    }

    #[test]
    fn score_rejects_unreasonable_decimals() {
        let s = score_safety(
            "M1",
            Some(&info(false, false, 1_000_000, 99)),
            &[],
            SafetyThresholds::default(),
        );
        assert!(s.rejection_reason.unwrap().contains("decimals"));
    }

    #[tokio::test]
    async fn score_token_end_to_end_over_rpc() {
        let server = MockServer::start().await;
        let payload = mint_payload(false, false, 1_000_000, 6, true);
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "getAccountInfo"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {"context": {"slot": 1}, "value": {"data": [b64(&payload), "base64"]}},
            })))
            .mount(&server)
            .await;
        let rows: Vec<Value> = (0..50)
            .map(|i| json!({"address": format!("h{i}"), "amount": "20000", "decimals": 6}))
            .collect();
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "getTokenLargestAccounts"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": {"value": rows},
            })))
            .mount(&server)
            .await;
        let scorer = RpcScorer::new(
            reqwest::Client::new(),
            server.uri(),
            SafetyThresholds::default(),
        );
        let s = scorer.score_token("MINT").await.unwrap();
        assert!(s.is_tradeable());
        assert_eq!(s.holder_count_top_20, 50);
    }

    #[tokio::test]
    async fn score_token_missing_account_and_holder_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "getAccountInfo"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": {"context": {"slot": 1}, "value": null},
            })))
            .mount(&server)
            .await;
        let scorer = RpcScorer::new(
            reqwest::Client::new(),
            server.uri(),
            SafetyThresholds::default(),
        );
        let s = scorer.score_token("MINT").await.unwrap();
        assert_eq!(
            s.rejection_reason.as_deref(),
            Some("mint account not found")
        );

        let server = MockServer::start().await;
        let payload = mint_payload(false, false, 1_000_000, 6, true);
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "getAccountInfo"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": {"value": {"data": [b64(&payload), "base64"]}},
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "getTokenLargestAccounts"}),
            ))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let scorer = RpcScorer::new(
            reqwest::Client::new(),
            server.uri(),
            SafetyThresholds::default(),
        );
        let s = scorer.score_token("MINT").await.unwrap();
        // Holder failure is non-fatal: authorities scored, no concentration.
        assert!(s.is_tradeable());
        assert_eq!(s.holder_count_top_20, 0);
    }
}

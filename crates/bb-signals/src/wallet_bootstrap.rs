//! Smart-money wallet list (`data/smart-money-wallets.json`) maintenance.
//!
//! The file is operator-editable: `{"wallets": [...], "last_refreshed_ms": N,
//! "_note": "..."}`. Wallet rows stay JSON objects so manual entries keep any
//! extra fields across refreshes. When stale, Birdeye entries are rebuilt from
//! the 7-day top-traders endpoint while `source == "manual"` rows are kept.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{Value, json};

use crate::common::{atomic_write_json, read_json_lenient};
use crate::error::SignalsError;
use crate::jsonv::{Object, int_or_zero, objects};

pub const BIRDEYE_TOP_TRADERS_URL: &str = "https://public-api.birdeye.so/trader/gainers-losers?type=1W&sort_by=PnL&sort_type=desc&offset=0&limit=10";
pub const EMPTY_WALLET_NOTE: &str = "No smart-money wallets are configured. Set BIRDEYE_API_KEY to refresh from Birdeye, or hand-edit this file and add wallets with source='manual'.";
const REFRESHED_NOTE: &str = "Manual entries are preserved across refreshes. Birdeye entries are rebuilt from the 7-day top-traders endpoint when stale.";
const PUBKEY_KEYS: [&str; 9] = [
    "pubkey",
    "wallet",
    "walletAddress",
    "wallet_address",
    "owner",
    "ownerAddress",
    "trader",
    "traderAddress",
    "address",
];

/// Where and how to reach Birdeye.
#[derive(Debug, Clone)]
pub struct BirdeyeSource {
    pub http: reqwest::Client,
    pub url: String,
    pub api_key: Option<String>,
}

impl BirdeyeSource {
    pub fn new(http: reqwest::Client, api_key: Option<String>) -> Self {
        Self {
            http,
            url: BIRDEYE_TOP_TRADERS_URL.to_owned(),
            api_key: api_key.filter(|k| !k.is_empty()),
        }
    }
}

fn load_payload(path: &Path) -> Object {
    match read_json_lenient(path) {
        Some(Value::Object(map)) => map,
        _ => Object::new(),
    }
}

fn wallets_of(payload: &Object) -> Vec<Object> {
    match payload.get("wallets") {
        Some(Value::Array(items)) => objects(items.clone()),
        _ => Vec::new(),
    }
}

/// Current wallets on disk; `[]` when the file is missing or malformed.
pub fn load_wallets(path: &Path) -> Vec<Object> {
    wallets_of(&load_payload(path))
}

/// Refresh from Birdeye when older than `max_age_hours`, preserving manual
/// entries; returns the wallet list to track. A Birdeye outage keeps the
/// existing list untouched.
pub async fn refresh_wallets_if_stale(
    path: &Path,
    birdeye: &BirdeyeSource,
    max_age_hours: f64,
    now_ms: i64,
) -> Result<Vec<Object>, SignalsError> {
    let payload = load_payload(path);
    let current = wallets_of(&payload);
    let last_refreshed_ms = int_or_zero(payload.get("last_refreshed_ms"));
    let max_age_ms = (max_age_hours * 3_600_000.0) as i64;
    if last_refreshed_ms != 0 && last_refreshed_ms >= now_ms - max_age_ms {
        return Ok(current);
    }

    let manual = current
        .iter()
        .filter(|w| w.get("source").and_then(Value::as_str) == Some("manual"))
        .cloned();
    let mut fetched = Vec::new();
    if let Some(key) = &birdeye.api_key {
        match fetch_birdeye_wallets(birdeye, key).await {
            Ok(w) => fetched = w,
            Err(err) => {
                tracing::warn!(%err, "Birdeye smart-money wallet refresh failed");
                return Ok(current);
            }
        }
    }

    let wallets = dedupe_wallets(manual.chain(fetched));
    if wallets.is_empty() && birdeye.api_key.is_none() {
        tracing::warn!(
            "Smart-money wallet list is empty and BIRDEYE_API_KEY is not set; set the key or hand-edit the wallet list."
        );
        atomic_write_json(
            path,
            &json!({"wallets": [], "last_refreshed_ms": now_ms, "_note": EMPTY_WALLET_NOTE}),
        )?;
        return Ok(Vec::new());
    }
    if wallets.is_empty() {
        tracing::warn!(
            "Birdeye smart-money wallet refresh returned no usable wallets; hand-edit the wallet list if this schema changed."
        );
    }
    atomic_write_json(
        path,
        &json!({"wallets": wallets, "last_refreshed_ms": now_ms, "_note": REFRESHED_NOTE}),
    )?;
    Ok(wallets)
}

async fn fetch_birdeye_wallets(
    birdeye: &BirdeyeSource,
    api_key: &str,
) -> Result<Vec<Object>, SignalsError> {
    let data: Value = birdeye
        .http
        .get(&birdeye.url)
        .header("X-API-KEY", api_key)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| SignalsError::http(&birdeye.url, e))?
        .json()
        .await
        .map_err(|e| SignalsError::http(&birdeye.url, e))?;

    let mut wallets = Vec::new();
    for (index, row) in extract_birdeye_rows(&data).iter().take(10).enumerate() {
        let rank = index + 1;
        let pubkey = extract_pubkey(row);
        if pubkey.is_empty() {
            tracing::warn!(rank, "Birdeye trader row did not include a wallet pubkey");
            continue;
        }
        if let Value::Object(w) = json!({
            "pubkey": pubkey,
            "label": format!("birdeye-top-7d-#{rank}"),
            "source": "birdeye",
            "weight": 0.6,
        }) {
            wallets.push(w);
        }
    }
    Ok(wallets)
}

/// Trader rows from the (loosely documented) Birdeye response shapes.
fn extract_birdeye_rows(data: &Value) -> Vec<Object> {
    let as_rows = |items: &Vec<Value>| objects(items.clone());
    match data {
        Value::Array(items) => return as_rows(items),
        Value::Object(map) => {
            for key in ["data", "result"] {
                match map.get(key) {
                    Some(Value::Array(items)) => return as_rows(items),
                    Some(Value::Object(inner)) => {
                        let nested =
                            first_list(inner, &["items", "list", "traders", "wallets", "data"]);
                        if !nested.is_empty() {
                            return nested;
                        }
                    }
                    _ => {}
                }
            }
            let nested = first_list(map, &["items", "list", "traders", "wallets"]);
            if !nested.is_empty() {
                return nested;
            }
            tracing::warn!("Birdeye smart-money response schema did not include trader rows");
        }
        _ => tracing::warn!("Birdeye smart-money response was not a list/object"),
    }
    Vec::new()
}

fn first_list(data: &Object, keys: &[&str]) -> Vec<Object> {
    keys.iter()
        .find_map(|k| data.get(*k).and_then(Value::as_array))
        .map(|items| objects(items.clone()))
        .unwrap_or_default()
}

fn extract_pubkey(row: &Object) -> String {
    for key in PUBKEY_KEYS {
        match row.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => return s.trim().to_owned(),
            Some(Value::Object(inner)) => {
                let nested = extract_pubkey(inner);
                if !nested.is_empty() {
                    return nested;
                }
            }
            _ => {}
        }
    }
    String::new()
}

fn dedupe_wallets(wallets: impl IntoIterator<Item = Object>) -> Vec<Object> {
    let mut seen = HashSet::new();
    wallets
        .into_iter()
        .filter(|w| match w.get("pubkey").and_then(Value::as_str) {
            Some(pk) if !pk.is_empty() => seen.insert(pk.to_owned()),
            _ => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn source(server: Option<&MockServer>, key: Option<&str>) -> BirdeyeSource {
        BirdeyeSource {
            http: reqwest::Client::new(),
            url: server.map_or_else(
                || "http://127.0.0.1:9/unused".into(),
                |s| format!("{}/trader", s.uri()),
            ),
            api_key: key.map(str::to_owned),
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    async fn birdeye(body: Value, status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("X-API-KEY", "key"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn load_wallets_returns_empty_on_missing_or_malformed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_wallets(&dir.path().join("missing.json")).is_empty());
        let path = dir.path().join("wallets.json");
        std::fs::write(&path, "{").unwrap();
        assert!(load_wallets(&path).is_empty());
    }

    #[tokio::test]
    async fn refresh_does_nothing_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallets.json");
        let payload = json!({"wallets": [{"pubkey": "Manual111", "source": "manual"}], "last_refreshed_ms": 1_999_000});
        std::fs::write(&path, payload.to_string()).unwrap();
        let wallets = refresh_wallets_if_stale(&path, &source(None, Some("key")), 1.0, 2_000_000)
            .await
            .unwrap();
        assert_eq!(
            Value::Array(wallets.into_iter().map(Value::Object).collect()),
            payload["wallets"]
        );
        assert_eq!(read(&path), payload);
    }

    #[tokio::test]
    async fn refresh_calls_birdeye_when_stale_and_key_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallets.json");
        let server = birdeye(json!({"data": [{"walletAddress": "Bird111"}]}), 200).await;
        let wallets =
            refresh_wallets_if_stale(&path, &source(Some(&server), Some("key")), 1.0, 10_000_000)
                .await
                .unwrap();
        assert_eq!(
            Value::Array(wallets.into_iter().map(Value::Object).collect()),
            json!([{"pubkey": "Bird111", "label": "birdeye-top-7d-#1", "source": "birdeye", "weight": 0.6}])
        );
        assert_eq!(read(&path)["last_refreshed_ms"], 10_000_000);
    }

    #[tokio::test]
    async fn refresh_preserves_manual_entries_across_birdeye_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallets.json");
        std::fs::write(
            &path,
            json!({
                "wallets": [
                    {"pubkey": "Manual111", "label": "curated", "source": "manual"},
                    {"pubkey": "OldBird", "source": "birdeye"},
                ],
                "last_refreshed_ms": 1,
            })
            .to_string(),
        )
        .unwrap();
        let server = birdeye(json!({"data": {"items": [{"owner": "Bird222"}]}}), 200).await;
        let wallets =
            refresh_wallets_if_stale(&path, &source(Some(&server), Some("key")), 1.0, 10_000_000)
                .await
                .unwrap();
        let pubkeys: Vec<&str> = wallets
            .iter()
            .filter_map(|w| w["pubkey"].as_str())
            .collect();
        assert_eq!(pubkeys, vec!["Manual111", "Bird222"]);
        assert_eq!(wallets[0]["source"], "manual");
        assert_eq!(wallets[0]["label"], "curated");
    }

    #[tokio::test]
    async fn refresh_without_key_on_stale_empty_file_writes_warning_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallets.json");
        let wallets = refresh_wallets_if_stale(&path, &source(None, None), 1.0, 10_000_000)
            .await
            .unwrap();
        let payload = read(&path);
        assert!(wallets.is_empty());
        assert_eq!(payload["wallets"], json!([]));
        assert_eq!(payload["last_refreshed_ms"], 10_000_000);
        assert!(
            payload["_note"]
                .as_str()
                .unwrap()
                .contains("BIRDEYE_API_KEY")
        );
    }

    #[tokio::test]
    async fn birdeye_http_error_keeps_existing_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallets.json");
        let payload = json!({"wallets": [{"pubkey": "Existing111", "source": "manual"}], "last_refreshed_ms": 1});
        std::fs::write(&path, payload.to_string()).unwrap();
        let server = birdeye(json!({}), 500).await;
        let wallets =
            refresh_wallets_if_stale(&path, &source(Some(&server), Some("key")), 1.0, 10_000_000)
                .await
                .unwrap();
        assert_eq!(
            Value::Array(wallets.into_iter().map(Value::Object).collect()),
            payload["wallets"]
        );
        assert_eq!(read(&path), payload);
    }
}

//! Minimal DexScreener public API client shared by the V1/V2 detectors and the
//! snapshotter. Pair payloads stay as JSON objects because they are persisted
//! verbatim (`raw_signal`, `raw_pair`).

use serde_json::Value;

use crate::error::SignalsError;
use crate::jsonv::{Object, objects};

pub const DEFAULT_BASE_URL: &str = "https://api.dexscreener.com";

#[derive(Debug, Clone)]
pub struct DexScreenerApi {
    http: reqwest::Client,
    base_url: String,
}

impl DexScreenerApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_base_url(http, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// `GET /token-profiles/latest/v1`: object entries of the returned list.
    pub async fn latest_token_profiles(&self) -> Result<Vec<Object>, SignalsError> {
        let url = format!("{}/token-profiles/latest/v1", self.base_url);
        match self.get_json(&url).await? {
            Value::Array(items) => Ok(objects(items)),
            _ => Err(SignalsError::Malformed(
                "DexScreener token profiles response was not a list".into(),
            )),
        }
    }

    /// `GET /latest/dex/tokens/{a,b,...}`: object entries of `pairs`.
    ///
    /// DexScreener answers `"pairs": null` for tokens without any pair; that is
    /// an empty result, not a malformed response.
    pub async fn token_pairs(&self, token_addresses: &str) -> Result<Vec<Object>, SignalsError> {
        let url = format!("{}/latest/dex/tokens/{}", self.base_url, token_addresses);
        let Value::Object(mut data) = self.get_json(&url).await? else {
            return Err(SignalsError::Malformed(
                "DexScreener token pairs response was not an object".into(),
            ));
        };
        match data.remove("pairs") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => Ok(objects(items)),
            Some(_) => Err(SignalsError::Malformed(
                "DexScreener token pairs list malformed".into(),
            )),
        }
    }

    async fn get_json(&self, url: &str) -> Result<Value, SignalsError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| SignalsError::http(url, e))?;
        resp.json::<Value>()
            .await
            .map_err(|e| SignalsError::http(url, e))
    }
}

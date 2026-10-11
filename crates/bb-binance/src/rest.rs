//! Core REST transport: signing, server-time sync, retries, rate limits.
//!
//! Every typed endpoint in [`crate::spot`], [`crate::wallet`], [`crate::convert`],
//! [`crate::margin`] and [`crate::futures`] is a thin wrapper around
//! [`BinanceClient::request`], which callers may also use directly for
//! endpoints without a typed wrapper.

use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Method;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::time::Instant;

use crate::config::BinanceConfig;
use crate::error::{BinanceError, ErrorKind, Result};
use crate::markets::MarketsCache;
use crate::ratelimit::{RateLimitUsage, WeightLimiter, backoff_delay};

/// Which Binance REST host family a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Api {
    /// `api.binance.com`: `/api/*` and `/sapi/*`.
    Spot,
    /// `fapi.binance.com`: `/fapi/*` (USD-M futures).
    Futures,
}

impl Api {
    fn index(self) -> usize {
        match self {
            Self::Spot => 0,
            Self::Futures => 1,
        }
    }

    fn time_path(self) -> &'static str {
        match self {
            Self::Spot => "/api/v3/time",
            Self::Futures => "/fapi/v1/time",
        }
    }
}

/// Authentication level required by an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// No key, no signature.
    Public,
    /// `X-MBX-APIKEY` header only (e.g. `userDataStream`).
    ApiKey,
    /// `X-MBX-APIKEY` + `timestamp` + `recvWindow` + `signature`.
    Signed,
}

/// One REST call. Parameters are sent in the query string (Binance accepts
/// that for every verb), so the signed payload is exactly what goes on the wire.
#[derive(Debug, Clone)]
pub struct RestRequest {
    pub api: Api,
    pub method: Method,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub security: Security,
    /// Request weight reserved in the client-side limiter.
    pub weight: u32,
    /// Safe to resend after a transport/5xx failure (unknown execution status).
    /// Defaults to `false` for `POST`; 429/418 and `-1021` rejections are always retried.
    pub idempotent: bool,
}

impl RestRequest {
    /// New request with weight 1 and public security.
    pub fn new(api: Api, method: Method, path: impl Into<String>) -> Self {
        let idempotent = method != Method::POST;
        Self {
            api,
            method,
            path: path.into(),
            params: Vec::new(),
            security: Security::Public,
            weight: 1,
            idempotent,
        }
    }

    /// `GET` request.
    pub fn get(api: Api, path: impl Into<String>) -> Self {
        Self::new(api, Method::GET, path)
    }

    /// `POST` request (non-idempotent by default).
    pub fn post(api: Api, path: impl Into<String>) -> Self {
        Self::new(api, Method::POST, path)
    }

    /// `PUT` request.
    pub fn put(api: Api, path: impl Into<String>) -> Self {
        Self::new(api, Method::PUT, path)
    }

    /// `DELETE` request.
    pub fn delete(api: Api, path: impl Into<String>) -> Self {
        Self::new(api, Method::DELETE, path)
    }

    /// Append a parameter.
    pub fn param(mut self, key: &str, value: impl Display) -> Self {
        self.params.push((key.to_owned(), value.to_string()));
        self
    }

    /// Append a parameter when `Some`.
    pub fn opt_param(self, key: &str, value: Option<impl Display>) -> Self {
        match value {
            Some(v) => self.param(key, v),
            None => self,
        }
    }

    /// Append a decimal parameter without trailing zeros or exponent notation.
    pub fn decimal(self, key: &str, value: Decimal) -> Self {
        self.param(key, fmt_decimal(value))
    }

    /// Append a decimal parameter when `Some`.
    pub fn opt_decimal(self, key: &str, value: Option<Decimal>) -> Self {
        match value {
            Some(v) => self.decimal(key, v),
            None => self,
        }
    }

    /// Require signing.
    pub fn signed(mut self) -> Self {
        self.security = Security::Signed;
        self
    }

    /// Require only the API-key header.
    pub fn api_key(mut self) -> Self {
        self.security = Security::ApiKey;
        self
    }

    /// Set the request weight.
    pub fn weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }

    /// Mark as safe to resend after ambiguous failures.
    pub fn idempotent(mut self) -> Self {
        self.idempotent = true;
        self
    }

    /// Mark as unsafe to resend after ambiguous failures.
    pub fn non_idempotent(mut self) -> Self {
        self.idempotent = false;
        self
    }

    fn endpoint(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// Format a decimal for Binance parameters (`1.50000` → `1.5`).
pub fn fmt_decimal(value: Decimal) -> String {
    value.normalize().to_string()
}

#[derive(Debug, Default)]
struct Clock {
    offset_ms: AtomicI64,
    synced: AtomicBool,
}

#[derive(Debug, Deserialize)]
struct ServerTimeBody {
    #[serde(rename = "serverTime")]
    server_time: i64,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: Option<i64>,
    msg: Option<String>,
}

struct Inner {
    http: reqwest::Client,
    config: BinanceConfig,
    limiter: WeightLimiter,
    clocks: [Clock; 2],
    usage: Mutex<[RateLimitUsage; 2]>,
    blocked_until: Mutex<Option<Instant>>,
    markets: MarketsCache,
}

/// Native Binance REST client. Cheap to clone (shared connection pool,
/// limiter, clock offset, usage counters and `exchangeInfo` cache).
#[derive(Clone)]
pub struct BinanceClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for BinanceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinanceClient")
            .field("testnet", &self.inner.config.testnet)
            .field("endpoints", &self.inner.config.endpoints)
            .field("authenticated", &self.inner.config.credentials.is_some())
            .finish()
    }
}

impl BinanceClient {
    /// Build a client from configuration.
    pub fn new(config: BinanceConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .user_agent(concat!("bb-binance/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| BinanceError::Config(format!("http client: {err}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                limiter: WeightLimiter::new(
                    config.request_weight_limit,
                    config.request_weight_period,
                ),
                markets: MarketsCache::new(config.exchange_info_ttl),
                http,
                config,
                clocks: Default::default(),
                usage: Mutex::new(Default::default()),
                blocked_until: Mutex::new(None),
            }),
        })
    }

    /// [`BinanceConfig::from_env`] + [`BinanceClient::new`].
    pub fn from_env(testnet_default: bool) -> Result<Self> {
        Self::new(BinanceConfig::from_env(testnet_default)?)
    }

    /// Active configuration.
    pub fn config(&self) -> &BinanceConfig {
        &self.inner.config
    }

    /// Whether credentials are configured.
    pub fn is_authenticated(&self) -> bool {
        self.inner.config.credentials.is_some()
    }

    pub(crate) fn markets_cache(&self) -> &MarketsCache {
        &self.inner.markets
    }

    /// Last server-reported usage counters for `api`.
    pub fn usage(&self, api: Api) -> RateLimitUsage {
        self.inner
            .usage
            .lock()
            .map(|u| u[api.index()].clone())
            .unwrap_or_default()
    }

    /// Last `X-MBX-USED-WEIGHT-1M` reported by the spot API.
    pub fn used_weight_1m(&self) -> Option<u64> {
        self.usage(Api::Spot).used_weight_1m()
    }

    /// Weight reserved in the client-side rolling window.
    pub fn client_used_weight(&self) -> u32 {
        self.inner.limiter.used()
    }

    /// Current `serverTime - localTime` estimate in milliseconds.
    pub fn time_offset_ms(&self, api: Api) -> i64 {
        self.inner.clocks[api.index()]
            .offset_ms
            .load(Ordering::Relaxed)
    }

    /// Local clock corrected by the server offset (ms since epoch).
    pub fn server_now_ms(&self, api: Api) -> i64 {
        bb_core::time::now_ms() + self.time_offset_ms(api)
    }

    /// Fetch server time and store the clock offset (midpoint of the round trip).
    pub async fn sync_time(&self, api: Api) -> Result<i64> {
        let req = RestRequest::get(api, api.time_path());
        self.inner.limiter.acquire(req.weight).await?;
        let before = bb_core::time::now_ms();
        let body: ServerTimeBody = self.execute(&req, &req.endpoint()).await?;
        let after = bb_core::time::now_ms();
        let offset = body.server_time - (before + after) / 2;
        let clock = &self.inner.clocks[api.index()];
        clock.offset_ms.store(offset, Ordering::Relaxed);
        clock.synced.store(true, Ordering::Relaxed);
        tracing::debug!(api = ?api, offset_ms = offset, "Binance server time synced");
        Ok(offset)
    }

    /// Send a request with throttling, signing, retries and error mapping.
    ///
    /// Retry policy: transport errors and 5xx/`-1001`-style failures are retried
    /// with exponential backoff only for idempotent requests; HTTP 429/418 are
    /// retried after `Retry-After` (when it does not exceed
    /// [`BinanceConfig::max_retry_after`]); `-1021` triggers one time resync + resend.
    pub async fn request<T: DeserializeOwned>(&self, req: RestRequest) -> Result<T> {
        let endpoint = req.endpoint();
        let config = &self.inner.config;
        let mut attempt: u32 = 0;
        let mut resynced = false;
        loop {
            attempt += 1;
            self.wait_for_backoff(&endpoint).await?;
            if req.security == Security::Signed
                && !self.inner.clocks[req.api.index()]
                    .synced
                    .load(Ordering::Relaxed)
            {
                self.sync_time(req.api).await?;
            }
            self.inner.limiter.acquire(req.weight).await?;
            let started = Instant::now();
            let err = match self.execute::<T>(&req, &endpoint).await {
                Ok(value) => {
                    tracing::debug!(
                        endpoint = %endpoint,
                        attempt,
                        latency_ms = started.elapsed().as_millis() as u64,
                        request_weight = req.weight,
                        used_weight_1m = self.usage(req.api).used_weight_1m(),
                        "Binance API request completed"
                    );
                    return Ok(value);
                }
                Err(err) => err,
            };
            let retries_left = attempt <= config.max_retries;
            if err.kind() == ErrorKind::Timestamp && req.security == Security::Signed && !resynced {
                resynced = true;
                attempt -= 1;
                tracing::warn!(endpoint = %endpoint, "Binance -1021 timestamp error; resyncing clock");
                self.sync_time(req.api).await?;
                continue;
            }
            if let BinanceError::RateLimited { retry_after, .. } = &err {
                let delay = retry_after.unwrap_or_else(|| self.backoff(attempt));
                self.block_for(delay);
                if retries_left && delay <= config.max_retry_after {
                    tracing::warn!(endpoint = %endpoint, attempt, sleep_ms = delay.as_millis() as u64, "Binance rate limited; backing off");
                    continue;
                }
            } else if retries_left && req.idempotent && err.is_retryable() {
                let delay = self.backoff(attempt);
                tracing::warn!(endpoint = %endpoint, attempt, sleep_ms = delay.as_millis() as u64, error = %err, "Binance API request failed; retrying");
                tokio::time::sleep(delay).await;
                continue;
            }
            tracing::warn!(endpoint = %endpoint, attempt, error = %err, "Binance API request failed");
            return Err(err);
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let config = &self.inner.config;
        backoff_delay(
            attempt,
            config.base_backoff,
            config.max_backoff,
            rand::random::<f64>(),
        )
    }

    fn block_for(&self, delay: Duration) {
        if let Ok(mut until) = self.inner.blocked_until.lock() {
            let candidate = Instant::now() + delay;
            if until.is_none_or(|u| u < candidate) {
                *until = Some(candidate);
            }
        }
    }

    async fn wait_for_backoff(&self, endpoint: &str) -> Result<()> {
        let remaining = self
            .inner
            .blocked_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .map(|until| until.saturating_duration_since(Instant::now()))
            .filter(|d| !d.is_zero());
        let Some(remaining) = remaining else {
            return Ok(());
        };
        if remaining > self.inner.config.max_retry_after {
            return Err(BinanceError::RateLimited {
                endpoint: endpoint.to_owned(),
                http_status: 429,
                code: None,
                msg: "client is backing off until the Binance Retry-After window expires".into(),
                retry_after: Some(remaining),
            });
        }
        tokio::time::sleep(remaining).await;
        Ok(())
    }

    fn base_url(&self, api: Api) -> &str {
        match api {
            Api::Spot => &self.inner.config.endpoints.spot_rest,
            Api::Futures => &self.inner.config.endpoints.futures_rest,
        }
    }

    /// Encoded query string, with `recvWindow`, `timestamp` and `signature` for signed requests.
    fn encode_query(&self, req: &RestRequest) -> Result<String> {
        let config = &self.inner.config;
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in &req.params {
            query.append_pair(key, value);
        }
        if req.security != Security::Signed {
            return Ok(query.finish());
        }
        let credentials = config.require_credentials()?;
        if !req.params.iter().any(|(k, _)| k == "recvWindow") {
            query.append_pair("recvWindow", &config.recv_window_ms.to_string());
        }
        query.append_pair("timestamp", &self.server_now_ms(req.api).to_string());
        let payload = query.finish();
        let signature = credentials.sign(&payload);
        let encoded: String = url::form_urlencoded::byte_serialize(signature.as_bytes()).collect();
        Ok(format!("{payload}&signature={encoded}"))
    }

    async fn execute<T: DeserializeOwned>(&self, req: &RestRequest, endpoint: &str) -> Result<T> {
        let query = self.encode_query(req)?;
        let url = if query.is_empty() {
            format!("{}{}", self.base_url(req.api), req.path)
        } else {
            format!("{}{}?{}", self.base_url(req.api), req.path, query)
        };
        let mut builder = self.inner.http.request(req.method.clone(), url);
        if req.security != Security::Public {
            let credentials = self.inner.config.require_credentials()?;
            builder = builder.header("X-MBX-APIKEY", credentials.api_key());
        }

        let network = |err: reqwest::Error| BinanceError::Network {
            endpoint: endpoint.to_owned(),
            timeout: err.is_timeout(),
            message: err.without_url().to_string(),
        };
        let response = builder.send().await.map_err(network)?;
        let status = response.status();
        if let Ok(mut usage) = self.inner.usage.lock() {
            usage[req.api.index()].update_from_headers(response.headers());
        }
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response.text().await.map_err(network)?;

        if status.is_success() {
            return serde_json::from_str(&body).map_err(|err| BinanceError::Decode {
                endpoint: endpoint.to_owned(),
                message: err.to_string(),
            });
        }

        let parsed: Option<ErrorBody> = serde_json::from_str(&body).ok();
        let code = parsed.as_ref().and_then(|b| b.code);
        let msg = parsed
            .and_then(|b| b.msg)
            .unwrap_or_else(|| truncate(&body, 200));
        let http_status = status.as_u16();
        if http_status == 429 || http_status == 418 {
            return Err(BinanceError::RateLimited {
                endpoint: endpoint.to_owned(),
                http_status,
                code,
                msg,
                retry_after,
            });
        }
        Err(BinanceError::Api {
            endpoint: endpoint.to_owned(),
            http_status,
            code,
            msg,
        })
    }
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &trimmed[..idx]),
        None => trimmed.to_owned(),
    }
}

//! Client configuration: credentials, endpoints, timeouts, retry and rate-limit budgets.

use std::time::Duration;

use crate::auth::Credentials;
use crate::error::{BinanceError, Result};

/// Base URLs for every Binance surface the crate talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// Spot REST + `sapi` (`https://api.binance.com`).
    pub spot_rest: String,
}

impl Endpoints {
    /// Production endpoints.
    pub fn mainnet() -> Self {
        Self {
            spot_rest: "https://api.binance.com".into(),
        }
    }

    /// Spot testnet endpoints (`testnet.binance.vision`).
    pub fn testnet() -> Self {
        Self {
            spot_rest: "https://testnet.binance.vision".into(),
        }
    }

    /// Mainnet or testnet endpoints.
    pub fn for_network(testnet: bool) -> Self {
        if testnet {
            Self::testnet()
        } else {
            Self::mainnet()
        }
    }
}

/// Env names for the testnet flag (`USE_TESTNET`, legacy `BINANCE_TESTNET`).
pub const TESTNET_ENV: [&str; 2] = ["USE_TESTNET", "BINANCE_TESTNET"];

/// Runtime settings for [`crate::BinanceClient`].
///
/// Defaults mirror the Python client: `recvWindow` 10 000 ms, 4 retries,
/// 0.5 s → 8 s exponential backoff, client-side budget of 1 100 weight / 60 s.
#[derive(Debug, Clone)]
pub struct BinanceConfig {
    /// `None` restricts the client to public endpoints.
    pub credentials: Option<Credentials>,
    /// Whether [`Endpoints::testnet`] were selected.
    pub testnet: bool,
    /// Base URLs (override for tests / proxies).
    pub endpoints: Endpoints,
    /// `recvWindow` sent with signed requests.
    pub recv_window_ms: u64,
    /// Total timeout per HTTP request.
    pub request_timeout: Duration,
    /// Connect timeout per HTTP request.
    pub connect_timeout: Duration,
    /// Retries after the first attempt for retryable failures.
    pub max_retries: u32,
    /// First retry delay; doubles per attempt.
    pub base_backoff: Duration,
    /// Retry delay cap.
    pub max_backoff: Duration,
    /// Longest `Retry-After` the client will sleep through before failing fast.
    pub max_retry_after: Duration,
    /// Client-side rolling request-weight budget.
    pub request_weight_limit: u32,
    /// Rolling window for [`Self::request_weight_limit`].
    pub request_weight_period: Duration,
    /// How long cached `exchangeInfo` stays fresh.
    pub exchange_info_ttl: Duration,
}

impl BinanceConfig {
    /// Public-only configuration for mainnet or testnet.
    pub fn new(testnet: bool) -> Self {
        Self {
            credentials: None,
            testnet,
            endpoints: Endpoints::for_network(testnet),
            recv_window_ms: 10_000,
            request_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            max_retries: 4,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
            max_retry_after: Duration::from_secs(60),
            request_weight_limit: 1_100,
            request_weight_period: Duration::from_secs(60),
            exchange_info_ttl: Duration::from_secs(3_600),
        }
    }

    /// Read credentials (`BINANCE_API_KEY`, `BINANCE_API_SECRET`/`BINANCE_SECRET_KEY`)
    /// and `USE_TESTNET` (alias `BINANCE_TESTNET`). Each Python entry point had its own
    /// `USE_TESTNET` default, so the caller supplies it. Missing credentials yield a
    /// public-only configuration.
    pub fn from_env(testnet_default: bool) -> Result<Self> {
        let testnet = bb_core::env::boolean(&TESTNET_ENV, testnet_default)
            .map_err(|err| BinanceError::Config(err.to_string()))?;
        let mut config = Self::new(testnet);
        config.credentials = Credentials::from_env()?;
        Ok(config)
    }

    /// Attach credentials.
    pub fn with_credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Override the spot/sapi REST base URL.
    pub fn with_spot_rest_url(mut self, url: impl Into<String>) -> Self {
        self.endpoints.spot_rest = trim_url(url.into());
        self
    }

    /// Override retry policy.
    pub fn with_retries(mut self, max_retries: u32, base: Duration, max: Duration) -> Self {
        self.max_retries = max_retries;
        self.base_backoff = base;
        self.max_backoff = max;
        self
    }

    /// Credentials or a configuration error naming what is missing.
    pub fn require_credentials(&self) -> Result<&Credentials> {
        self.credentials.as_ref().ok_or_else(|| {
            BinanceError::Config(
                "signed Binance request requires BINANCE_API_KEY and BINANCE_API_SECRET".into(),
            )
        })
    }
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self::new(false)
    }
}

fn trim_url(url: String) -> String {
    url.trim_end_matches('/').to_owned()
}

//! Typed Binance client errors.
//!
//! Errors carry the endpoint (`"GET /api/v3/account"`, never the query string),
//! the HTTP status and Binance's `code`/`msg` when available. API keys,
//! secrets, signatures and timestamps never appear in error values.

use std::time::Duration;

/// Coarse error classes matching the Python client's exception hierarchy
/// (`AuthenticationError`, `InsufficientFundsError`, `InvalidOrderError`,
/// `RateLimitError`, `BinanceNetworkError`, `ExchangeUnavailableError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bad/unauthorised API key, invalid signature, IP not whitelisted.
    Authentication,
    /// The account cannot fund the request.
    InsufficientFunds,
    /// Order parameters were rejected (filters, precision, unknown order...).
    InvalidOrder,
    /// Request weight / order rate limits (HTTP 429/418, `-1003`, `-1015`).
    RateLimit,
    /// Request timestamp outside `recvWindow` (`-1021`).
    Timestamp,
    /// Transport failure: connect, TLS, timeout, broken body.
    Network,
    /// Binance-side transient failure (5xx, `-1001`, `-1006`, `-1007`, `-1008`).
    Unavailable,
    /// Malformed request parameters or unknown symbol.
    BadRequest,
    /// Anything else (decode failures, configuration, WebSocket protocol...).
    Other,
}

/// Every failure the crate can produce.
#[derive(Debug, thiserror::Error)]
pub enum BinanceError {
    /// Binance answered with an error status (and usually a JSON `{code,msg}` body).
    #[error("binance {endpoint} failed with HTTP {http_status}{}: {msg}", fmt_code(*.code))]
    Api {
        endpoint: String,
        http_status: u16,
        code: Option<i64>,
        msg: String,
    },
    /// HTTP 429 (rate limited) or 418 (IP banned). `retry_after` comes from the `Retry-After` header.
    #[error("binance {endpoint} rate limited with HTTP {http_status}{}: {msg}", fmt_retry(*.retry_after))]
    RateLimited {
        endpoint: String,
        http_status: u16,
        code: Option<i64>,
        msg: String,
        retry_after: Option<Duration>,
    },
    /// The request could not be delivered or the response could not be read.
    #[error("binance {endpoint} transport error: {message}")]
    Network {
        endpoint: String,
        message: String,
        timeout: bool,
    },
    /// Binance answered 2xx but the body did not match the expected shape.
    #[error("binance {endpoint} returned an unexpected payload: {message}")]
    Decode { endpoint: String, message: String },
    /// Missing/invalid configuration (credentials, URLs, PEM keys).
    #[error("invalid binance configuration: {0}")]
    Config(String),
    /// Caller-side validation failure before anything was sent.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// A symbol that cannot be normalised or is not listed.
    #[error("invalid symbol: {0}")]
    InvalidSymbol(String),
    /// WebSocket connect/protocol/subscription failure.
    #[error("binance websocket error: {0}")]
    WebSocket(String),
    /// Paged history backfill failure (e.g. pagination did not advance).
    #[error("binance history error: {0}")]
    History(String),
}

fn fmt_code(code: Option<i64>) -> String {
    code.map(|c| format!(" (code {c})")).unwrap_or_default()
}

fn fmt_retry(retry_after: Option<Duration>) -> String {
    retry_after
        .map(|d| format!(" (retry after {}s)", d.as_secs()))
        .unwrap_or_default()
}

/// Crate result alias.
pub type Result<T, E = BinanceError> = std::result::Result<T, E>;

const INSUFFICIENT_MARKERS: [&str; 3] = ["insufficient", "not enough", "balance is not enough"];

impl BinanceError {
    /// Binance error `code` when the server supplied one.
    pub fn code(&self) -> Option<i64> {
        match self {
            Self::Api { code, .. } | Self::RateLimited { code, .. } => *code,
            _ => None,
        }
    }

    /// HTTP status when the failure came from an HTTP response.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Api { http_status, .. } | Self::RateLimited { http_status, .. } => {
                Some(*http_status)
            }
            _ => None,
        }
    }

    /// Endpoint (`METHOD /path`) the error relates to, when applicable.
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Api { endpoint, .. }
            | Self::RateLimited { endpoint, .. }
            | Self::Network { endpoint, .. }
            | Self::Decode { endpoint, .. } => Some(endpoint),
            _ => None,
        }
    }

    /// Binance `msg` (or transport message) for display/audit purposes.
    pub fn message(&self) -> String {
        match self {
            Self::Api { msg, .. } | Self::RateLimited { msg, .. } => msg.clone(),
            other => other.to_string(),
        }
    }

    /// Classify the error the way the Python client mapped ccxt exceptions.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::RateLimited { .. } => ErrorKind::RateLimit,
            Self::Network { .. } => ErrorKind::Network,
            Self::InvalidRequest(_) | Self::InvalidSymbol(_) => ErrorKind::BadRequest,
            Self::Api {
                http_status,
                code,
                msg,
                ..
            } => classify(*http_status, *code, msg),
            Self::Decode { .. } | Self::Config(_) | Self::WebSocket(_) | Self::History(_) => {
                ErrorKind::Other
            }
        }
    }

    /// Whether repeating the identical request may succeed (network, 5xx, rate limit).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind(),
            ErrorKind::Network | ErrorKind::Unavailable | ErrorKind::RateLimit
        )
    }
}

fn classify(http_status: u16, code: Option<i64>, msg: &str) -> ErrorKind {
    let lower = msg.to_ascii_lowercase();
    match code {
        Some(-1021) => ErrorKind::Timestamp,
        Some(-1003 | -1015) => ErrorKind::RateLimit,
        Some(-1002 | -1022 | -2008 | -2014 | -2015) => ErrorKind::Authentication,
        Some(-1000 | -1001 | -1006 | -1007 | -1008 | -1016) => ErrorKind::Unavailable,
        Some(-2018 | -2019 | -3041 | -5002) => ErrorKind::InsufficientFunds,
        Some(-2010) if INSUFFICIENT_MARKERS.iter().any(|m| lower.contains(m)) => {
            ErrorKind::InsufficientFunds
        }
        Some(
            -2010 | -2011 | -2013 | -2021 | -2022 | -1013 | -1111 | -1112 | -1114 | -1115 | -1116
            | -1117,
        ) => ErrorKind::InvalidOrder,
        Some(c) if (-4999..=-4000).contains(&c) => ErrorKind::InvalidOrder,
        Some(c) if (-1199..=-1100).contains(&c) => ErrorKind::BadRequest,
        _ if http_status == 401 || http_status == 403 => ErrorKind::Authentication,
        _ if http_status >= 500 => ErrorKind::Unavailable,
        _ if http_status == 400 || http_status == 404 => ErrorKind::BadRequest,
        _ => ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, code: i64, msg: &str) -> BinanceError {
        BinanceError::Api {
            endpoint: "POST /api/v3/order".into(),
            http_status: status,
            code: Some(code),
            msg: msg.into(),
        }
    }

    #[test]
    fn classifies_binance_codes_like_the_python_mapping() {
        assert_eq!(
            api(
                400,
                -2010,
                "Account has insufficient balance for requested action."
            )
            .kind(),
            ErrorKind::InsufficientFunds
        );
        assert_eq!(
            api(400, -1013, "Filter failure: NOTIONAL").kind(),
            ErrorKind::InvalidOrder
        );
        assert_eq!(
            api(401, -2015, "Invalid API-key").kind(),
            ErrorKind::Authentication
        );
        assert_eq!(
            api(400, -1021, "Timestamp outside recvWindow").kind(),
            ErrorKind::Timestamp
        );
        assert_eq!(
            api(503, -1001, "Internal error").kind(),
            ErrorKind::Unavailable
        );
        assert_eq!(
            api(400, -1121, "Invalid symbol.").kind(),
            ErrorKind::BadRequest
        );
        assert_eq!(
            api(400, -4164, "notional too small").kind(),
            ErrorKind::InvalidOrder
        );
        assert!(api(502, -1, "bad gateway").is_retryable());
        assert!(!api(400, -1013, "x").is_retryable());
    }

    #[test]
    fn display_never_contains_query_strings() {
        let err = api(400, -1013, "Filter failure: LOT_SIZE");
        assert_eq!(
            err.to_string(),
            "binance POST /api/v3/order failed with HTTP 400 (code -1013): Filter failure: LOT_SIZE"
        );
    }
}

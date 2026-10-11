//! Crate error type.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SignalsError {
    #[error(transparent)]
    Config(#[from] bb_core::env::ConfigError),
    #[error("http request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{0}")]
    Malformed(String),
    #[error("{method} RPC error: {error}")]
    Rpc { method: String, error: String },
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl SignalsError {
    pub(crate) fn http(url: &str, source: reqwest::Error) -> Self {
        Self::Http {
            url: redact_url(url),
            source: source.without_url(),
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Strip the query string so API keys passed as query parameters
/// (Helius `?api-key=`) never reach logs.
pub(crate) fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?<redacted>"),
        None => url.to_owned(),
    }
}

//! Crate-wide error type.
//!
//! Variants mirror the Python per-module exception classes so callers can keep
//! the same "domain failure vs. transport failure" distinctions: e.g. the paper
//! executor treats [`SniperError::Quote`] as "no route" but any other error as
//! an unexpected exit failure.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SniperError {
    /// Detector cannot proceed (`SniperDetectorError`).
    #[error("{0}")]
    Detector(String),
    /// pump.fun event payload could not be decoded (`PumpFunDecodeError`).
    #[error("{0}")]
    Decode(String),
    /// Jupiter could not produce a usable quote (`JupiterQuoteError`).
    #[error("{0}")]
    Quote(String),
    /// Safety scorer domain failure (`SniperScorerError`).
    #[error("{0}")]
    Scorer(String),
    /// Live executor gate or RPC failure (`SniperLiveError`).
    #[error("{0}")]
    Live(String),
    /// Wallet cannot be loaded, decrypted or saved (`SniperWalletError`).
    #[error("{0}")]
    Wallet(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("websocket error: {0}")]
    WebSocket(#[from] Box<tokio_tungstenite::tungstenite::Error>),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl SniperError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Short class name used in ledger `error` strings (Python's `type(exc).__name__`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Detector(_) => "SniperDetectorError",
            Self::Decode(_) => "PumpFunDecodeError",
            Self::Quote(_) => "JupiterQuoteError",
            Self::Scorer(_) => "SniperScorerError",
            Self::Live(_) => "SniperLiveError",
            Self::Wallet(_) => "SniperWalletError",
            Self::Http(_) => "HttpError",
            Self::WebSocket(_) => "WebSocketError",
            Self::Audit(_) => "AuditError",
            Self::Io { .. } => "IoError",
        }
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for SniperError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(err))
    }
}

/// First `max` characters of `text` (Python `str[:max]`).
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

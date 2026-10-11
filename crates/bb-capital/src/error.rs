//! Crate error type.

use thiserror::Error;

/// Every failure surfaced by the capital library.
#[derive(Debug, Error)]
pub enum CapitalError {
    /// Malformed environment value.
    #[error(transparent)]
    Env(#[from] bb_core::env::ConfigError),
    /// Invalid configuration or request (well-formed but out of range).
    #[error("{0}")]
    Invalid(String),
    /// Exchange request failed.
    #[error(transparent)]
    Exchange(#[from] bb_binance::BinanceError),
    /// omp agent failure.
    #[error(transparent)]
    Agent(#[from] bb_omp::OmpError),
    /// Audit write failure.
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl CapitalError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

/// Crate result alias.
pub type Result<T, E = CapitalError> = std::result::Result<T, E>;

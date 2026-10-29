//! Crate error type.

use thiserror::Error;

/// Every failure surfaced by the trader library.
#[derive(Debug, Error)]
pub enum TraderError {
    /// Malformed environment value.
    #[error(transparent)]
    Env(#[from] bb_core::env::ConfigError),
    /// Invalid configuration (validation of otherwise well-formed values).
    #[error("{0}")]
    Config(String),
    /// A hard-safety invariant refused the action (arming, stop file, scope, permissions, halt).
    #[error("{0}")]
    Safety(String),
    /// Exchange request failed.
    #[error(transparent)]
    Exchange(#[from] bb_binance::BinanceError),
    /// Catalog / route / commission failure.
    #[error(transparent)]
    Pool(#[from] bb_pool::PoolError),
    /// omp agent failure.
    #[error(transparent)]
    Agent(#[from] bb_omp::OmpError),
    /// Audit write failure.
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
    /// Exclusive run lock unavailable.
    #[error(transparent)]
    Lock(#[from] bb_core::fsutil::LockError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl TraderError {
    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub(crate) fn safety(message: impl Into<String>) -> Self {
        Self::Safety(message.into())
    }
}

/// Crate result alias.
pub type Result<T, E = TraderError> = std::result::Result<T, E>;

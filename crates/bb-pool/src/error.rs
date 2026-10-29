//! Crate error type.

use thiserror::Error;

/// Every failure surfaced by the market catalog, commission and route library.
#[derive(Debug, Error)]
pub enum PoolError {
    /// Invalid or missing configuration or input.
    #[error("{0}")]
    Config(String),
    /// Binance REST failure.
    #[error(transparent)]
    Exchange(#[from] bb_binance::BinanceError),
}

/// Crate result alias.
pub type Result<T, E = PoolError> = std::result::Result<T, E>;

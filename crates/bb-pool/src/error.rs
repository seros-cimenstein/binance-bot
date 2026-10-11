//! Crate error type.

use thiserror::Error;

/// Every failure surfaced by the paper pool library.
#[derive(Debug, Error)]
pub enum PoolError {
    /// Invalid or missing configuration (env, fixture settings, CLI input).
    #[error("{0}")]
    Config(String),
    /// Binance REST failure.
    #[error(transparent)]
    Exchange(#[from] bb_binance::BinanceError),
    /// SQLite ledger failure.
    #[error("paper ledger error: {0}")]
    Ledger(#[from] rusqlite::Error),
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON decode/encode failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The ledger has no portfolio with this id.
    #[error("unknown virtual portfolio: {0}")]
    UnknownPortfolio(String),
    /// A simulated fill would overdraw a virtual balance.
    #[error("paper route spends more than virtual balance")]
    Overdraw,
    /// Replay fixture or replay run is invalid.
    #[error("{0}")]
    Replay(String),
    /// Public-candle research replay is invalid.
    #[error("{0}")]
    HistoricalReplay(String),
    /// Paper report cannot be built.
    #[error("{0}")]
    Report(String),
    /// Unified valuation input is invalid.
    #[error("{0}")]
    Unified(String),
}

/// Crate result alias.
pub type Result<T, E = PoolError> = std::result::Result<T, E>;

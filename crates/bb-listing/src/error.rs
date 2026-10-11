//! Crate error type.

use std::path::PathBuf;

use bb_binance::BinanceError;
use bb_core::audit::AuditError;
use bb_core::env::ConfigError;

#[derive(Debug, thiserror::Error)]
pub enum ListingError {
    /// Malformed environment variable.
    #[error(transparent)]
    Env(#[from] ConfigError),
    /// Settings that parse but are out of range.
    #[error("{0}")]
    InvalidSettings(String),
    /// Operator emergency stop file is present.
    #[error("emergency stop file present at {}; refusing to run", .0.display())]
    EmergencyStop(PathBuf),
    /// A live-submission gate is not satisfied.
    #[error("{0}")]
    Gate(String),
    /// State file exists but cannot be read or parsed.
    #[error("could not read state at {}: {source}", path.display())]
    StateRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Watchlist file exists but cannot be read, parsed, or has malformed entries.
    #[error("{0}")]
    Watchlist(String),
    /// Writing a state/watchlist file failed.
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Binance CMS announcements API failure.
    #[error("{0}")]
    Announcements(String),
    /// Binance REST failure on a call the cycle cannot proceed without.
    #[error("{context}: {source}")]
    Exchange {
        context: &'static str,
        #[source]
        source: BinanceError,
    },
    #[error(transparent)]
    Audit(#[from] AuditError),
}

impl ListingError {
    pub(crate) fn exchange(context: &'static str) -> impl FnOnce(BinanceError) -> Self {
        move |source| Self::Exchange { context, source }
    }
}

pub type Result<T, E = ListingError> = std::result::Result<T, E>;

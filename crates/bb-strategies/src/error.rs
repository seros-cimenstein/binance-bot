//! Crate error type. Runtime variants carry the exact Python exception messages
//! (`FundingArbError`, `TrendFilterError`, `MeanReversionError`); backtest
//! configuration problems map to [`StrategyError::InvalidConfig`] (Python `ValueError`).

use std::path::PathBuf;

use bb_binance::BinanceError;
use bb_core::env::ConfigError;

#[derive(Debug, thiserror::Error)]
pub enum StrategyError {
    /// `FundingArbError` in Python.
    #[error("{0}")]
    FundingArb(String),
    /// `TrendFilterError` in Python.
    #[error("{0}")]
    TrendFilter(String),
    /// `MeanReversionError` in Python.
    #[error("{0}")]
    MeanReversion(String),
    /// Invalid replay configuration or input data (Python `ValueError`).
    #[error("{0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Binance(#[from] BinanceError),
    #[error(transparent)]
    Env(#[from] ConfigError),
    #[error("http error: {0}")]
    Http(String),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in {path}: {message}")]
    Json { path: PathBuf, message: String },
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

impl StrategyError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T, E = StrategyError> = std::result::Result<T, E>;

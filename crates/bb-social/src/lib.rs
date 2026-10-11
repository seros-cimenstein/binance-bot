//! Read-only social-signal collection and per-asset evidence.
//!
//! Ingests only authorized Telegram Bot API updates and official X recent-search
//! results, stores privacy-reduced records (hashed author/channel identities) in a
//! local JSONL audit file, and summarizes recent records into [`SocialEvidence`]
//! for route selection. There is no order-submission path.
//!
//! Typical consumer flow:
//! ```no_run
//! # fn demo() -> Result<(), bb_social::SocialSignalError> {
//! let settings = bb_social::SocialSettings::from_env()?;
//! let evidence = bb_social::load_evidence(&settings, ["BTC", "ETH"])?;
//! let blocked = evidence.get("BTC").is_some_and(bb_social::SocialEvidence::blocks_route);
//! # let _ = blocked; Ok(()) }
//! ```

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod analyzer;
pub mod cli;
pub mod collect;
pub mod evidence;
pub mod parse;
pub mod record;
pub mod settings;

pub use analyzer::HeadlessSocialAnalyzer;
pub use collect::SocialCollector;
pub use evidence::{
    AssetSocialSummary, EvidenceStatus, RecentSignal, SocialEvidence, evidence_for_assets,
    load_evidence, load_records, write_audit,
};
pub use parse::{parse_telegram_updates, parse_x_recent_search};
pub use record::{SocialLabel, SocialSignalRecord, SocialSource};
pub use settings::SocialSettings;

#[derive(Debug, thiserror::Error)]
pub enum SocialSignalError {
    #[error(transparent)]
    Config(#[from] bb_core::env::ConfigError),
    #[error("{0}")]
    InvalidSettings(String),
    #[error("{0}")]
    MissingCredentials(&'static str),
    #[error("social signal HTTP request failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("social signal audit io error at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
    #[error("{0}")]
    Ai(String),
}

impl From<reqwest::Error> for SocialSignalError {
    /// Strips the URL: Telegram embeds the bot token in the request path.
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err.without_url())
    }
}

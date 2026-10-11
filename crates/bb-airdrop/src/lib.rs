//! Airdrop farming infrastructure.
//!
//! - [`wallet`]: encrypted multi-chain (Solana + EVM) wallet store.
//! - [`catalog`]: hand-curated opportunity catalog (incl. wallet-less Binance
//!   account programs).
//! - [`scheduler`]: weekly per-wallet checklist + persisted completion state.
//! - [`reminder`]: on-chain activity snapshots, auto-marking and the Telegram
//!   digest.
//! - [`state`]: JSON state-file helpers and `AIRDROP_DATA_DIR` path defaults.
//!
//! No on-chain automation: the scheduler emits a checklist the operator
//! follows; protocol interactions are per-protocol work layered on top.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod catalog;
pub mod cli;
pub mod error;
pub mod reminder;
pub mod scheduler;
pub mod state;
pub mod wallet;

pub use error::{AirdropError, Result};

//! Solana memecoin sniper.
//!
//! - [`detector`]: `logsSubscribe` launchpad feed → [`detector::PoolCreationEvent`].
//! - [`decoder`]: pump.fun `CreateEvent` decoding from `Program data:` logs.
//! - [`scorer`]: on-chain token safety verdicts.
//! - [`pipeline`]: detector → decoder → scorer → [`pipeline::TradeCandidate`].
//! - [`jupiter`]: Jupiter quote client.
//! - [`paper`]: forward-paper executor (JSONL ledger + summary JSON).
//! - [`wallet`]: encrypted keypair storage.
//! - [`live`]: signed-swap executor building blocks behind the gate ladder.
//! - [`cli`]: `bb sniper-*` subcommands.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod cli;
pub mod decoder;
pub mod detector;
pub mod error;
pub mod jupiter;
pub mod live;
pub mod paper;
pub mod pipeline;
pub mod scorer;
pub mod wallet;

pub use error::SniperError;

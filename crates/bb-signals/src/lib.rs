//! Token signal detectors and the V2 early-launch evaluation pipeline.
//!
//! V1 detectors append [`candidate::SignalCandidate`] rows to the shared signal
//! ledger (`data/signals-ledger.jsonl`) and maintain a TTL watchlist
//! (`data/signals-watchlist.json`):
//! - [`dexscreener`]: newly profiled DexScreener tokens with liquidity/volume.
//! - [`smart_money`]: SPL buys by tracked Solana wallets (Helius RPC), with the
//!   wallet list maintained by [`wallet_bootstrap`] (Birdeye top traders).
//! - [`telegram_ingester`]: contract addresses posted in Telegram channels
//!   (MTProto user account via `grammers`, see [`telegram_session`]).
//!
//! V2 records early Solana launches ([`dexscreener_launches`]), captures forward
//! snapshots ([`snapshotter`]) and renders weekly outcome reports ([`v2_report`]).
//!
//! Phase 43 ([`x_insighter`]) turns X posts collected by the Camoufox sidecar into
//! traction-scored, Haiku-assessed paper signals for Binance Spot and Solana.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )
)]

pub mod alerts;
pub mod candidate;
pub mod cli;
pub mod common;
pub mod dexscreener;
pub mod dexscreener_api;
pub mod dexscreener_launches;
pub mod error;
pub mod jsonv;
pub mod smart_money;
pub mod snapshotter;
pub mod telegram_ingester;
pub mod telegram_session;
pub mod telegram_setup;
pub mod v2_report;
pub mod wallet_bootstrap;
pub mod watchlist;
pub mod x_insighter;

pub use candidate::SignalCandidate;
pub use error::SignalsError;

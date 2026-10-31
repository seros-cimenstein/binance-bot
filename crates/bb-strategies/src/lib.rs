//! Bot Spot research harnesses (offline or public data, no order path).
//!
//! - [`bot_spot_backtest`] replays Bot Spot's cost-aware momentum rule on historical klines.
//! - [`spec_snapshot`] writes a public-data speculation snapshot (CSVs + `SUMMARY.md`).
//! - [`entry_study`] measures entries after costs.
//!
//! Shared plumbing lives in [`common`]; [`cli`] exposes the `bb` subcommands.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )
)]

pub mod bot_spot_backtest;
pub mod cli;
pub mod common;
pub mod entry_study;
pub mod error;
pub mod spec_snapshot;

pub use error::{Result, StrategyError};

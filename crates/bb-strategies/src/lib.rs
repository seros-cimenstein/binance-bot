//! Deterministic (no-LLM) strategy runtimes and their historical replay harnesses.
//!
//! Runtimes (plan-only / paper by default, live behind layered gates):
//! - [`funding_arb`]: delta-neutral funding-rate harvest (spot long + USD-M perp short),
//!   executed by [`funding_arb_executor`].
//! - [`trend_filter`]: SMA trend-filter spot rotation.
//! - [`mean_reversion`]: intraday mean-reversion paper/live runtime.
//!
//! Backtests (`edge_likely` verdicts gate the `*_PROVEN_EDGE` flags):
//! [`funding_arb_backtest`], [`trend_filter_backtest`], [`mean_reversion_backtest`],
//! [`basis_arb_backtest`], [`grid_backtest`], [`news_event_backtest`],
//! [`stat_arb_backtest`], [`bot_spot_backtest`].
//!
//! Research: [`spec_snapshot`] writes a public-data speculation snapshot (CSVs + `SUMMARY.md`);
//! [`entry_study`] measures entries after costs; [`biz_lead_study`] checks whether /biz/ talk
//! comes before price moves.
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

pub mod basis_arb_backtest;
pub mod biz_lead_study;
pub mod bot_spot_backtest;
pub mod cli;
pub mod common;
pub mod entry_study;
pub mod error;
pub mod funding_arb;
pub mod funding_arb_backtest;
pub mod funding_arb_executor;
pub mod grid_backtest;
pub mod mean_reversion;
pub mod mean_reversion_backtest;
pub mod news_event_backtest;
pub mod spec_snapshot;
pub mod stat_arb_backtest;
pub mod trend_filter;
pub mod trend_filter_backtest;

pub use error::{Result, StrategyError};

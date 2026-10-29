//! Binance Spot algorithm trader (Phase 52: no LLM).
//!
//! The operator's rule ([`decider`]) decides each cycle on the /biz/ feed ([`biz`]) and
//! the coins' own 1m prices ([`book`]); every order goes through the route path, and hard
//! safety stays in
//! [`executor`] (arming, stop file, run lock, permissions, order-test, filters, sizing,
//! reconciliation, failed-route halt, scope, Spot MARKET only, ≤ 3 legs).
//!
//! - [`config`]: `TraderConfig::from_env` (`BOT_SPOT_*`).
//! - [`scope`]: operator allow/block lists and fiat blocking.
//! - [`universe`]: catalog view, balances, route candidates.
//! - [`assets`]: cash-like (low-upside) assets.
//! - [`memory`]: trade memory from the audit log.
//! - [`context`]: decision context, `context_hash`, `route_id`.
//! - [`executor`]: live route execution. [`audit`]: rows, halt, review marker.
//! - [`state`]: the persisted cycle counter and last cycle.
//! - [`algo_backtest`]: offline backtest of the algorithm on cached 1m candles, tuned on
//!   the older half and judged on the newer (research only, no network, no order path).
//! - [`biz`]: the /biz/ feed from biz-app, recorded per cycle, information only (Phase 51).
//! - [`decider`]: the operator's rule (no LLM); backtested as `biz_algo`.
//! - [`book`]: the snapshot and book the rule decides on, from live balances and prices.
//! - [`cycle`]: per-cycle runtime and the [`cycle::Trader`] loop. [`cli`]: `bot-spot`,
//!   `algo-backtest`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod algo_backtest;
pub mod assets;
pub mod audit;
pub mod biz;
pub mod book;
pub mod cli;
pub mod config;
pub mod context;
pub mod cycle;
pub mod decider;
pub mod error;
pub mod executor;
pub mod market_mood;
pub mod memory;
pub mod scope;
pub mod state;
pub mod trades;
pub mod universe;

pub use config::TraderConfig;
pub use cycle::{CycleMode, CycleOutcome, Runtime, Trader};
pub use error::{Result, TraderError};

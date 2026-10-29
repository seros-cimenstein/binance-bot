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
//! - [`exit_plan`]: the Phase 45 sell-plan menu. [`exit_replay`]: its historical replay.
//!   [`exit_book`]: plans set by `set_exit_plan`. [`exit_watch`]: the paper watcher.
//!   [`exit_guard`]: live exit plans inside the trader daemon.
//! - [`persona_backtest`]: Phase 48 offline backtest of deterministic rule personas
//!   (research only, no network, no order path).
//! - [`biz`]: the /biz/ feed from biz-app, recorded per cycle, information only (Phase 51).
//! - [`decider`]: the operator's rule (no LLM); backtested as persona `biz_algo`.
//! - [`book`]: the snapshot and book the rule decides on, from live balances and prices.
//! - [`cycle`]: per-cycle runtime and the [`cycle::Trader`] loop. [`cli`]: `bot-spot`,
//!   `bot-spot-exit-replay`, `bot-spot-exit-watch`,
//!   `bot-spot-persona-backtest`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

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
pub mod exit_book;
pub mod exit_guard;
pub mod exit_plan;
pub mod exit_replay;
pub mod exit_watch;
pub mod market_mood;
pub mod memory;
pub mod persona_backtest;
pub mod scope;
pub mod state;
pub mod trades;
pub mod universe;

pub use config::TraderConfig;
pub use cycle::{CycleMode, CycleOutcome, Runtime, Trader};
pub use error::{Result, TraderError};

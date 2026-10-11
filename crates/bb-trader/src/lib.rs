//! Claude agentic Binance Spot trader (replaces `src/claude_spot.py`).
//!
//! One long-lived omp RPC session per trader (persistent session dir, resumed after
//! restarts) decides; host tools are its only hands. Trading guardrails are gone — costs,
//! evidence, social data and trade memory are information — while hard safety stays in
//! [`executor`] (arming, stop file, run lock, permissions, order-test, filters, sizing,
//! reconciliation, failed-route halt, scope, Spot MARKET only, ≤ 3 legs).
//!
//! - [`config`]: `TraderConfig::from_env` (`CLAUDE_SPOT_*`).
//! - [`scope`]: operator allow/block lists and fiat blocking.
//! - [`universe`]: catalog view, balances, route candidates.
//! - [`evidence`]: multi-horizon evidence and relative momentum.
//! - [`memory`]: trade memory from the audit log.
//! - [`context`]: decision context, `context_hash`, `route_id`.
//! - [`tools`]: omp host tools. [`agent`]: omp session lifecycle and prompts.
//! - [`executor`]: live route execution. [`audit`]: rows, halt, review marker.
//! - [`alerts`]: Telegram ELI5. [`journal`]: notebook + agent state.
//! - [`cycle`]: per-cycle runtime and the [`cycle::Trader`] loop. [`cli`]: `claude-spot`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod agent;
pub mod alerts;
pub mod audit;
pub mod cli;
pub mod config;
pub mod context;
pub mod cycle;
pub mod error;
pub mod evidence;
pub mod executor;
pub mod journal;
pub mod memory;
pub mod scope;
pub mod tools;
pub mod universe;

pub use config::TraderConfig;
pub use cycle::{CycleMode, CycleOutcome, Runtime, Trader};
pub use error::{Result, TraderError};

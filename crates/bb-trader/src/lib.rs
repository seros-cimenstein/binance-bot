//! Agentic Binance Spot trader (replaces `src/claude_spot.py`).
//!
//! One long-lived omp RPC session per trader (persistent session dir, resumed after
//! restarts) decides; host tools are its only hands. Trading guardrails are gone — costs,
//! evidence, social data and trade memory are information — while hard safety stays in
//! [`executor`] (arming, stop file, run lock, permissions, order-test, filters, sizing,
//! reconciliation, failed-route halt, scope, Spot MARKET only, ≤ 3 legs).
//!
//! - [`config`]: `TraderConfig::from_env` (`BOT_SPOT_*`).
//! - [`scope`]: operator allow/block lists and fiat blocking.
//! - [`universe`]: catalog view, balances, route candidates.
//! - [`evidence`]: multi-horizon evidence and relative momentum.
//! - [`memory`]: trade memory from the audit log.
//! - [`context`]: decision context, `context_hash`, `route_id`.
//! - [`tools`]: omp host tools. [`agent`]: omp session lifecycle and prompts.
//! - [`executor`]: live route execution. [`audit`]: rows, halt, review marker.
//! - [`alerts`]: Telegram ELI5. [`journal`]: notebook + agent state.
//! - [`exit_plan`]: the Phase 45 sell-plan menu. [`exit_replay`]: its historical replay.
//!   [`exit_book`]: plans set by `set_exit_plan`. [`exit_watch`]: the paper watcher.
//!   [`exit_guard`]: live exit plans inside the trader daemon.
//! - [`persona_backtest`]: Phase 48 offline backtest of deterministic rule personas
//!   (research only, no network, no order path).
//! - [`gcp2`]: GCP2 (gcp2.net) public network coherence, read-only (Phase 49).
//! - [`schizo`]: the live trader's per-cycle schizo state (GCP2 mood ring, dice, the
//!   personas' votes), persisted next to the agent state (Phase 49).
//! - [`mdp`]: the long-term value voice, a Markov decision process fitted on cached
//!   candles (Phase 50).
//! - [`biz`]: the /biz/ feed from biz-app, recorded per cycle, information only (Phase 51).
//! - [`decider`]: the Phase 51 rule-based decider (no LLM); backtested as persona
//!   `biz_algo`, and the cycle's decider with `BOT_SPOT_DECIDER=algo`.
//! - [`cycle`]: per-cycle runtime and the [`cycle::Trader`] loop. [`cli`]: `bot-spot`,
//!   `bot-spot-exit-replay`, `bot-spot-exit-watch`, `bot-spot-mdp-fit`,
//!   `bot-spot-persona-backtest`, `gcp2-history`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod agent;
pub mod alerts;
pub mod audit;
pub mod biz;
pub mod book;
pub mod cli;
pub mod config;
pub mod context;
pub mod entry_timing;
pub mod cycle;
pub mod decider;
pub mod error;
pub mod evidence;
pub mod executor;
pub mod exit_book;
pub mod exit_guard;
pub mod exit_plan;
pub mod exit_replay;
pub mod exit_watch;
pub mod gcp2;
pub mod journal;
pub mod market_mood;
pub mod mdp;
pub mod memory;
pub mod persona_backtest;
pub mod schizo;
pub mod scope;
pub mod tools;
pub mod trades;
pub mod universe;

pub use config::TraderConfig;
pub use cycle::{CycleMode, CycleOutcome, Runtime, Trader};
pub use error::{Result, TraderError};

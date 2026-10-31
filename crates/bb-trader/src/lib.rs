//! Binance Spot algorithm trader (Phase 52: no LLM).
//!
//! The operator's rule ([`decider`]) decides each cycle on the tags feed ([`tags`]) and
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
//! - [`state`]: the persisted cycle counter, last cycle and sleeve holdings (strict load).
//!   [`sleeves`]: what each sleeve owns, migration, reconcile, the `sleeve_mismatch` halt.
//! - [`algo_backtest`]: offline backtest of the operator's rule file (every sleeve) on
//!   cached 1m candles and the tags history (research only, no network, no order path).
//! - [`tags`]: the tags feed (`tags-feed-v1`) from biz-app, read every tick and summarised
//!   on each decision row. [`reach`]: how tags reach tradable assets (own tag, the
//!   operator's proxies, baskets, links). [`signals`]: the signal catalog a rule weights,
//!   by name (legacy `talk.6h`/`stance.6h`, per-asset tag signals through reach,
//!   market-wide macro and mood, momentum per horizon). [`rules`]: the operator's rule
//!   file (`BOT_SPOT_ALGO_RULE`: sleeves, levels, weights by signal name; style `fixed`
//!   or `edge`). [`edge`]: edge-style weights from a point-in-time tag scoreboard.
//! - [`decider`]: the operator's rule (no LLM), live and in the backtest.
//! - [`book`]: the snapshot and book the rule decides on, from live balances and prices.
//! - [`nonstop`]: the trader loop (a tick every 15 s, rows on change plus an hourly
//!   heartbeat). [`cycle`]: the decision runtime (routes, audit rows). [`cli`]: `bot-spot`,
//!   `algo-backtest`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod algo_backtest;
pub mod assets;
pub mod audit;
pub mod book;
pub mod cli;
pub mod config;
pub mod context;
pub mod cycle;
pub mod decider;
pub mod edge;
pub mod error;
pub mod executor;
pub mod memory;
pub mod nonstop;
pub mod reach;
pub mod rules;
pub mod scope;
pub mod sleeves;
pub mod scoreboard;
pub mod signals;
pub mod state;
pub mod tags;
pub mod trades;
pub mod universe;

pub use config::TraderConfig;
pub use cycle::{CycleMode, Runtime};
pub use nonstop::Nonstop;
pub use error::{Result, TraderError};

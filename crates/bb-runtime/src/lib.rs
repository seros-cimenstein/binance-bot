//! Legacy strategy runtime (port of `src/main.py` and its component graph).
//!
//! - [`config`]: full environment-backed `Settings` (`src/config.py`).
//! - [`db`]: SQLite order/fill/daily-loss store (`src/db.py`, identical schema).
//! - [`orders`]: order models, lifecycle manager, payload parsing and sizing.
//! - [`risk`]: pre-trade `RiskGuard` (`src/risk/guard.py`).
//! - [`portfolio`]: account + local state snapshots (`src/portfolio/snapshot.py`).
//! - [`strategies`]: strategy trait, demo SMA and loader (`src/strategies/*`).
//! - [`execution`]: execution-report router (`src/execution/router.py`).
//! - [`exchange`]: exchange boundary + Binance adapter.
//! - [`alerts`]: runtime Telegram alerts (`src/telegram_alerts.py`).
//! - [`runtime`]: `StrategyOrderClient`, `BotRuntime`, crash-restart loop.
//! - [`ops_check`]: read-only operational verification (`src/ops_check.py`).
//! - [`cli`]: `main` and `ops-check` subcommands.

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
pub mod capital;
pub mod cli;
pub mod config;
pub mod db;
pub mod exchange;
pub mod execution;
pub mod ops_check;
pub mod orders;
pub mod portfolio;
pub mod risk;
pub mod runtime;
pub mod strategies;

pub use config::Settings;
pub use db::OrderDb;
pub use exchange::{BinanceExchange, Exchange};
pub use orders::{OrderManager, OrderSizer};
pub use risk::RiskGuard;
pub use runtime::{BotRuntime, StrategyOrderClient};

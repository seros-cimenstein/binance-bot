//! Paper-only modular Binance asset pool (port of `src/pool`).
//!
//! Library surface used by other crates:
//! - [`catalog::MarketCatalog`]: classify the spot universe (refresh from a live
//!   [`bb_binance::BinanceClient`] or any [`source::MarketSource`], or build from snapshots).
//! - [`fees::CommissionService`]: cached `GET /api/v3/account/commission` schedules.
//! - [`routes::RouteEngine`]: bounded best-first route search and fee costing.
//! - [`models`]: `MarketSnapshot`, `CommissionSnapshot`, `RouteLeg`, `RouteQuote`, ...
//!
//! Nothing in this crate can submit an order, transfer, or borrow.

pub mod catalog;
pub mod cli;
pub mod config;
pub mod error;
pub mod fees;
pub mod history_replay;
pub mod ledger;
pub mod models;
pub mod pyfmt;
pub mod replay;
pub mod report;
pub mod routes;
pub mod runtime;
pub mod scoring;
pub mod source;
pub mod unified;

pub use catalog::{CatalogConfig, MarketCatalog};
pub use config::PoolSettings;
pub use error::{PoolError, Result};
pub use fees::{CommissionService, estimate_bnb_reserve};
pub use ledger::PaperLedger;
pub use models::{
    AssetValuation, CommissionSnapshot, DecisionAction, LegSide, MarketSnapshot, PaperDecision,
    RouteLeg, RouteQuote, VirtualPortfolio,
};
pub use routes::{RouteConfig, RouteEngine, gross_leg};
pub use runtime::{CycleReport, PaperPoolRuntime};
pub use source::{
    BalanceSource, CommissionSource, MarketRules, MarketSource, PoolClient, TickerQuote,
};

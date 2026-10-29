//! Binance Spot market catalog, commission schedules and route search for Bot Spot.
//!
//! - [`catalog::MarketCatalog`]: classify the spot universe (refresh from a live
//!   [`bb_binance::BinanceClient`] or any [`source::MarketSource`], or build from snapshots).
//! - [`fees::CommissionService`]: cached `GET /api/v3/account/commission` schedules.
//! - [`routes::RouteEngine`]: bounded best-first route search and fee costing.
//! - [`models`]: `MarketSnapshot`, `CommissionSnapshot`, `RouteLeg`, `RouteQuote`, ...
//!
//! Nothing in this crate can submit an order, transfer, or borrow.

pub mod catalog;
pub mod error;
pub mod fees;
pub mod models;
pub mod routes;
pub mod source;

pub use catalog::{CatalogConfig, MarketCatalog};
pub use error::{PoolError, Result};
pub use fees::{CommissionService, estimate_bnb_reserve};
pub use models::{CommissionSnapshot, LegSide, MarketSnapshot, RouteLeg, RouteQuote};
pub use routes::{RouteConfig, RouteEngine, gross_leg};
pub use source::{
    BalanceSource, CommissionSource, MarketRules, MarketSource, PoolClient, TickerQuote,
};

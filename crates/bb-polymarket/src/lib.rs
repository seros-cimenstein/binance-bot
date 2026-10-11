//! Read-only Polymarket support: public CLOB client and the binary-arbitrage scanner.
//! Nothing in this crate places orders.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod cli;
pub mod client;
pub mod scanner;

pub use client::{
    CLOB_API_BASE, MarketToken, Orderbook, OrderbookLevel, PolymarketClient, PolymarketError,
    SimplifiedMarket,
};
pub use scanner::{ArbOpportunity, Direction, ScanConfig, ScanSummary, scan, summarize};

//! Binance listing sniper (restored from the pre-sunset Python modules).
//!
//! - [`detector`]: CMS announcement polling, title classification, ticker
//!   extraction, article cursor, watchlist arming (`bb listing-detector`).
//! - [`watchlist`]: shared detector → sniper watchlist (7-day expiry, atomic writes).
//! - [`sniper`]: exchangeInfo-diff daemon with flat/trailing/momentum exits,
//!   paper and live round trips (`bb listing-sniper`).
//! - [`status`]: typed state/ledger readers for the fleet-status report.
//! - [`cli`]: `bb listing-*` subcommands.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod alerts;
pub mod cli;
pub mod detector;
pub mod error;
pub mod market;
pub mod sniper;
pub mod status;
pub mod util;
pub mod watchlist;

pub use error::{ListingError, Result};

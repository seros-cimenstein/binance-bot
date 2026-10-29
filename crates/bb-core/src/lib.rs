//! Shared foundation for every `bb-*` crate.
//!
//! - [`env`]: typed environment configuration (`.env` aware).
//! - [`money`]: `Decimal` helpers (bps, step rounding). Money is never `f64`.
//! - [`audit`]: append-only JSONL audit logs.
//! - [`fsutil`]: atomic writes, exclusive run locks, stop-file checks.
//! - [`time`]: UTC clock helpers.
//! - [`logging`]: `tracing` subscriber setup.

pub mod audit;
pub mod env;
pub mod fsutil;
pub mod logging;
pub mod money;
pub mod time;

pub use rust_decimal::Decimal;

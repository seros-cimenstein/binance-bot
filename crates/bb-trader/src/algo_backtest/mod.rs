//! Offline backtest of the trader's algorithm on the cached 1m candles of `bb entry-study`.
//! Research only: it never builds a Binance client, never fetches, and has no order path.
//!
//! - [`data`]: the offline candle cache with point-in-time readers, and the tags feed
//!   history.
//! - [`engine`]: the step clock, the [`engine::StepRule`] trait, the shared account with
//!   per-sleeve holdings, fills, metrics.
//! - [`rule`]: the operator's rule file as a backtest rule (every sleeve, both styles).
//! - [`report`]: `bb algo-backtest` and its `.md` / `.csv` / `_trades.csv` / per-sleeve
//!   trace files.

pub mod data;
pub mod engine;
pub mod report;
pub mod rule;

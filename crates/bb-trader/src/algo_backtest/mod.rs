//! Offline backtest of the trader's algorithm on the cached 1m candles of `bb entry-study`.
//! Research only: it never builds a Binance client, never fetches, and has no order path.
//!
//! - [`data`]: the offline candle cache with point-in-time readers, and the /biz/ feed
//!   history.
//! - [`engine`]: the step clock, the [`engine::StepRule`] trait, fills, metrics.
//! - [`rule`]: `biz_algo`, the algorithm as a backtest rule.
//! - [`report`]: `bb algo-backtest` and its `.md` / `.csv` / `_trades.csv` / trace files;
//!   the older/newer split and the weight tuning.

pub mod data;
pub mod engine;
pub mod report;
pub mod rule;

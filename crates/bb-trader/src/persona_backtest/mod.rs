//! Phase 48 "schizoid" persona backtest: deterministic rule personas replayed offline on
//! the cached 1m candles of `bb entry-study`. Research only: it never builds a Binance
//! client, never fetches, and has no order path.
//!
//! - [`data`]: the offline candle cache with point-in-time readers.

pub mod data;

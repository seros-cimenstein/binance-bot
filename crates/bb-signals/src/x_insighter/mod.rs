//! Phase 43 X pre-pump signal insighter (paper only).
//!
//! Design, thresholds and the pre-registered gate: `docs/signals/x-insighter-design.md`.
//! - [`schema`]: row types of every file shared with the Python collector.
//! - [`resolve`]: cashtag / mint -> Binance Spot or Solana market.
//! - [`market`]: public Binance and DexScreener market state.
//! - [`features`]: per-token windows, coordination, traction, tiers, exhaustion, rug precursors.
//! - [`haiku`]: Claude Haiku verdicts through omp RPC, with the daily cost cap.
//! - [`engine`]: the `bb x-insighter` loop (focus file, signals, ticks).
//! - [`snapshots`]: `bb x-insighter-snapshotter`, venue-aware forward snapshots.
//! - [`report`]: `bb x-insighter-report`, paper exits, ablation and gate verdict.
//!
//! Nothing here can place an order.

pub mod engine;
pub mod features;
pub mod haiku;
pub mod market;
pub mod report;
pub mod resolve;
pub mod schema;
pub mod snapshots;

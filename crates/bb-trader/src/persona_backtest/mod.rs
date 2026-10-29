//! Phase 48 persona backtest: deterministic rule personas replayed offline on
//! the cached 1m candles of `bb entry-study`. Research only: it never builds a Binance
//! client, never fetches, and has no order path.
//!
//! - [`data`]: the offline candle cache with point-in-time readers, and the /biz/ feed
//!   history.
//! - [`engine`]: the step clock, the [`engine::Persona`] trait, fills, exit plans, metrics.
//! - [`personas`]: the rule sets (`v13` proxy, the base personas, `biz_algo`).
//! - [`report`]: `bb bot-spot-persona-backtest` and its `.md` / `.csv` / `_trades.csv`;
//!   with `biz_algo` the older/newer split and the weight tuning.

pub mod data;
pub mod engine;
pub mod personas;
pub mod report;

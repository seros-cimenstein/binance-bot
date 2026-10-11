//! Strategy plugin interface, demo SMA strategy and loader
//! (port of `src/strategies/{base,demo_sma,loader}.py`).

pub mod base;
pub mod demo_sma;
pub mod loader;

pub use base::{
    Candle, FillData, OrderClient, OrderResult, Strategy, StrategyError, StrategyOrder, TickData,
};
pub use demo_sma::{DemoSmaStrategy, SmaSettings, SmaSnapshot};
pub use loader::{StrategyFactory, StrategyRegistry};

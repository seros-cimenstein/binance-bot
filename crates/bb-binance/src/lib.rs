//! Native Binance Spot client: spot and wallet (`sapi`) REST.
//!
//! - [`config`]: [`BinanceConfig`] from env (`BINANCE_API_KEY`, `BINANCE_API_SECRET`
//!   / `BINANCE_SECRET_KEY`, `USE_TESTNET`) with base-URL overrides.
//! - [`auth`]: HMAC-SHA256 and Ed25519 request signing.
//! - [`rest`]: [`BinanceClient`] transport (time sync, `-1021` resync, retries,
//!   429/418 `Retry-After` backoff, weight limiter, usage headers).
//! - [`error`]: [`BinanceError`] carrying endpoint, HTTP status, Binance code/msg.
//! - [`symbols`]: unified (`BTC/USDT`) ↔ exchange id (`BTCUSDT`).
//! - [`rules`] + [`markets`]: typed `exchangeInfo` rules, rounding, validation, cache.
//! - [`spot`], [`wallet`]: typed endpoints.
//! - [`history`]: paged kline backfill.

pub mod auth;
pub mod config;
pub mod error;
pub mod fiat;
pub mod history;
pub mod markets;
pub mod models;
pub mod ratelimit;
pub mod rest;
pub mod rules;
pub mod spot;
pub mod symbols;
pub mod wallet;

pub use auth::Credentials;
pub use config::{BinanceConfig, Endpoints};
pub use error::{BinanceError, ErrorKind, Result};
pub use rest::{Api, BinanceClient, RestRequest, Security};
pub use rules::{OrderCheck, OrderType, RuleViolation, Side, SymbolRules};
pub use symbols::{exchange_id, normalize_symbol};

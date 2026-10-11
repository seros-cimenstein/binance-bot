//! Native Binance client: spot / sapi (wallet, convert, margin) / USD-M futures
//! REST, market + user-data WebSockets, and market-data feeds.
//!
//! - [`config`]: [`BinanceConfig`] from env (`BINANCE_API_KEY`, `BINANCE_API_SECRET`
//!   / `BINANCE_SECRET_KEY`, `USE_TESTNET`) with base-URL overrides.
//! - [`auth`]: HMAC-SHA256 and Ed25519 request signing.
//! - [`rest`]: [`BinanceClient`] transport (time sync, `-1021` resync, retries,
//!   429/418 `Retry-After` backoff, weight limiter, usage headers).
//! - [`error`]: [`BinanceError`] carrying endpoint, HTTP status, Binance code/msg.
//! - [`symbols`]: unified (`BTC/USDT`, `BTC/USDT:USDT`) ↔ exchange id (`BTCUSDT`).
//! - [`rules`] + [`markets`]: typed `exchangeInfo` rules, rounding, validation, cache.
//! - [`spot`], [`wallet`], [`convert`], [`margin`], [`futures`]: typed endpoints.
//! - [`fills`]: average price / executed qty / commission from order fills.
//! - [`history`]: paged kline backfill.
//! - [`ws`]: market-stream + user-data WebSocket manager with reconnect/heartbeat.
//! - [`feeds`]: polling and WebSocket candle/ticker feeds.
//! - [`queue`]: drop-oldest bounded queue used by WS subscribers and feeds.

pub mod auth;
pub mod config;
pub mod convert;
pub mod error;
pub mod feeds;
pub mod fiat;
pub mod fills;
pub mod futures;
pub mod history;
pub mod margin;
pub mod markets;
pub mod models;
pub mod queue;
pub mod ratelimit;
pub mod rest;
pub mod rules;
pub mod spot;
pub mod symbols;
pub mod wallet;
pub mod ws;

pub use auth::Credentials;
pub use config::{BinanceConfig, Endpoints};
pub use error::{BinanceError, ErrorKind, Result};
pub use rest::{Api, BinanceClient, RestRequest, Security};
pub use rules::{OrderCheck, OrderType, RuleViolation, Side, SymbolRules};
pub use symbols::{exchange_id, normalize_symbol};

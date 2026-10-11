//! Read-only exchange inputs consumed by the pool, with live [`BinanceClient`] adapters.
//!
//! The catalog, commission service and runtime only see these traits, so the same code
//! runs against Binance, deterministic replay fixtures, and in-memory test doubles.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use bb_binance::BinanceClient;
use bb_binance::SymbolRules;
use bb_binance::spot::{AccountCommission, Ticker24h};
use rust_decimal::Decimal;

use crate::error::Result;

/// Exchange rules for one spot market, shaped like a ccxt market (`load_markets`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarketRules {
    /// Unified symbol (`BTC/USDT`).
    pub symbol: String,
    pub base: String,
    pub quote: String,
    /// Spot market (non-spot entries are skipped by the catalog).
    pub spot: bool,
    /// `status == TRADING`.
    pub active: bool,
    /// LOT_SIZE `minQty`.
    pub min_amount: Option<Decimal>,
    /// NOTIONAL / MIN_NOTIONAL `minNotional`.
    pub min_notional: Option<Decimal>,
    /// Amount precision as a step (LOT_SIZE `stepSize`, else `10^-baseAssetPrecision`).
    pub amount_precision: Option<Decimal>,
    /// Largest positive LOT_SIZE / MARKET_LOT_SIZE `stepSize`.
    pub amount_step: Option<Decimal>,
}

impl MarketRules {
    /// ccxt-equivalent market rules from Binance `exchangeInfo`.
    pub fn from_symbol_rules(rules: &SymbolRules) -> Self {
        let lot_step = rules
            .lot
            .map(|lot| lot.step_size)
            .filter(|s| *s > Decimal::ZERO);
        let market_step = rules
            .market_lot
            .map(|lot| lot.step_size)
            .filter(|s| *s > Decimal::ZERO);
        let amount_step = match (lot_step, market_step) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let precision_step = rules
            .base_asset_precision
            .and_then(|places| (places <= 28).then(|| Decimal::new(1, places)));
        Self {
            symbol: rules.unified.clone(),
            base: rules.base_asset.clone(),
            quote: rules.quote_asset.clone(),
            spot: true,
            active: rules.status == "TRADING",
            min_amount: rules.lot.map(|lot| lot.min_qty),
            min_notional: rules.notional.and_then(|n| n.min_notional),
            amount_precision: lot_step.or(precision_step),
            amount_step,
        }
    }
}

/// Top-of-book and 24h statistics for one market (ccxt `fetch_tickers` row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickerQuote {
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
    pub last: Option<Decimal>,
    pub quote_volume: Option<Decimal>,
    /// Ticker time (ms); Binance `closeTime`.
    pub timestamp_ms: Option<i64>,
}

impl From<&Ticker24h> for TickerQuote {
    fn from(ticker: &Ticker24h) -> Self {
        Self {
            bid: ticker.bid_price,
            ask: ticker.ask_price,
            last: Some(ticker.last_price),
            quote_volume: Some(ticker.quote_volume),
            timestamp_ms: (ticker.close_time > 0).then_some(ticker.close_time),
        }
    }
}

/// Market universe and tickers.
#[async_trait]
pub trait MarketSource: Send + Sync {
    /// Every market (ccxt `load_markets`).
    async fn load_markets(&self) -> Result<Vec<MarketRules>>;
    /// Tickers keyed by unified symbol (ccxt `fetch_tickers`).
    async fn fetch_tickers(&self) -> Result<HashMap<String, TickerQuote>>;
}

/// Authenticated commission schedules (`GET /api/v3/account/commission`).
#[async_trait]
pub trait CommissionSource: Send + Sync {
    /// Commission schedule for `symbol` (any spelling, e.g. `BNB/USDT`).
    async fn account_commission(&self, symbol: &str) -> Result<AccountCommission>;
}

/// Spot holdings.
#[async_trait]
pub trait BalanceSource: Send + Sync {
    /// `asset -> total` for every asset with a positive total balance.
    async fn nonzero_balances(&self) -> Result<BTreeMap<String, Decimal>>;
}

/// Everything the paper runtime reads from an exchange. Read-only by construction.
pub trait PoolClient: MarketSource + CommissionSource + BalanceSource {}

impl<T: MarketSource + CommissionSource + BalanceSource> PoolClient for T {}

#[async_trait]
impl MarketSource for BinanceClient {
    async fn load_markets(&self) -> Result<Vec<MarketRules>> {
        let markets = self.spot_markets(false).await?;
        Ok(markets.iter().map(MarketRules::from_symbol_rules).collect())
    }

    async fn fetch_tickers(&self) -> Result<HashMap<String, TickerQuote>> {
        let markets = self.spot_markets(false).await?;
        let tickers = self.tickers_24h().await?;
        Ok(tickers
            .iter()
            .filter_map(|ticker| {
                markets
                    .unified(&ticker.symbol)
                    .map(|unified| (unified.to_owned(), TickerQuote::from(ticker)))
            })
            .collect())
    }
}

#[async_trait]
impl CommissionSource for BinanceClient {
    async fn account_commission(&self, symbol: &str) -> Result<AccountCommission> {
        Ok(BinanceClient::account_commission(self, symbol).await?)
    }
}

#[async_trait]
impl BalanceSource for BinanceClient {
    async fn nonzero_balances(&self) -> Result<BTreeMap<String, Decimal>> {
        let account = self.account().await?;
        Ok(account
            .balances
            .iter()
            .filter(|balance| balance.total() > Decimal::ZERO)
            .map(|balance| (balance.asset.to_uppercase(), balance.total()))
            .collect())
    }
}

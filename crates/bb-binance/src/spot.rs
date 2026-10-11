//! Spot market data, account and order endpoints (`/api/v3/*`, plus the
//! spot-account `sapi` reads `asset/tradeFee` and `account/apiRestrictions`).

use std::collections::BTreeMap;
use std::fmt;

use rust_decimal::Decimal;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::error::{BinanceError, Result};
use crate::models::{Kline, PriceLevel, RateLimitRule, ServerTime, de_opt_i64_flexible};
use crate::rest::{Api, BinanceClient, RestRequest};
use crate::rules::{OrderType, Side, SymbolFilter};
use crate::symbols::exchange_id;

/// `GET /api/v3/exchangeInfo` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExchangeInfo {
    pub timezone: String,
    pub server_time: i64,
    pub rate_limits: Vec<RateLimitRule>,
    pub symbols: Vec<SpotSymbolInfo>,
}

/// One spot symbol in `exchangeInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SpotSymbolInfo {
    pub symbol: String,
    pub status: String,
    pub base_asset: String,
    pub base_asset_precision: Option<u32>,
    pub quote_asset: String,
    pub quote_precision: Option<u32>,
    pub quote_asset_precision: Option<u32>,
    pub base_commission_precision: Option<u32>,
    pub quote_commission_precision: Option<u32>,
    pub order_types: Vec<String>,
    pub iceberg_allowed: bool,
    pub oco_allowed: bool,
    pub oto_allowed: bool,
    pub quote_order_qty_market_allowed: bool,
    pub allow_trailing_stop: bool,
    pub cancel_replace_allowed: bool,
    pub is_spot_trading_allowed: bool,
    pub is_margin_trading_allowed: bool,
    pub filters: Vec<SymbolFilter>,
    pub permissions: Vec<String>,
    pub permission_sets: Vec<Vec<String>>,
    pub default_self_trade_prevention_mode: Option<String>,
    pub allowed_self_trade_prevention_modes: Vec<String>,
}

/// `GET /api/v3/ticker/24hr` row (FULL type).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Ticker24h {
    pub symbol: String,
    pub price_change: Decimal,
    pub price_change_percent: Decimal,
    pub weighted_avg_price: Decimal,
    pub prev_close_price: Option<Decimal>,
    pub last_price: Decimal,
    pub last_qty: Option<Decimal>,
    pub bid_price: Option<Decimal>,
    pub bid_qty: Option<Decimal>,
    pub ask_price: Option<Decimal>,
    pub ask_qty: Option<Decimal>,
    pub open_price: Decimal,
    pub high_price: Decimal,
    pub low_price: Decimal,
    pub volume: Decimal,
    pub quote_volume: Decimal,
    pub open_time: i64,
    pub close_time: i64,
    pub first_id: i64,
    pub last_id: i64,
    pub count: u64,
}

/// `GET /api/v3/ticker/bookTicker` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BookTicker {
    pub symbol: String,
    pub bid_price: Decimal,
    pub bid_qty: Decimal,
    pub ask_price: Decimal,
    pub ask_qty: Decimal,
}

impl BookTicker {
    /// `(bid + ask) / 2` when both sides are positive.
    pub fn mid(&self) -> Option<Decimal> {
        (self.bid_price > Decimal::ZERO && self.ask_price > Decimal::ZERO)
            .then(|| (self.bid_price + self.ask_price) / Decimal::TWO)
    }
}

/// `GET /api/v3/ticker/price` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PriceTicker {
    pub symbol: String,
    pub price: Decimal,
}

/// `GET /api/v3/avgPrice`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AvgPrice {
    pub mins: u32,
    pub price: Decimal,
    pub close_time: Option<i64>,
}

/// `GET /api/v3/depth`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OrderBook {
    pub last_update_id: i64,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

/// `GET /api/v3/trades` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PublicTrade {
    pub id: i64,
    pub price: Decimal,
    pub qty: Decimal,
    pub quote_qty: Decimal,
    pub time: i64,
    pub is_buyer_maker: bool,
    pub is_best_match: bool,
}

/// One balance row of `GET /api/v3/account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Balance {
    pub asset: String,
    pub free: Decimal,
    pub locked: Decimal,
}

impl Balance {
    /// `free + locked`.
    pub fn total(&self) -> Decimal {
        self.free + self.locked
    }
}

/// Commission rates block (`commissionRates` in account / order test / account commission).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommissionRates {
    pub maker: Decimal,
    pub taker: Decimal,
    pub buyer: Decimal,
    pub seller: Decimal,
}

/// `GET /api/v3/account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountInfo {
    pub maker_commission: i64,
    pub taker_commission: i64,
    pub buyer_commission: i64,
    pub seller_commission: i64,
    pub commission_rates: Option<CommissionRates>,
    pub can_trade: bool,
    pub can_withdraw: bool,
    pub can_deposit: bool,
    pub brokered: bool,
    pub require_self_trade_prevention: bool,
    pub prevent_sor: bool,
    pub update_time: i64,
    pub account_type: String,
    pub balances: Vec<Balance>,
    pub permissions: Vec<String>,
    pub uid: Option<i64>,
}

impl AccountInfo {
    /// Balance row for `asset` (zero row when absent).
    pub fn balance(&self, asset: &str) -> Balance {
        self.balances
            .iter()
            .find(|b| b.asset.eq_ignore_ascii_case(asset))
            .cloned()
            .unwrap_or_else(|| Balance {
                asset: asset.to_uppercase(),
                ..Balance::default()
            })
    }

    /// `asset → free` for every row with `free > 0`.
    pub fn free_balances(&self) -> BTreeMap<String, Decimal> {
        self.balances
            .iter()
            .filter(|b| b.free > Decimal::ZERO)
            .map(|b| (b.asset.clone(), b.free))
            .collect()
    }
}

/// `GET /api/v3/account/commission`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountCommission {
    pub symbol: String,
    pub standard_commission: CommissionRates,
    pub tax_commission: CommissionRates,
    pub special_commission: Option<CommissionRates>,
    pub discount: Option<CommissionDiscount>,
}

/// BNB-burn discount block of [`AccountCommission`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CommissionDiscount {
    pub enabled_for_account: bool,
    pub enabled_for_symbol: bool,
    pub discount_asset: String,
    pub discount: Decimal,
}

/// `GET /sapi/v1/asset/tradeFee` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TradeFee {
    pub symbol: String,
    pub maker_commission: Decimal,
    pub taker_commission: Decimal,
}

/// `GET /sapi/v1/account/apiRestrictions`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiRestrictions {
    pub ip_restrict: bool,
    pub create_time: i64,
    pub enable_reading: bool,
    pub enable_spot_and_margin_trading: bool,
    pub enable_withdrawals: bool,
    pub enable_internal_transfer: bool,
    pub enable_margin: bool,
    pub enable_futures: bool,
    pub permits_universal_transfer: bool,
    pub enable_vanilla_options: bool,
    pub enable_fix_api: bool,
    pub enable_fix_read_only: bool,
    pub enable_portfolio_margin_trading: bool,
    pub trading_authority_expiration_time: Option<i64>,
}

/// Order status (`status` / `X`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    #[default]
    New,
    PendingNew,
    PartiallyFilled,
    Filled,
    Canceled,
    PendingCancel,
    Rejected,
    Expired,
    ExpiredInMatch,
    /// Any value this crate does not know yet.
    #[serde(other)]
    Unknown,
}

impl OrderStatus {
    /// Final states (Python treated `closed/filled/canceled/rejected/expired` as terminal).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Canceled | Self::Rejected | Self::Expired | Self::ExpiredInMatch
        )
    }

    /// ccxt-style lower-case status (`open`, `closed`, `canceled`, `rejected`, `expired`).
    pub fn ccxt(self) -> &'static str {
        match self {
            Self::New | Self::PendingNew | Self::PartiallyFilled => "open",
            Self::Filled => "closed",
            Self::Canceled | Self::PendingCancel => "canceled",
            Self::Rejected => "rejected",
            Self::Expired | Self::ExpiredInMatch => "expired",
            Self::Unknown => "unknown",
        }
    }
}

/// `timeInForce`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
    /// Futures post-only.
    Gtx,
}

impl fmt::Display for TimeInForce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gtc => "GTC",
            Self::Ioc => "IOC",
            Self::Fok => "FOK",
            Self::Gtx => "GTX",
        })
    }
}

/// `newOrderRespType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResponseType {
    Ack,
    Result,
    Full,
}

impl fmt::Display for ResponseType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ack => "ACK",
            Self::Result => "RESULT",
            Self::Full => "FULL",
        })
    }
}

/// A new spot (or margin) order. Use the constructors, then optional setters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    /// Unified or exchange symbol; converted to the exchange id on send.
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Option<Decimal>,
    pub quote_order_qty: Option<Decimal>,
    pub price: Option<Decimal>,
    pub stop_price: Option<Decimal>,
    pub time_in_force: Option<TimeInForce>,
    pub new_client_order_id: Option<String>,
    pub response_type: ResponseType,
}

impl NewOrder {
    fn base(symbol: &str, side: Side, order_type: OrderType) -> Self {
        Self {
            symbol: symbol.to_owned(),
            side,
            order_type,
            quantity: None,
            quote_order_qty: None,
            price: None,
            stop_price: None,
            time_in_force: None,
            new_client_order_id: None,
            response_type: ResponseType::Full,
        }
    }

    /// MARKET order sized in base asset (`quantity`).
    pub fn market(symbol: &str, side: Side, quantity: Decimal) -> Self {
        Self {
            quantity: Some(quantity),
            ..Self::base(symbol, side, OrderType::Market)
        }
    }

    /// MARKET order sized in quote asset (`quoteOrderQty`).
    pub fn market_quote(symbol: &str, side: Side, quote_qty: Decimal) -> Self {
        Self {
            quote_order_qty: Some(quote_qty),
            ..Self::base(symbol, side, OrderType::Market)
        }
    }

    /// LIMIT GTC order.
    pub fn limit(symbol: &str, side: Side, quantity: Decimal, price: Decimal) -> Self {
        Self {
            quantity: Some(quantity),
            price: Some(price),
            time_in_force: Some(TimeInForce::Gtc),
            ..Self::base(symbol, side, OrderType::Limit)
        }
    }

    /// LIMIT_MAKER (post-only) order.
    pub fn limit_maker(symbol: &str, side: Side, quantity: Decimal, price: Decimal) -> Self {
        Self {
            quantity: Some(quantity),
            price: Some(price),
            ..Self::base(symbol, side, OrderType::LimitMaker)
        }
    }

    /// Set `newClientOrderId`.
    pub fn client_order_id(mut self, id: impl Into<String>) -> Self {
        self.new_client_order_id = Some(id.into());
        self
    }

    /// Set `timeInForce`.
    pub fn time_in_force(mut self, tif: TimeInForce) -> Self {
        self.time_in_force = Some(tif);
        self
    }

    /// Set `newOrderRespType` (default FULL).
    pub fn response_type(mut self, response_type: ResponseType) -> Self {
        self.response_type = response_type;
        self
    }

    /// Set `stopPrice`.
    pub fn stop_price(mut self, stop: Decimal) -> Self {
        self.stop_price = Some(stop);
        self
    }

    /// Pre-flight checks ported from `_validate_order_request`.
    pub fn validate(&self) -> Result<()> {
        let positive = |v: Option<Decimal>| v.is_some_and(|v| v > Decimal::ZERO);
        if self.symbol.trim().is_empty() {
            return Err(BinanceError::InvalidRequest(
                "symbol must not be blank".into(),
            ));
        }
        match (self.quantity, self.quote_order_qty) {
            (Some(_), Some(_)) => {
                return Err(BinanceError::InvalidRequest(
                    "quantity and quoteOrderQty are mutually exclusive".into(),
                ));
            }
            (None, None) => {
                return Err(BinanceError::InvalidRequest(
                    "amount must be a finite value greater than zero".into(),
                ));
            }
            (q, qq) if !positive(q.or(qq)) => {
                return Err(BinanceError::InvalidRequest(
                    "amount must be a finite value greater than zero".into(),
                ));
            }
            _ => {}
        }
        if self.quote_order_qty.is_some() && self.order_type != OrderType::Market {
            return Err(BinanceError::InvalidRequest(
                "quoteOrderQty is only valid for MARKET orders".into(),
            ));
        }
        if !self.order_type.is_market() && !positive(self.price) {
            return Err(BinanceError::InvalidRequest(
                "price must be a finite value greater than zero for limit orders".into(),
            ));
        }
        Ok(())
    }

    /// Append the order parameters to a request.
    pub(crate) fn apply(&self, req: RestRequest) -> Result<RestRequest> {
        self.validate()?;
        Ok(req
            .param("symbol", exchange_id(&self.symbol)?)
            .param("side", self.side)
            .param("type", self.order_type)
            .opt_param("timeInForce", self.time_in_force)
            .opt_decimal("quantity", self.quantity)
            .opt_decimal("quoteOrderQty", self.quote_order_qty)
            .opt_decimal("price", self.price)
            .opt_decimal("stopPrice", self.stop_price)
            .opt_param("newClientOrderId", self.new_client_order_id.as_deref())
            .param("newOrderRespType", self.response_type))
    }
}

/// One execution in a FULL order response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OrderFill {
    pub price: Decimal,
    pub qty: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub trade_id: Option<i64>,
}

/// `POST /api/v3/order`, `GET /api/v3/order`, `DELETE /api/v3/order` and open-orders rows
/// (fields absent from a given response type default to zero / empty).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Order {
    pub symbol: String,
    pub order_id: i64,
    pub order_list_id: Option<i64>,
    pub client_order_id: String,
    pub orig_client_order_id: Option<String>,
    #[serde(deserialize_with = "de_opt_i64_flexible")]
    pub transact_time: Option<i64>,
    pub price: Decimal,
    pub orig_qty: Decimal,
    pub executed_qty: Decimal,
    pub cummulative_quote_qty: Decimal,
    pub orig_quote_order_qty: Option<Decimal>,
    pub status: OrderStatus,
    pub time_in_force: Option<String>,
    #[serde(rename = "type")]
    pub order_type: String,
    pub side: String,
    pub stop_price: Option<Decimal>,
    pub time: Option<i64>,
    pub update_time: Option<i64>,
    pub is_working: Option<bool>,
    pub working_time: Option<i64>,
    pub self_trade_prevention_mode: Option<String>,
    pub fills: Vec<OrderFill>,
    /// Margin orders: amount auto-borrowed (`sideEffectType=MARGIN_BUY`).
    pub margin_buy_borrow_amount: Option<Decimal>,
    pub margin_buy_borrow_asset: Option<String>,
    /// Margin orders: `isIsolated`.
    pub is_isolated: Option<bool>,
}

impl Order {
    /// `cummulativeQuoteQty / executedQty` when anything executed.
    pub fn average_price(&self) -> Option<Decimal> {
        (self.executed_qty > Decimal::ZERO).then(|| self.cummulative_quote_qty / self.executed_qty)
    }
}

/// `GET /api/v3/myTrades` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MyTrade {
    pub symbol: String,
    pub id: i64,
    pub order_id: i64,
    pub order_list_id: Option<i64>,
    pub price: Decimal,
    pub qty: Decimal,
    pub quote_qty: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub time: i64,
    pub is_buyer: bool,
    pub is_maker: bool,
    pub is_best_match: bool,
}

/// Filters for [`BinanceClient::my_trades`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TradeQuery {
    pub order_id: Option<i64>,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub from_id: Option<i64>,
    pub limit: Option<u32>,
}

/// Identify an order by exchange id or client id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderRef {
    Id(i64),
    ClientId(String),
}

impl OrderRef {
    pub(crate) fn apply(&self, req: RestRequest) -> RestRequest {
        match self {
            Self::Id(id) => req.param("orderId", id),
            Self::ClientId(id) => req.param("origClientOrderId", id),
        }
    }
}

impl From<i64> for OrderRef {
    fn from(id: i64) -> Self {
        Self::Id(id)
    }
}

/// Free/locked/total for one asset (port of `get_balance(asset)`; `used` = `locked`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetBalance {
    pub asset: String,
    pub free: Decimal,
    pub used: Decimal,
    pub total: Decimal,
}

impl BinanceClient {
    /// `GET /api/v3/ping`.
    pub async fn ping(&self) -> Result<()> {
        self.request::<IgnoredAny>(RestRequest::get(Api::Spot, "/api/v3/ping"))
            .await
            .map(|_| ())
    }

    /// `GET /api/v3/time`.
    pub async fn server_time(&self) -> Result<i64> {
        self.request::<ServerTime>(RestRequest::get(Api::Spot, "/api/v3/time"))
            .await
            .map(|t| t.server_time)
    }

    /// `GET /api/v3/exchangeInfo` (all symbols, or the given ones). Prefer
    /// [`BinanceClient::spot_markets`] for cached typed rules.
    pub async fn exchange_info(&self, symbols: Option<&[&str]>) -> Result<ExchangeInfo> {
        let mut req = RestRequest::get(Api::Spot, "/api/v3/exchangeInfo").weight(20);
        if let Some(symbols) = symbols {
            let ids: Vec<String> = symbols
                .iter()
                .map(|s| exchange_id(s))
                .collect::<Result<_>>()?;
            req = req.param("symbols", json_list(&ids));
        }
        self.request(req).await
    }

    /// `GET /api/v3/ticker/24hr?symbol=`.
    pub async fn ticker_24h(&self, symbol: &str) -> Result<Ticker24h> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/ticker/24hr")
                .param("symbol", exchange_id(symbol)?)
                .weight(2),
        )
        .await
    }

    /// `GET /api/v3/ticker/24hr` for every symbol (weight 80).
    pub async fn tickers_24h(&self) -> Result<Vec<Ticker24h>> {
        self.request(RestRequest::get(Api::Spot, "/api/v3/ticker/24hr").weight(80))
            .await
    }

    /// `GET /api/v3/ticker/bookTicker?symbol=`.
    pub async fn book_ticker(&self, symbol: &str) -> Result<BookTicker> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/ticker/bookTicker")
                .param("symbol", exchange_id(symbol)?)
                .weight(2),
        )
        .await
    }

    /// `GET /api/v3/ticker/bookTicker` for every symbol.
    pub async fn book_tickers(&self) -> Result<Vec<BookTicker>> {
        self.request(RestRequest::get(Api::Spot, "/api/v3/ticker/bookTicker").weight(4))
            .await
    }

    /// `GET /api/v3/ticker/price?symbol=`.
    pub async fn price(&self, symbol: &str) -> Result<PriceTicker> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/ticker/price")
                .param("symbol", exchange_id(symbol)?)
                .weight(2),
        )
        .await
    }

    /// `GET /api/v3/ticker/price` for every symbol.
    pub async fn prices(&self) -> Result<Vec<PriceTicker>> {
        self.request(RestRequest::get(Api::Spot, "/api/v3/ticker/price").weight(4))
            .await
    }

    /// `GET /api/v3/avgPrice`.
    pub async fn avg_price(&self, symbol: &str) -> Result<AvgPrice> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/avgPrice").param("symbol", exchange_id(symbol)?),
        )
        .await
    }

    /// Mid price from the book ticker, falling back to the last trade price
    /// (port of Python `get_ticker`).
    pub async fn mid_price(&self, symbol: &str) -> Result<Decimal> {
        if let Some(mid) = self.book_ticker(symbol).await?.mid() {
            return Ok(mid);
        }
        let last = self.price(symbol).await?.price;
        if last > Decimal::ZERO {
            return Ok(last);
        }
        Err(BinanceError::Decode {
            endpoint: "GET /api/v3/ticker/price".into(),
            message: format!("ticker for {symbol} did not include bid/ask or last price"),
        })
    }

    /// `GET /api/v3/depth` (limit 5..5000).
    pub async fn depth(&self, symbol: &str, limit: u32) -> Result<OrderBook> {
        let weight = match limit {
            0..=100 => 5,
            101..=500 => 25,
            501..=1000 => 50,
            _ => 250,
        };
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/depth")
                .param("symbol", exchange_id(symbol)?)
                .param("limit", limit)
                .weight(weight),
        )
        .await
    }

    /// `GET /api/v3/klines` (one page, `limit` ≤ 1000). See [`crate::history`] for backfill.
    pub async fn klines(
        &self,
        symbol: &str,
        interval: &str,
        start_time: Option<i64>,
        end_time: Option<i64>,
        limit: Option<u32>,
    ) -> Result<Vec<Kline>> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/klines")
                .param("symbol", exchange_id(symbol)?)
                .param("interval", interval.trim())
                .opt_param("startTime", start_time)
                .opt_param("endTime", end_time)
                .opt_param("limit", limit)
                .weight(2),
        )
        .await
    }

    /// `GET /api/v3/trades`.
    pub async fn recent_trades(
        &self,
        symbol: &str,
        limit: Option<u32>,
    ) -> Result<Vec<PublicTrade>> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/trades")
                .param("symbol", exchange_id(symbol)?)
                .opt_param("limit", limit)
                .weight(25),
        )
        .await
    }

    /// `GET /api/v3/account` (signed, weight 20).
    pub async fn account(&self) -> Result<AccountInfo> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/account")
                .signed()
                .weight(20),
        )
        .await
    }

    /// Balance for one asset (port of `get_balance(asset)`).
    pub async fn asset_balance(&self, asset: &str) -> Result<AssetBalance> {
        let asset = asset.trim().to_uppercase();
        if asset.is_empty() {
            return Err(BinanceError::InvalidRequest(
                "asset must not be blank".into(),
            ));
        }
        let row = self.account().await?.balance(&asset);
        Ok(AssetBalance {
            total: row.total(),
            free: row.free,
            used: row.locked,
            asset,
        })
    }

    /// `GET /api/v3/account/commission` (signed, weight 20).
    pub async fn account_commission(&self, symbol: &str) -> Result<AccountCommission> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/account/commission")
                .param("symbol", exchange_id(symbol)?)
                .signed()
                .weight(20),
        )
        .await
    }

    /// `GET /sapi/v1/asset/tradeFee` (signed). `None` = all symbols.
    pub async fn trade_fee(&self, symbol: Option<&str>) -> Result<Vec<TradeFee>> {
        let symbol = symbol.map(exchange_id).transpose()?;
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/asset/tradeFee")
                .opt_param("symbol", symbol)
                .signed(),
        )
        .await
    }

    /// `GET /sapi/v1/account/apiRestrictions` (signed).
    pub async fn api_restrictions(&self) -> Result<ApiRestrictions> {
        self.request(RestRequest::get(Api::Spot, "/sapi/v1/account/apiRestrictions").signed())
            .await
    }

    /// `POST /api/v3/order/test`: validates and signs the order without
    /// sending it to the matching engine.
    pub async fn test_order(&self, order: &NewOrder) -> Result<()> {
        let req = order.apply(
            RestRequest::post(Api::Spot, "/api/v3/order/test")
                .signed()
                .idempotent(),
        )?;
        self.request::<IgnoredAny>(req).await.map(|_| ())
    }

    /// `POST /api/v3/order` (LIVE). Not retried after ambiguous failures.
    pub async fn create_order(&self, order: &NewOrder) -> Result<Order> {
        let req = order.apply(RestRequest::post(Api::Spot, "/api/v3/order").signed())?;
        self.request(req).await
    }

    /// `GET /api/v3/order`.
    pub async fn query_order(&self, symbol: &str, order: &OrderRef) -> Result<Order> {
        let req = RestRequest::get(Api::Spot, "/api/v3/order")
            .param("symbol", exchange_id(symbol)?)
            .signed()
            .weight(4);
        self.request(order.apply(req)).await
    }

    /// `DELETE /api/v3/order`.
    pub async fn cancel_order(&self, symbol: &str, order: &OrderRef) -> Result<Order> {
        let req = RestRequest::delete(Api::Spot, "/api/v3/order")
            .param("symbol", exchange_id(symbol)?)
            .signed();
        self.request(order.apply(req)).await
    }

    /// `DELETE /api/v3/openOrders` (all open orders on a symbol).
    pub async fn cancel_open_orders(&self, symbol: &str) -> Result<Vec<Order>> {
        self.request(
            RestRequest::delete(Api::Spot, "/api/v3/openOrders")
                .param("symbol", exchange_id(symbol)?)
                .signed(),
        )
        .await
    }

    /// `GET /api/v3/openOrders` (one symbol, or all with weight 80).
    pub async fn open_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>> {
        let symbol = symbol.map(exchange_id).transpose()?;
        let weight = if symbol.is_some() { 6 } else { 80 };
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/openOrders")
                .opt_param("symbol", symbol)
                .signed()
                .weight(weight),
        )
        .await
    }

    /// `GET /api/v3/myTrades`.
    pub async fn my_trades(&self, symbol: &str, query: &TradeQuery) -> Result<Vec<MyTrade>> {
        self.request(
            RestRequest::get(Api::Spot, "/api/v3/myTrades")
                .param("symbol", exchange_id(symbol)?)
                .opt_param("orderId", query.order_id)
                .opt_param("startTime", query.start_time)
                .opt_param("endTime", query.end_time)
                .opt_param("fromId", query.from_id)
                .opt_param("limit", query.limit)
                .signed()
                .weight(20),
        )
        .await
    }
}

/// `["A","B"]` JSON list parameter (no spaces), as Binance expects for `symbols=`.
pub(crate) fn json_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", quoted.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn new_order_validation_matches_python_rules() {
        assert!(
            NewOrder::market_quote("BTC/USDT", Side::Buy, dec!(6))
                .validate()
                .is_ok()
        );
        assert!(
            NewOrder::market("BTC/USDT", Side::Sell, dec!(0))
                .validate()
                .is_err()
        );
        let mut limit = NewOrder::limit("BTC/USDT", Side::Buy, dec!(1), dec!(1));
        limit.price = None;
        assert!(limit.validate().is_err());
        let mut both = NewOrder::market("BTCUSDT", Side::Buy, dec!(1));
        both.quote_order_qty = Some(dec!(1));
        assert!(both.validate().is_err());
    }

    #[test]
    fn order_parses_full_response_and_status() -> serde_json::Result<()> {
        let order: Order = serde_json::from_str(
            r#"{"symbol":"BTCUSDT","orderId":28,"orderListId":-1,"clientOrderId":"6gCrw2kRUAF9CvJDGP16IP","transactTime":1507725176595,"price":"0.00000000","origQty":"10.00000000","executedQty":"10.00000000","cummulativeQuoteQty":"10.00000000","status":"FILLED","timeInForce":"GTC","type":"MARKET","side":"SELL","workingTime":1507725176595,"selfTradePreventionMode":"NONE","fills":[{"price":"4000.00000000","qty":"1.00000000","commission":"4.00000000","commissionAsset":"USDT","tradeId":56}]}"#,
        )?;
        assert_eq!(order.status, OrderStatus::Filled);
        assert!(order.status.is_terminal());
        assert_eq!(order.status.ccxt(), "closed");
        assert_eq!(order.fills[0].commission, dec!(4));
        assert_eq!(order.average_price(), Some(dec!(1)));
        let weird: OrderStatus = serde_json::from_str("\"SOMETHING_NEW\"")?;
        assert_eq!(weird, OrderStatus::Unknown);
        Ok(())
    }
}

//! USD-M futures (`fapi.binance.com`): market data, account, positions,
//! leverage/margin settings and orders.
//!
//! Setting changes (`change_leverage`, `change_margin_type`, `set_position_side_dual`,
//! `modify_position_margin`) and order creation are LIVE and never retried after
//! ambiguous failures.

use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{BinanceError, Result};
use crate::models::{Kline, RateLimitRule, de_opt_i64_flexible};
use crate::rest::{Api, BinanceClient, RestRequest};
use crate::rules::{OrderType, Side, SymbolFilter};
use crate::spot::{OrderRef, OrderStatus, TimeInForce};
use crate::symbols::exchange_id;

/// `GET /fapi/v1/exchangeInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesExchangeInfo {
    pub timezone: String,
    pub server_time: i64,
    pub rate_limits: Vec<RateLimitRule>,
    pub symbols: Vec<FuturesSymbolInfo>,
}

/// One USD-M contract in `exchangeInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesSymbolInfo {
    pub symbol: String,
    pub pair: String,
    pub contract_type: String,
    pub delivery_date: i64,
    pub onboard_date: i64,
    pub status: String,
    pub maint_margin_percent: Option<Decimal>,
    pub required_margin_percent: Option<Decimal>,
    pub base_asset: String,
    pub quote_asset: String,
    pub margin_asset: String,
    pub price_precision: Option<u32>,
    pub quantity_precision: Option<u32>,
    pub base_asset_precision: Option<u32>,
    pub quote_precision: Option<u32>,
    pub underlying_type: String,
    pub underlying_sub_type: Vec<String>,
    pub trigger_protect: Option<Decimal>,
    pub liquidation_fee: Option<Decimal>,
    pub market_take_bound: Option<Decimal>,
    pub filters: Vec<SymbolFilter>,
    pub order_types: Vec<String>,
    pub time_in_force: Vec<String>,
}

/// `GET /fapi/v1/premiumIndex` (mark price + funding).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PremiumIndex {
    pub symbol: String,
    pub mark_price: Decimal,
    pub index_price: Decimal,
    pub estimated_settle_price: Decimal,
    pub last_funding_rate: Decimal,
    pub interest_rate: Decimal,
    pub next_funding_time: i64,
    pub time: i64,
}

/// `GET /fapi/v1/fundingRate` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FundingRate {
    pub symbol: String,
    pub funding_rate: Decimal,
    pub funding_time: i64,
    /// Empty string for old rows → `None`.
    #[serde(deserialize_with = "de_opt_decimal_lenient")]
    pub mark_price: Option<Decimal>,
}

/// `GET /fapi/v1/fundingInfo` row (symbols with adjusted caps / intervals).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FundingInfo {
    pub symbol: String,
    pub adjusted_funding_rate_cap: Decimal,
    pub adjusted_funding_rate_floor: Decimal,
    pub funding_interval_hours: u32,
    pub disclaimer: bool,
}

/// `GET /fapi/v1/openInterest`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenInterest {
    pub symbol: String,
    pub open_interest: Decimal,
    pub time: i64,
}

/// `GET /futures/data/openInterestHist` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenInterestHist {
    pub symbol: String,
    pub sum_open_interest: Decimal,
    pub sum_open_interest_value: Decimal,
    #[serde(rename = "CMCCirculatingSupply")]
    pub cmc_circulating_supply: Option<Decimal>,
    #[serde(deserialize_with = "crate::models::de_i64_flexible")]
    pub timestamp: i64,
}

/// `GET /futures/data/globalLongShortAccountRatio` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LongShortRatio {
    pub symbol: String,
    pub long_short_ratio: Decimal,
    pub long_account: Decimal,
    pub short_account: Decimal,
    #[serde(deserialize_with = "crate::models::de_i64_flexible")]
    pub timestamp: i64,
}

/// `GET /fapi/v1/ticker/24hr` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesTicker24h {
    pub symbol: String,
    pub price_change: Decimal,
    pub price_change_percent: Decimal,
    pub weighted_avg_price: Decimal,
    pub last_price: Decimal,
    pub last_qty: Decimal,
    pub open_price: Decimal,
    pub high_price: Decimal,
    pub low_price: Decimal,
    pub volume: Decimal,
    pub quote_volume: Decimal,
    pub open_time: i64,
    pub close_time: i64,
    pub count: u64,
}

/// `GET /fapi/v1/ticker/bookTicker` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesBookTicker {
    pub symbol: String,
    pub bid_price: Decimal,
    pub bid_qty: Decimal,
    pub ask_price: Decimal,
    pub ask_qty: Decimal,
    pub time: i64,
}

/// One `assets` row of `GET /fapi/v3/account` / `GET /fapi/v3/balance`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesAsset {
    pub asset: String,
    pub wallet_balance: Decimal,
    pub unrealized_profit: Decimal,
    pub margin_balance: Decimal,
    pub maint_margin: Decimal,
    pub initial_margin: Decimal,
    pub position_initial_margin: Decimal,
    pub open_order_initial_margin: Decimal,
    pub cross_wallet_balance: Decimal,
    pub cross_un_pnl: Decimal,
    pub available_balance: Decimal,
    pub max_withdraw_amount: Decimal,
    pub update_time: i64,
    /// `/fapi/v3/balance` uses `balance` instead of `walletBalance`.
    pub balance: Option<Decimal>,
}

/// One `positions` row of `GET /fapi/v3/account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesAccountPosition {
    pub symbol: String,
    pub position_side: String,
    pub position_amt: Decimal,
    pub unrealized_profit: Decimal,
    pub isolated_margin: Decimal,
    pub notional: Decimal,
    pub isolated_wallet: Decimal,
    pub initial_margin: Decimal,
    pub maint_margin: Decimal,
    pub update_time: i64,
}

/// `GET /fapi/v3/account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesAccount {
    pub total_initial_margin: Decimal,
    pub total_maint_margin: Decimal,
    pub total_wallet_balance: Decimal,
    pub total_unrealized_profit: Decimal,
    pub total_margin_balance: Decimal,
    pub total_position_initial_margin: Decimal,
    pub total_open_order_initial_margin: Decimal,
    pub total_cross_wallet_balance: Decimal,
    pub total_cross_un_pnl: Decimal,
    pub available_balance: Decimal,
    pub max_withdraw_amount: Decimal,
    pub assets: Vec<FuturesAsset>,
    pub positions: Vec<FuturesAccountPosition>,
}

/// `GET /fapi/v3/positionRisk` row (v2-only fields optional).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PositionRisk {
    pub symbol: String,
    pub position_side: String,
    pub position_amt: Decimal,
    pub entry_price: Decimal,
    pub break_even_price: Decimal,
    pub mark_price: Decimal,
    pub un_realized_profit: Decimal,
    pub liquidation_price: Decimal,
    pub isolated_margin: Decimal,
    pub notional: Decimal,
    pub margin_asset: Option<String>,
    pub isolated_wallet: Decimal,
    pub initial_margin: Option<Decimal>,
    pub maint_margin: Option<Decimal>,
    pub position_initial_margin: Option<Decimal>,
    pub open_order_initial_margin: Option<Decimal>,
    pub adl: Option<i64>,
    pub bid_notional: Option<Decimal>,
    pub ask_notional: Option<Decimal>,
    pub update_time: i64,
    pub leverage: Option<Decimal>,
    pub margin_type: Option<String>,
    pub max_notional_value: Option<String>,
}

/// `GET /fapi/v1/symbolConfig` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SymbolConfig {
    pub symbol: String,
    pub margin_type: String,
    pub is_auto_add_margin: bool,
    pub leverage: u32,
    /// May be `"INF"`, so kept as text.
    pub max_notional_value: String,
}

/// One bracket of `GET /fapi/v1/leverageBracket`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LeverageBracket {
    pub bracket: u32,
    pub initial_leverage: u32,
    pub notional_cap: Decimal,
    pub notional_floor: Decimal,
    pub maint_margin_ratio: Decimal,
    pub cum: Decimal,
}

/// Brackets for one symbol.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SymbolBrackets {
    pub symbol: String,
    pub notional_coef: Option<Decimal>,
    pub brackets: Vec<LeverageBracket>,
}

/// `POST /fapi/v1/leverage` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LeverageChange {
    pub leverage: u32,
    pub max_notional_value: String,
    pub symbol: String,
}

/// `ISOLATED` / `CROSSED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginType {
    Isolated,
    Crossed,
}

impl fmt::Display for MarginType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Isolated => "ISOLATED",
            Self::Crossed => "CROSSED",
        })
    }
}

/// `BOTH` (one-way) / `LONG` / `SHORT` (hedge mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSide {
    Both,
    Long,
    Short,
}

impl fmt::Display for PositionSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Both => "BOTH",
            Self::Long => "LONG",
            Self::Short => "SHORT",
        })
    }
}

/// A new USD-M order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuturesNewOrder {
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub position_side: Option<PositionSide>,
    pub quantity: Option<Decimal>,
    pub price: Option<Decimal>,
    pub stop_price: Option<Decimal>,
    pub time_in_force: Option<TimeInForce>,
    pub reduce_only: bool,
    pub close_position: bool,
    pub new_client_order_id: Option<String>,
}

impl FuturesNewOrder {
    /// MARKET order for `quantity` contracts (base units).
    pub fn market(symbol: &str, side: Side, quantity: Decimal) -> Self {
        Self {
            symbol: symbol.to_owned(),
            side,
            order_type: OrderType::Market,
            position_side: None,
            quantity: Some(quantity),
            price: None,
            stop_price: None,
            time_in_force: None,
            reduce_only: false,
            close_position: false,
            new_client_order_id: None,
        }
    }

    /// LIMIT GTC order.
    pub fn limit(symbol: &str, side: Side, quantity: Decimal, price: Decimal) -> Self {
        Self {
            order_type: OrderType::Limit,
            price: Some(price),
            time_in_force: Some(TimeInForce::Gtc),
            ..Self::market(symbol, side, quantity)
        }
    }

    /// Set `reduceOnly=true`.
    pub fn reduce_only(mut self) -> Self {
        self.reduce_only = true;
        self
    }

    /// Set `positionSide`.
    pub fn position_side(mut self, side: PositionSide) -> Self {
        self.position_side = Some(side);
        self
    }

    /// Set `newClientOrderId`.
    pub fn client_order_id(mut self, id: impl Into<String>) -> Self {
        self.new_client_order_id = Some(id.into());
        self
    }

    fn apply(&self, req: RestRequest) -> Result<RestRequest> {
        if !self.close_position && !self.quantity.is_some_and(|q| q > Decimal::ZERO) {
            return Err(BinanceError::InvalidRequest(
                "futures quantity must be greater than zero".into(),
            ));
        }
        if !self.order_type.is_market() && !self.price.is_some_and(|p| p > Decimal::ZERO) {
            return Err(BinanceError::InvalidRequest(
                "price must be greater than zero for limit orders".into(),
            ));
        }
        Ok(req
            .param("symbol", exchange_id(&self.symbol)?)
            .param("side", self.side)
            .param("type", self.order_type)
            .opt_param("positionSide", self.position_side)
            .opt_param("timeInForce", self.time_in_force)
            .opt_decimal("quantity", self.quantity)
            .opt_decimal("price", self.price)
            .opt_decimal("stopPrice", self.stop_price)
            .opt_param("reduceOnly", self.reduce_only.then_some("true"))
            .opt_param("closePosition", self.close_position.then_some("true"))
            .opt_param("newClientOrderId", self.new_client_order_id.as_deref())
            .param("newOrderRespType", "RESULT"))
    }
}

/// USD-M order (create/query/cancel/open orders).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesOrder {
    pub order_id: i64,
    pub symbol: String,
    pub status: OrderStatus,
    pub client_order_id: String,
    pub price: Decimal,
    pub avg_price: Decimal,
    pub orig_qty: Decimal,
    pub executed_qty: Decimal,
    pub cum_qty: Option<Decimal>,
    pub cum_quote: Decimal,
    pub time_in_force: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub orig_type: Option<String>,
    pub reduce_only: bool,
    pub close_position: bool,
    pub side: String,
    pub position_side: String,
    pub stop_price: Option<Decimal>,
    pub working_type: Option<String>,
    pub price_protect: Option<bool>,
    pub time: Option<i64>,
    pub update_time: i64,
}

/// `GET /fapi/v1/userTrades` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesTrade {
    pub symbol: String,
    pub id: i64,
    pub order_id: i64,
    pub side: String,
    pub position_side: String,
    pub price: Decimal,
    pub qty: Decimal,
    pub quote_qty: Decimal,
    pub realized_pnl: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub time: i64,
    pub buyer: bool,
    pub maker: bool,
}

/// `GET /fapi/v1/income` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Income {
    pub symbol: String,
    pub income_type: String,
    pub income: Decimal,
    pub asset: String,
    pub info: String,
    pub time: i64,
    #[serde(deserialize_with = "de_opt_i64_flexible")]
    pub tran_id: Option<i64>,
    pub trade_id: String,
}

/// `GET /fapi/v1/commissionRate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FuturesCommissionRate {
    pub symbol: String,
    pub maker_commission_rate: Decimal,
    pub taker_commission_rate: Decimal,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DualSide {
    dual_side_position: bool,
}

fn de_opt_decimal_lenient<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Decimal>, D::Error> {
    use std::str::FromStr as _;
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) if s.trim().is_empty() => Ok(None),
        serde_json::Value::String(s) => Decimal::from_str(s.trim())
            .map(Some)
            .map_err(serde::de::Error::custom),
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string())
            .or_else(|_| Decimal::from_scientific(&n.to_string()))
            .map(Some)
            .map_err(serde::de::Error::custom),
        other => Err(serde::de::Error::custom(format!(
            "expected decimal, got {other}"
        ))),
    }
}

/// Market-data range filters for history endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeQuery {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub limit: Option<u32>,
}

impl RangeQuery {
    fn apply(&self, req: RestRequest) -> RestRequest {
        req.opt_param("startTime", self.start_time)
            .opt_param("endTime", self.end_time)
            .opt_param("limit", self.limit)
    }
}

fn single_symbol(symbol: Option<&str>) -> Result<Option<String>> {
    symbol.map(exchange_id).transpose()
}

impl BinanceClient {
    /// `GET /fapi/v1/exchangeInfo`. Prefer [`BinanceClient::futures_markets`] for cached rules.
    pub async fn futures_exchange_info(&self) -> Result<FuturesExchangeInfo> {
        self.request(RestRequest::get(Api::Futures, "/fapi/v1/exchangeInfo"))
            .await
    }

    /// `GET /fapi/v1/premiumIndex?symbol=`.
    pub async fn premium_index(&self, symbol: &str) -> Result<PremiumIndex> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v1/premiumIndex")
                .param("symbol", exchange_id(symbol)?),
        )
        .await
    }

    /// `GET /fapi/v1/premiumIndex` for every contract.
    pub async fn premium_indexes(&self) -> Result<Vec<PremiumIndex>> {
        self.request(RestRequest::get(Api::Futures, "/fapi/v1/premiumIndex").weight(10))
            .await
    }

    /// `GET /fapi/v1/fundingRate` (history; `limit` ≤ 1000).
    pub async fn funding_rate_history(
        &self,
        symbol: Option<&str>,
        range: &RangeQuery,
    ) -> Result<Vec<FundingRate>> {
        let req = RestRequest::get(Api::Futures, "/fapi/v1/fundingRate")
            .opt_param("symbol", single_symbol(symbol)?);
        self.request(range.apply(req)).await
    }

    /// `GET /fapi/v1/fundingInfo`.
    pub async fn funding_info(&self) -> Result<Vec<FundingInfo>> {
        self.request(RestRequest::get(Api::Futures, "/fapi/v1/fundingInfo"))
            .await
    }

    /// `GET /fapi/v1/openInterest`.
    pub async fn open_interest(&self, symbol: &str) -> Result<OpenInterest> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v1/openInterest")
                .param("symbol", exchange_id(symbol)?),
        )
        .await
    }

    /// `GET /futures/data/openInterestHist` (`period`: 5m..1d).
    pub async fn open_interest_hist(
        &self,
        symbol: &str,
        period: &str,
        range: &RangeQuery,
    ) -> Result<Vec<OpenInterestHist>> {
        let req = RestRequest::get(Api::Futures, "/futures/data/openInterestHist")
            .param("symbol", exchange_id(symbol)?)
            .param("period", period);
        self.request(range.apply(req)).await
    }

    /// `GET /futures/data/globalLongShortAccountRatio`.
    pub async fn global_long_short_ratio(
        &self,
        symbol: &str,
        period: &str,
        range: &RangeQuery,
    ) -> Result<Vec<LongShortRatio>> {
        let req = RestRequest::get(Api::Futures, "/futures/data/globalLongShortAccountRatio")
            .param("symbol", exchange_id(symbol)?)
            .param("period", period);
        self.request(range.apply(req)).await
    }

    /// `GET /fapi/v1/klines` (one page, `limit` ≤ 1500).
    pub async fn futures_klines(
        &self,
        symbol: &str,
        interval: &str,
        range: &RangeQuery,
    ) -> Result<Vec<Kline>> {
        let req = RestRequest::get(Api::Futures, "/fapi/v1/klines")
            .param("symbol", exchange_id(symbol)?)
            .param("interval", interval.trim())
            .weight(5);
        self.request(range.apply(req)).await
    }

    /// `GET /fapi/v1/ticker/24hr` for every contract.
    pub async fn futures_tickers_24h(&self) -> Result<Vec<FuturesTicker24h>> {
        self.request(RestRequest::get(Api::Futures, "/fapi/v1/ticker/24hr").weight(40))
            .await
    }

    /// `GET /fapi/v1/ticker/bookTicker?symbol=`.
    pub async fn futures_book_ticker(&self, symbol: &str) -> Result<FuturesBookTicker> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v1/ticker/bookTicker")
                .param("symbol", exchange_id(symbol)?)
                .weight(2),
        )
        .await
    }

    /// `GET /fapi/v3/account` (weight 5).
    pub async fn futures_account(&self) -> Result<FuturesAccount> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v3/account")
                .signed()
                .weight(5),
        )
        .await
    }

    /// `GET /fapi/v3/balance` (weight 5).
    pub async fn futures_balances(&self) -> Result<Vec<FuturesAsset>> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v3/balance")
                .signed()
                .weight(5),
        )
        .await
    }

    /// `GET /fapi/v3/positionRisk` (one symbol or all; weight 5).
    pub async fn position_risk(&self, symbol: Option<&str>) -> Result<Vec<PositionRisk>> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v3/positionRisk")
                .opt_param("symbol", single_symbol(symbol)?)
                .signed()
                .weight(5),
        )
        .await
    }

    /// `GET /fapi/v1/symbolConfig` (weight 5).
    pub async fn symbol_config(&self, symbol: Option<&str>) -> Result<Vec<SymbolConfig>> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v1/symbolConfig")
                .opt_param("symbol", single_symbol(symbol)?)
                .signed()
                .weight(5),
        )
        .await
    }

    /// `GET /fapi/v1/leverageBracket` (the single-symbol form may return an object or a list).
    pub async fn leverage_brackets(&self, symbol: Option<&str>) -> Result<Vec<SymbolBrackets>> {
        let value: serde_json::Value = self
            .request(
                RestRequest::get(Api::Futures, "/fapi/v1/leverageBracket")
                    .opt_param("symbol", single_symbol(symbol)?)
                    .signed(),
            )
            .await?;
        let decode = |err: serde_json::Error| BinanceError::Decode {
            endpoint: "GET /fapi/v1/leverageBracket".into(),
            message: err.to_string(),
        };
        if value.is_array() {
            serde_json::from_value(value).map_err(decode)
        } else {
            serde_json::from_value(value)
                .map(|one| vec![one])
                .map_err(decode)
        }
    }

    /// `GET /fapi/v1/positionSide/dual` → hedge mode enabled?
    pub async fn position_side_dual(&self) -> Result<bool> {
        self.request::<DualSide>(
            RestRequest::get(Api::Futures, "/fapi/v1/positionSide/dual")
                .signed()
                .weight(30),
        )
        .await
        .map(|d| d.dual_side_position)
    }

    /// `POST /fapi/v1/positionSide/dual` (LIVE setting).
    pub async fn set_position_side_dual(&self, dual: bool) -> Result<()> {
        self.request::<serde::de::IgnoredAny>(
            RestRequest::post(Api::Futures, "/fapi/v1/positionSide/dual")
                .param("dualSidePosition", dual)
                .signed(),
        )
        .await
        .map(|_| ())
    }

    /// `POST /fapi/v1/leverage` (LIVE setting).
    pub async fn change_leverage(&self, symbol: &str, leverage: u32) -> Result<LeverageChange> {
        self.request(
            RestRequest::post(Api::Futures, "/fapi/v1/leverage")
                .param("symbol", exchange_id(symbol)?)
                .param("leverage", leverage)
                .signed(),
        )
        .await
    }

    /// `POST /fapi/v1/marginType` (LIVE setting). `-4046 No need to change margin type`
    /// is treated as success.
    pub async fn change_margin_type(&self, symbol: &str, margin_type: MarginType) -> Result<()> {
        let result = self
            .request::<serde::de::IgnoredAny>(
                RestRequest::post(Api::Futures, "/fapi/v1/marginType")
                    .param("symbol", exchange_id(symbol)?)
                    .param("marginType", margin_type)
                    .signed(),
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(err) if err.code() == Some(-4046) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// `POST /fapi/v1/positionMargin` (LIVE; `add=true` adds isolated margin, else reduces).
    pub async fn modify_position_margin(
        &self,
        symbol: &str,
        amount: Decimal,
        add: bool,
        position_side: Option<PositionSide>,
    ) -> Result<serde_json::Value> {
        self.request(
            RestRequest::post(Api::Futures, "/fapi/v1/positionMargin")
                .param("symbol", exchange_id(symbol)?)
                .decimal("amount", amount)
                .param("type", if add { 1 } else { 2 })
                .opt_param("positionSide", position_side)
                .signed(),
        )
        .await
    }

    /// `POST /fapi/v1/order` (LIVE).
    pub async fn create_futures_order(&self, order: &FuturesNewOrder) -> Result<FuturesOrder> {
        let req = order.apply(RestRequest::post(Api::Futures, "/fapi/v1/order").signed())?;
        self.request(req).await
    }

    /// `POST /fapi/v1/order/test` (validation only).
    pub async fn test_futures_order(&self, order: &FuturesNewOrder) -> Result<()> {
        let req = order.apply(
            RestRequest::post(Api::Futures, "/fapi/v1/order/test")
                .signed()
                .idempotent(),
        )?;
        self.request::<serde::de::IgnoredAny>(req).await.map(|_| ())
    }

    /// `GET /fapi/v1/order`.
    pub async fn query_futures_order(
        &self,
        symbol: &str,
        order: &OrderRef,
    ) -> Result<FuturesOrder> {
        let req = RestRequest::get(Api::Futures, "/fapi/v1/order")
            .param("symbol", exchange_id(symbol)?)
            .signed();
        self.request(order.apply(req)).await
    }

    /// `DELETE /fapi/v1/order`.
    pub async fn cancel_futures_order(
        &self,
        symbol: &str,
        order: &OrderRef,
    ) -> Result<FuturesOrder> {
        let req = RestRequest::delete(Api::Futures, "/fapi/v1/order")
            .param("symbol", exchange_id(symbol)?)
            .signed();
        self.request(order.apply(req)).await
    }

    /// `GET /fapi/v1/openOrders`.
    pub async fn futures_open_orders(&self, symbol: Option<&str>) -> Result<Vec<FuturesOrder>> {
        let weight = if symbol.is_some() { 1 } else { 40 };
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v1/openOrders")
                .opt_param("symbol", single_symbol(symbol)?)
                .signed()
                .weight(weight),
        )
        .await
    }

    /// `GET /fapi/v1/userTrades`.
    pub async fn futures_user_trades(
        &self,
        symbol: &str,
        order_id: Option<i64>,
        range: &RangeQuery,
    ) -> Result<Vec<FuturesTrade>> {
        let req = RestRequest::get(Api::Futures, "/fapi/v1/userTrades")
            .param("symbol", exchange_id(symbol)?)
            .opt_param("orderId", order_id)
            .signed()
            .weight(5);
        self.request(range.apply(req)).await
    }

    /// `GET /fapi/v1/income` (e.g. `income_type = Some("FUNDING_FEE")`).
    pub async fn futures_income(
        &self,
        symbol: Option<&str>,
        income_type: Option<&str>,
        range: &RangeQuery,
    ) -> Result<Vec<Income>> {
        let req = RestRequest::get(Api::Futures, "/fapi/v1/income")
            .opt_param("symbol", single_symbol(symbol)?)
            .opt_param("incomeType", income_type)
            .signed()
            .weight(30);
        self.request(range.apply(req)).await
    }

    /// `GET /fapi/v1/commissionRate`.
    pub async fn futures_commission_rate(&self, symbol: &str) -> Result<FuturesCommissionRate> {
        self.request(
            RestRequest::get(Api::Futures, "/fapi/v1/commissionRate")
                .param("symbol", exchange_id(symbol)?)
                .signed()
                .weight(20),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn funding_rate_tolerates_blank_mark_price() -> serde_json::Result<()> {
        let rows: Vec<FundingRate> = serde_json::from_str(
            r#"[{"symbol":"BTCUSDT","fundingRate":"-0.03750000","fundingTime":1570608000000,"markPrice":""},
                {"symbol":"BTCUSDT","fundingRate":"0.00010000","fundingTime":1570636800000,"markPrice":"34287.54619963"}]"#,
        )?;
        assert_eq!(rows[0].mark_price, None);
        assert_eq!(rows[1].mark_price, Some(dec!(34287.54619963)));
        assert_eq!(rows[0].funding_rate, dec!(-0.0375));
        Ok(())
    }

    #[test]
    fn futures_symbol_maps_to_unified_with_settle() -> serde_json::Result<()> {
        let info: FuturesSymbolInfo = serde_json::from_str(
            r#"{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","marginAsset":"USDT",
                "filters":[{"filterType":"PRICE_FILTER","minPrice":"556.80","maxPrice":"4529764","tickSize":"0.10"},
                           {"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"1000","minQty":"0.001"},
                           {"filterType":"MARKET_LOT_SIZE","stepSize":"0.001","maxQty":"120","minQty":"0.001"},
                           {"filterType":"MIN_NOTIONAL","notional":"100"}],
                "orderTypes":["LIMIT","MARKET"]}"#,
        )?;
        let rules = crate::rules::SymbolRules::from_futures(&info);
        assert_eq!(rules.unified, "BTC/USDT:USDT");
        assert_eq!(rules.min_notional(), Some(dec!(100)));
        assert_eq!(rules.round_market_qty(dec!(0.0019)), dec!(0.001));
        let dated = FuturesSymbolInfo {
            symbol: "BTCUSDT_251226".into(),
            contract_type: "CURRENT_QUARTER".into(),
            ..info
        };
        assert_eq!(
            crate::rules::SymbolRules::from_futures(&dated).unified,
            "BTC/USDT:USDT-251226"
        );
        Ok(())
    }
}

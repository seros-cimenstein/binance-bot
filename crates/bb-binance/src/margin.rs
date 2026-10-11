//! Cross / isolated margin (`/sapi/v1/margin/*`).
//!
//! `borrow_repay` and `create_margin_order` touch real funds/debt and are
//! never retried after ambiguous failures.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::models::Rows;
use crate::rest::{Api, BinanceClient, RestRequest};
use crate::spot::{NewOrder, Order, OrderRef};
use crate::symbols::exchange_id;

/// One `userAssets` row of the cross-margin account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarginAsset {
    pub asset: String,
    pub borrowed: Decimal,
    pub free: Decimal,
    pub interest: Decimal,
    pub locked: Decimal,
    pub net_asset: Decimal,
}

/// `GET /sapi/v1/margin/account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CrossMarginAccount {
    pub created: Option<bool>,
    pub borrow_enabled: bool,
    pub margin_level: Decimal,
    pub collateral_margin_level: Option<Decimal>,
    pub total_asset_of_btc: Decimal,
    pub total_liability_of_btc: Decimal,
    pub total_net_asset_of_btc: Decimal,
    #[serde(
        rename = "TotalCollateralValueInUSDT",
        alias = "totalCollateralValueInUSDT"
    )]
    pub total_collateral_value_in_usdt: Option<Decimal>,
    #[serde(rename = "totalOpenOrderLossInUSDT")]
    pub total_open_order_loss_in_usdt: Option<Decimal>,
    pub trade_enabled: bool,
    pub transfer_in_enabled: bool,
    pub transfer_out_enabled: bool,
    pub account_type: Option<String>,
    pub user_assets: Vec<MarginAsset>,
}

/// Base or quote leg of an isolated-margin pair.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IsolatedAsset {
    pub asset: String,
    pub borrow_enabled: bool,
    pub borrowed: Decimal,
    pub free: Decimal,
    pub interest: Decimal,
    pub locked: Decimal,
    pub net_asset: Decimal,
    pub net_asset_of_btc: Decimal,
    pub repay_enabled: bool,
    pub total_asset: Decimal,
}

/// One pair of `GET /sapi/v1/margin/isolated/account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IsolatedPair {
    pub symbol: String,
    pub base_asset: IsolatedAsset,
    pub quote_asset: IsolatedAsset,
    pub isolated_created: bool,
    pub enabled: bool,
    pub margin_level: Decimal,
    pub margin_level_status: String,
    pub margin_ratio: Decimal,
    pub index_price: Decimal,
    pub liquidate_price: Decimal,
    pub liquidate_rate: Decimal,
    pub trade_enabled: bool,
}

/// `GET /sapi/v1/margin/isolated/account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IsolatedMarginAccount {
    pub assets: Vec<IsolatedPair>,
    pub total_asset_of_btc: Option<Decimal>,
    pub total_liability_of_btc: Option<Decimal>,
    pub total_net_asset_of_btc: Option<Decimal>,
}

/// `GET /sapi/v1/margin/maxBorrowable`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MaxBorrowable {
    pub amount: Decimal,
    pub borrow_limit: Option<Decimal>,
}

/// `GET /sapi/v1/margin/next-hourly-interest-rate` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NextHourlyInterestRate {
    pub asset: String,
    pub next_hourly_interest_rate: Decimal,
}

/// `GET /sapi/v1/margin/interestHistory` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InterestRecord {
    pub tx_id: Option<i64>,
    pub interest_accured_time: i64,
    pub asset: String,
    pub raw_asset: Option<String>,
    pub principal: Decimal,
    pub interest: Decimal,
    pub interest_rate: Decimal,
    #[serde(rename = "type")]
    pub interest_type: String,
    pub isolated_symbol: Option<String>,
}

/// `BORROW` or `REPAY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorrowRepayType {
    Borrow,
    Repay,
}

impl BorrowRepayType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Borrow => "BORROW",
            Self::Repay => "REPAY",
        }
    }
}

/// Margin account selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarginMode {
    Cross,
    /// Isolated pair symbol (any spelling).
    Isolated(String),
}

impl MarginMode {
    fn apply(&self, req: RestRequest) -> Result<RestRequest> {
        Ok(match self {
            Self::Cross => req.param("isIsolated", "FALSE"),
            Self::Isolated(symbol) => req
                .param("isIsolated", "TRUE")
                .param("symbol", exchange_id(symbol)?),
        })
    }
}

/// `sideEffectType` for margin orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffect {
    NoSideEffect,
    MarginBuy,
    AutoRepay,
    AutoBorrowRepay,
}

impl SideEffect {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoSideEffect => "NO_SIDE_EFFECT",
            Self::MarginBuy => "MARGIN_BUY",
            Self::AutoRepay => "AUTO_REPAY",
            Self::AutoBorrowRepay => "AUTO_BORROW_REPAY",
        }
    }
}

/// `GET /sapi/v1/margin/borrow-repay` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BorrowRepayRecord {
    pub tx_id: i64,
    pub asset: String,
    pub amount: Decimal,
    pub principal: Option<Decimal>,
    pub interest: Option<Decimal>,
    pub isolated_symbol: Option<String>,
    pub timestamp: i64,
    pub status: String,
}

impl BinanceClient {
    /// `GET /sapi/v1/margin/account` (weight 10).
    pub async fn cross_margin_account(&self) -> Result<CrossMarginAccount> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/account")
                .signed()
                .weight(10),
        )
        .await
    }

    /// `GET /sapi/v1/margin/isolated/account` (≤ 5 symbols, or all; weight 10).
    pub async fn isolated_margin_account(&self, symbols: &[&str]) -> Result<IsolatedMarginAccount> {
        let ids: Vec<String> = symbols
            .iter()
            .map(|s| exchange_id(s))
            .collect::<Result<_>>()?;
        let req = RestRequest::get(Api::Spot, "/sapi/v1/margin/isolated/account")
            .signed()
            .weight(10);
        let req = if ids.is_empty() {
            req
        } else {
            req.param("symbols", ids.join(","))
        };
        self.request(req).await
    }

    /// `GET /sapi/v1/margin/maxBorrowable` (weight 50).
    pub async fn max_borrowable(
        &self,
        asset: &str,
        isolated_symbol: Option<&str>,
    ) -> Result<MaxBorrowable> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/maxBorrowable")
                .param("asset", asset.to_uppercase())
                .opt_param(
                    "isolatedSymbol",
                    isolated_symbol.map(exchange_id).transpose()?,
                )
                .signed()
                .weight(50),
        )
        .await
    }

    /// `GET /sapi/v1/margin/next-hourly-interest-rate`.
    pub async fn next_hourly_interest_rate(
        &self,
        assets: &[&str],
        isolated: bool,
    ) -> Result<Vec<NextHourlyInterestRate>> {
        let assets: Vec<String> = assets.iter().map(|a| a.to_uppercase()).collect();
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/next-hourly-interest-rate")
                .param("assets", assets.join(","))
                .param("isIsolated", if isolated { "TRUE" } else { "FALSE" })
                .signed(),
        )
        .await
    }

    /// `GET /sapi/v1/margin/interestHistory`.
    pub async fn margin_interest_history(
        &self,
        asset: Option<&str>,
        isolated_symbol: Option<&str>,
        start_time: Option<i64>,
        end_time: Option<i64>,
        size: Option<u32>,
    ) -> Result<Rows<InterestRecord>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/interestHistory")
                .opt_param("asset", asset.map(str::to_uppercase))
                .opt_param(
                    "isolatedSymbol",
                    isolated_symbol.map(exchange_id).transpose()?,
                )
                .opt_param("startTime", start_time)
                .opt_param("endTime", end_time)
                .opt_param("size", size)
                .signed(),
        )
        .await
    }

    /// `POST /sapi/v1/margin/borrow-repay` (LIVE debt change; weight 1500).
    pub async fn borrow_repay(
        &self,
        kind: BorrowRepayType,
        asset: &str,
        amount: Decimal,
        mode: &MarginMode,
    ) -> Result<crate::wallet::TransferId> {
        let req = RestRequest::post(Api::Spot, "/sapi/v1/margin/borrow-repay")
            .param("asset", asset.to_uppercase())
            .decimal("amount", amount)
            .param("type", kind.as_str())
            .signed()
            .weight(1500);
        self.request(mode.apply(req)?).await
    }

    /// `GET /sapi/v1/margin/borrow-repay` history.
    pub async fn borrow_repay_history(
        &self,
        kind: BorrowRepayType,
        asset: Option<&str>,
        isolated_symbol: Option<&str>,
        start_time: Option<i64>,
        end_time: Option<i64>,
    ) -> Result<Rows<BorrowRepayRecord>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/borrow-repay")
                .param("type", kind.as_str())
                .opt_param("asset", asset.map(str::to_uppercase))
                .opt_param(
                    "isolatedSymbol",
                    isolated_symbol.map(exchange_id).transpose()?,
                )
                .opt_param("startTime", start_time)
                .opt_param("endTime", end_time)
                .signed()
                .weight(10),
        )
        .await
    }

    /// `POST /sapi/v1/margin/order` (LIVE; weight 6).
    pub async fn create_margin_order(
        &self,
        order: &NewOrder,
        isolated: bool,
        side_effect: Option<SideEffect>,
    ) -> Result<Order> {
        let req = order.apply(
            RestRequest::post(Api::Spot, "/sapi/v1/margin/order")
                .param("isIsolated", if isolated { "TRUE" } else { "FALSE" })
                .opt_param("sideEffectType", side_effect.map(SideEffect::as_str))
                .signed()
                .weight(6),
        )?;
        self.request(req).await
    }

    /// `GET /sapi/v1/margin/order`.
    pub async fn query_margin_order(
        &self,
        symbol: &str,
        order: &OrderRef,
        isolated: bool,
    ) -> Result<Order> {
        let req = RestRequest::get(Api::Spot, "/sapi/v1/margin/order")
            .param("symbol", exchange_id(symbol)?)
            .param("isIsolated", if isolated { "TRUE" } else { "FALSE" })
            .signed()
            .weight(10);
        self.request(order.apply(req)).await
    }

    /// `DELETE /sapi/v1/margin/order`.
    pub async fn cancel_margin_order(
        &self,
        symbol: &str,
        order: &OrderRef,
        isolated: bool,
    ) -> Result<Order> {
        let req = RestRequest::delete(Api::Spot, "/sapi/v1/margin/order")
            .param("symbol", exchange_id(symbol)?)
            .param("isIsolated", if isolated { "TRUE" } else { "FALSE" })
            .signed()
            .weight(10);
        self.request(order.apply(req)).await
    }

    /// `GET /sapi/v1/margin/openOrders`.
    pub async fn margin_open_orders(
        &self,
        symbol: Option<&str>,
        isolated: bool,
    ) -> Result<Vec<Order>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/openOrders")
                .opt_param("symbol", symbol.map(exchange_id).transpose()?)
                .param("isIsolated", if isolated { "TRUE" } else { "FALSE" })
                .signed()
                .weight(10),
        )
        .await
    }
}

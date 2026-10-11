//! Wallet (`/sapi`) endpoints: coin/network config, asset balances, universal
//! transfer, withdrawals, deposits.
//!
//! `universal_transfer` and `withdraw` move real funds and are never retried
//! after ambiguous failures.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::models::Rows;
use crate::rest::{Api, BinanceClient, RestRequest};
use crate::symbols::exchange_id;

/// One network of a coin in `GET /sapi/v1/capital/config/getall`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CoinNetwork {
    pub network: String,
    pub coin: String,
    pub name: String,
    pub is_default: bool,
    pub deposit_enable: bool,
    pub withdraw_enable: bool,
    pub withdraw_fee: Decimal,
    pub withdraw_min: Decimal,
    pub withdraw_max: Decimal,
    pub withdraw_integer_multiple: Option<Decimal>,
    pub min_confirm: u32,
    pub un_lock_confirm: u32,
    pub address_regex: Option<String>,
    pub memo_regex: Option<String>,
    pub same_address: bool,
    pub busy: bool,
    pub special_tips: Option<String>,
    pub deposit_desc: Option<String>,
    pub withdraw_desc: Option<String>,
    pub contract_address: Option<String>,
}

/// `GET /sapi/v1/capital/config/getall` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CoinInfo {
    pub coin: String,
    pub name: String,
    pub deposit_all_enable: bool,
    pub withdraw_all_enable: bool,
    pub trading: bool,
    pub free: Decimal,
    pub locked: Decimal,
    pub freeze: Decimal,
    pub withdrawing: Decimal,
    pub ipoing: Decimal,
    pub ipoable: Decimal,
    pub storage: Decimal,
    pub is_legal_money: bool,
    pub network_list: Vec<CoinNetwork>,
}

/// `GET /sapi/v1/asset/wallet/balance` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WalletBalance {
    pub activate: bool,
    pub balance: Decimal,
    pub wallet_name: String,
}

/// `POST /sapi/v3/asset/getUserAsset` and `POST /sapi/v1/asset/get-funding-asset` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserAsset {
    pub asset: String,
    pub free: Decimal,
    pub locked: Decimal,
    pub freeze: Decimal,
    pub withdrawing: Decimal,
    pub ipoable: Option<Decimal>,
    pub btc_valuation: Option<Decimal>,
}

/// Universal transfer type (`MAIN_UMFUTURE`, `UMFUTURE_MAIN`, `MAIN_MARGIN`,
/// `MARGIN_MAIN`, `MAIN_FUNDING`, `FUNDING_MAIN`, `ISOLATEDMARGIN_MARGIN`, ...).
pub type TransferType = String;

/// `POST /sapi/v1/asset/transfer` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniversalTransfer {
    pub transfer_type: TransferType,
    pub asset: String,
    pub amount: Decimal,
    /// Required for `ISOLATEDMARGIN_*` sources.
    pub from_symbol: Option<String>,
    /// Required for `*_ISOLATEDMARGIN` destinations.
    pub to_symbol: Option<String>,
}

/// `{"tranId": ...}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferId {
    pub tran_id: i64,
}

/// `GET /sapi/v1/asset/transfer` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferRecord {
    pub asset: String,
    pub amount: Decimal,
    #[serde(rename = "type")]
    pub transfer_type: String,
    pub status: String,
    pub tran_id: i64,
    pub timestamp: i64,
}

/// Filters for transfer / withdraw / deposit history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryQuery {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    /// Page (transfer history) / offset (withdraw & deposit history).
    pub current: Option<u32>,
    pub size: Option<u32>,
}

/// `POST /sapi/v1/capital/withdraw/apply` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawRequest {
    pub coin: String,
    pub network: Option<String>,
    pub address: String,
    pub address_tag: Option<String>,
    pub amount: Decimal,
    pub withdraw_order_id: Option<String>,
    /// `true` = fee deducted from the amount (`transactionFeeFlag`).
    pub transaction_fee_flag: Option<bool>,
    pub name: Option<String>,
    /// 0 = spot wallet, 1 = funding wallet.
    pub wallet_type: Option<u8>,
}

/// `{"id": "..."}` withdrawal id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WithdrawId {
    pub id: String,
}

/// `GET /sapi/v1/capital/withdraw/history` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WithdrawRecord {
    pub id: String,
    pub amount: Decimal,
    pub transaction_fee: Decimal,
    pub coin: String,
    pub status: i32,
    pub address: String,
    pub address_tag: Option<String>,
    pub tx_id: Option<String>,
    pub apply_time: String,
    pub network: String,
    pub transfer_type: i32,
    pub withdraw_order_id: Option<String>,
    pub info: Option<String>,
    pub confirm_no: Option<u32>,
    pub wallet_type: Option<u8>,
    pub complete_time: Option<String>,
}

/// `GET /sapi/v1/capital/deposit/address`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DepositAddress {
    pub address: String,
    pub coin: String,
    pub tag: String,
    pub url: String,
}

/// `GET /sapi/v1/capital/deposit/hisrec` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DepositRecord {
    pub id: String,
    pub amount: Decimal,
    pub coin: String,
    pub network: String,
    pub status: i32,
    pub address: String,
    pub address_tag: Option<String>,
    pub tx_id: String,
    pub insert_time: i64,
    pub transfer_type: i32,
    pub confirm_times: Option<String>,
    pub unlock_confirm: Option<u32>,
    pub wallet_type: Option<u8>,
}

/// `GET /sapi/v1/asset/assetDetail` value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AssetDetail {
    pub min_withdraw_amount: Option<Decimal>,
    pub deposit_status: bool,
    pub withdraw_fee: Option<Decimal>,
    pub withdraw_status: bool,
    pub deposit_tip: Option<String>,
}

impl BinanceClient {
    /// `GET /sapi/v1/capital/config/getall` (weight 10).
    pub async fn coin_config(&self) -> Result<Vec<CoinInfo>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/capital/config/getall")
                .signed()
                .weight(10),
        )
        .await
    }

    /// `GET /sapi/v1/asset/assetDetail` (`asset → detail`).
    pub async fn asset_detail(
        &self,
        asset: Option<&str>,
    ) -> Result<std::collections::BTreeMap<String, AssetDetail>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/asset/assetDetail")
                .opt_param("asset", asset.map(str::to_uppercase))
                .signed(),
        )
        .await
    }

    /// `GET /sapi/v1/asset/wallet/balance` (weight 60): per-wallet totals in `quote_asset`.
    pub async fn wallet_balances(&self, quote_asset: &str) -> Result<Vec<WalletBalance>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/asset/wallet/balance")
                .param("quoteAsset", quote_asset.to_uppercase())
                .signed()
                .weight(60),
        )
        .await
    }

    /// `POST /sapi/v3/asset/getUserAsset` (read-only; weight 5).
    pub async fn user_assets(
        &self,
        asset: Option<&str>,
        need_btc_valuation: bool,
    ) -> Result<Vec<UserAsset>> {
        self.request(
            RestRequest::post(Api::Spot, "/sapi/v3/asset/getUserAsset")
                .opt_param("asset", asset.map(str::to_uppercase))
                .param("needBtcValuation", need_btc_valuation)
                .signed()
                .weight(5)
                .idempotent(),
        )
        .await
    }

    /// `POST /sapi/v1/asset/get-funding-asset` (read-only).
    pub async fn funding_assets(
        &self,
        asset: Option<&str>,
        need_btc_valuation: bool,
    ) -> Result<Vec<UserAsset>> {
        self.request(
            RestRequest::post(Api::Spot, "/sapi/v1/asset/get-funding-asset")
                .opt_param("asset", asset.map(str::to_uppercase))
                .param("needBtcValuation", need_btc_valuation)
                .signed()
                .idempotent(),
        )
        .await
    }

    /// `POST /sapi/v1/asset/transfer` (LIVE funds movement; weight 900).
    pub async fn universal_transfer(&self, transfer: &UniversalTransfer) -> Result<TransferId> {
        let req = RestRequest::post(Api::Spot, "/sapi/v1/asset/transfer")
            .param("type", &transfer.transfer_type)
            .param("asset", transfer.asset.to_uppercase())
            .decimal("amount", transfer.amount)
            .opt_param(
                "fromSymbol",
                transfer
                    .from_symbol
                    .as_deref()
                    .map(exchange_id)
                    .transpose()?,
            )
            .opt_param(
                "toSymbol",
                transfer.to_symbol.as_deref().map(exchange_id).transpose()?,
            )
            .signed()
            .weight(900);
        self.request(req).await
    }

    /// `GET /sapi/v1/asset/transfer`.
    pub async fn transfer_history(
        &self,
        transfer_type: &str,
        query: &HistoryQuery,
    ) -> Result<Rows<TransferRecord>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/asset/transfer")
                .param("type", transfer_type)
                .opt_param("startTime", query.start_time)
                .opt_param("endTime", query.end_time)
                .opt_param("current", query.current)
                .opt_param("size", query.size)
                .signed(),
        )
        .await
    }

    /// `POST /sapi/v1/capital/withdraw/apply` (LIVE on-chain withdrawal; weight 900).
    pub async fn withdraw(&self, request: &WithdrawRequest) -> Result<WithdrawId> {
        let req = RestRequest::post(Api::Spot, "/sapi/v1/capital/withdraw/apply")
            .param("coin", request.coin.to_uppercase())
            .opt_param("network", request.network.as_deref())
            .param("address", &request.address)
            .opt_param("addressTag", request.address_tag.as_deref())
            .decimal("amount", request.amount)
            .opt_param("withdrawOrderId", request.withdraw_order_id.as_deref())
            .opt_param("transactionFeeFlag", request.transaction_fee_flag)
            .opt_param("name", request.name.as_deref())
            .opt_param("walletType", request.wallet_type)
            .signed()
            .weight(900);
        self.request(req).await
    }

    /// `GET /sapi/v1/capital/withdraw/history`.
    pub async fn withdraw_history(
        &self,
        coin: Option<&str>,
        query: &HistoryQuery,
    ) -> Result<Vec<WithdrawRecord>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/capital/withdraw/history")
                .opt_param("coin", coin.map(str::to_uppercase))
                .opt_param("startTime", query.start_time)
                .opt_param("endTime", query.end_time)
                .opt_param("offset", query.current)
                .opt_param("limit", query.size)
                .signed()
                .weight(10),
        )
        .await
    }

    /// `GET /sapi/v1/capital/deposit/address`.
    pub async fn deposit_address(
        &self,
        coin: &str,
        network: Option<&str>,
    ) -> Result<DepositAddress> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/capital/deposit/address")
                .param("coin", coin.to_uppercase())
                .opt_param("network", network)
                .signed()
                .weight(10),
        )
        .await
    }

    /// `GET /sapi/v1/capital/deposit/hisrec`.
    pub async fn deposit_history(
        &self,
        coin: Option<&str>,
        query: &HistoryQuery,
    ) -> Result<Vec<DepositRecord>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/capital/deposit/hisrec")
                .opt_param("coin", coin.map(str::to_uppercase))
                .opt_param("startTime", query.start_time)
                .opt_param("endTime", query.end_time)
                .opt_param("offset", query.current)
                .opt_param("limit", query.size)
                .signed(),
        )
        .await
    }
}

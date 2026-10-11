//! Binance Convert (`/sapi/v1/convert/*`): quote → accept → status.
//!
//! `accept_quote` executes a real conversion and is never retried after
//! ambiguous failures.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{BinanceError, Result};
use crate::models::{de_i64_flexible, de_opt_i64_flexible};
use crate::rest::{Api, BinanceClient, RestRequest};

/// `GET /sapi/v1/convert/exchangeInfo` row: a convertible pair and its limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConvertPair {
    pub from_asset: String,
    pub to_asset: String,
    pub from_asset_min_amount: Decimal,
    pub from_asset_max_amount: Decimal,
    pub to_asset_min_amount: Decimal,
    pub to_asset_max_amount: Decimal,
}

/// `GET /sapi/v1/convert/assetInfo` row (amount precision per asset).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConvertAssetInfo {
    pub asset: String,
    pub fraction: u32,
}

/// Which side of the conversion the amount fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertAmount {
    /// Sell exactly this much `from_asset`.
    From(Decimal),
    /// Receive exactly this much `to_asset`.
    To(Decimal),
}

/// `POST /sapi/v1/convert/getQuote` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteRequest {
    pub from_asset: String,
    pub to_asset: String,
    pub amount: ConvertAmount,
    /// `SPOT` (default), `FUNDING`, `SPOT_FUNDING`.
    pub wallet_type: Option<String>,
    /// `10s` (default), `30s`, `1m`.
    pub valid_time: Option<String>,
}

/// `POST /sapi/v1/convert/getQuote` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConvertQuote {
    pub quote_id: String,
    pub ratio: Decimal,
    pub inverse_ratio: Decimal,
    #[serde(deserialize_with = "de_i64_flexible")]
    pub valid_timestamp: i64,
    pub to_amount: Decimal,
    pub from_amount: Decimal,
}

/// `POST /sapi/v1/convert/acceptQuote` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcceptedQuote {
    pub order_id: String,
    #[serde(deserialize_with = "de_i64_flexible")]
    pub create_time: i64,
    /// `PROCESS`, `ACCEPT_SUCCESS`, `SUCCESS`, `FAIL`.
    pub order_status: String,
}

/// `GET /sapi/v1/convert/orderStatus` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConvertOrder {
    #[serde(deserialize_with = "de_i64_flexible")]
    pub order_id: i64,
    pub order_status: String,
    pub from_asset: String,
    pub from_amount: Decimal,
    pub to_asset: String,
    pub to_amount: Decimal,
    pub ratio: Decimal,
    pub inverse_ratio: Decimal,
    #[serde(deserialize_with = "de_opt_i64_flexible")]
    pub create_time: Option<i64>,
}

impl ConvertOrder {
    /// `SUCCESS` or `FAIL`.
    pub fn is_terminal(&self) -> bool {
        matches!(self.order_status.as_str(), "SUCCESS" | "FAIL")
    }
}

/// Look up a conversion by order id or quote id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertRef {
    OrderId(String),
    QuoteId(String),
}

impl BinanceClient {
    /// `GET /sapi/v1/convert/exchangeInfo` (public data, still served from `/sapi`).
    pub async fn convert_pairs(
        &self,
        from_asset: Option<&str>,
        to_asset: Option<&str>,
    ) -> Result<Vec<ConvertPair>> {
        if from_asset.is_none() && to_asset.is_none() {
            return Err(BinanceError::InvalidRequest(
                "convert exchangeInfo requires fromAsset or toAsset".into(),
            ));
        }
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/convert/exchangeInfo")
                .opt_param("fromAsset", from_asset.map(str::to_uppercase))
                .opt_param("toAsset", to_asset.map(str::to_uppercase))
                .weight(20),
        )
        .await
    }

    /// `GET /sapi/v1/convert/assetInfo` (signed, weight 100).
    pub async fn convert_asset_info(&self) -> Result<Vec<ConvertAssetInfo>> {
        self.request(
            RestRequest::get(Api::Spot, "/sapi/v1/convert/assetInfo")
                .signed()
                .weight(100),
        )
        .await
    }

    /// `POST /sapi/v1/convert/getQuote` (no funds move until accepted; weight 200).
    pub async fn convert_quote(&self, request: &QuoteRequest) -> Result<ConvertQuote> {
        let req = RestRequest::post(Api::Spot, "/sapi/v1/convert/getQuote")
            .param("fromAsset", request.from_asset.to_uppercase())
            .param("toAsset", request.to_asset.to_uppercase());
        let req = match request.amount {
            ConvertAmount::From(v) => req.decimal("fromAmount", v),
            ConvertAmount::To(v) => req.decimal("toAmount", v),
        };
        self.request(
            req.opt_param("walletType", request.wallet_type.as_deref())
                .opt_param("validTime", request.valid_time.as_deref())
                .signed()
                .weight(200)
                .idempotent(),
        )
        .await
    }

    /// `POST /sapi/v1/convert/acceptQuote` (LIVE conversion; weight 500).
    pub async fn convert_accept(&self, quote_id: &str) -> Result<AcceptedQuote> {
        self.request(
            RestRequest::post(Api::Spot, "/sapi/v1/convert/acceptQuote")
                .param("quoteId", quote_id)
                .signed()
                .weight(500),
        )
        .await
    }

    /// `GET /sapi/v1/convert/orderStatus` (weight 100).
    pub async fn convert_order_status(&self, reference: &ConvertRef) -> Result<ConvertOrder> {
        let req = RestRequest::get(Api::Spot, "/sapi/v1/convert/orderStatus")
            .signed()
            .weight(100);
        let req = match reference {
            ConvertRef::OrderId(id) => req.param("orderId", id),
            ConvertRef::QuoteId(id) => req.param("quoteId", id),
        };
        self.request(req).await
    }
}

//! Wallet (`/sapi`) read: per-wallet balance totals.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::rest::{Api, BinanceClient, RestRequest};

/// `GET /sapi/v1/asset/wallet/balance` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WalletBalance {
    pub activate: bool,
    pub balance: Decimal,
    pub wallet_name: String,
}

impl BinanceClient {
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
}

//! Read-only margin discovery and local risk modelling (port of `src/margin.py`).
//!
//! Only `GET` margin endpoints are called; borrow/repay/order/transfer write
//! endpoints are never used and `--confirm-live-margin` always fails.

use bb_binance::{Api, BinanceClient, BinanceError, RestRequest, normalize_symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde_json::Value;

use crate::common::{
    self, exchange_symbol, fmt_dec, fmt_opt, is_stable_asset, jbool, jfloat, jtext, non_negative,
    num, num_opt, optional_positive, yes_no,
};
use crate::settings::Settings;

pub const ACCOUNT_CROSS: &str = "cross";
pub const ACCOUNT_ISOLATED: &str = "isolated";
pub const WRITE_ENDPOINTS_BLOCKED: [&str; 4] = [
    "sapiPostMarginBorrowRepay",
    "sapiPostMarginOrder",
    "sapiPostMarginTransfer",
    "sapiPostMarginIsolatedTransfer",
];

/// Raised when dry-run margin modelling cannot proceed safely.
#[derive(Debug, thiserror::Error)]
pub enum MarginModelError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Binance(#[from] BinanceError),
}

fn invalid(msg: impl Into<String>) -> MarginModelError {
    MarginModelError::Invalid(msg.into())
}

/// Margin account kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AccountType {
    Cross,
    Isolated,
}

impl AccountType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cross => ACCOUNT_CROSS,
            Self::Isolated => ACCOUNT_ISOLATED,
        }
    }
}

/// One asset balance inside a cross or isolated margin account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarginAssetBalance {
    pub asset: String,
    #[serde(with = "num")]
    pub free: Decimal,
    #[serde(with = "num")]
    pub locked: Decimal,
    #[serde(with = "num")]
    pub borrowed: Decimal,
    #[serde(with = "num")]
    pub interest: Decimal,
    #[serde(with = "num")]
    pub net_asset: Decimal,
    #[serde(with = "num")]
    pub total_asset: Decimal,
    #[serde(with = "num")]
    pub net_asset_btc: Decimal,
    pub borrow_enabled: Option<bool>,
    pub repay_enabled: Option<bool>,
}

impl MarginAssetBalance {
    pub fn liability(&self) -> Decimal {
        self.borrowed + self.interest
    }

    fn parse(raw: &Value) -> Self {
        let get = |key: &str| raw.get(key);
        let asset = jtext(get("asset")).trim().to_uppercase();
        Self {
            asset: if asset.is_empty() {
                "UNKNOWN".to_owned()
            } else {
                asset
            },
            free: non_negative(get("free")),
            locked: non_negative(get("locked")),
            borrowed: non_negative(get("borrowed")),
            interest: non_negative(get("interest")),
            net_asset: jfloat(get("netAsset")),
            total_asset: non_negative(get("totalAsset")),
            net_asset_btc: non_negative(get("netAssetOfBtc")),
            borrow_enabled: jbool(get("borrowEnabled")),
            repay_enabled: jbool(get("repayEnabled")),
        }
    }
}

/// Read-only cross-margin account snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrossMarginSnapshot {
    pub created: Option<bool>,
    pub borrow_enabled: Option<bool>,
    pub trade_enabled: Option<bool>,
    pub transfer_in_enabled: Option<bool>,
    pub transfer_out_enabled: Option<bool>,
    pub account_type: String,
    #[serde(with = "num_opt")]
    pub margin_level: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub collateral_margin_level: Option<Decimal>,
    #[serde(with = "num")]
    pub total_asset_btc: Decimal,
    #[serde(with = "num")]
    pub total_liability_btc: Decimal,
    #[serde(with = "num")]
    pub total_net_asset_btc: Decimal,
    #[serde(with = "num")]
    pub total_collateral_usdt: Decimal,
    #[serde(with = "num")]
    pub total_open_order_loss_usdt: Decimal,
    pub assets: Vec<MarginAssetBalance>,
}

impl CrossMarginSnapshot {
    pub fn asset_balance(&self, asset: &str) -> Option<&MarginAssetBalance> {
        let asset = asset.trim().to_uppercase();
        self.assets.iter().find(|b| b.asset == asset)
    }

    pub fn asset_liability(&self, asset: &str) -> Decimal {
        self.asset_balance(asset)
            .map_or(Decimal::ZERO, MarginAssetBalance::liability)
    }
}

/// Read-only isolated-margin account snapshot for one symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IsolatedMarginSnapshot {
    pub symbol: String,
    pub isolated_created: Option<bool>,
    pub enabled: Option<bool>,
    pub trade_enabled: Option<bool>,
    #[serde(with = "num_opt")]
    pub margin_level: Option<Decimal>,
    pub margin_level_status: String,
    #[serde(with = "num_opt")]
    pub margin_ratio: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub index_price: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub liquidate_price: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub liquidate_rate: Option<Decimal>,
    pub base_asset: MarginAssetBalance,
    pub quote_asset: MarginAssetBalance,
}

impl IsolatedMarginSnapshot {
    pub fn asset_balance(&self, asset: &str) -> Option<&MarginAssetBalance> {
        let asset = asset.trim().to_uppercase();
        [&self.base_asset, &self.quote_asset]
            .into_iter()
            .find(|b| b.asset == asset)
    }

    pub fn asset_liability(&self, asset: &str) -> Decimal {
        self.asset_balance(asset)
            .map_or(Decimal::ZERO, MarginAssetBalance::liability)
    }

    /// `(asset value, liability)` in USDT when the quote is a stablecoin.
    fn usdt_values(&self) -> Option<(Decimal, Decimal)> {
        let index = self.index_price?;
        if !is_stable_asset(&self.quote_asset.asset) {
            return None;
        }
        let value = self.base_asset.total_asset * index + self.quote_asset.total_asset;
        let liability = self.base_asset.liability() * index + self.quote_asset.liability();
        Some((value, liability))
    }
}

/// Combined cross and isolated margin discovery result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarginDiscovery {
    pub timestamp: String,
    pub cross: CrossMarginSnapshot,
    pub isolated: Vec<IsolatedMarginSnapshot>,
    pub live_trading_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

impl MarginDiscovery {
    pub fn isolated_snapshot(&self, symbol: &str) -> Option<&IsolatedMarginSnapshot> {
        let normalized = normalize_symbol(symbol).ok()?;
        let id = exchange_symbol(&normalized);
        self.isolated
            .iter()
            .find(|s| s.symbol == normalized || s.symbol == id)
    }
}

/// Conservative local risk limits for margin dry-run plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarginRiskLimits {
    #[serde(with = "num")]
    pub min_margin_level: Decimal,
    #[serde(with = "num")]
    pub liquidation_margin_level: Decimal,
    #[serde(with = "num")]
    pub min_liquidation_buffer_percent: Decimal,
    #[serde(with = "num")]
    pub max_borrow_utilization_percent: Decimal,
    #[serde(with = "num")]
    pub max_hourly_interest_usdt: Decimal,
    #[serde(with = "num")]
    pub max_total_liability_usdt: Decimal,
}

impl Default for MarginRiskLimits {
    fn default() -> Self {
        Self {
            min_margin_level: dec!(2.0),
            liquidation_margin_level: dec!(1.1),
            min_liquidation_buffer_percent: dec!(25.0),
            max_borrow_utilization_percent: dec!(50.0),
            max_hourly_interest_usdt: dec!(0.05),
            max_total_liability_usdt: dec!(25.0),
        }
    }
}

impl MarginRiskLimits {
    fn validate(&self) -> Result<(), MarginModelError> {
        positive(self.min_margin_level, "min_margin_level")?;
        positive(self.liquidation_margin_level, "liquidation_margin_level")?;
        positive(
            self.min_liquidation_buffer_percent,
            "min_liquidation_buffer_percent",
        )?;
        positive(
            self.max_borrow_utilization_percent,
            "max_borrow_utilization_percent",
        )?;
        if self.max_borrow_utilization_percent > dec!(100) {
            return Err(invalid("max_borrow_utilization_percent must be <= 100"));
        }
        positive(self.max_hourly_interest_usdt, "max_hourly_interest_usdt")?;
        positive(self.max_total_liability_usdt, "max_total_liability_usdt")?;
        Ok(())
    }
}

/// Requested borrow modelling inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginBorrowRequest {
    pub account_type: AccountType,
    pub asset: String,
    pub requested_amount: Decimal,
    pub isolated_symbol: Option<String>,
    pub asset_usdt_price: Option<Decimal>,
}

/// Requested repay modelling inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginRepayRequest {
    pub account_type: AccountType,
    pub asset: String,
    pub requested_amount: Decimal,
    pub isolated_symbol: Option<String>,
}

/// Dry-run borrow sizing and risk result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarginBorrowPlan {
    pub timestamp: String,
    pub account_type: AccountType,
    pub isolated_symbol: Option<String>,
    pub asset: String,
    #[serde(with = "num")]
    pub requested_amount: Decimal,
    #[serde(with = "num")]
    pub max_borrowable: Decimal,
    #[serde(with = "num")]
    pub borrow_limit: Decimal,
    #[serde(with = "num")]
    pub max_allowed_by_utilization: Decimal,
    #[serde(with = "num")]
    pub planned_borrow_amount: Decimal,
    #[serde(with = "num")]
    pub utilization_percent: Decimal,
    #[serde(with = "num")]
    pub hourly_interest_rate: Decimal,
    #[serde(with = "num")]
    pub projected_hourly_interest: Decimal,
    #[serde(with = "num_opt")]
    pub projected_hourly_interest_usdt: Option<Decimal>,
    #[serde(with = "num")]
    pub debt_before: Decimal,
    #[serde(with = "num")]
    pub projected_debt: Decimal,
    #[serde(with = "num_opt")]
    pub margin_level_before: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub margin_level_after_estimate: Option<Decimal>,
    #[serde(with = "num")]
    pub liquidation_reference_level: Decimal,
    #[serde(with = "num_opt")]
    pub liquidation_buffer_percent: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub projected_total_liability_usdt: Option<Decimal>,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub risk_limits: MarginRiskLimits,
}

/// Dry-run repay sizing result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarginRepayPlan {
    pub timestamp: String,
    pub account_type: AccountType,
    pub isolated_symbol: Option<String>,
    pub asset: String,
    #[serde(with = "num")]
    pub requested_amount: Decimal,
    #[serde(with = "num")]
    pub borrowed_amount: Decimal,
    #[serde(with = "num")]
    pub interest_amount: Decimal,
    #[serde(with = "num")]
    pub liability_amount: Decimal,
    #[serde(with = "num")]
    pub planned_repay_amount: Decimal,
    #[serde(with = "num")]
    pub remaining_liability: Decimal,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
}

/// Full dry-run margin report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarginDryRunReport {
    pub timestamp: String,
    pub mode: String,
    pub discovery: MarginDiscovery,
    pub borrow_plan: Option<MarginBorrowPlan>,
    pub repay_plan: Option<MarginRepayPlan>,
    pub live_trading_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

#[derive(Clone, Copy)]
enum Snapshot<'a> {
    Cross(&'a CrossMarginSnapshot),
    Isolated(&'a IsolatedMarginSnapshot),
}

impl<'a> Snapshot<'a> {
    fn balance(self, asset: &str) -> Option<&'a MarginAssetBalance> {
        match self {
            Self::Cross(s) => s.asset_balance(asset),
            Self::Isolated(s) => s.asset_balance(asset),
        }
    }

    fn liability(self, asset: &str) -> Decimal {
        self.balance(asset)
            .map_or(Decimal::ZERO, MarginAssetBalance::liability)
    }

    fn margin_level(self) -> Option<Decimal> {
        match self {
            Self::Cross(s) => s.margin_level,
            Self::Isolated(s) => s.margin_level,
        }
    }

    fn borrow_enabled(self, asset: &str) -> bool {
        match self {
            Self::Cross(s) => s.created != Some(false) && s.borrow_enabled != Some(false),
            Self::Isolated(s) => {
                s.enabled != Some(false)
                    && self
                        .balance(asset)
                        .is_none_or(|b| b.borrow_enabled != Some(false))
            }
        }
    }

    fn repay_enabled(self, asset: &str) -> bool {
        match self {
            Self::Cross(s) => s.created != Some(false),
            Self::Isolated(s) => {
                s.enabled != Some(false)
                    && self
                        .balance(asset)
                        .is_none_or(|b| b.repay_enabled != Some(false))
            }
        }
    }

    fn infer_usdt_price(self, asset: &str) -> Option<Decimal> {
        if is_stable_asset(asset) {
            return Some(Decimal::ONE);
        }
        match self {
            Self::Isolated(s)
                if s.base_asset.asset == asset && is_stable_asset(&s.quote_asset.asset) =>
            {
                s.index_price
            }
            _ => None,
        }
    }

    /// Scale the current margin level by the added liability.
    fn scaled_level(
        self,
        asset: &str,
        additional_usdt: Decimal,
        price: Decimal,
    ) -> Option<Decimal> {
        let before = self.margin_level();
        let before_liability = self.liability(asset) * price;
        match before {
            Some(level) if level > Decimal::ZERO && before_liability > Decimal::ZERO => {
                (level * before_liability).checked_div(before_liability + additional_usdt)
            }
            _ => before,
        }
    }

    fn margin_level_after(
        self,
        asset: &str,
        new_liability: Decimal,
        price: Option<Decimal>,
    ) -> Option<Decimal> {
        let Some(price) = price else {
            return self.margin_level();
        };
        let additional = new_liability * price;
        match self {
            Self::Cross(s) if s.total_collateral_usdt > Decimal::ZERO => {
                let liability = s.asset_liability(asset) * price + additional;
                if liability > Decimal::ZERO {
                    s.total_collateral_usdt.checked_div(liability)
                } else {
                    None
                }
            }
            Self::Cross(_) => self.scaled_level(asset, additional, price),
            Self::Isolated(s) => match s.usdt_values() {
                Some((value, liability)) => value.checked_div(liability + additional),
                None => self.scaled_level(asset, additional, price),
            },
        }
    }

    fn projected_liability_usdt(
        self,
        asset: &str,
        new_liability: Decimal,
        price: Option<Decimal>,
    ) -> Option<Decimal> {
        let price = price?;
        let additional = new_liability * price;
        Some(match self {
            Self::Isolated(s) => s.usdt_values().map_or(Decimal::ZERO, |(_, l)| l) + additional,
            Self::Cross(s) => s.asset_liability(asset) * price + additional,
        })
    }
}

/// Port of `parse_cross_margin_snapshot`.
pub fn parse_cross_margin_snapshot(raw: &Value) -> Result<CrossMarginSnapshot, MarginModelError> {
    if !raw.is_object() {
        return Err(invalid("cross margin account response was not a mapping"));
    }
    let get = |key: &str| raw.get(key);
    let assets = get("userAssets")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(MarginAssetBalance::parse).collect())
        .unwrap_or_default();
    let collateral = get("TotalCollateralValueInUSDT")
        .filter(|v| !v.is_null())
        .or_else(|| get("totalCollateralValueInUSDT"));
    let account_type = jtext(get("accountType"));
    Ok(CrossMarginSnapshot {
        created: jbool(get("created")),
        borrow_enabled: jbool(get("borrowEnabled")),
        trade_enabled: jbool(get("tradeEnabled")),
        transfer_in_enabled: jbool(get("transferInEnabled")),
        transfer_out_enabled: jbool(get("transferOutEnabled")),
        account_type: if account_type.is_empty() {
            ACCOUNT_CROSS.to_owned()
        } else {
            account_type
        },
        margin_level: optional_positive(get("marginLevel")),
        collateral_margin_level: optional_positive(get("collateralMarginLevel")),
        total_asset_btc: non_negative(get("totalAssetOfBtc")),
        total_liability_btc: non_negative(get("totalLiabilityOfBtc")),
        total_net_asset_btc: non_negative(get("totalNetAssetOfBtc")),
        total_collateral_usdt: non_negative(collateral),
        total_open_order_loss_usdt: non_negative(get("totalOpenOrderLossInUSDT")),
        assets,
    })
}

/// Port of `parse_isolated_margin_snapshots` (object with `assets` or a bare list).
pub fn parse_isolated_margin_snapshots(
    raw: &Value,
) -> Result<Vec<IsolatedMarginSnapshot>, MarginModelError> {
    let items = match raw {
        Value::Object(map) => match map.get("assets") {
            None => Some(&[][..]),
            Some(v) => v.as_array().map(Vec::as_slice),
        },
        Value::Array(items) => Some(items.as_slice()),
        _ => None,
    }
    .ok_or_else(|| invalid("isolated margin account response did not include assets"))?;
    let empty = Value::Object(serde_json::Map::new());
    let mut snapshots = Vec::new();
    for item in items.iter().filter(|i| i.is_object()) {
        let get = |key: &str| item.get(key);
        let symbol = jtext(get("symbol")).trim().to_uppercase();
        if symbol.is_empty() {
            continue;
        }
        let side = |key: &str| get(key).filter(|v| v.is_object()).unwrap_or(&empty);
        let status = jtext(get("marginLevelStatus"));
        snapshots.push(IsolatedMarginSnapshot {
            symbol,
            isolated_created: jbool(get("isolatedCreated")),
            enabled: jbool(get("enabled")),
            trade_enabled: jbool(get("tradeEnabled")),
            margin_level: optional_positive(get("marginLevel")),
            margin_level_status: if status.is_empty() {
                "unknown".to_owned()
            } else {
                status
            },
            margin_ratio: optional_positive(get("marginRatio")),
            index_price: optional_positive(get("indexPrice")),
            liquidate_price: optional_positive(get("liquidatePrice")),
            liquidate_rate: optional_positive(get("liquidateRate")),
            base_asset: MarginAssetBalance::parse(side("baseAsset")),
            quote_asset: MarginAssetBalance::parse(side("quoteAsset")),
        });
    }
    Ok(snapshots)
}

/// Discover cross and optional isolated margin accounts with read-only calls.
pub async fn discover_margin_accounts(
    client: &BinanceClient,
    include_isolated: bool,
    isolated_symbols: &[String],
) -> Result<MarginDiscovery, MarginModelError> {
    let cross_raw: Value = client
        .request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/account")
                .signed()
                .weight(10),
        )
        .await?;
    let cross = parse_cross_margin_snapshot(&cross_raw)?;
    let mut isolated = Vec::new();
    if include_isolated {
        let mut req = RestRequest::get(Api::Spot, "/sapi/v1/margin/isolated/account")
            .signed()
            .weight(10);
        if !isolated_symbols.is_empty() {
            let ids = isolated_symbols
                .iter()
                .map(|s| normalize_symbol(s).map(|n| exchange_symbol(&n)))
                .collect::<Result<Vec<_>, _>>()?;
            req = req.param("symbols", ids.join(","));
        }
        let raw: Value = client.request(req).await?;
        isolated = parse_isolated_margin_snapshots(&raw)?;
    }
    Ok(MarginDiscovery {
        timestamp: common::utc_timestamp(),
        cross,
        isolated,
        live_trading_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

/// Build a dry-run borrow plan from read-only Binance data.
pub async fn build_margin_borrow_plan(
    client: &BinanceClient,
    discovery: &MarginDiscovery,
    request: &MarginBorrowRequest,
    limits: &MarginRiskLimits,
) -> Result<MarginBorrowPlan, MarginModelError> {
    limits.validate()?;
    let asset = asset(&request.asset)?;
    let requested = positive(request.requested_amount, "requested borrow amount")?;
    let isolated_symbol =
        required_isolated_symbol(request.account_type, request.isolated_symbol.as_deref())?;

    let (max_borrowable, borrow_limit) =
        query_max_borrowable(client, &asset, isolated_symbol.as_deref()).await?;
    let hourly_rate = query_next_hourly_interest_rate(client, request.account_type, &asset).await?;
    let snapshot = account_snapshot(discovery, request.account_type, isolated_symbol.as_deref())?;
    let price = match asset_usdt_price(&asset, request.asset_usdt_price)? {
        Some(price) => Some(price),
        None => snapshot.infer_usdt_price(&asset),
    };
    let debt_before = snapshot.liability(&asset);
    let margin_level_before = snapshot.margin_level();
    let max_allowed = max_borrowable * limits.max_borrow_utilization_percent / dec!(100);
    let planned = requested.min(max_allowed);
    let utilization = if max_borrowable > Decimal::ZERO {
        planned / max_borrowable * dec!(100)
    } else {
        Decimal::ZERO
    };
    let interest = planned * hourly_rate;
    let projected_debt = debt_before + planned + interest;
    let interest_usdt = price.map(|p| interest * p);
    let projected_total = snapshot.projected_liability_usdt(&asset, planned + interest, price);
    let level_after = snapshot.margin_level_after(&asset, planned + interest, price);
    let buffer = liquidation_buffer_percent(level_after, limits.liquidation_margin_level);

    let mut warnings = Vec::new();
    let mut rejections = Vec::new();
    if requested > max_allowed && max_allowed > Decimal::ZERO {
        warnings.push("requested amount was capped by max_borrow_utilization_percent");
    }
    if max_borrowable <= Decimal::ZERO {
        rejections.push("max borrowable amount is zero");
    }
    if planned <= Decimal::ZERO {
        rejections.push("planned borrow amount is zero");
    }
    if !snapshot.borrow_enabled(&asset) {
        rejections.push("borrowing is not enabled for this margin account or asset");
    }
    if margin_level_before.is_some_and(|l| l < limits.min_margin_level) {
        rejections.push("current margin level is below the configured minimum");
    }
    if level_after.is_some_and(|l| l < limits.min_margin_level) {
        rejections.push("estimated margin level after borrow is below minimum");
    }
    if buffer.is_some_and(|b| b < limits.min_liquidation_buffer_percent) {
        rejections.push("estimated liquidation buffer is below minimum");
    }
    if interest_usdt.is_some_and(|i| i > limits.max_hourly_interest_usdt) {
        rejections.push("projected hourly interest exceeds the configured cap");
    }
    if projected_total.is_some_and(|t| t > limits.max_total_liability_usdt) {
        rejections.push("projected total liability exceeds the configured cap");
    }
    if price.is_none() {
        rejections.push("asset_usdt_price is required for non-stable asset liability caps");
        warnings.push("planned borrow value in USDT was not calculated");
    }

    Ok(MarginBorrowPlan {
        timestamp: common::utc_timestamp(),
        account_type: request.account_type,
        isolated_symbol,
        asset,
        requested_amount: requested,
        max_borrowable,
        borrow_limit,
        max_allowed_by_utilization: max_allowed,
        planned_borrow_amount: planned,
        utilization_percent: utilization,
        hourly_interest_rate: hourly_rate,
        projected_hourly_interest: interest,
        projected_hourly_interest_usdt: interest_usdt,
        debt_before,
        projected_debt,
        margin_level_before,
        margin_level_after_estimate: level_after,
        liquidation_reference_level: limits.liquidation_margin_level,
        liquidation_buffer_percent: buffer,
        projected_total_liability_usdt: projected_total,
        allowed: rejections.is_empty(),
        rejection_reasons: rejections.into_iter().map(str::to_owned).collect(),
        warnings: warnings.into_iter().map(str::to_owned).collect(),
        risk_limits: limits.clone(),
    })
}

/// Build a dry-run repay plan without submitting a repay request.
pub fn build_margin_repay_plan(
    discovery: &MarginDiscovery,
    request: &MarginRepayRequest,
) -> Result<MarginRepayPlan, MarginModelError> {
    let asset = asset(&request.asset)?;
    let requested = positive(request.requested_amount, "requested repay amount")?;
    let isolated_symbol =
        required_isolated_symbol(request.account_type, request.isolated_symbol.as_deref())?;
    let snapshot = account_snapshot(discovery, request.account_type, isolated_symbol.as_deref())?;
    let balance = snapshot.balance(&asset);
    let borrowed = balance.map_or(Decimal::ZERO, |b| b.borrowed);
    let interest = balance.map_or(Decimal::ZERO, |b| b.interest);
    let liability = borrowed + interest;
    let planned = requested.min(liability);
    let mut rejections = Vec::new();
    if liability <= Decimal::ZERO {
        rejections.push("no outstanding liability exists for this asset".to_owned());
    }
    if planned <= Decimal::ZERO {
        rejections.push("planned repay amount is zero".to_owned());
    }
    if !snapshot.repay_enabled(&asset) {
        rejections.push("repay is not enabled for this margin account or asset".to_owned());
    }
    Ok(MarginRepayPlan {
        timestamp: common::utc_timestamp(),
        account_type: request.account_type,
        isolated_symbol,
        asset,
        requested_amount: requested,
        borrowed_amount: borrowed,
        interest_amount: interest,
        liability_amount: liability,
        planned_repay_amount: planned,
        remaining_liability: (liability - planned).max(Decimal::ZERO),
        allowed: rejections.is_empty(),
        rejection_reasons: rejections,
    })
}

/// Inputs for [`build_margin_dry_run_report`].
#[derive(Debug, Clone, Default)]
pub struct MarginDryRunInputs {
    pub include_isolated: bool,
    pub isolated_symbols: Vec<String>,
    pub borrow_request: Option<MarginBorrowRequest>,
    pub repay_request: Option<MarginRepayRequest>,
    pub limits: MarginRiskLimits,
}

/// Build a full dry-run report from read-only margin endpoints.
pub async fn build_margin_dry_run_report(
    settings: &Settings,
    client: &BinanceClient,
    inputs: &MarginDryRunInputs,
) -> Result<MarginDryRunReport, MarginModelError> {
    let discovery =
        discover_margin_accounts(client, inputs.include_isolated, &inputs.isolated_symbols).await?;
    let borrow_plan = match &inputs.borrow_request {
        Some(request) => {
            Some(build_margin_borrow_plan(client, &discovery, request, &inputs.limits).await?)
        }
        None => None,
    };
    let repay_plan = inputs
        .repay_request
        .as_ref()
        .map(|request| build_margin_repay_plan(&discovery, request))
        .transpose()?;
    Ok(MarginDryRunReport {
        timestamp: common::utc_timestamp(),
        mode: common::mode(settings).to_owned(),
        discovery,
        borrow_plan,
        repay_plan,
        live_trading_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

/// Fail closed for any attempted live margin activation.
pub fn assert_margin_live_disabled(confirm_live_margin: bool) -> Result<(), MarginModelError> {
    if confirm_live_margin {
        return Err(invalid(
            "live margin activation is not implemented; Phase 19 is dry-run only",
        ));
    }
    Ok(())
}

/// Concise human-readable margin dry-run report.
pub fn render_margin_report(report: &MarginDryRunReport) -> String {
    let cross = &report.discovery.cross;
    let mut lines = vec![
        format!("Mode: {}", report.mode),
        "Live margin trading: disabled".to_owned(),
        "Write endpoints used: none".to_owned(),
        format!("Cross account type: {}", cross.account_type),
        format!("Cross margin level: {}", fmt_opt(cross.margin_level)),
        format!(
            "Cross total liability BTC: {}",
            fmt_dec(cross.total_liability_btc)
        ),
        format!("Isolated snapshots: {}", report.discovery.isolated.len()),
    ];
    if let Some(borrow) = &report.borrow_plan {
        lines.extend([
            "Borrow plan:".to_owned(),
            format!("  - asset: {}", borrow.asset),
            format!(
                "  - planned amount: {}",
                fmt_dec(borrow.planned_borrow_amount)
            ),
            format!("  - allowed: {}", yes_no(borrow.allowed)),
            format!(
                "  - estimated margin level: {}",
                fmt_opt(borrow.margin_level_after_estimate)
            ),
        ]);
        if !borrow.rejection_reasons.is_empty() {
            lines.push(format!(
                "  - rejection reasons: {}",
                borrow.rejection_reasons.join(", ")
            ));
        }
        if !borrow.warnings.is_empty() {
            lines.push(format!("  - warnings: {}", borrow.warnings.join(", ")));
        }
    }
    if let Some(repay) = &report.repay_plan {
        lines.extend([
            "Repay plan:".to_owned(),
            format!("  - asset: {}", repay.asset),
            format!(
                "  - planned amount: {}",
                fmt_dec(repay.planned_repay_amount)
            ),
            format!(
                "  - remaining liability: {}",
                fmt_dec(repay.remaining_liability)
            ),
            format!("  - allowed: {}", yes_no(repay.allowed)),
        ]);
        if !repay.rejection_reasons.is_empty() {
            lines.push(format!(
                "  - rejection reasons: {}",
                repay.rejection_reasons.join(", ")
            ));
        }
    }
    lines.join("\n")
}

async fn query_max_borrowable(
    client: &BinanceClient,
    asset: &str,
    isolated_symbol: Option<&str>,
) -> Result<(Decimal, Decimal), MarginModelError> {
    let mut req = RestRequest::get(Api::Spot, "/sapi/v1/margin/maxBorrowable")
        .param("asset", asset)
        .signed()
        .weight(50);
    if let Some(symbol) = isolated_symbol {
        req = req.param("isolatedSymbol", exchange_symbol(symbol));
    }
    let payload: Value = client.request(req).await?;
    if !payload.is_object() {
        return Err(invalid("max borrow response was not a mapping"));
    }
    Ok((
        non_negative(payload.get("amount")),
        non_negative(payload.get("borrowLimit")),
    ))
}

async fn query_next_hourly_interest_rate(
    client: &BinanceClient,
    account_type: AccountType,
    asset: &str,
) -> Result<Decimal, MarginModelError> {
    let payload: Value = client
        .request(
            RestRequest::get(Api::Spot, "/sapi/v1/margin/next-hourly-interest-rate")
                .param("assets", asset)
                .param(
                    "isIsolated",
                    if account_type == AccountType::Isolated {
                        "TRUE"
                    } else {
                        "FALSE"
                    },
                )
                .signed()
                .weight(100),
        )
        .await?;
    if !payload.is_object() && !payload.is_array() {
        return Err(invalid("next hourly interest response was not usable"));
    }
    for item in common::rows(&payload).into_iter().filter(|i| i.is_object()) {
        let item_asset = jtext(item.get("asset")).trim().to_uppercase();
        if item_asset.is_empty() || item_asset == asset {
            return Ok(non_negative(item.get("nextHourlyInterestRate")));
        }
    }
    Ok(Decimal::ZERO)
}

fn account_snapshot<'a>(
    discovery: &'a MarginDiscovery,
    account_type: AccountType,
    isolated_symbol: Option<&str>,
) -> Result<Snapshot<'a>, MarginModelError> {
    match account_type {
        AccountType::Cross => Ok(Snapshot::Cross(&discovery.cross)),
        AccountType::Isolated => {
            let symbol = isolated_symbol
                .ok_or_else(|| invalid("isolated_symbol is required for isolated margin"))?;
            discovery
                .isolated_snapshot(symbol)
                .map(Snapshot::Isolated)
                .ok_or_else(|| invalid(format!("isolated margin snapshot not found for {symbol}")))
        }
    }
}

fn liquidation_buffer_percent(
    level: Option<Decimal>,
    liquidation_level: Decimal,
) -> Option<Decimal> {
    let level = level?;
    if liquidation_level <= Decimal::ZERO {
        return None;
    }
    Some((level - liquidation_level) / liquidation_level * dec!(100))
}

fn asset_usdt_price(
    asset: &str,
    value: Option<Decimal>,
) -> Result<Option<Decimal>, MarginModelError> {
    if is_stable_asset(asset) {
        return Ok(Some(Decimal::ONE));
    }
    value.map(|v| positive(v, "asset_usdt_price")).transpose()
}

fn required_isolated_symbol(
    account_type: AccountType,
    symbol: Option<&str>,
) -> Result<Option<String>, MarginModelError> {
    if account_type != AccountType::Isolated {
        return Ok(None);
    }
    match symbol.map(str::trim).filter(|s| !s.is_empty()) {
        Some(symbol) => Ok(Some(normalize_symbol(symbol)?)),
        None => Err(invalid("isolated_symbol is required for isolated margin")),
    }
}

/// Upper-cased, non-blank asset symbol.
pub fn asset(value: &str) -> Result<String, MarginModelError> {
    let text = value.trim().to_uppercase();
    if text.is_empty() {
        return Err(invalid("asset must not be blank"));
    }
    Ok(text)
}

fn positive(value: Decimal, field: &str) -> Result<Decimal, MarginModelError> {
    if value <= Decimal::ZERO {
        return Err(invalid(format!("{field} must be greater than zero")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::testutil::{mock_client, mount_get, requests_to, settings_builder};

    pub(crate) fn cross_payload(margin_level: &str, collateral: &str) -> Value {
        json!({
            "created": true, "borrowEnabled": true, "tradeEnabled": true,
            "transferInEnabled": true, "transferOutEnabled": true, "accountType": "MARGIN_1",
            "marginLevel": margin_level, "collateralMarginLevel": margin_level,
            "totalAssetOfBtc": "0.01000000", "totalLiabilityOfBtc": "0.00100000",
            "totalNetAssetOfBtc": "0.00900000", "TotalCollateralValueInUSDT": collateral,
            "totalOpenOrderLossInUSDT": "0",
            "userAssets": [
                {"asset": "USDT", "free": "20", "locked": "0", "borrowed": "5", "interest": "0.01", "netAsset": "14.99"},
                {"asset": "BNB", "free": "0", "locked": "0", "borrowed": "0", "interest": "0", "netAsset": "0"}
            ]
        })
    }

    pub(crate) fn isolated_payload() -> Value {
        json!({"assets": [{
            "symbol": "BNBUSDT", "isolatedCreated": true, "enabled": true, "tradeEnabled": true,
            "marginLevel": "2.8", "marginLevelStatus": "NORMAL", "marginRatio": "1.2",
            "indexPrice": "600", "liquidatePrice": "420", "liquidateRate": "1.1",
            "baseAsset": {"asset": "BNB", "borrowEnabled": true, "repayEnabled": true, "free": "0.09", "locked": "0",
                "borrowed": "0.01", "interest": "0.0001", "netAsset": "0.0799", "netAssetOfBtc": "0.001", "totalAsset": "0.1"},
            "quoteAsset": {"asset": "USDT", "borrowEnabled": true, "repayEnabled": true, "free": "40", "locked": "0",
                "borrowed": "0", "interest": "0", "netAsset": "40", "netAssetOfBtc": "0.0005", "totalAsset": "40"}
        }]})
    }

    pub(crate) async fn mount_margin(
        server: &MockServer,
        cross: Value,
        max_borrow: Value,
        interest: Value,
    ) {
        mount_get(server, "/sapi/v1/margin/account", cross).await;
        mount_get(
            server,
            "/sapi/v1/margin/isolated/account",
            isolated_payload(),
        )
        .await;
        mount_get(server, "/sapi/v1/margin/maxBorrowable", max_borrow).await;
        mount_get(
            server,
            "/sapi/v1/margin/next-hourly-interest-rate",
            interest,
        )
        .await;
    }

    fn default_interest() -> Value {
        json!([{"asset": "USDT", "nextHourlyInterestRate": "0.00001"}, {"asset": "BNB", "nextHourlyInterestRate": "0.00002"}])
    }

    fn limits(
        min_level: Decimal,
        buffer: Decimal,
        util: Decimal,
        interest: Decimal,
        liability: Decimal,
    ) -> MarginRiskLimits {
        MarginRiskLimits {
            min_margin_level: min_level,
            liquidation_margin_level: dec!(1.1),
            min_liquidation_buffer_percent: buffer,
            max_borrow_utilization_percent: util,
            max_hourly_interest_usdt: interest,
            max_total_liability_usdt: liability,
        }
    }

    fn borrow(account_type: AccountType, asset: &str, amount: Decimal) -> MarginBorrowRequest {
        MarginBorrowRequest {
            account_type,
            asset: asset.into(),
            requested_amount: amount,
            isolated_symbol: None,
            asset_usdt_price: None,
        }
    }

    async fn setup(
        cross: Value,
        max_borrow: Value,
        interest: Value,
    ) -> (MockServer, BinanceClient) {
        let server = MockServer::start().await;
        mount_margin(&server, cross, max_borrow, interest).await;
        let client = mock_client(&server);
        (server, client)
    }

    #[tokio::test]
    async fn discovery_reads_cross_and_isolated_without_writes() {
        let (server, client) = setup(
            cross_payload("3.2", "100"),
            json!({"amount": "50", "borrowLimit": "100"}),
            default_interest(),
        )
        .await;
        let discovery = discover_margin_accounts(&client, true, &["BNB/USDT".into()])
            .await
            .unwrap();
        assert!(!discovery.live_trading_enabled);
        assert!(discovery.write_endpoints_used.is_empty());
        assert_eq!(discovery.cross.account_type, "MARGIN_1");
        assert_eq!(discovery.cross.asset_liability("USDT"), dec!(5.01));
        assert!(discovery.isolated_snapshot("BNB/USDT").is_some());
        let isolated = requests_to(&server, "/sapi/v1/margin/isolated/account").await;
        assert_eq!(isolated.len(), 1);
        assert!(isolated[0].contains("symbols=BNBUSDT"), "{}", isolated[0]);
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method.as_str() == "GET")
        );
    }

    #[tokio::test]
    async fn borrow_plan_caps_requested_amount_and_models_interest() {
        let (_server, client) = setup(
            cross_payload("3.2", "100"),
            json!({"amount": "50", "borrowLimit": "100"}),
            default_interest(),
        )
        .await;
        let discovery = discover_margin_accounts(&client, false, &[]).await.unwrap();
        let plan = build_margin_borrow_plan(
            &client,
            &discovery,
            &borrow(AccountType::Cross, "USDT", dec!(30)),
            &limits(dec!(1.5), dec!(10), dec!(20), dec!(1), dec!(1000)),
        )
        .await
        .unwrap();
        assert!(plan.allowed, "{:?}", plan.rejection_reasons);
        assert_eq!(plan.max_borrowable, dec!(50));
        assert_eq!(plan.max_allowed_by_utilization, dec!(10));
        assert_eq!(plan.planned_borrow_amount, dec!(10));
        assert_eq!(plan.hourly_interest_rate, dec!(0.00001));
        assert_eq!(plan.projected_hourly_interest, dec!(0.0001));
        assert_eq!(plan.projected_total_liability_usdt, Some(dec!(15.0101)));
        assert_eq!(plan.debt_before, dec!(5.01));
        assert!(plan.warnings[0].contains("requested amount was capped"));
    }

    #[tokio::test]
    async fn borrow_plan_rejects_interest_and_liquidation_risk() {
        let (_server, client) = setup(
            cross_payload("1.2", "12"),
            json!({"amount": "50", "borrowLimit": "100"}),
            json!([{"asset": "USDT", "nextHourlyInterestRate": "0.01"}]),
        )
        .await;
        let discovery = discover_margin_accounts(&client, false, &[]).await.unwrap();
        let plan = build_margin_borrow_plan(
            &client,
            &discovery,
            &borrow(AccountType::Cross, "USDT", dec!(10)),
            &limits(dec!(2), dec!(25), dec!(100), dec!(0.05), dec!(1000)),
        )
        .await
        .unwrap();
        assert!(!plan.allowed);
        for reason in [
            "current margin level is below the configured minimum",
            "estimated margin level after borrow is below minimum",
            "estimated liquidation buffer is below minimum",
            "projected hourly interest exceeds the configured cap",
        ] {
            assert!(
                plan.rejection_reasons.iter().any(|r| r == reason),
                "{reason}"
            );
        }
    }

    #[tokio::test]
    async fn cross_non_stable_borrow_requires_usdt_price() {
        let (_server, client) = setup(
            cross_payload("3.2", "100"),
            json!({"amount": "50", "borrowLimit": "100"}),
            default_interest(),
        )
        .await;
        let discovery = discover_margin_accounts(&client, false, &[]).await.unwrap();
        let plan = build_margin_borrow_plan(
            &client,
            &discovery,
            &borrow(AccountType::Cross, "BNB", dec!(1)),
            &limits(dec!(1.5), dec!(10), dec!(100), dec!(1), dec!(1000)),
        )
        .await
        .unwrap();
        assert!(!plan.allowed);
        assert!(plan.rejection_reasons.contains(
            &"asset_usdt_price is required for non-stable asset liability caps".to_owned()
        ));
    }

    #[tokio::test]
    async fn isolated_borrow_plan_uses_pair_values() {
        let (server, client) = setup(
            cross_payload("3.2", "100"),
            json!({"amount": "0.05", "borrowLimit": "1"}),
            default_interest(),
        )
        .await;
        let discovery = discover_margin_accounts(&client, true, &["BNB/USDT".into()])
            .await
            .unwrap();
        let request = MarginBorrowRequest {
            isolated_symbol: Some("BNB/USDT".into()),
            asset_usdt_price: Some(dec!(600)),
            ..borrow(AccountType::Isolated, "BNB", dec!(0.02))
        };
        let plan = build_margin_borrow_plan(
            &client,
            &discovery,
            &request,
            &limits(dec!(2), dec!(25), dec!(100), dec!(1), dec!(100)),
        )
        .await
        .unwrap();
        assert!(plan.allowed, "{:?}", plan.rejection_reasons);
        assert_eq!(plan.planned_borrow_amount, dec!(0.02));
        let expected = dec!(100) / dec!(18.06024);
        let got = plan.margin_level_after_estimate.unwrap();
        assert!(
            (got - expected).abs() < dec!(0.0000001),
            "{got} vs {expected}"
        );
        let max_borrow = requests_to(&server, "/sapi/v1/margin/maxBorrowable").await;
        assert!(
            max_borrow[0].contains("asset=BNB") && max_borrow[0].contains("isolatedSymbol=BNBUSDT")
        );
        let interest = requests_to(&server, "/sapi/v1/margin/next-hourly-interest-rate").await;
        assert!(interest[0].contains("isIsolated=TRUE"));
    }

    #[tokio::test]
    async fn dry_run_report_never_enables_live_trading_and_repay_caps() {
        let (_server, client) = setup(
            cross_payload("3.2", "100"),
            json!({"amount": "50", "borrowLimit": "100"}),
            default_interest(),
        )
        .await;
        let mut builder = settings_builder();
        builder.use_testnet = false;
        let settings = builder.build().unwrap();
        let inputs = MarginDryRunInputs {
            borrow_request: Some(borrow(AccountType::Cross, "USDT", dec!(1))),
            repay_request: Some(MarginRepayRequest {
                account_type: AccountType::Cross,
                asset: "USDT".into(),
                requested_amount: dec!(6),
                isolated_symbol: None,
            }),
            limits: limits(dec!(1.5), dec!(10), dec!(50), dec!(1), dec!(1000)),
            ..MarginDryRunInputs::default()
        };
        let report = build_margin_dry_run_report(&settings, &client, &inputs)
            .await
            .unwrap();
        assert_eq!(report.mode, "LIVE");
        assert!(!report.live_trading_enabled);
        assert!(report.write_endpoints_used.is_empty());
        assert!(report.borrow_plan.is_some());
        let repay = report.repay_plan.as_ref().unwrap();
        assert!(repay.allowed);
        assert_eq!(repay.borrowed_amount, dec!(5));
        assert_eq!(repay.interest_amount, dec!(0.01));
        assert_eq!(repay.planned_repay_amount, dec!(5.01));
        assert_eq!(repay.remaining_liability, Decimal::ZERO);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["repay_plan"]["planned_repay_amount"], json!(5.01));
        assert_eq!(json["borrow_plan"]["account_type"], "cross");
        let text = render_margin_report(&report);
        assert!(text.contains("Repay plan:"));
        assert!(text.contains("Cross margin level: 3.2"));
    }

    #[test]
    fn live_activation_fails_closed() {
        let err = assert_margin_live_disabled(true).unwrap_err();
        assert!(
            err.to_string()
                .contains("live margin activation is not implemented")
        );
        assert!(assert_margin_live_disabled(false).is_ok());
        let bad = MarginRiskLimits {
            max_borrow_utilization_percent: dec!(101),
            ..MarginRiskLimits::default()
        };
        assert!(bad.validate().is_err());
    }
}

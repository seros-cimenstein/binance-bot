//! Read-only wallet discovery and local internal-transfer planning
//! (port of `src/transfers.py`). Universal transfers are never submitted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bb_binance::{Api, BinanceClient, BinanceError, RestRequest, normalize_symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde_json::Value;

use crate::common::{
    self, append_audit, exchange_symbol, fmt_dec, is_stable_asset, jbool, jfloat, jtext, num,
    num_opt, yes_no,
};
use crate::settings::Settings;

pub const WALLET_SPOT: &str = "spot";
pub const WALLET_FUNDING: &str = "funding";
pub const WALLET_CROSS_MARGIN: &str = "cross_margin";
pub const WALLET_ISOLATED_MARGIN: &str = "isolated_margin";
pub const WALLET_USD_M_FUTURES: &str = "usd_m_futures";
pub const ALL_WALLETS: [&str; 5] = [
    WALLET_SPOT,
    WALLET_FUNDING,
    WALLET_CROSS_MARGIN,
    WALLET_ISOLATED_MARGIN,
    WALLET_USD_M_FUTURES,
];
pub const WRITE_ENDPOINTS_BLOCKED: [&str; 1] = ["sapiPostAssetTransfer"];
pub const DEFAULT_AUDIT_PATH: &str = "data/transfer-plans.jsonl";

/// Binance universal-transfer type for a `(source, destination)` wallet pair.
pub fn transfer_route(source: &str, destination: &str) -> Option<&'static str> {
    Some(match (source, destination) {
        (WALLET_SPOT, WALLET_USD_M_FUTURES) => "MAIN_UMFUTURE",
        (WALLET_USD_M_FUTURES, WALLET_SPOT) => "UMFUTURE_MAIN",
        (WALLET_SPOT, WALLET_CROSS_MARGIN) => "MAIN_MARGIN",
        (WALLET_CROSS_MARGIN, WALLET_SPOT) => "MARGIN_MAIN",
        (WALLET_CROSS_MARGIN, WALLET_USD_M_FUTURES) => "MARGIN_UMFUTURE",
        (WALLET_USD_M_FUTURES, WALLET_CROSS_MARGIN) => "UMFUTURE_MARGIN",
        (WALLET_SPOT, WALLET_FUNDING) => "MAIN_FUNDING",
        (WALLET_FUNDING, WALLET_SPOT) => "FUNDING_MAIN",
        (WALLET_FUNDING, WALLET_USD_M_FUTURES) => "FUNDING_UMFUTURE",
        (WALLET_USD_M_FUTURES, WALLET_FUNDING) => "UMFUTURE_FUNDING",
        (WALLET_CROSS_MARGIN, WALLET_FUNDING) => "MARGIN_FUNDING",
        (WALLET_FUNDING, WALLET_CROSS_MARGIN) => "FUNDING_MARGIN",
        (WALLET_ISOLATED_MARGIN, WALLET_CROSS_MARGIN) => "ISOLATEDMARGIN_MARGIN",
        (WALLET_CROSS_MARGIN, WALLET_ISOLATED_MARGIN) => "MARGIN_ISOLATEDMARGIN",
        (WALLET_ISOLATED_MARGIN, WALLET_ISOLATED_MARGIN) => "ISOLATEDMARGIN_ISOLATEDMARGIN",
        _ => return None,
    })
}

/// Raised when dry-run transfer planning cannot proceed safely.
#[derive(Debug, thiserror::Error)]
pub enum TransferPlanError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Binance(#[from] BinanceError),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

fn invalid(msg: impl Into<String>) -> TransferPlanError {
    TransferPlanError::Invalid(msg.into())
}

/// Wallet summary row from Binance's wallet-balance endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WalletActivation {
    pub wallet: String,
    pub wallet_name: String,
    pub active: Option<bool>,
    #[serde(with = "num")]
    pub quote_balance: Decimal,
}

/// Asset balance for one wallet used by transfer planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WalletAssetBalance {
    pub wallet: String,
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
    pub available: Decimal,
    #[serde(with = "num")]
    pub wallet_balance: Decimal,
    #[serde(with = "num")]
    pub btc_valuation: Decimal,
}

impl WalletAssetBalance {
    pub fn empty(wallet: &str, asset: &str) -> Self {
        Self {
            wallet: wallet.to_owned(),
            asset: asset.to_owned(),
            free: Decimal::ZERO,
            locked: Decimal::ZERO,
            borrowed: Decimal::ZERO,
            interest: Decimal::ZERO,
            available: Decimal::ZERO,
            wallet_balance: Decimal::ZERO,
            btc_valuation: Decimal::ZERO,
        }
    }

    pub fn liability(&self) -> Decimal {
        self.borrowed + self.interest
    }

    pub fn transferable_free(&self) -> Decimal {
        if self.available > Decimal::ZERO {
            self.available
        } else {
            self.free
        }
    }
}

/// Read-only universal-transfer history row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferHistoryRecord {
    pub transfer_type: String,
    pub asset: String,
    #[serde(with = "num")]
    pub amount: Decimal,
    pub status: String,
    pub transaction_id: String,
    pub timestamp_ms: Option<i64>,
}

/// Combined read-only wallet state for transfer planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferDiscovery {
    pub timestamp: String,
    pub asset: String,
    pub quote_asset: String,
    pub wallet_activations: Vec<WalletActivation>,
    pub balances: Vec<WalletAssetBalance>,
    pub recent_transfers: Vec<TransferHistoryRecord>,
    pub live_transfers_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

impl TransferDiscovery {
    pub fn activation(&self, wallet: &str) -> Option<&WalletActivation> {
        self.wallet_activations.iter().find(|a| a.wallet == wallet)
    }

    pub fn balance(&self, wallet: &str, asset: &str) -> WalletAssetBalance {
        self.balances
            .iter()
            .find(|b| b.wallet == wallet && b.asset == asset)
            .cloned()
            .unwrap_or_else(|| WalletAssetBalance::empty(wallet, asset))
    }
}

/// Conservative local limits for one dry-run transfer plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferRiskLimits {
    #[serde(with = "num")]
    pub max_transfer_amount: Decimal,
    #[serde(with = "num")]
    pub max_transfer_value_usdt: Decimal,
    #[serde(with = "num")]
    pub min_source_free_after: Decimal,
    pub require_active_wallets: bool,
    pub block_margin_debt_transfer_out: bool,
    pub allowed_wallets: Vec<String>,
}

impl Default for TransferRiskLimits {
    fn default() -> Self {
        Self {
            max_transfer_amount: dec!(10.0),
            max_transfer_value_usdt: dec!(25.0),
            min_source_free_after: dec!(0.0),
            require_active_wallets: true,
            block_margin_debt_transfer_out: true,
            allowed_wallets: ALL_WALLETS.iter().map(|w| (*w).to_owned()).collect(),
        }
    }
}

impl TransferRiskLimits {
    fn validate(&self) -> Result<(), TransferPlanError> {
        positive(self.max_transfer_amount, "max_transfer_amount")?;
        positive(self.max_transfer_value_usdt, "max_transfer_value_usdt")?;
        if self.min_source_free_after < Decimal::ZERO {
            return Err(invalid("min_source_free_after must be at least zero"));
        }
        for wallet in &self.allowed_wallets {
            normalize_wallet(wallet)?;
        }
        Ok(())
    }
}

/// Requested internal transfer modelling inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub source_wallet: String,
    pub destination_wallet: String,
    pub asset: String,
    pub amount: Decimal,
    pub reason: String,
    pub asset_usdt_price: Option<Decimal>,
    pub from_symbol: Option<String>,
    pub to_symbol: Option<String>,
}

impl TransferRequest {
    pub fn new(source: &str, destination: &str, asset: &str, amount: Decimal) -> Self {
        Self {
            source_wallet: source.to_owned(),
            destination_wallet: destination.to_owned(),
            asset: asset.to_owned(),
            amount,
            reason: "manual dry-run transfer plan".to_owned(),
            asset_usdt_price: None,
            from_symbol: None,
            to_symbol: None,
        }
    }
}

/// Free balance before/after for one wallet in the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllocationRow {
    pub asset: String,
    #[serde(with = "num")]
    pub free_before: Decimal,
    #[serde(with = "num")]
    pub free_after: Decimal,
}

/// Dry-run transfer plan and resulting allocation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferPlan {
    pub timestamp: String,
    pub source_wallet: String,
    pub destination_wallet: String,
    pub transfer_type: Option<String>,
    pub asset: String,
    #[serde(with = "num")]
    pub amount: Decimal,
    #[serde(with = "num_opt")]
    pub asset_usdt_price: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub transfer_value_usdt: Option<Decimal>,
    pub reason: String,
    pub from_symbol: Option<String>,
    pub to_symbol: Option<String>,
    #[serde(with = "num")]
    pub source_free_before: Decimal,
    #[serde(with = "num")]
    pub source_free_after: Decimal,
    #[serde(with = "num")]
    pub destination_free_before: Decimal,
    #[serde(with = "num")]
    pub destination_free_after: Decimal,
    pub source_active: Option<bool>,
    pub destination_active: Option<bool>,
    pub submission_attempted: bool,
    pub submitted: bool,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub resulting_allocation: BTreeMap<String, AllocationRow>,
    pub risk_limits: TransferRiskLimits,
}

/// Full dry-run transfer report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferDryRunReport {
    pub timestamp: String,
    pub mode: String,
    pub discovery: TransferDiscovery,
    pub plan: Option<TransferPlan>,
    pub audit_path: String,
    pub live_transfers_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

/// What to read during discovery.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryScope<'a> {
    pub history_type: Option<&'a str>,
    pub include_margin: bool,
    pub include_futures: bool,
}

impl DiscoveryScope<'_> {
    /// Scope implied by a source/destination wallet pair.
    pub fn for_route(source: &str, destination: &str) -> Self {
        let is_margin = |w: &str| w == WALLET_CROSS_MARGIN || w == WALLET_ISOLATED_MARGIN;
        Self {
            history_type: transfer_route(source, destination),
            include_margin: is_margin(source) || is_margin(destination),
            include_futures: source == WALLET_USD_M_FUTURES || destination == WALLET_USD_M_FUTURES,
        }
    }
}

/// Discover transfer-relevant wallet balances with read-only endpoints.
pub async fn discover_transfer_wallets(
    client: &BinanceClient,
    asset: &str,
    quote_asset: &str,
    scope: DiscoveryScope<'_>,
) -> Result<TransferDiscovery, TransferPlanError> {
    let asset = asset_symbol(asset)?;
    let quote = asset_symbol(quote_asset)?;
    let summary: Value = client
        .request(
            RestRequest::get(Api::Spot, "/sapi/v1/asset/wallet/balance")
                .param("quoteAsset", &quote)
                .signed()
                .weight(60),
        )
        .await?;
    let spot: Value = client
        .request(
            RestRequest::post(Api::Spot, "/sapi/v3/asset/getUserAsset")
                .param("asset", &asset)
                .param("needBtcValuation", "true")
                .signed()
                .idempotent()
                .weight(5),
        )
        .await?;
    let funding: Value = client
        .request(
            RestRequest::post(Api::Spot, "/sapi/v1/asset/get-funding-asset")
                .param("asset", &asset)
                .param("needBtcValuation", "true")
                .signed()
                .idempotent()
                .weight(1),
        )
        .await?;
    let mut balances = vec![
        parse_spot_balance(&spot, &asset),
        parse_funding_balance(&funding, &asset),
    ];
    if scope.include_margin {
        let margin: Value = client
            .request(
                RestRequest::get(Api::Spot, "/sapi/v1/margin/account")
                    .signed()
                    .weight(10),
            )
            .await?;
        balances.push(parse_cross_margin_balance(&margin, &asset));
    }
    if scope.include_futures {
        let futures: Value = client
            .request(
                RestRequest::get(Api::Futures, "/fapi/v3/account")
                    .signed()
                    .weight(5),
            )
            .await?;
        balances.push(parse_usdm_futures_balance(&futures, &asset));
    }
    let mut recent_transfers = Vec::new();
    if let Some(kind) = scope.history_type {
        let history: Value = client
            .request(
                RestRequest::get(Api::Spot, "/sapi/v1/asset/transfer")
                    .param("type", kind)
                    .param("size", 10)
                    .signed()
                    .weight(1),
            )
            .await?;
        recent_transfers = parse_transfer_history(&history);
    }
    Ok(TransferDiscovery {
        timestamp: common::utc_timestamp(),
        asset,
        quote_asset: quote,
        wallet_activations: parse_wallet_activations(&summary),
        balances,
        recent_transfers,
        live_transfers_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

/// Build a dry-run internal transfer plan without submitting it.
pub fn build_transfer_plan(
    discovery: &TransferDiscovery,
    request: &TransferRequest,
    limits: &TransferRiskLimits,
) -> Result<TransferPlan, TransferPlanError> {
    limits.validate()?;
    let source = normalize_wallet(&request.source_wallet)?;
    let destination = normalize_wallet(&request.destination_wallet)?;
    let asset = asset_symbol(&request.asset)?;
    let amount = positive(request.amount, "amount")?;
    let transfer_type = transfer_route(source, destination);
    let source_balance = discovery.balance(source, &asset);
    let destination_balance = discovery.balance(destination, &asset);
    let source_active = discovery.activation(source).and_then(|a| a.active);
    let destination_active = discovery.activation(destination).and_then(|a| a.active);
    let price = if is_stable_asset(&asset) {
        Some(Decimal::ONE)
    } else {
        request
            .asset_usdt_price
            .map(|p| positive(p, "asset_usdt_price"))
            .transpose()?
    };
    let value_usdt = price.map(|p| amount * p);
    let source_before = source_balance.transferable_free();
    let destination_before = destination_balance.transferable_free();
    let source_after = source_before - amount;
    let destination_after = destination_before + amount;
    let allowed_wallet = |w: &str| limits.allowed_wallets.iter().any(|a| a == w);

    let mut rejections: Vec<&str> = Vec::new();
    let mut warnings: Vec<&str> = Vec::new();
    if source == destination && source != WALLET_ISOLATED_MARGIN {
        rejections.push("source and destination wallets must differ");
    }
    if !allowed_wallet(source) {
        rejections.push("source wallet is not allowed by risk limits");
    }
    if !allowed_wallet(destination) {
        rejections.push("destination wallet is not allowed by risk limits");
    }
    if transfer_type.is_none() {
        rejections.push("wallet route is not supported by Binance universal transfer");
    }
    if limits.require_active_wallets {
        if source_active == Some(false) {
            rejections.push("source wallet is not active");
        }
        if destination_active == Some(false) {
            rejections.push("destination wallet is not active");
        }
    }
    if amount > limits.max_transfer_amount {
        rejections.push("amount exceeds max_transfer_amount");
    }
    match value_usdt {
        None => rejections.push("asset_usdt_price is required for non-stable transfer caps"),
        Some(v) if v > limits.max_transfer_value_usdt => {
            rejections.push("transfer value exceeds max_transfer_value_usdt");
        }
        Some(_) => {}
    }
    if amount > source_before {
        rejections.push("amount exceeds source transferable balance");
    }
    if source_after < limits.min_source_free_after {
        rejections.push("source free balance after transfer is below minimum");
    }
    if limits.block_margin_debt_transfer_out
        && (source == WALLET_CROSS_MARGIN || source == WALLET_ISOLATED_MARGIN)
        && source_balance.liability() > Decimal::ZERO
    {
        rejections.push("source margin wallet has outstanding debt or interest");
    }
    let has = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.is_empty());
    if matches!(
        transfer_type,
        Some("ISOLATEDMARGIN_MARGIN" | "ISOLATEDMARGIN_ISOLATEDMARGIN")
    ) && !has(&request.from_symbol)
    {
        rejections.push("from_symbol is required for isolated-margin source");
    }
    if matches!(
        transfer_type,
        Some("MARGIN_ISOLATEDMARGIN" | "ISOLATEDMARGIN_ISOLATEDMARGIN")
    ) && !has(&request.to_symbol)
    {
        rejections.push("to_symbol is required for isolated-margin destination");
    }
    if source_active.is_none() {
        warnings.push("source wallet activation status was not available");
    }
    if destination_active.is_none() {
        warnings.push("destination wallet activation status was not available");
    }

    let mut allocation = BTreeMap::new();
    allocation.insert(
        source.to_owned(),
        AllocationRow {
            asset: asset.clone(),
            free_before: source_before,
            free_after: source_after,
        },
    );
    allocation.insert(
        destination.to_owned(),
        AllocationRow {
            asset: asset.clone(),
            free_before: destination_before,
            free_after: destination_after,
        },
    );

    Ok(TransferPlan {
        timestamp: common::utc_timestamp(),
        source_wallet: source.to_owned(),
        destination_wallet: destination.to_owned(),
        transfer_type: transfer_type.map(str::to_owned),
        asset,
        amount,
        asset_usdt_price: price,
        transfer_value_usdt: value_usdt,
        reason: request.reason.clone(),
        from_symbol: optional_exchange_symbol(request.from_symbol.as_deref())?,
        to_symbol: optional_exchange_symbol(request.to_symbol.as_deref())?,
        source_free_before: source_before,
        source_free_after: source_after,
        destination_free_before: destination_before,
        destination_free_after: destination_after,
        source_active,
        destination_active,
        submission_attempted: false,
        submitted: false,
        allowed: rejections.is_empty(),
        rejection_reasons: rejections.into_iter().map(str::to_owned).collect(),
        warnings: warnings.into_iter().map(str::to_owned).collect(),
        resulting_allocation: allocation,
        risk_limits: limits.clone(),
    })
}

/// Build and optionally audit a dry-run internal transfer report.
pub async fn build_transfer_dry_run_report(
    settings: &Settings,
    client: &BinanceClient,
    request: &TransferRequest,
    limits: &TransferRiskLimits,
    audit_path: Option<&Path>,
) -> Result<TransferDryRunReport, TransferPlanError> {
    let source = normalize_wallet(&request.source_wallet)?;
    let destination = normalize_wallet(&request.destination_wallet)?;
    let discovery = discover_transfer_wallets(
        client,
        &request.asset,
        "USDT",
        DiscoveryScope::for_route(source, destination),
    )
    .await?;
    let plan = build_transfer_plan(&discovery, request, limits)?;
    let audit = match audit_path {
        Some(path) => append_audit(path, &plan)?.display().to_string(),
        None => String::new(),
    };
    Ok(TransferDryRunReport {
        timestamp: common::utc_timestamp(),
        mode: common::mode(settings).to_owned(),
        discovery,
        plan: Some(plan),
        audit_path: audit,
        live_transfers_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

/// Append one proposed transfer plan to a JSONL audit file.
pub fn write_transfer_audit(
    path: &Path,
    plan: &TransferPlan,
) -> Result<PathBuf, TransferPlanError> {
    Ok(append_audit(path, plan)?)
}

pub fn parse_wallet_activations(raw: &Value) -> Vec<WalletActivation> {
    let Some(items) = raw.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|i| i.is_object())
        .filter_map(|item| {
            let name = jtext(item.get("walletName")).trim().to_owned();
            let wallet = wallet_from_name(&name)?;
            Some(WalletActivation {
                wallet: wallet.to_owned(),
                wallet_name: name,
                active: jbool(item.get("activate")),
                quote_balance: jfloat(item.get("balance")),
            })
        })
        .collect()
}

pub fn parse_transfer_history(raw: &Value) -> Vec<TransferHistoryRecord> {
    let rows = match raw {
        Value::Object(map) => map.get("rows").and_then(Value::as_array),
        Value::Array(items) => Some(items),
        _ => None,
    };
    rows.into_iter()
        .flatten()
        .filter(|i| i.is_object())
        .filter_map(|item| {
            let status = jtext(item.get("status")).trim().to_uppercase();
            Some(TransferHistoryRecord {
                transfer_type: jtext(item.get("type")).trim().to_uppercase(),
                asset: asset_symbol(&jtext(item.get("asset"))).ok()?,
                amount: jfloat(item.get("amount")),
                status: if status.is_empty() {
                    "UNKNOWN".to_owned()
                } else {
                    status
                },
                transaction_id: jtext(item.get("tranId")),
                timestamp_ms: match item.get("timestamp") {
                    Some(Value::Number(n)) => n.as_i64(),
                    Some(Value::String(s)) => s.trim().parse().ok(),
                    _ => None,
                },
            })
        })
        .collect()
}

/// Fail closed for any attempted live transfer activation.
pub fn assert_transfer_live_disabled(confirm_live_transfer: bool) -> Result<(), TransferPlanError> {
    if confirm_live_transfer {
        return Err(invalid(
            "live transfer activation is not implemented; Phase 21 is dry-run only",
        ));
    }
    Ok(())
}

/// Concise human-readable dry-run transfer report.
pub fn render_transfer_report(report: &TransferDryRunReport) -> String {
    let mut lines = vec![
        format!("Mode: {}", report.mode),
        "Live transfers: disabled".to_owned(),
        "Write endpoints used: none".to_owned(),
    ];
    if let Some(plan) = &report.plan {
        lines.extend([
            "Transfer plan:".to_owned(),
            format!(
                "  - route: {} -> {}",
                plan.source_wallet, plan.destination_wallet
            ),
            format!(
                "  - type: {}",
                plan.transfer_type.as_deref().unwrap_or("None")
            ),
            format!("  - asset: {}", plan.asset),
            format!("  - amount: {}", fmt_dec(plan.amount)),
            format!("  - source after: {}", fmt_dec(plan.source_free_after)),
            format!(
                "  - destination after: {}",
                fmt_dec(plan.destination_free_after)
            ),
            format!("  - allowed: {}", yes_no(plan.allowed)),
        ]);
        if !plan.rejection_reasons.is_empty() {
            lines.push(format!(
                "  - rejection reasons: {}",
                plan.rejection_reasons.join(", ")
            ));
        }
        if !plan.warnings.is_empty() {
            lines.push(format!("  - warnings: {}", plan.warnings.join(", ")));
        }
    }
    if !report.audit_path.is_empty() {
        lines.push(format!("Audit path: {}", report.audit_path));
    }
    lines.join("\n")
}

fn first_asset_row<'a>(raw: Option<&'a Value>, asset: &str) -> Option<&'a Value> {
    match raw? {
        row @ Value::Object(_) => {
            let row_asset = jtext(row.get("asset")).trim().to_uppercase();
            (row_asset.is_empty() || row_asset == asset).then_some(row)
        }
        Value::Array(items) => items.iter().find(|item| {
            item.is_object() && jtext(item.get("asset")).trim().to_uppercase() == asset
        }),
        _ => None,
    }
}

fn field(row: Option<&Value>, key: &str) -> Decimal {
    jfloat(row.and_then(|r| r.get(key)))
}

fn parse_spot_balance(raw: &Value, asset: &str) -> WalletAssetBalance {
    let row = first_asset_row(Some(raw), asset);
    WalletAssetBalance {
        free: field(row, "free"),
        locked: field(row, "locked") + field(row, "freeze"),
        btc_valuation: field(row, "btcValuation"),
        ..WalletAssetBalance::empty(WALLET_SPOT, asset)
    }
}

fn parse_funding_balance(raw: &Value, asset: &str) -> WalletAssetBalance {
    let row = first_asset_row(Some(raw), asset);
    WalletAssetBalance {
        free: field(row, "free"),
        locked: field(row, "locked") + field(row, "freeze") + field(row, "withdrawing"),
        btc_valuation: field(row, "btcValuation"),
        ..WalletAssetBalance::empty(WALLET_FUNDING, asset)
    }
}

fn parse_cross_margin_balance(raw: &Value, asset: &str) -> WalletAssetBalance {
    let row = first_asset_row(raw.get("userAssets"), asset);
    WalletAssetBalance {
        free: field(row, "free"),
        locked: field(row, "locked"),
        borrowed: field(row, "borrowed"),
        interest: field(row, "interest"),
        ..WalletAssetBalance::empty(WALLET_CROSS_MARGIN, asset)
    }
}

fn parse_usdm_futures_balance(raw: &Value, asset: &str) -> WalletAssetBalance {
    let row = first_asset_row(raw.get("assets"), asset);
    WalletAssetBalance {
        free: field(row, "availableBalance"),
        available: field(row, "availableBalance"),
        wallet_balance: field(row, "walletBalance"),
        locked: field(row, "initialMargin") + field(row, "maintMargin"),
        ..WalletAssetBalance::empty(WALLET_USD_M_FUTURES, asset)
    }
}

/// Canonical wallet id for an alias (`main`, `margin`, `usdm`, ...).
pub fn normalize_wallet(value: &str) -> Result<&'static str, TransferPlanError> {
    let normalized = value.trim().to_lowercase().replace(['-', ' '], "_");
    Ok(match normalized.as_str() {
        "main" | "spot" => WALLET_SPOT,
        "fund" | "funding" => WALLET_FUNDING,
        "margin" | "cross_margin" => WALLET_CROSS_MARGIN,
        "isolated" | "isolated_margin" => WALLET_ISOLATED_MARGIN,
        "umfuture" | "um_futures" | "usd_m" | "usd_m_futures" | "usdm" => WALLET_USD_M_FUTURES,
        _ => return Err(invalid(format!("unsupported wallet: {value}"))),
    })
}

fn wallet_from_name(name: &str) -> Option<&'static str> {
    let normalized = name.trim().to_lowercase();
    match normalized.as_str() {
        "spot" => Some(WALLET_SPOT),
        "funding" => Some(WALLET_FUNDING),
        "cross margin" => Some(WALLET_CROSS_MARGIN),
        "isolated margin" => Some(WALLET_ISOLATED_MARGIN),
        n if n.contains("usd") && n.contains("futures") => Some(WALLET_USD_M_FUTURES),
        _ => None,
    }
}

fn optional_exchange_symbol(symbol: Option<&str>) -> Result<Option<String>, TransferPlanError> {
    match symbol.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Ok(Some(exchange_symbol(&normalize_symbol(s)?))),
        None => Ok(None),
    }
}

fn asset_symbol(value: &str) -> Result<String, TransferPlanError> {
    let text = value.trim().to_uppercase();
    if text.is_empty() {
        return Err(invalid("asset must not be blank"));
    }
    Ok(text)
}

fn positive(value: Decimal, field: &str) -> Result<Decimal, TransferPlanError> {
    if value <= Decimal::ZERO {
        return Err(invalid(format!("{field} must be greater than zero")));
    }
    Ok(value)
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::testutil::{
        mock_client, mount_get, mount_post, request_paths, requests_to, settings_builder,
    };

    pub(crate) async fn mount_wallets(
        server: &MockServer,
        spot_free: &str,
        margin_borrowed: &str,
        futures_active: bool,
    ) {
        mount_get(
            server,
            "/sapi/v1/asset/wallet/balance",
            json!([
                {"activate": true, "balance": "100", "walletName": "Spot"},
                {"activate": true, "balance": "10", "walletName": "Funding"},
                {"activate": true, "balance": "50", "walletName": "Cross Margin"},
                {"activate": false, "balance": "0", "walletName": "Isolated Margin"},
                {"activate": futures_active, "balance": "20", "walletName": "USDⓈ-M Futures"},
                {"activate": true, "balance": "0", "walletName": "Earn"}
            ]),
        )
        .await;
        mount_post(server, "/sapi/v3/asset/getUserAsset", 200, json!([{"asset": "USDT", "free": spot_free, "locked": "1", "freeze": "0", "btcValuation": "0.001"}])).await;
        mount_post(server, "/sapi/v1/asset/get-funding-asset", 200, json!([{"asset": "USDT", "free": "10", "locked": "0", "freeze": "0", "withdrawing": "0", "btcValuation": "0"}])).await;
        mount_get(server, "/sapi/v1/margin/account", json!({"userAssets": [{"asset": "USDT", "free": "30", "locked": "0", "borrowed": margin_borrowed, "interest": "0"}]})).await;
        mount_get(server, "/fapi/v3/account", json!({"assets": [{"asset": "USDT", "walletBalance": "20", "availableBalance": "15", "initialMargin": "2", "maintMargin": "1"}]})).await;
        mount_get(server, "/sapi/v1/asset/transfer", json!({"total": 1, "rows": [{"asset": "USDT", "amount": "1", "type": "MAIN_FUNDING", "status": "CONFIRMED", "tranId": 11, "timestamp": 1700000000000_i64}]})).await;
    }

    #[tokio::test]
    async fn discovery_reads_wallets_and_history_without_transfer_writes() {
        let server = MockServer::start().await;
        mount_wallets(&server, "50", "0", true).await;
        let client = mock_client(&server);
        let discovery = discover_transfer_wallets(
            &client,
            "usdt",
            "USDT",
            DiscoveryScope::for_route(WALLET_SPOT, WALLET_FUNDING),
        )
        .await
        .unwrap();
        assert_eq!(discovery.asset, "USDT");
        assert_eq!(discovery.balance(WALLET_SPOT, "USDT").free, dec!(50));
        assert_eq!(discovery.balance(WALLET_SPOT, "USDT").locked, dec!(1));
        assert_eq!(
            discovery.activation(WALLET_USD_M_FUTURES).unwrap().active,
            Some(true)
        );
        assert_eq!(discovery.wallet_activations.len(), 5);
        assert_eq!(discovery.recent_transfers[0].transaction_id, "11");
        assert!(
            !request_paths(&server)
                .await
                .iter()
                .any(|p| p.contains("/sapi/v1/asset/transfer") && p.starts_with("POST"))
        );
        assert!(
            requests_to(&server, "/sapi/v1/asset/transfer").await[0].contains("type=MAIN_FUNDING")
        );
        assert!(
            requests_to(&server, "/sapi/v1/margin/account")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn plan_models_allocation_and_audits() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_wallets(&server, "50", "0", true).await;
        let client = mock_client(&server);
        let settings = settings_builder().build().unwrap();
        let audit = dir.path().join("transfer-plans.jsonl");
        let request = TransferRequest::new("spot", "funding", "USDT", dec!(5));
        let report = build_transfer_dry_run_report(
            &settings,
            &client,
            &request,
            &TransferRiskLimits::default(),
            Some(&audit),
        )
        .await
        .unwrap();
        let plan = report.plan.as_ref().unwrap();
        assert!(plan.allowed, "{:?}", plan.rejection_reasons);
        assert_eq!(plan.transfer_type.as_deref(), Some("MAIN_FUNDING"));
        assert_eq!(plan.source_free_after, dec!(45));
        assert_eq!(plan.destination_free_after, dec!(15));
        assert!(!plan.submission_attempted && !plan.submitted);
        assert_eq!(report.audit_path, audit.display().to_string());
        let row: Value = serde_json::from_str(
            std::fs::read_to_string(&audit)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            row["resulting_allocation"]["spot"]["free_after"].to_string(),
            "45"
        );
        assert!(render_transfer_report(&report).contains("  - route: spot -> funding"));
    }

    #[tokio::test]
    async fn plan_rejects_debt_inactive_wallets_caps_and_unsupported_routes() {
        let server = MockServer::start().await;
        mount_wallets(&server, "3", "2", false).await;
        let client = mock_client(&server);
        let limits = TransferRiskLimits::default();

        let discovery = discover_transfer_wallets(
            &client,
            "USDT",
            "USDT",
            DiscoveryScope::for_route(WALLET_CROSS_MARGIN, WALLET_USD_M_FUTURES),
        )
        .await
        .unwrap();
        let plan = build_transfer_plan(
            &discovery,
            &TransferRequest::new("margin", "usdm", "USDT", dec!(5)),
            &limits,
        )
        .unwrap();
        assert!(
            plan.rejection_reasons
                .contains(&"source margin wallet has outstanding debt or interest".to_owned())
        );
        assert!(
            plan.rejection_reasons
                .contains(&"destination wallet is not active".to_owned())
        );

        let plan = build_transfer_plan(
            &discovery,
            &TransferRequest::new("spot", "funding", "USDT", dec!(20)),
            &limits,
        )
        .unwrap();
        for reason in [
            "amount exceeds max_transfer_amount",
            "amount exceeds source transferable balance",
            "source free balance after transfer is below minimum",
        ] {
            assert!(
                plan.rejection_reasons.iter().any(|r| r == reason),
                "{reason}"
            );
        }
        let plan = build_transfer_plan(
            &discovery,
            &TransferRequest::new("funding", "isolated", "BNB", dec!(1)),
            &limits,
        )
        .unwrap();
        assert!(
            plan.rejection_reasons.contains(
                &"wallet route is not supported by Binance universal transfer".to_owned()
            )
        );
        assert!(
            plan.rejection_reasons
                .contains(&"asset_usdt_price is required for non-stable transfer caps".to_owned())
        );
        let plan = build_transfer_plan(
            &discovery,
            &TransferRequest::new("margin", "isolated", "USDT", dec!(1)),
            &limits,
        )
        .unwrap();
        assert!(
            plan.rejection_reasons
                .contains(&"to_symbol is required for isolated-margin destination".to_owned())
        );
        assert!(
            build_transfer_plan(
                &discovery,
                &TransferRequest::new("earn", "spot", "USDT", dec!(1)),
                &limits
            )
            .is_err()
        );
        assert!(assert_transfer_live_disabled(true).is_err());
    }
}

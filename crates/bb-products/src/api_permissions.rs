//! Binance API permission diagnostics and feature capability registry
//! (port of `src/api_permissions.py`).
//!
//! The only exchange calls are `GET /api/v3/account` and the safe, non-matching
//! `POST /api/v3/order/test`; nothing here can submit a real order.

use std::fmt;
use std::str::FromStr;

use bb_binance::spot::{AccountInfo, NewOrder};
use bb_binance::{BinanceClient, BinanceError, OrderType, Side, normalize_symbol};
use bb_core::money::ceil_to_step;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;

use crate::settings::Settings;

/// Raised when a feature is not allowed by permission diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum ApiPermissionError {
    #[error("{0}")]
    Denied(String),
    #[error("unknown feature: {0}")]
    UnknownFeature(String),
    #[error("unknown capability: {0}")]
    UnknownCapability(String),
    #[error("{0}")]
    Market(String),
    #[error(transparent)]
    Binance(#[from] BinanceError),
}

/// Result of one capability diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Ok,
    Missing,
    Unknown,
    PolicyBlocked,
    Skipped,
}

impl CapabilityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
            Self::PolicyBlocked => "policy_blocked",
            Self::Skipped => "skipped",
        }
    }
}

impl fmt::Display for CapabilityStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Binance API capability names, in report order (`CAPABILITY_ORDER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Reading,
    SpotTrading,
    Margin,
    Futures,
    UniversalTransfer,
    Withdrawals,
    Prediction,
    SymbolWhitelist,
    IpRestriction,
}

impl Capability {
    pub const ORDER: [Capability; 9] = [
        Self::Reading,
        Self::SpotTrading,
        Self::Margin,
        Self::Futures,
        Self::UniversalTransfer,
        Self::Withdrawals,
        Self::Prediction,
        Self::SymbolWhitelist,
        Self::IpRestriction,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reading => "reading",
            Self::SpotTrading => "spot_trading",
            Self::Margin => "margin",
            Self::Futures => "futures",
            Self::UniversalTransfer => "universal_transfer",
            Self::Withdrawals => "withdrawals",
            Self::Prediction => "prediction",
            Self::SymbolWhitelist => "symbol_whitelist",
            Self::IpRestriction => "ip_restriction",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Capability {
    type Err = ApiPermissionError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let normalized = normalize_name(name);
        Self::ORDER
            .into_iter()
            .find(|capability| capability.as_str() == normalized)
            .ok_or_else(|| ApiPermissionError::UnknownCapability(name.to_owned()))
    }
}

/// Bot features gated by capability diagnostics (`FEATURES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum Feature {
    OperationsCheck,
    PaperPool,
    ManualLiveProbe,
    SpotLiveCanary,
    SpotLivePool,
    ClaudeSpotAutonomous,
    MarginDryRun,
    MarginCanaryPlan,
    MarginTrading,
    FuturesDryRun,
    FuturesCanaryPlan,
    FuturesTrading,
    TransferDryRun,
    TransferCanaryPlan,
    UnifiedPoolDryRun,
    SymbolScopeAudit,
    WithdrawalDryRun,
    PredictionDryRun,
    UniversalTransfer,
    Withdrawals,
    PredictionTrading,
    FutureProductModule,
}

/// Registry entry: required capabilities, implemented flag, description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureSpec {
    pub required: &'static [Capability],
    pub implemented: bool,
    pub detail: &'static str,
}

const READ: &[Capability] = &[Capability::Reading];
const READ_SPOT: &[Capability] = &[Capability::Reading, Capability::SpotTrading];

impl Feature {
    /// Registry order (Python dict insertion order).
    pub const ALL: [Feature; 22] = [
        Self::OperationsCheck,
        Self::PaperPool,
        Self::ManualLiveProbe,
        Self::SpotLiveCanary,
        Self::SpotLivePool,
        Self::ClaudeSpotAutonomous,
        Self::MarginDryRun,
        Self::MarginCanaryPlan,
        Self::MarginTrading,
        Self::FuturesDryRun,
        Self::FuturesCanaryPlan,
        Self::FuturesTrading,
        Self::TransferDryRun,
        Self::TransferCanaryPlan,
        Self::UnifiedPoolDryRun,
        Self::SymbolScopeAudit,
        Self::WithdrawalDryRun,
        Self::PredictionDryRun,
        Self::UniversalTransfer,
        Self::Withdrawals,
        Self::PredictionTrading,
        Self::FutureProductModule,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperationsCheck => "operations_check",
            Self::PaperPool => "paper_pool",
            Self::ManualLiveProbe => "manual_live_probe",
            Self::SpotLiveCanary => "spot_live_canary",
            Self::SpotLivePool => "spot_live_pool",
            Self::ClaudeSpotAutonomous => "claude_spot_autonomous",
            Self::MarginDryRun => "margin_dry_run",
            Self::MarginCanaryPlan => "margin_canary_plan",
            Self::MarginTrading => "margin_trading",
            Self::FuturesDryRun => "futures_dry_run",
            Self::FuturesCanaryPlan => "futures_canary_plan",
            Self::FuturesTrading => "futures_trading",
            Self::TransferDryRun => "transfer_dry_run",
            Self::TransferCanaryPlan => "transfer_canary_plan",
            Self::UnifiedPoolDryRun => "unified_pool_dry_run",
            Self::SymbolScopeAudit => "symbol_scope_audit",
            Self::WithdrawalDryRun => "withdrawal_dry_run",
            Self::PredictionDryRun => "prediction_dry_run",
            Self::UniversalTransfer => "universal_transfer",
            Self::Withdrawals => "withdrawals",
            Self::PredictionTrading => "prediction_trading",
            Self::FutureProductModule => "future_product_module",
        }
    }

    pub fn spec(self) -> FeatureSpec {
        let (required, implemented, detail) = match self {
            Self::OperationsCheck => (READ, true, "read-only operational diagnostics"),
            Self::PaperPool => (READ, true, "paper-only pool evaluation"),
            Self::ManualLiveProbe => (READ_SPOT, true, "foreground one-shot live probe"),
            Self::SpotLiveCanary => (READ_SPOT, true, "foreground tiny spot live canary"),
            Self::SpotLivePool => (READ_SPOT, true, "foreground guarded live spot allocator"),
            Self::ClaudeSpotAutonomous => (
                READ_SPOT,
                true,
                "Claude-controlled autonomous Spot allocator",
            ),
            Self::MarginDryRun => (
                READ,
                true,
                "read-only margin discovery and local risk modeling",
            ),
            Self::MarginCanaryPlan => (
                READ,
                true,
                "foreground margin canary planning with live mutation blocked",
            ),
            Self::MarginTrading => (
                &[Capability::Reading, Capability::Margin][..],
                false,
                "live margin activation is not implemented",
            ),
            Self::FuturesDryRun => (
                READ,
                true,
                "read-only USD-M futures discovery and local risk modeling",
            ),
            Self::FuturesCanaryPlan => (
                READ,
                true,
                "foreground USD-M futures canary planning with live mutation blocked",
            ),
            Self::FuturesTrading => (
                &[Capability::Reading, Capability::Futures][..],
                false,
                "live futures activation is not implemented",
            ),
            Self::TransferDryRun => (
                READ,
                true,
                "read-only wallet discovery and local transfer planning",
            ),
            Self::TransferCanaryPlan => (
                READ,
                true,
                "foreground transfer canary planning with live mutation blocked",
            ),
            Self::UnifiedPoolDryRun => (
                READ,
                true,
                "read-only unified account-type valuation and constraint modeling",
            ),
            Self::SymbolScopeAudit => (READ, true, "local symbol and asset whitelist diagnostics"),
            Self::WithdrawalDryRun => (
                READ,
                true,
                "local withdrawal safety planning with live withdrawal blocked",
            ),
            Self::PredictionDryRun => (
                READ,
                true,
                "local prediction risk modeling with live prediction trading blocked",
            ),
            Self::UniversalTransfer => (
                &[Capability::Reading, Capability::UniversalTransfer][..],
                false,
                "live transfer activation is not implemented",
            ),
            Self::Withdrawals => (
                &[Capability::Reading, Capability::Withdrawals][..],
                false,
                "hard-blocked by policy",
            ),
            Self::PredictionTrading => (
                &[Capability::Reading, Capability::Prediction][..],
                false,
                "phase 23 roadmap item",
            ),
            Self::FutureProductModule => (
                READ,
                false,
                "phase 23 placeholder for future Binance product modules",
            ),
        };
        FeatureSpec {
            required,
            implemented,
            detail,
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Feature {
    type Err = ApiPermissionError;

    /// Accepts `-` or `_` separators and any case, like `_normalize_feature_name`.
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let normalized = normalize_name(name);
        Self::ALL
            .into_iter()
            .find(|feature| feature.as_str() == normalized)
            .ok_or_else(|| ApiPermissionError::UnknownFeature(name.to_owned()))
    }
}

fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase().replace('-', "_")
}

/// One Binance API capability diagnostic result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityCheck {
    pub capability: Capability,
    pub status: CapabilityStatus,
    pub source: String,
    pub detail: String,
    pub error_code: Option<i64>,
    pub http_status: Option<u16>,
}

impl CapabilityCheck {
    pub fn new(
        capability: Capability,
        status: CapabilityStatus,
        source: &str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            capability,
            status,
            source: source.to_owned(),
            detail: detail.into(),
            error_code: None,
            http_status: None,
        }
    }

    fn suffix(&self) -> String {
        let mut suffix = String::new();
        if let Some(code) = self.error_code {
            suffix.push_str(&format!(" error_code={code}"));
        }
        if let Some(status) = self.http_status {
            suffix.push_str(&format!(" http_status={status}"));
        }
        suffix
    }

    fn failure_message(&self) -> String {
        format!(
            "{} permission check failed: {}; {}{}",
            self.capability,
            self.status,
            self.detail,
            self.suffix()
        )
    }
}

/// Whether one bot feature is allowed by current capability diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeatureCheck {
    pub feature: Feature,
    pub allowed: bool,
    pub implemented: bool,
    pub required_capabilities: Vec<Capability>,
    pub missing_capabilities: Vec<Capability>,
    pub unknown_capabilities: Vec<Capability>,
    pub policy_blocked_capabilities: Vec<Capability>,
    pub detail: String,
}

/// Safe spot order-test payload for Binance's non-matching endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotOrderTest {
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
}

impl SpotOrderTest {
    /// Build the `/api/v3/order/test` request (MARKET or LIMIT GTC).
    pub fn to_new_order(&self) -> Result<NewOrder, ApiPermissionError> {
        let positive = |value: Decimal| {
            if value > Decimal::ZERO {
                Ok(value.normalize())
            } else {
                Err(ApiPermissionError::Market(
                    "order-test numeric values must be positive".into(),
                ))
            }
        };
        let symbol = normalize_symbol(&self.symbol)?;
        let quantity = positive(self.quantity)?;
        match self.order_type {
            OrderType::Limit => {
                let price = self.price.ok_or_else(|| {
                    ApiPermissionError::Market("limit order test requires a price".into())
                })?;
                Ok(NewOrder::limit(
                    &symbol,
                    self.side,
                    quantity,
                    positive(price)?,
                ))
            }
            OrderType::Market => Ok(NewOrder::market(&symbol, self.side, quantity)),
            other => Err(ApiPermissionError::Market(format!(
                "unsupported order-test type: {}",
                other.as_str()
            ))),
        }
    }
}

/// Full permission diagnostic report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermissionReport {
    pub timestamp: String,
    pub mode: String,
    pub account_type: Option<String>,
    pub account_permissions: Vec<String>,
    pub can_trade: Option<bool>,
    pub can_withdraw: Option<bool>,
    pub spot_test_symbol: Option<String>,
    pub capabilities: Vec<CapabilityCheck>,
    pub features: Vec<FeatureCheck>,
}

impl PermissionReport {
    /// Capability row (every report carries all nine capabilities).
    pub fn capability(&self, capability: Capability) -> Option<&CapabilityCheck> {
        self.capabilities
            .iter()
            .find(|check| check.capability == capability)
    }

    /// Evaluated feature row, `None` when the feature was not selected.
    pub fn feature(&self, feature: Feature) -> Option<&FeatureCheck> {
        self.features.iter().find(|check| check.feature == feature)
    }

    /// JSON form identical to Python `PermissionReport.as_dict()`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Options for [`inspect_api_permissions`].
#[derive(Debug, Clone)]
pub struct InspectOptions {
    /// Spot symbol for the safe order-test probe (defaults to `settings.trading_symbol`).
    pub spot_test_symbol: Option<String>,
    pub include_spot_order_test: bool,
    /// Features to evaluate; empty means every registered feature.
    pub features: Vec<Feature>,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            spot_test_symbol: None,
            include_spot_order_test: true,
            features: Vec::new(),
        }
    }
}

/// Inspect API capabilities without submitting real orders.
pub async fn inspect_api_permissions(
    settings: &Settings,
    client: &BinanceClient,
    options: &InspectOptions,
) -> Result<PermissionReport, ApiPermissionError> {
    let symbol = normalize_symbol(
        options
            .spot_test_symbol
            .as_deref()
            .unwrap_or(&settings.trading_symbol),
    )?;
    let (reading, account) = probe_account_info(client).await;

    let spot_trading = if options.include_spot_order_test && reading.status == CapabilityStatus::Ok
    {
        let order_test = build_dynamic_spot_order_test(client, &symbol).await?;
        probe_spot_order_test(client, &order_test).await
    } else if !options.include_spot_order_test {
        CapabilityCheck::new(
            Capability::SpotTrading,
            CapabilityStatus::Skipped,
            "operator",
            "spot order-test probe was skipped",
        )
    } else {
        CapabilityCheck::new(
            Capability::SpotTrading,
            CapabilityStatus::Unknown,
            "account",
            "spot trading was not checked because account reading failed",
        )
    };

    let mut capabilities = vec![reading, spot_trading];
    capabilities.extend(policy_capabilities(&capabilities));
    let selected: &[Feature] = if options.features.is_empty() {
        &Feature::ALL
    } else {
        &options.features
    };
    let features = selected
        .iter()
        .map(|feature| evaluate_feature(&capabilities, *feature))
        .collect();
    let mut ordered = Vec::with_capacity(Capability::ORDER.len());
    for name in Capability::ORDER {
        if let Some(check) = capabilities.iter().find(|check| check.capability == name) {
            ordered.push(check.clone());
        }
    }

    Ok(PermissionReport {
        timestamp: bb_core::time::now().to_rfc3339(),
        mode: mode(settings).to_owned(),
        account_type: account
            .as_ref()
            .and_then(|a| optional_text(&a.account_type)),
        account_permissions: account
            .as_ref()
            .map(|a| a.permissions.iter().map(|p| p.to_uppercase()).collect())
            .unwrap_or_default(),
        can_trade: account.as_ref().map(|a| a.can_trade),
        can_withdraw: account.as_ref().map(|a| a.can_withdraw),
        spot_test_symbol: options.include_spot_order_test.then_some(symbol),
        capabilities: ordered,
        features,
    })
}

/// Build a valid-looking LIMIT BUY order-test payload from current market metadata.
pub async fn build_dynamic_spot_order_test(
    client: &BinanceClient,
    symbol: &str,
) -> Result<SpotOrderTest, ApiPermissionError> {
    let normalized = normalize_symbol(symbol)?;
    let markets = client.spot_markets(false).await?;
    let rules = markets
        .get(&normalized)
        .filter(|rules| rules.is_trading())
        .ok_or_else(|| {
            ApiPermissionError::Market(format!(
                "{normalized} is not an active spot market for order testing"
            ))
        })?
        .clone();

    let ticker = client.ticker_24h(&normalized).await?;
    let price = [Some(ticker.last_price), ticker.ask_price, ticker.bid_price]
        .into_iter()
        .flatten()
        .find(|price| *price > Decimal::ZERO)
        .ok_or_else(|| {
            ApiPermissionError::Market(format!(
                "{normalized} ticker did not include a usable price"
            ))
        })?;

    let rounded = rules.round_price(price);
    let price = if rounded > Decimal::ZERO {
        rounded
    } else {
        price
    };
    let min_amount = rules
        .min_qty()
        .filter(|v| *v > Decimal::ZERO)
        .unwrap_or(Decimal::ZERO);
    let min_notional = rules
        .min_notional()
        .filter(|v| *v > Decimal::ZERO)
        .unwrap_or(dec!(5));
    let required = min_amount.max(min_notional * dec!(1.02) / price);
    let quantity = ceil_to_step(required, rules.step_size());
    if quantity <= Decimal::ZERO {
        return Err(ApiPermissionError::Market(format!(
            "{normalized} market metadata produced a zero test quantity"
        )));
    }
    Ok(SpotOrderTest {
        symbol: normalized,
        side: Side::Buy,
        order_type: OrderType::Limit,
        quantity,
        price: Some(price),
    })
}

/// Run Binance's safe spot order-test endpoint.
pub async fn probe_spot_order_test(
    client: &BinanceClient,
    order_test: &SpotOrderTest,
) -> CapabilityCheck {
    const SOURCE: &str = "POST /api/v3/order/test";
    let result = match order_test.to_new_order() {
        Ok(order) => client.test_order(&order).await,
        Err(ApiPermissionError::Binance(err)) => Err(err),
        Err(err) => Err(BinanceError::InvalidRequest(err.to_string())),
    };
    match result {
        Ok(()) => CapabilityCheck::new(
            Capability::SpotTrading,
            CapabilityStatus::Ok,
            SOURCE,
            format!("safe spot order-test succeeded for {}", order_test.symbol),
        ),
        Err(err) => capability_from_error(
            Capability::SpotTrading,
            SOURCE,
            &err,
            "Binance rejected the safe order-test endpoint. Enable 'Spot & Margin Trading' for \
             this API key and verify the current server IP is allowed in API Management.",
            "Binance reached the safe order-test endpoint but rejected the test payload; inspect \
             market rules, balances, and the raw exchange error before enabling live orders.",
        ),
    }
}

/// Fail closed unless Binance accepts a safe order-test payload.
pub async fn ensure_spot_order_test_allowed(
    client: &BinanceClient,
    order_test: &SpotOrderTest,
) -> Result<(), ApiPermissionError> {
    let order_test = SpotOrderTest {
        symbol: normalize_symbol(&order_test.symbol)?,
        ..order_test.clone()
    };
    let check = probe_spot_order_test(client, &order_test).await;
    if check.status != CapabilityStatus::Ok {
        return Err(ApiPermissionError::Denied(check.failure_message()));
    }
    Ok(())
}

/// Evaluate whether a bot feature may run under current diagnostics.
pub fn evaluate_feature(capabilities: &[CapabilityCheck], feature: Feature) -> FeatureCheck {
    let spec = feature.spec();
    let mut missing = Vec::new();
    let mut unknown = Vec::new();
    let mut policy_blocked = Vec::new();
    for &capability in spec.required {
        let status = capabilities
            .iter()
            .find(|check| check.capability == capability)
            .map_or(CapabilityStatus::Unknown, |check| check.status);
        match status {
            CapabilityStatus::Missing => missing.push(capability),
            CapabilityStatus::PolicyBlocked => policy_blocked.push(capability),
            CapabilityStatus::Ok => {}
            CapabilityStatus::Unknown | CapabilityStatus::Skipped => unknown.push(capability),
        }
    }
    let allowed =
        spec.implemented && missing.is_empty() && unknown.is_empty() && policy_blocked.is_empty();
    let detail = if !spec.implemented {
        format!("{}; feature is not implemented yet", spec.detail)
    } else if allowed {
        "all required capabilities passed".to_owned()
    } else {
        "feature fails closed until required capabilities pass".to_owned()
    };
    FeatureCheck {
        feature,
        allowed,
        implemented: spec.implemented,
        required_capabilities: spec.required.to_vec(),
        missing_capabilities: missing,
        unknown_capabilities: unknown,
        policy_blocked_capabilities: policy_blocked,
        detail,
    }
}

/// Error unless the report allows `feature` (fails closed when it was not evaluated).
pub fn ensure_feature_allowed(
    report: &PermissionReport,
    feature: Feature,
) -> Result<FeatureCheck, ApiPermissionError> {
    let check = report
        .feature(feature)
        .ok_or_else(|| ApiPermissionError::UnknownFeature(feature.as_str().to_owned()))?;
    if !check.allowed {
        let mut parts = vec![format!(
            "{} is not allowed: {}",
            check.feature, check.detail
        )];
        for capability in check
            .missing_capabilities
            .iter()
            .chain(&check.unknown_capabilities)
            .chain(&check.policy_blocked_capabilities)
        {
            if let Some(cap) = report.capability(*capability) {
                parts.push(format!(
                    "{}={} ({})",
                    cap.capability, cap.status, cap.detail
                ));
            }
        }
        return Err(ApiPermissionError::Denied(parts.join("; ")));
    }
    Ok(check.clone())
}

/// Concise human-readable report.
pub fn render_report(report: &PermissionReport) -> String {
    let mut lines = vec![
        format!("Mode: {}", report.mode),
        format!(
            "Account type: {}",
            report.account_type.as_deref().unwrap_or("unknown")
        ),
        format!(
            "Account permissions: {}",
            if report.account_permissions.is_empty() {
                "unknown".to_owned()
            } else {
                report.account_permissions.join(", ")
            }
        ),
        format!("canTrade: {}", render_optional(report.can_trade)),
        format!("canWithdraw: {}", render_optional(report.can_withdraw)),
        "Capabilities:".to_owned(),
    ];
    for check in &report.capabilities {
        lines.push(format!(
            "  - {}: {} ({}) - {}{}",
            check.capability,
            check.status,
            check.source,
            check.detail,
            check.suffix()
        ));
    }
    lines.push("Features:".to_owned());
    for feature in &report.features {
        let verdict = if feature.allowed {
            "allowed"
        } else {
            "blocked"
        };
        lines.push(format!(
            "  - {}: {verdict} - {}",
            feature.feature, feature.detail
        ));
        for (label, caps) in [
            ("missing", &feature.missing_capabilities),
            ("unknown", &feature.unknown_capabilities),
            ("policy_blocked", &feature.policy_blocked_capabilities),
        ] {
            if !caps.is_empty() {
                let names: Vec<&str> = caps.iter().map(|c| c.as_str()).collect();
                lines.push(format!("    {label}: {}", names.join(", ")));
            }
        }
    }
    lines.join("\n")
}

async fn probe_account_info(client: &BinanceClient) -> (CapabilityCheck, Option<AccountInfo>) {
    const SOURCE: &str = "GET /api/v3/account";
    match client.account().await {
        Ok(account) => (
            CapabilityCheck::new(
                Capability::Reading,
                CapabilityStatus::Ok,
                SOURCE,
                "account information request succeeded",
            ),
            Some(account),
        ),
        Err(BinanceError::Decode { .. }) => (
            CapabilityCheck::new(
                Capability::Reading,
                CapabilityStatus::Unknown,
                SOURCE,
                "account information response was not a mapping",
            ),
            None,
        ),
        Err(err) => (
            capability_from_error(
                Capability::Reading,
                SOURCE,
                &err,
                "Binance rejected account information access. Check the API key/secret, Enable \
                 Reading, and any IP restriction.",
                "Binance account information request failed before permissions could be confirmed.",
            ),
            None,
        ),
    }
}

fn policy_capabilities(capabilities: &[CapabilityCheck]) -> Vec<CapabilityCheck> {
    let permission_error_seen = capabilities
        .iter()
        .any(|check| is_permission_error_code(check.error_code));
    let ip_detail = if permission_error_seen {
        "Binance does not expose the API-key IP whitelist in this safe diagnostic. Because a \
         -2015-style error was observed, verify the current server IP in API Management."
    } else {
        "Binance does not expose the API-key IP whitelist in this safe diagnostic."
    };
    use Capability as C;
    use CapabilityStatus as S;
    vec![
        CapabilityCheck::new(
            C::Margin,
            S::PolicyBlocked,
            "policy",
            "live margin trading remains disabled; Phase 19 only permits dry-run discovery and local modeling",
        ),
        CapabilityCheck::new(
            C::Futures,
            S::PolicyBlocked,
            "policy",
            "live futures trading remains disabled; Phase 20 only permits dry-run discovery and local modeling",
        ),
        CapabilityCheck::new(
            C::UniversalTransfer,
            S::PolicyBlocked,
            "policy",
            "live transfer automation remains disabled; Phase 21 only permits dry-run planning and local audit",
        ),
        CapabilityCheck::new(
            C::Withdrawals,
            S::PolicyBlocked,
            "policy",
            "withdrawals are hard-blocked unless a future withdrawal-safety phase approves them",
        ),
        CapabilityCheck::new(
            C::Prediction,
            S::PolicyBlocked,
            "policy",
            "future phase 23 module; prediction trading is not implemented",
        ),
        CapabilityCheck::new(
            C::SymbolWhitelist,
            S::Unknown,
            "api-management",
            "Binance does not expose the full symbol whitelist in this safe diagnostic",
        ),
        CapabilityCheck::new(C::IpRestriction, S::Unknown, "api-management", ip_detail),
    ]
}

fn capability_from_error(
    capability: Capability,
    source: &str,
    err: &BinanceError,
    permission_detail: &str,
    unknown_detail: &str,
) -> CapabilityCheck {
    let permission = is_permission_error_code(err.code())
        || err
            .to_string()
            .to_lowercase()
            .contains("invalid api-key, ip, or permissions");
    let (status, detail) = if permission {
        (CapabilityStatus::Missing, permission_detail.to_owned())
    } else {
        (
            CapabilityStatus::Unknown,
            format!("{unknown_detail} Exchange said: {err}"),
        )
    };
    CapabilityCheck {
        error_code: err.code(),
        http_status: err.http_status(),
        ..CapabilityCheck::new(capability, status, source, detail)
    }
}

fn is_permission_error_code(code: Option<i64>) -> bool {
    code == Some(-2015)
}

fn mode(settings: &Settings) -> &'static str {
    if settings.dry_run {
        "DRY_RUN"
    } else if settings.use_testnet {
        "testnet"
    } else {
        "LIVE"
    }
}

fn optional_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn render_optional(value: Option<bool>) -> &'static str {
    match value {
        None => "unknown",
        Some(true) => "true",
        Some(false) => "false",
    }
}

#[cfg(test)]
mod tests {
    use bb_binance::BinanceConfig;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    pub(crate) fn mock_client(server: &MockServer) -> BinanceClient {
        let config = BinanceConfig::new(false)
            .with_credentials(bb_binance::Credentials::new("key", "secret").unwrap())
            .with_spot_rest_url(server.uri())
            .with_retries(0, std::time::Duration::ZERO, std::time::Duration::ZERO);
        BinanceClient::new(config).unwrap()
    }

    async fn mount_time(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"serverTime": bb_core::time::now_ms()})),
            )
            .mount(server)
            .await;
    }

    fn exchange_info() -> serde_json::Value {
        serde_json::json!({"symbols": [{
            "symbol": "BTCUSDT", "status": "TRADING", "baseAsset": "BTC", "quoteAsset": "USDT",
            "baseAssetPrecision": 8, "quoteAssetPrecision": 8, "orderTypes": ["LIMIT", "MARKET"],
            "isSpotTradingAllowed": true, "isMarginTradingAllowed": true,
            "permissions": ["SPOT"],
            "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": "0.01", "maxPrice": "1000000", "tickSize": "0.01"},
                {"filterType": "LOT_SIZE", "minQty": "0.00001", "maxQty": "9000", "stepSize": "0.00001"},
                {"filterType": "NOTIONAL", "minNotional": "5", "applyMinToMarket": true, "maxNotional": "9000000", "applyMaxToMarket": false, "avgPriceMins": 5}
            ]
        }]})
    }

    fn ticker() -> serde_json::Value {
        serde_json::json!({
            "symbol": "BTCUSDT", "priceChange": "0", "priceChangePercent": "0",
            "weightedAvgPrice": "50000", "lastPrice": "50000.123", "openPrice": "50000",
            "highPrice": "50000", "lowPrice": "50000", "volume": "1", "quoteVolume": "1",
            "openTime": 0, "closeTime": 0, "firstId": 0, "lastId": 0, "count": 0
        })
    }

    fn settings() -> Settings {
        let mut builder = crate::settings::SettingsBuilder::new("key", "secret");
        builder.use_testnet = false;
        builder.build().unwrap()
    }

    fn check(capability: Capability, status: CapabilityStatus) -> CapabilityCheck {
        CapabilityCheck::new(capability, status, "test", "detail")
    }

    #[test]
    fn feature_names_round_trip_and_normalize() {
        for feature in Feature::ALL {
            assert_eq!(feature.as_str().parse::<Feature>().unwrap(), feature);
        }
        assert_eq!(
            " Claude-Spot-Autonomous ".parse::<Feature>().unwrap(),
            Feature::ClaudeSpotAutonomous
        );
        assert!(matches!(
            "nope".parse::<Feature>(),
            Err(ApiPermissionError::UnknownFeature(_))
        ));
        assert_eq!(
            "ip-restriction".parse::<Capability>().unwrap(),
            Capability::IpRestriction
        );
    }

    #[test]
    fn evaluate_feature_classifies_capabilities() {
        let caps = vec![
            check(Capability::Reading, CapabilityStatus::Ok),
            check(Capability::SpotTrading, CapabilityStatus::Missing),
        ];
        let spot = evaluate_feature(&caps, Feature::SpotLiveCanary);
        assert!(!spot.allowed);
        assert_eq!(spot.missing_capabilities, vec![Capability::SpotTrading]);
        assert_eq!(
            spot.detail,
            "feature fails closed until required capabilities pass"
        );

        let paper = evaluate_feature(&caps, Feature::PaperPool);
        assert!(paper.allowed);
        assert_eq!(paper.detail, "all required capabilities passed");

        let caps = vec![
            check(Capability::Reading, CapabilityStatus::Ok),
            check(Capability::SpotTrading, CapabilityStatus::Skipped),
            check(Capability::Withdrawals, CapabilityStatus::PolicyBlocked),
        ];
        let claude = evaluate_feature(&caps, Feature::ClaudeSpotAutonomous);
        assert_eq!(claude.unknown_capabilities, vec![Capability::SpotTrading]);
        let withdrawals = evaluate_feature(&caps, Feature::Withdrawals);
        assert!(!withdrawals.implemented);
        assert_eq!(
            withdrawals.policy_blocked_capabilities,
            vec![Capability::Withdrawals]
        );
        assert_eq!(
            withdrawals.detail,
            "hard-blocked by policy; feature is not implemented yet"
        );
    }

    #[test]
    fn order_test_payload_formats_limit_and_market() {
        let limit = SpotOrderTest {
            symbol: "btcusdt".into(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            quantity: Decimal::new(1000, 7),
            price: Some(Decimal::new(5000010, 2)),
        };
        let order = limit.to_new_order().unwrap();
        assert_eq!(order.symbol, "BTC/USDT");
        assert_eq!(order.quantity, Some(Decimal::new(1, 4)));
        assert_eq!(order.price, Some(Decimal::new(500001, 1)));
        let missing_price = SpotOrderTest {
            price: None,
            ..limit.clone()
        };
        assert!(
            missing_price
                .to_new_order()
                .unwrap_err()
                .to_string()
                .contains("requires a price")
        );
        let zero = SpotOrderTest {
            quantity: Decimal::ZERO,
            ..limit
        };
        assert!(
            zero.to_new_order()
                .unwrap_err()
                .to_string()
                .contains("must be positive")
        );
    }

    #[tokio::test]
    async fn inspect_reports_ok_capabilities_and_policy_blocks() {
        let server = MockServer::start().await;
        mount_time(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v3/account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accountType": "SPOT", "canTrade": true, "canWithdraw": false,
                "permissions": ["spot"], "balances": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(exchange_info()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/24hr"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ticker()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order/test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server);
        let options = InspectOptions {
            features: vec![Feature::SpotLiveCanary, Feature::MarginTrading],
            ..InspectOptions::default()
        };
        let report = inspect_api_permissions(&settings(), &client, &options)
            .await
            .unwrap();
        assert_eq!(report.mode, "LIVE");
        assert_eq!(report.account_type.as_deref(), Some("SPOT"));
        assert_eq!(report.account_permissions, vec!["SPOT"]);
        assert_eq!(report.can_trade, Some(true));
        assert_eq!(report.spot_test_symbol.as_deref(), Some("BTC/USDT"));
        assert_eq!(report.capabilities.len(), 9);
        let order: Vec<Capability> = report.capabilities.iter().map(|c| c.capability).collect();
        assert_eq!(order, Capability::ORDER.to_vec());
        assert_eq!(
            report.capability(Capability::SpotTrading).unwrap().status,
            CapabilityStatus::Ok
        );
        assert_eq!(
            report.capability(Capability::Margin).unwrap().status,
            CapabilityStatus::PolicyBlocked
        );
        assert!(ensure_feature_allowed(&report, Feature::SpotLiveCanary).is_ok());
        let err = ensure_feature_allowed(&report, Feature::MarginTrading)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("margin_trading is not allowed"), "{err}");
        assert!(err.contains("margin=policy_blocked"), "{err}");

        // Order-test request: LIMIT BUY at the floored tick with ceil'd quantity.
        let requests = server.received_requests().await.unwrap();
        let test = requests
            .iter()
            .find(|r| r.url.path() == "/api/v3/order/test")
            .unwrap();
        let query = format!(
            "{}&{}",
            test.url.query().unwrap_or(""),
            String::from_utf8_lossy(&test.body)
        );
        assert!(query.contains("type=LIMIT"), "{query}");
        assert!(query.contains("price=50000.12"), "{query}");
        assert!(query.contains("quantity=0.00011"), "{query}");

        let json = report.to_json();
        assert_eq!(json["capabilities"][0]["capability"], "reading");
        assert_eq!(
            json["capabilities"][0]["error_code"],
            serde_json::Value::Null
        );
        assert_eq!(json["features"][0]["feature"], "spot_live_canary");
        assert!(
            render_report(&report)
                .contains("  - spot_live_canary: allowed - all required capabilities passed")
        );
    }

    #[tokio::test]
    async fn permission_error_marks_reading_missing_and_ip_detail() {
        let server = MockServer::start().await;
        mount_time(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v3/account"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "code": -2015, "msg": "Invalid API-key, IP, or permissions for action."
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server);
        let report = inspect_api_permissions(&settings(), &client, &InspectOptions::default())
            .await
            .unwrap();
        let reading = report.capability(Capability::Reading).unwrap();
        assert_eq!(reading.status, CapabilityStatus::Missing);
        assert_eq!(reading.error_code, Some(-2015));
        assert_eq!(reading.http_status, Some(401));
        let spot = report.capability(Capability::SpotTrading).unwrap();
        assert_eq!(spot.status, CapabilityStatus::Unknown);
        assert_eq!(spot.source, "account");
        assert!(
            report
                .capability(Capability::IpRestriction)
                .unwrap()
                .detail
                .contains("-2015-style")
        );
        assert_eq!(report.features.len(), Feature::ALL.len());
        assert!(report.features.iter().all(|f| !f.allowed));
        assert_eq!(report.can_trade, None);
    }

    #[tokio::test]
    async fn skip_spot_order_test_and_rejected_order_test() {
        let server = MockServer::start().await;
        mount_time(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v3/account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accountType": "SPOT", "canTrade": true, "permissions": ["SPOT"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order/test"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -1013, "msg": "Filter failure: NOTIONAL"
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server);
        let options = InspectOptions {
            include_spot_order_test: false,
            features: vec![Feature::ClaudeSpotAutonomous],
            ..InspectOptions::default()
        };
        let report = inspect_api_permissions(&settings(), &client, &options)
            .await
            .unwrap();
        assert_eq!(
            report.capability(Capability::SpotTrading).unwrap().status,
            CapabilityStatus::Skipped
        );
        assert_eq!(report.spot_test_symbol, None);
        assert!(
            !report
                .feature(Feature::ClaudeSpotAutonomous)
                .unwrap()
                .allowed
        );

        let order = SpotOrderTest {
            symbol: "BTC/USDT".into(),
            side: Side::Sell,
            order_type: OrderType::Market,
            quantity: Decimal::ONE,
            price: None,
        };
        let err = ensure_spot_order_test_allowed(&client, &order)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("spot_trading permission check failed: unknown;"),
            "{err}"
        );
        assert!(err.ends_with("error_code=-1013 http_status=400"), "{err}");
    }
}

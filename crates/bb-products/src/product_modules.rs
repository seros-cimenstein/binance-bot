//! Advanced Binance product module registry and local activation gates
//! (port of `src/product_modules.py`).

use serde::Serialize;

use crate::api_permissions::{
    Capability, Feature, FeatureCheck, PermissionReport, evaluate_feature,
};

/// Lifecycle status of a product module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleStatus {
    DryRun,
    LiveDisabled,
    Planned,
}

impl ModuleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::LiveDisabled => "live_disabled",
            Self::Planned => "planned",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProductModuleError {
    #[error("unknown product module: {0}")]
    Unknown(String),
}

/// Static contract for one advanced Binance product capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductModule {
    pub module_id: &'static str,
    pub display_name: &'static str,
    pub feature_name: Feature,
    pub status: ModuleStatus,
    pub required_capabilities: &'static [Capability],
    pub strategy_interface: &'static str,
    pub market_data_source: &'static str,
    pub risk_model: &'static str,
    pub audit_trail: &'static str,
    pub activation_gate: &'static str,
    pub live_activation_enabled: bool,
    pub write_endpoints_enabled: &'static [&'static str],
    pub notes: &'static str,
}

/// Evaluated local gate state for one product module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductModuleCheck {
    pub module_id: &'static str,
    pub display_name: &'static str,
    pub feature_name: Feature,
    pub status: ModuleStatus,
    pub required_capabilities: Vec<Capability>,
    pub reporting_allowed: bool,
    pub feature_allowed: bool,
    pub feature_implemented: bool,
    pub live_activation_allowed: bool,
    pub live_activation_enabled: bool,
    pub missing_capabilities: Vec<Capability>,
    pub unknown_capabilities: Vec<Capability>,
    pub policy_blocked_capabilities: Vec<Capability>,
    pub detail: String,
    pub strategy_interface: &'static str,
    pub market_data_source: &'static str,
    pub risk_model: &'static str,
    pub audit_trail: &'static str,
    pub activation_gate: &'static str,
    pub write_endpoints_enabled: Vec<&'static str>,
}

const READ: &[Capability] = &[Capability::Reading];

#[allow(clippy::too_many_arguments)]
const fn module(
    module_id: &'static str,
    display_name: &'static str,
    feature_name: Feature,
    status: ModuleStatus,
    required_capabilities: &'static [Capability],
    strategy_interface: &'static str,
    market_data_source: &'static str,
    risk_model: &'static str,
    audit_trail: &'static str,
    activation_gate: &'static str,
    notes: &'static str,
) -> ProductModule {
    ProductModule {
        module_id,
        display_name,
        feature_name,
        status,
        required_capabilities,
        strategy_interface,
        market_data_source,
        risk_model,
        audit_trail,
        activation_gate,
        live_activation_enabled: false,
        write_endpoints_enabled: &[],
        notes,
    }
}

/// Registry in Python definition order.
pub static PRODUCT_MODULES: [ProductModule; 15] = [
    module(
        "margin_dry_run",
        "Margin Dry Run",
        Feature::MarginDryRun,
        ModuleStatus::DryRun,
        READ,
        "MarginDryRunStrategy",
        "read-only cross/isolated margin account, max-borrow, and interest endpoints",
        "local borrow, debt, interest, margin-level, and liquidation-buffer model",
        "local dry-run report and optional audit rows only",
        "requires --dry-run; --confirm-live-margin fails closed",
        "Phase 19 modeling only; no live margin write endpoint is enabled.",
    ),
    module(
        "margin_trading",
        "Live Margin Trading",
        Feature::MarginTrading,
        ModuleStatus::LiveDisabled,
        &[Capability::Reading, Capability::Margin],
        "MarginTradingStrategy",
        "margin account, borrow, interest, and spot/margin market data",
        "must define borrow caps, repayment, liquidation, exposure, and stop conditions",
        "must audit borrow, repay, order, transfer, reconciliation, and alert state",
        "requires a separate live-margin activation phase",
        "API margin permission alone must not activate this module.",
    ),
    module(
        "margin_canary_plan",
        "Margin Canary Plan",
        Feature::MarginCanaryPlan,
        ModuleStatus::DryRun,
        READ,
        "MarginCanaryPlanner",
        "read-only margin discovery, max-borrow, interest, and operator scope configuration",
        "borrow/debt caps, repayment intent, margin-level, liquidation-buffer, and emergency-stop checks",
        "foreground canary-plan audit rows only",
        "live mutation remains blocked pending a separate explicit execution approval",
        "Phase 24 planning slice only; no margin write endpoint is enabled.",
    ),
    module(
        "futures_dry_run",
        "USD-M Futures Dry Run",
        Feature::FuturesDryRun,
        ModuleStatus::DryRun,
        READ,
        "FuturesDryRunStrategy",
        "read-only USD-M account, position, exchange-rule, funding, symbol-config, and leverage-bracket endpoints",
        "local leverage, margin-mode, funding, liquidation-distance, reduce-only, and reconciliation model",
        "local dry-run report only",
        "requires --dry-run; --confirm-live-futures fails closed",
        "Phase 20 modeling only; no futures write endpoint is enabled.",
    ),
    module(
        "futures_trading",
        "Live USD-M Futures Trading",
        Feature::FuturesTrading,
        ModuleStatus::LiveDisabled,
        &[Capability::Reading, Capability::Futures],
        "FuturesTradingStrategy",
        "USD-M account, positions, funding, mark price, exchange rules, and leverage brackets",
        "must define leverage caps, liquidation buffer, margin mode, funding, reduce-only, and loss stops",
        "must audit leverage, margin mode, orders, fills, funding, reconciliation, and alerts",
        "requires a separate live-futures activation phase",
        "API futures permission alone must not activate this module.",
    ),
    module(
        "futures_canary_plan",
        "USD-M Futures Canary Plan",
        Feature::FuturesCanaryPlan,
        ModuleStatus::DryRun,
        READ,
        "FuturesCanaryPlanner",
        "read-only USD-M account, position, rules, mark-price, funding, and operator scope configuration",
        "notional, leverage, margin-mode, liquidation, maintenance-buffer, funding, exit-plan, and emergency-stop checks",
        "foreground canary-plan audit rows only",
        "live mutation remains blocked pending a separate explicit execution approval",
        "Phase 25 planning slice only; no futures write endpoint is enabled.",
    ),
    module(
        "transfer_dry_run",
        "Transfer Dry Run",
        Feature::TransferDryRun,
        ModuleStatus::DryRun,
        READ,
        "TransferDryRunPlanner",
        "read-only wallet, spot, funding, margin, futures, and transfer-history endpoints",
        "local wallet route, debt, activation, balance, and allocation-impact model",
        "local transfer-plan audit rows only",
        "requires --dry-run; --confirm-live-transfer fails closed",
        "Phase 21 planning only; no universal-transfer write endpoint is enabled.",
    ),
    module(
        "universal_transfer",
        "Live Universal Transfer",
        Feature::UniversalTransfer,
        ModuleStatus::LiveDisabled,
        &[Capability::Reading, Capability::UniversalTransfer],
        "CapitalTransferRouter",
        "wallet balances, account-type constraints, transfer availability, and transfer history",
        "must define transfer caps, reserves, debt checks, destination risk, and rollback handling",
        "must audit proposed, approved, submitted, failed, and reconciled transfers",
        "requires a separate live-transfer activation phase",
        "API universal-transfer permission alone must not activate this module.",
    ),
    module(
        "transfer_canary_plan",
        "Universal Transfer Canary Plan",
        Feature::TransferCanaryPlan,
        ModuleStatus::DryRun,
        READ,
        "TransferCanaryPlanner",
        "read-only wallet balances, account constraints, transfer history, and operator scope configuration",
        "wallet activation, balance, transfer-cap, margin-debt, isolated-symbol, and emergency-stop checks",
        "foreground canary-plan audit rows only",
        "live mutation remains blocked pending a separate explicit execution approval",
        "Phase 26 planning slice only; no universal-transfer write endpoint is enabled.",
    ),
    module(
        "unified_pool_dry_run",
        "Unified Pool Dry Run",
        Feature::UnifiedPoolDryRun,
        ModuleStatus::DryRun,
        READ,
        "UnifiedPoolValuation",
        "local holdings plus read-only spot, funding, margin, futures, transfer, and benchmark data",
        "local allocation, reserve, leverage, liquidity, volatility, debt, and PnL constraints",
        "local valuation and allocation-constraint report only",
        "local dry-run valuation only; no live allocator path",
        "Phase 22 valuation only; no order, margin, futures, or transfer write endpoint is enabled.",
    ),
    module(
        "withdrawal_dry_run",
        "Withdrawal Safety Dry Run",
        Feature::WithdrawalDryRun,
        ModuleStatus::DryRun,
        READ,
        "WithdrawalDryRunPlanner",
        "local allowlist, cap, approval, cooldown, emergency-stop, and operator inputs",
        "local address, network, daily/request cap, approval, cooldown, and operator checks",
        "local withdrawal-plan JSONL audit rows only",
        "requires --dry-run; --confirm-live-withdrawal fails closed",
        "Phase 27 planning only; no withdrawal endpoint is enabled.",
    ),
    module(
        "withdrawals",
        "Withdrawals",
        Feature::Withdrawals,
        ModuleStatus::LiveDisabled,
        &[Capability::Reading, Capability::Withdrawals],
        "WithdrawalController",
        "wallet balance and withdrawal metadata",
        "must define address allowlists, caps, delay windows, approvals, and emergency stop",
        "must audit request, approval, submission, reconciliation, and operator identity",
        "hard-blocked unless a future withdrawal-specific safety phase is approved",
        "Withdrawals remain blocked by policy.",
    ),
    module(
        "prediction_dry_run",
        "Prediction Trading Dry Run",
        Feature::PredictionDryRun,
        ModuleStatus::DryRun,
        READ,
        "PredictionDryRunStrategy",
        "local or injected read-only prediction market payloads",
        "local stake, settlement-window, liquidity, drawdown, availability, and emergency-stop checks",
        "local prediction discovery and plan report only",
        "requires --dry-run; --confirm-live-prediction fails closed",
        "Phase 28 modeling only; no prediction write endpoint is enabled.",
    ),
    module(
        "prediction_trading",
        "Prediction Trading",
        Feature::PredictionTrading,
        ModuleStatus::Planned,
        &[Capability::Reading, Capability::Prediction],
        "PredictionTradingStrategy",
        "prediction market data source not implemented",
        "must define market rules, settlement risk, stake caps, drawdown, and stop conditions",
        "must audit market selection, stake plan, submission, settlement, and alerts",
        "planned Phase 23+ module; no activation path exists",
        "Prediction permission alone must not activate this module.",
    ),
    module(
        "future_product_module",
        "Future Binance Product Placeholder",
        Feature::FutureProductModule,
        ModuleStatus::Planned,
        READ,
        "FutureProductStrategy",
        "must be defined when Binance exposes the product",
        "must be defined per product before implementation",
        "must be defined per product before implementation",
        "requires a new roadmap phase, tests, risk review, and explicit approval",
        "Placeholder for Binance products not yet modeled by this bot.",
    ),
];

/// All modules sorted by id (Python `list_product_modules`).
pub fn list_product_modules() -> Vec<&'static ProductModule> {
    let mut modules: Vec<&ProductModule> = PRODUCT_MODULES.iter().collect();
    modules.sort_by_key(|module| module.module_id);
    modules
}

/// One product module; fails closed for unknown identifiers.
pub fn get_product_module(module_id: &str) -> Result<&'static ProductModule, ProductModuleError> {
    let normalized = module_id.trim().to_lowercase().replace('-', "_");
    PRODUCT_MODULES
        .iter()
        .find(|module| module.module_id == normalized)
        .ok_or_else(|| ProductModuleError::Unknown(module_id.to_owned()))
}

/// Evaluate selected modules (all when `module_ids` is empty).
pub fn evaluate_product_modules(
    report: Option<&PermissionReport>,
    module_ids: &[String],
) -> Result<Vec<ProductModuleCheck>, ProductModuleError> {
    let modules = if module_ids.is_empty() {
        list_product_modules()
    } else {
        module_ids
            .iter()
            .map(|id| get_product_module(id))
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(modules
        .into_iter()
        .map(|module| evaluate_product_module(module, report))
        .collect())
}

/// Whether one product module may report or activate.
pub fn evaluate_product_module(
    product: &ProductModule,
    report: Option<&PermissionReport>,
) -> ProductModuleCheck {
    let feature = report.map(|report| {
        report
            .feature(product.feature_name)
            .cloned()
            .unwrap_or_else(|| evaluate_feature(&report.capabilities, product.feature_name))
    });
    let feature_allowed = feature.as_ref().is_some_and(|f| f.allowed);
    let detail = module_detail(product, feature.as_ref());
    let (missing, unknown, policy) = feature.map_or_else(Default::default, |f| {
        (
            f.missing_capabilities,
            f.unknown_capabilities,
            f.policy_blocked_capabilities,
        )
    });
    ProductModuleCheck {
        module_id: product.module_id,
        display_name: product.display_name,
        feature_name: product.feature_name,
        status: product.status,
        required_capabilities: product.required_capabilities.to_vec(),
        reporting_allowed: true,
        feature_allowed,
        feature_implemented: report.is_some() && product.feature_name.spec().implemented,
        live_activation_allowed: product.live_activation_enabled && feature_allowed,
        live_activation_enabled: product.live_activation_enabled,
        missing_capabilities: missing,
        unknown_capabilities: unknown,
        policy_blocked_capabilities: policy,
        detail,
        strategy_interface: product.strategy_interface,
        market_data_source: product.market_data_source,
        risk_model: product.risk_model,
        audit_trail: product.audit_trail,
        activation_gate: product.activation_gate,
        write_endpoints_enabled: product.write_endpoints_enabled.to_vec(),
    }
}

fn module_detail(product: &ProductModule, feature: Option<&FeatureCheck>) -> String {
    match feature {
        None => "local module metadata can be reported without exchange access; live activation \
                 remains blocked because no permission report was supplied"
            .to_owned(),
        Some(f) if product.live_activation_enabled && f.allowed => {
            "module activation gate and required capabilities passed".to_owned()
        }
        Some(f) if f.allowed => "feature is available only in its implemented non-live scope; \
                                 live activation remains disabled by the product-module gate"
            .to_owned(),
        Some(f) => format!("{}; live activation remains disabled", f.detail),
    }
}

/// Concise local module report.
pub fn render_product_module_report(checks: &[ProductModuleCheck]) -> String {
    let names = |caps: &[Capability]| {
        caps.iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec!["Advanced Binance product modules:".to_owned()];
    for check in checks {
        let live = if check.live_activation_allowed {
            "allowed"
        } else {
            "blocked"
        };
        let feature = if check.feature_allowed {
            "allowed"
        } else {
            "blocked"
        };
        lines.extend([
            format!(
                "  - {}: {} ({})",
                check.module_id,
                check.display_name,
                check.status.as_str()
            ),
            format!("    feature: {} ({feature})", check.feature_name),
            format!("    live activation: {live}"),
            format!("    permissions: {}", names(&check.required_capabilities)),
            format!("    interface: {}", check.strategy_interface),
            format!("    market data: {}", check.market_data_source),
            format!("    risk: {}", check.risk_model),
            format!("    audit: {}", check.audit_trail),
            format!("    gate: {}", check.activation_gate),
            format!("    detail: {}", check.detail),
        ]);
        for (label, caps) in [
            ("missing", &check.missing_capabilities),
            ("unknown", &check.unknown_capabilities),
            ("policy_blocked", &check.policy_blocked_capabilities),
        ] {
            if !caps.is_empty() {
                lines.push(format!("    {label}: {}", names(caps)));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_permissions::{CapabilityCheck, CapabilityStatus};

    fn report(caps: &[(Capability, CapabilityStatus)], features: &[Feature]) -> PermissionReport {
        let capabilities: Vec<CapabilityCheck> = caps
            .iter()
            .map(|(cap, status)| CapabilityCheck::new(*cap, *status, "test", "detail"))
            .collect();
        PermissionReport {
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            mode: "testnet".into(),
            account_type: Some("SPOT".into()),
            account_permissions: vec!["SPOT".into()],
            can_trade: Some(true),
            can_withdraw: Some(false),
            spot_test_symbol: None,
            features: features
                .iter()
                .map(|f| evaluate_feature(&capabilities, *f))
                .collect(),
            capabilities,
        }
    }

    #[test]
    fn registry_tracks_advanced_modules() {
        let modules = list_product_modules();
        assert_eq!(modules.len(), 15);
        let ids: Vec<&str> = modules.iter().map(|m| m.module_id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        for id in [
            "withdrawal_dry_run",
            "prediction_dry_run",
            "margin_canary_plan",
            "futures_canary_plan",
            "transfer_canary_plan",
            "prediction_trading",
            "future_product_module",
        ] {
            let module = get_product_module(id).unwrap();
            assert!(!module.live_activation_enabled);
            assert!(module.write_endpoints_enabled.is_empty());
            assert_eq!(module.feature_name.as_str(), id);
        }
        assert_eq!(
            get_product_module("prediction-trading").unwrap().status,
            ModuleStatus::Planned
        );
    }

    #[test]
    fn unknown_module_fails_closed() {
        let err = get_product_module("not-a-real-module").unwrap_err();
        assert!(err.to_string().contains("unknown product module"));
        assert!(evaluate_product_modules(None, &["not-real".into()]).is_err());
    }

    #[test]
    fn dry_run_modules_reportable_without_live_activation() {
        let check = evaluate_product_module(get_product_module("margin_dry_run").unwrap(), None);
        assert!(check.reporting_allowed);
        assert!(!check.feature_allowed);
        assert!(!check.live_activation_allowed);
        assert!(check.detail.contains("permission report was supplied"));
    }

    #[test]
    fn api_permission_alone_does_not_authorize_live_activation() {
        for (cap, feature, id) in [
            (
                Capability::Futures,
                Feature::FuturesTrading,
                "futures_trading",
            ),
            (
                Capability::Prediction,
                Feature::PredictionTrading,
                "prediction_trading",
            ),
        ] {
            let report = report(
                &[
                    (Capability::Reading, CapabilityStatus::Ok),
                    (cap, CapabilityStatus::Ok),
                ],
                &[feature],
            );
            let check = evaluate_product_module(get_product_module(id).unwrap(), Some(&report));
            assert!(!check.feature_allowed);
            assert!(!check.feature_implemented);
            assert!(!check.live_activation_allowed);
            assert!(check.detail.contains("feature is not implemented yet"));
        }
    }

    #[test]
    fn implemented_feature_without_report_row_is_evaluated() {
        let report = report(&[(Capability::Reading, CapabilityStatus::Ok)], &[]);
        let check =
            evaluate_product_module(get_product_module("margin_dry_run").unwrap(), Some(&report));
        assert!(check.feature_allowed);
        assert!(check.feature_implemented);
        assert!(!check.live_activation_allowed);
        assert!(check.detail.starts_with("feature is available only"));
    }

    #[test]
    fn render_reports_local_gates() {
        let checks = evaluate_product_modules(None, &["prediction_trading".into()]).unwrap();
        let rendered = render_product_module_report(&checks);
        assert!(rendered.contains("  - prediction_trading: Prediction Trading (planned)"));
        assert!(rendered.contains("live activation: blocked"));
        assert!(rendered.contains("permissions: reading, prediction"));
    }
}

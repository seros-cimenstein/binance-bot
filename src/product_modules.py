"""Phase 23 advanced Binance product module registry.

This module is local metadata and gating only. It defines how advanced Binance
product modules must describe their strategy interface, permissions, market
data, risk model, audit trail, and activation gate before any future activation
work. It does not call exchange endpoints.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import asdict, dataclass
from typing import Any, Sequence

from src.api_permissions import (
    FUTURES,
    MARGIN,
    PREDICTION,
    READING,
    UNIVERSAL_TRANSFER,
    WITHDRAWALS,
    FeatureCheck,
    PermissionReport,
    evaluate_feature,
)

MODULE_STATUS_DRY_RUN = "dry_run"
MODULE_STATUS_LIVE_DISABLED = "live_disabled"
MODULE_STATUS_PLANNED = "planned"


class ProductModuleError(RuntimeError):
    """Raised when an advanced product module gate cannot be evaluated."""


@dataclass(frozen=True, slots=True)
class ProductModule:
    """Static contract for one advanced Binance product capability."""

    module_id: str
    display_name: str
    feature_name: str
    status: str
    required_capabilities: tuple[str, ...]
    strategy_interface: str
    market_data_source: str
    risk_model: str
    audit_trail: str
    activation_gate: str
    live_activation_enabled: bool = False
    write_endpoints_enabled: tuple[str, ...] = ()
    notes: str = ""

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ProductModuleCheck:
    """Evaluated local gate state for one product module."""

    module_id: str
    display_name: str
    feature_name: str
    status: str
    required_capabilities: tuple[str, ...]
    reporting_allowed: bool
    feature_allowed: bool
    feature_implemented: bool
    live_activation_allowed: bool
    live_activation_enabled: bool
    missing_capabilities: tuple[str, ...]
    unknown_capabilities: tuple[str, ...]
    policy_blocked_capabilities: tuple[str, ...]
    detail: str
    strategy_interface: str
    market_data_source: str
    risk_model: str
    audit_trail: str
    activation_gate: str
    write_endpoints_enabled: tuple[str, ...]

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


PRODUCT_MODULES: dict[str, ProductModule] = {
    "margin_dry_run": ProductModule(
        module_id="margin_dry_run",
        display_name="Margin Dry Run",
        feature_name="margin_dry_run",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="MarginDryRunStrategy",
        market_data_source="read-only cross/isolated margin account, max-borrow, and interest endpoints",
        risk_model="local borrow, debt, interest, margin-level, and liquidation-buffer model",
        audit_trail="local dry-run report and optional audit rows only",
        activation_gate="requires --dry-run; --confirm-live-margin fails closed",
        notes="Phase 19 modeling only; no live margin write endpoint is enabled.",
    ),
    "margin_trading": ProductModule(
        module_id="margin_trading",
        display_name="Live Margin Trading",
        feature_name="margin_trading",
        status=MODULE_STATUS_LIVE_DISABLED,
        required_capabilities=(READING, MARGIN),
        strategy_interface="MarginTradingStrategy",
        market_data_source="margin account, borrow, interest, and spot/margin market data",
        risk_model="must define borrow caps, repayment, liquidation, exposure, and stop conditions",
        audit_trail="must audit borrow, repay, order, transfer, reconciliation, and alert state",
        activation_gate="requires a separate live-margin activation phase",
        notes="API margin permission alone must not activate this module.",
    ),
    "margin_canary_plan": ProductModule(
        module_id="margin_canary_plan",
        display_name="Margin Canary Plan",
        feature_name="margin_canary_plan",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="MarginCanaryPlanner",
        market_data_source="read-only margin discovery, max-borrow, interest, and operator scope configuration",
        risk_model="borrow/debt caps, repayment intent, margin-level, liquidation-buffer, and emergency-stop checks",
        audit_trail="foreground canary-plan audit rows only",
        activation_gate="live mutation remains blocked pending a separate explicit execution approval",
        notes="Phase 24 planning slice only; no margin write endpoint is enabled.",
    ),
    "futures_dry_run": ProductModule(
        module_id="futures_dry_run",
        display_name="USD-M Futures Dry Run",
        feature_name="futures_dry_run",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="FuturesDryRunStrategy",
        market_data_source="read-only USD-M account, position, exchange-rule, funding, symbol-config, and leverage-bracket endpoints",
        risk_model="local leverage, margin-mode, funding, liquidation-distance, reduce-only, and reconciliation model",
        audit_trail="local dry-run report only",
        activation_gate="requires --dry-run; --confirm-live-futures fails closed",
        notes="Phase 20 modeling only; no futures write endpoint is enabled.",
    ),
    "futures_trading": ProductModule(
        module_id="futures_trading",
        display_name="Live USD-M Futures Trading",
        feature_name="futures_trading",
        status=MODULE_STATUS_LIVE_DISABLED,
        required_capabilities=(READING, FUTURES),
        strategy_interface="FuturesTradingStrategy",
        market_data_source="USD-M account, positions, funding, mark price, exchange rules, and leverage brackets",
        risk_model="must define leverage caps, liquidation buffer, margin mode, funding, reduce-only, and loss stops",
        audit_trail="must audit leverage, margin mode, orders, fills, funding, reconciliation, and alerts",
        activation_gate="requires a separate live-futures activation phase",
        notes="API futures permission alone must not activate this module.",
    ),
    "futures_canary_plan": ProductModule(
        module_id="futures_canary_plan",
        display_name="USD-M Futures Canary Plan",
        feature_name="futures_canary_plan",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="FuturesCanaryPlanner",
        market_data_source="read-only USD-M account, position, rules, mark-price, funding, and operator scope configuration",
        risk_model="notional, leverage, margin-mode, liquidation, maintenance-buffer, funding, exit-plan, and emergency-stop checks",
        audit_trail="foreground canary-plan audit rows only",
        activation_gate="live mutation remains blocked pending a separate explicit execution approval",
        notes="Phase 25 planning slice only; no futures write endpoint is enabled.",
    ),
    "transfer_dry_run": ProductModule(
        module_id="transfer_dry_run",
        display_name="Transfer Dry Run",
        feature_name="transfer_dry_run",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="TransferDryRunPlanner",
        market_data_source="read-only wallet, spot, funding, margin, futures, and transfer-history endpoints",
        risk_model="local wallet route, debt, activation, balance, and allocation-impact model",
        audit_trail="local transfer-plan audit rows only",
        activation_gate="requires --dry-run; --confirm-live-transfer fails closed",
        notes="Phase 21 planning only; no universal-transfer write endpoint is enabled.",
    ),
    "universal_transfer": ProductModule(
        module_id="universal_transfer",
        display_name="Live Universal Transfer",
        feature_name="universal_transfer",
        status=MODULE_STATUS_LIVE_DISABLED,
        required_capabilities=(READING, UNIVERSAL_TRANSFER),
        strategy_interface="CapitalTransferRouter",
        market_data_source="wallet balances, account-type constraints, transfer availability, and transfer history",
        risk_model="must define transfer caps, reserves, debt checks, destination risk, and rollback handling",
        audit_trail="must audit proposed, approved, submitted, failed, and reconciled transfers",
        activation_gate="requires a separate live-transfer activation phase",
        notes="API universal-transfer permission alone must not activate this module.",
    ),
    "transfer_canary_plan": ProductModule(
        module_id="transfer_canary_plan",
        display_name="Universal Transfer Canary Plan",
        feature_name="transfer_canary_plan",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="TransferCanaryPlanner",
        market_data_source="read-only wallet balances, account constraints, transfer history, and operator scope configuration",
        risk_model="wallet activation, balance, transfer-cap, margin-debt, isolated-symbol, and emergency-stop checks",
        audit_trail="foreground canary-plan audit rows only",
        activation_gate="live mutation remains blocked pending a separate explicit execution approval",
        notes="Phase 26 planning slice only; no universal-transfer write endpoint is enabled.",
    ),
    "unified_pool_dry_run": ProductModule(
        module_id="unified_pool_dry_run",
        display_name="Unified Pool Dry Run",
        feature_name="unified_pool_dry_run",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="UnifiedPoolValuation",
        market_data_source="local holdings plus read-only spot, funding, margin, futures, transfer, and benchmark data",
        risk_model="local allocation, reserve, leverage, liquidity, volatility, debt, and PnL constraints",
        audit_trail="local valuation and allocation-constraint report only",
        activation_gate="local dry-run valuation only; no live allocator path",
        notes="Phase 22 valuation only; no order, margin, futures, or transfer write endpoint is enabled.",
    ),
    "withdrawal_dry_run": ProductModule(
        module_id="withdrawal_dry_run",
        display_name="Withdrawal Safety Dry Run",
        feature_name="withdrawal_dry_run",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="WithdrawalDryRunPlanner",
        market_data_source="local allowlist, cap, approval, cooldown, emergency-stop, and operator inputs",
        risk_model="local address, network, daily/request cap, approval, cooldown, and operator checks",
        audit_trail="local withdrawal-plan JSONL audit rows only",
        activation_gate="requires --dry-run; --confirm-live-withdrawal fails closed",
        notes="Phase 27 planning only; no withdrawal endpoint is enabled.",
    ),
    "withdrawals": ProductModule(
        module_id="withdrawals",
        display_name="Withdrawals",
        feature_name="withdrawals",
        status=MODULE_STATUS_LIVE_DISABLED,
        required_capabilities=(READING, WITHDRAWALS),
        strategy_interface="WithdrawalController",
        market_data_source="wallet balance and withdrawal metadata",
        risk_model="must define address allowlists, caps, delay windows, approvals, and emergency stop",
        audit_trail="must audit request, approval, submission, reconciliation, and operator identity",
        activation_gate="hard-blocked unless a future withdrawal-specific safety phase is approved",
        notes="Withdrawals remain blocked by policy.",
    ),
    "prediction_dry_run": ProductModule(
        module_id="prediction_dry_run",
        display_name="Prediction Trading Dry Run",
        feature_name="prediction_dry_run",
        status=MODULE_STATUS_DRY_RUN,
        required_capabilities=(READING,),
        strategy_interface="PredictionDryRunStrategy",
        market_data_source="local or injected read-only prediction market payloads",
        risk_model="local stake, settlement-window, liquidity, drawdown, availability, and emergency-stop checks",
        audit_trail="local prediction discovery and plan report only",
        activation_gate="requires --dry-run; --confirm-live-prediction fails closed",
        notes="Phase 28 modeling only; no prediction write endpoint is enabled.",
    ),
    "prediction_trading": ProductModule(
        module_id="prediction_trading",
        display_name="Prediction Trading",
        feature_name="prediction_trading",
        status=MODULE_STATUS_PLANNED,
        required_capabilities=(READING, PREDICTION),
        strategy_interface="PredictionTradingStrategy",
        market_data_source="prediction market data source not implemented",
        risk_model="must define market rules, settlement risk, stake caps, drawdown, and stop conditions",
        audit_trail="must audit market selection, stake plan, submission, settlement, and alerts",
        activation_gate="planned Phase 23+ module; no activation path exists",
        notes="Prediction permission alone must not activate this module.",
    ),
    "future_product_module": ProductModule(
        module_id="future_product_module",
        display_name="Future Binance Product Placeholder",
        feature_name="future_product_module",
        status=MODULE_STATUS_PLANNED,
        required_capabilities=(READING,),
        strategy_interface="FutureProductStrategy",
        market_data_source="must be defined when Binance exposes the product",
        risk_model="must be defined per product before implementation",
        audit_trail="must be defined per product before implementation",
        activation_gate="requires a new roadmap phase, tests, risk review, and explicit approval",
        notes="Placeholder for Binance products not yet modeled by this bot.",
    ),
}


def list_product_modules() -> tuple[ProductModule, ...]:
    """Return all known product modules in stable order."""
    return tuple(PRODUCT_MODULES[module_id] for module_id in sorted(PRODUCT_MODULES))


def get_product_module(module_id: str) -> ProductModule:
    """Return one product module or fail closed for unknown identifiers."""
    normalized = _normalize_module_id(module_id)
    try:
        return PRODUCT_MODULES[normalized]
    except KeyError as exc:
        raise ProductModuleError(f"unknown product module: {module_id}") from exc


def evaluate_product_modules(
    *,
    report: PermissionReport | None = None,
    module_ids: Sequence[str] | None = None,
) -> tuple[ProductModuleCheck, ...]:
    """Evaluate one or more product modules against an optional permission report."""
    modules = (
        [get_product_module(module_id) for module_id in module_ids]
        if module_ids
        else list_product_modules()
    )
    return tuple(evaluate_product_module(module, report=report) for module in modules)


def evaluate_product_module(
    module: str | ProductModule,
    *,
    report: PermissionReport | None = None,
) -> ProductModuleCheck:
    """Evaluate whether one product module may report or activate."""
    product = get_product_module(module) if isinstance(module, str) else module
    feature = _feature_from_report(product, report)
    feature_allowed = bool(feature and feature.allowed)
    feature_implemented = bool(feature and feature.implemented)
    live_activation_allowed = product.live_activation_enabled and feature_allowed
    detail = _module_detail(product, feature)

    return ProductModuleCheck(
        module_id=product.module_id,
        display_name=product.display_name,
        feature_name=product.feature_name,
        status=product.status,
        required_capabilities=product.required_capabilities,
        reporting_allowed=True,
        feature_allowed=feature_allowed,
        feature_implemented=feature_implemented,
        live_activation_allowed=live_activation_allowed,
        live_activation_enabled=product.live_activation_enabled,
        missing_capabilities=feature.missing_capabilities if feature else (),
        unknown_capabilities=feature.unknown_capabilities if feature else (),
        policy_blocked_capabilities=feature.policy_blocked_capabilities if feature else (),
        detail=detail,
        strategy_interface=product.strategy_interface,
        market_data_source=product.market_data_source,
        risk_model=product.risk_model,
        audit_trail=product.audit_trail,
        activation_gate=product.activation_gate,
        write_endpoints_enabled=product.write_endpoints_enabled,
    )


def render_product_module_report(checks: Sequence[ProductModuleCheck]) -> str:
    """Render a concise local module report."""
    lines = ["Advanced Binance product modules:"]
    for check in checks:
        live_status = "allowed" if check.live_activation_allowed else "blocked"
        feature_status = "allowed" if check.feature_allowed else "blocked"
        lines.extend(
            [
                f"  - {check.module_id}: {check.display_name} ({check.status})",
                f"    feature: {check.feature_name} ({feature_status})",
                f"    live activation: {live_status}",
                f"    permissions: {', '.join(check.required_capabilities)}",
                f"    interface: {check.strategy_interface}",
                f"    market data: {check.market_data_source}",
                f"    risk: {check.risk_model}",
                f"    audit: {check.audit_trail}",
                f"    gate: {check.activation_gate}",
                f"    detail: {check.detail}",
            ]
        )
        if check.missing_capabilities:
            lines.append("    missing: " + ", ".join(check.missing_capabilities))
        if check.unknown_capabilities:
            lines.append("    unknown: " + ", ".join(check.unknown_capabilities))
        if check.policy_blocked_capabilities:
            lines.append(
                "    policy_blocked: "
                + ", ".join(check.policy_blocked_capabilities)
            )
    return "\n".join(lines)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Inspect local advanced Binance product module gates."
    )
    parser.add_argument(
        "--module",
        action="append",
        help="module id to report; may be passed more than once",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print module report as JSON",
    )
    args = parser.parse_args(argv)
    try:
        checks = evaluate_product_modules(module_ids=args.module)
    except Exception as exc:
        print(f"Product module check failed: {exc}")
        return 1
    if args.json:
        print(json.dumps([check.as_dict() for check in checks], indent=2, sort_keys=True))
    else:
        print(render_product_module_report(checks))
    return 0


def _feature_from_report(
    product: ProductModule,
    report: PermissionReport | None,
) -> FeatureCheck | None:
    if report is None:
        return None
    try:
        return report.feature(product.feature_name)
    except KeyError:
        capabilities = {
            capability.capability: capability for capability in report.capabilities
        }
        return evaluate_feature(capabilities, product.feature_name)


def _module_detail(
    product: ProductModule,
    feature: FeatureCheck | None,
) -> str:
    if feature is None:
        return (
            "local module metadata can be reported without exchange access; "
            "live activation remains blocked because no permission report was supplied"
        )
    if product.live_activation_enabled and feature.allowed:
        return "module activation gate and required capabilities passed"
    if feature.allowed:
        return (
            "feature is available only in its implemented non-live scope; "
            "live activation remains disabled by the product-module gate"
        )
    return f"{feature.detail}; live activation remains disabled"


def _normalize_module_id(module_id: str) -> str:
    return module_id.strip().lower().replace("-", "_")


if __name__ == "__main__":
    raise SystemExit(run())

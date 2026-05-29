from __future__ import annotations

from src.api_permissions import (
    FUTURES,
    PREDICTION,
    READING,
    STATUS_OK,
    CapabilityCheck,
    PermissionReport,
    evaluate_feature,
)
from src.product_modules import (
    ProductModuleError,
    evaluate_product_module,
    evaluate_product_modules,
    get_product_module,
    list_product_modules,
    render_product_module_report,
    run,
)


def _report(
    capabilities: tuple[CapabilityCheck, ...],
    *,
    features: tuple[str, ...],
) -> PermissionReport:
    capability_map = {
        capability.capability: capability for capability in capabilities
    }
    return PermissionReport(
        timestamp="2026-05-26T00:00:00+00:00",
        mode="LIVE",
        account_type="SPOT",
        account_permissions=("SPOT",),
        can_trade=True,
        can_withdraw=False,
        spot_test_symbol="BTC/USDT",
        capabilities=capabilities,
        features=tuple(evaluate_feature(capability_map, feature) for feature in features),
    )


def test_product_module_registry_tracks_advanced_modules() -> None:
    modules = {module.module_id: module for module in list_product_modules()}

    assert "withdrawal_dry_run" in modules
    assert "prediction_dry_run" in modules
    assert "margin_canary_plan" in modules
    assert "futures_canary_plan" in modules
    assert "transfer_canary_plan" in modules
    assert "prediction_trading" in modules
    assert "future_product_module" in modules
    assert modules["withdrawal_dry_run"].live_activation_enabled is False
    assert modules["prediction_dry_run"].feature_name == "prediction_dry_run"
    assert modules["prediction_dry_run"].status == "dry_run"
    assert modules["margin_canary_plan"].live_activation_enabled is False
    assert modules["futures_canary_plan"].write_endpoints_enabled == ()
    assert modules["transfer_canary_plan"].status == "dry_run"
    assert modules["prediction_trading"].feature_name == "prediction_trading"
    assert modules["prediction_trading"].strategy_interface
    assert modules["prediction_trading"].market_data_source
    assert modules["prediction_trading"].risk_model
    assert modules["prediction_trading"].audit_trail
    assert modules["prediction_trading"].activation_gate
    assert modules["prediction_trading"].live_activation_enabled is False


def test_unknown_product_module_fails_closed() -> None:
    try:
        get_product_module("not-a-real-module")
    except ProductModuleError as exc:
        assert "unknown product module" in str(exc)
    else:
        raise AssertionError("unknown product module should fail closed")


def test_dry_run_modules_are_reportable_without_live_activation() -> None:
    check = evaluate_product_module("margin_dry_run")

    assert check.reporting_allowed is True
    assert check.feature_allowed is False
    assert check.live_activation_allowed is False
    assert check.write_endpoints_enabled == ()
    assert "permission report was supplied" in check.detail


def test_api_permission_alone_does_not_authorize_live_activation() -> None:
    report = _report(
        (
            CapabilityCheck(
                READING,
                STATUS_OK,
                "test",
                "reading ok",
            ),
            CapabilityCheck(
                FUTURES,
                STATUS_OK,
                "test",
                "futures permission enabled",
            ),
        ),
        features=("futures_trading",),
    )

    check = evaluate_product_module("futures_trading", report=report)

    assert check.feature_allowed is False
    assert check.feature_implemented is False
    assert check.live_activation_allowed is False
    assert check.live_activation_enabled is False
    assert "feature is not implemented yet" in check.detail


def test_policy_blocked_future_modules_remain_disabled_even_with_permission() -> None:
    report = _report(
        (
            CapabilityCheck(
                READING,
                STATUS_OK,
                "test",
                "reading ok",
            ),
            CapabilityCheck(
                PREDICTION,
                STATUS_OK,
                "test",
                "prediction permission enabled",
            ),
        ),
        features=("prediction_trading",),
    )

    check = evaluate_product_module("prediction_trading", report=report)

    assert check.feature_allowed is False
    assert check.feature_implemented is False
    assert check.live_activation_allowed is False
    assert "feature is not implemented yet" in check.detail


def test_render_and_cli_report_local_module_gates(capsys) -> None:
    checks = evaluate_product_modules(module_ids=("prediction_trading",))
    rendered = render_product_module_report(checks)

    assert "prediction_trading" in rendered
    assert "live activation: blocked" in rendered
    assert run(["--module", "prediction_trading"]) == 0
    assert "Prediction Trading" in capsys.readouterr().out


def test_cli_fails_closed_for_unknown_module(capsys) -> None:
    assert run(["--module", "not-real"]) == 1
    assert "unknown product module" in capsys.readouterr().out

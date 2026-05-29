"""Foreground USD-M futures canary planning with live execution disabled.

Phase 25 begins as a plan-only readiness slice. It reads through the existing
USD-M futures model, records a proposed entry and its mandatory reduce-only
exit intent, and fails closed for every submission or live-confirmation flag.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
from dataclasses import asdict, dataclass, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping

from src.config import Settings
from src.data.feeds import normalize_symbol
from src.exchange.client import BinanceClient
from src.futures import (
    ORDER_LIMIT,
    ORDER_MARKET,
    POSITION_BOTH,
    POSITION_LONG,
    POSITION_SHORT,
    SIDE_BUY,
    SIDE_SELL,
    FuturesOrderIntent,
    FuturesRiskLimits,
    build_futures_dry_run_report,
)
from src.scope_audit import ScopeAuditError, build_scope_audit_report, ensure_scope_allowed


class FuturesCanaryError(RuntimeError):
    """Raised when a plan-only futures canary cannot proceed safely."""


@dataclass(frozen=True, slots=True)
class FuturesCanaryControls:
    """Operational controls for one foreground planning run."""

    audit_path: str = "data/futures-canary.jsonl"
    emergency_stop_path: str = "data/futures-canary.stop"


@dataclass(frozen=True, slots=True)
class FuturesExitPlanIntent:
    """Mandatory closure intent for an entry plan, never submitted here."""

    symbol: str
    side: str
    quantity: float
    trigger: str
    order_type: str = ORDER_MARKET
    price: float | None = None
    position_side: str = POSITION_BOTH
    reduce_only: bool = True

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesCanaryResult:
    """Auditable result from one plan-only foreground canary run."""

    timestamp: str
    mode: str
    status: str
    symbol: str
    entry_plan: dict[str, Any]
    exit_plan_intent: dict[str, Any]
    reconciliation: dict[str, Any]
    scope_report: dict[str, Any]
    rejection_reasons: tuple[str, ...] = ()
    submission_attempted: bool = False
    live_execution_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()
    audit_path: str = ""

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def run_futures_canary(
    settings: Settings,
    *,
    intent: FuturesOrderIntent,
    exit_plan_intent: FuturesExitPlanIntent,
    limits: FuturesRiskLimits | None = None,
    controls: FuturesCanaryControls | None = None,
    submit_order: bool = False,
    confirm_live_futures: bool = False,
    client: Any | None = None,
) -> FuturesCanaryResult:
    """Plan a USD-M canary entry and documented reduce-only exit.

    This function intentionally has no order-submission branch. The injected
    client is used only by ``src.futures`` read-only discovery.
    """
    _assert_plan_only(
        submit_order=submit_order,
        confirm_live_futures=confirm_live_futures,
    )
    controls = controls or FuturesCanaryControls()
    _validate_controls(controls)
    _ensure_no_emergency_stop(controls.emergency_stop_path)
    symbol = _normalize_symbol(intent.symbol)
    _validate_exit_plan(intent, exit_plan_intent)
    try:
        scope_report = ensure_scope_allowed(
            build_scope_audit_report(settings, futures_symbols=(symbol,))
        )
    except ScopeAuditError as exc:
        raise FuturesCanaryError(str(exc)) from exc

    owns_client = client is None
    client = client or BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    try:
        report = await build_futures_dry_run_report(
            settings,
            client=client,
            symbols=(symbol,),
            intent=intent,
            limits=limits,
        )
    finally:
        if owns_client:
            await client.close()

    if report.live_trading_enabled or report.write_endpoints_used:
        raise FuturesCanaryError("read-only futures model reported a live write path")
    if report.order_plan is None or report.reconciliation is None:
        raise FuturesCanaryError("futures canary planning did not produce a complete report")

    rejection_reasons = _combined_rejections(
        report.order_plan,
        report.reconciliation,
    )
    allowed = (
        bool(report.order_plan.get("allowed"))
        and bool(report.reconciliation.get("allowed"))
        and not rejection_reasons
    )
    result = FuturesCanaryResult(
        timestamp=_utc_timestamp(),
        mode=_mode(settings),
        status="planned" if allowed else "blocked",
        symbol=symbol,
        entry_plan=report.order_plan,
        exit_plan_intent=_normalized_exit_plan(exit_plan_intent),
        reconciliation=report.reconciliation,
        scope_report=scope_report.as_dict(),
        rejection_reasons=rejection_reasons,
    )
    result = replace(result, audit_path=str(Path(controls.audit_path)))
    _write_audit(Path(controls.audit_path), result)
    return result


def run(argv: list[str] | None = None) -> int:
    """Command-line entry point for plan-only foreground futures readiness."""
    defaults = FuturesRiskLimits()
    controls = FuturesCanaryControls()
    parser = argparse.ArgumentParser(
        description="Plan a foreground USD-M futures canary; live execution is disabled."
    )
    parser.add_argument(
        "--plan-only",
        "--dry-run",
        dest="plan_only",
        action="store_true",
        help="required; permits read-only planning and JSONL audit output",
    )
    parser.add_argument(
        "--submit-order",
        "--submit",
        dest="submit_order",
        action="store_true",
        help="always fails; this slice has no futures submission path",
    )
    parser.add_argument(
        "--confirm-live-futures",
        action="store_true",
        help="always fails; live futures execution is not approved",
    )
    parser.add_argument("--symbol", help="USD-M symbol, for example BTC/USDT")
    parser.add_argument("--side", choices=(SIDE_BUY, SIDE_SELL), default=SIDE_BUY)
    parser.add_argument("--quantity", type=float, help="entry quantity to plan")
    parser.add_argument(
        "--order-type",
        choices=(ORDER_MARKET, ORDER_LIMIT),
        default=ORDER_MARKET,
    )
    parser.add_argument("--price", type=float, help="entry limit price")
    parser.add_argument(
        "--position-side",
        choices=(POSITION_BOTH, POSITION_LONG, POSITION_SHORT),
        default=POSITION_BOTH,
    )
    parser.add_argument("--requested-leverage", type=float)
    parser.add_argument(
        "--exit-side",
        choices=(SIDE_BUY, SIDE_SELL),
        help="required opposite-side closure intent",
    )
    parser.add_argument("--exit-quantity", type=float, help="defaults to entry quantity")
    parser.add_argument(
        "--exit-order-type",
        choices=(ORDER_MARKET, ORDER_LIMIT),
        default=ORDER_MARKET,
    )
    parser.add_argument("--exit-price", type=float, help="exit limit price")
    parser.add_argument(
        "--exit-reduce-only",
        action="store_true",
        help="required; documents that the exit may only reduce the canary position",
    )
    parser.add_argument(
        "--exit-trigger",
        help="required operator-readable trigger or condition for the exit intent",
    )
    parser.add_argument("--audit-path", default=controls.audit_path)
    parser.add_argument("--emergency-stop-path", default=controls.emergency_stop_path)
    parser.add_argument("--max-leverage", type=float, default=defaults.max_leverage)
    parser.add_argument(
        "--max-notional-usdt",
        type=float,
        default=defaults.max_notional_usdt,
    )
    parser.add_argument(
        "--max-account-notional-percent",
        type=float,
        default=defaults.max_account_notional_percent,
    )
    parser.add_argument(
        "--max-initial-margin-usdt",
        type=float,
        default=defaults.max_initial_margin_usdt,
    )
    parser.add_argument(
        "--min-free-balance-after-initial-margin-usdt",
        type=float,
        default=defaults.min_free_balance_after_initial_margin_usdt,
    )
    parser.add_argument(
        "--min-liquidation-distance-percent",
        type=float,
        default=defaults.min_liquidation_distance_percent,
    )
    parser.add_argument(
        "--max-abs-funding-rate",
        type=float,
        default=defaults.max_abs_funding_rate,
    )
    parser.add_argument(
        "--max-abs-funding-payment-usdt",
        type=float,
        default=defaults.max_abs_funding_payment_usdt,
    )
    parser.add_argument(
        "--min-maintenance-buffer-percent",
        type=float,
        default=defaults.min_maintenance_buffer_percent,
    )
    parser.add_argument("--json", action="store_true", help="print JSON output")
    args = parser.parse_args(argv)

    if args.submit_order or args.confirm_live_futures:
        print("Futures canary failed: live futures submission is disabled")
        return 1
    if not args.plan_only:
        print("Futures canary failed: --plan-only is required")
        return 1
    if args.quantity is None:
        print("Futures canary failed: --quantity is required")
        return 1
    if not args.exit_side or not args.exit_reduce_only or not args.exit_trigger:
        print(
            "Futures canary failed: --exit-side, --exit-reduce-only, "
            "and --exit-trigger are required"
        )
        return 1
    try:
        settings = Settings.from_env()
        symbol = _normalize_symbol(args.symbol or settings.trading_symbol)
        limits = FuturesRiskLimits(
            max_leverage=args.max_leverage,
            max_notional_usdt=args.max_notional_usdt,
            max_account_notional_percent=args.max_account_notional_percent,
            max_initial_margin_usdt=args.max_initial_margin_usdt,
            min_free_balance_after_initial_margin_usdt=(
                args.min_free_balance_after_initial_margin_usdt
            ),
            min_liquidation_distance_percent=args.min_liquidation_distance_percent,
            max_abs_funding_rate=args.max_abs_funding_rate,
            max_abs_funding_payment_usdt=args.max_abs_funding_payment_usdt,
            min_maintenance_buffer_percent=args.min_maintenance_buffer_percent,
        )
        result = asyncio.run(
            run_futures_canary(
                settings,
                intent=FuturesOrderIntent(
                    symbol=symbol,
                    side=args.side,
                    quantity=args.quantity,
                    order_type=args.order_type,
                    price=args.price,
                    position_side=args.position_side,
                    requested_leverage=args.requested_leverage,
                ),
                exit_plan_intent=FuturesExitPlanIntent(
                    symbol=symbol,
                    side=args.exit_side,
                    quantity=(
                        args.exit_quantity
                        if args.exit_quantity is not None
                        else args.quantity
                    ),
                    trigger=args.exit_trigger,
                    order_type=args.exit_order_type,
                    price=args.exit_price,
                    position_side=args.position_side,
                    reduce_only=args.exit_reduce_only,
                ),
                limits=limits,
                controls=FuturesCanaryControls(
                    audit_path=args.audit_path,
                    emergency_stop_path=args.emergency_stop_path,
                ),
            )
        )
        if args.json:
            print(json.dumps(result.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_futures_canary_result(result))
    except Exception as exc:
        print(f"Futures canary failed: {exc}")
        return 1
    return 0 if result.status == "planned" else 1


def render_futures_canary_result(result: FuturesCanaryResult) -> str:
    """Render a concise plan-only canary result."""
    plan = result.entry_plan
    lines = [
        f"Mode: {result.mode}",
        "Live futures execution: disabled",
        "Write endpoints used: none",
        f"Status: {result.status}",
        f"Symbol: {result.symbol}",
        f"Entry side/quantity: {plan.get('side')} {plan.get('quantity')}",
        f"Entry notional USDT: {plan.get('notional_usdt')}",
        f"Configured leverage: {plan.get('configured_leverage')}",
        "Exit intent: reduce-only "
        f"{result.exit_plan_intent.get('side')} {result.exit_plan_intent.get('quantity')}",
        f"Exit trigger: {result.exit_plan_intent.get('trigger')}",
        f"Audit path: {result.audit_path}",
    ]
    if result.rejection_reasons:
        lines.append("Rejection reasons: " + ", ".join(result.rejection_reasons))
    return "\n".join(lines)


def _assert_plan_only(*, submit_order: bool, confirm_live_futures: bool) -> None:
    if submit_order:
        raise FuturesCanaryError(
            "USD-M futures canary is plan-only; order submission is disabled"
        )
    if confirm_live_futures:
        raise FuturesCanaryError(
            "USD-M futures canary is plan-only; live confirmation is disabled"
        )


def _validate_exit_plan(
    intent: FuturesOrderIntent,
    exit_plan_intent: FuturesExitPlanIntent,
) -> None:
    symbol = _normalize_symbol(intent.symbol)
    if _normalize_symbol(exit_plan_intent.symbol) != symbol:
        raise FuturesCanaryError("exit plan symbol must match the entry symbol")
    entry_side = _side(intent.side, "entry side")
    exit_side = _side(exit_plan_intent.side, "exit side")
    if exit_side == entry_side:
        raise FuturesCanaryError("exit plan side must oppose the entry side")
    if not exit_plan_intent.reduce_only:
        raise FuturesCanaryError("exit plan intent must be reduce-only")
    if _position_side(exit_plan_intent.position_side) != _position_side(intent.position_side):
        raise FuturesCanaryError("exit plan position_side must match the entry intent")
    entry_quantity = _positive_number(intent.quantity, "entry quantity")
    exit_quantity = _positive_number(exit_plan_intent.quantity, "exit quantity")
    if not math.isclose(entry_quantity, exit_quantity, rel_tol=0.0, abs_tol=1e-12):
        raise FuturesCanaryError("exit plan quantity must fully close the planned entry")
    if not exit_plan_intent.trigger.strip():
        raise FuturesCanaryError("exit plan trigger must not be blank")
    _order_type(exit_plan_intent.order_type)
    if exit_plan_intent.order_type.strip().upper() == ORDER_LIMIT:
        _positive_number(exit_plan_intent.price, "exit price")


def _normalized_exit_plan(exit_plan_intent: FuturesExitPlanIntent) -> dict[str, Any]:
    return {
        **exit_plan_intent.as_dict(),
        "symbol": _normalize_symbol(exit_plan_intent.symbol),
        "side": _side(exit_plan_intent.side, "exit side"),
        "position_side": _position_side(exit_plan_intent.position_side),
        "order_type": _order_type(exit_plan_intent.order_type),
        "trigger": exit_plan_intent.trigger.strip(),
    }


def _combined_rejections(
    entry_plan: Mapping[str, Any],
    reconciliation: Mapping[str, Any],
) -> tuple[str, ...]:
    reasons: list[str] = []
    for reason in entry_plan.get("rejection_reasons", ()):
        text = str(reason)
        if text not in reasons:
            reasons.append(text)
    for reason in reconciliation.get("rejection_reasons", ()):
        text = str(reason)
        if text not in reasons:
            reasons.append(text)
    return tuple(reasons)


def _validate_controls(controls: FuturesCanaryControls) -> None:
    if not controls.audit_path.strip():
        raise FuturesCanaryError("audit_path must not be blank")
    if not controls.emergency_stop_path.strip():
        raise FuturesCanaryError("emergency_stop_path must not be blank")


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise FuturesCanaryError(f"USD-M futures canary emergency stop is active: {path}")


def _write_audit(path: Path, result: FuturesCanaryResult) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(result.as_dict(), sort_keys=True) + "\n")
    return path


def _normalize_symbol(value: str) -> str:
    try:
        return normalize_symbol(value)
    except Exception as exc:
        raise FuturesCanaryError(f"invalid futures canary symbol: {value}") from exc


def _side(value: str, field_name: str) -> str:
    side = str(value).strip().upper()
    if side not in {SIDE_BUY, SIDE_SELL}:
        raise FuturesCanaryError(f"{field_name} must be BUY or SELL")
    return side


def _position_side(value: str) -> str:
    side = str(value).strip().upper()
    if side not in {POSITION_BOTH, POSITION_LONG, POSITION_SHORT}:
        raise FuturesCanaryError("position_side must be BOTH, LONG, or SHORT")
    return side


def _order_type(value: str) -> str:
    order_type = str(value).strip().upper()
    if order_type not in {ORDER_MARKET, ORDER_LIMIT}:
        raise FuturesCanaryError("order_type must be MARKET or LIMIT")
    return order_type


def _positive_number(value: float | None, field_name: str) -> float:
    if value is None or not math.isfinite(float(value)) or float(value) <= 0:
        raise FuturesCanaryError(f"{field_name} must be a positive finite number")
    return float(value)


def _mode(settings: Settings) -> str:
    return "testnet-plan-only" if settings.use_testnet else "mainnet-plan-only"


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


if __name__ == "__main__":
    raise SystemExit(run())

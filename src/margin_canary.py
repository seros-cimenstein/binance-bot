"""Foreground margin canary planning with live margin actions disabled.

Phase 24 exposes readiness planning only. This module can read margin account
state through ``src.margin`` and write local audit records, but it has no
borrow, repay, transfer, or margin-order submission path.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

from src.config import Settings
from src.exchange.client import BinanceClient
from src.margin import (
    ACCOUNT_CROSS,
    ACCOUNT_ISOLATED,
    MarginBorrowRequest,
    MarginDryRunReport,
    MarginRepayRequest,
    MarginRiskLimits,
    build_margin_dry_run_report,
    render_margin_report,
)
from src.scope_audit import (
    ScopeAuditError,
    build_scope_audit_report,
    ensure_scope_allowed,
)
from src.telegram_alerts import TelegramAlerter


class MarginCanaryError(RuntimeError):
    """Raised when margin canary planning cannot proceed safely."""


@dataclass(frozen=True, slots=True)
class MarginCanaryLimits:
    """Operational controls for one foreground plan-only run."""

    audit_path: str = "data/margin-canary.jsonl"
    emergency_stop_path: str = "data/margin-canary.stop"


@dataclass(frozen=True, slots=True)
class MarginCanaryResult:
    """Auditable outcome of one read-only margin canary plan."""

    timestamp: str
    mode: str
    status: str
    report: dict[str, Any]
    submission_attempted: bool = False
    error: str = ""
    audit_path: str = ""
    telegram_sent: bool = False
    write_endpoints_used: tuple[str, ...] = field(default_factory=tuple)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def run_margin_canary(
    settings: Settings,
    *,
    submit_action: bool = False,
    confirm_live_margin: bool = False,
    include_isolated: bool = False,
    isolated_symbols: Sequence[str] = (),
    margin_symbols: Sequence[str] = (),
    borrow_request: MarginBorrowRequest | None = None,
    repay_request: MarginRepayRequest | None = None,
    risk_limits: MarginRiskLimits | None = None,
    limits: MarginCanaryLimits | None = None,
    client: Any | None = None,
    alerter: Any | None = None,
) -> MarginCanaryResult:
    """Build and audit one read-only margin plan; never submit an action."""
    _preflight(
        submit_action=submit_action,
        confirm_live_margin=confirm_live_margin,
    )
    limits = limits or MarginCanaryLimits()
    _validate_limits(limits)
    _ensure_no_emergency_stop(limits.emergency_stop_path)
    try:
        ensure_scope_allowed(
            build_scope_audit_report(
                settings,
                margin_symbols=_requested_margin_symbols(
                    margin_symbols,
                    isolated_symbols,
                    borrow_request,
                    repay_request,
                ),
            )
        )
    except ScopeAuditError as exc:
        raise MarginCanaryError(str(exc)) from exc

    owns_client = client is None
    client = client or BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    alerter = alerter or TelegramAlerter(
        settings.telegram_bot_token,
        settings.telegram_chat_id,
    )
    try:
        report = await build_margin_dry_run_report(
            settings,
            client=client,
            include_isolated=include_isolated,
            isolated_symbols=isolated_symbols,
            borrow_request=borrow_request,
            repay_request=repay_request,
            limits=risk_limits,
        )
        _ensure_report_is_read_only(report)
        rejections = _risk_rejections(
            report,
            borrow_request=borrow_request,
            repay_request=repay_request,
        )
        result = MarginCanaryResult(
            timestamp=_utc_timestamp(),
            mode=report.mode,
            status="risk_rejected" if rejections else "planned",
            report=report.as_dict(),
            error="; ".join(rejections),
        )
        audit_file = _write_audit(Path(limits.audit_path), result)
        telegram_sent = False
        if rejections:
            telegram_sent = await _send_margin_alert(alerter, result, audit_file)
        return MarginCanaryResult(
            **{
                **result.as_dict(),
                "audit_path": str(audit_file),
                "telegram_sent": telegram_sent,
            }
        )
    finally:
        if owns_client:
            await client.close()


def run(argv: list[str] | None = None) -> int:
    """Run one foreground plan-only margin canary from command-line inputs."""
    parser = argparse.ArgumentParser(
        description=(
            "Plan one foreground margin canary using read-only margin data; "
            "live margin actions are disabled."
        )
    )
    parser.add_argument(
        "--plan-only",
        action="store_true",
        help="required; build and audit a read-only plan without mutation",
    )
    parser.add_argument(
        "--submit-action",
        action="store_true",
        help="always fails; margin action submission is not implemented",
    )
    parser.add_argument(
        "--confirm-live-margin",
        action="store_true",
        help="always fails; live margin execution is not approved",
    )
    parser.add_argument(
        "--account-type",
        choices=(ACCOUNT_CROSS, ACCOUNT_ISOLATED),
        default=ACCOUNT_CROSS,
    )
    parser.add_argument("--isolated-symbol", help="isolated margin symbol")
    parser.add_argument(
        "--include-isolated",
        action="store_true",
        help="also read isolated margin account state",
    )
    parser.add_argument(
        "--margin-symbol",
        action="append",
        help="symbol intended for margin planning and local scope auditing",
    )
    parser.add_argument("--asset", default="USDT")
    parser.add_argument("--borrow-amount", type=float, help="borrow amount to model")
    parser.add_argument("--repay-amount", type=float, help="repay amount to model")
    parser.add_argument(
        "--asset-usdt-price",
        type=float,
        help="USDT price for non-stable asset liability calculations",
    )
    parser.add_argument(
        "--min-margin-level",
        type=float,
        default=MarginRiskLimits.min_margin_level,
    )
    parser.add_argument(
        "--liquidation-margin-level",
        type=float,
        default=MarginRiskLimits.liquidation_margin_level,
    )
    parser.add_argument(
        "--min-liquidation-buffer-percent",
        type=float,
        default=MarginRiskLimits.min_liquidation_buffer_percent,
    )
    parser.add_argument(
        "--max-borrow-utilization-percent",
        type=float,
        default=MarginRiskLimits.max_borrow_utilization_percent,
    )
    parser.add_argument(
        "--max-hourly-interest-usdt",
        type=float,
        default=MarginRiskLimits.max_hourly_interest_usdt,
    )
    parser.add_argument(
        "--max-total-liability-usdt",
        type=float,
        default=MarginRiskLimits.max_total_liability_usdt,
    )
    parser.add_argument(
        "--audit-path",
        default=os.getenv("MARGIN_CANARY_AUDIT_PATH", "data/margin-canary.jsonl"),
    )
    parser.add_argument(
        "--emergency-stop-path",
        default=os.getenv("MARGIN_CANARY_STOP_PATH", "data/margin-canary.stop"),
    )
    parser.add_argument("--json", action="store_true", help="print JSON output")
    args = parser.parse_args(argv)

    if not args.plan_only:
        print("Margin canary failed: --plan-only is required; live margin is disabled")
        return 1
    try:
        _preflight(
            submit_action=args.submit_action,
            confirm_live_margin=args.confirm_live_margin,
        )
        borrow_request = (
            MarginBorrowRequest(
                account_type=args.account_type,
                isolated_symbol=args.isolated_symbol,
                asset=args.asset,
                requested_amount=args.borrow_amount,
                asset_usdt_price=args.asset_usdt_price,
            )
            if args.borrow_amount is not None
            else None
        )
        repay_request = (
            MarginRepayRequest(
                account_type=args.account_type,
                isolated_symbol=args.isolated_symbol,
                asset=args.asset,
                requested_amount=args.repay_amount,
            )
            if args.repay_amount is not None
            else None
        )
        report = asyncio.run(
            run_margin_canary(
                Settings.from_env(),
                submit_action=args.submit_action,
                confirm_live_margin=args.confirm_live_margin,
                include_isolated=(
                    args.include_isolated or args.account_type == ACCOUNT_ISOLATED
                ),
                isolated_symbols=(
                    (args.isolated_symbol,) if args.isolated_symbol else ()
                ),
                margin_symbols=tuple(args.margin_symbol or ()),
                borrow_request=borrow_request,
                repay_request=repay_request,
                risk_limits=MarginRiskLimits(
                    min_margin_level=args.min_margin_level,
                    liquidation_margin_level=args.liquidation_margin_level,
                    min_liquidation_buffer_percent=args.min_liquidation_buffer_percent,
                    max_borrow_utilization_percent=args.max_borrow_utilization_percent,
                    max_hourly_interest_usdt=args.max_hourly_interest_usdt,
                    max_total_liability_usdt=args.max_total_liability_usdt,
                ),
                limits=MarginCanaryLimits(
                    audit_path=args.audit_path,
                    emergency_stop_path=args.emergency_stop_path,
                ),
            )
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_margin_canary_result(report))
    except Exception as exc:
        print(f"Margin canary failed: {exc}")
        return 1
    return 0 if report.status == "planned" else 1


def render_margin_canary_result(result: MarginCanaryResult) -> str:
    """Render a concise human-readable plan-only canary result."""
    report = MarginDryRunReport(**result.report)
    lines = [
        "Margin foreground canary: plan only",
        f"Status: {result.status}",
        "Submission attempted: no",
        "Margin write endpoints used: none",
        render_margin_report(report),
    ]
    if result.error:
        lines.append(f"Risk rejection: {result.error}")
    if result.audit_path:
        lines.append(f"Audit path: {result.audit_path}")
    return "\n".join(lines)


def _preflight(*, submit_action: bool, confirm_live_margin: bool) -> None:
    if submit_action:
        raise MarginCanaryError(
            "margin canary is plan-only; submit_action is disabled pending "
            "explicit live execution approval"
        )
    if confirm_live_margin:
        raise MarginCanaryError(
            "margin canary is plan-only; live margin confirmation is disabled "
            "pending explicit live execution approval"
        )


def _validate_limits(limits: MarginCanaryLimits) -> None:
    if not limits.audit_path.strip():
        raise MarginCanaryError("audit_path must not be blank")
    if not limits.emergency_stop_path.strip():
        raise MarginCanaryError("emergency_stop_path must not be blank")


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise MarginCanaryError(f"margin canary emergency stop is active: {path}")


def _requested_margin_symbols(
    margin_symbols: Sequence[str],
    isolated_symbols: Sequence[str],
    borrow_request: MarginBorrowRequest | None,
    repay_request: MarginRepayRequest | None,
) -> tuple[str, ...]:
    requested = list(margin_symbols)
    requested.extend(isolated_symbols)
    for request in (borrow_request, repay_request):
        if request is not None and request.isolated_symbol:
            requested.append(request.isolated_symbol)
    return tuple(dict.fromkeys(requested))


def _ensure_report_is_read_only(report: MarginDryRunReport) -> None:
    if report.live_trading_enabled or report.write_endpoints_used:
        raise MarginCanaryError("margin canary received a report containing live writes")


def _risk_rejections(
    report: MarginDryRunReport,
    *,
    borrow_request: MarginBorrowRequest | None,
    repay_request: MarginRepayRequest | None,
) -> tuple[str, ...]:
    rejections: list[str] = []
    if borrow_request is not None and repay_request is None:
        rejections.append("borrow: a documented repayment intent is required")
    for action, plan in (
        ("borrow", report.borrow_plan),
        ("repay", report.repay_plan),
    ):
        if not isinstance(plan, Mapping) or plan.get("allowed", True):
            continue
        reasons = plan.get("rejection_reasons")
        if isinstance(reasons, Sequence) and not isinstance(reasons, (str, bytes)):
            rejections.extend(f"{action}: {reason}" for reason in reasons)
        else:
            rejections.append(f"{action}: risk plan was rejected")
    return tuple(rejections)


async def _send_margin_alert(
    alerter: Any,
    result: MarginCanaryResult,
    audit_path: Path,
) -> bool:
    send = getattr(alerter, "send", None)
    if send is None or not callable(send):
        return False
    return bool(
        await send(
            "<b>MARGIN CANARY PLAN</b>\n"
            f"Status: <code>{result.status}</code>\n"
            "Action submission: <code>disabled</code>\n"
            f"Risk: <code>{result.error}</code>\n"
            f"Audit: <code>{audit_path}</code>"
        )
    )


def _write_audit(path: Path, result: MarginCanaryResult) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(result.as_dict(), sort_keys=True) + "\n")
    return path


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


if __name__ == "__main__":
    raise SystemExit(run())

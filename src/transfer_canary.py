"""Plan-only foreground universal-transfer canary.

Phase 26 validates read-only transfer discovery and transfer-plan risk gates.
It intentionally exposes no universal-transfer submission operation.
"""

from __future__ import annotations

import argparse
import asyncio
import json
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from src.config import Settings
from src.exchange.client import BinanceClient
from src.scope_audit import (
    ScopeAuditError,
    build_scope_audit_report,
    ensure_scope_allowed,
)
from src.transfers import (
    WALLET_FUNDING,
    WALLET_SPOT,
    TransferPlanError,
    TransferRequest,
    TransferRiskLimits,
    assert_transfer_live_disabled,
    build_transfer_dry_run_report,
)


class TransferCanaryError(RuntimeError):
    """Raised when a foreground transfer plan cannot proceed safely."""

    def __init__(
        self,
        message: str,
        *,
        result: TransferCanaryResult | None = None,
    ) -> None:
        super().__init__(message)
        self.result = result


@dataclass(frozen=True, slots=True)
class TransferCanaryLimits:
    """Local controls for one foreground transfer planning attempt."""

    risk_limits: TransferRiskLimits = field(default_factory=TransferRiskLimits)
    audit_path: str = "data/transfer-canary.jsonl"
    emergency_stop_path: str = "data/transfer-canary.stop"


@dataclass(frozen=True, slots=True)
class TransferCanaryResult:
    """Auditable outcome of one plan-only transfer canary attempt."""

    timestamp: str
    mode: str
    status: str
    plan: dict[str, Any]
    discovery: dict[str, Any]
    submission_attempted: bool = False
    submitted: bool = False
    error: str = ""
    audit_path: str = ""
    live_transfers_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def run_transfer_canary(
    settings: Settings,
    *,
    request: TransferRequest,
    submit_transfer: bool = False,
    confirm_live_transfer: bool = False,
    limits: TransferCanaryLimits | None = None,
    client: Any | None = None,
) -> TransferCanaryResult:
    """Build and audit one read-only transfer plan; never submit a transfer."""
    limits = limits or TransferCanaryLimits()
    _validate_limits(limits)
    _preflight(
        submit_transfer=submit_transfer,
        confirm_live_transfer=confirm_live_transfer,
        emergency_stop_path=limits.emergency_stop_path,
    )
    _ensure_request_scope(settings, request)

    owns_client = client is None
    client = client or BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    try:
        try:
            report = await build_transfer_dry_run_report(
                settings,
                client=client,
                request=request,
                limits=limits.risk_limits,
            )
        except TransferPlanError as exc:
            raise TransferCanaryError(str(exc)) from exc
    finally:
        if owns_client:
            await client.close()

    if report.live_transfers_enabled or report.write_endpoints_used:
        raise TransferCanaryError(
            "transfer canary rejected unexpected live-transfer activity"
        )
    if report.plan is None:
        raise TransferCanaryError("transfer canary did not produce a plan")

    plan_allowed = bool(report.plan.get("allowed"))
    error = ""
    if not plan_allowed:
        reasons = tuple(report.plan.get("rejection_reasons") or ())
        error = "transfer plan rejected: " + ", ".join(str(item) for item in reasons)
    result = TransferCanaryResult(
        timestamp=_utc_timestamp(),
        mode="PLAN_ONLY",
        status="planned" if plan_allowed else "blocked",
        plan=report.plan,
        discovery=report.discovery,
        error=error,
    )
    audit_path = write_transfer_canary_audit(Path(limits.audit_path), result)
    result = TransferCanaryResult(
        **{**result.as_dict(), "audit_path": str(audit_path)}
    )
    if not plan_allowed:
        raise TransferCanaryError(error, result=result)
    return result


def write_transfer_canary_audit(path: Path, result: TransferCanaryResult) -> Path:
    """Append one plan-only canary result to its JSONL audit trail."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(result.as_dict(), sort_keys=True) + "\n")
    return path


def render_transfer_canary_result(result: TransferCanaryResult) -> str:
    """Render the operator-facing plan-only transfer outcome."""
    return "\n".join(
        (
            "Universal-transfer canary: plan only",
            "Live transfer submission: disabled",
            "Write endpoints used: none",
            f"Status: {result.status}",
            (
                "Route: "
                f"{result.plan['source_wallet']} -> {result.plan['destination_wallet']}"
            ),
            f"Type: {result.plan['transfer_type']}",
            f"Asset: {result.plan['asset']}",
            f"Amount: {result.plan['amount']}",
            f"Allowed: {'yes' if result.plan['allowed'] else 'no'}",
            f"Audit path: {result.audit_path}",
        )
    )


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Build a foreground Binance universal-transfer canary plan. "
            "Live transfer submission is not implemented."
        )
    )
    parser.add_argument(
        "--plan-only",
        action="store_true",
        help="required; run read-only discovery and local transfer planning",
    )
    parser.add_argument(
        "--submit-transfer",
        action="store_true",
        help="always fails; universal-transfer submission is disabled",
    )
    parser.add_argument(
        "--confirm-live-transfer",
        action="store_true",
        help="always fails; live universal-transfer activation is disabled",
    )
    parser.add_argument("--source-wallet", default=WALLET_SPOT)
    parser.add_argument("--destination-wallet", default=WALLET_FUNDING)
    parser.add_argument("--asset", default="USDT")
    parser.add_argument("--amount", type=float)
    parser.add_argument("--asset-usdt-price", type=float)
    parser.add_argument("--from-symbol")
    parser.add_argument("--to-symbol")
    parser.add_argument("--reason", default="foreground plan-only transfer canary")
    parser.add_argument("--max-transfer-amount", type=float, default=10.0)
    parser.add_argument("--max-transfer-value-usdt", type=float, default=25.0)
    parser.add_argument("--min-source-free-after", type=float, default=0.0)
    parser.add_argument("--audit-path", default="data/transfer-canary.jsonl")
    parser.add_argument(
        "--emergency-stop-path",
        default="data/transfer-canary.stop",
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    try:
        if not args.plan_only:
            raise TransferCanaryError("--plan-only is required; submission is disabled")
        _preflight(
            submit_transfer=args.submit_transfer,
            confirm_live_transfer=args.confirm_live_transfer,
            emergency_stop_path=args.emergency_stop_path,
        )
        if args.amount is None:
            raise TransferCanaryError("--amount is required for transfer planning")
        settings = Settings.from_env()
        result = asyncio.run(
            run_transfer_canary(
                settings,
                request=TransferRequest(
                    source_wallet=args.source_wallet,
                    destination_wallet=args.destination_wallet,
                    asset=args.asset,
                    amount=args.amount,
                    reason=args.reason,
                    asset_usdt_price=args.asset_usdt_price,
                    from_symbol=args.from_symbol,
                    to_symbol=args.to_symbol,
                ),
                submit_transfer=args.submit_transfer,
                confirm_live_transfer=args.confirm_live_transfer,
                limits=TransferCanaryLimits(
                    risk_limits=TransferRiskLimits(
                        max_transfer_amount=args.max_transfer_amount,
                        max_transfer_value_usdt=args.max_transfer_value_usdt,
                        min_source_free_after=args.min_source_free_after,
                    ),
                    audit_path=args.audit_path,
                    emergency_stop_path=args.emergency_stop_path,
                ),
            )
        )
        if args.json:
            print(json.dumps(result.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_transfer_canary_result(result))
    except Exception as exc:
        print(f"transfer canary failed: {exc}")
        return 1
    return 0


def _preflight(
    *,
    submit_transfer: bool,
    confirm_live_transfer: bool,
    emergency_stop_path: str,
) -> None:
    if submit_transfer:
        raise TransferCanaryError(
            "universal-transfer submission is not implemented; "
            "Phase 26 is plan-only"
        )
    try:
        assert_transfer_live_disabled(confirm_live_transfer=confirm_live_transfer)
    except TransferPlanError as exc:
        raise TransferCanaryError(
            "live universal-transfer confirmation is disabled; "
            "Phase 26 is plan-only"
        ) from exc
    if Path(emergency_stop_path).exists():
        raise TransferCanaryError("transfer canary emergency stop is active")


def _validate_limits(limits: TransferCanaryLimits) -> None:
    if not limits.audit_path.strip():
        raise TransferCanaryError("audit_path must not be blank")
    if not limits.emergency_stop_path.strip():
        raise TransferCanaryError("emergency_stop_path must not be blank")


def _ensure_request_scope(settings: Settings, request: TransferRequest) -> None:
    isolated_symbols = tuple(
        symbol for symbol in (request.from_symbol, request.to_symbol) if symbol
    )
    try:
        ensure_scope_allowed(
            build_scope_audit_report(
                settings,
                transfer_assets=(request.asset,),
                margin_symbols=isolated_symbols,
            )
        )
    except ScopeAuditError as exc:
        raise TransferCanaryError(str(exc)) from exc


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


if __name__ == "__main__":
    raise SystemExit(run())

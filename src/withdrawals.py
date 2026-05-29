"""Dry-run withdrawal safety planning.

Phase 27 is planning only. This module models withdrawal safety controls
locally and intentionally has no live withdrawal submission path.
"""

from __future__ import annotations

import argparse
import json
import math
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Sequence

from src.config import Settings

WRITE_ENDPOINTS_BLOCKED = ("sapiPostCapitalWithdrawApply",)


class WithdrawalSafetyError(RuntimeError):
    """Raised when withdrawal planning cannot proceed safely."""


@dataclass(frozen=True, slots=True)
class WithdrawalAddress:
    """One locally approved withdrawal destination."""

    asset: str
    address: str
    networks: tuple[str, ...]
    label: str = ""
    memo: str | None = None
    active: bool = True

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class WithdrawalRequest:
    """Requested external withdrawal modeling inputs."""

    asset: str
    amount: float
    address: str
    network: str | None = None
    memo: str | None = None
    operator_id: str = ""
    reason: str = "manual dry-run withdrawal plan"


@dataclass(frozen=True, slots=True)
class WithdrawalSafetyPolicy:
    """Local controls that must pass before a withdrawal could be considered."""

    allowlist: tuple[WithdrawalAddress, ...] = ()
    max_per_request_amount: float = 10.0
    max_daily_amount: float = 25.0
    daily_used_amount: float = 0.0
    approvals_required: int = 2
    approvals: tuple[str, ...] = ()
    cooldown_seconds: int = 3600
    last_withdrawal_at: str | None = None
    emergency_stop: bool = False
    allowed_operators: tuple[str, ...] = ()
    require_operator_identity: bool = True

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class WithdrawalPlan:
    """Dry-run withdrawal safety decision and audit payload."""

    timestamp: str
    asset: str
    amount: float
    address: str
    network: str | None
    memo: str | None
    allowlist_label: str
    operator_id: str
    reason: str
    per_request_cap: float
    daily_cap: float
    daily_used: float
    daily_remaining: float
    approvals_required: int
    approvals: tuple[str, ...]
    cooldown_seconds_remaining: int
    emergency_stop: bool
    submission_attempted: bool
    submitted: bool
    allowed: bool
    rejection_reasons: tuple[str, ...] = ()
    warnings: tuple[str, ...] = ()
    live_withdrawals_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()
    policy: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class WithdrawalDryRunReport:
    """Full dry-run withdrawal report."""

    timestamp: str
    mode: str
    plan: dict[str, Any]
    audit_path: str = ""
    live_withdrawals_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


def build_withdrawal_plan(
    request: WithdrawalRequest,
    *,
    policy: WithdrawalSafetyPolicy | None = None,
) -> WithdrawalPlan:
    """Build a dry-run withdrawal safety plan without submitting anything."""
    policy = policy or WithdrawalSafetyPolicy()
    _validate_policy(policy)
    asset = _asset(request.asset)
    amount = _positive_number(request.amount, "amount")
    address = _non_blank(request.address, "address")
    operator_id = request.operator_id.strip()
    requested_network = _optional_network(request.network)
    memo = _optional_text(request.memo)
    approvals = tuple(
        sorted({_non_blank(value, "approval") for value in policy.approvals})
    )
    allowed_address = _matching_allowlist_entry(
        policy.allowlist,
        asset=asset,
        address=address,
        memo=memo,
    )
    network = requested_network or (
        allowed_address.networks[0]
        if allowed_address and allowed_address.networks
        else None
    )
    daily_remaining = max(0.0, policy.max_daily_amount - policy.daily_used_amount)
    cooldown_remaining = _cooldown_remaining(
        policy.last_withdrawal_at,
        policy.cooldown_seconds,
    )

    rejections: list[str] = []
    warnings: list[str] = []
    if policy.emergency_stop:
        rejections.append("emergency stop is active")
    if allowed_address is None:
        rejections.append("address is not allowlisted for this asset and memo")
    elif not allowed_address.active:
        rejections.append("allowlisted address is inactive")
    if requested_network is None:
        if network is None:
            rejections.append("network is required when allowlist has no default network")
        else:
            warnings.append("network was selected from the allowlist default")
    elif (
        allowed_address is not None
        and requested_network not in allowed_address.networks
    ):
        rejections.append("network is not allowlisted for this address")
    if amount > policy.max_per_request_amount:
        rejections.append("amount exceeds max_per_request_amount")
    if amount > daily_remaining:
        rejections.append("amount exceeds remaining daily withdrawal cap")
    if len(approvals) < policy.approvals_required:
        rejections.append("approval state does not satisfy approvals_required")
    if cooldown_remaining > 0:
        rejections.append("withdrawal cooldown is still active")
    if policy.require_operator_identity and not operator_id:
        rejections.append("operator identity is required")
    if (
        operator_id
        and policy.allowed_operators
        and operator_id not in policy.allowed_operators
    ):
        rejections.append("operator is not allowed by withdrawal policy")

    return WithdrawalPlan(
        timestamp=_utc_timestamp(),
        asset=asset,
        amount=amount,
        address=address,
        network=network,
        memo=memo,
        allowlist_label=allowed_address.label if allowed_address else "",
        operator_id=operator_id,
        reason=request.reason,
        per_request_cap=policy.max_per_request_amount,
        daily_cap=policy.max_daily_amount,
        daily_used=policy.daily_used_amount,
        daily_remaining=daily_remaining,
        approvals_required=policy.approvals_required,
        approvals=approvals,
        cooldown_seconds_remaining=cooldown_remaining,
        emergency_stop=policy.emergency_stop,
        submission_attempted=False,
        submitted=False,
        allowed=not rejections,
        rejection_reasons=tuple(rejections),
        warnings=tuple(warnings),
        policy=policy.as_dict(),
    )


def build_withdrawal_dry_run_report(
    settings: Settings,
    *,
    request: WithdrawalRequest,
    policy: WithdrawalSafetyPolicy | None = None,
    audit_path: str | None = None,
) -> WithdrawalDryRunReport:
    """Build and optionally audit a dry-run withdrawal report."""
    plan = build_withdrawal_plan(request, policy=policy)
    report = WithdrawalDryRunReport(
        timestamp=_utc_timestamp(),
        mode=_mode(settings),
        plan=plan.as_dict(),
    )
    if audit_path:
        written = write_withdrawal_audit(Path(audit_path), plan)
        return WithdrawalDryRunReport(
            **{**report.as_dict(), "audit_path": str(written)}
        )
    return report


def write_withdrawal_audit(path: Path, plan: WithdrawalPlan) -> Path:
    """Append one proposed withdrawal plan to a JSONL audit file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(plan.as_dict(), sort_keys=True) + "\n")
    return path


def assert_withdrawal_live_disabled(*, confirm_live_withdrawal: bool = False) -> None:
    """Fail closed for any attempted live withdrawal activation."""
    if confirm_live_withdrawal:
        raise WithdrawalSafetyError(
            "live withdrawal activation is not implemented; Phase 27 is dry-run only"
        )


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Plan Binance withdrawals without submitting them."
    )
    parser.add_argument("--dry-run", action="store_true", help="required")
    parser.add_argument(
        "--confirm-live-withdrawal",
        action="store_true",
        help="always fails; live withdrawal activation is not implemented",
    )
    parser.add_argument("--asset", default="USDT")
    parser.add_argument("--amount", type=float)
    parser.add_argument("--address")
    parser.add_argument("--network")
    parser.add_argument("--memo")
    parser.add_argument("--operator-id", default="")
    parser.add_argument("--reason", default="manual dry-run withdrawal plan")
    parser.add_argument("--allow-address", action="append", default=())
    parser.add_argument("--allow-network", action="append", default=())
    parser.add_argument("--allow-memo")
    parser.add_argument("--allow-label", default="")
    parser.add_argument("--allowed-operator", action="append", default=())
    parser.add_argument("--approval", action="append", default=())
    parser.add_argument("--approvals-required", type=int, default=2)
    parser.add_argument("--max-per-request-amount", type=float, default=10.0)
    parser.add_argument("--max-daily-amount", type=float, default=25.0)
    parser.add_argument("--daily-used-amount", type=float, default=0.0)
    parser.add_argument("--cooldown-seconds", type=int, default=3600)
    parser.add_argument("--last-withdrawal-at")
    parser.add_argument("--emergency-stop", action="store_true")
    parser.add_argument("--audit-path", default="data/withdrawal-plans.jsonl")
    parser.add_argument("--no-audit", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    if not args.dry_run:
        print(
            "withdrawal check failed: --dry-run is required; "
            "live withdrawal is disabled"
        )
        return 1
    try:
        assert_withdrawal_live_disabled(
            confirm_live_withdrawal=args.confirm_live_withdrawal
        )
        if args.amount is None:
            raise WithdrawalSafetyError("--amount is required for withdrawal planning")
        if not args.address:
            raise WithdrawalSafetyError("--address is required for withdrawal planning")
        settings = Settings.from_env()
        allowlist = tuple(
            WithdrawalAddress(
                asset=args.asset,
                address=address,
                networks=tuple(_network(network) for network in args.allow_network),
                label=args.allow_label,
                memo=_optional_text(args.allow_memo),
            )
            for address in args.allow_address
        )
        request = WithdrawalRequest(
            asset=args.asset,
            amount=args.amount,
            address=args.address,
            network=args.network,
            memo=args.memo,
            operator_id=args.operator_id,
            reason=args.reason,
        )
        policy = WithdrawalSafetyPolicy(
            allowlist=allowlist,
            max_per_request_amount=args.max_per_request_amount,
            max_daily_amount=args.max_daily_amount,
            daily_used_amount=args.daily_used_amount,
            approvals_required=args.approvals_required,
            approvals=tuple(args.approval),
            cooldown_seconds=args.cooldown_seconds,
            last_withdrawal_at=args.last_withdrawal_at,
            emergency_stop=args.emergency_stop,
            allowed_operators=tuple(args.allowed_operator),
        )
        report = build_withdrawal_dry_run_report(
            settings,
            request=request,
            policy=policy,
            audit_path=None if args.no_audit else args.audit_path,
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_withdrawal_report(report))
    except Exception as exc:
        print(f"withdrawal check failed: {exc}")
        return 1
    return 0


def render_withdrawal_report(report: WithdrawalDryRunReport) -> str:
    """Render a concise human-readable dry-run withdrawal report."""
    plan = report.plan
    lines = [
        f"Mode: {report.mode}",
        "Live withdrawals: disabled",
        "Write endpoints used: none",
        "Withdrawal plan:",
        f"  - asset: {plan['asset']}",
        f"  - amount: {plan['amount']}",
        f"  - network: {plan['network']}",
        f"  - allowlist label: {plan['allowlist_label'] or 'none'}",
        f"  - operator: {plan['operator_id'] or 'missing'}",
        f"  - daily remaining: {plan['daily_remaining']}",
        f"  - cooldown remaining seconds: {plan['cooldown_seconds_remaining']}",
        f"  - allowed: {_yes_no(bool(plan['allowed']))}",
    ]
    if plan["rejection_reasons"]:
        lines.append("  - rejection reasons: " + ", ".join(plan["rejection_reasons"]))
    if plan["warnings"]:
        lines.append("  - warnings: " + ", ".join(plan["warnings"]))
    if report.audit_path:
        lines.append(f"Audit path: {report.audit_path}")
    return "\n".join(lines)


def _matching_allowlist_entry(
    allowlist: Sequence[WithdrawalAddress],
    *,
    asset: str,
    address: str,
    memo: str | None,
) -> WithdrawalAddress | None:
    for entry in allowlist:
        if _asset(entry.asset) != asset:
            continue
        if entry.address.strip() != address:
            continue
        if _optional_text(entry.memo) != memo:
            continue
        return entry
    return None


def _validate_policy(policy: WithdrawalSafetyPolicy) -> None:
    _positive_number(policy.max_per_request_amount, "max_per_request_amount")
    _positive_number(policy.max_daily_amount, "max_daily_amount")
    _non_negative_number(policy.daily_used_amount, "daily_used_amount")
    if policy.approvals_required < 0:
        raise WithdrawalSafetyError("approvals_required must be at least zero")
    if policy.cooldown_seconds < 0:
        raise WithdrawalSafetyError("cooldown_seconds must be at least zero")
    for entry in policy.allowlist:
        _asset(entry.asset)
        _non_blank(entry.address, "allowlist address")
        for network in entry.networks:
            _network(network)
    for operator in policy.allowed_operators:
        _non_blank(operator, "allowed operator")
    if policy.last_withdrawal_at is not None:
        _parse_timestamp(policy.last_withdrawal_at)


def _cooldown_remaining(last_withdrawal_at: str | None, cooldown_seconds: int) -> int:
    if last_withdrawal_at is None or cooldown_seconds <= 0:
        return 0
    elapsed = (
        datetime.now(timezone.utc) - _parse_timestamp(last_withdrawal_at)
    ).total_seconds()
    return max(0, int(math.ceil(cooldown_seconds - elapsed)))


def _parse_timestamp(value: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise WithdrawalSafetyError("last_withdrawal_at must be an ISO timestamp") from exc
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _asset(value: object) -> str:
    text = str(value or "").strip().upper()
    if not text:
        raise WithdrawalSafetyError("asset must not be blank")
    return text


def _network(value: object) -> str:
    text = str(value or "").strip().upper()
    if not text:
        raise WithdrawalSafetyError("network must not be blank")
    return text


def _optional_network(value: str | None) -> str | None:
    if value is None or not value.strip():
        return None
    return _network(value)


def _optional_text(value: str | None) -> str | None:
    if value is None:
        return None
    text = value.strip()
    return text or None


def _non_blank(value: object, field_name: str) -> str:
    text = str(value or "").strip()
    if not text:
        raise WithdrawalSafetyError(f"{field_name} must not be blank")
    return text


def _positive_number(value: float, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number <= 0:
        raise WithdrawalSafetyError(f"{field_name} must be greater than zero")
    return number


def _non_negative_number(value: float, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number < 0:
        raise WithdrawalSafetyError(f"{field_name} must be at least zero")
    return number


def _coerce_float(value: object, field_name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise WithdrawalSafetyError(f"{field_name} must be a number") from exc
    if not math.isfinite(number):
        raise WithdrawalSafetyError(f"{field_name} must be finite")
    return number


def _mode(settings: Settings) -> str:
    if settings.dry_run:
        return "DRY_RUN"
    if settings.use_testnet:
        return "testnet"
    return "LIVE"


def _yes_no(value: bool) -> str:
    return "yes" if value else "no"


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


if __name__ == "__main__":
    raise SystemExit(run())

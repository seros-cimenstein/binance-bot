from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from src import withdrawals
from src.config import Settings
from src.withdrawals import (
    WithdrawalAddress,
    WithdrawalRequest,
    WithdrawalSafetyError,
    WithdrawalSafetyPolicy,
    assert_withdrawal_live_disabled,
    build_withdrawal_dry_run_report,
    build_withdrawal_plan,
    write_withdrawal_audit,
)


def _settings() -> Settings:
    return Settings(
        api_key="key",
        api_secret="secret",
        use_testnet=False,
        dry_run=False,
        confirm_live_trading="yes",
    )


def _allowlisted_address(*, active: bool = True) -> WithdrawalAddress:
    return WithdrawalAddress(
        asset="USDT",
        address="TAllowlistedAddress",
        networks=("TRX", "ETH"),
        label="treasury",
        active=active,
    )


def _policy(**overrides: object) -> WithdrawalSafetyPolicy:
    values = {
        "allowlist": (_allowlisted_address(),),
        "max_per_request_amount": 10.0,
        "max_daily_amount": 25.0,
        "daily_used_amount": 5.0,
        "approvals_required": 2,
        "approvals": ("risk", "ops"),
        "cooldown_seconds": 0,
        "last_withdrawal_at": None,
        "emergency_stop": False,
        "allowed_operators": ("alice",),
    }
    values.update(overrides)
    return WithdrawalSafetyPolicy(**values)


def _request(**overrides: object) -> WithdrawalRequest:
    values = {
        "asset": "USDT",
        "amount": 5.0,
        "address": "TAllowlistedAddress",
        "network": "TRX",
        "operator_id": "alice",
        "reason": "rebalance custody",
    }
    values.update(overrides)
    return WithdrawalRequest(**values)


def test_live_withdrawal_activation_is_hard_blocked() -> None:
    with pytest.raises(
        WithdrawalSafetyError,
        match="live withdrawal activation is not implemented",
    ):
        assert_withdrawal_live_disabled(confirm_live_withdrawal=True)

    plan = build_withdrawal_plan(_request(), policy=_policy())

    assert plan.submission_attempted is False
    assert plan.submitted is False
    assert plan.live_withdrawals_enabled is False
    assert plan.write_endpoints_used == ()


def test_withdrawal_plan_validates_allowlist_and_network_selection() -> None:
    unknown_address = build_withdrawal_plan(
        _request(address="TUnknownAddress"),
        policy=_policy(),
    )
    wrong_network = build_withdrawal_plan(
        _request(network="BSC"),
        policy=_policy(),
    )
    default_network = build_withdrawal_plan(
        _request(network=None),
        policy=_policy(),
    )

    assert unknown_address.allowed is False
    assert (
        "address is not allowlisted for this asset and memo"
        in unknown_address.rejection_reasons
    )
    assert wrong_network.allowed is False
    assert "network is not allowlisted for this address" in wrong_network.rejection_reasons
    assert default_network.allowed is True
    assert default_network.network == "TRX"
    assert "network was selected from the allowlist default" in default_network.warnings


def test_withdrawal_plan_rejects_per_request_and_daily_caps() -> None:
    plan = build_withdrawal_plan(
        _request(amount=21),
        policy=_policy(
            max_per_request_amount=10,
            max_daily_amount=25,
            daily_used_amount=5,
        ),
    )

    assert plan.allowed is False
    assert "amount exceeds max_per_request_amount" in plan.rejection_reasons
    assert "amount exceeds remaining daily withdrawal cap" in plan.rejection_reasons


def test_withdrawal_plan_rejects_approval_cooldown_emergency_and_operator() -> None:
    recent = datetime.now(timezone.utc) - timedelta(seconds=30)
    plan = build_withdrawal_plan(
        _request(operator_id="mallory"),
        policy=_policy(
            approvals=("risk",),
            cooldown_seconds=3600,
            last_withdrawal_at=recent.isoformat(),
            emergency_stop=True,
        ),
    )

    assert plan.allowed is False
    assert "approval state does not satisfy approvals_required" in plan.rejection_reasons
    assert "withdrawal cooldown is still active" in plan.rejection_reasons
    assert "emergency stop is active" in plan.rejection_reasons
    assert "operator is not allowed by withdrawal policy" in plan.rejection_reasons
    assert plan.cooldown_seconds_remaining > 0


def test_withdrawal_audit_persistence(tmp_path: Path) -> None:
    audit_path = tmp_path / "withdrawal-plans.jsonl"
    plan = build_withdrawal_plan(_request(), policy=_policy())

    written = write_withdrawal_audit(audit_path, plan)
    report = build_withdrawal_dry_run_report(
        _settings(),
        request=_request(amount=1),
        policy=_policy(),
        audit_path=str(audit_path),
    )

    rows = audit_path.read_text(encoding="utf-8").splitlines()
    assert written == audit_path
    assert report.audit_path == str(audit_path)
    assert len(rows) == 2
    first = json.loads(rows[0])
    second = json.loads(rows[1])
    assert first["operator_id"] == "alice"
    assert first["submitted"] is False
    assert second["reason"] == "rebalance custody"


def test_withdrawal_cli_fails_closed_without_env(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)
    monkeypatch.delenv("BINANCE_SECRET_KEY", raising=False)

    assert withdrawals.run([]) == 1
    assert withdrawals.run(["--dry-run", "--confirm-live-withdrawal"]) == 1

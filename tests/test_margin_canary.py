from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from src.config import Settings
from src.margin import (
    ACCOUNT_CROSS,
    MarginBorrowRequest,
    MarginRepayRequest,
    MarginRiskLimits,
)
from src.margin_canary import (
    MarginCanaryError,
    MarginCanaryLimits,
    run,
    run_margin_canary,
)


def _settings(**overrides: Any) -> Settings:
    values = {
        "api_key": "key",
        "api_secret": "secret",
        "use_testnet": False,
        "dry_run": False,
        "confirm_live_trading": "yes",
        "api_symbol_whitelist": ("BTC/USDT", "BNB/USDT"),
    }
    values.update(overrides)
    return Settings(**values)


def _cross_payload(*, margin_level: str = "3.2") -> dict[str, Any]:
    return {
        "created": True,
        "borrowEnabled": True,
        "tradeEnabled": True,
        "transferInEnabled": True,
        "transferOutEnabled": True,
        "accountType": "MARGIN_1",
        "marginLevel": margin_level,
        "collateralMarginLevel": margin_level,
        "totalAssetOfBtc": "0.01000000",
        "totalLiabilityOfBtc": "0.00100000",
        "totalNetAssetOfBtc": "0.00900000",
        "TotalCollateralValueInUSDT": "100",
        "totalOpenOrderLossInUSDT": "0",
        "userAssets": [
            {
                "asset": "USDT",
                "free": "20",
                "locked": "0",
                "borrowed": "5",
                "interest": "0.01",
                "netAsset": "14.99",
                "borrowEnabled": True,
                "repayEnabled": True,
            }
        ],
    }


class FakeReadOnlyMarginClient:
    def __init__(
        self,
        *,
        cross: dict[str, Any] | None = None,
        interest_rate: str = "0.00001",
    ) -> None:
        self.cross = cross or _cross_payload()
        self.interest_rate = interest_rate
        self.requests: list[str] = []
        self.write_calls: list[str] = []

    async def request(self, method: str, *_args: Any, **_kwargs: Any) -> Any:
        self.requests.append(method)
        if method.startswith("sapiPost"):
            self.write_calls.append(method)
            raise AssertionError(f"write endpoint should not be used: {method}")
        if method == "sapiGetMarginAccount":
            return self.cross
        if method == "sapiGetMarginMaxBorrowable":
            return {"amount": "50", "borrowLimit": "100"}
        if method == "sapiGetMarginNextHourlyInterestRate":
            return [
                {
                    "asset": "USDT",
                    "nextHourlyInterestRate": self.interest_rate,
                }
            ]
        raise AssertionError(f"unexpected request: {method}")


class FakeAlerter:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> bool:
        self.messages.append(message)
        return True


def _borrow_request() -> MarginBorrowRequest:
    return MarginBorrowRequest(
        account_type=ACCOUNT_CROSS,
        asset="USDT",
        requested_amount=1,
    )


def _repay_request() -> MarginRepayRequest:
    return MarginRepayRequest(
        account_type=ACCOUNT_CROSS,
        asset="USDT",
        requested_amount=1,
    )


def _risk_limits() -> MarginRiskLimits:
    return MarginRiskLimits(
        min_margin_level=1.5,
        liquidation_margin_level=1.1,
        min_liquidation_buffer_percent=10,
        max_borrow_utilization_percent=50,
        max_hourly_interest_usdt=1,
        max_total_liability_usdt=1000,
    )


@pytest.mark.asyncio
async def test_allowed_plan_reads_only_and_writes_audit_jsonl(tmp_path: Path) -> None:
    client = FakeReadOnlyMarginClient()
    audit_path = tmp_path / "margin-canary.jsonl"

    result = await run_margin_canary(
        _settings(),
        margin_symbols=("BNB/USDT",),
        borrow_request=_borrow_request(),
        repay_request=_repay_request(),
        risk_limits=_risk_limits(),
        limits=MarginCanaryLimits(audit_path=str(audit_path)),
        client=client,
        alerter=FakeAlerter(),
    )

    assert result.status == "planned"
    assert result.submission_attempted is False
    assert result.report["borrow_plan"]["allowed"] is True
    assert result.write_endpoints_used == ()
    assert client.requests == [
        "sapiGetMarginAccount",
        "sapiGetMarginMaxBorrowable",
        "sapiGetMarginNextHourlyInterestRate",
    ]
    assert client.write_calls == []
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "planned"
    assert payload["submission_attempted"] is False
    assert payload["report"]["write_endpoints_used"] == []


@pytest.mark.asyncio
async def test_scope_block_happens_before_margin_reads_or_audit(tmp_path: Path) -> None:
    client = FakeReadOnlyMarginClient()
    audit_path = tmp_path / "margin-canary.jsonl"

    with pytest.raises(MarginCanaryError, match="API scope audit failed"):
        await run_margin_canary(
            _settings(api_symbol_whitelist=("BTC/USDT",)),
            margin_symbols=("BNB/USDT",),
            borrow_request=_borrow_request(),
            limits=MarginCanaryLimits(audit_path=str(audit_path)),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.requests == []
    assert client.write_calls == []
    assert not audit_path.exists()


@pytest.mark.asyncio
async def test_emergency_stop_blocks_before_margin_reads_or_audit(
    tmp_path: Path,
) -> None:
    stop_path = tmp_path / "margin-canary.stop"
    stop_path.write_text("stop\n", encoding="utf-8")
    client = FakeReadOnlyMarginClient()
    audit_path = tmp_path / "margin-canary.jsonl"

    with pytest.raises(MarginCanaryError, match="emergency stop"):
        await run_margin_canary(
            _settings(),
            margin_symbols=("BNB/USDT",),
            borrow_request=_borrow_request(),
            limits=MarginCanaryLimits(
                audit_path=str(audit_path),
                emergency_stop_path=str(stop_path),
            ),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.requests == []
    assert client.write_calls == []
    assert not audit_path.exists()


@pytest.mark.parametrize(
    ("kwargs", "message"),
    [
        ({"submit_action": True}, "submit_action is disabled"),
        ({"confirm_live_margin": True}, "live margin confirmation is disabled"),
    ],
)
@pytest.mark.asyncio
async def test_mutation_and_live_confirmation_flags_fail_before_reads(
    tmp_path: Path,
    kwargs: dict[str, bool],
    message: str,
) -> None:
    client = FakeReadOnlyMarginClient()
    audit_path = tmp_path / "margin-canary.jsonl"

    with pytest.raises(MarginCanaryError, match=message):
        await run_margin_canary(
            _settings(),
            margin_symbols=("BNB/USDT",),
            borrow_request=_borrow_request(),
            limits=MarginCanaryLimits(audit_path=str(audit_path)),
            client=client,
            alerter=FakeAlerter(),
            **kwargs,
        )

    assert client.requests == []
    assert client.write_calls == []
    assert not audit_path.exists()


@pytest.mark.asyncio
async def test_risk_rejection_is_visible_audited_and_alerted(tmp_path: Path) -> None:
    client = FakeReadOnlyMarginClient(
        cross=_cross_payload(margin_level="1.2"),
        interest_rate="0.01",
    )
    alerter = FakeAlerter()
    audit_path = tmp_path / "margin-canary.jsonl"

    result = await run_margin_canary(
        _settings(),
        margin_symbols=("BNB/USDT",),
        borrow_request=_borrow_request(),
        repay_request=_repay_request(),
        risk_limits=MarginRiskLimits(
            min_margin_level=2.0,
            liquidation_margin_level=1.1,
            min_liquidation_buffer_percent=25,
            max_borrow_utilization_percent=100,
            max_hourly_interest_usdt=0.001,
            max_total_liability_usdt=1000,
        ),
        limits=MarginCanaryLimits(audit_path=str(audit_path)),
        client=client,
        alerter=alerter,
    )

    assert result.status == "risk_rejected"
    assert "current margin level is below the configured minimum" in result.error
    assert "projected hourly interest exceeds the configured cap" in result.error
    assert result.telegram_sent is True
    assert "MARGIN CANARY PLAN" in alerter.messages[0]
    assert client.write_calls == []
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "risk_rejected"
    assert payload["report"]["borrow_plan"]["allowed"] is False


@pytest.mark.asyncio
async def test_borrow_canary_requires_documented_repayment_intent(
    tmp_path: Path,
) -> None:
    result = await run_margin_canary(
        _settings(),
        margin_symbols=("BNB/USDT",),
        borrow_request=_borrow_request(),
        risk_limits=_risk_limits(),
        limits=MarginCanaryLimits(audit_path=str(tmp_path / "margin-canary.jsonl")),
        client=FakeReadOnlyMarginClient(),
        alerter=FakeAlerter(),
    )

    assert result.status == "risk_rejected"
    assert "documented repayment intent" in result.error


def test_cli_has_help_and_requires_plan_only_before_loading_environment(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    with pytest.raises(SystemExit) as help_exit:
        run(["--help"])
    assert help_exit.value.code == 0
    assert "--plan-only" in capsys.readouterr().out

    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)
    monkeypatch.delenv("BINANCE_SECRET_KEY", raising=False)
    assert run([]) == 1
    assert "--plan-only is required" in capsys.readouterr().out

    assert run(["--plan-only", "--submit-action"]) == 1
    assert "submit_action is disabled" in capsys.readouterr().out

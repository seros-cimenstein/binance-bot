from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from src.config import Settings
from src.transfer_canary import (
    TransferCanaryError,
    TransferCanaryLimits,
    run,
    run_transfer_canary,
)
from src.transfers import (
    WALLET_CROSS_MARGIN,
    WALLET_FUNDING,
    WALLET_ISOLATED_MARGIN,
    WALLET_SPOT,
    TransferRequest,
    TransferRiskLimits,
)


def _settings(**overrides: Any) -> Settings:
    values: dict[str, Any] = {
        "api_key": "key",
        "api_secret": "secret",
        "use_testnet": False,
        "dry_run": False,
        "confirm_live_trading": "yes",
        "api_symbol_whitelist": ("BTC/USDT", "BNB/USDT"),
        "api_asset_whitelist": ("USDT", "BNB"),
    }
    values.update(overrides)
    return Settings(**values)


class FakeTransferCanaryClient:
    def __init__(
        self,
        *,
        spot_free: str = "20",
        margin_borrowed: str = "0",
        margin_interest: str = "0",
    ) -> None:
        self.spot_free = spot_free
        self.margin_borrowed = margin_borrowed
        self.margin_interest = margin_interest
        self.requests: list[str] = []
        self.write_calls = 0

    async def request(self, method: str, *args: Any, **kwargs: Any) -> Any:
        del args, kwargs
        self.requests.append(method)
        if method == "sapiPostAssetTransfer":
            self.write_calls += 1
            raise AssertionError("live universal-transfer endpoint must not be called")
        if method == "sapiGetAssetWalletBalance":
            return [
                {"walletName": "Spot", "activate": True, "balance": "20"},
                {"walletName": "Funding", "activate": True, "balance": "1"},
                {"walletName": "Cross Margin", "activate": True, "balance": "8"},
                {"walletName": "Isolated Margin", "activate": True, "balance": "0"},
            ]
        if method == "sapiV3PostAssetGetUserAsset":
            return [{"asset": "USDT", "free": self.spot_free, "locked": "0"}]
        if method == "sapiPostAssetGetFundingAsset":
            return [{"asset": "USDT", "free": "1", "locked": "0"}]
        if method == "sapiGetMarginAccount":
            return {
                "userAssets": [
                    {
                        "asset": "USDT",
                        "free": "8",
                        "locked": "0",
                        "borrowed": self.margin_borrowed,
                        "interest": self.margin_interest,
                    }
                ]
            }
        if method == "sapiGetAssetTransfer":
            return {"rows": []}
        raise AssertionError(f"unexpected request: {method}")


def _limits(tmp_path: Path) -> TransferCanaryLimits:
    return TransferCanaryLimits(
        risk_limits=TransferRiskLimits(
            max_transfer_amount=10,
            max_transfer_value_usdt=25,
            min_source_free_after=0,
        ),
        audit_path=str(tmp_path / "transfer-canary.jsonl"),
        emergency_stop_path=str(tmp_path / "transfer-canary.stop"),
    )


@pytest.mark.asyncio
async def test_plan_only_transfer_canary_audits_allowed_plan_without_writes(
    tmp_path: Path,
) -> None:
    client = FakeTransferCanaryClient()
    limits = _limits(tmp_path)

    result = await run_transfer_canary(
        _settings(),
        request=TransferRequest(
            source_wallet=WALLET_SPOT,
            destination_wallet=WALLET_FUNDING,
            asset="USDT",
            amount=5,
        ),
        limits=limits,
        client=client,
    )

    assert result.status == "planned"
    assert result.plan["allowed"] is True
    assert result.plan["transfer_type"] == "MAIN_FUNDING"
    assert result.submission_attempted is False
    assert result.submitted is False
    assert result.write_endpoints_used == ()
    assert client.write_calls == 0
    assert "sapiPostAssetTransfer" not in client.requests
    row = json.loads(Path(result.audit_path).read_text(encoding="utf-8"))
    assert row["status"] == "planned"
    assert row["plan"]["amount"] == 5
    assert row["submission_attempted"] is False


@pytest.mark.asyncio
async def test_transfer_canary_blocks_asset_scope_before_reads(tmp_path: Path) -> None:
    client = FakeTransferCanaryClient()

    with pytest.raises(TransferCanaryError, match="API scope audit failed"):
        await run_transfer_canary(
            _settings(api_asset_whitelist=()),
            request=TransferRequest(
                source_wallet=WALLET_SPOT,
                destination_wallet=WALLET_FUNDING,
                asset="USDT",
                amount=1,
            ),
            limits=_limits(tmp_path),
            client=client,
        )

    assert client.requests == []
    assert client.write_calls == 0


@pytest.mark.asyncio
async def test_transfer_canary_scopes_isolated_margin_symbol_before_reads(
    tmp_path: Path,
) -> None:
    client = FakeTransferCanaryClient()

    with pytest.raises(TransferCanaryError, match="BNB/USDT"):
        await run_transfer_canary(
            _settings(api_symbol_whitelist=("BTC/USDT",)),
            request=TransferRequest(
                source_wallet=WALLET_CROSS_MARGIN,
                destination_wallet=WALLET_ISOLATED_MARGIN,
                asset="USDT",
                amount=1,
                to_symbol="BNB/USDT",
            ),
            limits=_limits(tmp_path),
            client=client,
        )

    assert client.requests == []


@pytest.mark.asyncio
async def test_transfer_canary_emergency_stop_blocks_before_reads(
    tmp_path: Path,
) -> None:
    limits = _limits(tmp_path)
    Path(limits.emergency_stop_path).write_text("stop\n", encoding="utf-8")
    client = FakeTransferCanaryClient()

    with pytest.raises(TransferCanaryError, match="emergency stop"):
        await run_transfer_canary(
            _settings(),
            request=TransferRequest(
                source_wallet=WALLET_SPOT,
                destination_wallet=WALLET_FUNDING,
                asset="USDT",
                amount=1,
            ),
            limits=limits,
            client=client,
        )

    assert client.requests == []
    assert client.write_calls == 0


@pytest.mark.asyncio
async def test_transfer_canary_rejects_blank_audit_path_before_reads() -> None:
    client = FakeTransferCanaryClient()

    with pytest.raises(TransferCanaryError, match="audit_path must not be blank"):
        await run_transfer_canary(
            _settings(),
            request=TransferRequest(
                source_wallet=WALLET_SPOT,
                destination_wallet=WALLET_FUNDING,
                asset="USDT",
                amount=1,
            ),
            limits=TransferCanaryLimits(audit_path=""),
            client=client,
        )

    assert client.requests == []


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("kwargs", "error"),
    [
        ({"submit_transfer": True}, "submission is not implemented"),
        ({"confirm_live_transfer": True}, "confirmation is disabled"),
    ],
)
async def test_transfer_canary_submission_switches_fail_closed_before_reads(
    tmp_path: Path,
    kwargs: dict[str, bool],
    error: str,
) -> None:
    client = FakeTransferCanaryClient()

    with pytest.raises(TransferCanaryError, match=error):
        await run_transfer_canary(
            _settings(),
            request=TransferRequest(
                source_wallet=WALLET_SPOT,
                destination_wallet=WALLET_FUNDING,
                asset="USDT",
                amount=1,
            ),
            limits=_limits(tmp_path),
            client=client,
            **kwargs,
        )

    assert client.requests == []
    assert client.write_calls == 0


@pytest.mark.asyncio
async def test_transfer_canary_surfaces_margin_debt_rejection_and_audits(
    tmp_path: Path,
) -> None:
    client = FakeTransferCanaryClient(margin_borrowed="1", margin_interest="0.01")

    with pytest.raises(TransferCanaryError, match="outstanding debt") as exc_info:
        await run_transfer_canary(
            _settings(),
            request=TransferRequest(
                source_wallet=WALLET_CROSS_MARGIN,
                destination_wallet=WALLET_SPOT,
                asset="USDT",
                amount=1,
            ),
            limits=_limits(tmp_path),
            client=client,
        )

    result = exc_info.value.result
    assert result is not None
    assert result.status == "blocked"
    assert result.submission_attempted is False
    assert client.write_calls == 0
    row = json.loads(Path(result.audit_path).read_text(encoding="utf-8"))
    assert "outstanding debt" in row["error"]
    assert row["plan"]["allowed"] is False


@pytest.mark.asyncio
async def test_transfer_canary_surfaces_insufficient_balance_rejection(
    tmp_path: Path,
) -> None:
    client = FakeTransferCanaryClient(spot_free="0.5")

    with pytest.raises(
        TransferCanaryError,
        match="amount exceeds source transferable balance",
    ) as exc_info:
        await run_transfer_canary(
            _settings(),
            request=TransferRequest(
                source_wallet=WALLET_SPOT,
                destination_wallet=WALLET_FUNDING,
                asset="USDT",
                amount=1,
            ),
            limits=_limits(tmp_path),
            client=client,
        )

    assert exc_info.value.result is not None
    assert exc_info.value.result.plan["allowed"] is False
    assert client.write_calls == 0


def test_transfer_canary_cli_help_and_submission_mode_fail_closed(
    capsys: pytest.CaptureFixture[str],
) -> None:
    with pytest.raises(SystemExit) as exc_info:
        run(["--help"])
    assert exc_info.value.code == 0
    help_output = capsys.readouterr().out
    assert "--plan-only" in help_output
    assert "--submit-transfer" in help_output
    assert "--confirm-live-transfer" in help_output

    assert run(["--submit-transfer"]) == 1
    assert "--plan-only is required" in capsys.readouterr().out

    assert run(["--plan-only", "--submit-transfer"]) == 1
    assert "submission is not implemented" in capsys.readouterr().out

    assert run(["--plan-only", "--confirm-live-transfer"]) == 1
    assert "confirmation is disabled" in capsys.readouterr().out

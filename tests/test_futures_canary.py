from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from src.config import Settings
from src.futures import (
    WRITE_ENDPOINTS_BLOCKED,
    FuturesOrderIntent,
    FuturesRiskLimits,
)
from src.futures_canary import (
    FuturesCanaryControls,
    FuturesCanaryError,
    FuturesExitPlanIntent,
    run,
    run_futures_canary,
)


class ReadOnlyFuturesClient:
    def __init__(self, *, leverage: str = "3") -> None:
        self.leverage = leverage
        self.requests: list[str] = []

    async def request(self, method: str, *_args: Any, **_kwargs: Any) -> Any:
        self.requests.append(method)
        if method in WRITE_ENDPOINTS_BLOCKED or "Post" in method:
            raise AssertionError(f"futures write endpoint was called: {method}")
        if method == "fapiPrivateV3GetAccount":
            return {
                "totalWalletBalance": "200",
                "totalUnrealizedProfit": "0",
                "totalMarginBalance": "200",
                "totalInitialMargin": "0",
                "totalMaintMargin": "1",
                "totalPositionInitialMargin": "0",
                "totalOpenOrderInitialMargin": "0",
                "totalCrossWalletBalance": "200",
                "totalCrossUnPnl": "0",
                "availableBalance": "180",
                "maxWithdrawAmount": "180",
                "assets": [],
            }
        if method == "fapiPrivateV3GetPositionRisk":
            return [
                {
                    "symbol": "BTCUSDT",
                    "positionSide": "BOTH",
                    "positionAmt": "0",
                    "entryPrice": "0",
                    "breakEvenPrice": "0",
                    "markPrice": "50000",
                    "unRealizedProfit": "0",
                    "liquidationPrice": "0",
                    "notional": "0",
                    "marginAsset": "USDT",
                    "isolatedMargin": "0",
                    "isolatedWallet": "0",
                    "initialMargin": "0",
                    "maintMargin": "0",
                    "positionInitialMargin": "0",
                    "openOrderInitialMargin": "0",
                    "leverage": self.leverage,
                    "marginType": "cross",
                }
            ]
        if method == "fapiPublicGetExchangeInfo":
            return {
                "symbols": [
                    {
                        "symbol": "BTCUSDT",
                        "contractType": "PERPETUAL",
                        "status": "TRADING",
                        "baseAsset": "BTC",
                        "quoteAsset": "USDT",
                        "marginAsset": "USDT",
                        "filters": [
                            {
                                "filterType": "PRICE_FILTER",
                                "minPrice": "1",
                                "maxPrice": "1000000",
                                "tickSize": "0.1",
                            },
                            {
                                "filterType": "LOT_SIZE",
                                "minQty": "0.001",
                                "maxQty": "100",
                                "stepSize": "0.001",
                            },
                            {"filterType": "MIN_NOTIONAL", "notional": "5"},
                        ],
                    }
                ]
            }
        if method == "fapiPrivateGetSymbolConfig":
            return [
                {
                    "symbol": "BTCUSDT",
                    "marginType": "CROSSED",
                    "leverage": self.leverage,
                    "maxNotionalValue": "1000000",
                }
            ]
        if method == "fapiPrivateGetLeverageBracket":
            return [
                {
                    "symbol": "BTCUSDT",
                    "brackets": [
                        {
                            "initialLeverage": "20",
                            "notionalFloor": "0",
                            "notionalCap": "50000",
                            "maintMarginRatio": "0.004",
                            "cum": "0",
                        }
                    ],
                }
            ]
        if method == "fapiPublicGetPremiumIndex":
            return {
                "symbol": "BTCUSDT",
                "markPrice": "50000",
                "indexPrice": "49990",
                "estimatedSettlePrice": "49995",
                "lastFundingRate": "0.0001",
                "interestRate": "0.0001",
            }
        raise AssertionError(f"unexpected request: {method}")


def _settings(**overrides: Any) -> Settings:
    values = {
        "api_key": "key",
        "api_secret": "secret",
        "use_testnet": False,
        "trading_symbol": "BTC/USDT",
        "api_symbol_whitelist": ("BTC/USDT",),
    }
    values.update(overrides)
    return Settings(**values)


def _limits() -> FuturesRiskLimits:
    return FuturesRiskLimits(
        max_leverage=3,
        max_notional_usdt=100,
        max_account_notional_percent=100,
        max_initial_margin_usdt=20,
        min_free_balance_after_initial_margin_usdt=5,
        min_liquidation_distance_percent=20,
        max_abs_funding_rate=0.001,
        max_abs_funding_payment_usdt=0.05,
        min_maintenance_buffer_percent=50,
    )


def _entry() -> FuturesOrderIntent:
    return FuturesOrderIntent(symbol="BTC/USDT", side="BUY", quantity=0.001)


def _exit(**overrides: Any) -> FuturesExitPlanIntent:
    values = {
        "symbol": "BTC/USDT",
        "side": "SELL",
        "quantity": 0.001,
        "trigger": "operator-approved stop or target closure",
        "reduce_only": True,
    }
    values.update(overrides)
    return FuturesExitPlanIntent(**values)


@pytest.mark.asyncio
async def test_plan_only_canary_records_allowed_entry_and_reduce_only_exit(
    tmp_path: Path,
) -> None:
    client = ReadOnlyFuturesClient()
    audit_path = tmp_path / "futures-canary.jsonl"

    result = await run_futures_canary(
        _settings(),
        intent=_entry(),
        exit_plan_intent=_exit(),
        limits=_limits(),
        controls=FuturesCanaryControls(
            audit_path=str(audit_path),
            emergency_stop_path=str(tmp_path / "stop"),
        ),
        client=client,
    )

    assert result.status == "planned"
    assert result.live_execution_enabled is False
    assert result.submission_attempted is False
    assert result.write_endpoints_used == ()
    assert result.entry_plan["allowed"] is True
    assert result.exit_plan_intent["reduce_only"] is True
    assert result.exit_plan_intent["trigger"] == "operator-approved stop or target closure"
    assert not set(client.requests).intersection(WRITE_ENDPOINTS_BLOCKED)
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "planned"
    assert payload["audit_path"] == str(audit_path)
    assert payload["exit_plan_intent"]["reduce_only"] is True


@pytest.mark.asyncio
async def test_scope_block_occurs_before_any_futures_request(tmp_path: Path) -> None:
    client = ReadOnlyFuturesClient()

    with pytest.raises(FuturesCanaryError, match="API scope audit failed"):
        await run_futures_canary(
            _settings(api_symbol_whitelist=()),
            intent=_entry(),
            exit_plan_intent=_exit(),
            limits=_limits(),
            controls=FuturesCanaryControls(
                audit_path=str(tmp_path / "audit.jsonl"),
                emergency_stop_path=str(tmp_path / "stop"),
            ),
            client=client,
        )

    assert client.requests == []


@pytest.mark.asyncio
async def test_emergency_stop_blocks_before_any_futures_request(tmp_path: Path) -> None:
    client = ReadOnlyFuturesClient()
    stop_path = tmp_path / "futures.stop"
    stop_path.write_text("stop\n", encoding="utf-8")

    with pytest.raises(FuturesCanaryError, match="emergency stop is active"):
        await run_futures_canary(
            _settings(),
            intent=_entry(),
            exit_plan_intent=_exit(),
            limits=_limits(),
            controls=FuturesCanaryControls(
                audit_path=str(tmp_path / "audit.jsonl"),
                emergency_stop_path=str(stop_path),
            ),
            client=client,
        )

    assert client.requests == []


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("kwargs", "message"),
    [
        ({"submit_order": True}, "order submission is disabled"),
        ({"confirm_live_futures": True}, "live confirmation is disabled"),
    ],
)
async def test_submission_and_live_confirmation_fail_closed_before_reads(
    tmp_path: Path,
    kwargs: dict[str, bool],
    message: str,
) -> None:
    client = ReadOnlyFuturesClient()

    with pytest.raises(FuturesCanaryError, match=message):
        await run_futures_canary(
            _settings(),
            intent=_entry(),
            exit_plan_intent=_exit(),
            limits=_limits(),
            controls=FuturesCanaryControls(
                audit_path=str(tmp_path / "audit.jsonl"),
                emergency_stop_path=str(tmp_path / "stop"),
            ),
            client=client,
            **kwargs,
        )

    assert client.requests == []


@pytest.mark.asyncio
async def test_leverage_failure_is_surfaced_and_audited(tmp_path: Path) -> None:
    client = ReadOnlyFuturesClient(leverage="10")
    audit_path = tmp_path / "futures-canary.jsonl"

    result = await run_futures_canary(
        _settings(),
        intent=_entry(),
        exit_plan_intent=_exit(),
        limits=_limits(),
        controls=FuturesCanaryControls(
            audit_path=str(audit_path),
            emergency_stop_path=str(tmp_path / "stop"),
        ),
        client=client,
    )

    assert result.status == "blocked"
    assert "configured leverage exceeds the risk limit" in result.rejection_reasons
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "blocked"
    assert "configured leverage exceeds the risk limit" in payload["rejection_reasons"]


@pytest.mark.asyncio
async def test_exit_plan_must_be_reduce_only_before_reads(tmp_path: Path) -> None:
    client = ReadOnlyFuturesClient()

    with pytest.raises(FuturesCanaryError, match="must be reduce-only"):
        await run_futures_canary(
            _settings(),
            intent=_entry(),
            exit_plan_intent=_exit(reduce_only=False),
            limits=_limits(),
            controls=FuturesCanaryControls(
                audit_path=str(tmp_path / "audit.jsonl"),
                emergency_stop_path=str(tmp_path / "stop"),
            ),
            client=client,
        )

    assert client.requests == []


def test_cli_help_and_live_flags_fail_closed_without_environment(
    capsys: pytest.CaptureFixture[str],
) -> None:
    with pytest.raises(SystemExit) as excinfo:
        run(["--help"])
    assert excinfo.value.code == 0
    assert "plan a foreground usd-m futures canary" in capsys.readouterr().out.lower()

    assert run(["--submit-order"]) == 1
    assert "live futures submission is disabled" in capsys.readouterr().out.lower()
    assert run(["--confirm-live-futures"]) == 1
    assert "live futures submission is disabled" in capsys.readouterr().out.lower()

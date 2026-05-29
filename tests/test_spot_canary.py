from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest

from src.config import Settings
from src.spot_canary import (
    SpotCanaryError,
    SpotCanaryLimits,
    build_spot_canary_plan,
    run,
    run_spot_canary,
)


def _settings(**overrides: Any) -> Settings:
    values = {
        "api_key": "key",
        "api_secret": "secret",
        "use_testnet": False,
        "dry_run": False,
        "confirm_live_trading": "yes",
        "api_symbol_whitelist": ("BTC/USDT", "BNB/USDT"),
        "api_asset_whitelist": ("BNB", "USDT"),
    }
    values.update(overrides)
    return Settings(**values)


def _market(symbol: str = "BNB/USDT") -> dict[str, Any]:
    base, quote = symbol.split("/")
    return {
        "symbol": symbol,
        "base": base,
        "quote": quote,
        "spot": True,
        "active": True,
        "precision": {"amount": 0.00001},
        "limits": {"amount": {"min": 0.00001}, "cost": {"min": 5.0}},
    }


class FakeCanaryClient:
    def __init__(
        self,
        *,
        free: dict[str, float] | None = None,
        markets: dict[str, Any] | None = None,
        ticker: dict[str, float] | None = None,
        order_test_error: Exception | None = None,
        order_error: Exception | None = None,
    ) -> None:
        self.free = {"BNB": 0.02, "USDT": 1.0} if free is None else free
        self.markets = {"BNB/USDT": _market()} if markets is None else markets
        self.ticker = {"bid": 600.0, "ask": 600.6, "last": 600.0} if ticker is None else ticker
        self.order_test_error = order_test_error
        self.order_error = order_error
        self.closed = False
        self.order_test_calls = 0
        self.order_calls = 0
        self.balance_reads = 0
        self.requests: list[tuple[str, tuple[Any, ...]]] = []

    async def get_balance(self) -> dict[str, dict[str, float]]:
        self.balance_reads += 1
        return {"free": self.free, "total": dict(self.free)}

    async def request(self, method: str, *args: Any, **_: Any) -> Any:
        self.requests.append((method, args))
        if method == "privateGetAccount":
            return {
                "canTrade": True,
                "canWithdraw": False,
                "accountType": "SPOT",
                "permissions": ["SPOT"],
            }
        if method == "load_markets":
            return self.markets
        if method == "privatePostOrderTest":
            self.order_test_calls += 1
            if self.order_test_error is not None:
                raise self.order_test_error
            return {}
        if method == "fetch_order":
            return {
                "id": "canary-1",
                "status": "closed",
                "filled": 0.01,
                "average": 600.0,
            }
        raise AssertionError(f"unexpected request: {method}")

    async def fetch_ticker(self, symbol: str) -> dict[str, float]:
        assert symbol == "BNB/USDT"
        return self.ticker

    async def get_commission_rates(self, symbol: str) -> dict[str, Any]:
        assert symbol == "BNB/USDT"
        return {
            "symbol": "BNBUSDT",
            "standardCommission": {
                "maker": "0.001",
                "taker": "0.001",
                "buyer": "0",
                "seller": "0",
            },
            "discount": {"enabledForAccount": False, "enabledForSymbol": False},
        }

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
    ) -> dict[str, Any]:
        self.order_calls += 1
        assert symbol == "BNB/USDT"
        assert order_type == "market"
        assert side == "sell"
        assert amount == pytest.approx(0.01)
        if self.order_error is not None:
            raise self.order_error
        return {
            "id": "canary-1",
            "status": "closed",
            "filled": amount,
            "average": 600.0,
            "fee": {"currency": "USDT", "cost": 0.006},
        }

    async def close(self) -> None:
        self.closed = True


class FakeAlerter:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> bool:
        self.messages.append(message)
        return True


@pytest.mark.asyncio
async def test_build_spot_canary_plan_uses_dynamic_limits_and_fees(
    tmp_path: Path,
) -> None:
    client = FakeCanaryClient()

    plan, balance = await build_spot_canary_plan(
        client,
        source_asset="BNB",
        destination_asset="USDT",
        limits=SpotCanaryLimits(audit_path=str(tmp_path / "audit.jsonl")),
    )

    assert plan.symbol == "BNB/USDT"
    assert plan.side == "sell"
    assert plan.amount == pytest.approx(0.01)
    assert plan.estimated_notional_usdt == pytest.approx(6.0)
    assert plan.expected_destination_amount == pytest.approx(5.994)
    assert plan.fee_rate == pytest.approx(0.001)
    assert plan.route_quote["legs"][0]["symbol"] == "BNB/USDT"
    assert balance["free"]["BNB"] == 0.02
    assert client.order_calls == 0
    assert client.order_test_calls == 0


@pytest.mark.asyncio
async def test_plan_only_canary_writes_audit_without_order(
    tmp_path: Path,
) -> None:
    client = FakeCanaryClient()
    audit_path = tmp_path / "spot-canary.jsonl"

    result = await run_spot_canary(
        _settings(),
        submit_order=False,
        source_asset="BNB",
        destination_asset="USDT",
        limits=SpotCanaryLimits(audit_path=str(audit_path)),
        client=client,
        alerter=FakeAlerter(),
    )

    assert result.status == "planned"
    assert result.submission_attempted is False
    assert result.audit_path == str(audit_path)
    assert client.order_calls == 0
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "planned"
    assert payload["plan"]["symbol"] == "BNB/USDT"


@pytest.mark.asyncio
async def test_canary_blocks_missing_local_symbol_scope_before_exchange_reads(
    tmp_path: Path,
) -> None:
    client = FakeCanaryClient()

    with pytest.raises(SpotCanaryError, match="API scope audit failed"):
        await run_spot_canary(
            _settings(api_symbol_whitelist=()),
            submit_order=False,
            source_asset="BNB",
            destination_asset="USDT",
            limits=SpotCanaryLimits(audit_path=str(tmp_path / "audit.jsonl")),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.balance_reads == 0
    assert client.requests == []
    assert client.order_calls == 0


@pytest.mark.asyncio
async def test_confirmed_canary_submits_one_order_after_all_gates(
    tmp_path: Path,
) -> None:
    client = FakeCanaryClient()
    alerter = FakeAlerter()
    audit_path = tmp_path / "spot-canary.jsonl"

    result = await run_spot_canary(
        _settings(),
        submit_order=True,
        confirm_live_canary=True,
        confirm_replay_passed=True,
        confirm_paper_warmup_reviewed=True,
        source_asset="BNB",
        destination_asset="USDT",
        limits=SpotCanaryLimits(audit_path=str(audit_path)),
        client=client,
        alerter=alerter,
    )

    assert result.status == "submitted"
    assert result.submission_attempted is True
    assert result.order["id"] == "canary-1"
    assert result.telegram_sent is True
    assert "SPOT LIVE CANARY" in alerter.messages[0]
    assert client.order_test_calls == 2
    assert client.order_calls == 1
    assert client.balance_reads == 2
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "submitted"
    assert payload["submission_attempted"] is True


@pytest.mark.asyncio
async def test_canary_requires_confirmation_flags_before_submit(
    tmp_path: Path,
) -> None:
    client = FakeCanaryClient()

    with pytest.raises(SpotCanaryError, match="--confirm-live-canary"):
        await run_spot_canary(
            _settings(),
            submit_order=True,
            source_asset="BNB",
            destination_asset="USDT",
            limits=SpotCanaryLimits(audit_path=str(tmp_path / "audit.jsonl")),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.order_calls == 0
    assert client.order_test_calls == 0


@pytest.mark.asyncio
async def test_canary_blocks_when_order_would_be_below_binance_minimum(
    tmp_path: Path,
) -> None:
    client = FakeCanaryClient()

    with pytest.raises(SpotCanaryError, match="below Binance minimum"):
        await build_spot_canary_plan(
            client,
            source_asset="BNB",
            destination_asset="USDT",
            limits=SpotCanaryLimits(
                max_order_notional_usdt=4.0,
                max_daily_notional_usdt=4.0,
                audit_path=str(tmp_path / "audit.jsonl"),
            ),
        )


@pytest.mark.asyncio
async def test_canary_enforces_daily_order_limit(tmp_path: Path) -> None:
    audit_path = tmp_path / "spot-canary.jsonl"
    audit_path.write_text(
        json.dumps(
            {
                "timestamp": datetime.now(timezone.utc).isoformat(),
                "submission_attempted": True,
                "plan": {"estimated_notional_usdt": 6.0},
            }
        )
        + "\n",
        encoding="utf-8",
    )

    with pytest.raises(SpotCanaryError, match="daily order limit"):
        await build_spot_canary_plan(
            FakeCanaryClient(),
            source_asset="BNB",
            destination_asset="USDT",
            limits=SpotCanaryLimits(audit_path=str(audit_path)),
        )


@pytest.mark.asyncio
async def test_canary_emergency_stop_blocks_before_exchange_reads(
    tmp_path: Path,
) -> None:
    stop_path = tmp_path / "spot-canary.stop"
    stop_path.write_text("stop\n", encoding="utf-8")
    client = FakeCanaryClient()

    with pytest.raises(SpotCanaryError, match="emergency stop"):
        await build_spot_canary_plan(
            client,
            source_asset="BNB",
            destination_asset="USDT",
            limits=SpotCanaryLimits(
                audit_path=str(tmp_path / "audit.jsonl"),
                emergency_stop_path=str(stop_path),
            ),
        )

    assert client.balance_reads == 0
    assert client.requests == []


@pytest.mark.asyncio
async def test_canary_permission_failure_audits_and_stops_before_order(
    tmp_path: Path,
) -> None:
    class PermissionDenied(RuntimeError):
        error_code = -2015

    client = FakeCanaryClient(
        order_test_error=PermissionDenied(
            "Invalid API-key, IP, or permissions for action."
        )
    )
    alerter = FakeAlerter()
    audit_path = tmp_path / "spot-canary.jsonl"

    with pytest.raises(SpotCanaryError, match="spot_live_canary is not allowed"):
        await run_spot_canary(
            _settings(),
            submit_order=True,
            confirm_live_canary=True,
            confirm_replay_passed=True,
            confirm_paper_warmup_reviewed=True,
            source_asset="BNB",
            destination_asset="USDT",
            limits=SpotCanaryLimits(audit_path=str(audit_path)),
            client=client,
            alerter=alerter,
        )

    assert client.order_calls == 0
    assert client.order_test_calls == 1
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "blocked"
    assert payload["submission_attempted"] is False
    assert "Spot & Margin Trading" in payload["error"]
    assert "SPOT LIVE CANARY" in alerter.messages[0]


def test_canary_cli_fails_closed_without_loading_environment(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)

    assert run([]) == 1

    assert "--confirm-live-canary" in capsys.readouterr().out

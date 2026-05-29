from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest

from src.config import Settings
from src.live_spot_allocator import (
    LiveSpotAllocatorError,
    LiveSpotIntent,
    LiveSpotLimits,
    build_live_spot_plan,
    run,
    run_live_spot_allocator,
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


def _market(symbol: str) -> dict[str, Any]:
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


class FakeLiveSpotClient:
    def __init__(
        self,
        *,
        order_test_error: Exception | None = None,
        order_error: Exception | None = None,
    ) -> None:
        self.order_test_error = order_test_error
        self.order_error = order_error
        self.balance_reads = 0
        self.order_test_calls = 0
        self.order_calls = 0
        self.open_order_reads = 0
        self.requests: list[tuple[str, tuple[Any, ...]]] = []

    async def get_balance(self) -> dict[str, dict[str, float]]:
        self.balance_reads += 1
        if self.order_calls:
            return {
                "free": {"BNB": 0.01, "USDT": 6.994},
                "total": {"BNB": 0.01, "USDT": 6.994},
            }
        return {
            "free": {"BNB": 0.02, "USDT": 1.0},
            "total": {"BNB": 0.02, "USDT": 1.0},
        }

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
            return {
                "BNB/USDT": _market("BNB/USDT"),
                "BTC/USDT": _market("BTC/USDT"),
                "PAXG/USDT": _market("PAXG/USDT"),
                "XAUT/USDT": _market("XAUT/USDT"),
            }
        if method == "privatePostOrderTest":
            self.order_test_calls += 1
            if self.order_test_error is not None:
                raise self.order_test_error
            return {}
        if method == "fetch_order":
            return {
                "id": "alloc-1",
                "status": "closed",
                "filled": 0.01,
                "average": 600.0,
            }
        if method == "fetch_open_orders":
            self.open_order_reads += 1
            return []
        raise AssertionError(f"unexpected request: {method}")

    async def fetch_ticker(self, symbol: str) -> dict[str, float]:
        tickers = {
            "BNB/USDT": {"bid": 600.0, "ask": 600.6, "last": 600.0},
            "BTC/USDT": {"bid": 77_000.0, "ask": 77_010.0, "last": 77_000.0},
            "PAXG/USDT": {"bid": 4_500.0, "ask": 4_501.0, "last": 4_500.0},
            "XAUT/USDT": {"bid": 4_490.0, "ask": 4_491.0, "last": 4_490.0},
        }
        return tickers[symbol]

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
            "id": "alloc-1",
            "status": "closed",
            "filled": amount,
            "average": 600.0,
            "fee": {"currency": "USDT", "cost": 0.006},
        }

    async def close(self) -> None:
        return None


class FakeAlerter:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> bool:
        self.messages.append(message)
        return True


def _intent() -> LiveSpotIntent:
    return LiveSpotIntent(source_asset="BNB", destination_asset="USDT")


@pytest.mark.asyncio
async def test_build_live_spot_plan_enforces_dynamic_risk_and_benchmarks(
    tmp_path: Path,
) -> None:
    client = FakeLiveSpotClient()

    plan, balance = await build_live_spot_plan(
        client,
        intent=_intent(),
        limits=LiveSpotLimits(audit_path=str(tmp_path / "alloc.jsonl")),
    )

    assert plan.symbol == "BNB/USDT"
    assert plan.side == "sell"
    assert plan.order_amount == pytest.approx(0.01)
    assert plan.source_spend_amount == pytest.approx(0.01)
    assert plan.estimated_notional_usdt == pytest.approx(6.0)
    assert plan.expected_destination_amount == pytest.approx(5.994)
    assert plan.destination_value_usdt_after == pytest.approx(6.994)
    assert plan.benchmark_values_after["USDT"] == pytest.approx(12.994)
    assert "BTC" in plan.benchmark_values_after
    assert plan.risk["max_route_legs"] == 1
    assert balance["total"]["BNB"] == 0.02
    assert client.order_calls == 0


@pytest.mark.asyncio
async def test_live_spot_plan_only_writes_audit_without_order(
    tmp_path: Path,
) -> None:
    client = FakeLiveSpotClient()
    audit_path = tmp_path / "alloc.jsonl"

    result = await run_live_spot_allocator(
        _settings(),
        submit_order=False,
        intent=_intent(),
        limits=LiveSpotLimits(audit_path=str(audit_path)),
        client=client,
        alerter=FakeAlerter(),
    )

    assert result.status == "planned"
    assert result.submission_attempted is False
    assert result.audit_path == str(audit_path)
    assert client.order_calls == 0
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["plan"]["symbol"] == "BNB/USDT"
    assert payload["submission_attempted"] is False


@pytest.mark.asyncio
async def test_live_spot_allocator_blocks_resolved_symbol_outside_local_scope(
    tmp_path: Path,
) -> None:
    client = FakeLiveSpotClient()
    audit_path = tmp_path / "alloc.jsonl"

    with pytest.raises(LiveSpotAllocatorError, match="API scope audit failed"):
        await run_live_spot_allocator(
            _settings(api_symbol_whitelist=("BTC/USDT",)),
            submit_order=False,
            intent=_intent(),
            limits=LiveSpotLimits(audit_path=str(audit_path)),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.order_calls == 0
    assert client.order_test_calls == 0
    assert not audit_path.exists()


@pytest.mark.asyncio
async def test_confirmed_live_spot_allocator_submits_one_order_and_reconciles(
    tmp_path: Path,
) -> None:
    client = FakeLiveSpotClient()
    alerter = FakeAlerter()
    audit_path = tmp_path / "alloc.jsonl"

    result = await run_live_spot_allocator(
        _settings(),
        submit_order=True,
        confirm_live_allocator=True,
        confirm_canary_reviewed=True,
        confirm_replay_passed=True,
        confirm_paper_warmup_reviewed=True,
        confirm_permission_diagnostics=True,
        intent=_intent(),
        limits=LiveSpotLimits(audit_path=str(audit_path)),
        client=client,
        alerter=alerter,
    )

    assert result.status == "submitted"
    assert result.order["id"] == "alloc-1"
    assert result.reconciliation["actual_source_delta"] == pytest.approx(-0.01)
    assert result.reconciliation["actual_destination_delta"] == pytest.approx(5.994)
    assert result.open_orders_after == []
    assert result.telegram_sent is True
    assert "LIVE SPOT ALLOCATOR" in alerter.messages[0]
    assert client.order_test_calls == 2
    assert client.order_calls == 1
    assert client.open_order_reads == 1


@pytest.mark.asyncio
async def test_live_spot_allocator_requires_submit_confirmations(
    tmp_path: Path,
) -> None:
    client = FakeLiveSpotClient()

    with pytest.raises(LiveSpotAllocatorError, match="--confirm-live-allocator"):
        await run_live_spot_allocator(
            _settings(),
            submit_order=True,
            intent=_intent(),
            limits=LiveSpotLimits(audit_path=str(tmp_path / "alloc.jsonl")),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.order_calls == 0
    assert client.requests == []


@pytest.mark.asyncio
async def test_live_spot_allocator_enforces_daily_order_limit(
    tmp_path: Path,
) -> None:
    audit_path = tmp_path / "alloc.jsonl"
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

    with pytest.raises(LiveSpotAllocatorError, match="daily order limit"):
        await build_live_spot_plan(
            FakeLiveSpotClient(),
            intent=_intent(),
            limits=LiveSpotLimits(audit_path=str(audit_path), max_daily_orders=1),
        )


@pytest.mark.asyncio
async def test_live_spot_allocator_enforces_turnover_limit(tmp_path: Path) -> None:
    with pytest.raises(LiveSpotAllocatorError, match="turnover"):
        await build_live_spot_plan(
            FakeLiveSpotClient(),
            intent=_intent(),
            limits=LiveSpotLimits(
                audit_path=str(tmp_path / "alloc.jsonl"),
                max_turnover_percent=10,
            ),
        )


@pytest.mark.asyncio
async def test_live_spot_allocator_permission_failure_audits_and_blocks(
    tmp_path: Path,
) -> None:
    class PermissionDenied(RuntimeError):
        error_code = -2015

    client = FakeLiveSpotClient(
        order_test_error=PermissionDenied(
            "Invalid API-key, IP, or permissions for action."
        )
    )
    audit_path = tmp_path / "alloc.jsonl"

    with pytest.raises(LiveSpotAllocatorError, match="spot_live_pool is not allowed"):
        await run_live_spot_allocator(
            _settings(),
            submit_order=True,
            confirm_live_allocator=True,
            confirm_canary_reviewed=True,
            confirm_replay_passed=True,
            confirm_paper_warmup_reviewed=True,
            confirm_permission_diagnostics=True,
            intent=_intent(),
            limits=LiveSpotLimits(audit_path=str(audit_path)),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.order_calls == 0
    assert client.order_test_calls == 1
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "blocked"
    assert payload["submission_attempted"] is False


@pytest.mark.asyncio
async def test_live_spot_allocator_order_error_does_not_retry(
    tmp_path: Path,
) -> None:
    client = FakeLiveSpotClient(order_error=RuntimeError("network vanished"))
    audit_path = tmp_path / "alloc.jsonl"

    with pytest.raises(LiveSpotAllocatorError, match="no retry"):
        await run_live_spot_allocator(
            _settings(),
            submit_order=True,
            confirm_live_allocator=True,
            confirm_canary_reviewed=True,
            confirm_replay_passed=True,
            confirm_paper_warmup_reviewed=True,
            confirm_permission_diagnostics=True,
            intent=_intent(),
            limits=LiveSpotLimits(audit_path=str(audit_path)),
            client=client,
            alerter=FakeAlerter(),
        )

    assert client.order_calls == 1
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["status"] == "submission_error"
    assert payload["submission_attempted"] is True


def test_live_spot_allocator_cli_fails_closed_without_loading_environment(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)

    assert run([]) == 1

    assert "--confirm-live-allocator" in capsys.readouterr().out

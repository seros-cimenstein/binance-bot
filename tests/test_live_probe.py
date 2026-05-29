from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from src.config import Settings
from src.live_probe import LiveProbeError, run, run_live_probe, select_live_sell_candidate


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


def _market(symbol: str, base: str) -> dict[str, Any]:
    return {
        "symbol": symbol,
        "base": base,
        "quote": "USDT",
        "spot": True,
        "active": True,
        "precision": {"amount": 0.00001},
        "limits": {"amount": {"min": 0.00001}, "cost": {"min": 5.0}},
    }


class FakeProbeClient:
    def __init__(
        self,
        *,
        free: dict[str, float] | None = None,
        markets: dict[str, Any] | None = None,
        tickers: dict[str, dict[str, float]] | None = None,
        order: dict[str, Any] | None = None,
        raise_on_order_test: Exception | None = None,
        raise_on_order: Exception | None = None,
    ) -> None:
        self.free = {"BNB": 0.02} if free is None else free
        self.markets = {"BNB/USDT": _market("BNB/USDT", "BNB")} if markets is None else markets
        self.tickers = {"BNB/USDT": {"bid": 600.0, "last": 600.0}} if tickers is None else tickers
        self.order = order if order is not None else {
            "id": "probe-1",
            "status": "closed",
            "filled": 0.00834,
            "average": 600.0,
            "fee": {"cost": 0.00001, "currency": "BNB"},
        }
        self.raise_on_order = raise_on_order
        self.raise_on_order_test = raise_on_order_test
        self.order_test_calls = 0
        self.order_calls = 0
        self.closed = False
        self.request_calls: list[tuple[str, tuple[Any, ...]]] = []

    async def get_balance(self) -> dict[str, dict[str, float]]:
        return {"free": self.free, "total": dict(self.free)}

    async def request(self, method: str, *args: Any, **kwargs: Any) -> dict[str, Any]:
        self.request_calls.append((method, args))
        if method == "load_markets":
            return self.markets
        if method == "fetch_order":
            return self.order
        if method == "privatePostOrderTest":
            self.order_test_calls += 1
            if self.raise_on_order_test is not None:
                raise self.raise_on_order_test
            return {}
        raise AssertionError(f"unexpected request: {method}")

    async def fetch_ticker(self, symbol: str) -> dict[str, float]:
        return self.tickers[symbol]

    async def get_commission_rates(self, symbol: str) -> dict[str, Any]:
        return {
            "symbol": symbol,
            "standardCommission": {"taker": "0.001"},
            "discount": {"enabledForAccount": True, "enabledForSymbol": True},
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
        assert amount == pytest.approx(0.00834)
        if self.raise_on_order is not None:
            raise self.raise_on_order
        return self.order

    async def close(self) -> None:
        self.closed = True


class FakeAlerter:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> bool:
        self.messages.append(message)
        return True


@pytest.mark.asyncio
async def test_select_live_sell_candidate_prefers_bnb_and_minimum_valid_notional() -> None:
    client = FakeProbeClient(
        free={"ETH": 1.0, "BNB": 0.02},
        markets={
            "BNB/USDT": _market("BNB/USDT", "BNB"),
            "ETH/USDT": _market("ETH/USDT", "ETH"),
        },
        tickers={
            "BNB/USDT": {"bid": 600.0, "last": 600.0},
            "ETH/USDT": {"bid": 3000.0, "last": 3000.0},
        },
    )

    candidate = await select_live_sell_candidate(
        client,
        client.free,
        client.markets,
        max_notional_usdt=6.0,
    )

    assert candidate.symbol == "BNB/USDT"
    assert candidate.sell_amount == pytest.approx(0.00834)
    assert candidate.estimated_notional_usdt == pytest.approx(5.004)
    assert candidate.cap_exceeded_to_satisfy_minimum is False


@pytest.mark.asyncio
async def test_select_live_sell_candidate_aborts_without_direct_usdt_market() -> None:
    client = FakeProbeClient(free={"BNB": 0.02}, markets={})

    with pytest.raises(LiveProbeError, match="direct USDT market"):
        await select_live_sell_candidate(
            client,
            client.free,
            client.markets,
            max_notional_usdt=6.0,
        )


@pytest.mark.asyncio
async def test_select_live_sell_candidate_allows_minimum_to_exceed_preferred_cap() -> None:
    market = _market("BNB/USDT", "BNB")
    market["limits"]["cost"]["min"] = 10.0
    client = FakeProbeClient(free={"BNB": 0.02}, markets={"BNB/USDT": market})

    candidate = await select_live_sell_candidate(
        client,
        client.free,
        client.markets,
        max_notional_usdt=6.0,
    )

    assert candidate.sell_amount == pytest.approx(0.01667)
    assert candidate.estimated_notional_usdt > 10.0
    assert candidate.cap_exceeded_to_satisfy_minimum is True


@pytest.mark.asyncio
async def test_live_probe_writes_audit_and_sends_live_probe_alert(tmp_path: Path) -> None:
    client = FakeProbeClient()
    alerter = FakeAlerter()
    audit_path = tmp_path / "live-probes.jsonl"

    result = await run_live_probe(
        _settings(),
        confirm_real_order=True,
        client=client,
        alerter=alerter,
        audit_path=audit_path,
    )

    assert result.order_id == "probe-1"
    assert result.order_status == "closed"
    assert result.audit_path == str(audit_path)
    assert result.telegram_sent is True
    assert "LIVE PROBE" in alerter.messages[0]
    assert client.order_test_calls == 1
    assert client.order_calls == 1
    payload = json.loads(audit_path.read_text(encoding="utf-8").strip())
    assert payload["symbol"] == "BNB/USDT"
    assert payload["side"] == "sell"
    assert payload["audit_path"] == ""


@pytest.mark.asyncio
async def test_live_probe_blocks_resolved_symbol_outside_local_scope(
    tmp_path: Path,
) -> None:
    client = FakeProbeClient()

    with pytest.raises(LiveProbeError, match="API scope audit failed"):
        await run_live_probe(
            _settings(api_symbol_whitelist=("BTC/USDT",)),
            confirm_real_order=True,
            client=client,
            alerter=FakeAlerter(),
            audit_path=tmp_path / "audit.jsonl",
        )

    assert client.order_test_calls == 0
    assert client.order_calls == 0
    assert not (tmp_path / "audit.jsonl").exists()


@pytest.mark.asyncio
async def test_live_probe_requires_explicit_confirmation() -> None:
    with pytest.raises(LiveProbeError, match="--confirm-real-order"):
        await run_live_probe(
            _settings(),
            confirm_real_order=False,
            client=FakeProbeClient(),
            alerter=FakeAlerter(),
        )


def test_live_probe_cli_fails_closed_without_loading_environment(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)

    assert run([]) == 1

    assert "--confirm-real-order" in capsys.readouterr().out


@pytest.mark.asyncio
async def test_live_probe_does_not_retry_after_ambiguous_order_error(
    tmp_path: Path,
) -> None:
    client = FakeProbeClient(raise_on_order=RuntimeError("network vanished"))

    with pytest.raises(RuntimeError, match="network vanished"):
        await run_live_probe(
            _settings(),
            confirm_real_order=True,
            client=client,
            alerter=FakeAlerter(),
            audit_path=tmp_path / "audit.jsonl",
        )

    assert client.order_test_calls == 1
    assert client.order_calls == 1
    assert not (tmp_path / "audit.jsonl").exists()


@pytest.mark.asyncio
async def test_live_probe_stops_before_order_when_spot_permission_probe_fails(
    tmp_path: Path,
) -> None:
    class PermissionError(RuntimeError):
        error_code = -2015

    client = FakeProbeClient(
        raise_on_order_test=PermissionError(
            "Invalid API-key, IP, or permissions for action."
        )
    )

    with pytest.raises(LiveProbeError, match="Spot & Margin Trading"):
        await run_live_probe(
            _settings(),
            confirm_real_order=True,
            client=client,
            alerter=FakeAlerter(),
            audit_path=tmp_path / "audit.jsonl",
        )

    assert client.order_test_calls == 1
    assert client.order_calls == 0
    assert not (tmp_path / "audit.jsonl").exists()

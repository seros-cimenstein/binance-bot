"""Tests for the funding-arb live executor (mocked exchange surfaces)."""

from __future__ import annotations

from typing import Any, Mapping

import pytest

from src.funding_arb import FundingArbError, FundingArbPosition
from src.funding_arb_executor import (
    FundingArbExecutor,
    ReconciliationReport,
)


class FakeSpot:
    def __init__(self, *, last: float = 200.0, fill_ratio: float = 1.0) -> None:
        self.last = last
        self.fill_ratio = fill_ratio
        self.calls: list[dict[str, Any]] = []
        self.balance = {"free": {"BTC": 0.0, "USDT": 10_000.0}}

    async def fetch_ticker(self, symbol: str) -> Mapping[str, Any]:
        return {"last": self.last, "symbol": symbol}

    async def create_order(
        self,
        symbol: str,
        type: str,
        side: str,
        amount: float,
        price: float | None = None,
        params: Mapping[str, Any] | None = None,
    ) -> Mapping[str, Any]:
        self.calls.append(
            {"venue": "spot", "symbol": symbol, "side": side, "amount": amount, "params": params}
        )
        filled = amount * self.fill_ratio
        return {
            "id": f"spot-{len(self.calls)}",
            "filled": filled,
            "average": self.last,
            "cost": filled * self.last,
        }

    async def fetch_balance(self) -> Mapping[str, Any]:
        return self.balance


class FailingSpotOnSecondCall(FakeSpot):
    async def create_order(self, *args, **kwargs):  # type: ignore[override]
        order = await super().create_order(*args, **kwargs)
        if len(self.calls) == 2:
            raise RuntimeError("simulated spot sell rollback failure")
        return order


class FakeFutures:
    def __init__(self, *, last: float = 200.0, fill_ratio: float = 1.0, fail_on_open: bool = False) -> None:
        self.last = last
        self.fill_ratio = fill_ratio
        self.fail_on_open = fail_on_open
        self.calls: list[dict[str, Any]] = []
        self.positions: list[dict[str, Any]] = []
        self.balance = {"free": {"USDT": 5_000.0}}

    async def fetch_ticker(self, symbol: str) -> Mapping[str, Any]:
        return {"last": self.last, "symbol": symbol}

    async def create_order(
        self,
        symbol: str,
        type: str,
        side: str,
        amount: float,
        price: float | None = None,
        params: Mapping[str, Any] | None = None,
    ) -> Mapping[str, Any]:
        self.calls.append(
            {"venue": "futures", "symbol": symbol, "side": side, "amount": amount, "params": params}
        )
        if self.fail_on_open and side == "sell":
            raise RuntimeError("simulated perp short failure")
        filled = amount * self.fill_ratio
        return {
            "id": f"futures-{len(self.calls)}",
            "filled": filled,
            "average": self.last,
            "cost": filled * self.last,
        }

    async def fetch_balance(self) -> Mapping[str, Any]:
        return self.balance

    async def fetch_positions(self, symbols: list[str] | None = None) -> list[Mapping[str, Any]]:
        return list(self.positions)


class FakeCollateral:
    def __init__(self, *, fail: bool = False) -> None:
        self.fail = fail
        self.calls: list[dict[str, Any]] = []

    async def transfer_spot_to_futures(self, amount_usdt: float) -> Mapping[str, Any]:
        self.calls.append({"dir": "spot->futures", "amount": amount_usdt})
        if self.fail:
            raise RuntimeError("simulated transfer failure")
        return {"ok": True}

    async def transfer_futures_to_spot(self, amount_usdt: float) -> Mapping[str, Any]:
        self.calls.append({"dir": "futures->spot", "amount": amount_usdt})
        return {"ok": True}


def _exec(**overrides) -> FundingArbExecutor:
    return FundingArbExecutor(
        spot=overrides.get("spot", FakeSpot()),
        futures=overrides.get("futures", FakeFutures()),
        collateral=overrides.get("collateral", FakeCollateral()),
        leverage=overrides.get("leverage", 1),
    )


@pytest.mark.asyncio
async def test_open_position_places_both_legs_and_returns_position() -> None:
    spot = FakeSpot(last=200.0)
    futures = FakeFutures(last=200.0)
    collateral = FakeCollateral()
    executor = _exec(spot=spot, futures=futures, collateral=collateral)
    result = await executor.open_position(
        "BTC/USDT", notional_usdt=2_000.0, entry_funding_bps=5.0, open_time_ms=123
    )
    assert result.spot_leg.side == "buy"
    assert result.spot_leg.filled_qty == pytest.approx(10.0)
    assert result.perp_leg.side == "sell"
    assert result.perp_leg.filled_qty == pytest.approx(10.0)
    assert result.position.spot_qty == pytest.approx(10.0)
    assert result.position.perp_qty == pytest.approx(-10.0)
    assert collateral.calls == [{"dir": "spot->futures", "amount": 2_000.0}]


@pytest.mark.asyncio
async def test_open_rolls_back_spot_on_perp_failure() -> None:
    spot = FakeSpot(last=200.0)
    futures = FakeFutures(fail_on_open=True)
    executor = _exec(spot=spot, futures=futures)
    with pytest.raises(FundingArbError, match="perp short failed"):
        await executor.open_position(
            "BTC/USDT", notional_usdt=2_000.0, entry_funding_bps=5.0, open_time_ms=0
        )
    # Spot was called twice: buy + rollback sell.
    sides = [c["side"] for c in spot.calls]
    assert sides == ["buy", "sell"]


@pytest.mark.asyncio
async def test_open_raises_clearly_when_rollback_also_fails() -> None:
    spot = FailingSpotOnSecondCall(last=200.0)
    futures = FakeFutures(fail_on_open=True)
    executor = _exec(spot=spot, futures=futures)
    with pytest.raises(FundingArbError, match="manual reconciliation required"):
        await executor.open_position(
            "BTC/USDT", notional_usdt=2_000.0, entry_funding_bps=5.0, open_time_ms=0
        )


@pytest.mark.asyncio
async def test_open_rejects_non_positive_notional() -> None:
    executor = _exec()
    with pytest.raises(FundingArbError, match="notional_usdt"):
        await executor.open_position("BTC/USDT", notional_usdt=0.0, entry_funding_bps=5.0, open_time_ms=0)


@pytest.mark.asyncio
async def test_open_rejects_when_spot_price_unavailable() -> None:
    spot = FakeSpot(last=0.0)
    executor = _exec(spot=spot)
    with pytest.raises(FundingArbError, match="spot price unavailable"):
        await executor.open_position("BTC/USDT", notional_usdt=2_000.0, entry_funding_bps=5.0, open_time_ms=0)


@pytest.mark.asyncio
async def test_close_position_runs_perp_then_spot_and_returns_pnl() -> None:
    spot = FakeSpot(last=210.0)
    futures = FakeFutures(last=210.0)
    collateral = FakeCollateral()
    executor = _exec(spot=spot, futures=futures, collateral=collateral)
    position = FundingArbPosition(
        symbol="BTC/USDT",
        open_time_ms=0,
        entry_funding_bps=5.0,
        notional_usdt=2_000.0,
        spot_qty=10.0,
        perp_qty=-10.0,
        funding_received_usdt=8.0,
    )
    result = await executor.close_position(position)
    assert result.perp_leg.side == "buy"
    assert result.perp_leg.filled_qty == pytest.approx(10.0)
    assert result.spot_leg.side == "sell"
    assert result.spot_leg.filled_qty == pytest.approx(10.0)
    # spot_proceeds == perp_cost (both at same price) → delta-neutral leg P&L = 0,
    # so realized = funding_received_usdt.
    assert result.realized_pnl_usdt == pytest.approx(8.0)
    assert collateral.calls == [{"dir": "futures->spot", "amount": 2_000.0}]


@pytest.mark.asyncio
async def test_close_first_leg_uses_reduce_only() -> None:
    spot = FakeSpot(last=200.0)
    futures = FakeFutures(last=200.0)
    executor = _exec(spot=spot, futures=futures)
    position = FundingArbPosition(
        symbol="BTC/USDT",
        open_time_ms=0,
        entry_funding_bps=5.0,
        notional_usdt=2_000.0,
        spot_qty=10.0,
        perp_qty=-10.0,
    )
    await executor.close_position(position)
    assert futures.calls[-1]["params"] == {"reduceOnly": True}


@pytest.mark.asyncio
async def test_close_rejects_position_with_no_qty() -> None:
    executor = _exec()
    with pytest.raises(FundingArbError, match="no spot quantity"):
        await executor.close_position(
            FundingArbPosition(
                symbol="BTC/USDT", open_time_ms=0, entry_funding_bps=5.0,
                notional_usdt=0.0, spot_qty=0.0, perp_qty=0.0,
            )
        )


@pytest.mark.asyncio
async def test_reconcile_passes_when_balances_match() -> None:
    spot = FakeSpot()
    spot.balance = {"free": {"BTC": 10.5, "USDT": 1_000.0}}
    futures = FakeFutures()
    futures.positions = [{"symbol": "BTC/USDT", "contracts": -10.0}]
    executor = _exec(spot=spot, futures=futures)
    position = FundingArbPosition(
        symbol="BTC/USDT", open_time_ms=0, entry_funding_bps=5.0,
        notional_usdt=2_000.0, spot_qty=10.0, perp_qty=-10.0,
    )
    report = await executor.reconcile({position.symbol: position})
    assert report.ok
    assert report.mismatches == ()


@pytest.mark.asyncio
async def test_reconcile_flags_spot_shortfall() -> None:
    spot = FakeSpot()
    spot.balance = {"free": {"BTC": 5.0, "USDT": 1_000.0}}  # not enough
    futures = FakeFutures()
    futures.positions = [{"symbol": "BTC/USDT", "contracts": -10.0}]
    executor = _exec(spot=spot, futures=futures)
    position = FundingArbPosition(
        symbol="BTC/USDT", open_time_ms=0, entry_funding_bps=5.0,
        notional_usdt=2_000.0, spot_qty=10.0, perp_qty=-10.0,
    )
    report = await executor.reconcile({position.symbol: position})
    assert not report.ok
    assert any("spot BTC balance" in m.detail for m in report.mismatches)


@pytest.mark.asyncio
async def test_reconcile_flags_perp_position_drift() -> None:
    spot = FakeSpot()
    spot.balance = {"free": {"BTC": 10.5, "USDT": 1_000.0}}
    futures = FakeFutures()
    futures.positions = [{"symbol": "BTC/USDT", "contracts": -7.0}]  # drift > 1%
    executor = _exec(spot=spot, futures=futures)
    position = FundingArbPosition(
        symbol="BTC/USDT", open_time_ms=0, entry_funding_bps=5.0,
        notional_usdt=2_000.0, spot_qty=10.0, perp_qty=-10.0,
    )
    report = await executor.reconcile({position.symbol: position})
    assert not report.ok
    assert any("futures position" in m.detail for m in report.mismatches)


# ---------------------------------------------------------------------------
# Live-mode integration through run_cycle
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_cycle_live_halts_on_reconciliation_mismatch(
    tmp_path,
    monkeypatch,
) -> None:
    import src.funding_arb as fa
    from src.funding_arb import (
        CandidateSignal,
        FundingArbError,
        FundingArbPosition,
        FundingArbSettings,
        FundingArbState,
        run_cycle,
        save_state,
    )

    async def _no_universe(_settings):
        return ["BTC/USDT"]

    async def _no_signals(_symbols):
        return [CandidateSignal(symbol="BTC/USDT", funding_bps=2.0, mark_price=200, spot_price=200)]

    monkeypatch.setattr(fa, "_resolve_universe", _no_universe)
    monkeypatch.setattr(fa, "_fetch_current_signals", _no_signals)
    monkeypatch.setenv("FUNDING_ARB_PROVEN_EDGE", "yes")

    settings = FundingArbSettings(
        api_key="k", api_secret="s", use_testnet=False, dry_run=False,
        enabled=True, confirm_live_funding_arb="yes",
        dynamic_balance=False,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )
    save_state(
        FundingArbState(
            positions=(FundingArbPosition(
                symbol="BTC/USDT", open_time_ms=0, entry_funding_bps=5.0,
                notional_usdt=2_000.0, spot_qty=10.0, perp_qty=-10.0,
            ),)
        ),
        settings.state_path,
    )

    spot = FakeSpot()
    spot.balance = {"free": {"BTC": 0.0, "USDT": 1_000.0}}  # spot missing
    futures = FakeFutures()
    futures.positions = []
    executor = _exec(spot=spot, futures=futures)

    with pytest.raises(FundingArbError, match="reconciliation mismatch"):
        await run_cycle(settings, mode="live", executor=executor)


@pytest.mark.asyncio
async def test_run_cycle_live_opens_position_when_signal_passes(
    tmp_path,
    monkeypatch,
) -> None:
    import src.funding_arb as fa
    from src.funding_arb import (
        CandidateSignal,
        FundingArbSettings,
        load_state,
        run_cycle,
    )

    async def _universe(_settings):
        return ["BTC/USDT"]

    async def _signals(_symbols):
        return [CandidateSignal(symbol="BTC/USDT", funding_bps=5.0, mark_price=200, spot_price=200)]

    monkeypatch.setattr(fa, "_resolve_universe", _universe)
    monkeypatch.setattr(fa, "_fetch_current_signals", _signals)
    monkeypatch.setenv("FUNDING_ARB_PROVEN_EDGE", "yes")

    settings = FundingArbSettings(
        api_key="k", api_secret="s", use_testnet=False, dry_run=False,
        enabled=True, confirm_live_funding_arb="yes",
        entry_funding_bps=3.0, exit_funding_bps=-1.0,
        per_position_notional_pct=20.0, starting_balance_usdt=10_000.0,
        dynamic_balance=False,  # pin to configured balance for predictable sizing
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )

    spot = FakeSpot(last=200.0)
    futures = FakeFutures(last=200.0)
    executor = _exec(spot=spot, futures=futures)

    result = await run_cycle(settings, mode="live", executor=executor)
    assert any(r["action"] == "open" and r["ok"] for r in result["live_results"])
    state = load_state(settings.state_path)
    assert len(state.positions) == 1
    assert state.positions[0].symbol == "BTC/USDT"
    assert state.positions[0].spot_qty == pytest.approx(10.0)


@pytest.mark.asyncio
async def test_run_cycle_live_closes_existing_position_on_exit_signal(
    tmp_path,
    monkeypatch,
) -> None:
    import src.funding_arb as fa
    from src.funding_arb import (
        CandidateSignal,
        FundingArbPosition,
        FundingArbSettings,
        FundingArbState,
        load_state,
        run_cycle,
        save_state,
    )

    async def _universe(_settings):
        return ["BTC/USDT"]

    async def _signals(_symbols):
        return [CandidateSignal(symbol="BTC/USDT", funding_bps=-2.0, mark_price=200, spot_price=200)]

    monkeypatch.setattr(fa, "_resolve_universe", _universe)
    monkeypatch.setattr(fa, "_fetch_current_signals", _signals)
    monkeypatch.setenv("FUNDING_ARB_PROVEN_EDGE", "yes")

    settings = FundingArbSettings(
        api_key="k", api_secret="s", use_testnet=False, dry_run=False,
        enabled=True, confirm_live_funding_arb="yes",
        entry_funding_bps=3.0, exit_funding_bps=-1.0,
        dynamic_balance=False,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )
    save_state(
        FundingArbState(
            positions=(FundingArbPosition(
                symbol="BTC/USDT", open_time_ms=0, entry_funding_bps=5.0,
                notional_usdt=2_000.0, spot_qty=10.0, perp_qty=-10.0,
                funding_received_usdt=4.0,
            ),)
        ),
        settings.state_path,
    )

    spot = FakeSpot(last=200.0)
    spot.balance = {"free": {"BTC": 10.5, "USDT": 1_000.0}}
    futures = FakeFutures(last=200.0)
    futures.positions = [{"symbol": "BTC/USDT", "contracts": -10.0}]
    executor = _exec(spot=spot, futures=futures)

    result = await run_cycle(settings, mode="live", executor=executor)
    assert any(r["action"] == "close" and r["ok"] for r in result["live_results"])
    state = load_state(settings.state_path)
    assert state.positions == ()
    # Ledger row should have been written for the closed trade.
    import json
    lines = (tmp_path / "ledger.jsonl").read_text().splitlines()
    assert len(lines) == 1
    decoded = json.loads(lines[0])
    assert decoded["symbol"] == "BTC/USDT"


# ---------------------------------------------------------------------------
# Dynamic balance feature
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_dynamic_balance_uses_live_wallet_for_sizing(
    tmp_path,
    monkeypatch,
) -> None:
    import src.funding_arb as fa
    from src.funding_arb import (
        CandidateSignal,
        FundingArbSettings,
        load_state,
        run_cycle,
    )

    async def _universe(_settings):
        return ["BTC/USDT"]

    async def _signals(_symbols):
        return [CandidateSignal(symbol="BTC/USDT", funding_bps=5.0, mark_price=200, spot_price=200)]

    monkeypatch.setattr(fa, "_resolve_universe", _universe)
    monkeypatch.setattr(fa, "_fetch_current_signals", _signals)
    monkeypatch.setenv("FUNDING_ARB_PROVEN_EDGE", "yes")

    settings = FundingArbSettings(
        api_key="k", api_secret="s", use_testnet=False, dry_run=False,
        enabled=True, confirm_live_funding_arb="yes",
        entry_funding_bps=3.0, exit_funding_bps=-1.0,
        per_position_notional_pct=20.0,
        starting_balance_usdt=10_000.0,  # configured but should be overridden
        dynamic_balance=True,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )

    spot = FakeSpot(last=200.0)
    spot.balance = {"free": {"BTC": 0.0, "USDT": 600.0}}  # actual wallet
    futures = FakeFutures(last=200.0)
    futures.balance = {"free": {"USDT": 400.0}}  # actual wallet
    executor = _exec(spot=spot, futures=futures)

    result = await run_cycle(settings, mode="live", executor=executor)
    # Dynamic balance = 1000 USDT. 20% per position = 200 USDT notional.
    # At $200/BTC → 1 BTC.
    state = load_state(settings.state_path)
    assert len(state.positions) == 1
    assert state.positions[0].spot_qty == pytest.approx(1.0)
    assert result["balance_source"] == "live"
    assert result["effective_balance_usdt"] == pytest.approx(1_000.0)
    assert result["configured_balance_usdt"] == pytest.approx(10_000.0)


@pytest.mark.asyncio
async def test_dynamic_balance_falls_back_when_fetch_fails(
    tmp_path,
    monkeypatch,
) -> None:
    import src.funding_arb as fa
    from src.funding_arb import (
        CandidateSignal,
        FundingArbSettings,
        run_cycle,
    )

    async def _universe(_settings):
        return ["BTC/USDT"]

    async def _signals(_symbols):
        return [CandidateSignal(symbol="BTC/USDT", funding_bps=5.0, mark_price=200, spot_price=200)]

    monkeypatch.setattr(fa, "_resolve_universe", _universe)
    monkeypatch.setattr(fa, "_fetch_current_signals", _signals)
    monkeypatch.setenv("FUNDING_ARB_PROVEN_EDGE", "yes")

    settings = FundingArbSettings(
        api_key="k", api_secret="s", use_testnet=False, dry_run=False,
        enabled=True, confirm_live_funding_arb="yes",
        entry_funding_bps=3.0, exit_funding_bps=-1.0,
        per_position_notional_pct=20.0, starting_balance_usdt=5_000.0,
        dynamic_balance=True,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )

    class FlakeySpot(FakeSpot):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.fetch_balance_calls = 0

        async def fetch_balance(self):
            self.fetch_balance_calls += 1
            if self.fetch_balance_calls == 1:
                raise RuntimeError("network blip")  # fail dynamic-balance fetch only
            return self.balance

    spot = FlakeySpot(last=200.0)
    futures = FakeFutures(last=200.0)
    futures.balance = {"free": {"USDT": 100.0}}
    executor = _exec(spot=spot, futures=futures)

    result = await run_cycle(settings, mode="live", executor=executor)
    assert result["balance_source"] == "fallback_configured"
    assert result["effective_balance_usdt"] == pytest.approx(5_000.0)


@pytest.mark.asyncio
async def test_low_balance_blocks_new_opens_but_allows_closes(
    tmp_path,
    monkeypatch,
) -> None:
    import src.funding_arb as fa
    from src.funding_arb import (
        CandidateSignal,
        FundingArbPosition,
        FundingArbSettings,
        FundingArbState,
        load_state,
        run_cycle,
        save_state,
    )

    async def _universe(_settings):
        return ["SOL/USDT", "BTC/USDT"]

    async def _signals(_symbols):
        return [
            CandidateSignal(symbol="SOL/USDT", funding_bps=8.0, mark_price=100, spot_price=100),
            CandidateSignal(symbol="BTC/USDT", funding_bps=-3.0, mark_price=200, spot_price=200),
        ]

    monkeypatch.setattr(fa, "_resolve_universe", _universe)
    monkeypatch.setattr(fa, "_fetch_current_signals", _signals)
    monkeypatch.setenv("FUNDING_ARB_PROVEN_EDGE", "yes")

    settings = FundingArbSettings(
        api_key="k", api_secret="s", use_testnet=False, dry_run=False,
        enabled=True, confirm_live_funding_arb="yes",
        entry_funding_bps=3.0, exit_funding_bps=-1.0,
        starting_balance_usdt=9.0, dynamic_balance=False,
        min_balance_usdt=50.0,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )
    # Pre-seed one held position so we can verify it gets closed even at low balance.
    save_state(
        FundingArbState(
            positions=(FundingArbPosition(
                symbol="BTC/USDT", open_time_ms=0, entry_funding_bps=5.0,
                notional_usdt=2_000.0, spot_qty=10.0, perp_qty=-10.0,
            ),)
        ),
        settings.state_path,
    )

    spot = FakeSpot(last=200.0)
    spot.balance = {"free": {"BTC": 10.5, "USDT": 9.0}}
    futures = FakeFutures(last=200.0)
    futures.positions = [{"symbol": "BTC/USDT", "contracts": -10.0}]
    executor = _exec(spot=spot, futures=futures)

    result = await run_cycle(settings, mode="live", executor=executor)
    actions = [d["action"] for d in result["decisions"]]
    assert "close" in actions
    assert "open" not in actions
    assert result["insufficient_balance"] is True
    state = load_state(settings.state_path)
    assert state.positions == ()  # the held BTC was closed

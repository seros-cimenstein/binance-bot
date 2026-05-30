"""Tests for the funding-arb runtime decision engine + safety gates."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.funding_arb import (
    CandidateSignal,
    FundingArbAdvisor,
    FundingArbDecision,
    FundingArbError,
    FundingArbPosition,
    FundingArbSettings,
    FundingArbState,
    _preflight,
    load_state,
    save_state,
)


def _settings(tmp_path: Path, **overrides) -> FundingArbSettings:
    values = dict(
        api_key="k",
        api_secret="s",
        use_testnet=False,
        dry_run=False,
        enabled=True,
        confirm_live_funding_arb="yes",
        entry_funding_bps=3.0,
        exit_funding_bps=-1.0,
        max_hold_funding_events=30,
        max_concurrent_positions=3,
        per_position_notional_pct=20.0,
        starting_balance_usdt=10_000.0,
        basis_circuit_breaker_bps=200.0,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )
    values.update(overrides)
    settings = FundingArbSettings(**values)
    settings.validate()
    return settings


def _signal(symbol: str, funding_bps: float, *, spot: float = 100.0, mark: float | None = None) -> CandidateSignal:
    return CandidateSignal(
        symbol=symbol,
        funding_bps=funding_bps,
        mark_price=mark if mark is not None else spot,
        spot_price=spot,
    )


def _position(symbol: str, *, entry_bps: float = 5.0, events_held: int = 0) -> FundingArbPosition:
    return FundingArbPosition(
        symbol=symbol,
        open_time_ms=0,
        entry_funding_bps=entry_bps,
        notional_usdt=2000.0,
        spot_qty=20.0,
        perp_qty=-20.0,
        events_held=events_held,
    )


def test_advisor_holds_when_no_signals_clear_entry(tmp_path: Path) -> None:
    advisor = FundingArbAdvisor(_settings(tmp_path))
    signals = [_signal("FOO/USDT", 1.0), _signal("BAR/USDT", 2.0)]
    decisions = advisor.decide(state=FundingArbState.empty(), signals=signals)
    assert decisions == []  # no holds for non-held symbols


def test_advisor_opens_when_signal_clears_entry(tmp_path: Path) -> None:
    advisor = FundingArbAdvisor(_settings(tmp_path))
    signals = [_signal("BTC/USDT", 5.0), _signal("ETH/USDT", 4.0)]
    decisions = advisor.decide(state=FundingArbState.empty(), signals=signals)
    actions = sorted((d.action, d.symbol) for d in decisions)
    assert actions == [("open", "BTC/USDT"), ("open", "ETH/USDT")]
    assert all(d.notional_usdt == pytest.approx(2_000.0) for d in decisions)


def test_advisor_respects_max_concurrent(tmp_path: Path) -> None:
    settings = _settings(tmp_path, max_concurrent_positions=2)
    advisor = FundingArbAdvisor(settings)
    signals = [
        _signal("A/USDT", 10.0),
        _signal("B/USDT", 9.0),
        _signal("C/USDT", 8.0),
        _signal("D/USDT", 7.0),
    ]
    decisions = advisor.decide(state=FundingArbState.empty(), signals=signals)
    open_symbols = [d.symbol for d in decisions if d.action == "open"]
    assert open_symbols == ["A/USDT", "B/USDT"]


def test_advisor_holds_existing_position_when_funding_still_above_exit(tmp_path: Path) -> None:
    advisor = FundingArbAdvisor(_settings(tmp_path))
    state = FundingArbState(positions=(_position("BTC/USDT", events_held=5),))
    signals = [_signal("BTC/USDT", 2.0)]
    decisions = advisor.decide(state=state, signals=signals)
    assert len(decisions) == 1
    assert decisions[0].action == "hold"
    assert decisions[0].symbol == "BTC/USDT"


def test_advisor_closes_when_funding_drops_to_exit_threshold(tmp_path: Path) -> None:
    advisor = FundingArbAdvisor(_settings(tmp_path))
    state = FundingArbState(positions=(_position("BTC/USDT"),))
    signals = [_signal("BTC/USDT", -2.0)]
    decisions = advisor.decide(state=state, signals=signals)
    assert any(d.action == "close" and d.symbol == "BTC/USDT" for d in decisions)


def test_advisor_closes_when_max_hold_reached(tmp_path: Path) -> None:
    settings = _settings(tmp_path, max_hold_funding_events=10)
    advisor = FundingArbAdvisor(settings)
    state = FundingArbState(positions=(_position("BTC/USDT", events_held=10),))
    signals = [_signal("BTC/USDT", 5.0)]
    decisions = advisor.decide(state=state, signals=signals)
    close = next(d for d in decisions if d.symbol == "BTC/USDT")
    assert close.action == "close"
    assert "max hold" in close.reason


def test_advisor_closes_when_basis_blows_out(tmp_path: Path) -> None:
    settings = _settings(tmp_path, basis_circuit_breaker_bps=100.0)
    advisor = FundingArbAdvisor(settings)
    state = FundingArbState(positions=(_position("BTC/USDT"),))
    # mark 102 vs spot 100 = 200 bps basis, above the 100 bps breaker.
    signals = [_signal("BTC/USDT", 5.0, spot=100.0, mark=102.0)]
    decisions = advisor.decide(state=state, signals=signals)
    close = next(d for d in decisions if d.symbol == "BTC/USDT")
    assert close.action == "close"
    assert "basis circuit breaker" in close.reason


def test_advisor_skips_opening_on_blown_basis(tmp_path: Path) -> None:
    settings = _settings(tmp_path, basis_circuit_breaker_bps=100.0)
    advisor = FundingArbAdvisor(settings)
    signals = [_signal("BTC/USDT", 5.0, spot=100.0, mark=102.0)]
    decisions = advisor.decide(state=FundingArbState.empty(), signals=signals)
    assert decisions == []


def test_advisor_pending_close_frees_capacity_for_new_open(tmp_path: Path) -> None:
    settings = _settings(tmp_path, max_concurrent_positions=1)
    advisor = FundingArbAdvisor(settings)
    state = FundingArbState(positions=(_position("BTC/USDT"),))
    signals = [
        _signal("BTC/USDT", -5.0),  # forces close
        _signal("SOL/USDT", 8.0),  # candidate to open with freed capacity
    ]
    decisions = advisor.decide(state=state, signals=signals)
    actions = {(d.action, d.symbol) for d in decisions}
    assert ("close", "BTC/USDT") in actions
    assert ("open", "SOL/USDT") in actions


def test_settings_validation_rejects_inverted_thresholds(tmp_path: Path) -> None:
    with pytest.raises(FundingArbError, match="entry_funding_bps must be strictly above"):
        _settings(tmp_path, entry_funding_bps=1.0, exit_funding_bps=2.0)


def test_settings_validation_rejects_bad_notional_pct(tmp_path: Path) -> None:
    with pytest.raises(FundingArbError, match="per_position_notional_pct"):
        _settings(tmp_path, per_position_notional_pct=150.0)


def test_preflight_blocks_when_emergency_stop_present(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    Path(settings.emergency_stop_path).write_text("halt")
    with pytest.raises(FundingArbError, match="emergency stop"):
        _preflight(settings, submit_order=False)


def test_preflight_passes_in_plan_only_without_proven_edge(tmp_path: Path) -> None:
    # Even with all live gates set, plan-only must not require the PROVEN_EDGE gate.
    settings = _settings(tmp_path)
    _preflight(settings, submit_order=False)  # no raise


def test_preflight_requires_proven_edge_for_live(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    settings = _settings(tmp_path)
    monkeypatch.delenv("FUNDING_ARB_PROVEN_EDGE", raising=False)
    with pytest.raises(FundingArbError, match="FUNDING_ARB_PROVEN_EDGE"):
        _preflight(settings, submit_order=True)


def test_preflight_passes_live_when_all_gates_set(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    settings = _settings(tmp_path)
    monkeypatch.setenv("FUNDING_ARB_PROVEN_EDGE", "yes")
    _preflight(settings, submit_order=True)  # no raise


def test_state_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    state = FundingArbState(
        positions=(
            _position("BTC/USDT", entry_bps=5.0, events_held=3),
            _position("SOL/USDT", entry_bps=8.0, events_held=1),
        )
    )
    save_state(state, path)
    restored = load_state(path)
    assert len(restored.positions) == 2
    by_symbol = {p.symbol: p for p in restored.positions}
    assert by_symbol["BTC/USDT"].entry_funding_bps == pytest.approx(5.0)
    assert by_symbol["SOL/USDT"].events_held == 1


def test_state_load_missing_file_returns_empty(tmp_path: Path) -> None:
    state = load_state(tmp_path / "nonexistent.json")
    assert state.positions == ()

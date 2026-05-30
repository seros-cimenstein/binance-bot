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
    PaperLedgerEntry,
    _preflight,
    append_ledger,
    apply_paper_decisions,
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
        ledger_path=str(tmp_path / "ledger.jsonl"),
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


# ---------------------------------------------------------------------------
# Paper execution layer
# ---------------------------------------------------------------------------


def test_apply_paper_open_creates_position_with_correct_qty(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    signal = _signal("BTC/USDT", 5.0, spot=200.0)
    decisions = [
        FundingArbDecision(
            action="open",
            symbol="BTC/USDT",
            reason="entry",
            current_funding_bps=5.0,
            notional_usdt=2_000.0,
        )
    ]
    state, ledger = apply_paper_decisions(
        settings=settings,
        state=FundingArbState.empty(),
        signals=[signal],
        decisions=decisions,
        cycle_ts_ms=1_700_000_000_000,
    )
    assert len(state.positions) == 1
    p = state.positions[0]
    assert p.symbol == "BTC/USDT"
    assert p.notional_usdt == pytest.approx(2_000.0)
    assert p.spot_qty == pytest.approx(10.0)
    assert p.perp_qty == pytest.approx(-10.0)
    assert p.events_held == 0  # no funding event applied yet for this fresh open
    assert ledger == []


def test_apply_paper_accrues_funding_on_held_position(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    position = FundingArbPosition(
        symbol="BTC/USDT",
        open_time_ms=0,
        entry_funding_bps=5.0,
        notional_usdt=2_000.0,
        spot_qty=10.0,
        perp_qty=-10.0,
    )
    signal = _signal("BTC/USDT", 5.0, spot=200.0)
    state, ledger = apply_paper_decisions(
        settings=settings,
        state=FundingArbState(positions=(position,)),
        signals=[signal],
        decisions=[
            FundingArbDecision(action="hold", symbol="BTC/USDT", reason="ok", current_funding_bps=5.0)
        ],
        cycle_ts_ms=1_700_000_000_000,
    )
    assert ledger == []
    held = state.positions[0]
    assert held.events_held == 1
    assert held.funding_received_bps == pytest.approx(5.0)
    assert held.funding_received_usdt == pytest.approx(2_000.0 * 0.0005)


def test_apply_paper_close_writes_ledger_entry_with_net_pnl(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    position = FundingArbPosition(
        symbol="BTC/USDT",
        open_time_ms=0,
        entry_funding_bps=5.0,
        notional_usdt=2_000.0,
        spot_qty=10.0,
        perp_qty=-10.0,
        funding_received_bps=80.0,
        funding_received_usdt=2_000.0 * 0.008,
        events_held=16,
    )
    # On close cycle, current funding is below exit. Close decision present.
    # The held position accrues one more event of funding at the exit rate.
    exit_rate_bps = -1.5
    signal = _signal("BTC/USDT", exit_rate_bps, spot=200.0)
    decisions = [
        FundingArbDecision(
            action="close", symbol="BTC/USDT", reason="exit", current_funding_bps=exit_rate_bps
        )
    ]
    state, ledger = apply_paper_decisions(
        settings=settings,
        state=FundingArbState(positions=(position,)),
        signals=[signal],
        decisions=decisions,
        cycle_ts_ms=1_700_000_001_000,
    )
    assert state.positions == ()
    assert len(ledger) == 1
    entry = ledger[0]
    assert entry.symbol == "BTC/USDT"
    expected_funding_bps = 80.0 + exit_rate_bps  # one more accrual on close cycle
    assert entry.funding_received_bps == pytest.approx(expected_funding_bps)
    # round-trip cost at defaults = 2*(10+5)+4*5 = 50 bps
    assert entry.round_trip_cost_bps == pytest.approx(50.0)
    assert entry.net_pnl_bps == pytest.approx(expected_funding_bps - 50.0)
    assert entry.net_pnl_usdt == pytest.approx(
        entry.funding_received_usdt - entry.round_trip_cost_usdt
    )


def test_apply_paper_open_skipped_when_spot_price_missing(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    signal = CandidateSignal(
        symbol="BTC/USDT", funding_bps=5.0, mark_price=200.0, spot_price=0.0
    )
    decisions = [
        FundingArbDecision(
            action="open",
            symbol="BTC/USDT",
            reason="entry",
            current_funding_bps=5.0,
            notional_usdt=2_000.0,
        )
    ]
    state, ledger = apply_paper_decisions(
        settings=settings,
        state=FundingArbState.empty(),
        signals=[signal],
        decisions=decisions,
        cycle_ts_ms=0,
    )
    assert state.positions == ()
    assert ledger == []


def test_append_ledger_writes_jsonl_lines(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    entry = PaperLedgerEntry(
        symbol="BTC/USDT",
        open_time_ms=1,
        close_time_ms=2,
        entry_funding_bps=5.0,
        funding_received_bps=80.0,
        funding_received_usdt=16.0,
        events_held=16,
        round_trip_cost_bps=50.0,
        round_trip_cost_usdt=10.0,
        net_pnl_bps=30.0,
        net_pnl_usdt=6.0,
    )
    append_ledger(settings.ledger_path, [entry])
    append_ledger(settings.ledger_path, [entry])
    lines = Path(settings.ledger_path).read_text().splitlines()
    assert len(lines) == 2
    decoded = json.loads(lines[0])
    assert decoded["symbol"] == "BTC/USDT"
    assert decoded["net_pnl_bps"] == 30.0

"""Tests for the mean-reversion live runtime."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.mean_reversion import (
    MeanReversionDecision,
    MeanReversionError,
    MeanReversionLedgerEntry,
    MeanReversionPosition,
    MeanReversionSettings,
    MeanReversionState,
    SymbolSignal,
    _preflight,
    apply_paper_decisions,
    compute_decisions,
    load_state,
    save_state,
)


def _settings(tmp_path: Path, **overrides) -> MeanReversionSettings:
    values = dict(
        api_key="k", api_secret="s",
        use_testnet=False, dry_run=False,
        enabled=True, confirm_live_mean_rev="yes",
        universe=("A/USDT", "B/USDT", "C/USDT"),
        dump_lookback_bars=15, dump_threshold_pct=-5.0,
        retrace_target_pct=0.5, stop_loss_pct=-3.0, max_hold_bars=60,
        taker_fee_bps=10.0, slippage_bps=5.0,
        notional_per_position_usdt=50.0,
        max_concurrent_positions=3, min_balance_usdt=50.0,
        cycle_interval_seconds=60.0,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )
    values.update(overrides)
    settings = MeanReversionSettings(**values)
    settings.validate()
    return settings


def _signal(
    symbol: str, *, current: float = 100.0, bar_low: float | None = None,
    bar_high: float | None = None, lookback_pct: float = 0.0,
) -> SymbolSignal:
    return SymbolSignal(
        symbol=symbol,
        current_price=current,
        bar_low=bar_low if bar_low is not None else current,
        bar_high=bar_high if bar_high is not None else current,
        lookback_return_pct=lookback_pct,
    )


def _held(symbol: str = "BTC/USDT", *, entry: float = 100.0, dump: float = -6.0,
          bars: int = 0, qty: float | None = None) -> MeanReversionPosition:
    notional = 50.0
    q = qty if qty is not None else notional / entry
    return MeanReversionPosition(
        symbol=symbol, open_time_ms=0,
        entry_price=entry, qty=q, notional_usdt=notional,
        dump_pct_at_entry=dump, bars_held=bars,
    )


# ---------------------------------------------------------------------------
# Settings validation
# ---------------------------------------------------------------------------


def test_settings_rejects_empty_universe(tmp_path: Path) -> None:
    with pytest.raises(MeanReversionError, match="universe"):
        _settings(tmp_path, universe=())


def test_settings_rejects_positive_dump_threshold(tmp_path: Path) -> None:
    with pytest.raises(MeanReversionError, match="dump_threshold_pct"):
        _settings(tmp_path, dump_threshold_pct=5.0)


def test_settings_rejects_zero_max_hold(tmp_path: Path) -> None:
    with pytest.raises(MeanReversionError, match="max_hold_bars"):
        _settings(tmp_path, max_hold_bars=0)


# ---------------------------------------------------------------------------
# Preflight gate ladder
# ---------------------------------------------------------------------------


def test_preflight_passes_in_plan_only(tmp_path: Path) -> None:
    _preflight(_settings(tmp_path, enabled=False), submit_order=False)


def test_preflight_blocks_when_emergency_stop_present(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    Path(s.emergency_stop_path).write_text("halt")
    with pytest.raises(MeanReversionError, match="emergency stop"):
        _preflight(s, submit_order=False)


def test_preflight_requires_proven_edge_for_live(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    s = _settings(tmp_path)
    monkeypatch.delenv("MEAN_REV_PROVEN_EDGE", raising=False)
    with pytest.raises(MeanReversionError, match="MEAN_REV_PROVEN_EDGE"):
        _preflight(s, submit_order=True)


def test_preflight_passes_live_when_all_gates_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    s = _settings(tmp_path)
    monkeypatch.setenv("MEAN_REV_PROVEN_EDGE", "yes")
    _preflight(s, submit_order=True)


# ---------------------------------------------------------------------------
# Decision rule
# ---------------------------------------------------------------------------


def test_compute_opens_on_dump_below_threshold(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    signals = [_signal("A/USDT", lookback_pct=-6.0, current=94.0)]
    decisions = compute_decisions(s, MeanReversionState.empty(), signals)
    actions = {(d.action, d.symbol) for d in decisions}
    assert ("open", "A/USDT") in actions
    open_d = next(d for d in decisions if d.action == "open")
    assert open_d.qty == pytest.approx(50.0 / 94.0)
    assert open_d.entry_price == pytest.approx(94.0)


def test_compute_skips_open_when_dump_above_threshold(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    signals = [_signal("A/USDT", lookback_pct=-3.0)]
    decisions = compute_decisions(s, MeanReversionState.empty(), signals)
    assert not any(d.action == "open" for d in decisions)


def test_compute_take_profit_when_bar_crosses_target(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    held = _held(symbol="A/USDT", entry=94.0, dump=-6.0, bars=2)
    state = MeanReversionState(positions=(held,))
    # Target = 94 * (1 + 0.5 * 6 / 100) = 94 * 1.03 = 96.82
    signals = [_signal("A/USDT", current=96.5, bar_low=94.0, bar_high=97.0,
                       lookback_pct=-2.0)]
    decisions = compute_decisions(s, state, signals)
    close = next(d for d in decisions if d.symbol == "A/USDT")
    assert close.action == "close"
    assert "take profit" in close.reason
    assert close.exit_price == pytest.approx(96.82, abs=0.01)


def test_compute_stop_loss_when_bar_crosses_stop(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    held = _held(symbol="A/USDT", entry=94.0, dump=-6.0, bars=2)
    # Stop = 94 * (1 - 0.03) = 91.18
    signals = [_signal("A/USDT", current=92.0, bar_low=90.0, bar_high=95.0,
                       lookback_pct=-2.0)]
    decisions = compute_decisions(s, MeanReversionState(positions=(held,)), signals)
    close = next(d for d in decisions if d.symbol == "A/USDT")
    assert close.action == "close"
    assert "stop loss" in close.reason
    assert close.exit_price == pytest.approx(91.18, abs=0.01)


def test_compute_max_hold_exit_when_time_runs_out(tmp_path: Path) -> None:
    s = _settings(tmp_path, max_hold_bars=10)
    held = _held(symbol="A/USDT", entry=94.0, dump=-6.0, bars=10)
    # Neither target nor stop crossed.
    signals = [_signal("A/USDT", current=94.5, bar_low=93.0, bar_high=95.0,
                       lookback_pct=-2.0)]
    decisions = compute_decisions(s, MeanReversionState(positions=(held,)), signals)
    close = next(d for d in decisions if d.symbol == "A/USDT")
    assert close.action == "close"
    assert "max hold" in close.reason


def test_compute_holds_when_neither_target_nor_stop_crossed(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    held = _held(symbol="A/USDT", entry=94.0, dump=-6.0, bars=2)
    signals = [_signal("A/USDT", current=94.5, bar_low=93.0, bar_high=95.0,
                       lookback_pct=-2.0)]
    decisions = compute_decisions(s, MeanReversionState(positions=(held,)), signals)
    dec = next(d for d in decisions if d.symbol == "A/USDT")
    assert dec.action == "hold"


def test_compute_respects_max_concurrent_positions(tmp_path: Path) -> None:
    s = _settings(tmp_path, max_concurrent_positions=1)
    signals = [
        _signal("A/USDT", lookback_pct=-7.0, current=93.0),
        _signal("B/USDT", lookback_pct=-6.0, current=94.0),
    ]
    decisions = compute_decisions(s, MeanReversionState.empty(), signals)
    opens = [d for d in decisions if d.action == "open"]
    assert len(opens) == 1
    # Deepest dump wins.
    assert opens[0].symbol == "A/USDT"


def test_compute_pending_close_frees_capacity_for_open(tmp_path: Path) -> None:
    s = _settings(tmp_path, max_concurrent_positions=1)
    held = _held(symbol="A/USDT", entry=94.0, dump=-6.0, bars=20)
    state = MeanReversionState(positions=(held,))
    # A/USDT bar crosses target → close. B/USDT has a fresh dump.
    signals = [
        _signal("A/USDT", current=97.0, bar_low=94.0, bar_high=98.0,
                lookback_pct=-1.0),
        _signal("B/USDT", current=90.0, bar_low=89.0, bar_high=92.0,
                lookback_pct=-10.0),
    ]
    decisions = compute_decisions(s, state, signals)
    actions = {(d.action, d.symbol) for d in decisions}
    assert ("close", "A/USDT") in actions
    assert ("open", "B/USDT") in actions


# ---------------------------------------------------------------------------
# Paper apply
# ---------------------------------------------------------------------------


def test_apply_paper_open_creates_position_and_ledger_entry(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    signals = [_signal("A/USDT", lookback_pct=-6.0, current=94.0)]
    decisions = compute_decisions(s, MeanReversionState.empty(), signals)
    state, ledger = apply_paper_decisions(
        settings=s, state=MeanReversionState.empty(),
        signals=signals, decisions=decisions, cycle_ts_ms=1_000_000,
    )
    assert len(state.positions) == 1
    p = state.positions[0]
    assert p.symbol == "A/USDT"
    assert p.notional_usdt == pytest.approx(50.0)
    assert p.bars_held == 0
    # Single ledger entry: open.
    assert len(ledger) == 1
    assert ledger[0].action == "open"
    # Open-leg "P&L" equals -entry fee.
    expected_fee = 50.0 * (10.0 + 5.0) / 10_000.0
    assert ledger[0].fee_usdt == pytest.approx(expected_fee)
    assert ledger[0].pnl_usdt == pytest.approx(-expected_fee)


def test_apply_paper_close_records_realised_pnl(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    qty = 50.0 / 94.0
    held = MeanReversionPosition(
        symbol="A/USDT", open_time_ms=0,
        entry_price=94.0, qty=qty, notional_usdt=50.0,
        dump_pct_at_entry=-6.0, bars_held=2,
    )
    state = MeanReversionState(positions=(held,))
    signals = [_signal("A/USDT", current=97.0, bar_low=94.0, bar_high=98.0,
                       lookback_pct=-2.0)]
    decisions = compute_decisions(s, state, signals)
    new_state, ledger = apply_paper_decisions(
        settings=s, state=state, signals=signals,
        decisions=decisions, cycle_ts_ms=1_000_001,
    )
    assert new_state.positions == ()  # closed out
    assert len(ledger) == 1 and ledger[0].action == "close"
    # exit at 96.82 (take-profit price); proceeds = qty * 96.82 * (1 - 15bps).
    # pnl = proceeds - 50.0
    proceeds = qty * 96.82 * (1.0 - 15.0 / 10_000.0)
    expected_pnl = proceeds - 50.0
    assert ledger[0].pnl_usdt == pytest.approx(expected_pnl, abs=0.01)


def test_apply_paper_increments_bars_held_on_carried_position(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    held = _held(symbol="A/USDT", entry=94.0, dump=-6.0, bars=2)
    state = MeanReversionState(positions=(held,))
    # Inside corridor → hold.
    signals = [_signal("A/USDT", current=94.5, bar_low=93.0, bar_high=95.0,
                       lookback_pct=-2.0)]
    decisions = compute_decisions(s, state, signals)
    new_state, ledger = apply_paper_decisions(
        settings=s, state=state, signals=signals,
        decisions=decisions, cycle_ts_ms=2_000_000,
    )
    assert len(new_state.positions) == 1
    assert new_state.positions[0].bars_held == 3  # 2 -> 3
    assert ledger == []  # no fills this cycle


# ---------------------------------------------------------------------------
# State round-trip
# ---------------------------------------------------------------------------


def test_state_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    state = MeanReversionState(positions=(_held(symbol="A/USDT"),))
    save_state(state, path)
    restored = load_state(path)
    assert len(restored.positions) == 1
    assert restored.positions[0].symbol == "A/USDT"


def test_state_load_missing_file_returns_empty(tmp_path: Path) -> None:
    state = load_state(tmp_path / "missing.json")
    assert state.positions == ()


def test_state_load_corrupt_file_raises(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    path.write_text("not valid json")
    with pytest.raises(MeanReversionError, match="could not read"):
        load_state(path)

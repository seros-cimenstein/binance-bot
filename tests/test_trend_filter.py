"""Tests for the trend filter runtime."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.trend_filter import (
    TrendFilterAdvisor,
    TrendFilterError,
    TrendFilterHolding,
    TrendFilterSettings,
    TrendFilterSignal,
    TrendFilterState,
    _preflight,
    load_state,
    save_state,
)


def _settings(tmp_path: Path, **overrides) -> TrendFilterSettings:
    values = dict(
        api_key="k", api_secret="s", use_testnet=False, dry_run=False,
        enabled=True, confirm_live_trend_filter="yes",
        universe=("A/USDT", "B/USDT", "C/USDT"),
        lookback_days=30, top_k=1, rebalance_every_days=30,
        min_balance_usdt=5.0, min_order_notional_usdt=5.0,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )
    values.update(overrides)
    settings = TrendFilterSettings(**values)
    settings.validate()
    return settings


def _signal(symbol: str, lookback_return_pct: float, *, price: float = 100.0) -> TrendFilterSignal:
    return TrendFilterSignal(symbol=symbol, current_price=price,
                              lookback_return_pct=lookback_return_pct)


def test_advisor_buys_top_k_when_no_holdings(tmp_path: Path) -> None:
    advisor = TrendFilterAdvisor(_settings(tmp_path, top_k=1))
    signals = [_signal("A/USDT", 10), _signal("B/USDT", 50), _signal("C/USDT", -5)]
    decisions = advisor.decide(
        state=TrendFilterState.empty(), signals=signals,
        balance_usdt=10.0, current_time_ms=1_000_000_000,
    )
    actions = [(d.action, d.symbol) for d in decisions]
    assert ("buy", "B/USDT") in actions  # highest lookback wins
    assert not any(d.action == "buy" and d.symbol != "B/USDT" for d in decisions)


def test_advisor_holds_when_current_still_in_top_k(tmp_path: Path) -> None:
    advisor = TrendFilterAdvisor(_settings(tmp_path, top_k=1))
    state = TrendFilterState(
        holdings=(TrendFilterHolding(symbol="B/USDT", qty=0.1,
                                       entry_price=100.0, entry_time_ms=0),),
        last_rebalance_time_ms=0,
    )
    signals = [_signal("A/USDT", 10), _signal("B/USDT", 50), _signal("C/USDT", -5)]
    decisions = advisor.decide(
        state=state, signals=signals, balance_usdt=0.0,
        current_time_ms=86_400_000 * 60,  # 60 days later — past rebalance window
    )
    actions = {(d.action, d.symbol) for d in decisions}
    assert ("hold", "B/USDT") in actions
    assert not any(d.action == "buy" for d in decisions)
    assert not any(d.action == "sell" for d in decisions)


def test_advisor_swaps_when_new_winner_emerges(tmp_path: Path) -> None:
    advisor = TrendFilterAdvisor(_settings(tmp_path, top_k=1))
    state = TrendFilterState(
        holdings=(TrendFilterHolding(symbol="B/USDT", qty=0.1,
                                       entry_price=100.0, entry_time_ms=0),),
        last_rebalance_time_ms=0,
    )
    # A now has higher return than B.
    signals = [_signal("A/USDT", 80), _signal("B/USDT", 10), _signal("C/USDT", -5)]
    decisions = advisor.decide(
        state=state, signals=signals, balance_usdt=0.0,
        current_time_ms=86_400_000 * 60,
    )
    actions = {(d.action, d.symbol) for d in decisions}
    assert ("sell", "B/USDT") in actions
    assert ("buy", "A/USDT") in actions


def test_advisor_skips_rebalance_when_notional_too_small(tmp_path: Path) -> None:
    advisor = TrendFilterAdvisor(_settings(tmp_path, top_k=1, min_order_notional_usdt=5.0))
    signals = [_signal("A/USDT", 50)]
    decisions = advisor.decide(
        state=TrendFilterState.empty(), signals=signals,
        balance_usdt=2.0, current_time_ms=0,
    )
    assert any(d.action == "skip_rebalance" for d in decisions)


def test_advisor_skips_cycle_when_not_yet_at_rebalance_time(tmp_path: Path) -> None:
    advisor = TrendFilterAdvisor(_settings(tmp_path, rebalance_every_days=30))
    state = TrendFilterState(
        holdings=(TrendFilterHolding(symbol="B/USDT", qty=0.1,
                                       entry_price=100.0, entry_time_ms=0),),
        last_rebalance_time_ms=1_000_000_000_000,
    )
    decisions = advisor.decide(
        state=state, signals=[_signal("A/USDT", 80)],
        balance_usdt=0.0, current_time_ms=1_000_000_001_000,  # only 1 second later
    )
    assert len(decisions) == 1 and decisions[0].action == "skip_cycle"


def test_advisor_holds_cash_when_no_eligible_signals(tmp_path: Path) -> None:
    advisor = TrendFilterAdvisor(_settings(tmp_path, min_lookback_return_pct=20.0))
    signals = [_signal("A/USDT", 5), _signal("B/USDT", -10)]
    decisions = advisor.decide(
        state=TrendFilterState.empty(), signals=signals,
        balance_usdt=10.0, current_time_ms=0,
    )
    assert any(d.action == "hold" and d.symbol == "" for d in decisions)


def test_settings_validation(tmp_path: Path) -> None:
    with pytest.raises(TrendFilterError, match="top_k"):
        _settings(tmp_path, top_k=0)
    with pytest.raises(TrendFilterError, match="top_k cannot exceed"):
        _settings(tmp_path, top_k=100)
    with pytest.raises(TrendFilterError, match="lookback_days"):
        _settings(tmp_path, lookback_days=0)
    with pytest.raises(TrendFilterError, match="rebalance_every_days"):
        _settings(tmp_path, rebalance_every_days=0)


def test_preflight_blocks_when_emergency_stop_present(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    Path(settings.emergency_stop_path).write_text("halt")
    with pytest.raises(TrendFilterError, match="emergency stop"):
        _preflight(settings, submit_order=False)


def test_preflight_requires_proven_edge_for_live(tmp_path: Path,
                                                    monkeypatch: pytest.MonkeyPatch) -> None:
    settings = _settings(tmp_path)
    monkeypatch.delenv("TREND_FILTER_PROVEN_EDGE", raising=False)
    with pytest.raises(TrendFilterError, match="TREND_FILTER_PROVEN_EDGE"):
        _preflight(settings, submit_order=True)


def test_preflight_passes_live_when_all_gates_set(tmp_path: Path,
                                                    monkeypatch: pytest.MonkeyPatch) -> None:
    settings = _settings(tmp_path)
    monkeypatch.setenv("TREND_FILTER_PROVEN_EDGE", "yes")
    _preflight(settings, submit_order=True)  # no raise


def test_state_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    state = TrendFilterState(
        holdings=(TrendFilterHolding(symbol="BTC/USDT", qty=0.0001,
                                       entry_price=80000.0, entry_time_ms=123),),
        last_rebalance_time_ms=456,
    )
    save_state(state, path)
    restored = load_state(path)
    assert restored.last_rebalance_time_ms == 456
    assert restored.holdings[0].symbol == "BTC/USDT"
    assert restored.holdings[0].qty == pytest.approx(0.0001)

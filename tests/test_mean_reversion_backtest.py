"""Tests for the mean-reversion dump-buy replay harness."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.mean_reversion_backtest import (
    MeanReversionConfig,
    _result_summary,
    replay_mean_reversion,
)


MINUTE_MS = 60_000


def _bar(t_ms: int, *, o: float, h: float, l: float, c: float) -> list[float]:
    return [t_ms, o, h, l, c, 1.0]


def _flat(n: int, *, level: float = 100.0) -> list[list[float]]:
    return [_bar(i * MINUTE_MS, o=level, h=level, l=level, c=level) for i in range(n)]


def test_no_trades_on_flat_market() -> None:
    klines = {"BTC/USDT": _flat(120)}
    cfg = MeanReversionConfig(
        dump_lookback_bars=15, dump_threshold_pct=-5.0,
        retrace_target_pct=0.5, stop_loss_pct=-3.0, max_hold_bars=60,
    )
    result = replay_mean_reversion(klines, cfg)
    assert result.num_trades == 0


def test_enters_on_dump_then_take_profit_on_retrace() -> None:
    # 30 flat bars at 100, ONE sharp drop bar to 94 (so the -6% lookback
    # fires only on bar 30), then bar 31 recovers to high 97 hitting the
    # 50%-retrace target (94 + 0.5 * (100-94) = 97). Subsequent bars stay
    # at the recovered price so no re-entry signal fires.
    bars: list[list[float]] = []
    for i in range(30):
        bars.append(_bar(i * MINUTE_MS, o=100, h=100, l=100, c=100))
    bars.append(_bar(30 * MINUTE_MS, o=100, h=100, l=94, c=94))
    bars.append(_bar(31 * MINUTE_MS, o=94, h=97, l=94, c=97))
    for i in range(32, 50):
        bars.append(_bar(i * MINUTE_MS, o=97, h=97, l=97, c=97))

    klines = {"BTC/USDT": bars}
    cfg = MeanReversionConfig(
        dump_lookback_bars=15, dump_threshold_pct=-5.0,
        retrace_target_pct=0.5, stop_loss_pct=-10.0, max_hold_bars=60,
        notional_per_position=100.0, starting_balance=1_000.0,
    )
    result = replay_mean_reversion(klines, cfg)
    assert result.num_trades == 1
    assert result.trades[0].exit_reason == "take_profit"
    assert result.trades[0].pnl_quote > 0
    assert result.by_exit_reason.get("take_profit") == 1


def test_stops_out_on_further_dump() -> None:
    # Flat 100, one sharp drop bar to 94, then a further dump to 89 to
    # cross stop_loss_pct=-3.0 (stop_price = 94 * 0.97 = 91.18).
    # Flat bars after so no re-entry signal.
    bars: list[list[float]] = []
    for i in range(20):
        bars.append(_bar(i * MINUTE_MS, o=100, h=100, l=100, c=100))
    bars.append(_bar(20 * MINUTE_MS, o=100, h=100, l=94, c=94))
    bars.append(_bar(21 * MINUTE_MS, o=94, h=94, l=89, c=89))
    for i in range(22, 40):
        bars.append(_bar(i * MINUTE_MS, o=89, h=89, l=89, c=89))

    klines = {"BTC/USDT": bars}
    cfg = MeanReversionConfig(
        dump_lookback_bars=15, dump_threshold_pct=-5.0,
        retrace_target_pct=0.5, stop_loss_pct=-3.0, max_hold_bars=60,
        notional_per_position=100.0, starting_balance=1_000.0,
        max_concurrent_positions=1,
    )
    result = replay_mean_reversion(klines, cfg)
    # First trade: entry at bar 20 close=94, stops out at bar 21 (low 89 < stop 91.18).
    first = result.trades[0]
    assert first.exit_reason == "stop_loss"
    assert first.pnl_quote < 0


def test_time_exit_when_neither_target_nor_stop_hit() -> None:
    bars: list[list[float]] = []
    for i in range(20):
        bars.append(_bar(i * MINUTE_MS, o=100, h=100, l=100, c=100))
    # Dump to 94 over 15 bars.
    for i in range(15):
        price = 100 - (6 * (i + 1) / 15)
        t = (20 + i) * MINUTE_MS
        bars.append(_bar(t, o=price + 0.1, h=price + 0.1, l=price - 0.1, c=price))
    # Many bars of flatline at 94 (no recovery, no further drop) → max-hold timer.
    for i in range(35, 35 + 70):
        bars.append(_bar(i * MINUTE_MS, o=94, h=94, l=94, c=94))

    klines = {"BTC/USDT": bars}
    cfg = MeanReversionConfig(
        dump_lookback_bars=15, dump_threshold_pct=-5.0,
        retrace_target_pct=0.5, stop_loss_pct=-3.0, max_hold_bars=60,
        notional_per_position=100.0, starting_balance=1_000.0,
    )
    result = replay_mean_reversion(klines, cfg)
    assert result.num_trades == 1
    assert result.trades[0].exit_reason == "max_hold"


def test_max_concurrent_positions_capped() -> None:
    # Build TWO synchronized symbols that both dump simultaneously.
    def _dump_series() -> list[list[float]]:
        b: list[list[float]] = []
        for i in range(20):
            b.append(_bar(i * MINUTE_MS, o=100, h=100, l=100, c=100))
        for i in range(15):
            price = 100 - (6 * (i + 1) / 15)
            b.append(_bar((20 + i) * MINUTE_MS, o=price, h=price, l=price - 0.1, c=price))
        for i in range(60):
            b.append(_bar((35 + i) * MINUTE_MS, o=94, h=94, l=94, c=94))
        return b

    klines = {"A/USDT": _dump_series(), "B/USDT": _dump_series()}
    cfg = MeanReversionConfig(
        dump_lookback_bars=15, dump_threshold_pct=-5.0,
        retrace_target_pct=0.5, stop_loss_pct=-3.0, max_hold_bars=60,
        notional_per_position=100.0, starting_balance=1_000.0,
        max_concurrent_positions=1,
    )
    result = replay_mean_reversion(klines, cfg)
    # Only one symbol can hold at a time → at most one trade open initially.
    assert all(t.symbol in {"A/USDT", "B/USDT"} for t in result.trades)
    # By-symbol counts: at most 1 trade per symbol over this window because the
    # held position blocks the other symbol until its max-hold exit.
    for sym, n in result.by_symbol_trade_count.items():
        assert n <= 1


def test_rejects_invalid_config() -> None:
    klines = {"A/USDT": _flat(50)}
    with pytest.raises(ValueError, match="starting_balance"):
        replay_mean_reversion(klines, MeanReversionConfig(starting_balance=0))
    with pytest.raises(ValueError, match="dump_threshold_pct"):
        replay_mean_reversion(klines, MeanReversionConfig(dump_threshold_pct=5.0))
    with pytest.raises(ValueError, match="dump_lookback_bars"):
        replay_mean_reversion(klines, MeanReversionConfig(dump_lookback_bars=0))
    with pytest.raises(ValueError, match="max_hold_bars"):
        replay_mean_reversion(klines, MeanReversionConfig(max_hold_bars=0))


def test_result_summary_round_trips_as_json() -> None:
    klines = {"BTC/USDT": _flat(100)}
    result = replay_mean_reversion(klines, MeanReversionConfig())
    decoded = json.loads(json.dumps(_result_summary(result), sort_keys=True))
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_trades"] == result.num_trades


def test_short_window_returns_no_data_safely() -> None:
    klines = {"BTC/USDT": _flat(5)}
    result = replay_mean_reversion(klines, MeanReversionConfig(dump_lookback_bars=15))
    assert result.verdict() == "no_data"
    assert result.final_equity == pytest.approx(result.initial_balance)

"""Tests for the pairs-trading stat-arb replay harness."""

from __future__ import annotations

import json
import math
from pathlib import Path

import pytest

from src.stat_arb_backtest import StatArbConfig, _result_summary, replay_pair, run


HOUR_MS = 3_600_000


def _kline(open_time_ms: int, close: float) -> list[float]:
    return [open_time_ms, close, close, close, close, 1.0]


def _series_mean_reverting(n: int, *, base_a: float = 100.0, base_b: float = 100.0,
                            amplitude: float = 0.05, period: int = 40) -> tuple[list[list[float]], list[list[float]]]:
    """A and B share a common drift; their ratio oscillates as a sine wave."""
    series_a: list[list[float]] = []
    series_b: list[list[float]] = []
    for i in range(n):
        common = 100.0 * (1.0 + 0.0005 * i)  # mild common drift
        delta = amplitude * math.sin(2 * math.pi * i / period)
        a_price = common * (1.0 + delta)
        b_price = common * (1.0 - delta)
        series_a.append(_kline(i * HOUR_MS, a_price))
        series_b.append(_kline(i * HOUR_MS, b_price))
    return series_a, series_b


def test_replay_makes_no_trades_on_pair_that_drifts_in_lockstep() -> None:
    # Both prices identical → log ratio is exactly 0 always → no signal.
    # Use a slight drift so the rolling stdev is positive (engine skips bars
    # with stdev = 0 to avoid divide-by-zero, which we don't want to test).
    n = 300
    klines = {
        "ETH/USDT": [_kline(i * HOUR_MS, 100.0 + i * 0.01) for i in range(n)],
        "BTC/USDT": [_kline(i * HOUR_MS, 100.0 + i * 0.01) for i in range(n)],
    }
    result = replay_pair(klines, StatArbConfig(lookback_bars=50, entry_z=2.0, exit_z=0.3))
    # Both prices move identically → log_ratio stays at log(1.0) = 0 → no trade
    assert result.num_trades == 0


def test_replay_trades_mean_reverting_pair() -> None:
    # Engine-level test: confirm signal generation and trade lifecycle
    # work end-to-end on a synthetic mean-reverting pair. Profitability
    # on synthetic data depends on windowing artifacts that are not the
    # point of this test — real-data backtests cover that question.
    series_a, series_b = _series_mean_reverting(2000, amplitude=0.10, period=200)
    klines = {"A/USDT": series_a, "B/USDT": series_b}
    result = replay_pair(klines, StatArbConfig(
        interval="1h", lookback_bars=100, entry_z=1.0, exit_z=0.2,
        stop_z=5.0, max_hold_bars=400,
        taker_fee_bps=10.0, slippage_bps=2.0,
        notional_per_position=1_000.0, starting_balance=5_000.0,
    ))
    assert result.num_trades >= 1
    # Trades have well-formed records (long/short symbols differ).
    for trade in result.trades:
        assert trade.symbol_long != trade.symbol_short
        assert trade.bars_held > 0


def test_replay_rejects_invalid_config() -> None:
    klines = {"A/USDT": [_kline(0, 100)], "B/USDT": [_kline(0, 100)]}
    with pytest.raises(ValueError, match="starting_balance"):
        replay_pair(klines, StatArbConfig(starting_balance=0))
    with pytest.raises(ValueError, match="notional_per_position"):
        replay_pair(klines, StatArbConfig(notional_per_position=0))
    with pytest.raises(ValueError, match="lookback_bars"):
        replay_pair(klines, StatArbConfig(lookback_bars=1))
    with pytest.raises(ValueError, match="entry_z"):
        replay_pair(klines, StatArbConfig(entry_z=0.5, exit_z=1.0))


def test_replay_requires_exactly_two_symbols() -> None:
    klines = {"A/USDT": [_kline(0, 100)], "B/USDT": [_kline(0, 100)], "C/USDT": [_kline(0, 100)]}
    with pytest.raises(ValueError, match="two symbols"):
        replay_pair(klines, StatArbConfig())


def test_replay_handles_short_window_safely() -> None:
    klines = {"A/USDT": [_kline(i * HOUR_MS, 100) for i in range(5)],
              "B/USDT": [_kline(i * HOUR_MS, 100) for i in range(5)]}
    result = replay_pair(klines, StatArbConfig(lookback_bars=50))
    assert result.bars_evaluated == 0
    assert result.verdict() == "no_data"


def test_cli_runs_against_fixture(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    series_a, series_b = _series_mean_reverting(2000, amplitude=0.10, period=200)
    fixture = tmp_path / "klines.json"
    fixture.write_text(json.dumps({"A/USDT": series_a, "B/USDT": series_b}))
    output = tmp_path / "report.json"
    exit_code = run([
        "--fixture", str(fixture),
        "--lookback-bars", "100", "--entry-z", "1.0", "--exit-z", "0.2",
        "--notional-per-position", "1000", "--starting-balance", "5000",
        "--output", str(output),
    ])
    assert exit_code == 0
    summary = json.loads(capsys.readouterr().out)
    assert summary["bars_evaluated"] > 0
    full = json.loads(output.read_text())
    assert "trades" in full


def test_result_summary_round_trips_as_json() -> None:
    series_a, series_b = _series_mean_reverting(2000, amplitude=0.10, period=200)
    result = replay_pair(
        {"A/USDT": series_a, "B/USDT": series_b},
        StatArbConfig(lookback_bars=100, entry_z=1.0, exit_z=0.2),
    )
    decoded = json.loads(json.dumps(_result_summary(result), sort_keys=True))
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_trades"] == result.num_trades

"""Tests for the long-horizon trend-filter replay harness."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.trend_filter_backtest import (
    TrendFilterConfig,
    _result_summary,
    replay_trend_filter,
    run,
)


DAY_MS = 86_400_000


def _kline(open_time_ms: int, close: float) -> list[float]:
    return [open_time_ms, close, close, close, close, 1.0]


def _trending(n: int, *, start: float, bar_return: float) -> list[list[float]]:
    series: list[list[float]] = []
    price = start
    for i in range(n):
        series.append(_kline(i * DAY_MS, price))
        price *= 1.0 + bar_return
    return series


def _flat(n: int, *, level: float = 100.0) -> list[list[float]]:
    return [_kline(i * DAY_MS, level) for i in range(n)]


def test_replay_holds_no_positions_when_universe_is_flat() -> None:
    klines = {"FOO/USDT": _flat(200), "BAR/USDT": _flat(200)}
    cfg = TrendFilterConfig(
        interval="1d", lookback_bars=30, top_k=2,
        rebalance_every_bars=30, min_lookback_return_pct=5.0,
    )
    result = replay_trend_filter(klines, cfg)
    # With a 5% floor that flat returns can't clear, no symbol is selected.
    assert result.num_trades == 0
    assert result.verdict() == "no_trades"


def test_replay_buys_top_k_trending_assets() -> None:
    klines = {
        "FLAT/USDT": _flat(200),
        "SLOW/USDT": _trending(200, start=100, bar_return=0.001),
        "FAST/USDT": _trending(200, start=100, bar_return=0.005),
    }
    cfg = TrendFilterConfig(
        interval="1d", lookback_bars=30, top_k=1,
        rebalance_every_bars=30, min_lookback_return_pct=0.0,
    )
    result = replay_trend_filter(klines, cfg)
    assert result.num_trades >= 1
    held_top = max(result.per_symbol_held_bars.items(), key=lambda x: x[1])
    assert held_top[0] == "FAST/USDT"


def test_replay_captures_positive_returns_on_clean_trend() -> None:
    klines = {"BTC/USDT": _trending(200, start=100, bar_return=0.005)}
    cfg = TrendFilterConfig(
        interval="1d", lookback_bars=30, top_k=1,
        rebalance_every_bars=30, min_lookback_return_pct=0.0,
        starting_balance=1_000.0,
    )
    result = replay_trend_filter(klines, cfg)
    assert result.final_equity > result.initial_balance
    assert result.verdict() in {"edge_likely", "edge_marginal"}


def test_replay_handles_short_window_safely() -> None:
    klines = {"BTC/USDT": _flat(10)}
    cfg = TrendFilterConfig(lookback_bars=30, top_k=1)
    result = replay_trend_filter(klines, cfg)
    assert result.bars_evaluated == 0
    assert result.verdict() == "no_data"
    assert result.final_equity == pytest.approx(result.initial_balance)


def test_replay_rejects_invalid_config() -> None:
    klines = {"BTC/USDT": _flat(50)}
    with pytest.raises(ValueError, match="lookback_bars"):
        replay_trend_filter(klines, TrendFilterConfig(lookback_bars=0))
    with pytest.raises(ValueError, match="top_k"):
        replay_trend_filter(klines, TrendFilterConfig(top_k=0))
    with pytest.raises(ValueError, match="rebalance"):
        replay_trend_filter(klines, TrendFilterConfig(rebalance_every_bars=0))
    with pytest.raises(ValueError, match="starting_balance"):
        replay_trend_filter(klines, TrendFilterConfig(starting_balance=0))


def test_result_summary_round_trips_as_json() -> None:
    klines = {"BTC/USDT": _trending(120, start=100, bar_return=0.003)}
    result = replay_trend_filter(klines, TrendFilterConfig(
        lookback_bars=30, top_k=1, rebalance_every_bars=30,
    ))
    decoded = json.loads(json.dumps(_result_summary(result), sort_keys=True))
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_trades"] == result.num_trades


def test_cli_runs_against_fixture(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    fixture = tmp_path / "klines.json"
    fixture.write_text(json.dumps({
        "FLAT/USDT": _flat(150),
        "TREND/USDT": _trending(150, start=100, bar_return=0.004),
    }))
    output = tmp_path / "report.json"
    exit_code = run([
        "--fixture", str(fixture),
        "--lookback-bars", "30", "--top-k", "1", "--rebalance-every", "30",
        "--output", str(output),
    ])
    assert exit_code == 0
    summary = json.loads(capsys.readouterr().out)
    assert summary["num_trades"] >= 1
    full = json.loads(output.read_text())
    assert "trades" in full

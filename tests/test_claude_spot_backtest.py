"""Tests for the deterministic Claude Spot replay harness."""

from __future__ import annotations

import json
import math
from pathlib import Path

import pytest

from src.claude_spot_backtest import (
    BASE_CASH,
    BacktestConfig,
    _half_spread_bps,
    _result_summary,
    replay,
    run,
)


HOUR_MS = 3_600_000


def _kline(open_time_ms: int, close: float, *, spread_pct: float = 0.001) -> list[float]:
    high = close * (1.0 + spread_pct)
    low = close * (1.0 - spread_pct)
    return [open_time_ms, close, high, low, close, 1.0]


def _flat_series(n: int, *, level: float = 100.0) -> list[list[float]]:
    return [_kline(i * HOUR_MS, level) for i in range(n)]


def _trending_series(n: int, *, start: float = 100.0, bar_return: float = 0.002) -> list[list[float]]:
    """Compounding upward trend large enough to clear the modeled cost bar."""
    series: list[list[float]] = []
    price = start
    for i in range(n):
        series.append(_kline(i * HOUR_MS, price))
        price *= 1.0 + bar_return
    return series


def test_half_spread_clipped_to_bounds() -> None:
    cfg = BacktestConfig(min_spread_bps=2.0, max_spread_bps=100.0)
    tight = _kline(0, 100.0, spread_pct=0.0)
    wide = _kline(0, 100.0, spread_pct=0.5)  # would otherwise blow past max
    assert _half_spread_bps(tight, cfg=cfg) == pytest.approx(cfg.min_spread_bps)
    assert _half_spread_bps(wide, cfg=cfg) == pytest.approx(cfg.max_spread_bps)


def test_replay_holds_when_no_signal_clears_cost_bar() -> None:
    klines = {"BTC/USDT": _flat_series(200)}
    result = replay(klines, BacktestConfig(lookback_bars=24, min_edge_after_friction_bps=50.0))
    assert result.num_trades == 0
    assert result.final_balance == pytest.approx(result.initial_balance)
    assert result.verdict() == "no_trades"
    assert result.bars_in_position_pct == 0.0


def test_replay_captures_trend_when_edge_clears_cost_bar() -> None:
    klines = {"BTC/USDT": _trending_series(200, bar_return=0.005)}
    cfg = BacktestConfig(
        lookback_bars=12,
        min_edge_after_friction_bps=50.0,
        taker_fee_bps=10.0,
        slippage_bps=5.0,
    )
    result = replay(klines, cfg)

    assert result.num_trades >= 1
    assert result.final_balance > result.initial_balance
    assert result.total_return_pct > 0.0
    assert result.bars_in_position_pct > 50.0
    assert result.verdict() in {"edge_likely", "edge_marginal"}


def test_replay_prefers_trending_over_flat_candidate() -> None:
    klines = {
        "FLAT/USDT": _flat_series(120),
        "TREND/USDT": _trending_series(120, bar_return=0.004),
    }
    result = replay(
        klines,
        BacktestConfig(lookback_bars=12, min_edge_after_friction_bps=50.0),
    )

    assert result.num_trades >= 1
    held = result.per_asset_bars_held
    assert held.get("TREND/USDT", 0) > held.get("FLAT/USDT", 0)
    assert held.get("TREND/USDT", 0) > held.get(BASE_CASH, 0)


def test_replay_handles_short_window_safely() -> None:
    klines = {"BTC/USDT": _flat_series(5)}
    result = replay(klines, BacktestConfig(lookback_bars=24))
    assert result.bars_evaluated == 0
    assert result.verdict() == "no_data"
    assert result.final_balance == pytest.approx(result.initial_balance)


def test_replay_rejects_invalid_config() -> None:
    klines = {"BTC/USDT": _flat_series(50)}
    with pytest.raises(ValueError, match="lookback_bars"):
        replay(klines, BacktestConfig(lookback_bars=0))
    with pytest.raises(ValueError, match="starting_balance"):
        replay(klines, BacktestConfig(starting_balance=0.0))


def test_result_summary_round_trips_as_json() -> None:
    klines = {"BTC/USDT": _trending_series(80, bar_return=0.003)}
    result = replay(klines, BacktestConfig(lookback_bars=12))
    summary = _result_summary(result)
    encoded = json.dumps(summary, sort_keys=True)
    decoded = json.loads(encoded)
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_trades"] == result.num_trades
    assert math.isclose(decoded["final_balance"], round(result.final_balance, 4))


def test_cli_runs_against_fixture(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    fixture_path = tmp_path / "klines.json"
    fixture_path.write_text(
        json.dumps({"BTC/USDT": _trending_series(80, bar_return=0.004)})
    )

    output_path = tmp_path / "report.json"
    exit_code = run(
        [
            "--fixture",
            str(fixture_path),
            "--lookback-bars",
            "12",
            "--min-edge-bps",
            "50",
            "--output",
            str(output_path),
        ]
    )
    assert exit_code == 0

    summary = json.loads(capsys.readouterr().out)
    assert summary["verdict"] in {"edge_likely", "edge_marginal", "no_edge", "no_trades"}
    assert summary["bars_evaluated"] > 0

    full_report = json.loads(output_path.read_text())
    assert "trades" in full_report
    assert "config" in full_report

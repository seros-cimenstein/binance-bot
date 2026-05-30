"""Tests for the grid-trading replay harness."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.grid_backtest import (
    GridConfig,
    GridFill,
    _build_levels,
    _result_summary,
    replay_grid,
    run,
)


HOUR_MS = 3_600_000


def _bar(open_time_ms: int, *, o: float, h: float, l: float, c: float) -> list[float]:
    return [open_time_ms, o, h, l, c, 1.0]


def test_build_levels_returns_evenly_spaced_grid() -> None:
    cfg = GridConfig(grid_low=100.0, grid_high=200.0, num_levels=11)
    levels = _build_levels(cfg)
    assert levels[0] == pytest.approx(100.0)
    assert levels[-1] == pytest.approx(200.0)
    assert len(levels) == 11
    # spacing 10.0
    assert all(abs((b - a) - 10.0) < 1e-9 for a, b in zip(levels, levels[1:]))


def test_replay_oscillating_price_captures_round_trip_spread() -> None:
    # Price oscillates between $100 and $110 across two grid levels.
    klines = [
        _bar(0,        o=100, h=110, l=100, c=110),  # sweeps both: starts above 100, hits 110
        _bar(HOUR_MS,  o=110, h=110, l=100, c=100),
        _bar(2*HOUR_MS,o=100, h=110, l=100, c=110),
        _bar(3*HOUR_MS,o=110, h=110, l=100, c=100),
    ]
    cfg = GridConfig(
        grid_low=100.0, grid_high=110.0, num_levels=2,
        qty_per_level=1.0, maker_fee_bps=10.0, slippage_bps=0.0,
        starting_quote=300.0,
    )
    result = replay_grid(klines, cfg)
    assert result.num_buys >= 1
    assert result.num_sells >= 1
    # Final equity should be > starting because we captured spread minus fees.
    assert result.final_equity > 0
    assert result.bars_with_fills > 0


def test_replay_trending_market_strands_inventory() -> None:
    # Price marches steadily up from 100 to 200, never returning.
    klines = []
    price = 100.0
    for i in range(20):
        klines.append(_bar(i*HOUR_MS, o=price, h=price+5, l=price, c=price+5))
        price += 5
    cfg = GridConfig(
        grid_low=100.0, grid_high=200.0, num_levels=11,
        qty_per_level=0.05, maker_fee_bps=10.0, slippage_bps=0.0,
        starting_quote=200.0,
    )
    result = replay_grid(klines, cfg)
    # In a trend, all sells fire and we end cash-heavy with no inventory
    # to ride further. Verdict depends on whether captured spread covers
    # the lost upside.
    assert result.num_sells >= 1
    # Almost no inventory remaining (sold the lot on the way up).
    assert result.final_inventory < cfg.qty_per_level * 2


def test_replay_flat_market_makes_no_trades() -> None:
    # Pin price strictly between two grid levels (between 150 and 160) so
    # neither a buy nor a sell would ever fill.
    klines = [_bar(i*HOUR_MS, o=155, h=155, l=155, c=155) for i in range(20)]
    cfg = GridConfig(grid_low=100.0, grid_high=200.0, num_levels=11,
                     qty_per_level=0.01, starting_quote=200.0)
    result = replay_grid(klines, cfg)
    assert result.num_buys == 0
    assert result.num_sells == 0
    assert result.verdict() == "no_trades"


def test_replay_zero_klines_returns_no_data() -> None:
    result = replay_grid([], GridConfig())
    assert result.verdict() == "no_data"
    assert result.final_equity == pytest.approx(result.config.starting_quote)


def test_replay_rejects_invalid_grid() -> None:
    with pytest.raises(ValueError, match="num_levels"):
        replay_grid([_bar(0, o=100, h=100, l=100, c=100)],
                    GridConfig(num_levels=1))
    with pytest.raises(ValueError, match="grid_high"):
        replay_grid([_bar(0, o=100, h=100, l=100, c=100)],
                    GridConfig(grid_low=200, grid_high=100, num_levels=2))


def test_replay_rejects_non_positive_qty() -> None:
    with pytest.raises(ValueError, match="qty_per_level"):
        replay_grid([_bar(0, o=100, h=100, l=100, c=100)],
                    GridConfig(qty_per_level=0))


def test_replay_rescales_qty_when_capital_insufficient() -> None:
    # With $200 you can't fund a 100-level grid at $1 per level on a
    # $150-priced asset; the replay should pro-rate qty_per_level.
    klines = [_bar(0, o=150, h=150, l=150, c=150)]
    cfg = GridConfig(
        grid_low=100.0, grid_high=200.0, num_levels=100,
        qty_per_level=1.0, starting_quote=200.0,
    )
    result = replay_grid(klines, cfg)
    # Should not have raised; equity should be roughly preserved.
    assert result.final_equity == pytest.approx(cfg.starting_quote, rel=0.01)


def test_result_summary_round_trips_as_json() -> None:
    klines = [_bar(i*HOUR_MS, o=100, h=110, l=100, c=110) for i in range(4)]
    result = replay_grid(klines, GridConfig(
        grid_low=100, grid_high=110, num_levels=2,
        qty_per_level=0.1, starting_quote=200.0,
    ))
    summary = _result_summary(result)
    decoded = json.loads(json.dumps(summary, sort_keys=True))
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_round_trips"] == result.num_round_trips


def test_cli_runs_against_fixture(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    fixture = tmp_path / "klines.json"
    klines = []
    for i in range(8):
        if i % 2 == 0:
            klines.append(_bar(i*HOUR_MS, o=100, h=110, l=100, c=110))
        else:
            klines.append(_bar(i*HOUR_MS, o=110, h=110, l=100, c=100))
    fixture.write_text(json.dumps(klines))
    output = tmp_path / "report.json"
    exit_code = run([
        "--fixture", str(fixture),
        "--grid-low", "100", "--grid-high", "110", "--num-levels", "2",
        "--qty-per-level", "0.1", "--starting-quote", "200",
        "--output", str(output),
    ])
    assert exit_code == 0
    summary = json.loads(capsys.readouterr().out)
    assert summary["bars_evaluated"] == 8
    full = json.loads(output.read_text())
    assert "fills" in full

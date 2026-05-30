"""Tests for the spot/perp basis-arbitrage replay harness."""

from __future__ import annotations

import json
import math
from pathlib import Path

import pytest

from src.basis_arb_backtest import BasisArbConfig, _result_summary, replay_basis_arb, run


HOUR_MS = 3_600_000


def _kline(open_time_ms: int, close: float) -> list[float]:
    return [open_time_ms, close, close, close, close, 1.0]


def _spot_perp_with_basis_spike(
    n: int, *, spot_level: float = 100.0, spike_at: int, spike_bps: float, decay_bars: int = 5,
) -> tuple[list[list[float]], list[list[float]]]:
    """Build spot + perp series where basis spikes positive at spike_at and decays."""
    spot: list[list[float]] = []
    perp: list[list[float]] = []
    for i in range(n):
        spot.append(_kline(i * HOUR_MS, spot_level))
        if i < spike_at:
            premium = 0.0
        else:
            decay = max(0.0, 1.0 - (i - spike_at) / decay_bars)
            premium = spike_bps * decay
        perp.append(_kline(i * HOUR_MS, spot_level * (1.0 + premium / 10_000.0)))
    return spot, perp


def test_replay_no_trades_when_basis_within_band() -> None:
    n = 100
    spot = [_kline(i * HOUR_MS, 100.0) for i in range(n)]
    perp = [_kline(i * HOUR_MS, 100.0) for i in range(n)]
    cfg = BasisArbConfig(entry_basis_bps=30.0, exit_basis_bps=5.0)
    result = replay_basis_arb("BTC/USDT", spot, perp, cfg)
    assert result.num_trades == 0


def test_replay_enters_on_premium_spike_and_exits_on_convergence() -> None:
    spot, perp = _spot_perp_with_basis_spike(80, spike_at=20, spike_bps=100, decay_bars=10)
    cfg = BasisArbConfig(
        entry_basis_bps=50.0, exit_basis_bps=10.0, stop_basis_bps=300.0,
        max_hold_bars=40,
        taker_fee_spot_bps=10.0, taker_fee_futures_bps=5.0,
        slippage_per_leg_bps=2.0,
        notional_per_position=1_000.0, starting_balance=5_000.0,
    )
    result = replay_basis_arb("BTC/USDT", spot, perp, cfg)
    assert result.num_trades >= 1
    assert any(t.side == "short_basis" for t in result.trades)
    # Convergence trade is profitable when realized > round-trip cost.
    # Realized basis ~ 100 - 10 = 90 bps. Round-trip = 2*(10+5)+4*2 = 38 bps.
    # Expected net positive.
    assert result.final_equity > result.initial_balance


def test_replay_skips_negative_basis_in_spot_only_mode() -> None:
    n = 80
    spot = [_kline(i * HOUR_MS, 100.0) for i in range(n)]
    # Perp trades at a discount.
    perp = [_kline(i * HOUR_MS, 100.0 * (1.0 - 50.0 / 10_000.0)) for i in range(n)]
    cfg = BasisArbConfig(entry_basis_bps=30.0, mode="spot_only")
    result = replay_basis_arb("BTC/USDT", spot, perp, cfg)
    assert result.num_trades == 0


def test_replay_both_sides_takes_negative_basis() -> None:
    n = 80
    spot = [_kline(i * HOUR_MS, 100.0) for i in range(n)]
    # Perp at a -50 bps discount that converges back to spot at bar 30.
    perp = []
    for i in range(n):
        if i < 30:
            premium = -50.0
        else:
            decay = max(0.0, 1.0 - (i - 30) / 10.0)
            premium = -50.0 * decay
        perp.append(_kline(i * HOUR_MS, 100.0 * (1.0 + premium / 10_000.0)))
    cfg = BasisArbConfig(entry_basis_bps=30.0, exit_basis_bps=5.0, mode="both_sides",
                          max_hold_bars=60)
    result = replay_basis_arb("BTC/USDT", spot, perp, cfg)
    assert result.num_trades >= 1
    assert any(t.side == "long_basis" for t in result.trades)


def test_replay_rejects_invalid_config() -> None:
    spot = [_kline(0, 100)]; perp = [_kline(0, 100)]
    with pytest.raises(ValueError, match="starting_balance"):
        replay_basis_arb("BTC/USDT", spot, perp, BasisArbConfig(starting_balance=0))
    with pytest.raises(ValueError, match="notional_per_position"):
        replay_basis_arb("BTC/USDT", spot, perp, BasisArbConfig(notional_per_position=0))
    with pytest.raises(ValueError, match="entry_basis_bps"):
        replay_basis_arb("BTC/USDT", spot, perp, BasisArbConfig(entry_basis_bps=5.0, exit_basis_bps=10.0))
    with pytest.raises(ValueError, match="mode"):
        replay_basis_arb("BTC/USDT", spot, perp, BasisArbConfig(mode="bogus"))


def test_result_summary_round_trips_as_json() -> None:
    spot, perp = _spot_perp_with_basis_spike(60, spike_at=15, spike_bps=80, decay_bars=10)
    result = replay_basis_arb("BTC/USDT", spot, perp, BasisArbConfig())
    decoded = json.loads(json.dumps(_result_summary(result), sort_keys=True))
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_trades"] == result.num_trades


def test_cli_runs_against_fixture(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    spot, perp = _spot_perp_with_basis_spike(80, spike_at=20, spike_bps=100, decay_bars=10)
    fixture = tmp_path / "fixture.json"
    fixture.write_text(json.dumps({"spot": spot, "perp": perp}))
    output = tmp_path / "report.json"
    exit_code = run([
        "--fixture", str(fixture), "--symbol", "BTC/USDT",
        "--entry-basis-bps", "50", "--exit-basis-bps", "10",
        "--notional-per-position", "1000", "--starting-balance", "5000",
        "--output", str(output),
    ])
    assert exit_code == 0
    summary = json.loads(capsys.readouterr().out)
    assert summary["bars_evaluated"] > 0
    full = json.loads(output.read_text())
    assert "trades" in full

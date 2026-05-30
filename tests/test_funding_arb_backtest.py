"""Tests for the funding-rate harvest replay harness."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.funding_arb_backtest import (
    FundingArbConfig,
    FundingBacktestResult,
    _result_summary,
    replay_funding,
    run,
)


EIGHT_HOURS_MS = 8 * 60 * 60 * 1000


def _funding_event(ts_ms: int, rate_bps: float, *, symbol: str = "BTCUSDT", mark: float = 100.0) -> dict:
    return {
        "symbol": symbol,
        "fundingTime": ts_ms,
        "fundingRate": rate_bps / 10_000.0,  # incoming is fraction, not bps
        "markPrice": str(mark),
    }


def _series(rate_bps_per_event: list[float], *, symbol: str = "BTCUSDT") -> list[dict]:
    return [
        _funding_event(i * EIGHT_HOURS_MS, rate, symbol=symbol)
        for i, rate in enumerate(rate_bps_per_event)
    ]


def test_replay_holds_when_funding_never_clears_entry() -> None:
    funding = {"BTCUSDT": _series([0.1] * 50)}
    result = replay_funding(
        funding,
        FundingArbConfig(entry_funding_bps=1.0, exit_funding_bps=-0.5),
    )
    assert result.num_trades == 0
    assert result.final_balance == pytest.approx(result.initial_balance)
    assert result.verdict() == "no_trades"


def test_replay_collects_funding_when_persistently_positive() -> None:
    # 100 funding events at 5 bps each. Round-trip cost at defaults:
    # (10+5)*2 + 2*2 = 38 bps. Funding collected if held all: 100*5 = 500 bps.
    # Net per trade: 500 - 38 = +462 bps gross — but only one trade fits if no
    # exit fires until max_hold_events. Set max_hold to a value < 100 so we
    # see multiple trades.
    funding = {"BTCUSDT": _series([5.0] * 100)}
    cfg = FundingArbConfig(
        entry_funding_bps=1.0,
        exit_funding_bps=-0.5,
        max_hold_funding_events=30,
        max_concurrent_positions=1,
        per_position_notional_pct=100.0,
        starting_balance=10_000.0,
    )
    result = replay_funding(funding, cfg)
    assert result.num_trades >= 2
    assert result.final_balance > result.initial_balance
    assert result.verdict() in {"edge_likely", "edge_marginal"}
    assert all(t.net_pnl_bps > 0 for t in result.trades)


def test_replay_exits_when_funding_drops_below_exit_threshold() -> None:
    series = _series([5.0] * 10 + [-2.0] * 10)
    funding = {"BTCUSDT": series}
    cfg = FundingArbConfig(
        entry_funding_bps=1.0,
        exit_funding_bps=-0.5,
        max_hold_funding_events=100,
        max_concurrent_positions=1,
        per_position_notional_pct=100.0,
    )
    result = replay_funding(funding, cfg)
    assert result.num_trades >= 1
    # The exit firing on the first negative bar must produce an event where
    # the trade closed before max_hold was hit.
    short_trades = [t for t in result.trades if t.funding_events_held < 100]
    assert short_trades, "expected at least one trade closed by exit threshold"


def test_replay_concurrent_positions_capped() -> None:
    # Two symbols both at high funding. cap concurrent = 1.
    funding = {
        "BTCUSDT": _series([5.0] * 20),
        "ETHUSDT": _series([5.0] * 20, symbol="ETHUSDT"),
    }
    cfg = FundingArbConfig(
        entry_funding_bps=1.0,
        max_concurrent_positions=1,
        max_hold_funding_events=5,
        per_position_notional_pct=100.0,
    )
    result = replay_funding(funding, cfg)
    # Even with two candidates each event, at most one is held at a time.
    # Trades should be a mix of both symbols.
    symbols_traded = {t.symbol for t in result.trades}
    assert len(symbols_traded) >= 1
    # No two trades should overlap in time.
    open_intervals = sorted((t.open_time_ms, t.close_time_ms) for t in result.trades)
    for (s1, e1), (s2, e2) in zip(open_intervals, open_intervals[1:]):
        assert s2 >= e1


def test_replay_force_closes_open_positions_at_end() -> None:
    funding = {"BTCUSDT": _series([5.0] * 10)}
    cfg = FundingArbConfig(
        entry_funding_bps=1.0,
        max_hold_funding_events=1000,  # very large so no natural close
        max_concurrent_positions=1,
        per_position_notional_pct=100.0,
    )
    result = replay_funding(funding, cfg)
    assert result.num_trades == 1


def test_fees_accounting_is_single_round_trip_per_trade() -> None:
    # One trade, held for a single funding event.
    funding = {"BTCUSDT": _series([5.0, -2.0])}
    cfg = FundingArbConfig(
        entry_funding_bps=1.0,
        exit_funding_bps=-0.5,
        max_concurrent_positions=1,
        per_position_notional_pct=100.0,
        starting_balance=10_000.0,
    )
    result = replay_funding(funding, cfg)
    assert result.num_trades == 1
    # round-trip cost at defaults = 2*(10+5) + 4*2 = 38 bps
    expected_round_trip = 38.0
    # Fees paid total across the trade should equal round-trip exactly.
    assert result.fees_paid_bps == pytest.approx(expected_round_trip, abs=0.5)


def test_replay_rejects_invalid_config() -> None:
    with pytest.raises(ValueError, match="starting_balance"):
        replay_funding({}, FundingArbConfig(starting_balance=0))
    with pytest.raises(ValueError, match="per_position_notional_pct"):
        replay_funding({}, FundingArbConfig(per_position_notional_pct=0))
    with pytest.raises(ValueError, match="max_concurrent_positions"):
        replay_funding({}, FundingArbConfig(max_concurrent_positions=0))


def test_result_summary_round_trips_as_json() -> None:
    funding = {"BTCUSDT": _series([5.0] * 20)}
    result = replay_funding(
        funding,
        FundingArbConfig(entry_funding_bps=1.0, max_hold_funding_events=5),
    )
    summary = _result_summary(result)
    encoded = json.dumps(summary, sort_keys=True)
    decoded = json.loads(encoded)
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_trades"] == result.num_trades


def test_cli_runs_against_fixture(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    fixture_path = tmp_path / "funding.json"
    fixture_path.write_text(json.dumps({"BTCUSDT": _series([5.0] * 30)}))
    output_path = tmp_path / "report.json"
    exit_code = run(
        [
            "--fixture",
            str(fixture_path),
            "--max-hold-events",
            "5",
            "--max-concurrent-positions",
            "1",
            "--per-position-notional-pct",
            "100",
            "--output",
            str(output_path),
        ]
    )
    assert exit_code == 0
    summary = json.loads(capsys.readouterr().out)
    assert summary["bars_evaluated"] > 0
    full = json.loads(output_path.read_text())
    assert "trades" in full

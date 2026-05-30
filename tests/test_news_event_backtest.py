"""Tests for the news/event pre-position backtest harness."""

from __future__ import annotations

import json

import pytest

from src.news_event_backtest import (
    MacroEvent,
    NewsEventConfig,
    _kline_close,
    replay_events,
    simulate_event,
)


def _bar(open_time_ms: int, close: float) -> list[float]:
    return [open_time_ms, close, close, close, close, 1.0]


def _build_klines(start_ms: int, n: int, *, prices) -> list[list[float]]:
    if callable(prices):
        return [_bar(start_ms + i * 60_000, float(prices(i))) for i in range(n)]
    return [_bar(start_ms + i * 60_000, float(prices[i])) for i in range(n)]


def test_kline_close_finds_bar_containing_target() -> None:
    klines = _build_klines(0, 10, prices=[100 + i for i in range(10)])
    # Target inside bar 3 (open 180000, contains 180000..239999).
    assert _kline_close(klines, 200_000) == 103.0


def test_kline_close_falls_back_to_nearest_within_5min() -> None:
    klines = _build_klines(0, 10, prices=[100 + i for i in range(10)])
    # Target 3 minutes past last bar — falls back to last bar's close.
    last_bar_open = 9 * 60_000
    assert _kline_close(klines, last_bar_open + 3 * 60_000) == 109.0


def test_kline_close_returns_none_when_far_outside() -> None:
    klines = _build_klines(0, 5, prices=[100, 100, 100, 100, 100])
    assert _kline_close(klines, 60_000 * 60) is None  # 60 min past, far outside


def test_kline_close_returns_none_when_no_klines() -> None:
    assert _kline_close([], 0) is None


def test_simulate_long_only_captures_upmove() -> None:
    event = MacroEvent("E", "fomc", "2024-01-31T19:00:00+00:00")
    event_ms = 1706727600_000
    # Pre window: 15 min before, price 100. Post: 30 min after, price 102.
    klines = []
    for i in range(60):
        # bar at (event_ms - 30*60000 + i*60000)
        t = event_ms - 30 * 60_000 + i * 60_000
        price = 100.0 if t < event_ms else 102.0
        klines.append(_bar(t, price))
    cfg = NewsEventConfig(pre_minutes=15, post_minutes=30, strategy="long_only",
                           taker_fee_bps=10.0, slippage_bps=5.0, notional_usdt=100.0)
    trade = simulate_event(event, klines, cfg)
    assert trade is not None
    # Move: +2%. Gross P&L: 2.0 USDT on 100 notional. Round-trip fee: 30 bps = 0.30
    assert trade.move_pct == pytest.approx(2.0, abs=0.01)
    assert trade.pnl_usdt > 1.5  # net of fees


def test_simulate_short_only_captures_downmove() -> None:
    event = MacroEvent("E", "fomc", "2024-01-31T19:00:00+00:00")
    event_ms = 1706727600_000
    klines = []
    for i in range(60):
        t = event_ms - 30 * 60_000 + i * 60_000
        price = 100.0 if t < event_ms else 98.0
        klines.append(_bar(t, price))
    cfg = NewsEventConfig(strategy="short_only", notional_usdt=100.0)
    trade = simulate_event(event, klines, cfg)
    assert trade is not None
    assert trade.pnl_usdt > 1.5


def test_simulate_straddle_captures_abs_move_minus_costs() -> None:
    event = MacroEvent("E", "fomc", "2024-01-31T19:00:00+00:00")
    event_ms = 1706727600_000
    klines = []
    for i in range(60):
        t = event_ms - 30 * 60_000 + i * 60_000
        price = 100.0 if t < event_ms else 105.0  # +5% move
        klines.append(_bar(t, price))
    cfg = NewsEventConfig(strategy="straddle", notional_usdt=100.0,
                           taker_fee_bps=10.0, slippage_bps=5.0)
    trade = simulate_event(event, klines, cfg)
    assert trade is not None
    # Abs move 5% * 100 = $5; fees = 4 * 100 * 15 bps = $0.60
    assert trade.pnl_usdt == pytest.approx(5.0 - 0.60, abs=0.05)


def test_simulate_returns_none_when_no_klines_match() -> None:
    event = MacroEvent("E", "fomc", "2024-01-31T19:00:00+00:00")
    trade = simulate_event(event, [], NewsEventConfig())
    assert trade is None


def test_simulate_rejects_unknown_strategy() -> None:
    event = MacroEvent("E", "fomc", "2024-01-31T19:00:00+00:00")
    event_ms = 1706727600_000
    # Klines on BOTH sides of the event so the strategy switch is reached.
    klines = [_bar(event_ms - 30 * 60_000 + i * 60_000, 100.0) for i in range(60)]
    cfg = NewsEventConfig(strategy="bogus")
    with pytest.raises(ValueError, match="bogus"):
        simulate_event(event, klines, cfg)


def test_replay_aggregates_metrics_correctly() -> None:
    e1 = MacroEvent("E1", "fomc", "2024-01-31T19:00:00+00:00")
    e2 = MacroEvent("E2", "fomc", "2024-03-20T18:00:00+00:00")
    e1_ms = 1706727600_000
    e2_ms = 1710957600_000

    def _klines(event_ms: int, post_price: float) -> list[list[float]]:
        ks = []
        for i in range(60):
            t = event_ms - 30 * 60_000 + i * 60_000
            ks.append(_bar(t, 100.0 if t < event_ms else post_price))
        return ks

    klines_by_event = {
        "E1": _klines(e1_ms, 102.0),  # +2% → long_only profits
        "E2": _klines(e2_ms, 98.0),   # -2% → long_only loses
    }
    result = replay_events([e1, e2], klines_by_event,
                            NewsEventConfig(strategy="long_only", notional_usdt=100.0))
    assert result.num_trades == 2
    # One win, one loss, average around 0.
    assert result.events_evaluated == 2
    assert result.events_skipped_missing_data == 0
    assert result.by_category_count == {"fomc": 2}


def test_replay_skips_events_with_missing_data() -> None:
    e1 = MacroEvent("E1", "fomc", "2024-01-31T19:00:00+00:00")
    e2 = MacroEvent("E2", "fomc", "2024-03-20T18:00:00+00:00")
    e1_ms = 1706727600_000
    klines_by_event = {
        "E1": _build_klines(e1_ms - 30 * 60_000, 60, prices=[100.0] * 60),
        "E2": [],  # missing
    }
    result = replay_events([e1, e2], klines_by_event, NewsEventConfig())
    assert result.events_evaluated == 2
    assert result.events_skipped_missing_data == 1
    assert result.num_trades == 1


def test_result_summary_round_trips_as_json() -> None:
    from src.news_event_backtest import _summary
    result = replay_events([], {}, NewsEventConfig())
    decoded = json.loads(json.dumps(_summary(result)))
    assert decoded["verdict"] == result.verdict()
    assert decoded["num_trades"] == 0

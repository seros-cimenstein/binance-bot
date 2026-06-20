"""Tests for the dedicated listing sniper bot."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.listing_sniper import (
    DailySpend,
    ListingSniperError,
    ListingSniperSettings,
    ListingSniperState,
    _format_live_fire_alert,
    _is_unknown_symbol_error,
    _preflight,
    compute_new_listings,
    fetch_trading_symbols_for_quote,
    fire_live,
    fire_paper,
    load_state,
    momentum_should_exit,
    save_state,
)


def _settings(tmp_path: Path, **overrides) -> ListingSniperSettings:
    values = dict(
        api_key="k", api_secret="s",
        use_testnet=False, dry_run=False,
        enabled=True, confirm_live_listing_sniper="yes",
        quote_asset="USDT",
        poll_interval_seconds=5.0, notional_usdt=5.0,
        hold_mode="trailing", hold_seconds=60,
        trailing_stop_pct=15.0, max_hold_seconds=14_400,
        trailing_poll_interval_seconds=10,
        daily_spend_cap_usdt=50.0, min_balance_usdt=10.0,
        state_path=str(tmp_path / "state.json"),
        audit_path=str(tmp_path / "audit.jsonl"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        emergency_stop_path=str(tmp_path / "stop"),
    )
    values.update(overrides)
    s = ListingSniperSettings(**values)
    s.validate()
    return s


# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------


def test_settings_rejects_subsecond_poll(tmp_path: Path) -> None:
    with pytest.raises(ListingSniperError, match="poll_interval"):
        _settings(tmp_path, poll_interval_seconds=0.5)


def test_settings_rejects_zero_notional(tmp_path: Path) -> None:
    with pytest.raises(ListingSniperError, match="notional_usdt"):
        _settings(tmp_path, notional_usdt=0.0)


def test_settings_rejects_unknown_hold_mode(tmp_path: Path) -> None:
    with pytest.raises(ListingSniperError, match="hold_mode"):
        _settings(tmp_path, hold_mode="bogus")


def test_settings_rejects_zero_daily_cap(tmp_path: Path) -> None:
    with pytest.raises(ListingSniperError, match="daily_spend_cap"):
        _settings(tmp_path, daily_spend_cap_usdt=0.0)


# ---------------------------------------------------------------------------
# Preflight gate ladder
# ---------------------------------------------------------------------------


def test_preflight_passes_in_plan_only(tmp_path: Path) -> None:
    _preflight(_settings(tmp_path, enabled=False), submit_order=False)


def test_preflight_blocks_when_emergency_stop_present(tmp_path: Path) -> None:
    s = _settings(tmp_path)
    Path(s.emergency_stop_path).write_text("halt")
    with pytest.raises(ListingSniperError, match="emergency stop"):
        _preflight(s, submit_order=False)


def test_preflight_requires_enabled_for_live(tmp_path: Path) -> None:
    with pytest.raises(ListingSniperError, match="ENABLED"):
        _preflight(_settings(tmp_path, enabled=False), submit_order=True)


def test_preflight_requires_proven_edge_for_live(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    s = _settings(tmp_path)
    monkeypatch.delenv("LISTING_SNIPER_PROVEN_EDGE", raising=False)
    with pytest.raises(ListingSniperError, match="PROVEN_EDGE"):
        _preflight(s, submit_order=True)


def test_preflight_passes_live_when_all_gates_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    s = _settings(tmp_path)
    monkeypatch.setenv("LISTING_SNIPER_PROVEN_EDGE", "yes")
    _preflight(s, submit_order=True)


# ---------------------------------------------------------------------------
# Symbol diff
# ---------------------------------------------------------------------------


def test_first_cycle_returns_no_new_listings() -> None:
    """Bootstrap: the very first cycle must not fire on the whole universe."""
    state = ListingSniperState()
    assert state.bootstrapped is False
    new = compute_new_listings(state=state, current_symbols=["BTCUSDT", "ETHUSDT"])
    assert new == []


def test_subsequent_cycle_returns_new_symbols_only() -> None:
    state = ListingSniperState(
        known_trading_symbols={"BTCUSDT", "ETHUSDT"}, bootstrapped=True,
    )
    new = compute_new_listings(
        state=state, current_symbols=["BTCUSDT", "ETHUSDT", "NEWUSDT"],
    )
    assert new == ["NEWUSDT"]


def test_already_sniped_symbols_skipped_even_when_diff() -> None:
    state = ListingSniperState(
        known_trading_symbols={"BTCUSDT"}, bootstrapped=True,
        sniped_symbols={"OLDNEWUSDT"},
    )
    new = compute_new_listings(
        state=state, current_symbols=["BTCUSDT", "OLDNEWUSDT", "FRESHUSDT"],
    )
    assert new == ["FRESHUSDT"]


# ---------------------------------------------------------------------------
# Daily spend tracker
# ---------------------------------------------------------------------------


def test_daily_spend_blocks_above_cap() -> None:
    s = DailySpend(date_utc="2026-05-31", spent_usdt=40.0)
    assert s.would_exceed(15.0, cap=50.0) is True
    assert s.would_exceed(5.0, cap=50.0) is False


def test_daily_spend_resets_on_new_day(monkeypatch: pytest.MonkeyPatch) -> None:
    s = DailySpend(date_utc="1999-01-01", spent_usdt=999.0)
    s.reset_if_new_day()
    assert s.date_utc != "1999-01-01"
    assert s.spent_usdt == 0.0


# ---------------------------------------------------------------------------
# State round-trip
# ---------------------------------------------------------------------------


def test_state_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    state = ListingSniperState(
        known_trading_symbols={"BTCUSDT"}, bootstrapped=True,
        sniped_symbols={"NEWUSDT"},
    )
    state.daily_spend.spent_usdt = 12.34
    save_state(state, path)
    restored = load_state(path)
    assert restored.known_trading_symbols == {"BTCUSDT"}
    assert restored.bootstrapped is True
    assert restored.sniped_symbols == {"NEWUSDT"}
    assert restored.daily_spend.spent_usdt == pytest.approx(12.34)


def test_load_state_returns_empty_when_missing(tmp_path: Path) -> None:
    state = load_state(tmp_path / "nope.json")
    assert state.bootstrapped is False
    assert state.known_trading_symbols == set()


def test_load_state_raises_on_corrupt(tmp_path: Path) -> None:
    p = tmp_path / "state.json"
    p.write_text("not json")
    with pytest.raises(ListingSniperError, match="could not read"):
        load_state(p)


# ---------------------------------------------------------------------------
# exchangeInfo fetch (mocked HTTP)
# ---------------------------------------------------------------------------


class _FakeResp:
    def __init__(self, *, status: int, payload: dict | None = None,
                 text: str = "") -> None:
        self.status = status
        self._payload = payload
        self._text = text

    async def __aenter__(self): return self
    async def __aexit__(self, *a): return None
    async def json(self): return self._payload
    async def text(self): return self._text


class _FakeSession:
    def __init__(self, resp: _FakeResp) -> None:
        self._resp = resp
        self.requests: list[dict] = []

    def get(self, url, *, timeout=None):
        self.requests.append({"url": url, "timeout": timeout})
        return self._resp


@pytest.mark.asyncio
async def test_fetch_trading_symbols_filters_by_status_and_quote() -> None:
    payload = {
        "symbols": [
            {"symbol": "BTCUSDT", "status": "TRADING", "quoteAsset": "USDT"},
            {"symbol": "ETHUSDT", "status": "BREAK", "quoteAsset": "USDT"},
            {"symbol": "BTCUSDC", "status": "TRADING", "quoteAsset": "USDC"},
            {"symbol": "NEWUSDT", "status": "TRADING", "quoteAsset": "USDT"},
        ],
    }
    session = _FakeSession(_FakeResp(status=200, payload=payload))
    result = await fetch_trading_symbols_for_quote(
        session=session, quote_asset="USDT",
    )
    assert result == {"BTCUSDT", "NEWUSDT"}


@pytest.mark.asyncio
async def test_fetch_trading_symbols_raises_on_http_error() -> None:
    session = _FakeSession(_FakeResp(status=503, text="busy"))
    with pytest.raises(ListingSniperError, match="HTTP 503"):
        await fetch_trading_symbols_for_quote(session=session, quote_asset="USDT")


# ---------------------------------------------------------------------------
# Paper fire shape
# ---------------------------------------------------------------------------


class _ScriptedPriceSession:
    """Mock aiohttp.ClientSession that returns scripted prices via /ticker/price."""

    def __init__(self, prices: list[float]) -> None:
        self._prices = list(prices)
        self.requests: list[dict] = []

    def get(self, url, *, params=None, timeout=None):
        self.requests.append({"url": url, "params": params})

        class _Resp:
            def __init__(inner, status, payload):
                inner.status = status
                inner._payload = payload
            async def __aenter__(inner): return inner
            async def __aexit__(inner, *a): return None
            async def json(inner): return inner._payload

        if self._prices:
            price = self._prices.pop(0)
            return _Resp(200, {"price": str(price)})
        return _Resp(503, None)


@pytest.mark.asyncio
async def test_fire_paper_flat_mode_records_entry_and_exit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    s = _settings(tmp_path, notional_usdt=10.0, hold_mode="flat", hold_seconds=0)
    # Two prices: first call = entry, second call after sleep = exit.
    session = _ScriptedPriceSession([1.0, 1.10])
    result = await fire_paper(symbol="FOOUSDT", settings=s, session=session)
    assert result["mode"] == "paper"
    assert result["action"] == "round_trip_complete"
    assert result["entry_price"] == pytest.approx(1.0)
    assert result["exit_price"] == pytest.approx(1.10)
    # Gross +10%, minus 0.2% fee drag = 9.8%. On $10 notional → $0.98.
    assert result["gross_pnl_pct"] == pytest.approx(10.0)
    assert result["pnl_usdt"] == pytest.approx(0.98, abs=0.01)
    assert result["hold_mode"] == "flat"
    assert result["exit_reason"] == "flat_hold_elapsed"


@pytest.mark.asyncio
async def test_fire_paper_returns_skipped_when_entry_price_unavailable(
    tmp_path: Path,
) -> None:
    s = _settings(tmp_path, hold_mode="flat", hold_seconds=0)
    session = _ScriptedPriceSession([])  # No prices available
    result = await fire_paper(symbol="FOOUSDT", settings=s, session=session)
    assert result["action"] == "buy_skipped_no_price"


# ---------------------------------------------------------------------------
# Momentum exit logic
# ---------------------------------------------------------------------------


def _mom_settings(tmp_path: Path, **overrides) -> ListingSniperSettings:
    base = dict(hold_mode="momentum", momentum_negative_polls_to_exit=2,
                momentum_no_new_high_polls_to_exit=5,
                momentum_ma_short_samples=3, momentum_ma_long_samples=6,
                momentum_ma_cross_min_pullback_pct=5.0)
    base.update(overrides)
    return _settings(tmp_path, **base)


def test_momentum_does_not_exit_with_too_few_samples(tmp_path: Path) -> None:
    s = _mom_settings(tmp_path)
    samples = [(0.0, 1.0)]
    should, reason = momentum_should_exit(settings=s, samples=samples, peak=1.0)
    assert should is False


def test_momentum_exits_on_consecutive_negative_velocity(tmp_path: Path) -> None:
    s = _mom_settings(tmp_path, momentum_negative_polls_to_exit=2)
    # Climb, then two strictly-decreasing samples.
    samples = [(0.0, 1.0), (10.0, 1.20), (20.0, 1.10), (30.0, 1.05)]
    should, reason = momentum_should_exit(settings=s, samples=samples, peak=1.20)
    assert should is True
    assert "momentum_negative_for_2_polls" in reason


def test_momentum_no_exit_when_single_dip_then_recovery(tmp_path: Path) -> None:
    s = _mom_settings(tmp_path, momentum_negative_polls_to_exit=2)
    samples = [(0.0, 1.0), (10.0, 1.20), (20.0, 1.10), (30.0, 1.30)]
    should, _ = momentum_should_exit(settings=s, samples=samples, peak=1.30)
    assert should is False


def test_momentum_exits_when_no_new_high_for_n_polls(tmp_path: Path) -> None:
    s = _mom_settings(tmp_path, momentum_no_new_high_polls_to_exit=3,
                       momentum_negative_polls_to_exit=999,
                       momentum_ma_short_samples=2, momentum_ma_long_samples=99)
    # Peak at sample idx 1, then 3 polls at lower values without new high.
    samples = [(0.0, 1.0), (10.0, 2.0), (20.0, 1.5), (30.0, 1.6), (40.0, 1.7)]
    should, reason = momentum_should_exit(settings=s, samples=samples, peak=2.0)
    assert should is True
    assert "no_new_high" in reason


def test_momentum_ma_cross_requires_pullback(tmp_path: Path) -> None:
    s = _mom_settings(tmp_path, momentum_ma_cross_min_pullback_pct=10.0,
                       momentum_negative_polls_to_exit=999,
                       momentum_no_new_high_polls_to_exit=999)
    # Short MA below long MA, but only 1% pullback from peak.
    samples = [(0.0, 1.0), (1.0, 1.05), (2.0, 1.10), (3.0, 1.15),
                (4.0, 1.18), (5.0, 1.16)]
    should, reason = momentum_should_exit(settings=s, samples=samples, peak=1.18)
    # short_ma (last 3): (1.15+1.18+1.16)/3 = 1.163
    # long_ma  (last 6): (1.0+1.05+1.10+1.15+1.18+1.16)/6 = 1.107
    # short_ma > long_ma; no MA cross → no exit.
    assert should is False


def test_momentum_ma_cross_fires_with_sufficient_pullback(tmp_path: Path) -> None:
    s = _mom_settings(tmp_path, momentum_ma_cross_min_pullback_pct=5.0,
                       momentum_negative_polls_to_exit=999,
                       momentum_no_new_high_polls_to_exit=999)
    # Peak then sustained decline so short_ma < long_ma AND pullback > 5%.
    samples = [(0.0, 1.0), (1.0, 1.10), (2.0, 1.20), (3.0, 1.15),
                (4.0, 1.05), (5.0, 1.00)]
    # short_ma last 3: (1.15+1.05+1.00)/3 = 1.067
    # long_ma  last 6: (1+1.1+1.2+1.15+1.05+1.00)/6 = 1.083
    # short_ma < long_ma. Pullback: (1.20-1.00)/1.20 = 16.7% > 5%.
    should, reason = momentum_should_exit(settings=s, samples=samples, peak=1.20)
    assert should is True
    assert "ma_cross_with_pullback" in reason


# ---------------------------------------------------------------------------
# Settings: momentum validation
# ---------------------------------------------------------------------------


def test_settings_rejects_short_ma_geq_long_ma(tmp_path: Path) -> None:
    with pytest.raises(ListingSniperError, match="momentum_ma_short_samples"):
        _settings(tmp_path, hold_mode="momentum",
                  momentum_ma_short_samples=6, momentum_ma_long_samples=6)


def test_settings_accepts_momentum_mode(tmp_path: Path) -> None:
    s = _settings(tmp_path, hold_mode="momentum")
    assert s.hold_mode == "momentum"


# ---------------------------------------------------------------------------
# Hard stop loss (universal, active from tick 1)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_hard_stop_fires_inside_floor_for_flat_mode(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Rug-pull scenario: price craters during the floor period.

    Hold mode is "flat" with a 30s floor. We set the floor and poll
    interval to zero so the test runs in milliseconds, but conceptually:
    even though flat mode would normally sit through the whole hold
    without an exit check, the hard stop fires the moment price drops
    25% from entry.
    """
    s = _settings(tmp_path, hold_mode="flat", hold_seconds=30,
                  trailing_poll_interval_seconds=0, max_hold_seconds=600,
                  hard_stop_loss_pct=-25.0)
    # Entry 1.0, then a -30% dump on the very next poll → triggers hard stop.
    session = _ScriptedPriceSession([1.0, 0.70])
    result = await fire_paper(symbol="RUGUSDT", settings=s, session=session)
    assert result["action"] == "round_trip_complete"
    assert "hard_stop_loss_hit" in result["exit_reason"]
    # Net P&L: gross -30% minus 0.2% fee = -30.2%.
    assert result["gross_pnl_pct"] == pytest.approx(-30.0)
    assert result["net_pnl_pct"] == pytest.approx(-30.2)


@pytest.mark.asyncio
async def test_hard_stop_fires_inside_floor_for_momentum_mode(
    tmp_path: Path,
) -> None:
    s = _settings(tmp_path, hold_mode="momentum", hold_seconds=30,
                  trailing_poll_interval_seconds=0, max_hold_seconds=600,
                  hard_stop_loss_pct=-25.0)
    session = _ScriptedPriceSession([1.0, 0.50])  # -50% dump
    result = await fire_paper(symbol="RUGUSDT", settings=s, session=session)
    assert "hard_stop_loss_hit" in result["exit_reason"]


@pytest.mark.asyncio
async def test_hard_stop_inactive_when_pct_zero(tmp_path: Path) -> None:
    """Setting hard_stop_loss_pct=0 disables the hard stop entirely."""
    s = _settings(tmp_path, hold_mode="flat", hold_seconds=0,
                  trailing_poll_interval_seconds=0, max_hold_seconds=600,
                  hard_stop_loss_pct=0.0)
    session = _ScriptedPriceSession([1.0, 0.30])  # -70% dump
    result = await fire_paper(symbol="RUGUSDT", settings=s, session=session)
    # Even though price cratered 70%, hard stop was disabled, so flat mode
    # falls through to the post-floor flat_hold_elapsed exit.
    assert result["exit_reason"] == "flat_hold_elapsed"
    assert result["gross_pnl_pct"] == pytest.approx(-70.0)


@pytest.mark.asyncio
async def test_normal_dip_inside_floor_does_not_fire_hard_stop(
    tmp_path: Path,
) -> None:
    """A 10% dip inside the floor should NOT trigger the -25% hard stop."""
    s = _settings(tmp_path, hold_mode="flat", hold_seconds=0,
                  trailing_poll_interval_seconds=0, max_hold_seconds=600,
                  hard_stop_loss_pct=-25.0)
    # Entry 1.0, small 10% dip, then recover to 0.95 (the exit price).
    session = _ScriptedPriceSession([1.0, 0.90, 0.95])
    result = await fire_paper(symbol="DIPUSDT", settings=s, session=session)
    assert "hard_stop" not in result["exit_reason"]
    # Falls through to flat_hold_elapsed at the first post-floor poll.
    assert result["exit_reason"] == "flat_hold_elapsed"


# ---------------------------------------------------------------------------
# fire_live: stale-market-cache retry on ccxt BadSymbol
#
# On 2026-06-11 four live fires (CRCLB, NVDAB, SNDKB, TSLAB) failed with
# "BadSymbol: binance does not have market symbol X/USDT" — ccxt's market
# cache was populated at daemon startup and didn't include the brand-new
# listings. fix: catch BadSymbol, reload markets, retry once.
# ---------------------------------------------------------------------------


class BadSymbol(Exception):
    """Stand-in for ccxt.BadSymbol — class name matches what _is_unknown_symbol_error checks."""


class _MockSpotClient:
    """Minimal async ccxt-shaped client for fire_live tests."""

    def __init__(self, buy_outcomes):
        # buy_outcomes: list of either Exception instances to raise, or dict
        # to return — popped in order on each create_order(buy) call.
        self._buy_outcomes = list(buy_outcomes)
        self.create_order_calls = 0
        self.load_markets_calls = []  # list of reload-flag values

    async def create_order(self, **kwargs):
        if kwargs.get("side") == "buy":
            self.create_order_calls += 1
            outcome = self._buy_outcomes.pop(0)
            if isinstance(outcome, Exception):
                raise outcome
            return outcome
        # sell or anything else — return a benign closed order
        return {"id": "x", "filled": 1.0, "average": 1.0, "status": "closed"}

    async def load_markets(self, reload=False):
        self.load_markets_calls.append(reload)
        return {}


def test_is_unknown_symbol_error_detects_bad_symbol():
    assert _is_unknown_symbol_error(BadSymbol("anything"))


def test_is_unknown_symbol_error_detects_message_pattern():
    assert _is_unknown_symbol_error(
        RuntimeError("binance does not have market symbol TSLAB/USDT")
    )


def test_is_unknown_symbol_error_ignores_other_failures():
    assert not _is_unknown_symbol_error(RuntimeError("insufficient balance"))
    assert not _is_unknown_symbol_error(ValueError("filter failure: notional"))


@pytest.mark.asyncio
async def test_fire_live_reloads_markets_and_retries_on_bad_symbol(tmp_path: Path):
    """Stale market cache → reload + retry → buy succeeds (returns zero_fill is fine).

    We don't care about the post-buy path here; the regression we're protecting
    against is that create_order is called twice with a load_markets(reload=True)
    in between.
    """
    s = _settings(tmp_path, hold_mode="flat", hold_seconds=0,
                  trailing_poll_interval_seconds=0, max_hold_seconds=0)
    # First attempt raises BadSymbol; second attempt returns a 0-fill order
    # (which makes fire_live return early without entering the hold loop).
    client = _MockSpotClient([
        BadSymbol("binance does not have market symbol TSLAB/USDT"),
        {"id": "1", "filled": 0.0, "average": 0.0, "status": "closed"},
    ])
    result = await fire_live(symbol="TSLABUSDT", settings=s, spot_client=client)
    assert client.create_order_calls == 2
    assert client.load_markets_calls == [True]  # called once with reload=True
    assert result["action"] == "buy_zero_fill"
    assert result["symbol"] == "TSLAB/USDT"


@pytest.mark.asyncio
async def test_fire_live_does_not_retry_on_non_symbol_error(tmp_path: Path):
    """Non-stale-cache errors (e.g. NOTIONAL filter) should NOT trigger reload."""
    s = _settings(tmp_path, hold_mode="flat", hold_seconds=0)
    client = _MockSpotClient([
        RuntimeError('binance {"code":-1013,"msg":"Filter failure: NOTIONAL"}'),
    ])
    result = await fire_live(symbol="MUBUSDT", settings=s, spot_client=client)
    assert client.create_order_calls == 1   # no retry
    assert client.load_markets_calls == []  # no reload
    assert result["action"] == "buy_failed"
    assert "NOTIONAL" in result["reason"]


def test_format_live_fire_alert_round_trip_win(tmp_path: Path):
    s = _settings(tmp_path, hold_mode="momentum")
    msg = _format_live_fire_alert({
        "action": "round_trip_complete", "symbol": "FOO/USDT",
        "avg_buy": 1.0, "avg_sell": 1.10, "pnl_quote": 0.50,
        "exit_reason": "trailing_stop_hit",
    }, s)
    assert "💰" in msg
    assert "FOO/USDT" in msg
    assert "+10.00%" in msg
    assert "trailing_stop_hit" in msg


def test_format_live_fire_alert_buy_failed(tmp_path: Path):
    s = _settings(tmp_path, hold_mode="momentum")
    msg = _format_live_fire_alert({
        "action": "buy_failed", "symbol": "TSLAB/USDT",
        "reason": "BadSymbol: market not found",
    }, s)
    assert "❌" in msg
    assert "TSLAB/USDT" in msg
    assert "BadSymbol" in msg


def test_format_live_fire_alert_sell_stranded(tmp_path: Path):
    s = _settings(tmp_path)
    msg = _format_live_fire_alert({
        "action": "sell_failed", "symbol": "X/USDT",
        "reason": "network timeout",
    }, s)
    assert "⚠️" in msg
    assert "STRANDED" in msg


@pytest.mark.asyncio
async def test_fire_live_reports_post_reload_failure_distinctly(tmp_path: Path):
    """If buy still fails after a market reload, the reason is tagged so we know."""
    s = _settings(tmp_path, hold_mode="flat", hold_seconds=0)
    client = _MockSpotClient([
        BadSymbol("binance does not have market symbol GHOSTUSDT"),
        BadSymbol("binance does not have market symbol GHOSTUSDT"),
    ])
    result = await fire_live(symbol="GHOSTUSDT", settings=s, spot_client=client)
    assert client.create_order_calls == 2
    assert client.load_markets_calls == [True]
    assert result["action"] == "buy_failed"
    assert result["reason"].startswith("after_market_reload:")

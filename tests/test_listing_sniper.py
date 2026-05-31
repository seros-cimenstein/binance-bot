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
    _preflight,
    compute_new_listings,
    fetch_trading_symbols_for_quote,
    fire_paper,
    load_state,
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


@pytest.mark.asyncio
async def test_fire_paper_returns_would_buy_record(tmp_path: Path) -> None:
    s = _settings(tmp_path, notional_usdt=7.5)
    result = await fire_paper(symbol="FOOUSDT", settings=s)
    assert result["mode"] == "paper"
    assert result["action"] == "would_buy"
    assert result["symbol"] == "FOOUSDT"
    assert result["notional_usdt"] == 7.5

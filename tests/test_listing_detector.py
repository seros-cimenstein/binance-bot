"""Tests for the Binance listing announcement detector."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import pytest

from src.listing_detector import (
    DetectorState,
    ListingDetectorError,
    ListingDetectorSettings,
    ListingEvent,
    classify_title,
    detect_new_listings,
    extract_ticker,
    extract_tickers,
    load_state,
    parse_announcement,
    save_state,
)


def _article(article_id: int, title: str, release_time_ms: int = 0) -> dict:
    return {"id": article_id, "title": title, "releaseDate": release_time_ms}


# ---------------------------------------------------------------------------
# Title classification
# ---------------------------------------------------------------------------


def test_classify_spot_list_title() -> None:
    assert classify_title("Binance Will List Gensyn (AIGENSYN) with Seed Tag Applied") == "spot_list"


def test_classify_futures_title_takes_precedence_over_will_list() -> None:
    # The string "Binance Futures Will Launch" should classify as futures, not spot.
    title = "Binance Futures Will Launch QNTXUSDT USDⓈ-Margined Perpetual Contract"
    assert classify_title(title) == "futures_only"


def test_classify_spot_add_title() -> None:
    title = "Binance Will Add Gensyn (AIGENSYN) on Earn, Buy Crypto, Convert, VIP Loan & Margin"
    assert classify_title(title) == "spot_add"


def test_classify_hodler_airdrop_title() -> None:
    title = "Introducing Genius Terminal (GENIUS) on Binance HODLer Airdrops!"
    assert classify_title(title) == "hodler_airdrop"


def test_classify_other_title() -> None:
    assert classify_title("Binance Daily Market Wrap") == "other"


# ---------------------------------------------------------------------------
# Ticker extraction
# ---------------------------------------------------------------------------


def test_extract_ticker_from_parenthesized_symbol() -> None:
    assert extract_ticker("Binance Will List Gensyn (AIGENSYN) with Seed Tag") == "AIGENSYN"


def test_extract_ticker_skips_common_non_tickers() -> None:
    # UTC and USDT are blocklisted; the real ticker is FOO.
    title = "Foo (FOO) listing at 12:00 (UTC) on the (USDT) market"
    assert extract_ticker(title) == "FOO"


def test_extract_ticker_returns_none_when_missing() -> None:
    assert extract_ticker("Binance Daily Market Wrap") is None


# ---------------------------------------------------------------------------
# Announcement parsing
# ---------------------------------------------------------------------------


def test_parse_announcement_extracts_everything() -> None:
    article = _article(123, "Binance Will List Gensyn (AIGENSYN)", release_time_ms=999)
    event = parse_announcement(article)
    assert event.article_id == 123
    assert event.detected_token == "AIGENSYN"
    assert event.listing_type == "spot_list"
    assert event.release_time_ms == 999


def test_parse_announcement_handles_missing_fields() -> None:
    event = parse_announcement({})
    assert event.article_id == 0
    assert event.detected_token is None
    assert event.listing_type == "other"


# ---------------------------------------------------------------------------
# Detection (cursor + ordering)
# ---------------------------------------------------------------------------


def test_detect_new_listings_filters_already_seen() -> None:
    articles = [
        _article(100, "Binance Will List Old (OLD)", release_time_ms=10),
        _article(200, "Binance Will List New (NEW)", release_time_ms=20),
    ]
    new = detect_new_listings(articles, last_seen_article_id=100)
    assert [e.article_id for e in new] == [200]


def test_detect_new_listings_returns_empty_when_all_seen() -> None:
    articles = [_article(50, "Binance Will List Foo (FOO)")]
    assert detect_new_listings(articles, last_seen_article_id=50) == []


def test_detect_new_listings_sorts_oldest_first() -> None:
    articles = [
        _article(300, "Binance Will List Cee (CEE)", release_time_ms=30),
        _article(100, "Binance Will List Aye (AYE)", release_time_ms=10),
        _article(200, "Binance Will List Bee (BEE)", release_time_ms=20),
    ]
    new = detect_new_listings(articles, last_seen_article_id=0)
    assert [e.detected_token for e in new] == ["AYE", "BEE", "CEE"]


def test_detect_new_listings_keeps_non_spot_for_audit() -> None:
    # Detector returns ALL fresh articles; classification is the caller's job.
    articles = [
        _article(100, "Binance Will List Foo (FOO)"),
        _article(200, "Binance Futures Will Launch BarPerp"),
    ]
    new = detect_new_listings(articles, last_seen_article_id=0)
    types = sorted(e.listing_type for e in new)
    assert types == ["futures_only", "spot_list"]


# ---------------------------------------------------------------------------
# State persistence
# ---------------------------------------------------------------------------


def test_state_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    save_state(DetectorState(last_seen_article_id=12345, last_poll_time_ms=999), path)
    restored = load_state(path)
    assert restored.last_seen_article_id == 12345
    assert restored.last_poll_time_ms == 999


def test_state_load_missing_file_returns_zero_cursor(tmp_path: Path) -> None:
    state = load_state(tmp_path / "nonexistent.json")
    assert state.last_seen_article_id == 0


def test_state_load_corrupt_file_raises(tmp_path: Path) -> None:
    p = tmp_path / "state.json"
    p.write_text("not valid json")
    with pytest.raises(ListingDetectorError, match="could not read state"):
        load_state(p)


# ---------------------------------------------------------------------------
# Multi-ticker extraction (Phase 2: capture every ticker in the title)
# ---------------------------------------------------------------------------


def test_extract_tickers_handles_multi_token_announcement() -> None:
    title = "Binance Will List Genius Terminal (GENIUS) and OpenGradient (OPG) with Seed Tag Applied"
    assert extract_tickers(title) == ("GENIUS", "OPG")


def test_extract_tickers_dedupes_repeats() -> None:
    title = "Foo (FOO) on FOO/(FOO) pair"
    assert extract_tickers(title) == ("FOO",)


def test_extract_tickers_returns_empty_for_no_tickers() -> None:
    assert extract_tickers("Binance Daily Wrap") == ()


def test_extract_tickers_filters_blocklisted_abbreviations() -> None:
    title = "Foo (FOO) launches at 12:00 (UTC), priced in (USDT)"
    assert extract_tickers(title) == ("FOO",)


def test_extract_ticker_backcompat_returns_first_token() -> None:
    title = "Binance Will List Genius (GENIUS) and OpenGradient (OPG)"
    assert extract_ticker(title) == "GENIUS"


def test_parse_announcement_populates_tokens_tuple() -> None:
    article = _article(1, "Binance Will List Genius (GENIUS) and OpenGradient (OPG)")
    event = parse_announcement(article)
    assert event.detected_tokens == ("GENIUS", "OPG")
    # Back-compat property still works.
    assert event.detected_token == "GENIUS"
    # Pairs are empty until run_once resolves them on the exchange.
    assert event.detected_pairs == ()
    assert event.detected_pair is None


# ---------------------------------------------------------------------------
# Trailing-stop hold helper
# ---------------------------------------------------------------------------


def _settings(**overrides) -> ListingDetectorSettings:
    base = dict(hold_mode="flat", hold_seconds=0, trailing_stop_pct=15.0,
                max_hold_seconds=600, trailing_poll_interval_seconds=0)
    base.update(overrides)
    return ListingDetectorSettings(**base)


class _PriceFeedClient:
    """Mock spot client whose ticker walks through a configured price series."""

    def __init__(self, prices: list[float]) -> None:
        self._prices = list(prices)

    async def fetch_ticker(self, symbol: str):
        return {"last": self._prices.pop(0)} if self._prices else {"last": 0.0}


@pytest.mark.asyncio
async def test_hold_then_sell_flat_mode_returns_elapsed_and_reason() -> None:
    from src.listing_detector import _hold_then_sell_price
    s = _settings(hold_mode="flat", hold_seconds=0)
    elapsed, reason = await _hold_then_sell_price(
        settings=s, spot_client=_PriceFeedClient([]),
        sym="X/USDT", avg_buy=1.0,
    )
    assert elapsed == 0
    assert reason == "flat_hold_elapsed"


@pytest.mark.asyncio
async def test_hold_then_sell_trailing_exits_on_retrace_from_peak() -> None:
    from src.listing_detector import _hold_then_sell_price
    # Price climbs 1.0 -> 1.5 (peak), then drops to 1.20 which is 20% below
    # peak. Trailing stop at 15% should fire.
    s = _settings(hold_mode="trailing", hold_seconds=0,
                  trailing_stop_pct=15.0, trailing_poll_interval_seconds=0,
                  max_hold_seconds=1_000_000)
    client = _PriceFeedClient([1.0, 1.5, 1.2])  # first call seeds peak; second + third poll
    elapsed, reason = await _hold_then_sell_price(
        settings=s, spot_client=client, sym="X/USDT", avg_buy=1.0,
    )
    assert reason.startswith("trailing_stop_hit")


@pytest.mark.asyncio
async def test_hold_then_sell_trailing_respects_max_hold_cap() -> None:
    from src.listing_detector import _hold_then_sell_price
    # Price keeps climbing (never retraces) but max_hold is short.
    s = _settings(hold_mode="trailing", hold_seconds=0,
                  trailing_stop_pct=15.0, trailing_poll_interval_seconds=1,
                  max_hold_seconds=3)
    climbing = [1.0, 1.01, 1.02, 1.03, 1.04, 1.05]
    elapsed, reason = await _hold_then_sell_price(
        settings=s, spot_client=_PriceFeedClient(climbing),
        sym="X/USDT", avg_buy=1.0,
    )
    assert reason == "max_hold_seconds_reached"


@pytest.mark.asyncio
async def test_hold_then_sell_rejects_unknown_mode() -> None:
    from src.listing_detector import _hold_then_sell_price
    s = _settings(hold_mode="bogus", hold_seconds=0)
    with pytest.raises(ListingDetectorError, match="unknown hold_mode"):
        await _hold_then_sell_price(
            settings=s, spot_client=_PriceFeedClient([]),
            sym="X/USDT", avg_buy=1.0,
        )


# ---------------------------------------------------------------------------
# _execute_event_live now takes one pair at a time
# ---------------------------------------------------------------------------


class _RecordingSpotClient(_PriceFeedClient):
    def __init__(self, prices: list[float]) -> None:
        super().__init__(prices)
        self.create_order_calls: list[dict] = []

    async def create_order(self, *, symbol, type, side, amount, price=None, params=None):
        self.create_order_calls.append(
            {"symbol": symbol, "type": type, "side": side,
             "amount": amount, "params": params}
        )
        if side == "buy":
            return {"id": "buy1", "filled": 10.0, "average": 1.0}
        return {"id": "sell1", "filled": amount, "average": 1.5}


@pytest.mark.asyncio
async def test_execute_event_live_runs_one_pair_through_flat_mode() -> None:
    from src.listing_detector import _execute_event_live
    s = _settings(hold_mode="flat", hold_seconds=0, notional_usdt=15.0)
    client = _RecordingSpotClient([])
    event = ListingEvent(
        article_id=1, title="x", release_time_ms=0, listing_type="spot_list",
        detected_tokens=("AIGENSYN",), detected_pairs=("AIGENSYN/USDT",),
    )
    result = await _execute_event_live(
        event=event, settings=s, spot_client=client, pair="AIGENSYN/USDT",
    )
    assert result["action"] == "round_trip_complete"
    assert result["symbol"] == "AIGENSYN/USDT"
    assert result["hold_mode"] == "flat"
    # Buy used quoteOrderQty; sell used base amount.
    assert client.create_order_calls[0]["params"] == {"quoteOrderQty": 15.0}
    assert client.create_order_calls[1]["side"] == "sell"

"""Tests for the Binance listing announcement detector."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.listing_detector import (
    DetectorState,
    ListingDetectorError,
    classify_title,
    detect_new_listings,
    extract_ticker,
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

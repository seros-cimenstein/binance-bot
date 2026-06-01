"""Tests for the shared listing watchlist and the detector/sniper handoff."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from src.listing_watchlist import (
    DEFAULT_EXPIRY_DAYS,
    WatchEntry,
    Watchlist,
    WatchlistError,
    build_watch_entry,
    load_watchlist,
    save_watchlist,
)
from src.listing_sniper import (
    ListingSniperState,
    arm_from_watchlist,
    compute_new_listings,
)


# ---------------------------------------------------------------------------
# WatchEntry + Watchlist primitives
# ---------------------------------------------------------------------------


def _now() -> datetime:
    return datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)


def _entry(token="FOO", article_id=1, *, days_ago=0, expiry_days=7) -> WatchEntry:
    announced = _now() - timedelta(days=days_ago)
    expires = announced + timedelta(days=expiry_days)
    return WatchEntry(
        token=token, article_id=article_id,
        announced_at_iso=announced.isoformat(),
        expires_at_iso=expires.isoformat(),
    )


def test_build_watch_entry_sets_expiry_to_announce_plus_default_days() -> None:
    entry = build_watch_entry(
        token="FOO", article_id=42, announced_at_ms=int(_now().timestamp() * 1000),
    )
    a = datetime.fromisoformat(entry.announced_at_iso)
    e = datetime.fromisoformat(entry.expires_at_iso)
    assert (e - a).days == DEFAULT_EXPIRY_DAYS


def test_watchlist_add_returns_false_for_duplicate_article_id() -> None:
    wl = Watchlist()
    assert wl.add(_entry(token="FOO", article_id=1)) is True
    assert wl.add(_entry(token="FOO", article_id=1)) is False
    # And the second add did NOT append.
    assert len(wl.entries) == 1


def test_watchlist_add_accepts_different_article_ids_for_same_token() -> None:
    wl = Watchlist()
    wl.add(_entry(token="FOO", article_id=1))
    wl.add(_entry(token="FOO", article_id=2))
    assert len(wl.entries) == 2


def test_watchlist_prune_drops_expired_entries() -> None:
    wl = Watchlist()
    wl.add(_entry(token="OLD", article_id=1, days_ago=10, expiry_days=7))   # expired
    wl.add(_entry(token="FRESH", article_id=2, days_ago=1, expiry_days=7))  # alive
    removed = wl.prune_expired(now=_now())
    assert removed == 1
    assert [e.token for e in wl.entries] == ["FRESH"]


def test_watchlist_remove_token_drops_all_matching() -> None:
    wl = Watchlist()
    wl.add(_entry(token="FOO", article_id=1))
    wl.add(_entry(token="FOO", article_id=2))
    wl.add(_entry(token="BAR", article_id=3))
    removed = wl.remove_token("FOO")
    assert removed == 2
    assert [e.token for e in wl.entries] == ["BAR"]


def test_watchlist_round_trips_through_disk(tmp_path: Path) -> None:
    path = tmp_path / "watchlist.json"
    wl = Watchlist()
    wl.add(_entry(token="FOO", article_id=1))
    save_watchlist(wl, path)
    loaded = load_watchlist(path, prune=False)
    assert [e.token for e in loaded.entries] == ["FOO"]


def test_load_watchlist_missing_file_returns_empty(tmp_path: Path) -> None:
    loaded = load_watchlist(tmp_path / "nope.json")
    assert loaded.entries == []


def test_load_watchlist_malformed_raises(tmp_path: Path) -> None:
    p = tmp_path / "wl.json"
    p.write_text("not json")
    with pytest.raises(WatchlistError):
        load_watchlist(p)


def test_load_watchlist_prunes_by_default(tmp_path: Path) -> None:
    path = tmp_path / "wl.json"
    wl = Watchlist()
    wl.add(_entry(token="OLD", article_id=1, days_ago=10, expiry_days=7))
    save_watchlist(wl, path)
    loaded = load_watchlist(path, now=_now())
    assert loaded.entries == []  # expired pruned on load


# ---------------------------------------------------------------------------
# sniper-side: arm_from_watchlist
# ---------------------------------------------------------------------------


def test_arm_from_watchlist_adds_token_pair_to_watched_pending(tmp_path: Path) -> None:
    path = tmp_path / "wl.json"
    wl = Watchlist()
    wl.add(_entry(token="AIGENSYN", article_id=274005, days_ago=1))
    save_watchlist(wl, path)

    state = ListingSniperState(bootstrapped=True)
    newly = arm_from_watchlist(state=state, watchlist_path=str(path),
                                 quote_asset="USDT")
    assert newly == ["AIGENSYNUSDT"]
    assert "AIGENSYNUSDT" in state.watched_pending
    assert 274005 in state.processed_article_ids


def test_arm_from_watchlist_skips_already_processed_articles(tmp_path: Path) -> None:
    path = tmp_path / "wl.json"
    wl = Watchlist()
    wl.add(_entry(token="FOO", article_id=1, days_ago=1))
    save_watchlist(wl, path)

    state = ListingSniperState(bootstrapped=True, processed_article_ids={1})
    newly = arm_from_watchlist(state=state, watchlist_path=str(path),
                                 quote_asset="USDT")
    assert newly == []
    assert state.watched_pending == set()


def test_arm_from_watchlist_skips_already_sniped_symbols(tmp_path: Path) -> None:
    path = tmp_path / "wl.json"
    wl = Watchlist()
    wl.add(_entry(token="FOO", article_id=1, days_ago=1))
    save_watchlist(wl, path)

    state = ListingSniperState(bootstrapped=True, sniped_symbols={"FOOUSDT"})
    newly = arm_from_watchlist(state=state, watchlist_path=str(path),
                                 quote_asset="USDT")
    assert newly == []
    # Article ID still recorded so we don't re-check it next cycle.
    assert 1 in state.processed_article_ids


def test_arm_from_watchlist_respects_quote_asset(tmp_path: Path) -> None:
    path = tmp_path / "wl.json"
    wl = Watchlist()
    wl.add(_entry(token="FOO", article_id=1, days_ago=1))
    save_watchlist(wl, path)

    state = ListingSniperState(bootstrapped=True)
    newly = arm_from_watchlist(state=state, watchlist_path=str(path),
                                 quote_asset="USDC")
    assert newly == ["FOOUSDC"]
    assert "FOOUSDC" in state.watched_pending


def test_arm_from_watchlist_handles_missing_file(tmp_path: Path) -> None:
    state = ListingSniperState(bootstrapped=True)
    newly = arm_from_watchlist(state=state,
                                 watchlist_path=str(tmp_path / "nope.json"),
                                 quote_asset="USDT")
    assert newly == []


# ---------------------------------------------------------------------------
# sniper-side: compute_new_listings now considers watched_pending
# ---------------------------------------------------------------------------


def test_compute_fires_on_watched_pair_already_in_known_set() -> None:
    """The whole point of the watchlist: a pair that bootstrap captured
    as 'already known' still fires once it's on the watched_pending set."""
    state = ListingSniperState(
        bootstrapped=True,
        known_trading_symbols={"AIGENSYNUSDT", "BTCUSDT", "ETHUSDT"},
        watched_pending={"AIGENSYNUSDT"},
    )
    fresh = compute_new_listings(
        state=state,
        current_symbols={"AIGENSYNUSDT", "BTCUSDT", "ETHUSDT"},
    )
    assert fresh == ["AIGENSYNUSDT"]


def test_compute_does_not_fire_on_watched_pair_not_yet_trading() -> None:
    """Watched but not yet in exchangeInfo → don't fire."""
    state = ListingSniperState(
        bootstrapped=True,
        known_trading_symbols={"BTCUSDT"},
        watched_pending={"NEWCOINUSDT"},
    )
    fresh = compute_new_listings(state=state,
                                   current_symbols={"BTCUSDT"})
    assert fresh == []


def test_compute_unions_watched_with_normal_diff() -> None:
    """A symbol can fire either by being a NEW unknown OR by being on the
    watchlist. Both paths coexist."""
    state = ListingSniperState(
        bootstrapped=True,
        known_trading_symbols={"BTCUSDT", "AIGENSYNUSDT"},
        watched_pending={"AIGENSYNUSDT"},
    )
    fresh = compute_new_listings(
        state=state,
        current_symbols={"BTCUSDT", "AIGENSYNUSDT", "FRESHCOINUSDT"},
    )
    # FRESHCOINUSDT is a real new diff; AIGENSYNUSDT is the watchlist hit.
    assert fresh == ["AIGENSYNUSDT", "FRESHCOINUSDT"]


def test_compute_excludes_already_sniped_from_both_paths() -> None:
    state = ListingSniperState(
        bootstrapped=True,
        known_trading_symbols={"BTCUSDT"},
        watched_pending={"FOOUSDT"},
        sniped_symbols={"FOOUSDT"},
    )
    fresh = compute_new_listings(
        state=state, current_symbols={"BTCUSDT", "FOOUSDT"},
    )
    assert fresh == []


def test_state_round_trips_watched_pending_and_processed_ids(tmp_path: Path) -> None:
    from src.listing_sniper import load_state, save_state
    state = ListingSniperState(
        bootstrapped=True,
        watched_pending={"FOOUSDT", "BARUSDT"},
        processed_article_ids={1, 2, 3},
    )
    path = tmp_path / "state.json"
    save_state(state, path)
    loaded = load_state(path)
    assert loaded.watched_pending == {"FOOUSDT", "BARUSDT"}
    assert loaded.processed_article_ids == {1, 2, 3}

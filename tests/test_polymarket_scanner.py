"""Tests for the Polymarket binary-arbitrage scanner."""

from __future__ import annotations

import pytest

from src.polymarket.client import (
    MarketToken,
    Orderbook,
    OrderbookLevel,
    SimplifiedMarket,
)
from src.polymarket.scanner import (
    ArbOpportunity,
    ScanConfig,
    _round_trip_fee_per_pair,
    evaluate_market,
)


def _market() -> SimplifiedMarket:
    return SimplifiedMarket(
        condition_id="0xabc",
        tokens=(
            MarketToken(token_id="YES", outcome="Yes", last_price=0.5),
            MarketToken(token_id="NO", outcome="No", last_price=0.5),
        ),
        accepting_orders=True, closed=False,
    )


def _book(*, bid: float, ask: float, bid_size: float = 100.0, ask_size: float = 100.0) -> Orderbook:
    return Orderbook(
        market="x", asset_id="y", timestamp_ms=0,
        bids=(OrderbookLevel(price=bid, size=bid_size),) if bid > 0 else (),
        asks=(OrderbookLevel(price=ask, size=ask_size),) if ask > 0 else (),
    )


def test_round_trip_fee_doubles_per_pair() -> None:
    # 10 bps per leg → 20 bps = 0.002 per pair
    assert _round_trip_fee_per_pair(10.0) == pytest.approx(0.002)


def test_evaluate_returns_none_for_non_binary_market() -> None:
    m = SimplifiedMarket(condition_id="x", tokens=(), accepting_orders=True, closed=False)
    assert evaluate_market(m, {}, cfg=ScanConfig()) is None


def test_evaluate_returns_none_when_orderbook_missing() -> None:
    m = _market()
    # Only YES has a book.
    assert evaluate_market(m, {"YES": _book(bid=0.4, ask=0.5)}, cfg=ScanConfig()) is None


def test_evaluate_finds_buy_both_opportunity() -> None:
    m = _market()
    # ask_yes 0.40 + ask_no 0.50 = 0.90 < 1.0 → 10¢ gross arb (minus 0.2¢ fees = 9.8¢ net)
    books = {
        "YES": _book(bid=0.30, ask=0.40, bid_size=100, ask_size=50),
        "NO":  _book(bid=0.40, ask=0.50, bid_size=80, ask_size=60),
    }
    opp = evaluate_market(m, books, cfg=ScanConfig(min_size_per_leg=5, min_gross_profit=0.005))
    assert opp is not None
    assert opp.direction == "buy_both"
    assert opp.buy_both_gross_profit == pytest.approx(0.10)
    assert opp.buy_both_net_profit == pytest.approx(0.098, abs=0.0005)
    assert opp.max_executable_pairs == 50  # min(50, 60)


def test_evaluate_finds_sell_both_opportunity() -> None:
    m = _market()
    # bid_yes 0.55 + bid_no 0.50 = 1.05 → 5¢ sell-both arb
    books = {
        "YES": _book(bid=0.55, ask=0.65, bid_size=40, ask_size=10),
        "NO":  _book(bid=0.50, ask=0.60, bid_size=30, ask_size=10),
    }
    opp = evaluate_market(m, books, cfg=ScanConfig(min_size_per_leg=5))
    assert opp is not None
    assert opp.direction == "sell_both"
    assert opp.sell_both_gross_profit == pytest.approx(0.05)
    assert opp.max_executable_pairs == 30  # min(40, 30) on bid sides


def test_evaluate_classifies_no_opportunity_when_book_in_line() -> None:
    m = _market()
    books = {
        "YES": _book(bid=0.45, ask=0.55),
        "NO":  _book(bid=0.45, ask=0.55),
    }
    opp = evaluate_market(m, books, cfg=ScanConfig())
    assert opp is not None and opp.direction == "none"


def test_evaluate_rejects_when_size_below_minimum() -> None:
    m = _market()
    # Same prices as the buy_both case but tiny size.
    books = {
        "YES": _book(bid=0.30, ask=0.40, ask_size=2),
        "NO":  _book(bid=0.40, ask=0.50, ask_size=3),
    }
    opp = evaluate_market(m, books, cfg=ScanConfig(min_size_per_leg=10))
    assert opp is not None and opp.direction == "none"


def test_evaluate_rejects_when_gross_below_threshold() -> None:
    m = _market()
    # 1 - (0.495 + 0.500) = 0.005 gross, below default 0.005 strict threshold (>)
    books = {
        "YES": _book(bid=0.40, ask=0.495),
        "NO":  _book(bid=0.40, ask=0.500),
    }
    opp = evaluate_market(m, books, cfg=ScanConfig(min_gross_profit=0.01))
    assert opp is not None and opp.direction == "none"

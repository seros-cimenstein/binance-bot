"""Tests for the Polymarket CLOB client."""

from __future__ import annotations

from typing import Any

import pytest

from src.polymarket.client import (
    Orderbook,
    OrderbookLevel,
    PolymarketAPIError,
    SimplifiedMarket,
    _parse_orderbook,
    _parse_simplified_market,
    fetch_active_markets_page,
    fetch_orderbook,
    fetch_orderbooks,
    iter_active_markets,
)


class _FakeResp:
    def __init__(self, *, status: int, json_payload: Any = None, text_payload: str = "") -> None:
        self.status = status
        self._json = json_payload
        self._text = text_payload

    async def __aenter__(self): return self
    async def __aexit__(self, *a): return None
    async def json(self): return self._json
    async def text(self): return self._text


class _FakeSession:
    def __init__(self, responses_by_path) -> None:
        self.responses_by_path = responses_by_path
        self.requests: list[dict] = []

    def get(self, url, *, params=None, timeout=None):
        self.requests.append({"url": url, "params": params, "timeout": timeout})
        for path, resp in self.responses_by_path.items():
            if path in url:
                if callable(resp):
                    return resp(params or {})
                if isinstance(resp, list):
                    return resp.pop(0)
                return resp
        return _FakeResp(status=404)


def test_parse_simplified_market_extracts_tokens() -> None:
    payload = {
        "condition_id": "0xabc",
        "tokens": [
            {"token_id": "T1", "outcome": "Yes", "price": 0.42},
            {"token_id": "T2", "outcome": "No", "price": 0.58},
        ],
        "accepting_orders": True, "closed": False,
    }
    market = _parse_simplified_market(payload)
    assert market is not None and market.condition_id == "0xabc"
    assert len(market.tokens) == 2
    assert market.tokens[0].outcome == "Yes" and market.tokens[0].last_price == 0.42
    assert market.accepting_orders is True


def test_parse_simplified_market_skips_payload_missing_id() -> None:
    assert _parse_simplified_market({"tokens": []}) is None


def test_parse_simplified_market_drops_malformed_tokens() -> None:
    payload = {
        "condition_id": "0xabc",
        "tokens": [
            {"token_id": "T1", "outcome": "Yes", "price": "not-a-num"},
            {"missing_id": True},
        ],
        "accepting_orders": True, "closed": False,
    }
    market = _parse_simplified_market(payload)
    assert market is not None
    # token_id="T1" survives because price falls back to 0; second is dropped.
    assert [t.outcome for t in market.tokens] == ["Yes"]


def test_parse_orderbook_sorts_sides_best_first() -> None:
    payload = {
        "market": "0xm", "asset_id": "T1", "timestamp": "123",
        "bids": [
            {"price": "0.10", "size": "100"},
            {"price": "0.05", "size": "200"},
            {"price": "0.12", "size": "50"},
        ],
        "asks": [
            {"price": "0.30", "size": "20"},
            {"price": "0.99", "size": "1"},
            {"price": "0.25", "size": "10"},
        ],
    }
    book = _parse_orderbook(payload)
    # bids: best is highest price.
    assert [lvl.price for lvl in book.bids] == [0.12, 0.10, 0.05]
    # asks: best is lowest price.
    assert [lvl.price for lvl in book.asks] == [0.25, 0.30, 0.99]
    assert book.best_bid.price == 0.12 and book.best_ask.price == 0.25


def test_parse_orderbook_returns_empty_sides_when_missing() -> None:
    book = _parse_orderbook({"market": "x", "asset_id": "y", "timestamp": "0"})
    assert book.bids == () and book.asks == ()
    assert book.best_bid is None and book.best_ask is None


def test_parse_orderbook_drops_invalid_levels() -> None:
    payload = {
        "market": "x", "asset_id": "y", "timestamp": "0",
        "bids": [
            {"price": "0", "size": "100"},
            {"price": "0.5", "size": "0"},
            {"price": "0.4", "size": "10"},
            {"price": "junk", "size": "5"},
        ],
        "asks": "not-a-list",
    }
    book = _parse_orderbook(payload)
    assert len(book.bids) == 1 and book.bids[0].price == 0.4
    assert book.asks == ()


def test_parse_orderbook_raises_on_non_object() -> None:
    with pytest.raises(PolymarketAPIError, match="not an object"):
        _parse_orderbook("not a mapping")  # type: ignore[arg-type]


@pytest.mark.asyncio
async def test_fetch_active_markets_page_parses_response() -> None:
    payload = {
        "data": [
            {"condition_id": "0x1", "tokens": [
                {"token_id": "A", "outcome": "Yes", "price": 0.5},
                {"token_id": "B", "outcome": "No", "price": 0.5},
            ], "accepting_orders": True, "closed": False},
        ],
        "next_cursor": "abc123",
    }
    session = _FakeSession({"sampling-simplified-markets": _FakeResp(status=200, json_payload=payload)})
    markets, cursor = await fetch_active_markets_page(session=session)
    assert len(markets) == 1 and markets[0].condition_id == "0x1"
    assert cursor == "abc123"


@pytest.mark.asyncio
async def test_fetch_active_markets_page_raises_on_http_error() -> None:
    session = _FakeSession({"sampling-simplified-markets":
                              _FakeResp(status=500, text_payload="boom")})
    with pytest.raises(PolymarketAPIError, match="HTTP 500"):
        await fetch_active_markets_page(session=session)


@pytest.mark.asyncio
async def test_iter_active_markets_paginates_until_no_cursor() -> None:
    page_1 = {"data": [
        {"condition_id": "0x1", "tokens": [
            {"token_id": "A1", "outcome": "Yes", "price": 0.5},
            {"token_id": "B1", "outcome": "No", "price": 0.5},
        ], "accepting_orders": True, "closed": False},
    ], "next_cursor": "cursor_2"}
    page_2 = {"data": [
        {"condition_id": "0x2", "tokens": [
            {"token_id": "A2", "outcome": "Yes", "price": 0.6},
            {"token_id": "B2", "outcome": "No", "price": 0.4},
        ], "accepting_orders": True, "closed": False},
    ], "next_cursor": ""}
    session = _FakeSession({
        "sampling-simplified-markets": [
            _FakeResp(status=200, json_payload=page_1),
            _FakeResp(status=200, json_payload=page_2),
        ],
    })
    seen = [m async for m in iter_active_markets(session=session)]
    assert [m.condition_id for m in seen] == ["0x1", "0x2"]


@pytest.mark.asyncio
async def test_fetch_orderbook_returns_none_on_404() -> None:
    session = _FakeSession({"book": _FakeResp(status=404)})
    assert await fetch_orderbook(token_id="T1", session=session) is None


@pytest.mark.asyncio
async def test_fetch_orderbook_returns_none_when_no_book_error() -> None:
    session = _FakeSession({"book": _FakeResp(
        status=200, json_payload={"error": "No orderbook exists for the requested token id"}
    )})
    assert await fetch_orderbook(token_id="T1", session=session) is None


@pytest.mark.asyncio
async def test_fetch_orderbook_returns_book_when_present() -> None:
    payload = {
        "market": "0xm", "asset_id": "T1", "timestamp": "999",
        "bids": [{"price": "0.4", "size": "10"}],
        "asks": [{"price": "0.5", "size": "5"}],
    }
    session = _FakeSession({"book": _FakeResp(status=200, json_payload=payload)})
    book = await fetch_orderbook(token_id="T1", session=session)
    assert book is not None
    assert book.best_bid.price == 0.4 and book.best_ask.price == 0.5


@pytest.mark.asyncio
async def test_fetch_orderbooks_skips_missing() -> None:
    good = {"market": "x", "asset_id": "T1", "timestamp": "1",
            "bids": [{"price": "0.4", "size": "10"}],
            "asks": [{"price": "0.5", "size": "5"}]}

    def _responder(params):
        tid = params.get("token_id")
        if tid == "T_missing":
            return _FakeResp(status=200, json_payload={"error": "no book"})
        return _FakeResp(status=200, json_payload={**good, "asset_id": tid})

    session = _FakeSession({"book": _responder})
    books = await fetch_orderbooks(token_ids=["T1", "T_missing", "T2"], session=session)
    assert set(books.keys()) == {"T1", "T2"}

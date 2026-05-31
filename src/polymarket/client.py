"""Async client for the Polymarket CLOB REST API.

Public endpoints only; no auth required. Used by the arbitrage scanner
to enumerate active markets and pull per-token orderbook depth.

Endpoints:

* ``GET /sampling-simplified-markets`` — active, accepting-orders markets
  with their YES/NO token IDs and last-traded prices. Paginated via
  ``next_cursor`` in the response.
* ``GET /book?token_id=...``           — orderbook for one outcome token.

Returns plain dataclasses; no implicit conversions or rounding.
"""

from __future__ import annotations

from collections.abc import AsyncIterator, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

import aiohttp


CLOB_API_BASE = "https://clob.polymarket.com"
DEFAULT_TIMEOUT_SECONDS = 15.0


class PolymarketAPIError(RuntimeError):
    """Raised when a CLOB endpoint returns an error or malformed payload."""


@dataclass(frozen=True, slots=True)
class MarketToken:
    """One outcome token within a market (e.g. "Yes" or "No")."""

    token_id: str
    outcome: str
    last_price: float


@dataclass(frozen=True, slots=True)
class SimplifiedMarket:
    """The simplified-market shape returned by /sampling-simplified-markets."""

    condition_id: str
    tokens: tuple[MarketToken, ...]
    accepting_orders: bool
    closed: bool


@dataclass(frozen=True, slots=True)
class OrderbookLevel:
    """One price level on a side of the book."""

    price: float
    size: float


@dataclass(frozen=True, slots=True)
class Orderbook:
    """Snapshot of one token's orderbook.

    ``bids`` are returned best-first (highest price), ``asks`` best-first
    (lowest price). Empty tuples when the side has no resting orders.
    """

    market: str
    asset_id: str
    timestamp_ms: int
    bids: tuple[OrderbookLevel, ...] = field(default_factory=tuple)
    asks: tuple[OrderbookLevel, ...] = field(default_factory=tuple)

    @property
    def best_bid(self) -> OrderbookLevel | None:
        return self.bids[0] if self.bids else None

    @property
    def best_ask(self) -> OrderbookLevel | None:
        return self.asks[0] if self.asks else None


def _parse_market_token(payload: Mapping[str, Any]) -> MarketToken | None:
    token_id = payload.get("token_id")
    outcome = payload.get("outcome")
    if not token_id or not outcome:
        return None
    try:
        price = float(payload.get("price") or 0.0)
    except (TypeError, ValueError):
        price = 0.0
    return MarketToken(token_id=str(token_id), outcome=str(outcome), last_price=price)


def _parse_simplified_market(payload: Mapping[str, Any]) -> SimplifiedMarket | None:
    condition_id = payload.get("condition_id")
    if not condition_id:
        return None
    raw_tokens = payload.get("tokens") or []
    tokens: list[MarketToken] = []
    if isinstance(raw_tokens, list):
        for raw in raw_tokens:
            if isinstance(raw, Mapping):
                token = _parse_market_token(raw)
                if token is not None:
                    tokens.append(token)
    return SimplifiedMarket(
        condition_id=str(condition_id),
        tokens=tuple(tokens),
        accepting_orders=bool(payload.get("accepting_orders")),
        closed=bool(payload.get("closed")),
    )


def _parse_orderbook_side(rows: Any, *, ascending: bool) -> tuple[OrderbookLevel, ...]:
    """Parse a price-size array into sorted ``OrderbookLevel`` tuples."""
    if not isinstance(rows, list):
        return ()
    levels: list[OrderbookLevel] = []
    for row in rows:
        if not isinstance(row, Mapping):
            continue
        try:
            price = float(row.get("price") or 0.0)
            size = float(row.get("size") or 0.0)
        except (TypeError, ValueError):
            continue
        if price <= 0 or size <= 0:
            continue
        levels.append(OrderbookLevel(price=price, size=size))
    # Polymarket returns bids ascending and asks descending; we want best-first.
    levels.sort(key=lambda lvl: lvl.price, reverse=not ascending)
    return tuple(levels)


def _parse_orderbook(payload: Mapping[str, Any]) -> Orderbook:
    if not isinstance(payload, Mapping):
        raise PolymarketAPIError("orderbook payload was not an object")
    try:
        ts = int(payload.get("timestamp") or 0)
    except (TypeError, ValueError):
        ts = 0
    # bids: best is HIGHEST price, so we sort descending → ascending=False.
    bids = _parse_orderbook_side(payload.get("bids"), ascending=False)
    # asks: best is LOWEST price, so we sort ascending.
    asks = _parse_orderbook_side(payload.get("asks"), ascending=True)
    return Orderbook(
        market=str(payload.get("market") or ""),
        asset_id=str(payload.get("asset_id") or ""),
        timestamp_ms=ts, bids=bids, asks=asks,
    )


async def fetch_active_markets_page(
    *,
    session: aiohttp.ClientSession,
    base_url: str = CLOB_API_BASE,
    cursor: str = "",
    limit: int = 100,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> tuple[list[SimplifiedMarket], str | None]:
    """Fetch one page of active markets. Returns (markets, next_cursor or None)."""
    params: dict[str, Any] = {"limit": limit}
    if cursor:
        params["next_cursor"] = cursor
    url = f"{base_url.rstrip('/')}/sampling-simplified-markets"
    async with session.get(url, params=params, timeout=timeout) as resp:
        if resp.status != 200:
            preview = (await resp.text())[:200]
            raise PolymarketAPIError(f"sampling-simplified-markets HTTP {resp.status}: {preview}")
        payload = await resp.json()
    data = payload.get("data") if isinstance(payload, Mapping) else None
    if not isinstance(data, list):
        return [], None
    markets: list[SimplifiedMarket] = []
    for raw in data:
        if isinstance(raw, Mapping):
            market = _parse_simplified_market(raw)
            if market is not None:
                markets.append(market)
    next_cursor = payload.get("next_cursor")
    return markets, (str(next_cursor) if next_cursor else None)


async def iter_active_markets(
    *,
    session: aiohttp.ClientSession,
    base_url: str = CLOB_API_BASE,
    max_pages: int = 50,
    limit_per_page: int = 100,
) -> AsyncIterator[SimplifiedMarket]:
    """Yield every active market by paging until ``next_cursor`` is empty."""
    cursor = ""
    for _ in range(max_pages):
        markets, next_cursor = await fetch_active_markets_page(
            session=session, base_url=base_url,
            cursor=cursor, limit=limit_per_page,
        )
        for market in markets:
            yield market
        if not next_cursor or next_cursor == cursor:
            return
        cursor = next_cursor


async def fetch_orderbook(
    *,
    token_id: str,
    session: aiohttp.ClientSession,
    base_url: str = CLOB_API_BASE,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> Orderbook | None:
    """Fetch the orderbook for one token. Returns None if no book exists."""
    url = f"{base_url.rstrip('/')}/book"
    async with session.get(url, params={"token_id": token_id}, timeout=timeout) as resp:
        if resp.status != 200:
            return None
        payload = await resp.json()
    if isinstance(payload, Mapping) and "error" in payload and "asset_id" not in payload:
        return None
    return _parse_orderbook(payload)


async def fetch_orderbooks(
    *,
    token_ids: Sequence[str],
    session: aiohttp.ClientSession,
    base_url: str = CLOB_API_BASE,
    concurrency: int = 8,
) -> dict[str, Orderbook]:
    """Fetch many orderbooks concurrently, dropping any that don't exist."""
    import asyncio
    sem = asyncio.Semaphore(max(concurrency, 1))
    out: dict[str, Orderbook] = {}

    async def _one(tid: str) -> None:
        async with sem:
            book = await fetch_orderbook(
                token_id=tid, session=session, base_url=base_url,
            )
        if book is not None:
            out[tid] = book

    await asyncio.gather(*(_one(tid) for tid in token_ids))
    return out

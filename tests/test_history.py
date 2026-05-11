"""Unit tests for Binance historical kline REST fetching."""

from __future__ import annotations

import asyncio
import json
from datetime import datetime, timezone
from typing import Any

import pytest

from src.data.history import HistoryFetcher


class FakeResponse:
    """Minimal aiohttp response double for kline tests."""

    def __init__(self, payload: Any, *, status: int = 200) -> None:
        self.status = status
        self._payload = payload

    async def __aenter__(self) -> "FakeResponse":
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: object | None,
    ) -> None:
        return None

    async def text(self) -> str:
        return json.dumps(self._payload)


class FakeSession:
    """Capture REST calls and return preloaded kline pages."""

    def __init__(self, pages: list[list[list[Any]]]) -> None:
        self._pages = list(pages)
        self.calls: list[dict[str, Any]] = []

    def get(self, url: str, *, params: dict[str, Any]) -> FakeResponse:
        self.calls.append({"url": url, "params": dict(params)})
        return FakeResponse(self._pages.pop(0))


def _kline(open_time: int) -> list[Any]:
    return [
        open_time,
        "1.0",
        "2.0",
        "0.5",
        "1.5",
        "10.0",
        open_time + 59_999,
        "15.0",
        42,
        "4.0",
        "6.0",
        "0",
    ]


def test_fetch_ohlcv_paginates_binance_rest_klines() -> None:
    first_page = [_kline(index * 60_000) for index in range(1_000)]
    second_page = [_kline(index * 60_000) for index in range(1_000, 1_500)]
    session = FakeSession([first_page, second_page])
    fetcher = HistoryFetcher(base_url="https://example.test", session=session)

    result = asyncio.run(
        fetcher.fetch_ohlcv(
            "BTC/USDT",
            "1m",
            datetime.fromtimestamp(0, tz=timezone.utc),
            datetime.fromtimestamp(1_499 * 60, tz=timezone.utc),
        )
    )

    assert len(result) == 1_500
    assert result[0] == [0, 1.0, 2.0, 0.5, 1.5, 10.0]
    assert result[-1] == [1_499 * 60_000, 1.0, 2.0, 0.5, 1.5, 10.0]
    assert session.calls == [
        {
            "url": "https://example.test/api/v3/klines",
            "params": {
                "symbol": "BTCUSDT",
                "interval": "1m",
                "startTime": 0,
                "endTime": 1_499 * 60_000,
                "limit": 1_000,
            },
        },
        {
            "url": "https://example.test/api/v3/klines",
            "params": {
                "symbol": "BTCUSDT",
                "interval": "1m",
                "startTime": 999 * 60_000 + 1,
                "endTime": 1_499 * 60_000,
                "limit": 1_000,
            },
        },
    ]


def test_fetch_klines_sorts_partial_page_and_normalizes_symbol() -> None:
    session = FakeSession(
        [
            [_kline(120_000), _kline(0), _kline(60_000)],
        ]
    )
    fetcher = HistoryFetcher(base_url="https://example.test", session=session)

    result = asyncio.run(
        fetcher.fetch_klines(
            "eth-usdt",
            "1m",
            datetime.fromtimestamp(0, tz=timezone.utc),
            datetime.fromtimestamp(3 * 60, tz=timezone.utc),
        )
    )

    assert [row[0] for row in result] == [0, 60_000, 120_000]
    assert session.calls[0]["params"]["symbol"] == "ETHUSDT"


def test_fetch_klines_rejects_invalid_time_range() -> None:
    fetcher = HistoryFetcher(
        base_url="https://example.test",
        session=FakeSession([]),
    )

    with pytest.raises(ValueError, match="start_time"):
        asyncio.run(
            fetcher.fetch_klines(
                "BTCUSDT",
                "1m",
                datetime.fromtimestamp(60, tz=timezone.utc),
                datetime.fromtimestamp(0, tz=timezone.utc),
            )
        )

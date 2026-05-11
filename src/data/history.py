"""Asynchronous Binance historical kline fetching utilities."""

from __future__ import annotations

import json
import logging
import re
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

import aiohttp

try:
    from loguru import logger
except ImportError:  # pragma: no cover - production installs loguru

    class _FallbackLogger:
        """Tiny loguru-compatible subset used when loguru is unavailable."""

        def __init__(self, logger_: logging.Logger) -> None:
            self._logger = logger_

        def bind(self, **_: Any) -> "_FallbackLogger":
            return self

        def debug(self, message: str, *args: Any, **kwargs: Any) -> None:
            self._log(logging.DEBUG, message, *args, **kwargs)

        def info(self, message: str, *args: Any, **kwargs: Any) -> None:
            self._log(logging.INFO, message, *args, **kwargs)

        def _log(
            self,
            level: int,
            message: str,
            *args: Any,
            **kwargs: Any,
        ) -> None:
            if args:
                message = message.format(*args)
            self._logger.log(level, message, **kwargs)

    logger = _FallbackLogger(logging.getLogger(__name__))

if TYPE_CHECKING:
    from src.exchange.client import BinanceClient


Kline = list[Any]
OhlcvRow = list[float | int]


class BinanceHistoryError(Exception):
    """Raised when Binance historical kline data cannot be fetched."""


class HistoryFetcher:
    """Fetch paginated Binance spot kline history through the REST API."""

    _BINANCE_REST_URL = "https://api.binance.com"
    _BINANCE_TESTNET_REST_URL = "https://testnet.binance.vision"
    _KLINES_PATH = "/api/v3/klines"
    _LIMIT = 1_000
    _SYMBOL_SEPARATOR_RE = re.compile(r"[/:\s_-]+")

    def __init__(
        self,
        client: BinanceClient | None = None,
        *,
        base_url: str | None = None,
        session: aiohttp.ClientSession | None = None,
        request_timeout_seconds: float = 10.0,
    ) -> None:
        """Initialize the fetcher.

        Args:
            client: Optional Binance client. Its ``testnet`` setting is used
                when ``base_url`` is not supplied.
            base_url: Optional REST API base URL override.
            session: Optional externally owned aiohttp session.
            request_timeout_seconds: Total timeout per REST request.
        """
        if request_timeout_seconds <= 0:
            raise ValueError("request_timeout_seconds must be greater than zero")

        self.client = client
        self.base_url = (base_url or self._base_url_from_client(client)).rstrip("/")
        self._session = session
        self._request_timeout_seconds = request_timeout_seconds

    async def fetch_klines(
        self,
        symbol: str,
        interval: str,
        start_time: datetime,
        end_time: datetime | None = None,
    ) -> list[Kline]:
        """Fetch raw Binance kline rows across the full requested timeframe.

        Binance limits each ``/api/v3/klines`` response to 1,000 rows. This
        method loops through pages by moving ``startTime`` past the last kline's
        open time, then consolidates the pages into one chronologically sorted
        dataset.

        Args:
            symbol: Market symbol, for example ``"BTC/USDT"`` or ``"BTCUSDT"``.
            interval: Binance kline interval, for example ``"1m"`` or ``"1h"``.
            start_time: First kline open timestamp to request.
            end_time: Optional final kline open timestamp to include.

        Returns:
            Raw Binance kline rows.

        Raises:
            ValueError: If required inputs are blank or the range is invalid.
            BinanceHistoryError: If Binance returns an HTTP or payload error.
        """
        rest_symbol = self._rest_symbol(symbol)
        normalized_interval = interval.strip()
        if not normalized_interval:
            raise ValueError("interval must not be blank")

        start_ms = _datetime_to_milliseconds(start_time)
        end_ms = (
            _datetime_to_milliseconds(end_time)
            if end_time is not None
            else None
        )
        if end_ms is not None and start_ms > end_ms:
            raise ValueError("start_time must be before or equal to end_time")

        logger.bind(
            symbol=rest_symbol,
            interval=normalized_interval,
            start_time_ms=start_ms,
            end_time_ms=end_ms,
            limit=self._LIMIT,
            base_url=self.base_url,
        ).info("Starting Binance REST kline history fetch")

        klines_by_open_time: dict[int, Kline] = {}
        next_start_ms = start_ms
        page_number = 0

        async with self._session_context() as session:
            while end_ms is None or next_start_ms <= end_ms:
                page_number += 1
                page = await self._fetch_kline_page(
                    session=session,
                    symbol=rest_symbol,
                    interval=normalized_interval,
                    start_ms=next_start_ms,
                    end_ms=end_ms,
                    page_number=page_number,
                )
                if not page:
                    logger.bind(
                        symbol=rest_symbol,
                        interval=normalized_interval,
                        page_number=page_number,
                        next_start_ms=next_start_ms,
                        total_klines=len(klines_by_open_time),
                    ).info("No klines returned; stopping history fetch")
                    break

                sorted_page = sorted(page, key=_kline_open_time)
                last_open_time = _kline_open_time(sorted_page[-1])

                for kline in sorted_page:
                    open_time = _kline_open_time(kline)
                    if open_time < start_ms:
                        continue
                    if end_ms is not None and open_time > end_ms:
                        continue
                    klines_by_open_time[open_time] = list(kline)

                next_page_start_ms = last_open_time + 1
                logger.bind(
                    symbol=rest_symbol,
                    interval=normalized_interval,
                    page_number=page_number,
                    page_size=len(page),
                    total_klines=len(klines_by_open_time),
                    last_open_time_ms=last_open_time,
                    next_start_ms=next_page_start_ms,
                ).info(
                    "Fetched {} Binance klines for {}; moving to {}",
                    len(page),
                    rest_symbol,
                    _format_milliseconds(next_page_start_ms),
                )

                if next_page_start_ms <= next_start_ms:
                    raise BinanceHistoryError(
                        "Binance kline pagination did not advance"
                    )

                next_start_ms = next_page_start_ms
                if len(page) < self._LIMIT:
                    break

        result = [
            klines_by_open_time[open_time]
            for open_time in sorted(klines_by_open_time)
        ]
        logger.bind(
            symbol=rest_symbol,
            interval=normalized_interval,
            total_klines=len(result),
            start_time_ms=start_ms,
            end_time_ms=end_ms,
        ).info("Completed Binance REST kline history fetch")
        return result

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str,
        start_time: datetime,
        end_time: datetime | None = None,
    ) -> list[OhlcvRow]:
        """Fetch historical OHLCV candles using Binance REST klines.

        The returned rows keep the ccxt-compatible shape used elsewhere in the
        project: ``[timestamp, open, high, low, close, volume]``.
        """
        klines = await self.fetch_klines(
            symbol=symbol,
            interval=timeframe,
            start_time=start_time,
            end_time=end_time,
        )
        return [_kline_to_ohlcv(kline) for kline in klines]

    async def _fetch_kline_page(
        self,
        *,
        session: aiohttp.ClientSession,
        symbol: str,
        interval: str,
        start_ms: int,
        end_ms: int | None,
        page_number: int,
    ) -> list[Kline]:
        """Fetch one page from Binance's public kline REST endpoint."""
        params: dict[str, int | str] = {
            "symbol": symbol,
            "interval": interval,
            "startTime": start_ms,
            "limit": self._LIMIT,
        }
        if end_ms is not None:
            params["endTime"] = end_ms

        url = f"{self.base_url}{self._KLINES_PATH}"
        try:
            async with session.get(url, params=params) as response:
                response_text = await response.text()
                if response.status >= 400:
                    raise BinanceHistoryError(
                        "Binance kline request failed "
                        f"with HTTP {response.status}: {response_text}"
                    )
        except TimeoutError as exc:
            raise BinanceHistoryError(
                "Timed out while fetching Binance klines"
            ) from exc
        except aiohttp.ClientError as exc:
            raise BinanceHistoryError(
                "Failed to fetch Binance klines"
            ) from exc

        try:
            payload = json.loads(response_text)
        except json.JSONDecodeError as exc:
            raise BinanceHistoryError(
                "Binance kline response was not valid JSON"
            ) from exc

        if not isinstance(payload, list):
            raise BinanceHistoryError(
                "Binance kline response was not a list",
            )

        for item in payload:
            if not isinstance(item, list) or len(item) < 6:
                raise BinanceHistoryError(
                    "Binance kline response contained an invalid row",
                )

        logger.bind(
            symbol=symbol,
            interval=interval,
            page_number=page_number,
            page_size=len(payload),
        ).debug("Fetched Binance kline page")
        return payload

    @asynccontextmanager
    async def _session_context(self) -> AsyncIterator[aiohttp.ClientSession]:
        """Yield an aiohttp session without closing externally owned sessions."""
        if self._session is not None:
            yield self._session
            return

        timeout = aiohttp.ClientTimeout(total=self._request_timeout_seconds)
        async with aiohttp.ClientSession(timeout=timeout) as session:
            yield session

    def _base_url_from_client(self, client: BinanceClient | None) -> str:
        config = getattr(client, "config", None)
        if getattr(config, "testnet", False):
            return self._BINANCE_TESTNET_REST_URL
        return self._BINANCE_REST_URL

    def _rest_symbol(self, symbol: str) -> str:
        normalized = symbol.strip().upper()
        if not normalized:
            raise ValueError("symbol must not be blank")

        parts = [part for part in self._SYMBOL_SEPARATOR_RE.split(normalized) if part]
        if len(parts) >= 2:
            return f"{parts[0]}{parts[1]}"
        return normalized


def _datetime_to_milliseconds(value: datetime) -> int:
    """Convert a datetime to a Unix timestamp in milliseconds.

    Naive datetimes are interpreted as UTC to avoid host-local timezone drift.
    """
    if value.tzinfo is None or value.utcoffset() is None:
        value = value.replace(tzinfo=timezone.utc)
    return int(value.timestamp() * 1_000)


def _format_milliseconds(value: int) -> str:
    """Format a millisecond timestamp for human-readable logs."""
    return datetime.fromtimestamp(value / 1_000, tz=timezone.utc).isoformat()


def _kline_open_time(kline: Kline) -> int:
    return int(kline[0])


def _kline_to_ohlcv(kline: Kline) -> OhlcvRow:
    return [
        int(kline[0]),
        float(kline[1]),
        float(kline[2]),
        float(kline[3]),
        float(kline[4]),
        float(kline[5]),
    ]


__all__ = ["BinanceHistoryError", "HistoryFetcher", "Kline", "OhlcvRow"]

"""High-level Binance market data feeds backed by ccxt.pro."""

from __future__ import annotations

import asyncio
import logging
import math
import random
import re
import time
from collections.abc import AsyncIterator, Iterable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Protocol, TypeVar

try:
    from loguru import logger
except ImportError:  # pragma: no cover - production installs loguru

    class _FallbackLogger:
        """Tiny loguru-compatible subset used when optional deps are absent."""

        def __init__(
            self,
            logger_: logging.Logger,
            *,
            exception: BaseException | bool | None = None,
        ) -> None:
            self._logger = logger_
            self._exception = exception

        def bind(self, **_: Any) -> "_FallbackLogger":
            return self

        def opt(self, **kwargs: Any) -> "_FallbackLogger":
            return _FallbackLogger(
                self._logger,
                exception=kwargs.get("exception"),
            )

        def debug(self, message: str, *args: Any, **kwargs: Any) -> None:
            self._log(logging.DEBUG, message, *args, **kwargs)

        def info(self, message: str, *args: Any, **kwargs: Any) -> None:
            self._log(logging.INFO, message, *args, **kwargs)

        def warning(self, message: str, *args: Any, **kwargs: Any) -> None:
            self._log(logging.WARNING, message, *args, **kwargs)

        def error(self, message: str, *args: Any, **kwargs: Any) -> None:
            self._log(logging.ERROR, message, *args, **kwargs)

        def _log(
            self,
            level: int,
            message: str,
            *args: Any,
            **kwargs: Any,
        ) -> None:
            exc_info = self._exception if self._exception else None
            self._logger.log(
                level,
                message,
                *args,
                exc_info=exc_info,
                **kwargs,
            )

    logger = _FallbackLogger(logging.getLogger(__name__))

try:  # pragma: no cover - exercised only when ccxt is installed.
    import ccxt
except ImportError:  # pragma: no cover - keeps pure normalization tests importable.
    ccxt = None

    class _MissingCcxtNetworkError(Exception):
        """Fallback used when ccxt is unavailable in the local environment."""

    class _MissingCcxtExchangeError(Exception):
        """Fallback used when ccxt is unavailable in the local environment."""

    NetworkError = _MissingCcxtNetworkError
    ExchangeError = _MissingCcxtExchangeError
else:
    NetworkError = ccxt.NetworkError
    ExchangeError = ccxt.ExchangeError


_QUOTE_ASSETS = (
    "FDUSD",
    "BUSD",
    "USDT",
    "USDC",
    "TUSD",
    "USDP",
    "BIDR",
    "BKRW",
    "BRL",
    "TRY",
    "EUR",
    "GBP",
    "AUD",
    "DAI",
    "BTC",
    "ETH",
    "BNB",
    "USD",
)
_SEPARATOR_RE = re.compile(r"[\s_-]+")
_NON_SYMBOL_RE = re.compile(r"[^A-Z0-9]")
_RETRYABLE_ERRORS = (
    NetworkError,
    ExchangeError,
    asyncio.TimeoutError,
    TimeoutError,
    ConnectionError,
    OSError,
    EOFError,
)


@dataclass(slots=True, frozen=True)
class TickerData:
    """Normalized best bid/ask and last-trade market data."""

    symbol: str
    timestamp: int
    bid: float
    ask: float
    last: float
    volume: float


@dataclass(slots=True, frozen=True)
class CandleData:
    """Normalized OHLCV candle data."""

    symbol: str
    timestamp: int
    open: float
    high: float
    low: float
    close: float
    volume: float


_TIMEFRAME_SECONDS: dict[str, int] = {
    "1m": 60, "3m": 180, "5m": 300, "15m": 900, "30m": 1800,
    "1h": 3600, "2h": 7200, "4h": 14400, "6h": 21600, "8h": 28800, "12h": 43200,
    "1d": 86400, "3d": 259200, "1w": 604800,
}


class CcxtProBinanceExchange(Protocol):
    """Subset of the ccxt Binance interface used by this module."""

    async def watch_ticker(self, symbol: str) -> Mapping[str, Any]:
        """Watch one ticker stream."""

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str = "1m",
        since: int | None = None,
        limit: int | None = None,
    ) -> Any:
        """Fetch OHLCV candles via REST."""


_QueueItem = TickerData | CandleData | BaseException
_QueueItemT = TypeVar("_QueueItemT", bound=_QueueItem)


class _ExponentialBackoff:
    """Exponential backoff with small jitter for reconnect loops."""

    def __init__(self, base_seconds: float, max_seconds: float) -> None:
        if base_seconds <= 0:
            raise ValueError("base_backoff_seconds must be greater than zero")
        if max_seconds <= 0:
            raise ValueError("max_backoff_seconds must be greater than zero")
        if max_seconds < base_seconds:
            raise ValueError(
                "max_backoff_seconds must be greater than or equal to "
                "base_backoff_seconds"
            )

        self._base_seconds = base_seconds
        self._max_seconds = max_seconds
        self._attempt = 0

    @property
    def attempt(self) -> int:
        """Return the number of consecutive failed attempts."""
        return self._attempt

    def reset(self) -> None:
        """Reset attempts after a successful stream read."""
        self._attempt = 0

    def next_delay(self) -> float:
        """Return the next retry delay in seconds."""
        delay = min(self._base_seconds * 2**self._attempt, self._max_seconds)
        self._attempt += 1
        return delay + random.uniform(0, delay * 0.25)


def normalize_symbol(symbol: str) -> str:
    """Normalize a Binance symbol to ccxt's ``BASE/QUOTE`` format.

    Examples:
        ``"BTCUSDT"``, ``"btc-usdt"``, and ``"btc_usdt"`` all become
        ``"BTC/USDT"``.

    Args:
        symbol: User-supplied market symbol.

    Returns:
        Symbol in ccxt spot format.

    Raises:
        ValueError: If the symbol is blank or cannot be split into base/quote.
    """
    raw_symbol = symbol.strip().upper()
    if not raw_symbol:
        raise ValueError("symbol must not be blank")

    separated = _SEPARATOR_RE.sub("/", raw_symbol)
    if "/" in separated:
        base, quote = _split_symbol_parts(separated, separator="/")
        return f"{base}/{quote}"

    if ":" in separated:
        base, quote = _split_symbol_parts(separated, separator=":")
        return f"{base}/{quote}"

    compact = _NON_SYMBOL_RE.sub("", separated)
    for quote in _QUOTE_ASSETS:
        if compact.endswith(quote) and len(compact) > len(quote):
            return f"{compact[: -len(quote)]}/{quote}"

    raise ValueError(f"could not infer quote asset from symbol: {symbol!r}")


class BinanceDataFeed:
    """Normalize high-level Binance market data streams from ccxt.pro.

    The feed does not own the supplied exchange instance. Callers remain
    responsible for closing the ccxt.pro exchange when the application exits.
    """

    def __init__(
        self,
        exchange: CcxtProBinanceExchange,
        *,
        queue_maxsize: int = 1_000,
        base_backoff_seconds: float = 1.0,
        max_backoff_seconds: float = 60.0,
        max_retries: int | None = None,
    ) -> None:
        """Initialize the data feed.

        Args:
            exchange: Async ccxt.pro Binance exchange instance.
            queue_maxsize: Per-subscription queue bound. When full, the oldest
                pending update is dropped to preserve freshness.
            base_backoff_seconds: Initial retry delay.
            max_backoff_seconds: Maximum retry delay.
            max_retries: Consecutive retry limit. ``None`` retries forever.
        """
        if queue_maxsize < 0:
            raise ValueError("queue_maxsize must be greater than or equal to zero")
        if max_retries is not None and max_retries < 0:
            raise ValueError("max_retries must be greater than or equal to zero")

        self.exchange = exchange
        self._queue_maxsize = queue_maxsize
        self._base_backoff_seconds = base_backoff_seconds
        self._max_backoff_seconds = max_backoff_seconds
        self._max_retries = max_retries

        _ExponentialBackoff(base_backoff_seconds, max_backoff_seconds)

    async def subscribe_tickers(
        self,
        symbols: Iterable[str] | str,
    ) -> AsyncIterator[TickerData]:
        """Yield normalized ticker updates for multiple symbols.

        Each symbol is watched by an independent task so a slow or reconnecting
        stream does not block the others.
        """
        normalized_symbols = _normalize_symbols(symbols)
        queue: asyncio.Queue[TickerData | BaseException] = asyncio.Queue(
            maxsize=self._queue_maxsize
        )
        tasks = [
            asyncio.create_task(
                self._watch_ticker(symbol, queue),
                name=f"binance-ticker-{symbol.replace('/', '')}",
            )
            for symbol in normalized_symbols
        ]

        logger.bind(
            exchange="binance",
            symbols=normalized_symbols,
            stream="ticker",
        ).info("Binance ticker streams subscribed")

        try:
            while True:
                item = await queue.get()
                if isinstance(item, BaseException):
                    raise item
                yield item
        finally:
            await _cancel_tasks(tasks)
            logger.bind(
                exchange="binance",
                symbols=normalized_symbols,
                stream="ticker",
            ).info("Binance ticker streams unsubscribed")

    async def subscribe_ohlcv(
        self,
        symbol: str,
        timeframe: str = "1m",
    ) -> AsyncIterator[CandleData]:
        """Yield normalized OHLCV updates for one symbol/timeframe."""
        normalized_symbol = normalize_symbol(symbol)
        normalized_timeframe = timeframe.strip()
        if not normalized_timeframe:
            raise ValueError("timeframe must not be blank")

        logger.bind(
            exchange="binance",
            symbol=normalized_symbol,
            timeframe=normalized_timeframe,
            stream="ohlcv",
        ).info("Binance OHLCV stream subscribed")

        poll_interval = _TIMEFRAME_SECONDS.get(normalized_timeframe, 60)
        last_timestamp: int | None = None
        backoff = self._new_backoff()
        try:
            while True:
                try:
                    candles = await self.exchange.fetch_ohlcv(
                        normalized_symbol,
                        normalized_timeframe,
                        limit=2,
                    )
                except asyncio.CancelledError:
                    logger.bind(
                        exchange="binance",
                        symbol=normalized_symbol,
                        timeframe=normalized_timeframe,
                        stream="ohlcv",
                    ).debug("Binance OHLCV stream cancelled")
                    raise
                except _RETRYABLE_ERRORS as exc:
                    await self._retry_or_raise(
                        exc,
                        backoff=backoff,
                        stream="ohlcv",
                        symbol=normalized_symbol,
                        timeframe=normalized_timeframe,
                    )
                    continue

                try:
                    candle = _latest_candle_data(normalized_symbol, candles)
                except (TypeError, ValueError) as exc:
                    logger.bind(
                        exchange="binance",
                        symbol=normalized_symbol,
                        timeframe=normalized_timeframe,
                        stream="ohlcv",
                        error_message=str(exc),
                    ).warning("Dropped malformed Binance OHLCV payload")
                    await asyncio.sleep(poll_interval)
                    continue

                self._log_reconnected_if_needed(
                    backoff,
                    stream="ohlcv",
                    symbol=normalized_symbol,
                    timeframe=normalized_timeframe,
                )
                backoff.reset()

                if candle.timestamp != last_timestamp:
                    last_timestamp = candle.timestamp
                    yield candle

                await asyncio.sleep(poll_interval)
        finally:
            logger.bind(
                exchange="binance",
                symbol=normalized_symbol,
                timeframe=normalized_timeframe,
                stream="ohlcv",
            ).info("Binance OHLCV stream unsubscribed")

    async def _watch_ticker(
        self,
        symbol: str,
        queue: asyncio.Queue[TickerData | BaseException],
    ) -> None:
        backoff = self._new_backoff()
        logger.bind(
            exchange="binance",
            symbol=symbol,
            stream="ticker",
        ).debug("Binance ticker stream worker started")

        while True:
            try:
                ticker = await self.exchange.watch_ticker(symbol)
            except asyncio.CancelledError:
                logger.bind(
                    exchange="binance",
                    symbol=symbol,
                    stream="ticker",
                ).debug("Binance ticker stream worker cancelled")
                raise
            except _RETRYABLE_ERRORS as exc:
                try:
                    await self._retry_or_raise(
                        exc,
                        backoff=backoff,
                        stream="ticker",
                        symbol=symbol,
                    )
                except Exception as retry_exc:
                    _enqueue(queue, retry_exc, stream="ticker", symbol=symbol)
                    return
                continue
            except Exception as exc:
                logger.bind(
                    exchange="binance",
                    symbol=symbol,
                    stream="ticker",
                    error_type=exc.__class__.__name__,
                    error_message=str(exc),
                ).opt(exception=exc).error("Binance ticker stream failed")
                _enqueue(queue, exc, stream="ticker", symbol=symbol)
                return

            try:
                ticker_data = _ticker_data(symbol, ticker)
            except (TypeError, ValueError) as exc:
                logger.bind(
                    exchange="binance",
                    symbol=symbol,
                    stream="ticker",
                    error_message=str(exc),
                ).warning("Dropped malformed Binance ticker payload")
                continue

            self._log_reconnected_if_needed(
                backoff,
                stream="ticker",
                symbol=symbol,
            )
            backoff.reset()
            _enqueue(queue, ticker_data, stream="ticker", symbol=symbol)

    def _new_backoff(self) -> _ExponentialBackoff:
        return _ExponentialBackoff(
            self._base_backoff_seconds,
            self._max_backoff_seconds,
        )

    async def _retry_or_raise(
        self,
        exc: BaseException,
        *,
        backoff: _ExponentialBackoff,
        stream: str,
        symbol: str,
        timeframe: str | None = None,
    ) -> None:
        retries_exhausted = (
            self._max_retries is not None and backoff.attempt >= self._max_retries
        )
        if retries_exhausted:
            logger.bind(
                exchange="binance",
                symbol=symbol,
                timeframe=timeframe,
                stream=stream,
                attempts=backoff.attempt,
                error_type=exc.__class__.__name__,
                error_message=str(exc),
            ).opt(exception=exc).error("Binance stream retries exhausted")
            raise exc

        delay = backoff.next_delay()
        logger.bind(
            exchange="binance",
            symbol=symbol,
            timeframe=timeframe,
            stream=stream,
            attempt=backoff.attempt,
            sleep_seconds=round(delay, 3),
            error_type=exc.__class__.__name__,
            error_message=str(exc),
        ).opt(exception=exc).warning(
            f"Binance stream dropped; reconnecting in {round(delay, 1)}s (attempt {backoff.attempt})"
        )
        await asyncio.sleep(delay)

    @staticmethod
    def _log_reconnected_if_needed(
        backoff: _ExponentialBackoff,
        *,
        stream: str,
        symbol: str,
        timeframe: str | None = None,
    ) -> None:
        if backoff.attempt == 0:
            return
        logger.bind(
            exchange="binance",
            symbol=symbol,
            timeframe=timeframe,
            stream=stream,
            attempts=backoff.attempt,
        ).info("Binance stream reconnected")


def _split_symbol_parts(symbol: str, *, separator: str) -> tuple[str, str]:
    parts = [part for part in symbol.split(separator) if part]
    if len(parts) != 2:
        raise ValueError(f"symbol must contain exactly one base and quote: {symbol!r}")

    base, quote = parts
    quote = quote.split(":", 1)[0]
    if not base or not quote:
        raise ValueError(f"symbol must contain base and quote assets: {symbol!r}")
    return base, quote


def _normalize_symbols(symbols: Iterable[str] | str) -> list[str]:
    normalized_symbols: list[str] = []
    seen: set[str] = set()
    source_symbols = (symbols,) if isinstance(symbols, str) else symbols
    for symbol in source_symbols:
        normalized = normalize_symbol(symbol)
        if normalized in seen:
            continue
        seen.add(normalized)
        normalized_symbols.append(normalized)

    if not normalized_symbols:
        raise ValueError("symbols must not be empty")

    return normalized_symbols


def _ticker_data(requested_symbol: str, ticker: Any) -> TickerData:
    if not isinstance(ticker, Mapping):
        raise ValueError("ticker payload must be a mapping")

    payloads = _payload_candidates(ticker)
    symbol = _symbol_from_payload(requested_symbol, payloads)
    timestamp = _timestamp_ms(
        _first_present(
            payloads,
            "timestamp",
            "E",
            "eventTime",
            "time",
            "closeTime",
            "C",
            "T",
        )
    )
    bid = _required_float_from(payloads, "bid", "bidPrice", "b")
    ask = _required_float_from(payloads, "ask", "askPrice", "a")
    last = _required_float_from(payloads, "last", "close", "lastPrice", "c")
    volume = _optional_float_from(
        payloads,
        "baseVolume",
        "volume",
        "v",
        default=0.0,
    )

    return TickerData(
        symbol=symbol,
        timestamp=timestamp,
        bid=bid,
        ask=ask,
        last=last,
        volume=volume,
    )


def _latest_candle_data(
    symbol: str,
    candles: Any,
) -> CandleData:
    if isinstance(candles, Mapping):
        return _candle_data_from_mapping(symbol, candles)

    if _looks_like_ohlcv_row(candles):
        return _candle_data_from_row(symbol, candles)

    if isinstance(candles, (str, bytes)) or not isinstance(candles, Sequence):
        raise ValueError("OHLCV payload must be a candle row or sequence of rows")

    if not candles:
        raise ValueError("OHLCV payload is empty")

    candle = candles[-1]
    if isinstance(candle, Mapping):
        return _candle_data_from_mapping(symbol, candle)

    return _candle_data_from_row(symbol, candle)


def _candle_data_from_mapping(
    default_symbol: str,
    candle: Mapping[str, Any],
) -> CandleData:
    payloads = _payload_candidates(candle)
    kline = _first_mapping(payloads, "k")
    if kline is not None:
        payloads = [kline, *payloads]

    return CandleData(
        symbol=_symbol_from_payload(default_symbol, payloads),
        timestamp=_timestamp_ms(
            _first_present(payloads, "timestamp", "time", "t", "T")
        ),
        open=_required_float_from(payloads, "open", "o"),
        high=_required_float_from(payloads, "high", "h"),
        low=_required_float_from(payloads, "low", "l"),
        close=_required_float_from(payloads, "close", "c"),
        volume=_required_float_from(payloads, "volume", "v"),
    )


def _candle_data_from_row(symbol: str, candle: Any) -> CandleData:
    if isinstance(candle, (str, bytes, Mapping)):
        raise ValueError("OHLCV candle must be a sequence row")
    if not isinstance(candle, Sequence):
        raise ValueError("OHLCV candle must be a sequence row")

    if len(candle) < 6:
        raise ValueError(f"OHLCV candle has {len(candle)} fields; expected 6")

    return CandleData(
        symbol=normalize_symbol(symbol),
        timestamp=_timestamp_ms(candle[0]),
        open=_as_float(candle[1], field_name="open"),
        high=_as_float(candle[2], field_name="high"),
        low=_as_float(candle[3], field_name="low"),
        close=_as_float(candle[4], field_name="close"),
        volume=_as_float(candle[5], field_name="volume"),
    )


def _payload_candidates(payload: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    data = payload.get("data")
    if isinstance(data, Mapping):
        candidates = [data, payload]
    else:
        candidates = [payload]

    for candidate in list(candidates):
        info = candidate.get("info")
        if isinstance(info, Mapping) and info not in candidates:
            candidates.append(info)

    return candidates


def _symbol_from_payload(
    default_symbol: str,
    payloads: Sequence[Mapping[str, Any]],
) -> str:
    for payload in payloads:
        for key in ("symbol", "s"):
            value = payload.get(key)
            if value:
                return normalize_symbol(str(value))

        stream = payload.get("stream")
        if stream:
            stream_symbol = str(stream).split("@", 1)[0]
            try:
                return normalize_symbol(stream_symbol)
            except ValueError:
                continue

    return normalize_symbol(default_symbol)


def _first_mapping(
    payloads: Sequence[Mapping[str, Any]],
    key: str,
) -> Mapping[str, Any] | None:
    for payload in payloads:
        value = payload.get(key)
        if isinstance(value, Mapping):
            return value
    return None


def _first_present(
    payloads: Sequence[Mapping[str, Any]],
    *keys: str,
) -> Any:
    for payload in payloads:
        for key in keys:
            value = payload.get(key)
            if value is not None:
                return value
    return None


def _required_float_from(
    payloads: Sequence[Mapping[str, Any]],
    *keys: str,
) -> float:
    value = _first_present(payloads, *keys)
    if value is not None:
        return _as_float(value, field_name="/".join(keys))
    raise ValueError(f"missing required numeric field: {'/'.join(keys)}")


def _optional_float_from(
    payloads: Sequence[Mapping[str, Any]],
    *keys: str,
    default: float,
) -> float:
    value = _first_present(payloads, *keys)
    if value is not None:
        return _as_float(value, field_name="/".join(keys))
    return default


def _looks_like_ohlcv_row(value: Any) -> bool:
    if isinstance(value, (str, bytes, Mapping)):
        return False
    if not isinstance(value, Sequence):
        return False
    if len(value) < 6:
        return False
    return not isinstance(value[0], (list, tuple, dict))


def _required_float(payload: Mapping[str, Any], *keys: str) -> float:
    for key in keys:
        value = payload.get(key)
        if value is not None:
            return _as_float(value, field_name=key)
    raise ValueError(f"missing required numeric field: {'/'.join(keys)}")


def _optional_float(
    payload: Mapping[str, Any],
    *keys: str,
    default: float,
) -> float:
    for key in keys:
        value = payload.get(key)
        if value is not None:
            return _as_float(value, field_name=key)
    return default


def _as_float(value: Any, *, field_name: str) -> float:
    try:
        result = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field_name} is not numeric: {value!r}") from exc
    if not math.isfinite(result):
        raise ValueError(f"{field_name} must be finite")
    return result


def _timestamp_ms(value: Any) -> int:
    if value is None:
        return int(time.time() * 1000)
    try:
        return int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(
            f"timestamp is not an integer millisecond value: {value!r}"
        ) from exc


def _enqueue(
    queue: asyncio.Queue[_QueueItemT],
    item: _QueueItemT,
    *,
    stream: str,
    symbol: str,
) -> None:
    try:
        queue.put_nowait(item)
        return
    except asyncio.QueueFull:
        pass

    try:
        queue.get_nowait()
    except asyncio.QueueEmpty:
        pass

    queue.put_nowait(item)
    logger.bind(
        exchange="binance",
        stream=stream,
        symbol=symbol,
    ).warning("Market data queue full; dropped oldest update")


async def _cancel_tasks(tasks: Sequence[asyncio.Task[Any]]) -> None:
    for task in tasks:
        task.cancel()
    if tasks:
        await asyncio.gather(*tasks, return_exceptions=True)


__all__ = [
    "BinanceDataFeed",
    "CandleData",
    "CcxtProBinanceExchange",
    "TickerData",
    "normalize_symbol",
]

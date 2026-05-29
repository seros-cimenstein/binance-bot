"""Asynchronous Binance WebSocket feed manager.

The manager uses Binance spot WebSocket streams directly through ``aiohttp``.
It is intentionally compatible with :class:`src.exchange.client.BinanceClient`
without importing that module at runtime, so public streams can still be used in
environments where ``ccxt`` is not installed.
"""

from __future__ import annotations

import asyncio
import hashlib
import hmac
import inspect
import json
import logging
import os
import random
import time
from collections.abc import Awaitable, Callable, Iterable, Mapping
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from types import TracebackType
from typing import TYPE_CHECKING, Any, TypeVar

import aiohttp
from aiohttp import ClientWebSocketResponse, WSMsgType
from dotenv import load_dotenv
from pydantic import BaseModel, Field, SecretStr, ValidationError

try:
    from loguru import logger
except ImportError:  # pragma: no cover - production installs loguru

    class _FallbackLogger:
        """Tiny subset of loguru used when optional logging deps are absent."""

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

try:
    from pydantic import ConfigDict, field_validator
except ImportError:  # pragma: no cover - compatibility with Pydantic v1
    ConfigDict = None
    from pydantic import validator as field_validator

if TYPE_CHECKING:  # pragma: no cover - only used by type checkers
    from .client import BinanceClient


_ModelT = TypeVar("_ModelT", bound=BaseModel)
EventCallback = Callable[["WebSocketEvent"], Awaitable[None] | None]


class BinanceWebSocketError(Exception):
    """Base exception for Binance WebSocket manager failures."""


class EventType(str, Enum):
    """Normalized event types emitted by :class:`BinanceWebSocketManager`."""

    TICKER_UPDATE = "TICKER_UPDATE"
    ORDER_BOOK_UPDATE = "ORDER_BOOK_UPDATE"
    KLINE_UPDATE = "KLINE_UPDATE"
    ACCOUNT_POSITION = "ACCOUNT_POSITION"
    BALANCE_UPDATE = "BALANCE_UPDATE"
    ORDER_UPDATE = "ORDER_UPDATE"
    USER_STREAM_EXPIRED = "USER_STREAM_EXPIRED"
    STREAM_ERROR = "STREAM_ERROR"
    RAW_MESSAGE = "RAW_MESSAGE"


class BinanceWebSocketConfig(BaseModel):
    """Validated runtime settings for :class:`BinanceWebSocketManager`."""

    api_key: SecretStr | None = None
    api_secret: SecretStr | None = None
    testnet: bool = False
    event_queue_size: int = Field(default=10_000, gt=0)
    subscriber_queue_size: int = Field(default=1_000, gt=0)
    dispatcher_workers: int = Field(default=2, gt=0)
    heartbeat_interval_seconds: float = Field(default=30.0, gt=0)
    message_timeout_seconds: float = Field(default=90.0, gt=0)
    listen_key_refresh_seconds: float = Field(default=30 * 60.0, gt=0)
    rest_timeout_seconds: float = Field(default=10.0, gt=0)
    ws_connect_timeout_seconds: float = Field(default=20.0, gt=0)
    base_backoff_seconds: float = Field(default=1.0, gt=0)
    max_backoff_seconds: float = Field(default=60.0, gt=0)

    @field_validator("api_key", "api_secret")
    def secret_must_not_be_blank(
        cls,
        value: SecretStr | None,
    ) -> SecretStr | None:
        """Validate credentials when they are supplied."""
        if value is not None and not value.get_secret_value().strip():
            raise ValueError("credential value must not be empty")
        return value

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class PriceLevel(BaseModel):
    """A single order-book price level."""

    price: float
    quantity: float


class TickerUpdate(BaseModel):
    """Validated ticker payload emitted from Binance ``@ticker`` streams."""

    symbol: str
    event_time: int | None = None
    price: float
    bid: float | None = None
    ask: float | None = None
    open_price: float | None = None
    high_price: float | None = None
    low_price: float | None = None
    base_volume: float | None = None
    quote_volume: float | None = None
    raw: dict[str, Any] = Field(default_factory=dict)


class OrderBookUpdate(BaseModel):
    """Validated L2 order-book update emitted from Binance depth streams."""

    symbol: str
    event_time: int | None = None
    last_update_id: int | None = None
    first_update_id: int | None = None
    final_update_id: int | None = None
    bids: list[PriceLevel] = Field(default_factory=list)
    asks: list[PriceLevel] = Field(default_factory=list)
    raw: dict[str, Any] = Field(default_factory=dict)


class KlineUpdate(BaseModel):
    """Validated candle update emitted from Binance ``@kline`` streams."""

    symbol: str
    interval: str
    event_time: int | None = None
    open_time: int
    close_time: int
    open: float
    high: float
    low: float
    close: float
    volume: float
    is_closed: bool
    raw: dict[str, Any] = Field(default_factory=dict)


class AccountBalance(BaseModel):
    """A balance row in an account-position event."""

    asset: str
    free: float
    locked: float


class AccountPositionUpdate(BaseModel):
    """Validated ``outboundAccountPosition`` user-data event."""

    event_time: int
    last_account_update_time: int | None = None
    balances: list[AccountBalance] = Field(default_factory=list)
    raw: dict[str, Any] = Field(default_factory=dict)


class BalanceUpdate(BaseModel):
    """Validated ``balanceUpdate`` user-data event."""

    event_time: int
    asset: str
    delta: float
    clear_time: int | None = None
    raw: dict[str, Any] = Field(default_factory=dict)


class ExecutionReportUpdate(BaseModel):
    """Validated ``executionReport`` order update event."""

    event_time: int
    symbol: str
    client_order_id: str
    side: str
    order_type: str
    time_in_force: str | None = None
    order_quantity: float | None = None
    order_price: float | None = None
    stop_price: float | None = None
    iceberg_quantity: float | None = None
    order_list_id: int | None = None
    original_client_order_id: str | None = None
    execution_type: str
    order_status: str
    reject_reason: str | None = None
    order_id: int
    last_executed_quantity: float | None = None
    cumulative_filled_quantity: float | None = None
    last_executed_price: float | None = None
    commission_amount: float | None = None
    commission_asset: str | None = None
    trade_time: int | None = None
    trade_id: int | None = None
    order_creation_time: int | None = None
    quote_order_quantity: float | None = None
    last_quote_transacted: float | None = None
    cumulative_quote_transacted: float | None = None
    is_maker: bool | None = None
    is_buyer: bool | None = None
    raw: dict[str, Any] = Field(default_factory=dict)


class UserStreamExpired(BaseModel):
    """Event emitted when Binance reports that the listen key expired."""

    event_time: int | None = None
    listen_key: str | None = None
    raw: dict[str, Any] = Field(default_factory=dict)


class StreamError(BaseModel):
    """Structured error event emitted by stream receiver tasks."""

    stream: str
    message: str
    error_type: str | None = None
    raw: dict[str, Any] = Field(default_factory=dict)


class RawMessage(BaseModel):
    """Fallback payload for valid but unsupported WebSocket messages."""

    stream: str
    raw: dict[str, Any]


WebSocketPayload = (
    TickerUpdate
    | OrderBookUpdate
    | KlineUpdate
    | AccountPositionUpdate
    | BalanceUpdate
    | ExecutionReportUpdate
    | UserStreamExpired
    | StreamError
    | RawMessage
)


class WebSocketEvent(BaseModel):
    """Envelope distributed to callbacks and subscriber queues."""

    event_type: EventType
    payload: WebSocketPayload
    received_at: datetime = Field(
        default_factory=lambda: datetime.now(timezone.utc)
    )


@dataclass(frozen=True)
class _PublicSubscription:
    """Internal representation of a public Binance stream subscription."""

    stream: str
    symbol: str
    event_type: EventType


class _ExponentialBackoff:
    """Small exponential backoff helper with jitter."""

    def __init__(self, base_seconds: float, max_seconds: float) -> None:
        self._base_seconds = base_seconds
        self._max_seconds = max_seconds
        self._attempt = 0

    def reset(self) -> None:
        """Reset the retry attempt counter after a successful connection."""
        self._attempt = 0

    def next_delay(self) -> float:
        """Return the next backoff delay in seconds."""
        delay = min(
            self._base_seconds * 2**self._attempt,
            self._max_seconds,
        )
        self._attempt += 1
        return delay + random.uniform(0, delay * 0.25)


class BinanceWebSocketManager:
    """Manage Binance market and user-data WebSocket feeds.

    Args:
        api_key: Optional Binance API key. Defaults to ``BINANCE_API_KEY`` from
            ``.env`` or the environment.
        api_secret: Optional Binance API secret. Required for signed
            WebSocket API user-data subscriptions.
        testnet: Whether to use Binance spot testnet endpoints. Defaults to
            ``BINANCE_TESTNET`` or a supplied REST client's config.
        rest_client: Optional :class:`BinanceClient` instance. The manager reads
            credentials and the testnet flag from it when explicit values are not
            supplied.
        session: Optional externally owned ``aiohttp.ClientSession``.
    """

    _PUBLIC_WS_URL = "wss://stream.binance.com:9443"
    _PUBLIC_TESTNET_WS_URL = "wss://stream.testnet.binance.vision"
    _USER_WS_API_URL = "wss://ws-api.binance.com:443/ws-api/v3"
    _USER_TESTNET_WS_API_URL = "wss://ws-api.testnet.binance.vision/ws-api/v3"

    def __init__(
        self,
        api_key: str | None = None,
        api_secret: str | None = None,
        testnet: bool | None = None,
        *,
        rest_client: "BinanceClient | None" = None,
        session: aiohttp.ClientSession | None = None,
        event_queue_size: int = 10_000,
        subscriber_queue_size: int = 1_000,
        dispatcher_workers: int = 2,
        heartbeat_interval_seconds: float = 30.0,
        message_timeout_seconds: float = 90.0,
        listen_key_refresh_seconds: float = 30 * 60.0,
        rest_timeout_seconds: float = 10.0,
        ws_connect_timeout_seconds: float = 20.0,
        base_backoff_seconds: float = 1.0,
        max_backoff_seconds: float = 60.0,
    ) -> None:
        load_dotenv()

        inferred_api_key = (
            api_key
            or self._secret_from_rest_client(rest_client, "api_key")
            or os.getenv("BINANCE_API_KEY")
            or os.getenv("API_KEY")
        )
        inferred_api_secret = (
            api_secret
            or self._secret_from_rest_client(rest_client, "api_secret")
            or os.getenv("BINANCE_API_SECRET")
            or os.getenv("API_SECRET")
        )
        inferred_testnet = (
            testnet
            if testnet is not None
            else self._testnet_from_rest_client(rest_client)
        )
        if inferred_testnet is None:
            inferred_testnet = self._env_bool("BINANCE_TESTNET", default=False)

        self.config = BinanceWebSocketConfig(
            api_key=SecretStr(inferred_api_key) if inferred_api_key else None,
            api_secret=(
                SecretStr(inferred_api_secret) if inferred_api_secret else None
            ),
            testnet=bool(inferred_testnet),
            event_queue_size=event_queue_size,
            subscriber_queue_size=subscriber_queue_size,
            dispatcher_workers=dispatcher_workers,
            heartbeat_interval_seconds=heartbeat_interval_seconds,
            message_timeout_seconds=message_timeout_seconds,
            listen_key_refresh_seconds=listen_key_refresh_seconds,
            rest_timeout_seconds=rest_timeout_seconds,
            ws_connect_timeout_seconds=ws_connect_timeout_seconds,
            base_backoff_seconds=base_backoff_seconds,
            max_backoff_seconds=max_backoff_seconds,
        )

        self._rest_client = rest_client
        self._session = session
        self._owns_session = session is None
        self._event_queue: asyncio.Queue[WebSocketEvent] = asyncio.Queue(
            maxsize=self.config.event_queue_size
        )
        self._callbacks: dict[EventType, set[EventCallback]] = {}
        self._subscriber_queues: dict[
            EventType,
            set[asyncio.Queue[WebSocketEvent]],
        ] = {}
        self._public_subscriptions: dict[str, _PublicSubscription] = {}
        self._symbol_lookup: dict[str, str] = {}
        self._subscription_lock = asyncio.Lock()
        self._subscription_changed = asyncio.Condition()
        self._public_ws_lock = asyncio.Lock()
        self._listen_key_lock = asyncio.Lock()
        self._tasks: set[asyncio.Task[Any]] = set()
        self._callback_tasks: set[asyncio.Task[Any]] = set()
        self._public_ws: ClientWebSocketResponse | None = None
        self._user_ws: ClientWebSocketResponse | None = None
        self._listen_key: str | None = None
        self._running = False
        self._user_stream_enabled = False
        self._stop_event = asyncio.Event()
        now = time.monotonic()
        self._last_public_message_at = now
        self._last_user_message_at = now
        self._dropped_events = 0
        self._dropped_subscriber_events = 0

    async def __aenter__(self) -> "BinanceWebSocketManager":
        """Start the manager for async context-manager use."""
        await self.start()
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        """Stop the manager when leaving an async context manager."""
        await self.stop()

    async def start(self, *, user_stream: bool = True) -> None:
        """Start dispatcher, public stream, health, and optional user stream.

        Public subscriptions can be added before or after this method is called.
        When ``user_stream`` is true, valid API credentials are required.
        """
        if self._running:
            return

        if user_stream:
            self._require_api_key()
            self._require_api_secret()

        await self._ensure_session()
        self._running = True
        self._user_stream_enabled = user_stream
        self._stop_event.clear()
        self._last_public_message_at = time.monotonic()
        self._last_user_message_at = time.monotonic()

        for worker_id in range(self.config.dispatcher_workers):
            self._track_task(
                asyncio.create_task(
                    self._dispatch_loop(worker_id),
                    name=f"binance-ws-dispatcher-{worker_id}",
                )
            )

        self._track_task(
            asyncio.create_task(
                self._public_stream_loop(),
                name="binance-public-ws",
            )
        )
        self._track_task(
            asyncio.create_task(
                self._health_loop(),
                name="binance-ws-health",
            )
        )
        if user_stream:
            self._track_task(
                asyncio.create_task(
                    self._user_stream_loop(),
                    name="binance-user-ws",
                )
            )

        async with self._subscription_changed:
            self._subscription_changed.notify_all()

        logger.bind(
            exchange="binance",
            testnet=self.config.testnet,
            user_stream=user_stream,
            public_subscriptions=len(self._public_subscriptions),
        ).info("Binance WebSocket manager started")

    async def stop(self) -> None:
        """Stop all WebSocket loops, dispatcher workers, and owned sessions."""
        if not self._running and not self._tasks:
            if self._owns_session and self._session is not None:
                await self._session.close()
                self._session = None
            return

        self._running = False
        self._user_stream_enabled = False
        self._stop_event.set()

        async with self._subscription_changed:
            self._subscription_changed.notify_all()

        await self._close_websocket(self._public_ws, "public")
        await self._close_websocket(self._user_ws, "user")
        self._public_ws = None
        self._user_ws = None

        for task in list(self._tasks):
            task.cancel()
        if self._tasks:
            await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

        for task in list(self._callback_tasks):
            task.cancel()
        if self._callback_tasks:
            await asyncio.gather(*self._callback_tasks, return_exceptions=True)
        self._callback_tasks.clear()

        if self._owns_session and self._session is not None:
            await self._session.close()
            self._session = None

        logger.bind(exchange="binance").info("Binance WebSocket manager stopped")

    async def close(self) -> None:
        """Alias for :meth:`stop`."""
        await self.stop()

    async def subscribe_ticker(self, symbols: Iterable[str] | str) -> None:
        """Subscribe to ticker updates for each symbol in ``symbols``.

        Symbols may be in ccxt format, such as ``"BTC/USDT"``, or raw Binance
        format, such as ``"BTCUSDT"``.
        """
        subscriptions = [
            _PublicSubscription(
                stream=f"{self._stream_symbol(symbol)}@ticker",
                symbol=self._clean_symbol(symbol),
                event_type=EventType.TICKER_UPDATE,
            )
            for symbol in self._iter_symbols(symbols)
        ]
        await self._add_public_subscriptions(subscriptions)

    async def subscribe_order_book(
        self,
        symbols: Iterable[str] | str,
        *,
        depth: int = 20,
        update_interval_ms: int = 100,
    ) -> None:
        """Subscribe to L2 order-book depth updates for each symbol.

        Binance partial book depth streams support depths of ``5``, ``10``, and
        ``20`` levels. ``update_interval_ms`` must be ``100`` or ``1000``.
        """
        if depth not in {5, 10, 20}:
            raise ValueError("depth must be one of 5, 10, or 20")
        if update_interval_ms not in {100, 1000}:
            raise ValueError("update_interval_ms must be 100 or 1000")

        interval_suffix = "@100ms" if update_interval_ms == 100 else ""
        subscriptions = [
            _PublicSubscription(
                stream=(
                    f"{self._stream_symbol(symbol)}"
                    f"@depth{depth}{interval_suffix}"
                ),
                symbol=self._clean_symbol(symbol),
                event_type=EventType.ORDER_BOOK_UPDATE,
            )
            for symbol in self._iter_symbols(symbols)
        ]
        await self._add_public_subscriptions(subscriptions)

    async def subscribe_klines(
        self,
        symbols: Iterable[str] | str,
        *,
        interval: str = "1m",
    ) -> None:
        """Subscribe to kline/candlestick updates for each symbol."""
        normalized_interval = interval.strip()
        if not normalized_interval:
            raise ValueError("interval must not be blank")

        subscriptions = [
            _PublicSubscription(
                stream=f"{self._stream_symbol(symbol)}@kline_{normalized_interval}",
                symbol=self._clean_symbol(symbol),
                event_type=EventType.KLINE_UPDATE,
            )
            for symbol in self._iter_symbols(symbols)
        ]
        await self._add_public_subscriptions(subscriptions)

    async def subscribe_market_data(
        self,
        symbols: Iterable[str] | str,
        *,
        ticker: bool = True,
        order_book: bool = True,
        order_book_depth: int = 20,
        order_book_update_interval_ms: int = 100,
    ) -> None:
        """Subscribe to ticker and/or L2 order-book streams for symbols."""
        symbol_list = list(self._iter_symbols(symbols))
        if ticker:
            await self.subscribe_ticker(symbol_list)
        if order_book:
            await self.subscribe_order_book(
                symbol_list,
                depth=order_book_depth,
                update_interval_ms=order_book_update_interval_ms,
            )

    async def unsubscribe_ticker(self, symbols: Iterable[str] | str) -> None:
        """Unsubscribe from ticker updates for each symbol in ``symbols``."""
        streams = [
            f"{self._stream_symbol(symbol)}@ticker"
            for symbol in self._iter_symbols(symbols)
        ]
        await self._remove_public_subscriptions(streams)

    async def unsubscribe_order_book(
        self,
        symbols: Iterable[str] | str,
        *,
        depth: int = 20,
        update_interval_ms: int = 100,
    ) -> None:
        """Unsubscribe from order-book updates for each symbol in ``symbols``."""
        interval_suffix = "@100ms" if update_interval_ms == 100 else ""
        streams = [
            f"{self._stream_symbol(symbol)}@depth{depth}{interval_suffix}"
            for symbol in self._iter_symbols(symbols)
        ]
        await self._remove_public_subscriptions(streams)

    async def unsubscribe_klines(
        self,
        symbols: Iterable[str] | str,
        *,
        interval: str = "1m",
    ) -> None:
        """Unsubscribe from kline updates for each symbol."""
        normalized_interval = interval.strip()
        if not normalized_interval:
            raise ValueError("interval must not be blank")
        streams = [
            f"{self._stream_symbol(symbol)}@kline_{normalized_interval}"
            for symbol in self._iter_symbols(symbols)
        ]
        await self._remove_public_subscriptions(streams)

    def subscribe(
        self,
        event_type: EventType | str,
        callback: EventCallback,
    ) -> Callable[[], None]:
        """Register a callback for an event type.

        The callback is invoked by dispatcher tasks, not by the WebSocket
        receiver loop. The returned function removes the callback.
        """
        normalized_event_type = self._normalize_event_type(event_type)
        self._callbacks.setdefault(normalized_event_type, set()).add(callback)

        def unsubscribe() -> None:
            callbacks = self._callbacks.get(normalized_event_type)
            if callbacks is not None:
                callbacks.discard(callback)
                if not callbacks:
                    self._callbacks.pop(normalized_event_type, None)

        return unsubscribe

    def subscribe_queue(
        self,
        event_type: EventType | str,
        *,
        maxsize: int | None = None,
    ) -> asyncio.Queue[WebSocketEvent]:
        """Create and register an ``asyncio.Queue`` for an event type."""
        normalized_event_type = self._normalize_event_type(event_type)
        queue: asyncio.Queue[WebSocketEvent] = asyncio.Queue(
            maxsize=maxsize or self.config.subscriber_queue_size
        )
        self._subscriber_queues.setdefault(normalized_event_type, set()).add(queue)
        return queue

    def unsubscribe_queue(
        self,
        event_type: EventType | str,
        queue: asyncio.Queue[WebSocketEvent],
    ) -> None:
        """Remove a previously registered subscriber queue."""
        normalized_event_type = self._normalize_event_type(event_type)
        queues = self._subscriber_queues.get(normalized_event_type)
        if queues is not None:
            queues.discard(queue)
            if not queues:
                self._subscriber_queues.pop(normalized_event_type, None)

    @property
    def listen_key(self) -> str | None:
        """Return the currently active Binance listen key, if any."""
        return self._listen_key

    @property
    def is_running(self) -> bool:
        """Return whether the manager has active background tasks."""
        return self._running

    async def _add_public_subscriptions(
        self,
        subscriptions: Iterable[_PublicSubscription],
    ) -> None:
        new_streams: list[str] = []
        async with self._subscription_lock:
            for subscription in subscriptions:
                if subscription.stream in self._public_subscriptions:
                    continue
                self._public_subscriptions[subscription.stream] = subscription
                stream_symbol = subscription.stream.split("@", 1)[0].upper()
                self._symbol_lookup[stream_symbol] = subscription.symbol
                new_streams.append(subscription.stream)

        if not new_streams:
            return

        logger.bind(
            exchange="binance",
            streams=new_streams,
        ).info("Binance public streams subscribed")

        await self._send_public_control("SUBSCRIBE", new_streams)
        async with self._subscription_changed:
            self._subscription_changed.notify_all()

    async def _remove_public_subscriptions(self, streams: Iterable[str]) -> None:
        removed_streams: list[str] = []
        async with self._subscription_lock:
            for stream in streams:
                if self._public_subscriptions.pop(stream, None) is not None:
                    removed_streams.append(stream)

        if not removed_streams:
            return

        logger.bind(
            exchange="binance",
            streams=removed_streams,
        ).info("Binance public streams unsubscribed")

        await self._send_public_control("UNSUBSCRIBE", removed_streams)
        if not await self._has_public_subscriptions():
            await self._close_websocket(self._public_ws, "public")
        async with self._subscription_changed:
            self._subscription_changed.notify_all()

    async def _public_stream_loop(self) -> None:
        """Maintain the combined public market-data WebSocket connection."""
        backoff = _ExponentialBackoff(
            self.config.base_backoff_seconds,
            self.config.max_backoff_seconds,
        )

        while self._running:
            await self._wait_for_public_subscriptions()
            if not self._running:
                break

            streams = await self._public_stream_snapshot()
            if not streams:
                continue

            url = f"{self._public_ws_base_url()}/stream?streams={'/'.join(streams)}"
            try:
                await self._ensure_session()
                logger.bind(
                    exchange="binance",
                    stream_count=len(streams),
                    testnet=self.config.testnet,
                ).info("Connecting Binance public WebSocket")

                async with self._session.ws_connect(
                    url,
                    heartbeat=self.config.heartbeat_interval_seconds,
                    **self._ws_connect_timeout_kwargs(
                        receive_timeout=self.config.message_timeout_seconds,
                    ),
                ) as websocket:
                    async with self._public_ws_lock:
                        self._public_ws = websocket
                    self._last_public_message_at = time.monotonic()
                    backoff.reset()
                    logger.bind(
                        exchange="binance",
                        stream_count=len(streams),
                    ).info("Binance public WebSocket connected")

                    if not await self._sync_public_connection_subscriptions(
                        websocket,
                        streams,
                    ):
                        continue

                    await self._receive_public_messages(websocket)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                if not self._running:
                    break
                delay = backoff.next_delay()
                logger.bind(
                    exchange="binance",
                    sleep_seconds=round(delay, 3),
                    error_type=exc.__class__.__name__,
                    error_message=str(exc),
                ).opt(exception=exc).warning(
                    "Binance public WebSocket disconnected; reconnecting"
                )
                await self._emit_stream_error(
                    "public",
                    "public WebSocket disconnected",
                    exc,
                )
                await self._sleep_or_stop(delay)
            finally:
                async with self._public_ws_lock:
                    if self._public_ws is not None and self._public_ws.closed:
                        self._public_ws = None

        logger.bind(exchange="binance").debug("Binance public stream loop exited")

    async def _receive_public_messages(
        self,
        websocket: ClientWebSocketResponse,
    ) -> None:
        async for message in websocket:
            if not self._running:
                break
            self._last_public_message_at = time.monotonic()

            if message.type == WSMsgType.TEXT:
                await self._handle_public_message(message.data)
                continue
            if message.type == WSMsgType.BINARY:
                await self._handle_public_message(message.data.decode())
                continue
            if message.type in {WSMsgType.CLOSED, WSMsgType.CLOSING, WSMsgType.CLOSE}:
                break
            if message.type == WSMsgType.ERROR:
                raise websocket.exception() or BinanceWebSocketError(
                    "public WebSocket error"
                )

    async def _user_stream_loop(self) -> None:
        """Maintain the authenticated user-data WebSocket connection."""
        backoff = _ExponentialBackoff(
            self.config.base_backoff_seconds,
            self.config.max_backoff_seconds,
        )

        while self._running and self._user_stream_enabled:
            try:
                await self._ensure_session()
                logger.bind(
                    exchange="binance",
                    testnet=self.config.testnet,
                ).info("Connecting Binance WebSocket API user-data stream")

                async with self._session.ws_connect(
                    self._user_ws_api_url(),
                    heartbeat=self.config.heartbeat_interval_seconds,
                    **self._ws_connect_timeout_kwargs(receive_timeout=None),
                ) as websocket:
                    self._user_ws = websocket
                    self._last_user_message_at = time.monotonic()
                    await self._subscribe_user_stream(websocket)
                    backoff.reset()
                    logger.bind(exchange="binance").info(
                        "Binance WebSocket API user-data stream subscribed"
                    )

                    await self._receive_user_messages(websocket)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                if not self._running or not self._user_stream_enabled:
                    break
                delay = backoff.next_delay()
                logger.bind(
                    exchange="binance",
                    sleep_seconds=round(delay, 3),
                    error_type=exc.__class__.__name__,
                    error_message=str(exc),
                ).opt(exception=exc).warning(
                    "Binance user-data WebSocket disconnected; reconnecting"
                )
                await self._emit_stream_error(
                    "user",
                    "user-data WebSocket disconnected",
                    exc,
                )
                await self._sleep_or_stop(delay)
            finally:
                if self._user_ws is not None and self._user_ws.closed:
                    self._user_ws = None

        logger.bind(exchange="binance").debug("Binance user stream loop exited")

    async def _receive_user_messages(
        self,
        websocket: ClientWebSocketResponse,
    ) -> None:
        async for message in websocket:
            if not self._running:
                break
            self._last_user_message_at = time.monotonic()

            if message.type == WSMsgType.TEXT:
                await self._handle_user_message(message.data)
                continue
            if message.type == WSMsgType.BINARY:
                await self._handle_user_message(message.data.decode())
                continue
            if message.type in {WSMsgType.CLOSED, WSMsgType.CLOSING, WSMsgType.CLOSE}:
                break
            if message.type == WSMsgType.ERROR:
                raise websocket.exception() or BinanceWebSocketError(
                    "user-data WebSocket error"
                )

    async def _subscribe_user_stream(
        self,
        websocket: ClientWebSocketResponse,
    ) -> None:
        api_key = self._require_api_key()
        api_secret = self._require_api_secret()
        params: dict[str, str | int] = {
            "apiKey": api_key,
            "timestamp": int(time.time() * 1000),
        }
        signature_payload = "&".join(
            f"{key}={params[key]}" for key in sorted(params)
        )
        params["signature"] = hmac.new(
            api_secret.encode(),
            signature_payload.encode(),
            hashlib.sha256,
        ).hexdigest()
        request_id = f"user-stream-{params['timestamp']}"
        await websocket.send_json(
            {
                "id": request_id,
                "method": "userDataStream.subscribe.signature",
                "params": params,
            }
        )
        message = await websocket.receive(timeout=self.config.rest_timeout_seconds)
        if message.type != WSMsgType.TEXT:
            raise BinanceWebSocketError(
                "Binance user-data subscription did not return a text response"
            )
        try:
            response = json.loads(message.data)
        except json.JSONDecodeError as exc:
            raise BinanceWebSocketError(
                "Binance user-data subscription response was not valid JSON"
            ) from exc
        if not isinstance(response, Mapping) or response.get("status") != 200:
            raise BinanceWebSocketError(
                f"Binance user-data subscription failed: {response}"
            )
        result = response.get("result")
        if not isinstance(result, Mapping) or "subscriptionId" not in result:
            raise BinanceWebSocketError(
                "Binance user-data subscription response omitted subscriptionId"
            )

    async def _handle_public_message(self, raw_message: str) -> None:
        try:
            message = json.loads(raw_message)
        except json.JSONDecodeError as exc:
            await self._emit_stream_error("public", "invalid JSON message", exc)
            return

        if self._is_control_response(message):
            logger.bind(exchange="binance", response=message).debug(
                "Binance public WebSocket control response received"
            )
            return

        data = message.get("data", message)
        stream = str(message.get("stream", "public"))
        if not isinstance(data, Mapping):
            await self._emit_raw_message("public", message)
            return

        if data.get("e") == "serverShutdown":
            await self._emit_raw_message(stream, dict(data))
            await self._close_websocket(self._public_ws, "public")
            return

        try:
            if self._is_ticker_payload(stream, data):
                await self._emit(
                    WebSocketEvent(
                        event_type=EventType.TICKER_UPDATE,
                        payload=self._parse_ticker(stream, data),
                    )
                )
                return
            if self._is_order_book_payload(stream, data):
                await self._emit(
                    WebSocketEvent(
                        event_type=EventType.ORDER_BOOK_UPDATE,
                        payload=self._parse_order_book(stream, data),
                    )
                )
                return
            if self._is_kline_payload(stream, data):
                await self._emit(
                    WebSocketEvent(
                        event_type=EventType.KLINE_UPDATE,
                        payload=self._parse_kline(stream, data),
                    )
                )
                return

            await self._emit_raw_message(stream, dict(data))
        except (ValidationError, ValueError, TypeError) as exc:
            logger.bind(
                exchange="binance",
                stream=stream,
                error_type=exc.__class__.__name__,
                error_message=str(exc),
            ).opt(exception=exc).error("Failed to validate public WebSocket payload")
            await self._emit_stream_error(
                stream,
                "failed to validate public WebSocket payload",
                exc,
                raw=dict(data),
            )

    async def _handle_user_message(self, raw_message: str) -> None:
        try:
            message = json.loads(raw_message)
        except json.JSONDecodeError as exc:
            await self._emit_stream_error("user", "invalid JSON message", exc)
            return

        if not isinstance(message, Mapping):
            await self._emit_raw_message("user", {"message": message})
            return

        event_payload = self._user_event_payload(message)
        event_name = str(event_payload.get("e", ""))
        try:
            if event_name == "outboundAccountPosition":
                await self._emit(
                    WebSocketEvent(
                        event_type=EventType.ACCOUNT_POSITION,
                        payload=self._parse_account_position(event_payload),
                    )
                )
                return
            if event_name == "balanceUpdate":
                await self._emit(
                    WebSocketEvent(
                        event_type=EventType.BALANCE_UPDATE,
                        payload=self._parse_balance_update(event_payload),
                    )
                )
                return
            if event_name == "executionReport":
                await self._emit(
                    WebSocketEvent(
                        event_type=EventType.ORDER_UPDATE,
                        payload=self._parse_execution_report(event_payload),
                    )
                )
                return
            if event_name in {"eventStreamTerminated", "listenKeyExpired"}:
                logger.bind(exchange="binance").warning(
                    "Binance user-data stream terminated"
                )
                await self._emit(
                    WebSocketEvent(
                        event_type=EventType.USER_STREAM_EXPIRED,
                        payload=self._validate_model(
                            UserStreamExpired,
                            {
                                "event_time": self._optional_int(
                                    event_payload.get("E")
                                ),
                                "listen_key": self._listen_key,
                                "raw": dict(message),
                            },
                        ),
                    )
                )
                async with self._listen_key_lock:
                    self._listen_key = None
                await self._close_websocket(self._user_ws, "user")
                return

            await self._emit_raw_message("user", dict(message))
        except (ValidationError, ValueError, TypeError) as exc:
            logger.bind(
                exchange="binance",
                event_name=event_name,
                error_type=exc.__class__.__name__,
                error_message=str(exc),
            ).opt(exception=exc).error("Failed to validate user-data payload")
            await self._emit_stream_error(
                "user",
                "failed to validate user-data payload",
                exc,
                raw=dict(message),
            )

    async def _dispatch_loop(self, worker_id: int) -> None:
        """Fan out validated events to queues and callbacks."""
        while self._running or not self._event_queue.empty():
            try:
                event = await asyncio.wait_for(self._event_queue.get(), timeout=0.5)
            except TimeoutError:
                continue
            except asyncio.CancelledError:
                raise

            try:
                self._dispatch_to_subscriber_queues(event)
                self._dispatch_to_callbacks(event)
            finally:
                self._event_queue.task_done()

        logger.bind(exchange="binance", worker_id=worker_id).debug(
            "Binance WebSocket dispatcher exited"
        )

    def _dispatch_to_subscriber_queues(self, event: WebSocketEvent) -> None:
        queues = list(self._subscriber_queues.get(event.event_type, set()))
        for queue in queues:
            self._put_queue_nowait(queue, event, subscriber_queue=True)

    def _dispatch_to_callbacks(self, event: WebSocketEvent) -> None:
        callbacks = list(self._callbacks.get(event.event_type, set()))
        for callback in callbacks:
            task = asyncio.create_task(
                self._invoke_callback(callback, event),
                name=f"binance-ws-callback-{event.event_type.value}",
            )
            self._callback_tasks.add(task)
            task.add_done_callback(self._callback_tasks.discard)

    async def _invoke_callback(
        self,
        callback: EventCallback,
        event: WebSocketEvent,
    ) -> None:
        try:
            if self._is_async_callable(callback):
                result = callback(event)
            else:
                result = await asyncio.to_thread(callback, event)

            if inspect.isawaitable(result):
                await result
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            logger.bind(
                exchange="binance",
                event_type=event.event_type.value,
                error_type=exc.__class__.__name__,
                error_message=str(exc),
            ).opt(exception=exc).error("Binance WebSocket callback failed")

    async def _emit(self, event: WebSocketEvent) -> None:
        self._put_queue_nowait(self._event_queue, event)

    async def _emit_raw_message(self, stream: str, raw: dict[str, Any]) -> None:
        await self._emit(
            WebSocketEvent(
                event_type=EventType.RAW_MESSAGE,
                payload=self._validate_model(
                    RawMessage,
                    {"stream": stream, "raw": raw},
                ),
            )
        )

    async def _emit_stream_error(
        self,
        stream: str,
        message: str,
        exc: Exception | None = None,
        *,
        raw: dict[str, Any] | None = None,
    ) -> None:
        await self._emit(
            WebSocketEvent(
                event_type=EventType.STREAM_ERROR,
                payload=self._validate_model(
                    StreamError,
                    {
                        "stream": stream,
                        "message": message,
                        "error_type": exc.__class__.__name__ if exc else None,
                        "raw": raw or {},
                    },
                ),
            )
        )

    def _put_queue_nowait(
        self,
        queue: asyncio.Queue[WebSocketEvent],
        event: WebSocketEvent,
        *,
        subscriber_queue: bool = False,
    ) -> None:
        try:
            queue.put_nowait(event)
            return
        except asyncio.QueueFull:
            if self._drop_oldest_queue_item(queue):
                if subscriber_queue:
                    self._dropped_subscriber_events += 1
                else:
                    self._dropped_events += 1
                self._log_queue_drop(subscriber_queue=subscriber_queue)

        try:
            queue.put_nowait(event)
        except asyncio.QueueFull:
            if subscriber_queue:
                self._dropped_subscriber_events += 1
            else:
                self._dropped_events += 1
            self._log_queue_drop(subscriber_queue=subscriber_queue)

    def _drop_oldest_queue_item(
        self,
        queue: asyncio.Queue[WebSocketEvent],
    ) -> bool:
        try:
            queue.get_nowait()
            queue.task_done()
            return True
        except asyncio.QueueEmpty:
            return False

    def _log_queue_drop(self, *, subscriber_queue: bool) -> None:
        dropped = (
            self._dropped_subscriber_events
            if subscriber_queue
            else self._dropped_events
        )
        if dropped == 1 or dropped % 100 == 0:
            logger.bind(
                exchange="binance",
                dropped_events=dropped,
                subscriber_queue=subscriber_queue,
            ).warning("Binance WebSocket event queue is dropping events")

    async def _health_loop(self) -> None:
        """Log stream health and force reconnects after message timeouts."""
        while self._running:
            await self._sleep_or_stop(self.config.heartbeat_interval_seconds)
            if not self._running:
                break

            now = time.monotonic()
            public_age = now - self._last_public_message_at
            user_age = now - self._last_user_message_at
            logger.bind(
                exchange="binance",
                public_connected=self._public_ws is not None
                and not self._public_ws.closed,
                user_connected=self._user_ws is not None and not self._user_ws.closed,
                public_last_message_seconds=round(public_age, 3),
                user_last_message_seconds=round(user_age, 3),
                queued_events=self._event_queue.qsize(),
            ).debug("Binance WebSocket heartbeat")

            if await self._has_public_subscriptions() and self._is_ws_stale(
                self._public_ws,
                public_age,
            ):
                logger.bind(
                    exchange="binance",
                    idle_seconds=round(public_age, 3),
                ).warning("Binance public WebSocket heartbeat timeout")
                await self._close_websocket(self._public_ws, "public")

    def _parse_ticker(
        self,
        stream: str,
        payload: Mapping[str, Any],
    ) -> TickerUpdate:
        symbol = self._payload_symbol(stream, payload)
        price = self._optional_float(payload.get("c"))
        if price is None:
            price = self._optional_float(payload.get("last"))
        if price is None:
            raise ValueError("ticker payload did not include a price")

        return self._validate_model(
            TickerUpdate,
            {
                "symbol": symbol,
                "event_time": self._optional_int(payload.get("E")),
                "price": price,
                "bid": self._optional_float(payload.get("b")),
                "ask": self._optional_float(payload.get("a")),
                "open_price": self._optional_float(payload.get("o")),
                "high_price": self._optional_float(payload.get("h")),
                "low_price": self._optional_float(payload.get("l")),
                "base_volume": self._optional_float(payload.get("v")),
                "quote_volume": self._optional_float(payload.get("q")),
                "raw": dict(payload),
            },
        )

    def _parse_order_book(
        self,
        stream: str,
        payload: Mapping[str, Any],
    ) -> OrderBookUpdate:
        bids = payload.get("bids", payload.get("b", []))
        asks = payload.get("asks", payload.get("a", []))
        return self._validate_model(
            OrderBookUpdate,
            {
                "symbol": self._payload_symbol(stream, payload),
                "event_time": self._optional_int(payload.get("E")),
                "last_update_id": self._optional_int(payload.get("lastUpdateId")),
                "first_update_id": self._optional_int(payload.get("U")),
                "final_update_id": self._optional_int(payload.get("u")),
                "bids": self._parse_price_levels(bids),
                "asks": self._parse_price_levels(asks),
                "raw": dict(payload),
            },
        )

    def _parse_kline(
        self,
        stream: str,
        payload: Mapping[str, Any],
    ) -> KlineUpdate:
        kline = payload.get("k", payload)
        if not isinstance(kline, Mapping):
            raise ValueError("kline payload did not include candle data")

        symbol = self._payload_symbol(stream, payload)
        if symbol == "UNKNOWN":
            symbol = self._payload_symbol(stream, kline)

        return self._validate_model(
            KlineUpdate,
            {
                "symbol": symbol,
                "interval": str(kline.get("i", "")),
                "event_time": self._optional_int(payload.get("E")),
                "open_time": self._int(kline.get("t")),
                "close_time": self._int(kline.get("T")),
                "open": self._float(kline.get("o")),
                "high": self._float(kline.get("h")),
                "low": self._float(kline.get("l")),
                "close": self._float(kline.get("c")),
                "volume": self._float(kline.get("v")),
                "is_closed": self._bool(kline.get("x")),
                "raw": dict(payload),
            },
        )

    def _parse_account_position(
        self,
        payload: Mapping[str, Any],
    ) -> AccountPositionUpdate:
        balances = []
        for balance in payload.get("B", []):
            if not isinstance(balance, Mapping):
                continue
            balances.append(
                {
                    "asset": str(balance.get("a", "")),
                    "free": self._float(balance.get("f")),
                    "locked": self._float(balance.get("l")),
                }
            )

        return self._validate_model(
            AccountPositionUpdate,
            {
                "event_time": self._int(payload.get("E")),
                "last_account_update_time": self._optional_int(payload.get("u")),
                "balances": balances,
                "raw": dict(payload),
            },
        )

    def _parse_balance_update(
        self,
        payload: Mapping[str, Any],
    ) -> BalanceUpdate:
        return self._validate_model(
            BalanceUpdate,
            {
                "event_time": self._int(payload.get("E")),
                "asset": str(payload.get("a", "")),
                "delta": self._float(payload.get("d")),
                "clear_time": self._optional_int(payload.get("T")),
                "raw": dict(payload),
            },
        )

    def _parse_execution_report(
        self,
        payload: Mapping[str, Any],
    ) -> ExecutionReportUpdate:
        return self._validate_model(
            ExecutionReportUpdate,
            {
                "event_time": self._int(payload.get("E")),
                "symbol": self._payload_symbol("user", payload),
                "client_order_id": str(payload.get("c", "")),
                "side": str(payload.get("S", "")),
                "order_type": str(payload.get("o", "")),
                "time_in_force": self._optional_str(payload.get("f")),
                "order_quantity": self._optional_float(payload.get("q")),
                "order_price": self._optional_float(payload.get("p")),
                "stop_price": self._optional_float(payload.get("P")),
                "iceberg_quantity": self._optional_float(payload.get("F")),
                "order_list_id": self._optional_int(payload.get("g")),
                "original_client_order_id": self._optional_str(payload.get("C")),
                "execution_type": str(payload.get("x", "")),
                "order_status": str(payload.get("X", "")),
                "reject_reason": self._optional_str(payload.get("r")),
                "order_id": self._int(payload.get("i")),
                "last_executed_quantity": self._optional_float(payload.get("l")),
                "cumulative_filled_quantity": self._optional_float(
                    payload.get("z")
                ),
                "last_executed_price": self._optional_float(payload.get("L")),
                "commission_amount": self._optional_float(payload.get("n")),
                "commission_asset": self._optional_str(payload.get("N")),
                "trade_time": self._optional_int(payload.get("T")),
                "trade_id": self._optional_int(payload.get("t")),
                "order_creation_time": self._optional_int(payload.get("O")),
                "quote_order_quantity": self._optional_float(payload.get("Q")),
                "last_quote_transacted": self._optional_float(payload.get("Y")),
                "cumulative_quote_transacted": self._optional_float(
                    payload.get("Z")
                ),
                "is_maker": self._optional_bool(payload.get("m")),
                "is_buyer": self._optional_bool(payload.get("M")),
                "raw": dict(payload),
            },
        )

    def _parse_price_levels(self, levels: Any) -> list[PriceLevel]:
        if not isinstance(levels, list):
            return []

        parsed_levels: list[PriceLevel] = []
        for level in levels:
            if not isinstance(level, (list, tuple)) or len(level) < 2:
                continue
            parsed_levels.append(
                self._validate_model(
                    PriceLevel,
                    {
                        "price": self._float(level[0]),
                        "quantity": self._float(level[1]),
                    },
                )
            )
        return parsed_levels

    def _payload_symbol(
        self,
        stream: str,
        payload: Mapping[str, Any],
    ) -> str:
        raw_symbol = payload.get("s")
        if not raw_symbol and "@" in stream:
            raw_symbol = stream.split("@", 1)[0].upper()
        if raw_symbol:
            return self._symbol_lookup.get(str(raw_symbol).upper(), str(raw_symbol))
        return "UNKNOWN"

    def _user_event_payload(self, message: Mapping[str, Any]) -> Mapping[str, Any]:
        """Return user-data event body for legacy and WebSocket API envelopes."""
        event = message.get("event")
        if isinstance(event, Mapping):
            return event
        return message

    def _is_ticker_payload(self, stream: str, payload: Mapping[str, Any]) -> bool:
        return payload.get("e") == "24hrTicker" or stream.endswith("@ticker")

    def _is_order_book_payload(
        self,
        stream: str,
        payload: Mapping[str, Any],
    ) -> bool:
        return (
            "depth" in stream
            or payload.get("e") == "depthUpdate"
            or "lastUpdateId" in payload
            or ("b" in payload and "a" in payload and "u" in payload)
        )

    def _is_kline_payload(self, stream: str, payload: Mapping[str, Any]) -> bool:
        return (
            payload.get("e") == "kline"
            or "@kline_" in stream
            or isinstance(payload.get("k"), Mapping)
        )

    def _is_control_response(self, message: Any) -> bool:
        return (
            isinstance(message, Mapping)
            and "id" in message
            and ("result" in message or "error" in message)
        )

    async def _wait_for_public_subscriptions(self) -> None:
        async with self._subscription_changed:
            while self._running and not await self._has_public_subscriptions():
                await self._subscription_changed.wait()

    async def _public_stream_snapshot(self) -> list[str]:
        async with self._subscription_lock:
            return sorted(self._public_subscriptions)

    async def _has_public_subscriptions(self) -> bool:
        async with self._subscription_lock:
            return bool(self._public_subscriptions)

    async def _send_public_control(self, method: str, streams: list[str]) -> None:
        if not self._running or not streams:
            return

        async with self._public_ws_lock:
            websocket = self._public_ws
            if websocket is None or websocket.closed:
                return
            await self._send_public_control_to(websocket, method, streams)

    async def _sync_public_connection_subscriptions(
        self,
        websocket: ClientWebSocketResponse,
        connected_streams: list[str],
    ) -> bool:
        """Reconcile subscription changes that happened during connection setup."""
        current_streams = await self._public_stream_snapshot()
        if not current_streams:
            await self._close_websocket(websocket, "public")
            return False

        current = set(current_streams)
        connected = set(connected_streams)
        stale_streams = sorted(connected - current)
        new_streams = sorted(current - connected)
        if stale_streams:
            await self._send_public_control_to(
                websocket,
                "UNSUBSCRIBE",
                stale_streams,
            )
        if new_streams:
            await self._send_public_control_to(websocket, "SUBSCRIBE", new_streams)
        return True

    async def _send_public_control_to(
        self,
        websocket: ClientWebSocketResponse,
        method: str,
        streams: list[str],
    ) -> None:
        request_id = int(time.time() * 1000) % 1_000_000_000
        await websocket.send_json(
            {
                "method": method,
                "params": streams,
                "id": request_id,
            }
        )

    async def _close_websocket(
        self,
        websocket: ClientWebSocketResponse | None,
        stream_name: str,
    ) -> None:
        if websocket is None or websocket.closed:
            return
        try:
            await websocket.close()
            logger.bind(exchange="binance", stream=stream_name).debug(
                "Binance WebSocket closed"
            )
        except Exception as exc:
            logger.bind(
                exchange="binance",
                stream=stream_name,
                error_type=exc.__class__.__name__,
                error_message=str(exc),
            ).opt(exception=exc).debug("Failed to close Binance WebSocket")

    async def _ensure_session(self) -> None:
        if self._session is None or self._session.closed:
            timeout = aiohttp.ClientTimeout(
                total=None,
                connect=self.config.ws_connect_timeout_seconds,
                sock_connect=self.config.ws_connect_timeout_seconds,
            )
            self._session = aiohttp.ClientSession(timeout=timeout)
            self._owns_session = True

    async def _sleep_or_stop(self, seconds: float) -> None:
        try:
            await asyncio.wait_for(self._stop_event.wait(), timeout=seconds)
        except TimeoutError:
            return

    def _track_task(self, task: asyncio.Task[Any]) -> None:
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)

    def _public_ws_base_url(self) -> str:
        if self.config.testnet:
            return self._PUBLIC_TESTNET_WS_URL
        return self._PUBLIC_WS_URL

    def _user_ws_api_url(self) -> str:
        if self.config.testnet:
            return self._USER_TESTNET_WS_API_URL
        return self._USER_WS_API_URL

    def _ws_connect_timeout_kwargs(
        self,
        *,
        receive_timeout: float | None,
    ) -> dict[str, Any]:
        ws_timeout = getattr(aiohttp, "ClientWSTimeout", None)
        if ws_timeout is None:
            return {
                "timeout": self.config.ws_connect_timeout_seconds,
                "receive_timeout": receive_timeout,
            }
        return {
            "timeout": ws_timeout(
                ws_receive=receive_timeout,
                ws_close=10.0,
            )
        }

    def _public_ws_timeout(self) -> Any:
        ws_timeout = getattr(aiohttp, "ClientWSTimeout", None)
        if ws_timeout is None:
            return self.config.ws_connect_timeout_seconds
        return ws_timeout(
            ws_receive=self.config.message_timeout_seconds,
            ws_close=10.0,
        )

    def _user_ws_timeout(self) -> Any:
        ws_timeout = getattr(aiohttp, "ClientWSTimeout", None)
        if ws_timeout is None:
            return self.config.ws_connect_timeout_seconds
        return ws_timeout(ws_receive=None, ws_close=10.0)

    def _require_api_key(self) -> str:
        if self.config.api_key is None:
            raise BinanceWebSocketError(
                "api_key is required for Binance user data stream"
            )
        return self.config.api_key.get_secret_value()

    def _require_api_secret(self) -> str:
        if self.config.api_secret is None:
            raise BinanceWebSocketError(
                "api_secret is required for Binance user data stream"
            )
        return self.config.api_secret.get_secret_value()

    def _is_ws_stale(
        self,
        websocket: ClientWebSocketResponse | None,
        idle_seconds: float,
    ) -> bool:
        return (
            websocket is not None
            and not websocket.closed
            and idle_seconds > self.config.message_timeout_seconds
        )

    def _clean_symbol(self, symbol: str) -> str:
        normalized = symbol.strip().upper()
        if not normalized:
            raise ValueError("symbol must not be blank")
        return normalized

    def _stream_symbol(self, symbol: str) -> str:
        normalized = self._clean_symbol(symbol)
        if "/" in normalized:
            base, quote = normalized.split("/", 1)
            quote = quote.split(":", 1)[0]
            normalized = f"{base}{quote}"
        return normalized.replace("-", "").replace("_", "").lower()

    def _iter_symbols(self, symbols: Iterable[str] | str) -> Iterable[str]:
        if isinstance(symbols, str):
            yield self._clean_symbol(symbols)
            return
        for symbol in symbols:
            yield self._clean_symbol(symbol)

    def _normalize_event_type(self, event_type: EventType | str) -> EventType:
        if isinstance(event_type, EventType):
            return event_type
        try:
            return EventType(event_type)
        except ValueError:
            return EventType[event_type]

    def _validate_model(
        self,
        model_class: type[_ModelT],
        data: Mapping[str, Any],
    ) -> _ModelT:
        if hasattr(model_class, "model_validate"):
            return model_class.model_validate(data)
        return model_class.parse_obj(data)

    @staticmethod
    def _secret_from_rest_client(
        rest_client: "BinanceClient | None",
        field_name: str,
    ) -> str | None:
        config = getattr(rest_client, "config", None)
        value = getattr(config, field_name, None)
        if value is None:
            return None
        if hasattr(value, "get_secret_value"):
            return value.get_secret_value()
        return str(value)

    @staticmethod
    def _testnet_from_rest_client(
        rest_client: "BinanceClient | None",
    ) -> bool | None:
        config = getattr(rest_client, "config", None)
        value = getattr(config, "testnet", None)
        if value is None:
            return None
        return bool(value)

    @staticmethod
    def _env_bool(name: str, *, default: bool) -> bool:
        value = os.getenv(name)
        if value is None:
            return default
        return value.strip().lower() in {"1", "true", "yes", "y", "on"}

    @staticmethod
    def _float(value: Any) -> float:
        if value is None:
            raise ValueError("numeric value is required")
        return float(value)

    @staticmethod
    def _optional_float(value: Any) -> float | None:
        if value is None or value == "":
            return None
        return float(value)

    @staticmethod
    def _int(value: Any) -> int:
        if value is None:
            raise ValueError("integer value is required")
        return int(value)

    @staticmethod
    def _optional_int(value: Any) -> int | None:
        if value is None or value == "":
            return None
        return int(value)

    @staticmethod
    def _optional_bool(value: Any) -> bool | None:
        if value is None:
            return None
        if isinstance(value, bool):
            return value
        if isinstance(value, str):
            normalized = value.strip().lower()
            if normalized in {"true", "1", "yes", "y"}:
                return True
            if normalized in {"false", "0", "no", "n"}:
                return False
        return bool(value)

    @staticmethod
    def _bool(value: Any) -> bool:
        if value is None:
            raise ValueError("boolean value is required")
        parsed = BinanceWebSocketManager._optional_bool(value)
        if parsed is None:
            raise ValueError("boolean value is required")
        return parsed

    @staticmethod
    def _optional_str(value: Any) -> str | None:
        if value is None:
            return None
        text = str(value)
        return text if text else None

    @staticmethod
    def _is_async_callable(callback: EventCallback) -> bool:
        return inspect.iscoroutinefunction(callback) or inspect.iscoroutinefunction(
            getattr(callback, "__call__", None)
        )


__all__ = [
    "AccountBalance",
    "AccountPositionUpdate",
    "BalanceUpdate",
    "BinanceWebSocketConfig",
    "BinanceWebSocketError",
    "BinanceWebSocketManager",
    "EventType",
    "ExecutionReportUpdate",
    "KlineUpdate",
    "OrderBookUpdate",
    "PriceLevel",
    "RawMessage",
    "StreamError",
    "TickerUpdate",
    "UserStreamExpired",
    "WebSocketEvent",
]

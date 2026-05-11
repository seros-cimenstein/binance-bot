"""Asynchronous Binance REST client built on top of ccxt."""

from __future__ import annotations

import asyncio
import math
import random
import re
import time
from collections import deque
from collections.abc import Awaitable, Callable, Mapping
from types import TracebackType
from typing import Any

import ccxt
import ccxt.async_support as ccxt_async
from loguru import logger
from pydantic import BaseModel, Field, SecretStr

try:
    from pydantic import ConfigDict, field_validator
except ImportError:  # pragma: no cover - compatibility with Pydantic v1
    ConfigDict = None
    from pydantic import validator as field_validator


class BinanceClientError(Exception):
    """Base exception for Binance client failures."""

    def __init__(
        self,
        message: str,
        *,
        context: Mapping[str, Any] | None = None,
        error_code: str | int | None = None,
        http_status: int | None = None,
    ) -> None:
        """Initialize a domain exception.

        Args:
            message: Human-readable error description.
            context: Sanitized request context related to the failure.
            error_code: Exchange-specific error code when available.
            http_status: HTTP status code when available.
        """
        super().__init__(message)
        self.context = dict(context or {})
        self.error_code = error_code
        self.http_status = http_status


class AuthenticationError(BinanceClientError):
    """Raised when Binance credentials are invalid or unauthorized."""


class InsufficientFundsError(BinanceClientError):
    """Raised when an order cannot be funded."""


class InvalidOrderError(BinanceClientError):
    """Raised when an order request is invalid."""


class RateLimitError(BinanceClientError):
    """Raised when Binance rate limits remain exhausted after retries."""


class BinanceNetworkError(BinanceClientError):
    """Raised when Binance cannot be reached reliably."""


class ExchangeUnavailableError(BinanceClientError):
    """Raised when Binance returns a transient server-side failure."""


class BinanceClientConfig(BaseModel):
    """Validated runtime settings for :class:`BinanceClient`."""

    api_key: SecretStr
    api_secret: SecretStr
    testnet: bool = False
    request_weight_limit: int = Field(default=1_100, gt=0)
    request_weight_period_seconds: float = Field(default=60.0, gt=0)
    max_retries: int = Field(default=4, ge=0)
    base_backoff_seconds: float = Field(default=0.5, gt=0)
    max_backoff_seconds: float = Field(default=8.0, gt=0)

    @field_validator("api_key", "api_secret")
    def secret_must_not_be_empty(cls, value: SecretStr) -> SecretStr:
        """Validate that required credentials are not blank."""
        if not value.get_secret_value().strip():
            raise ValueError("credential value must not be empty")
        return value

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class AsyncWeightedRateLimiter:
    """FIFO queued sliding-window async limiter for Binance request weights."""

    def __init__(self, limit: int, period_seconds: float) -> None:
        """Initialize the limiter.

        Args:
            limit: Maximum request weight allowed within the period.
            period_seconds: Rolling window length in seconds.
        """
        if limit <= 0:
            raise ValueError("limit must be greater than zero")
        if period_seconds <= 0:
            raise ValueError("period_seconds must be greater than zero")

        self._limit = limit
        self._period_seconds = period_seconds
        self._events: deque[tuple[float, int]] = deque()
        self._pending_queue: deque[object] = deque()
        self._used_weight = 0
        self._lock = asyncio.Lock()

    async def acquire(self, weight: int) -> None:
        """Reserve request weight before an API call is sent.

        Args:
            weight: Binance request weight for the endpoint.

        Raises:
            ValueError: If a single request exceeds the configured window.
        """
        if weight <= 0:
            return
        if weight > self._limit:
            raise ValueError("request weight exceeds configured limit")

        ticket = object()
        async with self._lock:
            self._pending_queue.append(ticket)

        acquired = False
        try:
            while True:
                async with self._lock:
                    now = time.monotonic()
                    self._prune(now)

                    is_next_request = self._pending_queue[0] is ticket
                    has_capacity = self._used_weight + weight <= self._limit
                    if is_next_request and has_capacity:
                        self._pending_queue.popleft()
                        self._events.append((now, weight))
                        self._used_weight += weight
                        acquired = True
                        return

                    if is_next_request and self._events:
                        oldest_time, _ = self._events[0]
                        sleep_seconds = max(
                            oldest_time + self._period_seconds - now,
                            0.05,
                        )
                    else:
                        sleep_seconds = 0.05

                    used_weight = self._used_weight
                    queue_depth = len(self._pending_queue)

                logger.bind(
                    request_weight=weight,
                    used_weight=used_weight,
                    weight_limit=self._limit,
                    queue_depth=queue_depth,
                    sleep_seconds=round(sleep_seconds, 3),
                ).debug("Binance request weight throttled")
                await asyncio.sleep(sleep_seconds)
        finally:
            if not acquired:
                async with self._lock:
                    try:
                        self._pending_queue.remove(ticket)
                    except ValueError:
                        pass

    def _prune(self, now: float) -> None:
        """Drop expired weight reservations from the rolling window."""
        cutoff = now - self._period_seconds
        while self._events and self._events[0][0] <= cutoff:
            _, weight = self._events.popleft()
            self._used_weight -= weight


class BinanceClient:
    """Production-grade asynchronous Binance REST client."""

    _BALANCE_WEIGHT = 20
    _ORDER_WEIGHT = 1
    _TICKER_WEIGHT = 2
    _ERROR_CODE_RE = re.compile(r"""["']code["']\s*:\s*["']?(-?\d+)""")
    _HTTP_STATUS_RE = re.compile(r"\b([45]\d{2})\b")
    _SENSITIVE_KEYS = {
        "api_key",
        "api_secret",
        "apikey",
        "secret",
        "password",
        "signature",
        "token",
    }
    _TRANSIENT_EXCHANGE_ERROR_MARKERS = (
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "temporarily unavailable",
        "server error",
        "maintenance",
    )

    def __init__(
        self,
        api_key: str,
        api_secret: str,
        testnet: bool = False,
        *,
        request_weight_limit: int = 1_100,
        request_weight_period_seconds: float = 60.0,
        max_retries: int = 4,
        base_backoff_seconds: float = 0.5,
        max_backoff_seconds: float = 8.0,
    ) -> None:
        """Initialize an authenticated Binance REST client.

        Args:
            api_key: Binance API key.
            api_secret: Binance API secret.
            testnet: Whether to route requests to Binance testnet.
            request_weight_limit: Client-side rolling request weight budget.
            request_weight_period_seconds: Rolling window for request weights.
            max_retries: Number of retry attempts after the initial request.
            base_backoff_seconds: Initial exponential backoff delay.
            max_backoff_seconds: Maximum retry delay.
        """
        self.config = BinanceClientConfig(
            api_key=api_key,
            api_secret=api_secret,
            testnet=testnet,
            request_weight_limit=request_weight_limit,
            request_weight_period_seconds=request_weight_period_seconds,
            max_retries=max_retries,
            base_backoff_seconds=base_backoff_seconds,
            max_backoff_seconds=max_backoff_seconds,
        )
        self.exchange = ccxt_async.binance(
            {
                "apiKey": self.config.api_key.get_secret_value(),
                "secret": self.config.api_secret.get_secret_value(),
                "enableRateLimit": True,
                "options": {
                    "adjustForTimeDifference": True,
                    "recvWindow": 10_000,
                },
            }
        )
        if self.config.testnet:
            self.exchange.set_sandbox_mode(True)

        self._rate_limiter = AsyncWeightedRateLimiter(
            limit=self.config.request_weight_limit,
            period_seconds=self.config.request_weight_period_seconds,
        )
        self._closed = False

    async def __aenter__(self) -> BinanceClient:
        """Return the client for async context manager use."""
        self._ensure_open()
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        """Close the underlying HTTP session when leaving the context."""
        await self.close()

    async def close(self) -> None:
        """Close the underlying aiohttp session owned by ccxt."""
        if self._closed:
            return

        await self.exchange.close()
        self._closed = True
        logger.bind(exchange="binance").debug("Binance client closed")

    async def fetch_balance(self) -> dict[str, Any]:
        """Fetch the raw ccxt account balance payload."""
        return await self._request(
            operation="fetch_balance",
            request_params={},
            request_weight=self._BALANCE_WEIGHT,
            call=lambda: self.exchange.fetch_balance(),
        )

    async def fetch_ticker(self, symbol: str) -> dict[str, Any]:
        """Fetch the raw ccxt ticker payload for a market symbol."""
        symbol_normalized = symbol.strip()
        if not symbol_normalized:
            raise BinanceClientError("symbol must not be blank")

        return await self._request(
            operation="fetch_ticker",
            request_params={"symbol": symbol_normalized},
            request_weight=self._TICKER_WEIGHT,
            call=lambda: self.exchange.fetch_ticker(symbol_normalized),
        )

    async def request(
        self,
        method_name: str,
        *args: Any,
        request_weight: int = 1,
        **kwargs: Any,
    ) -> Any:
        """Run an arbitrary async ccxt REST method with client safeguards."""
        method_name_normalized = method_name.strip()
        if not method_name_normalized:
            raise BinanceClientError("method_name must not be blank")

        method = getattr(self.exchange, method_name_normalized, None)
        if method is None or not callable(method):
            raise BinanceClientError(
                "ccxt method is not available",
                context={"method_name": method_name_normalized},
            )

        return await self._request(
            operation=method_name_normalized,
            request_params={
                "args": args,
                "kwargs": self._sanitize(kwargs),
            },
            request_weight=request_weight,
            call=lambda: method(*args, **kwargs),
        )

    async def get_balance(self, asset: str | None = None) -> dict[str, Any]:
        """Fetch account balances.

        Args:
            asset: Optional asset symbol, for example ``"USDT"``.

        Returns:
            Balance data containing ``free``, ``used``, and ``total`` amounts.

        Raises:
            BinanceClientError: If the exchange request fails.
        """
        asset_symbol = asset.strip().upper() if asset else None
        if asset is not None and not asset_symbol:
            raise BinanceClientError("asset must not be blank")

        balance = await self._request(
            operation="get_balance",
            request_params={"asset": asset_symbol},
            request_weight=self._BALANCE_WEIGHT,
            call=lambda: self.exchange.fetch_balance(),
        )

        if asset_symbol:
            return {
                "asset": asset_symbol,
                "free": self._to_float(balance.get("free", {}).get(asset_symbol)),
                "used": self._to_float(balance.get("used", {}).get(asset_symbol)),
                "total": self._to_float(
                    balance.get("total", {}).get(asset_symbol)
                ),
            }

        return {
            "free": self._extract_balance_group(balance, "free"),
            "used": self._extract_balance_group(balance, "used"),
            "total": self._extract_balance_group(balance, "total"),
        }

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> dict[str, Any]:
        """Create a Binance spot order.

        Args:
            symbol: Market symbol in ccxt format, for example ``"BTC/USDT"``.
            order_type: Order type. Supports ``"limit"`` and ``"market"``.
            side: Order side. Supports ``"buy"`` and ``"sell"``.
            amount: Base asset quantity.
            price: Limit price. Required for limit orders.

        Returns:
            The ccxt order response.

        Raises:
            InvalidOrderError: If order inputs are invalid.
            BinanceClientError: If the exchange request fails.
        """
        symbol_normalized = symbol.strip()
        order_type_normalized = order_type.strip().lower()
        side_normalized = side.strip().lower()
        context = {
            "symbol": symbol_normalized,
            "order_type": order_type_normalized,
            "side": side_normalized,
            "amount": amount,
            "price": price,
        }
        self._validate_order_request(
            order_type=order_type_normalized,
            side=side_normalized,
            amount=amount,
            price=price,
            context=context,
        )

        order_price = price if order_type_normalized == "limit" else None
        return await self._request(
            operation="create_order",
            request_params=context,
            request_weight=self._ORDER_WEIGHT,
            call=lambda: self.exchange.create_order(
                symbol=symbol_normalized,
                type=order_type_normalized,
                side=side_normalized,
                amount=amount,
                price=order_price,
            ),
        )

    async def cancel_order(self, order_id: str, symbol: str) -> dict[str, Any]:
        """Cancel an open Binance order.

        Args:
            order_id: Binance or ccxt order identifier.
            symbol: Market symbol in ccxt format.

        Returns:
            The ccxt cancellation response.

        Raises:
            BinanceClientError: If the exchange request fails.
        """
        order_id_normalized = order_id.strip()
        symbol_normalized = symbol.strip()
        if not order_id_normalized:
            raise InvalidOrderError(
                "order_id must not be blank",
                context={"order_id": order_id, "symbol": symbol},
            )
        if not symbol_normalized:
            raise InvalidOrderError(
                "symbol must not be blank",
                context={"order_id": order_id, "symbol": symbol},
            )

        return await self._request(
            operation="cancel_order",
            request_params={
                "order_id": order_id_normalized,
                "symbol": symbol_normalized,
            },
            request_weight=self._ORDER_WEIGHT,
            call=lambda: self.exchange.cancel_order(
                order_id_normalized,
                symbol_normalized,
            ),
        )

    async def get_ticker(self, symbol: str) -> float:
        """Fetch the current market price for a symbol.

        The method returns the mid-price when bid and ask are available;
        otherwise it falls back to the last traded price.

        Args:
            symbol: Market symbol in ccxt format.

        Returns:
            Mid-price or last traded price.

        Raises:
            BinanceClientError: If no usable price is returned.
        """
        symbol_normalized = symbol.strip()
        if not symbol_normalized:
            raise BinanceClientError("symbol must not be blank")

        ticker = await self._request(
            operation="get_ticker",
            request_params={"symbol": symbol_normalized},
            request_weight=self._TICKER_WEIGHT,
            call=lambda: self.exchange.fetch_ticker(symbol_normalized),
        )
        bid = self._to_optional_float(ticker.get("bid"))
        ask = self._to_optional_float(ticker.get("ask"))
        if bid is not None and ask is not None and bid > 0 and ask > 0:
            return (bid + ask) / 2

        last = self._to_optional_float(ticker.get("last"))
        if last is not None and last > 0:
            return last

        raise BinanceClientError(
            "ticker response did not include bid/ask or last price",
            context={"symbol": symbol_normalized, "ticker": self._sanitize(ticker)},
        )

    async def _request(
        self,
        *,
        operation: str,
        request_params: Mapping[str, Any],
        request_weight: int,
        call: Callable[[], Awaitable[Any]],
    ) -> Any:
        """Execute a ccxt request with throttling, retries, and logging."""
        self._ensure_open()
        sanitized_params = self._sanitize(request_params)

        for attempt in range(1, self.config.max_retries + 2):
            await self._rate_limiter.acquire(request_weight)
            started_at = time.perf_counter()

            try:
                result = await call()
                latency_ms = self._elapsed_ms(started_at)
                logger.bind(
                    exchange="binance",
                    operation=operation,
                    request_params=sanitized_params,
                    attempt=attempt,
                    latency_ms=latency_ms,
                    request_weight=request_weight,
                    used_weight_1m=self._used_weight_header("1m"),
                ).debug("Binance API request completed")
                return result
            except Exception as exc:
                latency_ms = self._elapsed_ms(started_at)
                mapped_error = self._map_exception(
                    exc,
                    operation=operation,
                    request_params=sanitized_params,
                )
                retryable = self._is_retryable(exc)
                retries_left = attempt <= self.config.max_retries
                log_context = {
                    "exchange": "binance",
                    "operation": operation,
                    "request_params": sanitized_params,
                    "attempt": attempt,
                    "latency_ms": latency_ms,
                    "request_weight": request_weight,
                    "retryable": retryable,
                    "retries_left": max(
                        self.config.max_retries - attempt + 1,
                        0,
                    ),
                    "error_type": exc.__class__.__name__,
                    "error_message": str(exc),
                    "error_code": mapped_error.error_code,
                    "http_status": mapped_error.http_status,
                    "used_weight_1m": self._used_weight_header("1m"),
                }

                if retryable and retries_left:
                    sleep_seconds = self._retry_sleep_seconds(attempt)
                    logger.bind(
                        **log_context,
                        sleep_seconds=round(sleep_seconds, 3),
                    ).opt(exception=exc).warning(
                        "Binance API request failed; retrying"
                    )
                    await asyncio.sleep(sleep_seconds)
                    continue

                logger.bind(**log_context).opt(exception=exc).error(
                    "Binance API request failed"
                )
                raise mapped_error from exc

        raise BinanceClientError(
            "request retry loop exited unexpectedly",
            context={"operation": operation, "request_params": sanitized_params},
        )

    def _map_exception(
        self,
        exc: Exception,
        *,
        operation: str,
        request_params: Mapping[str, Any],
    ) -> BinanceClientError:
        """Map ccxt exceptions to domain-specific exceptions."""
        context = {
            "operation": operation,
            "request_params": dict(request_params),
        }
        error_code = self._extract_error_code(exc)
        http_status = self._extract_http_status(exc)
        message = str(exc) or exc.__class__.__name__

        if self._is_ccxt_exception(exc, "InsufficientFunds"):
            return InsufficientFundsError(
                message,
                context=context,
                error_code=error_code,
                http_status=http_status,
            )
        if self._is_ccxt_exception(exc, "InvalidOrder"):
            return InvalidOrderError(
                message,
                context=context,
                error_code=error_code,
                http_status=http_status,
            )
        if self._is_ccxt_exception(exc, "AuthenticationError"):
            return AuthenticationError(
                message,
                context=context,
                error_code=error_code,
                http_status=http_status,
            )
        if self._is_ccxt_exception(exc, "RateLimitExceeded"):
            return RateLimitError(
                message,
                context=context,
                error_code=error_code,
                http_status=http_status,
            )
        if self._is_ccxt_exception(
            exc,
            "ExchangeNotAvailable",
        ) or self._is_ccxt_exception(exc, "OnMaintenance"):
            return ExchangeUnavailableError(
                message,
                context=context,
                error_code=error_code,
                http_status=http_status,
            )
        if self._is_ccxt_exception(exc, "NetworkError"):
            return BinanceNetworkError(
                message,
                context=context,
                error_code=error_code,
                http_status=http_status,
            )
        if self._is_exchange_unavailable(exc, http_status=http_status):
            return ExchangeUnavailableError(
                message,
                context=context,
                error_code=error_code,
                http_status=http_status,
            )

        return BinanceClientError(
            message,
            context=context,
            error_code=error_code,
            http_status=http_status,
        )

    def _is_retryable(self, exc: Exception) -> bool:
        """Return whether a ccxt exception should be retried."""
        non_retryable_names = (
            "AuthenticationError",
            "PermissionDenied",
            "AccountSuspended",
            "InsufficientFunds",
            "InvalidOrder",
            "BadRequest",
            "BadSymbol",
        )
        if any(self._is_ccxt_exception(exc, name) for name in non_retryable_names):
            return False

        if self._is_ccxt_exception(exc, "NetworkError"):
            return True
        if self._is_ccxt_exception(exc, "RateLimitExceeded"):
            return True
        if self._is_ccxt_exception(
            exc,
            "ExchangeNotAvailable",
        ) or self._is_ccxt_exception(exc, "OnMaintenance"):
            return True

        http_status = self._extract_http_status(exc)
        return self._is_exchange_unavailable(exc, http_status=http_status)

    def _is_exchange_unavailable(
        self,
        exc: Exception,
        *,
        http_status: int | None,
    ) -> bool:
        """Return whether an exception looks like a transient 5xx failure."""
        if not self._is_ccxt_exception(exc, "ExchangeError"):
            return False
        if http_status is not None and 500 <= http_status < 600:
            return True

        message = str(exc).lower()
        return any(
            marker in message
            for marker in self._TRANSIENT_EXCHANGE_ERROR_MARKERS
        )

    def _validate_order_request(
        self,
        *,
        order_type: str,
        side: str,
        amount: float,
        price: float | None,
        context: Mapping[str, Any],
    ) -> None:
        """Validate order inputs before they reach the exchange."""
        if not context.get("symbol"):
            raise InvalidOrderError(
                "symbol must not be blank",
                context=self._sanitize(context),
            )
        if order_type not in {"limit", "market"}:
            raise InvalidOrderError(
                "order_type must be 'limit' or 'market'",
                context=self._sanitize(context),
            )
        if side not in {"buy", "sell"}:
            raise InvalidOrderError(
                "side must be 'buy' or 'sell'",
                context=self._sanitize(context),
            )
        if not self._is_positive_finite(amount):
            raise InvalidOrderError(
                "amount must be a finite value greater than zero",
                context=self._sanitize(context),
            )
        if order_type == "limit" and (
            price is None or not self._is_positive_finite(price)
        ):
            raise InvalidOrderError(
                "price must be a finite value greater than zero for limit orders",
                context=self._sanitize(context),
            )

    def _retry_sleep_seconds(self, attempt: int) -> float:
        """Calculate exponential backoff with jitter."""
        exponential_delay = self.config.base_backoff_seconds * 2 ** (attempt - 1)
        capped_delay = min(exponential_delay, self.config.max_backoff_seconds)
        jitter = random.uniform(0, capped_delay * 0.25)
        return capped_delay + jitter

    def _extract_error_code(self, exc: Exception) -> str | int | None:
        """Extract an exchange error code from a ccxt exception."""
        code = getattr(exc, "code", None)
        if code is not None:
            return code

        match = self._ERROR_CODE_RE.search(str(exc))
        if match:
            return match.group(1)

        return None

    def _extract_http_status(self, exc: Exception) -> int | None:
        """Extract an HTTP status code from a ccxt exception."""
        for attr_name in ("http_status", "http_status_code", "status_code"):
            status = getattr(exc, attr_name, None)
            if isinstance(status, int):
                return status

        match = self._HTTP_STATUS_RE.search(str(exc))
        if match:
            return int(match.group(1))

        return None

    def _used_weight_header(self, interval: str) -> int | None:
        """Read Binance used-weight headers exposed by ccxt when available."""
        headers = getattr(self.exchange, "last_response_headers", None)
        if not isinstance(headers, Mapping):
            return None

        target = f"x-mbx-used-weight-{interval}".lower()
        for key, value in headers.items():
            if str(key).lower() == target:
                try:
                    return int(value)
                except (TypeError, ValueError):
                    return None

        return None

    def _ensure_open(self) -> None:
        """Raise if the client has already been closed."""
        if self._closed:
            raise BinanceClientError("BinanceClient is already closed")

    @staticmethod
    def _extract_balance_group(
        balance: Mapping[str, Any],
        group_name: str,
    ) -> dict[str, float]:
        """Extract a numeric balance group from a ccxt balance response."""
        group = balance.get(group_name, {})
        if not isinstance(group, Mapping):
            return {}

        return {
            str(asset): BinanceClient._to_float(amount)
            for asset, amount in group.items()
            if amount is not None
        }

    @staticmethod
    def _to_float(value: Any) -> float:
        """Convert an exchange numeric value to float with a zero fallback."""
        if value is None:
            return 0.0
        try:
            return float(value)
        except (TypeError, ValueError):
            return 0.0

    @staticmethod
    def _to_optional_float(value: Any) -> float | None:
        """Convert an exchange numeric value to float when possible."""
        if value is None:
            return None
        try:
            return float(value)
        except (TypeError, ValueError):
            return None

    @staticmethod
    def _elapsed_ms(started_at: float) -> float:
        """Return elapsed wall-clock time in milliseconds."""
        return round((time.perf_counter() - started_at) * 1_000, 3)

    @staticmethod
    def _is_positive_finite(value: Any) -> bool:
        """Return whether a value can be treated as a finite positive number."""
        try:
            numeric_value = float(value)
        except (TypeError, ValueError):
            return False

        return math.isfinite(numeric_value) and numeric_value > 0

    @classmethod
    def _sanitize(cls, value: Any) -> Any:
        """Sanitize structured values before they are attached to logs."""
        if isinstance(value, Mapping):
            sanitized: dict[str, Any] = {}
            for key, item in value.items():
                key_text = str(key)
                if key_text.lower() in cls._SENSITIVE_KEYS:
                    sanitized[key_text] = "***"
                else:
                    sanitized[key_text] = cls._sanitize(item)
            return sanitized
        if isinstance(value, list):
            return [cls._sanitize(item) for item in value]
        if isinstance(value, tuple):
            return tuple(cls._sanitize(item) for item in value)
        return value

    @staticmethod
    def _is_ccxt_exception(exc: Exception, name: str) -> bool:
        """Return whether an exception is an instance of a ccxt error class."""
        exception_class = getattr(ccxt, name, None)
        return isinstance(exception_class, type) and isinstance(
            exc,
            exception_class,
        )


__all__ = [
    "AuthenticationError",
    "AsyncWeightedRateLimiter",
    "BinanceClient",
    "BinanceClientConfig",
    "BinanceClientError",
    "BinanceNetworkError",
    "ExchangeUnavailableError",
    "InsufficientFundsError",
    "InvalidOrderError",
    "RateLimitError",
]

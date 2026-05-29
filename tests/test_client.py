"""Focused tests for Binance REST client request handling."""

from __future__ import annotations

import asyncio
import importlib
import sys
import types
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, Mock, call

import pytest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))


@dataclass(frozen=True)
class MockHttpResponse:
    """HTTP-shaped response returned by the fake ccxt transport."""

    status: int
    payload: dict[str, Any]


class FakeHttpBinanceExchange:
    """Minimal ccxt-like Binance exchange backed by mocked HTTP responses."""

    def __init__(
        self,
        config: dict[str, Any],
        responses: list[MockHttpResponse],
        ccxt_module: types.ModuleType,
    ) -> None:
        self.config = config
        self._responses = list(responses)
        self._ccxt = ccxt_module
        self.http_requests: list[dict[str, Any]] = []
        self.last_response_headers: dict[str, str] = {}
        self.close = AsyncMock(name="close")
        self.set_sandbox_mode = Mock(name="set_sandbox_mode")

    async def fetch_balance(self) -> dict[str, Any]:
        return await self._send_http_request(
            method="GET",
            path="/api/v3/account",
            params={},
            authenticated=True,
        )

    async def fetch_ticker(self, symbol: str) -> dict[str, Any]:
        return await self._send_http_request(
            method="GET",
            path="/api/v3/ticker/bookTicker",
            params={"symbol": symbol},
            authenticated=False,
        )

    async def _send_http_request(
        self,
        *,
        method: str,
        path: str,
        params: dict[str, Any],
        authenticated: bool,
    ) -> dict[str, Any]:
        headers = {}
        if authenticated:
            headers["X-MBX-APIKEY"] = self.config["apiKey"]

        self.http_requests.append(
            {
                "method": method,
                "path": path,
                "params": params,
                "headers": headers,
            }
        )

        response = self._responses.pop(0)
        self.last_response_headers = {
            str(key): str(value)
            for key, value in response.payload.get("headers", {}).items()
        }
        if response.status == 429:
            exc = self._ccxt.RateLimitExceeded("429 Too Many Requests")
            exc.http_status = response.status
            raise exc
        if response.status >= 500:
            exc = self._ccxt.ExchangeNotAvailable(
                f"{response.status} Service Unavailable"
            )
            exc.http_status = response.status
            raise exc
        if response.status >= 400:
            exc = self._ccxt.ExchangeError(f"{response.status} HTTP error")
            exc.http_status = response.status
            raise exc

        return response.payload


class NoopLogger:
    """Small loguru-compatible logger stub."""

    def bind(self, **_: Any) -> "NoopLogger":
        return self

    def opt(self, **_: Any) -> "NoopLogger":
        return self

    def debug(self, *_: Any, **__: Any) -> None:
        return None

    def warning(self, *_: Any, **__: Any) -> None:
        return None

    def error(self, *_: Any, **__: Any) -> None:
        return None


class CaptureLogger:
    """Loguru-compatible logger stub that records bound context."""

    def __init__(
        self,
        records: list[dict[str, Any]],
        context: dict[str, Any] | None = None,
        opts: dict[str, Any] | None = None,
    ) -> None:
        self.records = records
        self.context = dict(context or {})
        self.opts = dict(opts or {})

    def bind(self, **kwargs: Any) -> "CaptureLogger":
        return CaptureLogger(
            self.records,
            {**self.context, **kwargs},
            self.opts,
        )

    def opt(self, **kwargs: Any) -> "CaptureLogger":
        return CaptureLogger(
            self.records,
            self.context,
            {**self.opts, **kwargs},
        )

    def debug(self, message: str, **_: Any) -> None:
        self.records.append({"level": "debug", "message": message, **self.context, "opts": self.opts})

    def warning(self, message: str, **_: Any) -> None:
        self.records.append({"level": "warning", "message": message, **self.context, "opts": self.opts})

    def error(self, message: str, **_: Any) -> None:
        self.records.append({"level": "error", "message": message, **self.context, "opts": self.opts})


def _install_fake_ccxt(monkeypatch: pytest.MonkeyPatch) -> types.ModuleType:
    ccxt = types.ModuleType("ccxt")
    ccxt.__path__ = []

    class BaseError(Exception):
        pass

    class ExchangeError(BaseError):
        pass

    class NetworkError(BaseError):
        pass

    class ExchangeNotAvailable(NetworkError):
        pass

    class RateLimitExceeded(NetworkError):
        pass

    class AuthenticationError(ExchangeError):
        pass

    class PermissionDenied(AuthenticationError):
        pass

    class AccountSuspended(PermissionDenied):
        pass

    class InsufficientFunds(ExchangeError):
        pass

    class InvalidOrder(ExchangeError):
        pass

    class BadRequest(ExchangeError):
        pass

    class BadSymbol(BadRequest):
        pass

    for exception_class in (
        AccountSuspended,
        AuthenticationError,
        BadRequest,
        BadSymbol,
        BaseError,
        ExchangeError,
        ExchangeNotAvailable,
        InsufficientFunds,
        InvalidOrder,
        NetworkError,
        PermissionDenied,
        RateLimitExceeded,
    ):
        setattr(ccxt, exception_class.__name__, exception_class)

    async_support = types.ModuleType("ccxt.async_support")
    async_support.__package__ = "ccxt"
    async_support.binance = Mock(name="binance_factory")
    ccxt.async_support = async_support

    monkeypatch.setitem(sys.modules, "ccxt", ccxt)
    monkeypatch.setitem(sys.modules, "ccxt.async_support", async_support)
    return ccxt


def _install_fake_loguru(monkeypatch: pytest.MonkeyPatch) -> None:
    loguru = types.ModuleType("loguru")
    loguru.logger = NoopLogger()
    monkeypatch.setitem(sys.modules, "loguru", loguru)


@pytest.fixture()
def client_module(monkeypatch: pytest.MonkeyPatch) -> Any:
    """Import the client with mocked production-only dependencies."""
    _install_fake_ccxt(monkeypatch)
    _install_fake_loguru(monkeypatch)

    sys.modules.pop("src.exchange.client", None)
    module = importlib.import_module("src.exchange.client")
    yield module
    sys.modules.pop("src.exchange.client", None)


@pytest.fixture()
def make_client(client_module: Any) -> Any:
    """Build a client whose ccxt exchange returns mocked HTTP responses."""

    def _make_client(
        responses: list[MockHttpResponse],
        *,
        api_key: str = "test-api-key",
        api_secret: str = "test-api-secret",
        max_retries: int = 2,
        base_backoff_seconds: float = 0.1,
    ) -> tuple[Any, FakeHttpBinanceExchange]:
        exchanges: list[FakeHttpBinanceExchange] = []

        def build_exchange(config: dict[str, Any]) -> FakeHttpBinanceExchange:
            exchange = FakeHttpBinanceExchange(
                config=config,
                responses=responses,
                ccxt_module=client_module.ccxt,
            )
            exchanges.append(exchange)
            return exchange

        client_module.ccxt_async.binance.side_effect = build_exchange
        client = client_module.BinanceClient(
            api_key=api_key,
            api_secret=api_secret,
            request_weight_limit=10_000,
            request_weight_period_seconds=60.0,
            max_retries=max_retries,
            base_backoff_seconds=base_backoff_seconds,
            max_backoff_seconds=1.0,
        )
        return client, exchanges[0]

    return _make_client


def test_authenticated_request_applies_binance_api_key_header(make_client: Any) -> None:
    async def run() -> None:
        client, exchange = make_client(
            [
                MockHttpResponse(
                    status=200,
                    payload={
                        "free": {"USDT": "25.5"},
                        "used": {"USDT": "1.0"},
                        "total": {"USDT": "26.5"},
                        "headers": {"X-MBX-USED-WEIGHT-1M": "12"},
                    },
                )
            ],
            api_key="auth-key",
            api_secret="auth-secret",
        )

        result = await client.get_balance("USDT")

        assert result == {
            "asset": "USDT",
            "free": 25.5,
            "used": 1.0,
            "total": 26.5,
        }
        assert exchange.http_requests == [
            {
                "method": "GET",
                "path": "/api/v3/account",
                "params": {},
                "headers": {"X-MBX-APIKEY": "auth-key"},
            }
        ]
        assert client._used_weight_header("1m") == 12

    asyncio.run(run())


def test_client_disables_signed_currency_metadata_fetch(make_client: Any) -> None:
    client, exchange = make_client([])

    assert exchange.config["options"]["fetchCurrencies"] is False
    assert exchange.config["options"]["adjustForTimeDifference"] is True
    assert exchange.config["options"]["recvWindow"] == 10_000


def test_retryable_http_response_is_retried_before_success(
    make_client: Any,
    client_module: Any,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def run() -> None:
        client, exchange = make_client(
            [
                MockHttpResponse(status=503, payload={"msg": "unavailable"}),
                MockHttpResponse(
                    status=200,
                    payload={"bid": "99.5", "ask": "100.5"},
                ),
            ],
            max_retries=2,
            base_backoff_seconds=0.25,
        )
        sleep = AsyncMock(name="sleep")
        monkeypatch.setattr(client_module.asyncio, "sleep", sleep)
        monkeypatch.setattr(
            client_module.random,
            "uniform",
            Mock(return_value=0.0),
        )

        price = await client.get_ticker("BTC/USDT")

        assert price == 100.0
        assert [request["path"] for request in exchange.http_requests] == [
            "/api/v3/ticker/bookTicker",
            "/api/v3/ticker/bookTicker",
        ]
        assert [request["params"] for request in exchange.http_requests] == [
            {"symbol": "BTC/USDT"},
            {"symbol": "BTC/USDT"},
        ]
        sleep.assert_awaited_once_with(0.25)

    asyncio.run(run())


def test_retryable_http_response_raises_after_retry_exhaustion(
    make_client: Any,
    client_module: Any,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def run() -> None:
        client, exchange = make_client(
            [
                MockHttpResponse(status=429, payload={"msg": "rate limited"}),
                MockHttpResponse(status=429, payload={"msg": "rate limited"}),
                MockHttpResponse(status=429, payload={"msg": "rate limited"}),
            ],
            max_retries=2,
            base_backoff_seconds=0.1,
        )
        sleep = AsyncMock(name="sleep")
        monkeypatch.setattr(client_module.asyncio, "sleep", sleep)
        monkeypatch.setattr(
            client_module.random,
            "uniform",
            Mock(return_value=0.0),
        )

        with pytest.raises(client_module.RateLimitError) as exc_info:
            await client.get_ticker("ETH/USDT")

        assert len(exchange.http_requests) == 3
        assert sleep.await_args_list == [call(0.1), call(0.2)]
        assert exc_info.value.http_status == 429
        assert exc_info.value.context == {
            "operation": "get_ticker",
            "request_params": {"symbol": "ETH/USDT"},
        }

    asyncio.run(run())


def test_exchange_failure_logs_are_sanitized(
    make_client: Any,
    client_module: Any,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def run() -> None:
        records: list[dict[str, Any]] = []
        monkeypatch.setattr(client_module, "logger", CaptureLogger(records))
        client, exchange = make_client(
            [],
            api_key="live-api-key",
            api_secret="live-api-secret",
            max_retries=0,
        )
        exc = client_module.ccxt.InsufficientFunds(
            "binance POST /api/v3/order headers={'X-MBX-APIKEY': 'live-api-key'} "
            "body='symbol=HBARUSDT&timestamp=123456&signature=deadbeef' "
            "secret=live-api-secret"
        )
        exchange.fetch_balance = AsyncMock(side_effect=exc)

        with pytest.raises(client_module.InsufficientFundsError) as exc_info:
            await client.get_balance()

        rendered_records = repr(records)
        assert "live-api-key" not in rendered_records
        assert "live-api-secret" not in rendered_records
        assert "deadbeef" not in rendered_records
        assert "signature=deadbeef" not in str(exc_info.value)
        assert "live-api-key" not in str(exc_info.value)
        assert exc_info.value.__cause__ is None
        assert all(record["opts"].get("exception") is not exc for record in records)
        assert records[-1]["error_message"] != str(exc)

    asyncio.run(run())

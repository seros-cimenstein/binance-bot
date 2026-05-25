from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

from src.config import Settings
from src.data.feeds import CandleData
from src.db import OrderDB
from src.main import BotRuntime
from src.orders.manager import OrderManager
from src.risk.guard import RiskGuard


class FakeExchangeRestClient:
    def __init__(self) -> None:
        self.exchange = self
        self.orders: list[dict[str, Any]] = []
        self.closed = False
        self._counter = 0

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> dict[str, Any]:
        self._counter += 1
        fill_price = price or 82_000.0
        order = {
            "id": f"exchange-{self._counter}",
            "symbol": symbol,
            "type": order_type,
            "side": side,
            "amount": amount,
            "price": fill_price,
            "status": "closed",
            "filled": amount,
            "remaining": 0.0,
            "average": fill_price,
        }
        self.orders.append(order)
        return order

    async def get_balance(self) -> dict[str, Any]:
        return {
            "free": {"USDT": 10_000.0},
            "used": {"USDT": 0.0},
            "total": {"USDT": 10_000.0},
        }

    async def close(self) -> None:
        self.closed = True


class FiniteCandleFeed:
    def __init__(self) -> None:
        self.subscriptions: list[tuple[str, str]] = []

    async def subscribe_ohlcv(self, symbol: str, timeframe: str = "1m") -> Any:
        self.subscriptions.append((symbol, timeframe))
        prices = [82_000.0] * 8 + [86_000.0] * 4 + [78_000.0] * 5
        for index, price in enumerate(prices, start=1):
            await asyncio.sleep(0)
            yield CandleData(
                symbol=symbol,
                timestamp=index,
                open=price,
                high=price,
                low=price,
                close=price,
                volume=1.0,
            )


class FakeExecutionWebSocket:
    def __init__(self) -> None:
        self.queue: asyncio.Queue[Any] = asyncio.Queue()
        self.started_user_streams: list[bool] = []
        self.unsubscribed = False
        self.stopped = False

    def subscribe_queue(self, _: Any) -> asyncio.Queue[Any]:
        return self.queue

    def unsubscribe_queue(self, _: Any, queue: asyncio.Queue[Any]) -> None:
        assert queue is self.queue
        self.unsubscribed = True

    async def start(self, *, user_stream: bool = True) -> None:
        self.started_user_streams.append(user_stream)

    async def stop(self) -> None:
        self.stopped = True


class FakeTelegramAlerter:
    def __init__(self) -> None:
        self.started_modes: list[str] = []
        self.trades: list[dict[str, Any]] = []
        self.risk_breaches: list[str] = []

    async def alert_started(self, mode: str) -> None:
        self.started_modes.append(mode)

    async def alert_trade(
        self,
        *,
        side: str,
        symbol: str,
        amount: float,
        price: float | None,
        dry_run: bool = False,
    ) -> None:
        self.trades.append(
            {
                "side": side,
                "symbol": symbol,
                "amount": amount,
                "price": price,
                "dry_run": dry_run,
            }
        )

    async def alert_risk_breach(self, reason: str) -> None:
        self.risk_breaches.append(reason)


@pytest.mark.asyncio
async def test_runtime_pipeline_with_fake_exchange_websocket_telegram_and_sqlite(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "bot.db"
    rest_client = FakeExchangeRestClient()
    feed = FiniteCandleFeed()
    execution_ws = FakeExecutionWebSocket()
    alerter = FakeTelegramAlerter()
    settings = Settings(
        api_key="key",
        api_secret="secret",
        db_path=str(db_path),
        fast_period=3,
        slow_period=6,
        order_size=0.001,
        max_position_size_usd=50_000.0,
        daily_loss_limit_usd=10_000.0,
        order_update_mode="websocket",
    )

    runtime = BotRuntime(
        settings,
        asyncio.Event(),
        asyncio.Event(),
        alerter=alerter,
        db_factory=OrderDB,
        rest_client_factory=lambda _: rest_client,
        order_manager_factory=lambda client, db: OrderManager(client, db=db),
        risk_guard_factory=lambda _, db: RiskGuard(
            max_position_size_usd=50_000.0,
            daily_loss_limit_usd=10_000.0,
            db=db,
        ),
        data_feed_factory=lambda *_: feed,
        execution_manager_factory=lambda _: execution_ws,
    )

    await asyncio.wait_for(runtime.run(), timeout=2.0)

    assert feed.subscriptions == [("BTC/USDT", "1m")]
    assert [order["side"] for order in rest_client.orders] == ["buy", "sell"]
    assert alerter.started_modes == ["testnet"]
    assert [trade["side"] for trade in alerter.trades] == ["buy", "sell"]
    assert alerter.risk_breaches == []
    assert execution_ws.started_user_streams == [True]
    assert execution_ws.unsubscribed is True
    assert execution_ws.stopped is True
    assert rest_client.closed is True

    db = OrderDB(str(db_path))
    try:
        persisted_orders = db.load_orders()
    finally:
        db.close()
    assert [order.side.value for order in persisted_orders] == ["buy", "sell"]

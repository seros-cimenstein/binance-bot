from __future__ import annotations

import asyncio
from typing import Any

from src.data.websocket_feed import BinanceWebSocketCandleFeed
from src.exchange.websocket import EventType, KlineUpdate, WebSocketEvent


class FakeWebSocketManager:
    def __init__(self) -> None:
        self.queue: asyncio.Queue[WebSocketEvent] | None = None
        self.subscribed: list[tuple[str, str]] = []
        self.unsubscribed: list[tuple[str, str]] = []
        self.unsubscribed_queue: (
            tuple[EventType, asyncio.Queue[WebSocketEvent]] | None
        ) = None
        self.started_with: list[bool] = []
        self.stopped = False

    def subscribe_queue(
        self,
        event_type: EventType,
        *,
        maxsize: int | None = None,
    ) -> asyncio.Queue[WebSocketEvent]:
        self.queue = asyncio.Queue(maxsize=maxsize or 0)
        return self.queue

    def unsubscribe_queue(
        self,
        event_type: EventType,
        queue: asyncio.Queue[WebSocketEvent],
    ) -> None:
        self.unsubscribed_queue = (event_type, queue)

    async def subscribe_klines(self, symbol: str, *, interval: str) -> None:
        self.subscribed.append((symbol, interval))

    async def unsubscribe_klines(self, symbol: str, *, interval: str) -> None:
        self.unsubscribed.append((symbol, interval))

    async def start(self, *, user_stream: bool = True) -> None:
        self.started_with.append(user_stream)

    async def stop(self) -> None:
        self.stopped = True


def _event(
    *,
    symbol: str = "BTC/USDT",
    interval: str = "1m",
    open_time: int = 1000,
    closed: bool = True,
    close: float = 101.0,
) -> WebSocketEvent:
    return WebSocketEvent(
        event_type=EventType.KLINE_UPDATE,
        payload=KlineUpdate(
            symbol=symbol,
            interval=interval,
            open_time=open_time,
            close_time=open_time + 59999,
            open=100.0,
            high=102.0,
            low=99.0,
            close=close,
            volume=12.5,
            is_closed=closed,
        ),
    )


def test_websocket_candle_feed_yields_only_matching_closed_candles() -> None:
    async def run() -> None:
        manager = FakeWebSocketManager()
        feed = BinanceWebSocketCandleFeed(manager, queue_maxsize=2)
        stream = feed.subscribe_ohlcv("btcusdt", timeframe="1m")
        next_candle = asyncio.create_task(stream.__anext__())
        await asyncio.sleep(0)

        assert manager.subscribed == [("BTC/USDT", "1m")]
        assert manager.started_with == [False]
        assert manager.queue is not None

        await manager.queue.put(_event(closed=False))
        await manager.queue.put(_event(symbol="ETH/USDT"))
        await manager.queue.put(_event(close=103.5))

        candle = await asyncio.wait_for(next_candle, 1.0)

        assert candle.symbol == "BTC/USDT"
        assert candle.timestamp == 1000
        assert candle.open == 100.0
        assert candle.high == 102.0
        assert candle.low == 99.0
        assert candle.close == 103.5
        assert candle.volume == 12.5

        await stream.aclose()

        assert manager.unsubscribed == [("BTC/USDT", "1m")]
        assert manager.unsubscribed_queue == (EventType.KLINE_UPDATE, manager.queue)
        assert manager.stopped is True

    asyncio.run(run())


def test_websocket_candle_feed_skips_duplicate_closed_candles() -> None:
    async def run() -> None:
        manager = FakeWebSocketManager()
        feed = BinanceWebSocketCandleFeed(manager)
        stream = feed.subscribe_ohlcv("BTC/USDT", timeframe="1m")
        first_candle = asyncio.create_task(stream.__anext__())
        await asyncio.sleep(0)

        assert manager.queue is not None
        await manager.queue.put(_event(open_time=1000, close=101.0))
        first = await asyncio.wait_for(first_candle, 1.0)

        second_candle = asyncio.create_task(stream.__anext__())
        await asyncio.sleep(0)
        await manager.queue.put(_event(open_time=1000, close=999.0))
        await manager.queue.put(_event(open_time=61000, close=102.0))
        second = await asyncio.wait_for(second_candle, 1.0)

        assert first.close == 101.0
        assert second.timestamp == 61000
        assert second.close == 102.0

        await stream.aclose()

    asyncio.run(run())


def test_websocket_candle_feed_rejects_blank_timeframe() -> None:
    async def run() -> None:
        manager: Any = FakeWebSocketManager()
        feed = BinanceWebSocketCandleFeed(manager)
        stream = feed.subscribe_ohlcv("BTC/USDT", timeframe=" ")

        try:
            await stream.__anext__()
        except ValueError as exc:
            assert str(exc) == "timeframe must not be blank"
        else:
            raise AssertionError("expected ValueError")

    asyncio.run(run())

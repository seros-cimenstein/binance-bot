"""Unit tests for the Binance WebSocket manager."""

from __future__ import annotations

import asyncio
import json
from typing import Any

from src.exchange.websocket import (
    BinanceWebSocketManager,
    EventType,
    TickerUpdate,
    WebSocketEvent,
)


def test_subscribe_market_data_accepts_single_symbol_string() -> None:
    async def run() -> None:
        manager = BinanceWebSocketManager()

        await manager.subscribe_market_data(
            "BTC/USDT",
            ticker=True,
            order_book=True,
            order_book_depth=20,
            order_book_update_interval_ms=100,
        )

        assert sorted(manager._public_subscriptions) == [
            "btcusdt@depth20@100ms",
            "btcusdt@ticker",
        ]

    asyncio.run(run())


def test_public_messages_are_parsed_into_ticker_and_order_book_events() -> None:
    async def run() -> None:
        manager = BinanceWebSocketManager()
        await manager.subscribe_market_data(["BTC/USDT"])

        await manager._handle_public_message(
            json.dumps(
                {
                    "stream": "btcusdt@ticker",
                    "data": {
                        "e": "24hrTicker",
                        "E": 1672515782136,
                        "s": "BTCUSDT",
                        "c": "62000.50",
                        "b": "61999.90",
                        "a": "62001.10",
                        "o": "60000.00",
                        "h": "62500.00",
                        "l": "59800.00",
                        "v": "1200.0",
                        "q": "74400000.0",
                    },
                }
            )
        )
        ticker_event = await asyncio.wait_for(manager._event_queue.get(), 1.0)

        assert ticker_event.event_type == EventType.TICKER_UPDATE
        assert ticker_event.payload.symbol == "BTC/USDT"
        assert ticker_event.payload.price == 62000.50
        manager._event_queue.task_done()

        await manager._handle_public_message(
            json.dumps(
                {
                    "stream": "btcusdt@depth20@100ms",
                    "data": {
                        "lastUpdateId": 123,
                        "bids": [["61999.00", "0.5"]],
                        "asks": [["62001.00", "0.4"]],
                    },
                }
            )
        )
        book_event = await asyncio.wait_for(manager._event_queue.get(), 1.0)

        assert book_event.event_type == EventType.ORDER_BOOK_UPDATE
        assert book_event.payload.symbol == "BTC/USDT"
        assert book_event.payload.last_update_id == 123
        assert book_event.payload.bids[0].price == 61999.00
        assert book_event.payload.asks[0].quantity == 0.4
        manager._event_queue.task_done()

    asyncio.run(run())


def test_dispatcher_routes_events_to_callbacks_and_subscriber_queues() -> None:
    async def run() -> None:
        manager = BinanceWebSocketManager()
        callback_seen: asyncio.Future[WebSocketEvent] = asyncio.Future()

        async def callback(event: WebSocketEvent) -> None:
            callback_seen.set_result(event)

        manager.subscribe(EventType.TICKER_UPDATE, callback)
        queue = manager.subscribe_queue(EventType.TICKER_UPDATE, maxsize=1)
        manager._running = True
        dispatcher = asyncio.create_task(manager._dispatch_loop(0))

        event = WebSocketEvent(
            event_type=EventType.TICKER_UPDATE,
            payload=TickerUpdate(symbol="ETHUSDT", price=3000.0),
        )
        await manager._emit(event)

        queued_event = await asyncio.wait_for(queue.get(), 1.0)
        callback_event = await asyncio.wait_for(callback_seen, 1.0)

        assert queued_event is event
        assert callback_event is event

        queue.task_done()
        manager._running = False
        await asyncio.wait_for(dispatcher, 1.0)

    asyncio.run(run())


def test_user_data_stream_accepts_websocket_api_event_envelope() -> None:
    async def run() -> None:
        manager = BinanceWebSocketManager()

        await manager._handle_user_message(
            json.dumps(
                {
                    "subscriptionId": 0,
                    "event": {
                        "e": "outboundAccountPosition",
                        "E": 1564034571105,
                        "u": 1564034571073,
                        "B": [
                            {
                                "a": "ETH",
                                "f": "10000.000000",
                                "l": "0.000000",
                            }
                        ],
                    },
                }
            )
        )

        event = await asyncio.wait_for(manager._event_queue.get(), 1.0)

        assert event.event_type == EventType.ACCOUNT_POSITION
        assert event.payload.event_time == 1564034571105
        assert event.payload.balances[0].asset == "ETH"
        assert event.payload.balances[0].free == 10000.0
        manager._event_queue.task_done()

    asyncio.run(run())


def test_server_shutdown_event_closes_public_websocket() -> None:
    class FakeWebSocket:
        def __init__(self) -> None:
            self.closed = False

        async def close(self) -> None:
            self.closed = True

    async def run() -> None:
        manager = BinanceWebSocketManager()
        websocket: Any = FakeWebSocket()
        manager._public_ws = websocket

        await manager._handle_public_message(
            json.dumps(
                {
                    "stream": "!serverShutdown",
                    "data": {"e": "serverShutdown", "E": 1770123456789},
                }
            )
        )

        event = await asyncio.wait_for(manager._event_queue.get(), 1.0)

        assert websocket.closed is True
        assert event.event_type == EventType.RAW_MESSAGE
        manager._event_queue.task_done()

    asyncio.run(run())

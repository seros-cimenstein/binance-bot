"""Market-data feed adapters backed by the Binance WebSocket manager."""

from __future__ import annotations

from collections.abc import AsyncIterator

from src.data.feeds import CandleData, normalize_symbol
from src.exchange.websocket import (
    BinanceWebSocketManager,
    EventType,
    KlineUpdate,
)


class BinanceWebSocketCandleFeed:
    """Expose Binance kline WebSocket events as normalized closed candles."""

    def __init__(
        self,
        manager: BinanceWebSocketManager,
        *,
        queue_maxsize: int = 1_000,
    ) -> None:
        if queue_maxsize < 0:
            raise ValueError("queue_maxsize must be greater than or equal to zero")
        self.manager = manager
        self._queue_maxsize = queue_maxsize

    async def subscribe_ohlcv(
        self,
        symbol: str,
        timeframe: str = "1m",
    ) -> AsyncIterator[CandleData]:
        """Yield closed OHLCV candles from Binance kline WebSocket updates."""
        normalized_symbol = normalize_symbol(symbol)
        normalized_timeframe = timeframe.strip()
        if not normalized_timeframe:
            raise ValueError("timeframe must not be blank")

        queue = self.manager.subscribe_queue(
            EventType.KLINE_UPDATE,
            maxsize=self._queue_maxsize,
        )
        await self.manager.subscribe_klines(
            normalized_symbol,
            interval=normalized_timeframe,
        )
        await self.manager.start(user_stream=False)

        last_timestamp: int | None = None
        try:
            while True:
                event = await queue.get()
                try:
                    payload = event.payload
                    if (
                        event.event_type is not EventType.KLINE_UPDATE
                        or not isinstance(payload, KlineUpdate)
                    ):
                        continue
                    if normalize_symbol(payload.symbol) != normalized_symbol:
                        continue
                    if payload.interval != normalized_timeframe:
                        continue
                    if not payload.is_closed:
                        continue
                    if payload.open_time == last_timestamp:
                        continue
                    last_timestamp = payload.open_time

                    yield CandleData(
                        symbol=normalized_symbol,
                        timestamp=payload.open_time,
                        open=payload.open,
                        high=payload.high,
                        low=payload.low,
                        close=payload.close,
                        volume=payload.volume,
                    )
                finally:
                    queue.task_done()
        finally:
            self.manager.unsubscribe_queue(EventType.KLINE_UPDATE, queue)
            await self.manager.unsubscribe_klines(
                normalized_symbol,
                interval=normalized_timeframe,
            )
            await self.manager.stop()

from __future__ import annotations

from typing import Any

from src.strategies.base import Strategy, TickData
from src.strategies.demo_sma import DemoSMAStrategy, SMASettings


class RecordingClient:
    def __init__(self) -> None:
        self.orders: list[dict[str, Any]] = []

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> dict[str, Any]:
        order = {
            "id": len(self.orders) + 1,
            "symbol": symbol,
            "type": order_type,
            "side": side,
            "amount": amount,
            "price": price,
            "status": "closed",
        }
        self.orders.append(order)
        return order


def _tick(price: float, timestamp: int, symbol: str = "BTCUSDT") -> TickData:
    return TickData(
        timestamp=timestamp,
        symbol=symbol,
        bid=price,
        ask=price,
        last_price=price,
        volume=1.0,
    )


def test_demo_sma_defaults_to_20_50_and_registers_plugin() -> None:
    settings = SMASettings(symbol="btcusdt")

    assert settings.symbol == "BTC/USDT"
    assert settings.fast_period == 20
    assert settings.slow_period == 50
    assert Strategy.get_plugin("demo-sma") is DemoSMAStrategy


async def test_demo_sma_generates_buy_and_sell_orders_on_tick_crossovers() -> None:
    client = RecordingClient()
    strategy = DemoSMAStrategy(
        client,
        SMASettings(symbol="BTC/USDT", order_size=0.5),
    )

    await strategy.start()

    prices = [100.0] * 30 + [90.0] * 20
    for timestamp, price in enumerate(prices, start=1):
        await strategy.on_tick(_tick(price, timestamp))

    assert client.orders == []
    assert strategy.last_snapshot is not None
    assert strategy.last_snapshot.signal is None
    assert strategy.last_snapshot.relation == -1

    await strategy.on_tick(_tick(200.0, 51))
    assert client.orders == []
    assert strategy.last_snapshot is not None
    assert strategy.last_snapshot.relation == -1

    await strategy.on_tick(_tick(200.0, 52))
    assert [order["side"] for order in client.orders] == ["buy"]
    assert strategy.last_signal == "buy"
    assert strategy.last_snapshot is not None
    assert strategy.last_snapshot.signal == "buy"

    await strategy.on_tick(_tick(1.0, 53))
    assert [order["side"] for order in client.orders] == ["buy", "sell"]
    assert strategy.last_signal == "sell"
    assert strategy.last_snapshot is not None
    assert strategy.last_snapshot.signal == "sell"


async def test_demo_sma_candle_adapter_ignores_open_candles() -> None:
    client = RecordingClient()
    strategy = DemoSMAStrategy(
        client,
        {
            "symbol": "BTC/USDT",
            "fast_period": 2,
            "slow_period": 3,
            "order_size": 1.0,
        },
    )

    await strategy.start()
    await strategy.on_candle(
        {
            "symbol": "BTC/USDT",
            "timestamp": 1,
            "close": 100.0,
            "volume": 1.0,
        }
    )
    await strategy.on_candle(
        {
            "symbol": "BTC/USDT",
            "timestamp": 2,
            "close": 90.0,
            "volume": 1.0,
        }
    )
    await strategy.on_candle(
        {
            "symbol": "BTC/USDT",
            "timestamp": 3,
            "close": 90.0,
            "volume": 1.0,
        }
    )

    assert strategy.last_snapshot is not None
    assert strategy.last_snapshot.relation == -1

    await strategy.on_candle(
        {
            "symbol": "BTC/USDT",
            "timestamp": 4,
            "close": 200.0,
            "volume": 1.0,
            "closed": False,
        }
    )

    assert strategy.sample_count == 3
    assert client.orders == []

    await strategy.on_candle(
        {
            "symbol": "BTC/USDT",
            "timestamp": 5,
            "close": 200.0,
            "volume": 1.0,
            "closed": True,
        }
    )

    assert [order["side"] for order in client.orders] == ["buy"]

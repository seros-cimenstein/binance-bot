#!/usr/bin/env python3
"""End-to-end pipeline smoke test with synthetic candle data.

Runs the full stack (StrategyOrderClient → OrderManager → RiskGuard →
DemoSMAStrategy) without touching any real exchange or Telegram API.
Uses short SMA periods (fast=3, slow=6) so crossovers happen quickly.
"""

import asyncio
from typing import Any

from src.strategies.demo_sma import DemoSMAStrategy, SMASettings
from src.orders.manager import OrderManager
from src.risk.guard import RiskGuard
from src.telegram_alerts import TelegramAlerter
from src.main import StrategyOrderClient


class FakeExchange:
    """Records orders instead of sending them to Binance."""

    def __init__(self) -> None:
        self.orders: list[dict[str, Any]] = []
        self._counter = 0
        self.exchange = self  # StrategyOrderClient expects rest_client.exchange

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> dict[str, Any]:
        self._counter += 1
        fill_price = price or 82000.0
        order = {
            "id": str(self._counter),
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


class CapturingAlerter(TelegramAlerter):
    """Captures Telegram messages instead of sending them."""

    def __init__(self) -> None:
        self._token = "test"
        self._chat_id = "test"
        self._enabled = True
        self.messages: list[str] = []

    async def send(self, message: str) -> None:
        self.messages.append(message)


def _candle(price: float, i: int) -> dict[str, Any]:
    return {
        "symbol": "BTC/USDT",
        "timestamp": i,
        "open": price,
        "high": price,
        "low": price,
        "close": price,
        "volume": 1.0,
        "closed": True,
    }


async def run() -> None:
    exchange = FakeExchange()
    alerter = CapturingAlerter()
    order_manager = OrderManager(exchange)
    risk_guard = RiskGuard(
        max_position_size_usd=50_000.0,
        daily_loss_limit_usd=10_000.0,
    )
    client = StrategyOrderClient(
        exchange,
        order_manager,
        risk_guard=risk_guard,
        alerter=alerter,
        dry_run=False,
    )
    strategy = DemoSMAStrategy(
        client,
        SMASettings(
            symbol="BTC/USDT",
            fast_period=3,
            slow_period=6,
            order_size=0.001,
        ),
    )

    await strategy.start()

    # Price sequence engineered to produce two crossovers:
    #   flat baseline → spike up (BUY crossover) → crash (SELL crossover)
    prices = (
        [82_000.0] * 8   # baseline: fast ≈ slow, no signal
        + [86_000.0] * 4  # fast SMA rises above slow → BUY
        + [76_000.0] * 5  # fast SMA drops below slow → SELL
    )

    print(f"Feeding {len(prices)} candles  (fast=3, slow=6, size=0.001 BTC)\n")
    print(f"{'Candle':>6}  {'Price':>10}  Event")
    print("-" * 42)

    prev_order_count = 0
    prev_alert_count = 0

    for i, price in enumerate(prices, start=1):
        client.update_last_price(price)
        await strategy.on_candle(_candle(price, i))

        new_orders = exchange.orders[prev_order_count:]
        new_alerts = alerter.messages[prev_alert_count:]
        prev_order_count = len(exchange.orders)
        prev_alert_count = len(alerter.messages)

        events = []
        for o in new_orders:
            events.append(f"ORDER {o['side'].upper()} @ ${o['average']:,.0f}")
        for m in new_alerts:
            tag = "BUY" if "BUY" in m else "SELL" if "SELL" in m else "ALERT"
            events.append(f"TELEGRAM {tag}")

        print(f"{i:>6}  ${price:>10,.0f}  {'  |  '.join(events) if events else ''}")

    print("\n" + "=" * 42)
    print(f"Total orders placed : {len(exchange.orders)}")
    print(f"Telegram alerts sent: {len(alerter.messages)}")

    if exchange.orders:
        print("\nOrders:")
        for o in exchange.orders:
            print(f"  {o['side'].upper():4}  {o['amount']} BTC  @ ${o['average']:,.0f}")

    if alerter.messages:
        print("\nTelegram messages:")
        for m in alerter.messages:
            print(f"  {m.strip()}")
    else:
        print("\nNo Telegram messages — check TelegramAlerter wiring.")

    if not exchange.orders:
        print("\nNo orders fired — crossover did not occur with this price sequence.")


if __name__ == "__main__":
    asyncio.run(run())

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from src.orders.manager import (
    InvalidOrderTransitionError,
    OrderManager,
    OrderNotFoundError,
    OrderValidationError,
)
from src.orders.models import OrderSide, OrderStatus, OrderType


class FakeOrderClient:
    def __init__(self) -> None:
        self.create_payloads: list[dict[str, Any]] = []
        self.cancel_payloads: list[dict[str, Any]] = []
        self.fetch_payloads: list[dict[str, Any]] = []
        self.create_calls: list[dict[str, Any]] = []
        self.cancel_calls: list[tuple[str, str]] = []
        self.request_calls: list[tuple[str, tuple[Any, ...]]] = []

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> dict[str, Any]:
        self.create_calls.append(
            {
                "symbol": symbol,
                "order_type": order_type,
                "side": side,
                "amount": amount,
                "price": price,
            }
        )
        return self.create_payloads.pop(0)

    async def cancel_order(self, order_id: str, symbol: str) -> dict[str, Any]:
        self.cancel_calls.append((order_id, symbol))
        return self.cancel_payloads.pop(0)

    async def request(self, method_name: str, *args: Any) -> dict[str, Any]:
        self.request_calls.append((method_name, args))
        return self.fetch_payloads.pop(0)


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


def test_place_order_tracks_active_and_historical_state() -> None:
    client = FakeOrderClient()
    client.create_payloads.append(
        {
            "id": "order-1",
            "symbol": "BTC/USDT",
            "type": "limit",
            "side": "buy",
            "amount": "0.5",
            "price": "30000",
            "status": "open",
            "filled": "0",
            "remaining": "0.5",
        }
    )
    manager = OrderManager(client)

    order = _run(manager.place_order(" BTC/USDT ", "limit", "buy", 0.5, 30000))

    assert order.id == "order-1"
    assert order.type is OrderType.LIMIT
    assert order.side is OrderSide.BUY
    assert order.status is OrderStatus.OPEN
    assert order.remaining_amount == 0.5
    assert client.create_calls == [
        {
            "symbol": "BTC/USDT",
            "order_type": "limit",
            "side": "buy",
            "amount": 0.5,
            "price": 30000.0,
        }
    ]
    assert [active.id for active in manager.list_active_orders()] == ["order-1"]
    assert [historical.id for historical in manager.list_orders()] == ["order-1"]


def test_cancel_order_moves_active_order_to_historical_terminal_state() -> None:
    client = FakeOrderClient()
    client.create_payloads.append(
        {
            "id": "order-2",
            "symbol": "ETH/USDT",
            "type": "limit",
            "side": "sell",
            "amount": "2",
            "price": "2200",
            "status": "open",
        }
    )
    client.cancel_payloads.append(
        {
            "id": "order-2",
            "symbol": "ETH/USDT",
            "type": "limit",
            "side": "sell",
            "amount": "2",
            "price": "2200",
            "status": "canceled",
        }
    )
    manager = OrderManager(client)
    _run(manager.place_order("ETH/USDT", "limit", "sell", 2, 2200))

    canceled = _run(manager.cancel_order("order-2"))

    assert canceled.status is OrderStatus.CANCELED
    assert client.cancel_calls == [("order-2", "ETH/USDT")]
    assert manager.list_active_orders() == []
    assert manager.list_orders()[0].status is OrderStatus.CANCELED


def test_status_refresh_uses_generic_client_request_and_updates_order() -> None:
    client = FakeOrderClient()
    client.create_payloads.append(
        {
            "id": "order-3",
            "symbol": "BTC/USDT",
            "type": "limit",
            "side": "buy",
            "amount": "1",
            "price": "100",
            "status": "open",
            "filled": "0",
        }
    )
    client.fetch_payloads.append(
        {
            "id": "order-3",
            "symbol": "BTC/USDT",
            "type": "limit",
            "side": "buy",
            "amount": "1",
            "price": "100",
            "status": "filled",
            "filled": "1",
            "remaining": "0",
            "average": "100",
        }
    )
    manager = OrderManager(client)
    _run(manager.place_order("BTC/USDT", "limit", "buy", 1, 100))

    status = _run(manager.get_order_status("order-3", refresh=True))

    assert status is OrderStatus.CLOSED
    assert client.request_calls == [("fetch_order", ("order-3", "BTC/USDT"))]
    assert manager.list_active_orders() == []
    assert manager.get_position("BTC/USDT").amount == 1


def test_apply_order_updates_derive_positions_from_fill_deltas() -> None:
    client = FakeOrderClient()
    client.create_payloads.append(
        {
            "id": "order-4",
            "symbol": "BTC/USDT",
            "type": "limit",
            "side": "buy",
            "amount": "1",
            "price": "100",
            "status": "open",
            "filled": "0",
        }
    )
    manager = OrderManager(client)
    _run(manager.place_order("BTC/USDT", "limit", "buy", 1, 100))

    partial = manager.apply_order_update(
        {
            "id": "order-4",
            "symbol": "BTC/USDT",
            "type": "limit",
            "side": "buy",
            "amount": "1",
            "price": "100",
            "status": "PARTIALLY_FILLED",
            "filled": "0.4",
            "remaining": "0.6",
            "average": "100",
            "trades": [{"id": "fill-1", "amount": "0.4", "price": "100"}],
        }
    )
    assert partial.status is OrderStatus.OPEN
    assert manager.get_position("BTC/USDT").amount == pytest.approx(0.4)
    assert manager.get_position("BTC/USDT").entry_price == pytest.approx(100)

    closed = manager.apply_order_update(
        {
            "id": "order-4",
            "symbol": "BTC/USDT",
            "type": "limit",
            "side": "buy",
            "amount": "1",
            "price": "100",
            "status": "FILLED",
            "filled": "1",
            "remaining": "0",
            "average": "112",
            "trades": [
                {"id": "fill-1", "amount": "0.4", "price": "100"},
                {"id": "fill-2", "amount": "0.6", "price": "120"},
            ],
        }
    )

    position = manager.get_position("BTC/USDT")
    assert closed.status is OrderStatus.CLOSED
    assert len(closed.fills) == 2
    assert position.amount == pytest.approx(1)
    assert position.entry_price == pytest.approx(112)
    assert manager.list_active_orders() == []


def test_terminal_orders_reject_invalid_lifecycle_regressions() -> None:
    manager = OrderManager(FakeOrderClient())
    manager.apply_order_update(
        {
            "id": "order-5",
            "symbol": "ETH/USDT",
            "type": "limit",
            "side": "buy",
            "amount": "1",
            "price": "2000",
            "status": "filled",
            "filled": "1",
            "average": "2000",
        }
    )

    with pytest.raises(InvalidOrderTransitionError):
        manager.apply_order_update(
            {
                "id": "order-5",
                "symbol": "ETH/USDT",
                "type": "limit",
                "side": "buy",
                "amount": "1",
                "price": "2000",
                "status": "canceled",
            }
        )

    assert manager.list_orders()[0].status is OrderStatus.CLOSED


@pytest.mark.parametrize(
    ("order_type", "price", "expected_message"),
    [
        ("limit", None, "price is required"),
        ("stop", 100, "order_type"),
        ("market", 0, "price must be greater"),
    ],
)
def test_place_order_validates_requests_before_exchange_call(
    order_type: str,
    price: float | None,
    expected_message: str,
) -> None:
    client = FakeOrderClient()
    manager = OrderManager(client)

    with pytest.raises(OrderValidationError, match=expected_message):
        _run(manager.place_order("BTC/USDT", order_type, "buy", 1, price))

    assert client.create_calls == []


def test_unknown_local_status_query_raises_order_not_found() -> None:
    manager = OrderManager(FakeOrderClient())

    with pytest.raises(OrderNotFoundError):
        _run(manager.get_order_status("missing"))

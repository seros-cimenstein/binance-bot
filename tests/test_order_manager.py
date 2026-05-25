from __future__ import annotations

import asyncio
from datetime import datetime, timezone
from typing import Any

import pytest

from src.orders.manager import (
    InvalidOrderTransitionError,
    OrderManager,
    OrderNotFoundError,
    OrderValidationError,
)
from src.orders.models import Fill, Order, OrderSide, OrderStatus, OrderType


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


class FakeOrderDB:
    def __init__(self, orders: list[Order]) -> None:
        self.orders = orders

    def load_orders(self) -> list[Order]:
        return list(self.orders)

    def upsert_order(self, order: Order) -> None:
        self.orders = [existing for existing in self.orders if existing.id != order.id]
        self.orders.append(order)

    def delete_order(self, order_id: str) -> None:
        self.orders = [order for order in self.orders if order.id != order_id]


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


def _order(
    *,
    order_id: str,
    side: OrderSide,
    status: OrderStatus,
    amount: float,
    filled_amount: float,
    price: float,
) -> Order:
    timestamp = datetime.now(timezone.utc)
    return Order(
        id=order_id,
        symbol="BTC/USDT",
        type=OrderType.MARKET,
        side=side,
        amount=amount,
        status=status,
        filled_amount=filled_amount,
        remaining_amount=max(amount - filled_amount, 0.0),
        avg_fill_price=price,
        fills=[
            Fill(
                id=f"fill-{order_id}",
                order_id=order_id,
                symbol="BTC/USDT",
                side=side,
                price=price,
                amount=filled_amount,
                fee=0.0,
                fee_currency="",
                timestamp=timestamp,
            )
        ]
        if filled_amount > 0
        else [],
        timestamp=timestamp,
    )


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


def test_restore_from_db_rebuilds_orders_positions_and_active_index() -> None:
    manager = OrderManager(
        FakeOrderClient(),
        db=FakeOrderDB(
            [
                _order(
                    order_id="restore-buy",
                    side=OrderSide.BUY,
                    status=OrderStatus.CLOSED,
                    amount=1.0,
                    filled_amount=1.0,
                    price=100.0,
                ),
                _order(
                    order_id="restore-sell",
                    side=OrderSide.SELL,
                    status=OrderStatus.CLOSED,
                    amount=0.4,
                    filled_amount=0.4,
                    price=110.0,
                ),
                _order(
                    order_id="restore-open",
                    side=OrderSide.BUY,
                    status=OrderStatus.OPEN,
                    amount=0.5,
                    filled_amount=0.1,
                    price=120.0,
                ),
            ]
        ),
    )

    manager.restore_from_db()

    assert [order.id for order in manager.list_orders()] == [
        "restore-buy",
        "restore-sell",
        "restore-open",
    ]
    assert [order.id for order in manager.list_active_orders()] == ["restore-open"]
    position = manager.get_position("BTC/USDT")
    assert position.amount == pytest.approx(0.7)
    assert position.entry_price == pytest.approx(102.85714285714286)

    manager.apply_order_update(
        {
            "id": "restore-buy",
            "symbol": "BTC/USDT",
            "type": "market",
            "side": "buy",
            "amount": "1",
            "status": "filled",
            "filled": "1",
            "average": "100",
        }
    )
    assert manager.get_position("BTC/USDT").amount == pytest.approx(0.7)


def test_reconcile_active_orders_refreshes_restored_orders() -> None:
    client = FakeOrderClient()
    client.fetch_payloads.append(
        {
            "id": "restore-open",
            "symbol": "BTC/USDT",
            "type": "market",
            "side": "buy",
            "amount": "0.5",
            "status": "filled",
            "filled": "0.5",
            "remaining": "0",
            "average": "120",
        }
    )
    manager = OrderManager(
        client,
        db=FakeOrderDB(
            [
                _order(
                    order_id="restore-open",
                    side=OrderSide.BUY,
                    status=OrderStatus.OPEN,
                    amount=0.5,
                    filled_amount=0.1,
                    price=120.0,
                )
            ]
        ),
    )
    manager.restore_from_db()

    reconciled = _run(manager.reconcile_active_orders())

    assert [order.id for order in reconciled] == ["restore-open"]
    assert client.request_calls == [("fetch_order", ("restore-open", "BTC/USDT"))]
    assert manager.list_active_orders() == []
    position = manager.get_position("BTC/USDT")
    assert position.amount == pytest.approx(0.5)
    assert position.entry_price == pytest.approx(120)


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

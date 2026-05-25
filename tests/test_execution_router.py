from __future__ import annotations

from typing import Any

import pytest

from src.execution.router import (
    ExecutionUpdateRouter,
    execution_report_to_order_payload,
)
from src.exchange.websocket import ExecutionReportUpdate
from src.orders.manager import OrderManager


class NoopOrderClient:
    pass


class RecordingStrategy:
    def __init__(self) -> None:
        self.fills: list[Any] = []

    async def on_fill(self, fill: Any) -> None:
        self.fills.append(fill)


def _execution_report(
    *,
    order_id: int = 123,
    trade_id: int = 456,
    symbol: str = "BTC/USDT",
    status: str = "PARTIALLY_FILLED",
    last_quantity: float = 0.4,
    cumulative_quantity: float = 0.4,
    last_price: float = 100.0,
    cumulative_quote: float = 40.0,
) -> ExecutionReportUpdate:
    return ExecutionReportUpdate(
        event_time=1_700_000_000_000,
        symbol=symbol,
        client_order_id="client-1",
        side="BUY",
        order_type="LIMIT",
        order_quantity=1.0,
        order_price=100.0,
        execution_type="TRADE",
        order_status=status,
        order_id=order_id,
        last_executed_quantity=last_quantity,
        cumulative_filled_quantity=cumulative_quantity,
        last_executed_price=last_price,
        commission_amount=0.01,
        commission_asset="BNB",
        trade_time=1_700_000_000_001,
        trade_id=trade_id,
        order_creation_time=1_700_000_000_000,
        cumulative_quote_transacted=cumulative_quote,
        raw={"e": "executionReport"},
    )


def test_execution_report_to_order_payload_matches_order_manager_shape() -> None:
    payload = execution_report_to_order_payload(_execution_report())

    assert payload["id"] == "123"
    assert payload["orderId"] == "123"
    assert payload["symbol"] == "BTC/USDT"
    assert payload["type"] == "limit"
    assert payload["side"] == "buy"
    assert payload["amount"] == 1.0
    assert payload["price"] == 100.0
    assert payload["status"] == "partially_filled"
    assert payload["filled"] == 0.4
    assert payload["remaining"] == pytest.approx(0.6)
    assert payload["average"] == 100.0
    assert payload["trades"] == [
        {
            "id": "456",
            "orderId": "123",
            "symbol": "BTC/USDT",
            "side": "buy",
            "amount": 0.4,
            "price": 100.0,
            "commission": 0.01,
            "commissionAsset": "BNB",
            "timestamp": 1_700_000_000_001,
        }
    ]


async def test_execution_update_router_applies_order_update_and_notifies_strategy() -> None:
    manager = OrderManager(NoopOrderClient())
    strategy = RecordingStrategy()
    router = ExecutionUpdateRouter(manager, {"BTC/USDT": strategy})

    order = await router.route(_execution_report())

    assert order.id == "123"
    assert manager.get_position("BTC/USDT").amount == pytest.approx(0.4)
    assert len(strategy.fills) == 1
    assert strategy.fills[0].order_id == "123"
    assert strategy.fills[0].symbol == "BTC/USDT"
    assert strategy.fills[0].side == "buy"
    assert strategy.fills[0].price == 100.0
    assert strategy.fills[0].quantity == 0.4
    assert strategy.fills[0].commission == 0.01


async def test_execution_update_router_does_not_notify_duplicate_fills() -> None:
    manager = OrderManager(NoopOrderClient())
    strategy = RecordingStrategy()
    router = ExecutionUpdateRouter(manager, {"BTC/USDT": strategy})
    report = _execution_report()

    await router.route(report)
    await router.route(report)

    assert len(strategy.fills) == 1


async def test_execution_update_router_ignores_unmatched_strategy_symbol() -> None:
    manager = OrderManager(NoopOrderClient())
    strategy = RecordingStrategy()
    router = ExecutionUpdateRouter(manager, {"ETH/USDT": strategy})

    await router.route(_execution_report(symbol="BTC/USDT"))

    assert strategy.fills == []

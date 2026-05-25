"""Route exchange execution reports into orders and strategies."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from src.exchange.websocket import ExecutionReportUpdate
from src.orders.models import Fill, Order
from src.strategies.base import FillData


class ExecutionUpdateRouter:
    """Apply execution reports and dispatch newly observed fills."""

    def __init__(
        self,
        order_manager: Any,
        strategies_by_symbol: Mapping[str, Any],
    ) -> None:
        self._order_manager = order_manager
        self._strategies_by_symbol = strategies_by_symbol
        self._notified_fills: set[tuple[str, str]] = set()

    async def route(self, update: ExecutionReportUpdate) -> Order:
        """Apply an execution report and notify the matching strategy of fills."""
        order = self._order_manager.apply_order_update(
            execution_report_to_order_payload(update)
        )
        strategy = self._strategies_by_symbol.get(order.symbol)
        if strategy is None:
            return order

        for fill in order.fills:
            fill_key = (fill.order_id, fill.id)
            if fill_key in self._notified_fills:
                continue
            self._notified_fills.add(fill_key)
            await strategy.on_fill(_fill_to_strategy_fill(fill))

        return order


def execution_report_to_order_payload(
    update: ExecutionReportUpdate,
) -> dict[str, Any]:
    """Convert a Binance execution report into an OrderManager payload."""
    amount = update.order_quantity or update.cumulative_filled_quantity or 0.0
    filled = update.cumulative_filled_quantity or 0.0
    remaining = max(amount - filled, 0.0)
    average = None
    if update.cumulative_quote_transacted is not None and filled > 0:
        average = update.cumulative_quote_transacted / filled

    payload: dict[str, Any] = {
        "id": str(update.order_id),
        "orderId": str(update.order_id),
        "clientOrderId": update.client_order_id,
        "symbol": update.symbol,
        "type": update.order_type.lower(),
        "side": update.side.lower(),
        "amount": amount,
        "price": update.order_price or update.last_executed_price,
        "status": update.order_status.lower(),
        "filled": filled,
        "remaining": remaining,
        "average": average,
        "timestamp": update.order_creation_time or update.event_time,
        "info": dict(update.raw),
    }

    fill = _last_fill_payload(update)
    if fill is not None:
        payload["trades"] = [fill]

    return payload


def _last_fill_payload(update: ExecutionReportUpdate) -> dict[str, Any] | None:
    if (
        update.last_executed_quantity is None
        or update.last_executed_quantity <= 0
        or update.last_executed_price is None
        or update.last_executed_price <= 0
    ):
        return None

    return {
        "id": (
            str(update.trade_id)
            if update.trade_id is not None and update.trade_id >= 0
            else f"{update.order_id}:{update.event_time}"
        ),
        "orderId": str(update.order_id),
        "symbol": update.symbol,
        "side": update.side.lower(),
        "amount": update.last_executed_quantity,
        "price": update.last_executed_price,
        "commission": update.commission_amount or 0.0,
        "commissionAsset": update.commission_asset or "",
        "timestamp": update.trade_time or update.event_time,
    }


def _fill_to_strategy_fill(fill: Fill) -> FillData:
    return FillData(
        order_id=fill.order_id,
        symbol=fill.symbol,
        side=fill.side.value,
        price=fill.price,
        quantity=fill.amount,
        commission=fill.fee,
        timestamp=int(fill.timestamp.timestamp() * 1000),
    )

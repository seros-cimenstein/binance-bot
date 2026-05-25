from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

import pytest

from src.orders.models import Position
from src.orders.sizing import (
    OrderSizer,
    OrderSizingConfig,
    OrderSizingError,
    OrderSizingRequest,
)
from src.portfolio import PortfolioSnapshot, PortfolioSnapshotService


class FakeRestClient:
    def __init__(self, balance: dict[str, Any]) -> None:
        self.balance = balance

    async def get_balance(self) -> dict[str, Any]:
        return self.balance


class FakeRiskGuard:
    current_daily_loss = 75.0


class FakeOrderManager:
    def __init__(
        self,
        *,
        positions: list[Position] | None = None,
        open_orders: list[Any] | None = None,
    ) -> None:
        self.positions = positions or []
        self.open_orders = open_orders or []

    def list_positions(self) -> list[Position]:
        return list(self.positions)

    def list_open_orders(self) -> list[Any]:
        return list(self.open_orders)


class StaticPortfolioService:
    def __init__(self, snapshot: PortfolioSnapshot) -> None:
        self._snapshot = snapshot
        self.mark_prices: list[dict[str, float]] = []

    async def snapshot(self, *, mark_prices: dict[str, float]) -> PortfolioSnapshot:
        self.mark_prices.append(mark_prices)
        return self._snapshot


class FakeExchange:
    def __init__(self, market: dict[str, Any]) -> None:
        self.markets = {"BTC/USDT": market}


def _request(
    *,
    amount: float = 0.5,
    price: float | None = 100.0,
    side: str = "buy",
) -> OrderSizingRequest:
    return OrderSizingRequest(
        symbol="BTC/USDT",
        side=side,
        requested_amount=amount,
        price=price,
        order_type="market",
    )


def _portfolio(
    *,
    available: float = 1_000.0,
    equity: float = 2_000.0,
) -> PortfolioSnapshot:
    return PortfolioSnapshot(
        available_balance_usdt=available,
        total_balance_usdt=available,
        total_equity_usdt=equity,
    )


@pytest.mark.asyncio
async def test_portfolio_snapshot_reads_balance_positions_orders_and_pnl() -> None:
    rest_client = FakeRestClient(
        {
            "free": {"USDT": "900"},
            "used": {"USDT": "100"},
            "total": {"USDT": "1000"},
        }
    )
    order_manager = FakeOrderManager(
        positions=[
            Position(
                symbol="BTC/USDT",
                amount=0.01,
                entry_price=50_000.0,
                unrealized_pnl=25.0,
                timestamp=datetime.now(timezone.utc),
            )
        ],
        open_orders=[object(), object()],
    )
    service = PortfolioSnapshotService(rest_client, order_manager, FakeRiskGuard())

    snapshot = await service.snapshot(mark_prices={"BTC/USDT": 60_000.0})

    assert snapshot.balances_free == {"USDT": 900.0}
    assert snapshot.balances_used == {"USDT": 100.0}
    assert snapshot.balances_total == {"USDT": 1000.0}
    assert snapshot.available_balance_usdt == 900.0
    assert snapshot.total_balance_usdt == 1000.0
    assert snapshot.positions == {"BTC/USDT": 600.0}
    assert snapshot.total_exposure_usdt == 600.0
    assert snapshot.realized_pnl_today_usdt == -75.0
    assert snapshot.unrealized_pnl_today_usdt == 25.0
    assert snapshot.total_equity_usdt == 1625.0
    assert snapshot.open_orders_count == 2


@pytest.mark.asyncio
async def test_fixed_sizing_preserves_amount_and_rounds_to_exchange_precision() -> None:
    sizer = OrderSizer()
    exchange = FakeExchange(
        {
            "symbol": "BTC/USDT",
            "precision": {"amount": 3},
            "limits": {"amount": {"min": 0.001}, "cost": {"min": 10}},
        }
    )

    sized = await sizer.size_order(
        _request(amount=0.123456, price=1_000.0),
        OrderSizingConfig(min_order_notional_usd=10),
        exchange=exchange,
    )

    assert sized.amount == 0.123
    assert sized.notional_usdt == 123.0
    assert sized.portfolio is None


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("mode", "percent", "expected_amount"),
    [
        ("quote_percent", 10.0, 1.0),
        ("equity_percent", 5.0, 1.0),
        ("risk_percent", 10.0, 1.0),
    ],
)
async def test_percent_sizing_modes_resolve_target_notional(
    mode: str,
    percent: float,
    expected_amount: float,
) -> None:
    service = StaticPortfolioService(_portfolio(available=1_000.0, equity=2_000.0))

    sized = await OrderSizer().size_order(
        _request(price=100.0),
        OrderSizingConfig(
            mode=mode,
            order_size_percent=percent,
            max_position_size_usd=1_000.0,
        ),
        portfolio_service=service,
    )

    assert sized.amount == expected_amount
    assert sized.notional_usdt == 100.0
    assert service.mark_prices == [{"BTC/USDT": 100.0}]


@pytest.mark.asyncio
async def test_max_notional_clamps_sized_amount() -> None:
    sized = await OrderSizer().size_order(
        _request(amount=5.0, price=100.0),
        OrderSizingConfig(max_order_notional_usd=120.0),
    )

    assert sized.amount == 1.2
    assert sized.notional_usdt == 120.0


@pytest.mark.asyncio
async def test_minimum_notional_and_lot_size_are_rejected() -> None:
    with pytest.raises(OrderSizingError, match="order notional below minimum"):
        await OrderSizer().size_order(
            _request(amount=0.001, price=100.0),
            OrderSizingConfig(min_order_notional_usd=1.0),
        )

    with pytest.raises(OrderSizingError, match="below exchange lot size"):
        await OrderSizer().size_order(
            _request(amount=0.009, price=100.0),
            OrderSizingConfig(),
            exchange=FakeExchange({"limits": {"amount": {"min": 0.01}}}),
        )


@pytest.mark.asyncio
async def test_insufficient_balance_is_rejected_before_order_placement() -> None:
    service = StaticPortfolioService(_portfolio(available=40.0, equity=40.0))

    with pytest.raises(OrderSizingError, match="insufficient available balance"):
        await OrderSizer().size_order(
            _request(price=100.0),
            OrderSizingConfig(
                mode="quote_percent",
                order_size_percent=10.0,
                min_balance_usdt=50.0,
            ),
            portfolio_service=service,
        )

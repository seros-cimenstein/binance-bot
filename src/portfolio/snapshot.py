"""Account and local state snapshots for dynamic order sizing."""

from __future__ import annotations

import math
from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Any

from src.risk.guard import PortfolioState


DEFAULT_QUOTE_ASSET = "USDT"


@dataclass(frozen=True, slots=True)
class PortfolioSnapshot:
    """Combined account balance, position, order, and PnL state."""

    balances_free: dict[str, float] = field(default_factory=dict)
    balances_used: dict[str, float] = field(default_factory=dict)
    balances_total: dict[str, float] = field(default_factory=dict)
    available_balance_usdt: float = 0.0
    total_balance_usdt: float = 0.0
    total_equity_usdt: float = 0.0
    realized_pnl_today_usdt: float = 0.0
    unrealized_pnl_today_usdt: float = 0.0
    positions: dict[str, float] = field(default_factory=dict)
    total_exposure_usdt: float = 0.0
    open_orders_count: int = 0

    def to_risk_state(self) -> PortfolioState:
        """Return the snapshot in the structured shape accepted by RiskGuard."""
        return PortfolioState(
            total_equity_usdt=self.total_equity_usdt,
            available_balance_usdt=self.available_balance_usdt,
            realized_pnl_today_usdt=self.realized_pnl_today_usdt,
            unrealized_pnl_today_usdt=self.unrealized_pnl_today_usdt,
            positions=dict(self.positions),
            total_exposure_usdt=self.total_exposure_usdt,
            open_orders_count=self.open_orders_count,
        )


class PortfolioSnapshotService:
    """Build a portfolio snapshot from Binance balances and local state."""

    def __init__(
        self,
        rest_client: Any,
        order_manager: Any,
        risk_guard: Any | None = None,
        *,
        quote_asset: str = DEFAULT_QUOTE_ASSET,
    ) -> None:
        self._rest_client = rest_client
        self._order_manager = order_manager
        self._risk_guard = risk_guard
        self._quote_asset = quote_asset.strip().upper() or DEFAULT_QUOTE_ASSET

    async def snapshot(
        self,
        *,
        mark_prices: Mapping[str, float] | None = None,
    ) -> PortfolioSnapshot:
        """Read current account and local trading state."""
        get_balance = getattr(self._rest_client, "get_balance", None)
        if get_balance is None or not callable(get_balance):
            raise RuntimeError("rest client does not expose get_balance")

        raw_balance = await get_balance()
        free_balances = _balance_group(raw_balance, "free")
        used_balances = _balance_group(raw_balance, "used")
        total_balances = _balance_group(raw_balance, "total")

        available_quote = free_balances.get(self._quote_asset, 0.0)
        total_quote = total_balances.get(self._quote_asset, available_quote)
        positions, unrealized_pnl, total_exposure = self._position_snapshot(
            mark_prices or {}
        )
        realized_pnl = -_finite_float(
            getattr(self._risk_guard, "current_daily_loss", 0.0),
            default=0.0,
        )

        total_equity = total_quote + total_exposure + unrealized_pnl
        return PortfolioSnapshot(
            balances_free=free_balances,
            balances_used=used_balances,
            balances_total=total_balances,
            available_balance_usdt=available_quote,
            total_balance_usdt=total_quote,
            total_equity_usdt=max(total_equity, 0.0),
            realized_pnl_today_usdt=realized_pnl,
            unrealized_pnl_today_usdt=unrealized_pnl,
            positions=positions,
            total_exposure_usdt=total_exposure,
            open_orders_count=self._open_orders_count(),
        )

    def _position_snapshot(
        self,
        mark_prices: Mapping[str, float],
    ) -> tuple[dict[str, float], float, float]:
        positions: dict[str, float] = {}
        total_unrealized_pnl = 0.0

        for position in _list_positions(self._order_manager):
            symbol = str(getattr(position, "symbol", "")).strip()
            if not symbol:
                continue
            amount = _finite_float(getattr(position, "amount", 0.0), default=0.0)
            entry_price = _finite_float(
                getattr(position, "entry_price", 0.0),
                default=0.0,
            )
            mark_price = _finite_float(
                mark_prices.get(symbol, entry_price),
                default=entry_price,
            )
            if mark_price <= 0:
                mark_price = entry_price
            exposure = amount * mark_price
            if not math.isclose(exposure, 0.0, abs_tol=1e-12):
                positions[symbol] = exposure
            total_unrealized_pnl += _finite_float(
                getattr(position, "unrealized_pnl", 0.0),
                default=0.0,
            )

        total_exposure = sum(abs(value) for value in positions.values())
        return positions, total_unrealized_pnl, total_exposure

    def _open_orders_count(self) -> int:
        list_orders = getattr(self._order_manager, "list_open_orders", None)
        if list_orders is None or not callable(list_orders):
            list_orders = getattr(self._order_manager, "list_active_orders", None)
        if list_orders is None or not callable(list_orders):
            return 0
        return len(list_orders())


def _list_positions(order_manager: Any) -> list[Any]:
    list_positions = getattr(order_manager, "list_positions", None)
    if list_positions is None or not callable(list_positions):
        list_positions = getattr(order_manager, "get_positions", None)
    if list_positions is None or not callable(list_positions):
        return []
    return list(list_positions())


def _balance_group(raw_balance: Any, group_name: str) -> dict[str, float]:
    if not isinstance(raw_balance, Mapping):
        return {}
    group = raw_balance.get(group_name)
    if isinstance(group, Mapping):
        return {
            str(asset).strip().upper(): amount
            for asset, value in group.items()
            if (amount := _finite_float(value, default=None)) is not None
        }

    asset = raw_balance.get("asset")
    value = raw_balance.get(group_name)
    if asset is not None and value is not None:
        amount = _finite_float(value, default=None)
        if amount is not None:
            return {str(asset).strip().upper(): amount}
    return {}


def _finite_float(value: Any, *, default: float | None) -> float | None:
    try:
        result = float(value)
    except (TypeError, ValueError):
        return default
    if not math.isfinite(result):
        return default
    return result

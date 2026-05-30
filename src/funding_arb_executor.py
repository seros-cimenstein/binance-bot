"""Live execution layer for the funding-rate harvest runtime.

This module owns the actual order placement against Binance Spot and USD-M
Futures. It is intentionally minimal and aggressive about failure handling:

* ``open_position`` places spot BUY first, then perp SHORT. If the perp leg
  fails, it immediately sells the just-bought spot back to flatten before
  raising. This favours "no orphan leg" over "no rollback latency."
* ``close_position`` places perp BUY (close-short) first, then spot SELL.
  If the spot leg fails, the perp side is already closed so we have a clean
  USDT position; we raise but the state is consistent.
* ``reconcile`` reads on-exchange balances and futures positions and
  reports any mismatch with local state. The runtime halts on mismatch.

Cross-account collateral transfers (spot ↔ futures USDT) are handled via
the universal-transfer API. The runtime tops up futures collateral before
each open and sweeps the residual back after each close.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping, Protocol

from src.funding_arb import FundingArbError, FundingArbPosition


@dataclass(frozen=True, slots=True)
class ExecutionLeg:
    """Result of a single leg fill."""

    venue: str  # "spot" | "futures"
    side: str  # "buy" | "sell"
    symbol: str
    requested_qty: float
    filled_qty: float
    avg_price: float
    cost_usdt: float
    order_id: str


@dataclass(frozen=True, slots=True)
class OpenResult:
    spot_leg: ExecutionLeg
    perp_leg: ExecutionLeg
    position: FundingArbPosition


@dataclass(frozen=True, slots=True)
class CloseResult:
    perp_leg: ExecutionLeg
    spot_leg: ExecutionLeg
    realized_pnl_usdt: float


@dataclass(frozen=True, slots=True)
class ReconciliationMismatch:
    symbol: str
    detail: str


@dataclass(frozen=True, slots=True)
class ReconciliationReport:
    spot_balances: Mapping[str, float]
    futures_positions: Mapping[str, float]
    mismatches: tuple[ReconciliationMismatch, ...]

    @property
    def ok(self) -> bool:
        return not self.mismatches


class SpotExchange(Protocol):
    """Minimal spot exchange surface used by the executor (ccxt-shaped)."""

    async def fetch_ticker(self, symbol: str) -> Mapping[str, Any]: ...
    async def create_order(
        self,
        symbol: str,
        type: str,
        side: str,
        amount: float,
        price: float | None = ...,
        params: Mapping[str, Any] | None = ...,
    ) -> Mapping[str, Any]: ...
    async def fetch_balance(self) -> Mapping[str, Any]: ...


class FuturesExchange(Protocol):
    """Minimal USD-M futures surface (ccxt-shaped, e.g. ccxt.binanceusdm)."""

    async def fetch_ticker(self, symbol: str) -> Mapping[str, Any]: ...
    async def create_order(
        self,
        symbol: str,
        type: str,
        side: str,
        amount: float,
        price: float | None = ...,
        params: Mapping[str, Any] | None = ...,
    ) -> Mapping[str, Any]: ...
    async def fetch_balance(self) -> Mapping[str, Any]: ...
    async def fetch_positions(self, symbols: list[str] | None = ...) -> list[Mapping[str, Any]]: ...


class CollateralTransfer(Protocol):
    """Move USDT between spot and futures wallets."""

    async def transfer_spot_to_futures(self, amount_usdt: float) -> Mapping[str, Any]: ...
    async def transfer_futures_to_spot(self, amount_usdt: float) -> Mapping[str, Any]: ...


def _filled_qty(order: Mapping[str, Any]) -> float:
    return float(order.get("filled") or order.get("executedQty") or 0.0)


def _avg_price(order: Mapping[str, Any]) -> float:
    raw = order.get("average") or order.get("avgPrice") or order.get("price")
    return float(raw) if raw not in (None, "") else 0.0


def _order_id(order: Mapping[str, Any]) -> str:
    raw = order.get("id") or order.get("orderId") or ""
    return str(raw)


def _cost_usdt(order: Mapping[str, Any]) -> float:
    cost = order.get("cost")
    if cost not in (None, ""):
        try:
            return float(cost)
        except (TypeError, ValueError):
            pass
    return _filled_qty(order) * _avg_price(order)


class FundingArbExecutor:
    """Dual-leg open/close with rollback + reconciliation."""

    def __init__(
        self,
        *,
        spot: SpotExchange,
        futures: FuturesExchange,
        collateral: CollateralTransfer,
        leverage: int = 1,
    ) -> None:
        self.spot = spot
        self.futures = futures
        self.collateral = collateral
        self.leverage = leverage

    async def open_position(
        self,
        symbol: str,
        notional_usdt: float,
        *,
        entry_funding_bps: float,
        open_time_ms: int,
    ) -> OpenResult:
        """Open delta-neutral position: spot BUY then perp SHORT, with rollback."""
        if notional_usdt <= 0:
            raise FundingArbError("notional_usdt must be positive")
        ticker = await self.spot.fetch_ticker(symbol)
        spot_price = float(ticker.get("last") or ticker.get("close") or 0.0)
        if spot_price <= 0:
            raise FundingArbError(f"spot price unavailable for {symbol}")
        requested_qty = notional_usdt / spot_price

        # Top up futures collateral (notional/leverage) before placing perp.
        collateral_needed = notional_usdt / max(self.leverage, 1)
        await self.collateral.transfer_spot_to_futures(collateral_needed)

        # Leg 1: spot BUY.
        spot_order = await self.spot.create_order(
            symbol=symbol,
            type="market",
            side="buy",
            amount=requested_qty,
        )
        spot_filled = _filled_qty(spot_order)
        if spot_filled <= 0:
            raise FundingArbError(f"spot leg returned zero fill on {symbol}")
        spot_leg = ExecutionLeg(
            venue="spot",
            side="buy",
            symbol=symbol,
            requested_qty=requested_qty,
            filled_qty=spot_filled,
            avg_price=_avg_price(spot_order) or spot_price,
            cost_usdt=_cost_usdt(spot_order),
            order_id=_order_id(spot_order),
        )

        # Leg 2: perp SHORT for the filled spot qty.
        try:
            perp_order = await self.futures.create_order(
                symbol=symbol,
                type="market",
                side="sell",
                amount=spot_filled,
                params={"reduceOnly": False},
            )
        except Exception as exc:  # noqa: BLE001 — rollback any executor error
            await self._rollback_open_after_perp_failure(symbol, spot_leg)
            raise FundingArbError(
                f"perp short failed for {symbol} ({exc}); spot leg rolled back"
            ) from exc

        perp_filled = _filled_qty(perp_order)
        if perp_filled <= 0:
            await self._rollback_open_after_perp_failure(symbol, spot_leg)
            raise FundingArbError(
                f"perp short returned zero fill on {symbol}; spot leg rolled back"
            )

        perp_leg = ExecutionLeg(
            venue="futures",
            side="sell",
            symbol=symbol,
            requested_qty=spot_filled,
            filled_qty=perp_filled,
            avg_price=_avg_price(perp_order) or spot_price,
            cost_usdt=_cost_usdt(perp_order),
            order_id=_order_id(perp_order),
        )

        position = FundingArbPosition(
            symbol=symbol,
            open_time_ms=open_time_ms,
            entry_funding_bps=entry_funding_bps,
            notional_usdt=notional_usdt,
            spot_qty=spot_filled,
            perp_qty=-perp_filled,
        )
        return OpenResult(spot_leg=spot_leg, perp_leg=perp_leg, position=position)

    async def _rollback_open_after_perp_failure(
        self,
        symbol: str,
        spot_leg: ExecutionLeg,
    ) -> None:
        try:
            await self.spot.create_order(
                symbol=symbol,
                type="market",
                side="sell",
                amount=spot_leg.filled_qty,
            )
        except Exception:  # noqa: BLE001 — rollback failure means manual intervention
            raise FundingArbError(
                f"perp leg failed AND rollback sell failed on {symbol}: manual reconciliation required"
            )

    async def close_position(self, position: FundingArbPosition) -> CloseResult:
        """Close delta-neutral position: perp BUY (close short) then spot SELL."""
        if position.spot_qty <= 0:
            raise FundingArbError(f"position has no spot quantity to close: {position}")
        perp_close_qty = abs(position.perp_qty)
        if perp_close_qty <= 0:
            raise FundingArbError(f"position has no perp quantity to close: {position}")

        # Leg 1: close perp short (BUY).
        perp_order = await self.futures.create_order(
            symbol=position.symbol,
            type="market",
            side="buy",
            amount=perp_close_qty,
            params={"reduceOnly": True},
        )
        perp_filled = _filled_qty(perp_order)
        if perp_filled <= 0:
            raise FundingArbError(
                f"perp close returned zero fill on {position.symbol}; position still open"
            )
        perp_leg = ExecutionLeg(
            venue="futures",
            side="buy",
            symbol=position.symbol,
            requested_qty=perp_close_qty,
            filled_qty=perp_filled,
            avg_price=_avg_price(perp_order),
            cost_usdt=_cost_usdt(perp_order),
            order_id=_order_id(perp_order),
        )

        # Leg 2: sell spot.
        spot_order = await self.spot.create_order(
            symbol=position.symbol,
            type="market",
            side="sell",
            amount=position.spot_qty,
        )
        spot_filled = _filled_qty(spot_order)
        if spot_filled <= 0:
            raise FundingArbError(
                f"spot sell returned zero fill on {position.symbol}; perp already closed, manual reconciliation required"
            )
        spot_leg = ExecutionLeg(
            venue="spot",
            side="sell",
            symbol=position.symbol,
            requested_qty=position.spot_qty,
            filled_qty=spot_filled,
            avg_price=_avg_price(spot_order),
            cost_usdt=_cost_usdt(spot_order),
            order_id=_order_id(spot_order),
        )

        # Sweep collateral back to spot.
        collateral_returned = position.notional_usdt / max(self.leverage, 1)
        try:
            await self.collateral.transfer_futures_to_spot(collateral_returned)
        except Exception:  # noqa: BLE001 — non-fatal, can be done manually
            pass

        # Realized P&L approximation: spot_proceeds - perp_buy_cost + funding
        # received minus any execution slippage already captured in cost_usdt.
        spot_proceeds = spot_leg.cost_usdt
        perp_buy_cost = perp_leg.cost_usdt
        delta_neutral_pnl = spot_proceeds - perp_buy_cost
        realized = delta_neutral_pnl + position.funding_received_usdt
        return CloseResult(perp_leg=perp_leg, spot_leg=spot_leg, realized_pnl_usdt=realized)

    async def reconcile(
        self,
        local_positions: Mapping[str, FundingArbPosition],
    ) -> ReconciliationReport:
        """Compare exchange truth to local state.

        For each held symbol the executor verifies:
        * the spot leg balance >= position.spot_qty (with small tolerance)
        * the perp leg position size matches -position.perp_qty (with tolerance)
        """
        spot_balance = await self.spot.fetch_balance()
        free = (spot_balance.get("free") or {}) if isinstance(spot_balance, Mapping) else {}
        spot_free: dict[str, float] = {}
        for asset, amount in free.items() if isinstance(free, Mapping) else []:
            try:
                spot_free[str(asset)] = float(amount or 0.0)
            except (TypeError, ValueError):
                continue

        futures_positions_raw = await self.futures.fetch_positions(None)
        futures_positions: dict[str, float] = {}
        for row in futures_positions_raw:
            sym = str(row.get("symbol") or "")
            if not sym:
                continue
            try:
                qty = float(row.get("contracts") or row.get("positionAmt") or 0.0)
            except (TypeError, ValueError):
                continue
            futures_positions[sym] = qty

        tol = 1e-6
        mismatches: list[ReconciliationMismatch] = []
        for symbol, position in local_positions.items():
            base = symbol.split("/", 1)[0] if "/" in symbol else symbol
            spot_actual = spot_free.get(base, 0.0)
            if spot_actual + tol < position.spot_qty:
                mismatches.append(
                    ReconciliationMismatch(
                        symbol=symbol,
                        detail=(
                            f"spot {base} balance {spot_actual} is below required "
                            f"{position.spot_qty}"
                        ),
                    )
                )
            perp_actual = futures_positions.get(symbol, 0.0)
            if abs(perp_actual - position.perp_qty) > max(abs(position.perp_qty) * 0.01, tol):
                mismatches.append(
                    ReconciliationMismatch(
                        symbol=symbol,
                        detail=(
                            f"futures position {perp_actual} differs from expected "
                            f"{position.perp_qty} by more than 1% tolerance"
                        ),
                    )
                )
        return ReconciliationReport(
            spot_balances=spot_free,
            futures_positions=futures_positions,
            mismatches=tuple(mismatches),
        )

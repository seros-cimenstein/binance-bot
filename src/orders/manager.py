"""Order lifecycle management for Binance execution."""

from __future__ import annotations

import copy
import inspect
import math
import threading
import uuid
from collections.abc import Mapping
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

from .models import Fill, Order, OrderSide, OrderStatus, OrderType, Position

if TYPE_CHECKING:
    from src.db import OrderDB


ACTIVE_ORDER_STATUSES = frozenset({OrderStatus.PENDING, OrderStatus.OPEN})
TERMINAL_ORDER_STATUSES = frozenset(
    {
        OrderStatus.CLOSED,
        OrderStatus.CANCELED,
        OrderStatus.EXPIRED,
        OrderStatus.REJECTED,
    }
)
_EPSILON = 1e-12


class OrderManagerError(Exception):
    """Base exception for order manager failures."""


class OrderValidationError(OrderManagerError, ValueError):
    """Raised when an order request or update payload is invalid."""


class OrderNotFoundError(OrderManagerError, KeyError):
    """Raised when an order cannot be found in local or exchange state."""

    def __init__(self, order_id: str) -> None:
        """Initialize the exception with the missing order identifier."""
        super().__init__(f"order not found: {order_id}")
        self.order_id = order_id


class InvalidOrderTransitionError(OrderManagerError):
    """Raised when an order attempts an invalid lifecycle transition."""

    def __init__(
        self,
        order_id: str,
        current_status: OrderStatus,
        target_status: OrderStatus,
    ) -> None:
        """Initialize the exception with transition context."""
        super().__init__(
            "invalid order transition "
            f"{order_id}: {current_status.value} -> {target_status.value}"
        )
        self.order_id = order_id
        self.current_status = current_status
        self.target_status = target_status


class OrderStateMachine:
    """Validate order lifecycle transitions."""

    _TRANSITIONS: dict[OrderStatus, frozenset[OrderStatus]] = {
        OrderStatus.PENDING: frozenset(
            {
                OrderStatus.OPEN,
                OrderStatus.CLOSED,
                OrderStatus.CANCELED,
                OrderStatus.EXPIRED,
                OrderStatus.REJECTED,
            }
        ),
        OrderStatus.OPEN: frozenset(
            {
                OrderStatus.CLOSED,
                OrderStatus.CANCELED,
                OrderStatus.EXPIRED,
                OrderStatus.REJECTED,
            }
        ),
        OrderStatus.CLOSED: frozenset(),
        OrderStatus.CANCELED: frozenset(),
        OrderStatus.EXPIRED: frozenset(),
        OrderStatus.REJECTED: frozenset(),
    }

    @classmethod
    def can_transition(
        cls,
        current_status: OrderStatus,
        target_status: OrderStatus,
    ) -> bool:
        """Return whether the state machine accepts the transition."""
        return (
            current_status == target_status
            or target_status in cls._TRANSITIONS[current_status]
        )

    @classmethod
    def transition(cls, order: Order, target_status: OrderStatus) -> None:
        """Move an order to a target status or raise on invalid transitions."""
        if order.status == target_status:
            return
        if not cls.can_transition(order.status, target_status):
            raise InvalidOrderTransitionError(order.id, order.status, target_status)
        order.status = target_status


class OrderManager:
    """Coordinate exchange order calls and maintain local order state.

    The manager keeps every known order in memory for historical lookup, tracks
    active order IDs separately for fast open-order snapshots, and derives open
    positions from monotonically increasing filled quantities.
    """

    def __init__(self, client: Any, *, db: "OrderDB | None" = None) -> None:
        """Initialize the manager around a Binance-like async client."""
        self.client = client
        self._db = db
        self._orders: dict[str, Order] = {}
        self._order_ids_by_symbol: dict[str, set[str]] = {}
        self._active_order_ids: set[str] = set()
        self._positions: dict[str, Position] = {}
        self._position_applied_amounts: dict[str, float] = {}
        self._lock = threading.RLock()
        self._state_machine = OrderStateMachine()

    def restore_from_db(self) -> None:
        """Rebuild in-memory state from persisted orders on startup."""
        if self._db is None:
            return
        orders = self._db.load_orders()
        with self._lock:
            for order in orders:
                self._orders[order.id] = order
                self._order_ids_by_symbol.setdefault(order.symbol, set()).add(order.id)
                self._sync_active_index_unlocked(order)
                self._position_applied_amounts[order.id] = order.filled_amount

    async def place_order(
        self,
        symbol: str,
        order_type: OrderType | str,
        side: OrderSide | str,
        amount: float,
        price: float | None = None,
        *,
        client_order_id: str | None = None,
    ) -> Order:
        """Place an order and reconcile the exchange response into local state."""
        symbol_normalized = _normalize_symbol(symbol)
        order_type_enum = _coerce_order_type(order_type)
        side_enum = _coerce_order_side(side)
        amount_normalized = _positive_float(amount, "amount")
        price_normalized = _optional_positive_float(price, "price")
        if order_type_enum is OrderType.LIMIT and price_normalized is None:
            raise OrderValidationError("price is required for limit orders")

        local_order_id = _normalize_optional_id(client_order_id) or _local_order_id()
        order = Order(
            id=local_order_id,
            symbol=symbol_normalized,
            type=order_type_enum,
            side=side_enum,
            amount=amount_normalized,
            price=price_normalized,
            status=OrderStatus.PENDING,
            remaining_amount=amount_normalized,
            timestamp=_utcnow(),
        )
        with self._lock:
            if local_order_id in self._orders:
                raise OrderValidationError(f"duplicate local order id: {local_order_id}")
            self._store_order_unlocked(order)

        try:
            payload = await self._create_exchange_order(
                symbol=symbol_normalized,
                order_type=order_type_enum,
                side=side_enum,
                amount=amount_normalized,
                price=price_normalized,
            )
        except Exception:
            with self._lock:
                current_order = self._orders.get(local_order_id)
                if current_order is not None:
                    self._transition_unlocked(current_order, OrderStatus.REJECTED)
            raise

        with self._lock:
            current_order = self._orders[local_order_id]
            updated_order = self._reconcile_order_unlocked(
                current_order,
                payload,
                default_status=OrderStatus.OPEN,
            )
            return _snapshot_order(updated_order)

    async def cancel_order(self, order_id: str, symbol: str | None = None) -> Order:
        """Cancel an active order and update local state with the response."""
        order_id_normalized = _normalize_id(order_id)
        with self._lock:
            order = self._orders.get(order_id_normalized)
            if order is None:
                if symbol is None:
                    raise OrderNotFoundError(order_id_normalized)
                symbol_normalized = _normalize_symbol(symbol)
            else:
                if order.status in TERMINAL_ORDER_STATUSES:
                    raise InvalidOrderTransitionError(
                        order.id,
                        order.status,
                        OrderStatus.CANCELED,
                    )
                symbol_normalized = _normalize_symbol(symbol or order.symbol)

        payload = await self._cancel_exchange_order(order_id_normalized, symbol_normalized)
        with self._lock:
            order = self._orders.get(order_id_normalized)
            if order is None:
                order = self._order_from_payload_unlocked(
                    payload,
                    fallback_id=order_id_normalized,
                    fallback_symbol=symbol_normalized,
                    default_status=OrderStatus.CANCELED,
                )
                self._store_order_unlocked(order)
            else:
                order = self._reconcile_order_unlocked(
                    order,
                    payload,
                    default_status=OrderStatus.CANCELED,
                )
            return _snapshot_order(order)

    async def refresh_order(self, order_id: str, symbol: str | None = None) -> Order:
        """Fetch an order from the exchange and reconcile local state."""
        order_id_normalized = _normalize_id(order_id)
        with self._lock:
            local_order = self._orders.get(order_id_normalized)
            symbol_normalized = _normalize_symbol(symbol or local_order.symbol) if (
                symbol is not None or local_order is not None
            ) else None

        payload = await self._fetch_exchange_order(order_id_normalized, symbol_normalized)
        with self._lock:
            order = self._orders.get(order_id_normalized)
            if order is None:
                order = self._order_from_payload_unlocked(
                    payload,
                    fallback_id=order_id_normalized,
                    fallback_symbol=symbol_normalized,
                )
                self._store_order_unlocked(order)
            else:
                order = self._reconcile_order_unlocked(order, payload)
            return _snapshot_order(order)

    async def get_order(
        self,
        order_id: str,
        *,
        symbol: str | None = None,
        refresh: bool = False,
    ) -> Order:
        """Return a local order snapshot, optionally refreshed from Binance."""
        if refresh:
            return await self.refresh_order(order_id, symbol)

        order_id_normalized = _normalize_id(order_id)
        with self._lock:
            order = self._orders.get(order_id_normalized)
            if order is None:
                raise OrderNotFoundError(order_id_normalized)
            return _snapshot_order(order)

    async def get_order_status(
        self,
        order_id: str,
        *,
        symbol: str | None = None,
        refresh: bool = False,
    ) -> OrderStatus:
        """Return the lifecycle status for an order."""
        order = await self.get_order(order_id, symbol=symbol, refresh=refresh)
        return order.status

    def apply_order_update(self, payload: Mapping[str, Any]) -> Order:
        """Apply a REST or websocket order payload to local state."""
        with self._lock:
            order_id = _extract_order_id(payload)
            order = self._orders.get(order_id)
            if order is None:
                order = self._order_from_payload_unlocked(payload, fallback_id=order_id)
                self._store_order_unlocked(order)
            else:
                order = self._reconcile_order_unlocked(order, payload)
            return _snapshot_order(order)

    def list_orders(self, symbol: str | None = None) -> list[Order]:
        """Return snapshots for all known historical orders."""
        with self._lock:
            orders = self._orders_for_symbol_unlocked(symbol)
            return [_snapshot_order(order) for order in orders]

    def list_active_orders(self, symbol: str | None = None) -> list[Order]:
        """Return snapshots for pending and open orders."""
        with self._lock:
            active_orders = [
                self._orders[order_id]
                for order_id in self._active_order_ids
                if order_id in self._orders
            ]
            if symbol is not None:
                symbol_normalized = _normalize_symbol(symbol)
                active_orders = [
                    order
                    for order in active_orders
                    if order.symbol == symbol_normalized
                ]
            active_orders.sort(key=lambda order: order.timestamp)
            return [_snapshot_order(order) for order in active_orders]

    def list_open_orders(self, symbol: str | None = None) -> list[Order]:
        """Alias for active order snapshots."""
        return self.list_active_orders(symbol)

    def get_position(self, symbol: str) -> Position | None:
        """Return the current open position for a symbol, if any."""
        symbol_normalized = _normalize_symbol(symbol)
        with self._lock:
            position = self._positions.get(symbol_normalized)
            return _snapshot_position(position) if position is not None else None

    def list_positions(self) -> list[Position]:
        """Return snapshots for all non-flat positions."""
        with self._lock:
            positions = sorted(
                self._positions.values(),
                key=lambda position: position.symbol,
            )
            return [_snapshot_position(position) for position in positions]

    def _store_order_unlocked(self, order: Order) -> None:
        """Insert or replace an order in all in-memory indexes."""
        previous_order = self._orders.get(order.id)
        if previous_order is not None and previous_order.symbol != order.symbol:
            previous_symbol_ids = self._order_ids_by_symbol.get(previous_order.symbol)
            if previous_symbol_ids is not None:
                previous_symbol_ids.discard(order.id)
                if not previous_symbol_ids:
                    self._order_ids_by_symbol.pop(previous_order.symbol, None)

        self._orders[order.id] = order
        self._order_ids_by_symbol.setdefault(order.symbol, set()).add(order.id)
        self._sync_active_index_unlocked(order)
        if self._db is not None:
            self._db.upsert_order(order)

    def _rekey_order_unlocked(self, order: Order, target_order_id: str) -> Order:
        """Replace a local temporary ID with the exchange order ID."""
        if order.id == target_order_id:
            return order

        if target_order_id in self._orders:
            raise OrderValidationError(f"duplicate order id from exchange: {target_order_id}")

        previous_id = order.id
        self._orders.pop(previous_id, None)
        self._active_order_ids.discard(previous_id)
        symbol_ids = self._order_ids_by_symbol.get(order.symbol)
        if symbol_ids is not None:
            symbol_ids.discard(previous_id)
        if self._db is not None:
            self._db.delete_order(previous_id)

        applied_amount = self._position_applied_amounts.pop(previous_id, 0.0)
        if applied_amount:
            self._position_applied_amounts[target_order_id] = applied_amount

        order.id = target_order_id
        self._store_order_unlocked(order)
        return order

    def _order_from_payload_unlocked(
        self,
        payload: Mapping[str, Any],
        *,
        fallback_id: str | None = None,
        fallback_symbol: str | None = None,
        default_status: OrderStatus | None = None,
    ) -> Order:
        """Build an order from an exchange payload not already known locally."""
        order_id = _extract_order_id(payload, fallback=fallback_id)
        symbol = _extract_symbol(payload, fallback=fallback_symbol)
        order_type = _extract_order_type(payload)
        side = _extract_order_side(payload)
        amount = _extract_amount(payload)
        filled_amount = _extract_filled_amount(payload, fallback=0.0)
        remaining_amount = _extract_remaining_amount(
            payload,
            amount=amount,
            filled_amount=filled_amount,
            fallback=max(amount - filled_amount, 0.0),
        )
        status = _extract_order_status(
            payload,
            default_status=default_status,
            amount=amount,
            filled_amount=filled_amount,
        )
        order = Order(
            id=order_id,
            symbol=symbol,
            type=order_type,
            side=side,
            amount=amount,
            price=_extract_price(payload, fallback=None),
            status=status,
            filled_amount=filled_amount,
            remaining_amount=remaining_amount,
            avg_fill_price=_extract_average_price(
                payload,
                filled_amount=filled_amount,
                fallback=None,
            ),
            fills=_extract_fills(payload, order_id=order_id, symbol=symbol, side=side),
            timestamp=_extract_timestamp(payload),
        )
        self._apply_position_delta_unlocked(order, filled_amount)
        return order

    def _reconcile_order_unlocked(
        self,
        order: Order,
        payload: Mapping[str, Any] | None,
        *,
        default_status: OrderStatus | None = None,
    ) -> Order:
        """Merge an exchange payload into an existing order."""
        if payload is None:
            payload = {}
        if not isinstance(payload, Mapping):
            raise OrderValidationError("order payload must be a mapping")

        exchange_order_id = _extract_order_id(payload, fallback=order.id)
        new_symbol = _extract_symbol(payload, fallback=order.symbol)
        new_type = _extract_order_type(payload, fallback=order.type)
        new_side = _extract_order_side(payload, fallback=order.side)
        new_amount = _extract_amount(payload, fallback=order.amount)
        new_price = _extract_price(payload, fallback=order.price)
        new_filled_amount = _extract_filled_amount(
            payload,
            fallback=order.filled_amount,
        )
        new_remaining_amount = _extract_remaining_amount(
            payload,
            amount=new_amount,
            filled_amount=new_filled_amount,
            fallback=max(new_amount - new_filled_amount, 0.0),
        )
        previous_average_price = order.avg_fill_price
        new_average_price = _extract_average_price(
            payload,
            filled_amount=new_filled_amount,
            fallback=order.avg_fill_price,
        )
        target_status = _extract_order_status(
            payload,
            default_status=default_status or order.status,
            amount=new_amount,
            filled_amount=new_filled_amount,
        )
        incoming_fills = _extract_fills(
            payload,
            order_id=exchange_order_id,
            symbol=new_symbol,
            side=new_side,
        )

        if not self._state_machine.can_transition(order.status, target_status):
            raise InvalidOrderTransitionError(order.id, order.status, target_status)

        order = self._rekey_order_unlocked(order, exchange_order_id)

        previous_symbol = order.symbol
        order.symbol = new_symbol
        if previous_symbol != new_symbol:
            previous_symbol_ids = self._order_ids_by_symbol.get(previous_symbol)
            if previous_symbol_ids is not None:
                previous_symbol_ids.discard(order.id)
            self._order_ids_by_symbol.setdefault(new_symbol, set()).add(order.id)

        order.type = new_type
        order.side = new_side
        order.amount = new_amount
        order.price = new_price
        order.remaining_amount = new_remaining_amount
        order.avg_fill_price = new_average_price
        self._transition_unlocked(order, target_status)

        existing_fills = order.fills
        if incoming_fills and sum(fill.amount for fill in incoming_fills) >= (
            new_filled_amount - _EPSILON
        ):
            existing_fills = [
                fill
                for fill in existing_fills
                if not fill.id.startswith(f"{order.id}:synthetic:")
            ]
        order.fills = _merge_fills(existing_fills, incoming_fills)
        self._apply_position_delta_unlocked(
            order,
            new_filled_amount,
            previous_average_price=previous_average_price,
        )
        order.filled_amount = new_filled_amount
        self._sync_active_index_unlocked(order)
        if self._db is not None:
            self._db.upsert_order(order)
        return order

    def _transition_unlocked(self, order: Order, target_status: OrderStatus) -> None:
        """Apply a state-machine transition and refresh active indexes."""
        self._state_machine.transition(order, target_status)
        self._sync_active_index_unlocked(order)

    def _sync_active_index_unlocked(self, order: Order) -> None:
        """Keep the active order ID index consistent with order status."""
        if order.status in ACTIVE_ORDER_STATUSES:
            self._active_order_ids.add(order.id)
        else:
            self._active_order_ids.discard(order.id)

    def _apply_position_delta_unlocked(
        self,
        order: Order,
        target_filled_amount: float,
        *,
        previous_average_price: float | None = None,
    ) -> None:
        """Apply only newly filled quantity to the symbol position."""
        already_applied = self._position_applied_amounts.get(order.id, 0.0)
        fill_delta = target_filled_amount - already_applied
        if fill_delta <= _EPSILON:
            return

        fill_price = _position_fill_price(
            order,
            fill_delta=fill_delta,
            target_filled_amount=target_filled_amount,
            already_applied=already_applied,
            previous_average_price=previous_average_price,
        )
        self._apply_position_fill_unlocked(
            symbol=order.symbol,
            side=order.side,
            amount=fill_delta,
            price=fill_price,
        )
        self._position_applied_amounts[order.id] = target_filled_amount

        if not order.fills:
            order.fills.append(
                Fill(
                    id=f"{order.id}:synthetic:{target_filled_amount:g}",
                    order_id=order.id,
                    symbol=order.symbol,
                    side=order.side,
                    price=fill_price,
                    amount=fill_delta,
                    fee=0.0,
                    fee_currency="",
                    timestamp=_utcnow(),
                )
            )

    def _apply_position_fill_unlocked(
        self,
        *,
        symbol: str,
        side: OrderSide,
        amount: float,
        price: float,
    ) -> None:
        """Update the net position for a single fill."""
        signed_amount = amount if side is OrderSide.BUY else -amount
        existing = self._positions.get(symbol)
        if existing is None:
            if abs(signed_amount) > _EPSILON:
                self._positions[symbol] = Position(
                    symbol=symbol,
                    amount=signed_amount,
                    entry_price=price,
                    timestamp=_utcnow(),
                )
            return

        old_amount = existing.amount
        new_amount = old_amount + signed_amount
        if abs(new_amount) <= _EPSILON:
            self._positions.pop(symbol, None)
            return

        if _same_sign(old_amount, signed_amount):
            old_notional = abs(old_amount) * existing.entry_price
            fill_notional = abs(signed_amount) * price
            existing.entry_price = (old_notional + fill_notional) / abs(new_amount)
        elif not _same_sign(old_amount, new_amount):
            existing.entry_price = price

        existing.amount = new_amount
        existing.timestamp = _utcnow()

    def _orders_for_symbol_unlocked(self, symbol: str | None) -> list[Order]:
        """Return locally stored orders filtered by symbol."""
        if symbol is None:
            orders = list(self._orders.values())
        else:
            symbol_normalized = _normalize_symbol(symbol)
            orders = [
                self._orders[order_id]
                for order_id in self._order_ids_by_symbol.get(symbol_normalized, set())
                if order_id in self._orders
            ]
        orders.sort(key=lambda order: order.timestamp)
        return orders

    async def _create_exchange_order(
        self,
        *,
        symbol: str,
        order_type: OrderType,
        side: OrderSide,
        amount: float,
        price: float | None,
    ) -> Mapping[str, Any]:
        """Call the configured client with its supported create-order shape."""
        create_order = getattr(self.client, "create_order", None)
        if create_order is None or not callable(create_order):
            raise OrderManagerError("client does not expose create_order")

        if _call_accepts_parameter(create_order, "order_type"):
            payload = await create_order(
                symbol=symbol,
                order_type=order_type.value,
                side=side.value,
                amount=amount,
                price=price,
            )
        else:
            payload = await create_order(symbol, side.value, amount, price)
        return _ensure_mapping_payload(payload)

    async def _cancel_exchange_order(
        self,
        order_id: str,
        symbol: str,
    ) -> Mapping[str, Any]:
        """Call the configured client cancellation method."""
        cancel_order = getattr(self.client, "cancel_order", None)
        if cancel_order is None or not callable(cancel_order):
            raise OrderManagerError("client does not expose cancel_order")
        payload = await cancel_order(order_id, symbol)
        return _ensure_mapping_payload(payload)

    async def _fetch_exchange_order(
        self,
        order_id: str,
        symbol: str | None,
    ) -> Mapping[str, Any]:
        """Fetch an order through a dedicated method or generic ccxt request."""
        fetch_order = getattr(self.client, "fetch_order", None)
        if fetch_order is not None and callable(fetch_order):
            payload = await fetch_order(order_id, symbol)
            return _ensure_mapping_payload(payload)

        request = getattr(self.client, "request", None)
        if request is not None and callable(request):
            if symbol is None:
                payload = await request("fetch_order", order_id)
            else:
                payload = await request("fetch_order", order_id, symbol)
            return _ensure_mapping_payload(payload)

        raise OrderManagerError("client does not expose fetch_order or request")


def _call_accepts_parameter(callable_object: Any, parameter_name: str) -> bool:
    """Return whether a callable signature exposes a named parameter."""
    try:
        signature = inspect.signature(callable_object)
    except (TypeError, ValueError):
        return True
    return parameter_name in signature.parameters


def _ensure_mapping_payload(payload: Any) -> Mapping[str, Any]:
    """Validate that exchange responses look like ccxt order dictionaries."""
    if payload is None:
        return {}
    if not isinstance(payload, Mapping):
        raise OrderValidationError("exchange order response must be a mapping")
    return payload


def _normalize_symbol(symbol: str) -> str:
    """Normalize and validate a market symbol."""
    if not isinstance(symbol, str):
        raise OrderValidationError("symbol must be a string")
    symbol_normalized = symbol.strip()
    if not symbol_normalized:
        raise OrderValidationError("symbol must not be blank")
    return symbol_normalized


def _normalize_id(order_id: str) -> str:
    """Normalize and validate an order identifier."""
    order_id_normalized = _normalize_optional_id(order_id)
    if order_id_normalized is None:
        raise OrderValidationError("order_id must not be blank")
    return order_id_normalized


def _normalize_optional_id(order_id: str | None) -> str | None:
    """Normalize an optional order identifier."""
    if order_id is None:
        return None
    order_id_normalized = str(order_id).strip()
    return order_id_normalized or None


def _local_order_id() -> str:
    """Return a temporary client-side order ID."""
    return f"local-{uuid.uuid4().hex}"


def _positive_float(value: Any, field_name: str) -> float:
    """Coerce a strictly positive finite float."""
    try:
        numeric_value = float(value)
    except (TypeError, ValueError) as exc:
        raise OrderValidationError(f"{field_name} must be a finite number") from exc
    if not math.isfinite(numeric_value) or numeric_value <= 0:
        raise OrderValidationError(f"{field_name} must be greater than zero")
    return numeric_value


def _optional_positive_float(value: Any, field_name: str) -> float | None:
    """Coerce an optional strictly positive finite float."""
    if value is None:
        return None
    return _positive_float(value, field_name)


def _coerce_order_type(value: OrderType | str) -> OrderType:
    """Coerce an order type value into the local enum."""
    if isinstance(value, OrderType):
        return value
    if isinstance(value, str):
        try:
            return OrderType(value.strip().lower())
        except ValueError as exc:
            raise OrderValidationError("order_type must be 'limit' or 'market'") from exc
    raise OrderValidationError("order_type must be a string or OrderType")


def _coerce_order_side(value: OrderSide | str) -> OrderSide:
    """Coerce an order side value into the local enum."""
    if isinstance(value, OrderSide):
        return value
    if isinstance(value, str):
        try:
            return OrderSide(value.strip().lower())
        except ValueError as exc:
            raise OrderValidationError("side must be 'buy' or 'sell'") from exc
    raise OrderValidationError("side must be a string or OrderSide")


def _extract_order_id(payload: Mapping[str, Any], fallback: str | None = None) -> str:
    """Extract a ccxt or raw Binance order ID."""
    candidates = (
        payload.get("id"),
        payload.get("orderId"),
        payload.get("order_id"),
        _nested_info_value(payload, "orderId"),
        _nested_info_value(payload, "id"),
        fallback,
    )
    for candidate in candidates:
        normalized = _normalize_optional_id(candidate)
        if normalized is not None:
            return normalized
    raise OrderValidationError("order payload is missing an order id")


def _extract_symbol(payload: Mapping[str, Any], fallback: str | None = None) -> str:
    """Extract a market symbol from an order payload."""
    candidates = (
        payload.get("symbol"),
        _nested_info_value(payload, "symbol"),
        fallback,
    )
    for candidate in candidates:
        if candidate is not None:
            return _normalize_symbol(str(candidate))
    raise OrderValidationError("order payload is missing a symbol")


def _extract_order_type(
    payload: Mapping[str, Any],
    fallback: OrderType | None = None,
) -> OrderType:
    """Extract an order type from an order payload."""
    candidates = (
        payload.get("type"),
        payload.get("orderType"),
        _nested_info_value(payload, "type"),
        fallback,
    )
    for candidate in candidates:
        if candidate is None:
            continue
        if isinstance(candidate, OrderType):
            return candidate
        candidate_normalized = str(candidate).strip().lower()
        if candidate_normalized in {"limit_maker", "limit maker"}:
            candidate_normalized = "limit"
        try:
            return OrderType(candidate_normalized)
        except ValueError as exc:
            raise OrderValidationError("order payload has unsupported order type") from exc
    raise OrderValidationError("order payload is missing an order type")


def _extract_order_side(
    payload: Mapping[str, Any],
    fallback: OrderSide | None = None,
) -> OrderSide:
    """Extract an order side from an order payload."""
    candidates = (
        payload.get("side"),
        _nested_info_value(payload, "side"),
        fallback,
    )
    for candidate in candidates:
        if candidate is None:
            continue
        if isinstance(candidate, OrderSide):
            return candidate
        try:
            return OrderSide(str(candidate).strip().lower())
        except ValueError as exc:
            raise OrderValidationError("order payload has unsupported order side") from exc
    raise OrderValidationError("order payload is missing an order side")


def _extract_amount(payload: Mapping[str, Any], fallback: float | None = None) -> float:
    """Extract the original order quantity."""
    for candidate in (
        payload.get("amount"),
        payload.get("origQty"),
        _nested_info_value(payload, "origQty"),
        fallback,
    ):
        numeric_value = _optional_float(candidate)
        if numeric_value is not None:
            if numeric_value <= 0:
                raise OrderValidationError("order amount must be greater than zero")
            return numeric_value
    raise OrderValidationError("order payload is missing an amount")


def _extract_price(
    payload: Mapping[str, Any],
    fallback: float | None = None,
) -> float | None:
    """Extract an optional order price."""
    for candidate in (
        payload.get("price"),
        _nested_info_value(payload, "price"),
        fallback,
    ):
        numeric_value = _optional_float(candidate)
        if numeric_value is not None and numeric_value > 0:
            return numeric_value
    return None


def _extract_filled_amount(
    payload: Mapping[str, Any],
    *,
    fallback: float,
) -> float:
    """Extract filled quantity from ccxt or raw Binance payloads."""
    for candidate in (
        payload.get("filled"),
        payload.get("filledAmount"),
        payload.get("executedQty"),
        _nested_info_value(payload, "executedQty"),
    ):
        numeric_value = _optional_float(candidate)
        if numeric_value is not None:
            return max(numeric_value, 0.0)

    fills = _raw_fills(payload)
    if fills:
        return sum(_optional_float(fill.get("amount")) or 0.0 for fill in fills)
    return fallback


def _extract_remaining_amount(
    payload: Mapping[str, Any],
    *,
    amount: float,
    filled_amount: float,
    fallback: float,
) -> float:
    """Extract the unfilled quantity."""
    for candidate in (
        payload.get("remaining"),
        payload.get("remainingAmount"),
    ):
        numeric_value = _optional_float(candidate)
        if numeric_value is not None:
            return max(numeric_value, 0.0)
    return max(min(fallback, amount - filled_amount), 0.0)


def _extract_average_price(
    payload: Mapping[str, Any],
    *,
    filled_amount: float,
    fallback: float | None,
) -> float | None:
    """Extract or derive the average fill price."""
    for candidate in (
        payload.get("average"),
        payload.get("avg_fill_price"),
        payload.get("avgPrice"),
        _nested_info_value(payload, "avgPrice"),
    ):
        numeric_value = _optional_float(candidate)
        if numeric_value is not None and numeric_value > 0:
            return numeric_value

    cost = _optional_float(payload.get("cost")) or _optional_float(
        payload.get("cummulativeQuoteQty")
    ) or _optional_float(_nested_info_value(payload, "cummulativeQuoteQty"))
    if cost is not None and filled_amount > _EPSILON:
        return cost / filled_amount
    return fallback


def _extract_order_status(
    payload: Mapping[str, Any],
    *,
    default_status: OrderStatus | None,
    amount: float,
    filled_amount: float,
) -> OrderStatus:
    """Extract and normalize the exchange order status."""
    raw_status = payload.get("status") or _nested_info_value(payload, "status")
    if raw_status is not None:
        return _normalize_order_status(str(raw_status))
    if filled_amount >= amount - _EPSILON:
        return OrderStatus.CLOSED
    return default_status or OrderStatus.PENDING


def _normalize_order_status(raw_status: str) -> OrderStatus:
    """Map ccxt and raw Binance statuses to local lifecycle statuses."""
    status = raw_status.strip().lower()
    status_map = {
        "pending": OrderStatus.PENDING,
        "new": OrderStatus.OPEN,
        "open": OrderStatus.OPEN,
        "partially_filled": OrderStatus.OPEN,
        "partially filled": OrderStatus.OPEN,
        "filled": OrderStatus.CLOSED,
        "closed": OrderStatus.CLOSED,
        "done": OrderStatus.CLOSED,
        "canceled": OrderStatus.CANCELED,
        "cancelled": OrderStatus.CANCELED,
        "expired": OrderStatus.EXPIRED,
        "rejected": OrderStatus.REJECTED,
    }
    try:
        return status_map[status]
    except KeyError as exc:
        raise OrderValidationError(f"unsupported order status: {raw_status}") from exc


def _extract_timestamp(payload: Mapping[str, Any]) -> datetime:
    """Extract a UTC timestamp from ccxt or Binance payloads."""
    for candidate in (
        payload.get("timestamp"),
        payload.get("time"),
        payload.get("transactTime"),
        _nested_info_value(payload, "time"),
        _nested_info_value(payload, "transactTime"),
    ):
        timestamp = _datetime_from_timestamp(candidate)
        if timestamp is not None:
            return timestamp

    datetime_value = payload.get("datetime")
    if isinstance(datetime_value, str):
        try:
            parsed = datetime.fromisoformat(datetime_value.replace("Z", "+00:00"))
            if parsed.tzinfo is not None:
                parsed = parsed.astimezone(timezone.utc).replace(tzinfo=None)
            return parsed
        except ValueError:
            pass
    return _utcnow()


def _extract_fills(
    payload: Mapping[str, Any],
    *,
    order_id: str,
    symbol: str,
    side: OrderSide,
) -> list[Fill]:
    """Extract individual fills from ccxt ``trades`` or ``fills`` fields."""
    fills: list[Fill] = []
    for index, raw_fill in enumerate(_raw_fills(payload)):
        fill_price = _optional_float(raw_fill.get("price"))
        fill_amount = _optional_float(
            raw_fill.get("amount")
            if "amount" in raw_fill
            else raw_fill.get("qty")
        )
        if fill_price is None or fill_price <= 0 or fill_amount is None:
            continue

        fee_data = raw_fill.get("fee")
        fee = 0.0
        fee_currency = ""
        if isinstance(fee_data, Mapping):
            fee = _optional_float(fee_data.get("cost")) or 0.0
            currency = fee_data.get("currency")
            fee_currency = str(currency) if currency is not None else ""
        else:
            fee = _optional_float(raw_fill.get("commission")) or 0.0
            commission_asset = raw_fill.get("commissionAsset")
            fee_currency = (
                str(commission_asset) if commission_asset is not None else ""
            )

        fill_id = _normalize_optional_id(raw_fill.get("id")) or (
            f"{order_id}:fill:{index}:{fill_amount:g}@{fill_price:g}"
        )
        fill_symbol = (
            _normalize_symbol(str(raw_fill.get("symbol")))
            if raw_fill.get("symbol") is not None
            else symbol
        )
        fill_side = (
            _coerce_order_side(str(raw_fill.get("side")))
            if raw_fill.get("side") is not None
            else side
        )
        fills.append(
            Fill(
                id=fill_id,
                order_id=order_id,
                symbol=fill_symbol,
                side=fill_side,
                price=fill_price,
                amount=max(fill_amount, 0.0),
                fee=fee,
                fee_currency=fee_currency,
                timestamp=_extract_timestamp(raw_fill),
            )
        )
    return fills


def _raw_fills(payload: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    """Return raw fill dictionaries from common ccxt/Binance fields."""
    fills_value = payload.get("trades") or payload.get("fills") or []
    if not isinstance(fills_value, list):
        return []
    return [fill for fill in fills_value if isinstance(fill, Mapping)]


def _merge_fills(existing_fills: list[Fill], incoming_fills: list[Fill]) -> list[Fill]:
    """Merge fills by ID while preserving original order."""
    if not incoming_fills:
        return existing_fills
    merged = list(existing_fills)
    known_ids = {fill.id for fill in existing_fills}
    for fill in incoming_fills:
        if fill.id in known_ids:
            continue
        merged.append(fill)
        known_ids.add(fill.id)
    return merged


def _position_fill_price(
    order: Order,
    *,
    fill_delta: float,
    target_filled_amount: float,
    already_applied: float,
    previous_average_price: float | None,
) -> float:
    """Choose the best available price for a newly applied fill delta."""
    fill_price = _position_fill_price_from_fills(
        order.fills,
        fill_delta=fill_delta,
        target_filled_amount=target_filled_amount,
        already_applied=already_applied,
    )
    if fill_price is not None:
        return fill_price

    if order.avg_fill_price is not None and order.avg_fill_price > 0:
        if (
            previous_average_price is not None
            and previous_average_price > 0
            and already_applied > _EPSILON
        ):
            previous_cost = already_applied * previous_average_price
            target_cost = target_filled_amount * order.avg_fill_price
            delta_cost = target_cost - previous_cost
            if delta_cost > _EPSILON:
                return delta_cost / fill_delta
        return order.avg_fill_price
    if order.price is not None and order.price > 0:
        return order.price
    raise OrderValidationError("filled order is missing a usable fill price")


def _position_fill_price_from_fills(
    fills: list[Fill],
    *,
    fill_delta: float,
    target_filled_amount: float,
    already_applied: float,
) -> float | None:
    """Calculate a weighted delta price from detailed fill records."""
    if not fills:
        return None

    total_fill_amount = sum(fill.amount for fill in fills)
    if total_fill_amount <= _EPSILON:
        return None

    if total_fill_amount <= fill_delta + _EPSILON:
        return sum(fill.amount * fill.price for fill in fills) / total_fill_amount

    if total_fill_amount + _EPSILON < target_filled_amount:
        return None

    remaining_skip = already_applied
    remaining_take = fill_delta
    weighted_cost = 0.0
    weighted_amount = 0.0
    for fill in fills:
        fill_amount = fill.amount
        if remaining_skip > _EPSILON:
            skipped = min(remaining_skip, fill_amount)
            fill_amount -= skipped
            remaining_skip -= skipped
        if fill_amount <= _EPSILON:
            continue

        taken = min(remaining_take, fill_amount)
        weighted_cost += taken * fill.price
        weighted_amount += taken
        remaining_take -= taken
        if remaining_take <= _EPSILON:
            break

    if weighted_amount > _EPSILON:
        return weighted_cost / weighted_amount
    return None


def _nested_info_value(payload: Mapping[str, Any], key: str) -> Any:
    """Return a value from ccxt's raw ``info`` payload if present."""
    info = payload.get("info")
    if isinstance(info, Mapping):
        return info.get(key)
    return None


def _optional_float(value: Any) -> float | None:
    """Coerce a finite float or return None when no value is present."""
    if value is None or value == "":
        return None
    try:
        numeric_value = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(numeric_value):
        return None
    return numeric_value


def _datetime_from_timestamp(value: Any) -> datetime | None:
    """Convert second or millisecond epoch timestamps to naive UTC datetimes."""
    numeric_value = _optional_float(value)
    if numeric_value is None:
        return None
    if numeric_value > 10_000_000_000:
        numeric_value /= 1000
    return datetime.fromtimestamp(numeric_value, tz=timezone.utc).replace(tzinfo=None)


def _utcnow() -> datetime:
    """Return a naive UTC timestamp for compatibility with existing models."""
    return datetime.now(timezone.utc).replace(tzinfo=None)


def _same_sign(left: float, right: float) -> bool:
    """Return whether two non-zero values have the same sign."""
    return (left >= 0 and right >= 0) or (left < 0 and right < 0)


def _snapshot_order(order: Order) -> Order:
    """Return a defensive copy of an order."""
    return copy.deepcopy(order)


def _snapshot_position(position: Position) -> Position:
    """Return a defensive copy of a position."""
    return copy.deepcopy(position)


__all__ = [
    "ACTIVE_ORDER_STATUSES",
    "TERMINAL_ORDER_STATUSES",
    "InvalidOrderTransitionError",
    "OrderManager",
    "OrderManagerError",
    "OrderNotFoundError",
    "OrderStateMachine",
    "OrderValidationError",
]

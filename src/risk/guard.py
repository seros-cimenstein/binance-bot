"""Pre-trade risk checks for Binance order execution."""

from __future__ import annotations

import math
import threading
import time
from collections import deque
from collections.abc import Mapping
from datetime import date, datetime, timezone
from typing import Any, TypeAlias

from loguru import logger
from pydantic import BaseModel, Field

try:
    from pydantic import ConfigDict, field_validator

    _PYDANTIC_V2 = True
except ImportError:  # pragma: no cover - compatibility with Pydantic v1
    ConfigDict = None
    from pydantic import validator as field_validator

    _PYDANTIC_V2 = False


DuplicateOrderKey: TypeAlias = tuple[str, str, float]


def _before_field_validator(*fields: str) -> Any:
    """Return a Pydantic v1/v2 compatible pre-validation decorator."""
    if _PYDANTIC_V2:
        return field_validator(*fields, mode="before")
    return field_validator(*fields, pre=True)


class RiskBreachError(Exception):
    """Raised when an order violates a configured risk limit."""

    def __init__(
        self,
        message: str,
        *,
        context: Mapping[str, Any] | None = None,
    ) -> None:
        """Initialize the risk breach error.

        Args:
            message: Human-readable description of the breached limit.
            context: Structured details about the rejected order or state.
        """
        super().__init__(message)
        self.context = dict(context or {})


class RiskConfig(BaseModel):
    """Validated runtime settings for :class:`RiskGuard`."""

    max_position_size_usd: float = Field(gt=0)
    daily_loss_limit_usd: float = Field(gt=0)
    duplicate_window_seconds: float = Field(default=60.0, gt=0)
    max_total_exposure_usd: float | None = Field(default=None, gt=0)
    daily_drawdown_limit_pct: float | None = Field(default=None, gt=0)
    min_balance_usdt: float = Field(default=0.0, ge=0)
    allowed_leverage: float | None = Field(default=None, gt=0)
    max_open_orders: int | None = Field(default=None, gt=0)
    max_price_deviation_pct: float | None = Field(default=None, ge=0)

    @_before_field_validator(
        "max_position_size_usd",
        "daily_loss_limit_usd",
        "duplicate_window_seconds",
        "max_total_exposure_usd",
        "daily_drawdown_limit_pct",
        "min_balance_usdt",
        "allowed_leverage",
        "max_open_orders",
        "max_price_deviation_pct",
    )
    def numeric_values_must_be_finite(cls, value: Any) -> Any:
        """Validate that risk thresholds are finite numeric values."""
        if value is None:
            return value
        if isinstance(value, bool):
            raise ValueError("risk threshold must be a finite number")
        try:
            numeric_value = float(value)
        except (TypeError, ValueError) as exc:
            raise ValueError("risk threshold must be a finite number") from exc
        if not math.isfinite(numeric_value):
            raise ValueError("risk threshold must be a finite number")
        return value

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class OrderRequest(BaseModel):
    """Pydantic order snapshot used by structured risk validation."""

    symbol: str
    side: str
    amount: float = Field(gt=0)
    price: float = Field(gt=0)
    leverage: float = Field(default=1.0, gt=0)
    order_type: str = "limit"

    @_before_field_validator("amount", "price", "leverage")
    def numeric_values_must_be_finite(cls, value: Any) -> Any:
        """Validate that order numeric values are finite."""
        if isinstance(value, bool):
            raise ValueError("order value must be a finite number")
        try:
            numeric_value = float(value)
        except (TypeError, ValueError) as exc:
            raise ValueError("order value must be a finite number") from exc
        if not math.isfinite(numeric_value):
            raise ValueError("order value must be a finite number")
        return value

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class PortfolioState(BaseModel):
    """Pydantic account snapshot used by structured risk validation."""

    total_equity_usdt: float = Field(gt=0)
    available_balance_usdt: float = Field(ge=0)
    realized_pnl_today_usdt: float = 0.0
    unrealized_pnl_today_usdt: float = 0.0
    positions: dict[str, float] = Field(default_factory=dict)
    total_exposure_usdt: float = Field(default=0.0, ge=0)
    open_orders_count: int = Field(default=0, ge=0)

    @_before_field_validator(
        "total_equity_usdt",
        "available_balance_usdt",
        "realized_pnl_today_usdt",
        "unrealized_pnl_today_usdt",
        "total_exposure_usdt",
        "open_orders_count",
    )
    def numeric_values_must_be_finite(cls, value: Any) -> Any:
        """Validate that portfolio numeric values are finite."""
        if isinstance(value, bool):
            raise ValueError("portfolio value must be a finite number")
        try:
            numeric_value = float(value)
        except (TypeError, ValueError) as exc:
            raise ValueError("portfolio value must be a finite number") from exc
        if not math.isfinite(numeric_value):
            raise ValueError("portfolio value must be a finite number")
        return value

    @_before_field_validator("positions")
    def position_values_must_be_finite(cls, value: Any) -> Any:
        """Validate that position exposure values are finite."""
        if value is None:
            return {}
        if not isinstance(value, Mapping):
            raise ValueError("positions must be a mapping")
        for position_value in value.values():
            if isinstance(position_value, bool):
                raise ValueError("position value must be a finite number")
            try:
                numeric_value = float(position_value)
            except (TypeError, ValueError) as exc:
                raise ValueError("position value must be a finite number") from exc
            if not math.isfinite(numeric_value):
                raise ValueError("position value must be a finite number")
        return value

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class MarketData(BaseModel):
    """Pydantic market snapshot used by structured risk validation."""

    symbol: str
    mark_price: float = Field(gt=0)

    @_before_field_validator("mark_price")
    def numeric_values_must_be_finite(cls, value: Any) -> Any:
        """Validate that market numeric values are finite."""
        if isinstance(value, bool):
            raise ValueError("market value must be a finite number")
        try:
            numeric_value = float(value)
        except (TypeError, ValueError) as exc:
            raise ValueError("market value must be a finite number") from exc
        if not math.isfinite(numeric_value):
            raise ValueError("market value must be a finite number")
        return value

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class RiskGuard:
    """Validate orders against position, loss, and duplicate-order limits.

    The guard is designed to be called immediately before exchange order
    creation. Mutable risk state is protected with a re-entrant lock so it can
    be used safely from concurrent asyncio tasks and worker threads.
    """

    def __init__(
        self,
        config: RiskConfig | Mapping[str, Any] | None = None,
        *,
        db: Any | None = None,
        **config_values: Any,
    ) -> None:
        """Initialize the risk guard.

        Args:
            config: A validated risk configuration or mapping of config values.
            db: Optional :class:`~src.db.OrderDB` for persisting daily loss.
            **config_values: Config values used when ``config`` is omitted.

        Raises:
            ValueError: If both ``config`` and keyword config values are given.
        """
        if config is not None and config_values:
            raise ValueError("pass either config or keyword config values, not both")

        if isinstance(config, RiskConfig):
            self.config = config
        elif config is None:
            self.config = RiskConfig(**config_values)
        else:
            self.config = RiskConfig(**dict(config))

        self._db = db
        self._lock = threading.RLock()
        self._current_daily_loss = 0.0
        self._loss_window_date = self._utc_date()
        self._duplicate_orders: dict[DuplicateOrderKey, float] = {}
        self._duplicate_order_events: deque[tuple[float, DuplicateOrderKey]] = deque()
        self._logger = logger.bind(component="risk_guard")

    def restore_daily_loss_from_db(self) -> None:
        """Reload today's accumulated loss from persistent storage on startup."""
        if self._db is None:
            return
        today = self._utc_date().isoformat()
        loss = self._db.load_daily_loss(today)
        with self._lock:
            self._current_daily_loss = loss
            self._loss_window_date = self._utc_date()
        self._logger.bind(date=today, restored_loss=loss).info(
            "Daily loss restored from database"
        )

    @property
    def current_daily_loss(self) -> float:
        """Return the realized loss accumulated for the active UTC day."""
        with self._lock:
            self._reset_daily_loss_if_needed_locked()
            return self._current_daily_loss

    async def validate_order(
        self,
        symbol: str | OrderRequest | None = None,
        side: str | PortfolioState | None = None,
        amount: float | MarketData | None = None,
        price: float | None = None,
        current_position: float = 0.0,
        *,
        order: OrderRequest | None = None,
        portfolio: PortfolioState | None = None,
        market_data: MarketData | None = None,
        market: MarketData | None = None,
    ) -> None:
        """Validate an order before it is sent to Binance.

        Args:
            symbol: Market symbol in ccxt format, for example ``"BTC/USDT"``,
                or an :class:`OrderRequest` for structured validation.
            side: Order side, expected to be ``"buy"`` or ``"sell"``, or a
                :class:`PortfolioState` when ``symbol`` is an ``OrderRequest``.
            amount: Base asset quantity being ordered, or a ``MarketData`` when
                ``symbol`` is an ``OrderRequest``.
            price: Order price used to calculate notional exposure.
            current_position: Existing base asset position before the order.
            order: Structured order request.
            portfolio: Structured portfolio state.
            market_data: Structured market data.
            market: Alias for ``market_data``.

        Raises:
            RiskBreachError: If any risk check rejects the order.
        """
        if order is not None or isinstance(symbol, OrderRequest):
            structured_order = order or symbol
            structured_portfolio = portfolio or side
            structured_market = market_data or market or amount
            if not isinstance(structured_order, OrderRequest):
                raise TypeError("order must be an OrderRequest")
            if not isinstance(structured_portfolio, PortfolioState):
                raise TypeError("portfolio must be a PortfolioState")
            if not isinstance(structured_market, MarketData):
                raise TypeError("market_data must be a MarketData")
            self._validate_structured_order(
                order=structured_order,
                portfolio=structured_portfolio,
                market_data=structured_market,
            )
            return

        if symbol is None:
            raise TypeError("symbol is required")

        context = self._order_context(
            symbol=symbol,
            side=side,
            amount=amount,
            price=price,
            current_position=current_position,
        )
        self._logger.bind(**context).info("Risk validation started")

        try:
            self._validate_order_inputs(
                symbol=symbol,
                side=side,
                amount=amount,
                price=price,
                current_position=current_position,
            )
            self.check_max_position_size(
                symbol=symbol,
                side=side,
                amount=amount,
                price=price,
                current_position=current_position,
            )
            with self._lock:
                self.check_daily_loss()
                self.check_duplicate_order(symbol=symbol, side=side, amount=amount)
        except RiskBreachError as exc:
            self._logger.bind(**context, risk_context=exc.context).error(
                "Risk validation failed"
            )
            raise

        self._logger.bind(**context).info("Risk validation passed")

    def check_max_position_size(
        self,
        symbol: str,
        amount: float,
        price: float,
        current_position: float,
        side: str = "buy",
    ) -> None:
        """Check whether the resulting position value exceeds the limit.

        Args:
            symbol: Market symbol being evaluated.
            amount: Base asset quantity being ordered.
            price: Order price used to calculate notional exposure.
            current_position: Existing base asset position before the order.
            side: Order side, expected to be ``"buy"`` or ``"sell"``.

        Raises:
            RiskBreachError: If the resulting USD position value is too large.
        """
        side_normalized = side.strip().lower()
        position_delta = amount if side_normalized == "buy" else -amount
        resulting_position = current_position + position_delta
        resulting_value_usd = abs(resulting_position) * price
        context = {
            "symbol": symbol,
            "side": side_normalized,
            "amount": amount,
            "price": price,
            "current_position": current_position,
            "position_delta": position_delta,
            "resulting_position": resulting_position,
            "resulting_value_usd": resulting_value_usd,
            "max_position_size_usd": self.config.max_position_size_usd,
        }

        if resulting_value_usd > self.config.max_position_size_usd:
            self._logger.bind(**context).error("Maximum position size breached")
            raise RiskBreachError(
                (
                    "maximum position size breached: "
                    f"{resulting_value_usd:.8f} USD exceeds "
                    f"{self.config.max_position_size_usd:.8f} USD"
                ),
                context=context,
            )

    def check_daily_loss(self) -> None:
        """Check whether the active UTC day's realized loss limit is exhausted.

        The daily loss counter is reset automatically the first time the guard
        is used after the UTC date changes. This is equivalent to a 00:00 UTC
        reset without requiring a background scheduler.

        Raises:
            RiskBreachError: If the daily loss limit has been reached.
        """
        from pathlib import Path

        if Path("data/go_off.flag").exists():
            return

        with self._lock:
            self._reset_daily_loss_if_needed_locked()
            current_daily_loss = self._current_daily_loss
            context = {
                "current_daily_loss": current_daily_loss,
                "daily_loss_limit_usd": self.config.daily_loss_limit_usd,
                "loss_window_date": self._loss_window_date.isoformat(),
            }

        if current_daily_loss >= self.config.daily_loss_limit_usd:
            self._logger.bind(**context).critical("Daily loss limit breached")
            raise RiskBreachError(
                (
                    "daily loss limit breached: "
                    f"{current_daily_loss:.8f} USD has reached or exceeded "
                    f"{self.config.daily_loss_limit_usd:.8f} USD"
                ),
                context=context,
            )

    def update_realized_pnl(self, pnl: float) -> None:
        """Apply realized PnL from a closed trade to the daily loss counter.

        Only negative PnL increases ``current_daily_loss``; profitable trades do
        not offset previously realized losses.

        Args:
            pnl: Realized profit or loss in USD. Negative values are losses.

        Raises:
            ValueError: If ``pnl`` is not finite.
        """
        if not self._is_finite_number(pnl):
            raise ValueError("pnl must be a finite number")

        with self._lock:
            self._reset_daily_loss_if_needed_locked()
            loss_increment = abs(pnl) if pnl < 0 else 0.0
            self._current_daily_loss += loss_increment
            current_loss = self._current_daily_loss
            loss_date = self._loss_window_date.isoformat()
            context = {
                "pnl": pnl,
                "loss_increment": loss_increment,
                "current_daily_loss": current_loss,
                "daily_loss_limit_usd": self.config.daily_loss_limit_usd,
                "loss_window_date": loss_date,
            }

        if self._db is not None and loss_increment > 0:
            self._db.upsert_daily_loss(loss_date, current_loss)

        if self.current_daily_loss >= self.config.daily_loss_limit_usd:
            self._logger.bind(**context).warning(
                "Realized PnL applied to risk guard"
            )
        else:
            self._logger.bind(**context).info("Realized PnL applied to risk guard")

    def check_duplicate_order(self, symbol: str, side: str, amount: float) -> None:
        """Check and register an order against the duplicate-order window.

        Args:
            symbol: Market symbol being evaluated.
            side: Order side, expected to be ``"buy"`` or ``"sell"``.
            amount: Base asset quantity being ordered.

        Raises:
            RiskBreachError: If an identical order was recently validated.
        """
        now = time.monotonic()
        key = (symbol.strip(), side.strip().lower(), amount)
        with self._lock:
            self._prune_duplicate_orders_locked(now)
            previous_timestamp = self._duplicate_orders.get(key)
            if previous_timestamp is not None:
                elapsed_seconds = now - previous_timestamp
                context = {
                    "symbol": key[0],
                    "side": key[1],
                    "amount": key[2],
                    "elapsed_seconds": elapsed_seconds,
                    "duplicate_window_seconds": (
                        self.config.duplicate_window_seconds
                    ),
                }
                self._logger.bind(**context).error("Duplicate order breached")
                raise RiskBreachError(
                    (
                        "duplicate order breached: identical order submitted "
                        f"{elapsed_seconds:.3f}s ago within "
                        f"{self.config.duplicate_window_seconds:.3f}s window"
                    ),
                    context=context,
                )

            self._duplicate_orders[key] = now
            self._duplicate_order_events.append((now, key))

    def _validate_structured_order(
        self,
        *,
        order: OrderRequest,
        portfolio: PortfolioState,
        market_data: MarketData,
    ) -> None:
        """Validate a structured order against portfolio and market state."""
        context = self._structured_order_context(
            order=order,
            portfolio=portfolio,
            market_data=market_data,
        )
        self._logger.bind(**context).info("Risk validation started")

        try:
            self._validate_market_symbol(order=order, market_data=market_data)
            self._validate_order_inputs(
                symbol=order.symbol,
                side=order.side,
                amount=order.amount,
                price=order.price,
                current_position=(
                    portfolio.positions.get(order.symbol, 0.0) / order.price
                ),
            )
            self._check_position_exposure(order=order, portfolio=portfolio)
            self._check_daily_drawdown(portfolio=portfolio)
            self._check_minimum_balance(order=order, portfolio=portfolio)
            self._check_leverage(order=order)
            self._check_max_open_orders(portfolio=portfolio)
            self._check_price_deviation(order=order, market_data=market_data)
            with self._lock:
                self.check_daily_loss()
                self.check_duplicate_order(
                    symbol=order.symbol,
                    side=order.side,
                    amount=order.amount,
                )
        except RiskBreachError as exc:
            self._logger.bind(**context, risk_context=exc.context).error(
                "Risk validation failed"
            )
            raise

        self._logger.bind(**context).info("Risk validation passed")

    def _validate_market_symbol(
        self,
        *,
        order: OrderRequest,
        market_data: MarketData,
    ) -> None:
        """Ensure the market data belongs to the order's symbol."""
        order_symbol = order.symbol.strip()
        market_symbol = market_data.symbol.strip()
        if order_symbol == market_symbol:
            return

        context = {"order_symbol": order_symbol, "market_symbol": market_symbol}
        self._logger.bind(**context).error("Market data symbol mismatch")
        raise RiskBreachError(
            "market data symbol mismatch",
            context=context,
        )

    def _check_position_exposure(
        self,
        *,
        order: OrderRequest,
        portfolio: PortfolioState,
    ) -> None:
        """Check per-symbol and total USD exposure limits."""
        order_notional_usdt = self._order_notional(order)
        current_symbol_exposure_usdt = abs(
            portfolio.positions.get(order.symbol, 0.0)
        )
        resulting_symbol_exposure_usdt = self._resulting_symbol_exposure(
            order=order,
            current_symbol_exposure_usdt=portfolio.positions.get(order.symbol, 0.0),
        )
        context = {
            "symbol": order.symbol,
            "side": order.side,
            "order_notional_usdt": order_notional_usdt,
            "current_symbol_exposure_usdt": current_symbol_exposure_usdt,
            "resulting_symbol_exposure_usdt": resulting_symbol_exposure_usdt,
            "max_position_size_usd": self.config.max_position_size_usd,
        }

        if resulting_symbol_exposure_usdt > self.config.max_position_size_usd:
            self._logger.bind(**context).error("Maximum position size breached")
            raise RiskBreachError(
                (
                    "maximum position size breached: "
                    f"{resulting_symbol_exposure_usdt:.8f} USD exceeds "
                    f"{self.config.max_position_size_usd:.8f} USD"
                ),
                context=context,
            )

        if self.config.max_total_exposure_usd is None:
            return

        current_total_exposure_usdt = self._current_total_exposure(portfolio)
        resulting_total_exposure_usdt = (
            current_total_exposure_usdt
            - current_symbol_exposure_usdt
            + resulting_symbol_exposure_usdt
        )
        context = {
            **context,
            "current_total_exposure_usdt": current_total_exposure_usdt,
            "resulting_total_exposure_usdt": resulting_total_exposure_usdt,
            "max_total_exposure_usd": self.config.max_total_exposure_usd,
        }

        if resulting_total_exposure_usdt > self.config.max_total_exposure_usd:
            self._logger.bind(**context).error("Maximum total exposure breached")
            raise RiskBreachError(
                (
                    "maximum total exposure breached: "
                    f"{resulting_total_exposure_usdt:.8f} USD exceeds "
                    f"{self.config.max_total_exposure_usd:.8f} USD"
                ),
                context=context,
            )

    def _check_daily_drawdown(self, *, portfolio: PortfolioState) -> None:
        """Check realized plus unrealized daily loss percentage."""
        if self.config.daily_drawdown_limit_pct is None:
            return

        daily_pnl_usdt = (
            portfolio.realized_pnl_today_usdt
            + portfolio.unrealized_pnl_today_usdt
        )
        daily_loss_usdt = abs(daily_pnl_usdt) if daily_pnl_usdt < 0 else 0.0
        daily_drawdown_pct = (
            daily_loss_usdt / portfolio.total_equity_usdt
        ) * 100.0
        context = {
            "daily_pnl_usdt": daily_pnl_usdt,
            "daily_loss_usdt": daily_loss_usdt,
            "total_equity_usdt": portfolio.total_equity_usdt,
            "daily_drawdown_pct": daily_drawdown_pct,
            "daily_drawdown_limit_pct": self.config.daily_drawdown_limit_pct,
        }

        if daily_drawdown_pct > self.config.daily_drawdown_limit_pct:
            self._logger.bind(**context).critical("Daily drawdown breached")
            raise RiskBreachError(
                (
                    "daily drawdown breached: "
                    f"{daily_drawdown_pct:.8f}% exceeds "
                    f"{self.config.daily_drawdown_limit_pct:.8f}%"
                ),
                context=context,
            )

    def _check_minimum_balance(
        self,
        *,
        order: OrderRequest,
        portfolio: PortfolioState,
    ) -> None:
        """Ensure the order leaves the configured USDT balance buffer."""
        order_margin_usdt = self._order_notional(order) / order.leverage
        remaining_balance_usdt = (
            portfolio.available_balance_usdt - order_margin_usdt
        )
        context = {
            "available_balance_usdt": portfolio.available_balance_usdt,
            "order_margin_usdt": order_margin_usdt,
            "remaining_balance_usdt": remaining_balance_usdt,
            "min_balance_usdt": self.config.min_balance_usdt,
        }

        if remaining_balance_usdt < self.config.min_balance_usdt:
            self._logger.bind(**context).error("Minimum balance breached")
            raise RiskBreachError(
                (
                    "minimum balance breached: "
                    f"{remaining_balance_usdt:.8f} USDT would be below "
                    f"{self.config.min_balance_usdt:.8f} USDT"
                ),
                context=context,
            )

    def _check_leverage(self, *, order: OrderRequest) -> None:
        """Ensure the requested leverage matches the configured value."""
        if self.config.allowed_leverage is None:
            return

        context = {
            "requested_leverage": order.leverage,
            "allowed_leverage": self.config.allowed_leverage,
        }
        if math.isclose(
            order.leverage,
            self.config.allowed_leverage,
            rel_tol=0.0,
            abs_tol=1e-9,
        ):
            return

        self._logger.bind(**context).error("Leverage check breached")
        raise RiskBreachError(
            (
                "leverage check breached: "
                f"{order.leverage:.8f} does not match "
                f"{self.config.allowed_leverage:.8f}"
            ),
            context=context,
        )

    def _check_max_open_orders(self, *, portfolio: PortfolioState) -> None:
        """Ensure submitting the order does not exceed open order limits."""
        if self.config.max_open_orders is None:
            return

        resulting_open_orders = portfolio.open_orders_count + 1
        context = {
            "open_orders_count": portfolio.open_orders_count,
            "resulting_open_orders": resulting_open_orders,
            "max_open_orders": self.config.max_open_orders,
        }

        if resulting_open_orders > self.config.max_open_orders:
            self._logger.bind(**context).error("Maximum open orders breached")
            raise RiskBreachError(
                (
                    "maximum open orders breached: "
                    f"{resulting_open_orders} exceeds "
                    f"{self.config.max_open_orders}"
                ),
                context=context,
            )

    def _check_price_deviation(
        self,
        *,
        order: OrderRequest,
        market_data: MarketData,
    ) -> None:
        """Ensure a limit price is close enough to the mark price."""
        if self.config.max_price_deviation_pct is None:
            return

        price_deviation_pct = (
            abs(order.price - market_data.mark_price) / market_data.mark_price
        ) * 100.0
        context = {
            "symbol": order.symbol,
            "limit_price": order.price,
            "mark_price": market_data.mark_price,
            "price_deviation_pct": price_deviation_pct,
            "max_price_deviation_pct": self.config.max_price_deviation_pct,
        }

        if price_deviation_pct > self.config.max_price_deviation_pct:
            self._logger.bind(**context).error("Price deviation breached")
            raise RiskBreachError(
                (
                    "price deviation breached: "
                    f"{price_deviation_pct:.8f}% exceeds "
                    f"{self.config.max_price_deviation_pct:.8f}%"
                ),
                context=context,
            )

    def _validate_order_inputs(
        self,
        *,
        symbol: str,
        side: str,
        amount: float,
        price: float,
        current_position: float,
    ) -> None:
        """Validate basic order inputs before applying risk thresholds."""
        symbol_normalized = symbol.strip() if isinstance(symbol, str) else ""
        side_normalized = side.strip().lower() if isinstance(side, str) else ""
        context = {
            "symbol": symbol,
            "side": side,
            "amount": amount,
            "price": price,
            "current_position": current_position,
        }

        if not symbol_normalized:
            raise RiskBreachError("symbol must not be blank", context=context)
        if side_normalized not in {"buy", "sell"}:
            raise RiskBreachError(
                "side must be 'buy' or 'sell'",
                context=context,
            )
        if not self._is_finite_number(amount) or amount <= 0:
            raise RiskBreachError(
                "amount must be a finite value greater than zero",
                context=context,
            )
        if not self._is_finite_number(price) or price <= 0:
            raise RiskBreachError(
                "price must be a finite value greater than zero",
                context=context,
            )
        if not self._is_finite_number(current_position):
            raise RiskBreachError(
                "current_position must be a finite number",
                context=context,
            )

    def _prune_duplicate_orders_locked(self, now: float) -> None:
        """Drop duplicate-order entries older than the configured window."""
        cutoff = now - self.config.duplicate_window_seconds
        while self._duplicate_order_events:
            timestamp, key = self._duplicate_order_events[0]
            if timestamp > cutoff:
                break

            self._duplicate_order_events.popleft()
            if self._duplicate_orders.get(key) == timestamp:
                del self._duplicate_orders[key]

    def _reset_daily_loss_if_needed_locked(self) -> None:
        """Reset daily loss state when the UTC date has changed."""
        today = self._utc_date()
        if today == self._loss_window_date:
            return

        previous_date = self._loss_window_date
        previous_loss = self._current_daily_loss
        self._current_daily_loss = 0.0
        self._loss_window_date = today
        self._logger.bind(
            previous_loss_window_date=previous_date.isoformat(),
            loss_window_date=today.isoformat(),
            previous_daily_loss=previous_loss,
        ).info("Daily loss counter reset")

    @staticmethod
    def _order_notional(order: OrderRequest) -> float:
        """Return the order's notional value in USDT."""
        return order.amount * order.price

    @staticmethod
    def _resulting_symbol_exposure(
        *,
        order: OrderRequest,
        current_symbol_exposure_usdt: float,
    ) -> float:
        """Return signed-position exposure after applying the order."""
        order_notional_usdt = order.amount * order.price
        side = order.side.strip().lower()
        if side == "buy":
            resulting_exposure_usdt = (
                current_symbol_exposure_usdt + order_notional_usdt
            )
        else:
            resulting_exposure_usdt = (
                current_symbol_exposure_usdt - order_notional_usdt
            )
        return abs(resulting_exposure_usdt)

    @staticmethod
    def _current_total_exposure(portfolio: PortfolioState) -> float:
        """Return the larger explicit or position-derived exposure total."""
        position_total = sum(abs(value) for value in portfolio.positions.values())
        return max(portfolio.total_exposure_usdt, position_total)

    @staticmethod
    def _is_finite_number(value: Any) -> bool:
        """Return whether a value is a finite int or float, excluding bools."""
        return (
            isinstance(value, (int, float))
            and not isinstance(value, bool)
            and math.isfinite(value)
        )

    @staticmethod
    def _utc_date() -> date:
        """Return the current UTC calendar date."""
        return datetime.now(timezone.utc).date()

    @staticmethod
    def _order_context(
        *,
        symbol: str,
        side: str,
        amount: float,
        price: float,
        current_position: float,
    ) -> dict[str, Any]:
        """Build log context for an order validation attempt."""
        return {
            "symbol": symbol,
            "side": side,
            "amount": amount,
            "price": price,
            "current_position": current_position,
        }

    @staticmethod
    def _structured_order_context(
        *,
        order: OrderRequest,
        portfolio: PortfolioState,
        market_data: MarketData,
    ) -> dict[str, Any]:
        """Build log context for structured validation."""
        return {
            "order": RiskGuard._model_dump(order),
            "portfolio": RiskGuard._model_dump(portfolio),
            "market_data": RiskGuard._model_dump(market_data),
        }

    @staticmethod
    def _model_dump(model: BaseModel) -> dict[str, Any]:
        """Return a Pydantic v1/v2 compatible model dictionary."""
        if hasattr(model, "model_dump"):
            return model.model_dump()
        return model.dict()


__all__ = [
    "MarketData",
    "OrderRequest",
    "PortfolioState",
    "RiskBreachError",
    "RiskConfig",
    "RiskGuard",
]

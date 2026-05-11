"""Minimal 20/50 Simple Moving Average crossover strategy.

The strategy is intentionally small so it can serve as a reference
implementation of the base strategy lifecycle:

* ``start`` prepares local state.
* ``on_tick`` consumes normalized market data.
* ``on_fill`` handles private execution updates.
* ``on_error`` handles runtime failures.
* ``stop`` disables further tick processing.
"""

from __future__ import annotations

import math
from collections import deque
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Literal, Protocol

from loguru import logger
from pydantic import BaseModel, Field

from src.data.feeds import normalize_symbol
from src.strategies.base import BaseStrategy, FillData, TickData

try:
    from pydantic import ConfigDict, field_validator, model_validator

    _PYDANTIC_V2 = True
except ImportError:  # pragma: no cover - compatibility with Pydantic v1
    ConfigDict = None
    from pydantic import root_validator, validator as field_validator

    model_validator = None
    _PYDANTIC_V2 = False


OrderSide = Literal["buy", "sell"]
Relation = Literal[-1, 0, 1]


class OrderClient(Protocol):
    """Client capability used when the strategy executes generated signals."""

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> Mapping[str, Any]:
        """Submit an order to the exchange."""


def _before_field_validator(*fields: str) -> Any:
    """Return a Pydantic v1/v2 compatible pre-validation decorator."""
    if _PYDANTIC_V2:
        return field_validator(*fields, mode="before")
    return field_validator(*fields, pre=True)


class SMASettings(BaseModel):
    """Runtime configuration for :class:`DemoSMAStrategy`."""

    symbol: str = Field(..., min_length=1)
    fast_period: int = Field(default=20, gt=0)
    slow_period: int = Field(default=50, gt=0)
    order_size: float = Field(default=0.0, ge=0)
    timeframe: str = Field(default="1h", min_length=1)

    @_before_field_validator("symbol")
    def normalize_config_symbol(cls, value: Any) -> Any:
        """Normalize symbols to ccxt's ``BASE/QUOTE`` form."""
        if isinstance(value, str):
            return normalize_symbol(value)
        return value

    @_before_field_validator("timeframe")
    def normalize_timeframe(cls, value: Any) -> Any:
        """Strip surrounding whitespace from timeframe strings."""
        if isinstance(value, str):
            return value.strip()
        return value

    if _PYDANTIC_V2:

        @model_validator(mode="after")
        def periods_must_be_ordered(self) -> "SMASettings":
            """Ensure the fast SMA is actually faster than the slow SMA."""
            if self.fast_period >= self.slow_period:
                raise ValueError("fast_period must be less than slow_period")
            return self

    else:

        @root_validator
        def periods_must_be_ordered(cls, values: dict[str, Any]) -> dict[str, Any]:
            """Ensure the fast SMA is actually faster than the slow SMA."""
            fast_period = values.get("fast_period")
            slow_period = values.get("slow_period")
            if (
                fast_period is not None
                and slow_period is not None
                and fast_period >= slow_period
            ):
                raise ValueError("fast_period must be less than slow_period")
            return values

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


@dataclass(frozen=True, slots=True)
class SMASnapshot:
    """SMA state calculated from the latest accepted market update."""

    symbol: str
    timestamp: int
    price: float
    fast_sma: float
    slow_sma: float
    relation: Relation
    signal: OrderSide | None = None


class DemoSMAStrategy(
    BaseStrategy[OrderClient, SMASettings],
    plugin_name="demo-sma",
):
    """Reference SMA crossover strategy using 20/50 periods by default.

    A buy signal is generated when the fast SMA crosses from below/equal to
    above the slow SMA. A sell signal is generated when the fast SMA crosses
    from above/equal to below the slow SMA.
    """

    def __init__(
        self,
        client: OrderClient,
        config: SMASettings | Mapping[str, Any],
    ) -> None:
        """Initialize the strategy with a client and validated settings."""
        settings = _validate_settings(config)
        super().__init__(client, settings)

        self._prices: deque[float] = deque(maxlen=settings.slow_period)
        self._previous_relation: Relation | None = None
        self.last_snapshot: SMASnapshot | None = None
        self.last_signal: OrderSide | None = None
        self._logger = logger.bind(strategy=self.name, symbol=settings.symbol)

    @property
    def settings(self) -> SMASettings:
        """Return this strategy's typed settings."""
        return self.config

    @property
    def sample_count(self) -> int:
        """Return the number of prices currently loaded in the SMA window."""
        return len(self._prices)

    @property
    def required_samples(self) -> int:
        """Return the number of prices needed before SMA signals can form."""
        return self.settings.slow_period

    async def start(self) -> None:
        """Prepare the strategy for a new stream of market data."""
        if self.is_running:
            await super().start()
            return

        self._prices.clear()
        self._previous_relation = None
        self.last_snapshot = None
        self.last_signal = None
        await super().start()

    async def on_tick(self, tick: TickData) -> None:
        """Consume one normalized market tick and generate crossover signals."""
        if not self.is_running:
            self._logger.bind(tick_timestamp=tick.timestamp).debug(
                "Ignoring tick while strategy is stopped"
            )
            return
        if not _symbols_match(tick.symbol, self.settings.symbol):
            self._logger.bind(tick_symbol=tick.symbol).debug(
                "Ignoring tick for different symbol"
            )
            return

        self._prices.append(tick.last_price)
        if len(self._prices) < self.settings.slow_period:
            self._logger.bind(
                samples=len(self._prices),
                required_samples=self.settings.slow_period,
            ).debug("SMA window warming")
            return

        fast_sma = _sma(tuple(self._prices), self.settings.fast_period)
        slow_sma = _sma(tuple(self._prices), self.settings.slow_period)
        current_relation = _relation(fast_sma, slow_sma)
        signal = _signal_from_cross(self._previous_relation, current_relation)

        self._previous_relation = current_relation
        self.last_snapshot = SMASnapshot(
            symbol=self.settings.symbol,
            timestamp=tick.timestamp,
            price=tick.last_price,
            fast_sma=fast_sma,
            slow_sma=slow_sma,
            relation=current_relation,
            signal=signal,
        )

        self._logger.bind(
            tick_timestamp=tick.timestamp,
            price=tick.last_price,
            fast_sma=fast_sma,
            slow_sma=slow_sma,
            relation=current_relation,
            signal=signal,
        ).info("SMA indicators updated")

        if signal is None:
            return

        self.last_signal = signal
        await self._execute_signal(signal)

    async def on_candle(self, candle: Mapping[str, Any]) -> None:
        """Adapt OHLCV candle dictionaries into ``TickData`` close updates."""
        if not _candle_is_closed(candle):
            self._logger.debug("Ignoring open candle")
            return

        close = _required_float(candle, "close", "c")
        await self.on_tick(
            TickData(
                timestamp=_required_int(candle, "timestamp", "time", "t"),
                symbol=str(
                    candle.get("symbol")
                    or candle.get("s")
                    or self.settings.symbol
                ),
                bid=close,
                ask=close,
                last_price=close,
                volume=_float_or_default(candle, 0.0, "volume", "v"),
            )
        )

    async def on_fill(self, fill: FillData) -> None:
        """Handle a private order fill update."""
        self._logger.bind(
            order_id=fill.order_id,
            side=fill.side,
            price=fill.price,
            quantity=fill.quantity,
            timestamp=fill.timestamp,
        ).info("SMA strategy fill received")

    async def on_error(self, error: Exception) -> None:
        """Handle a strategy-level runtime error."""
        self._logger.bind(
            error_type=error.__class__.__name__,
            error_message=str(error),
        ).opt(exception=error).error("SMA strategy error")

    async def _execute_signal(self, side: OrderSide) -> None:
        """Submit a market order for the generated signal when configured."""
        if self.settings.order_size == 0:
            self._logger.bind(signal=side).info(
                "SMA signal generated; order_size is zero"
            )
            return

        self._logger.bind(
            signal=side,
            order_size=self.settings.order_size,
        ).info("SMA crossover signal generated; submitting market order")

        try:
            result = await self.client.create_order(
                symbol=self.settings.symbol,
                order_type="market",
                side=side,
                amount=self.settings.order_size,
            )
            self._logger.bind(
                signal=side,
                order_id=result.get("id"),
                status=result.get("status"),
                filled=result.get("filled"),
                average=result.get("average"),
            ).info("Market order placed")
        except Exception as exc:
            await self.on_error(exc)


def _validate_settings(config: SMASettings | Mapping[str, Any]) -> SMASettings:
    """Validate user-supplied SMA settings for Pydantic v1/v2."""
    if isinstance(config, SMASettings):
        return config
    if hasattr(SMASettings, "model_validate"):
        return SMASettings.model_validate(config)
    return SMASettings.parse_obj(config)


def _signal_from_cross(
    previous_relation: Relation | None,
    current_relation: Relation,
) -> OrderSide | None:
    """Return the side generated by a fast/slow SMA crossover."""
    if previous_relation is None:
        return None
    if previous_relation <= 0 and current_relation > 0:
        return "buy"
    if previous_relation >= 0 and current_relation < 0:
        return "sell"
    return None


def _sma(values: Sequence[float], period: int) -> float:
    """Return the arithmetic mean of the last ``period`` values."""
    if len(values) < period:
        raise ValueError("not enough values to calculate SMA")
    return sum(values[-period:]) / period


def _relation(left: float, right: float) -> Relation:
    """Return the sign of ``left - right`` as -1, 0, or 1."""
    if math.isclose(left, right, rel_tol=1e-12, abs_tol=1e-12):
        return 0
    return 1 if left > right else -1


def _symbols_match(left: str, right: str) -> bool:
    """Return whether two common Binance symbol spellings name the same market."""
    return _compact_symbol(left) == _compact_symbol(right)


def _compact_symbol(symbol: str) -> str:
    """Return a punctuation-free uppercase market symbol."""
    return "".join(character for character in symbol.upper() if character.isalnum())


def _candle_is_closed(candle: Mapping[str, Any]) -> bool:
    """Return whether an OHLCV payload represents a closed candle."""
    value = candle.get("closed", candle.get("is_closed", candle.get("x", True)))
    if isinstance(value, str):
        return value.strip().lower() in {"1", "true", "t", "yes", "y"}
    return bool(value)


def _required_float(candle: Mapping[str, Any], *keys: str) -> float:
    """Read the first present candle field as a positive float."""
    value = _first_present(candle, *keys)
    try:
        result = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{'/'.join(keys)} is not numeric: {value!r}") from exc
    if not math.isfinite(result) or result <= 0:
        raise ValueError(f"{'/'.join(keys)} must be a positive finite number")
    return result


def _float_or_default(
    candle: Mapping[str, Any],
    default: float,
    *keys: str,
) -> float:
    """Read an optional candle field as a non-negative float."""
    for key in keys:
        value = candle.get(key)
        if value is None:
            continue
        try:
            result = float(value)
        except (TypeError, ValueError) as exc:
            raise ValueError(f"{key} is not numeric: {value!r}") from exc
        if not math.isfinite(result) or result < 0:
            raise ValueError(f"{key} must be a non-negative finite number")
        return result
    return default


def _required_int(candle: Mapping[str, Any], *keys: str) -> int:
    """Read the first present candle field as a non-negative integer."""
    value = _first_present(candle, *keys)
    try:
        result = int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{'/'.join(keys)} is not an integer: {value!r}") from exc
    if result < 0:
        raise ValueError(f"{'/'.join(keys)} must be non-negative")
    return result


def _first_present(candle: Mapping[str, Any], *keys: str) -> Any:
    """Return the first non-``None`` value for the supplied keys."""
    for key in keys:
        value = candle.get(key)
        if value is not None:
            return value
    raise ValueError(f"missing required field: {'/'.join(keys)}")


__all__ = ["DemoSMAStrategy", "OrderClient", "SMASettings", "SMASnapshot"]

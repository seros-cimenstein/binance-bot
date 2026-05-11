"""Base interfaces for plugin-style trading strategies."""

from __future__ import annotations

import inspect
from abc import ABC, abstractmethod
from collections.abc import Callable, Mapping
from types import MappingProxyType
from typing import Any, ClassVar, Generic, Literal, TypeVar, cast, overload

from loguru import logger
from pydantic import BaseModel, Field

try:
    from pydantic import ConfigDict, field_validator

    _PYDANTIC_V2 = True
except ImportError:  # pragma: no cover - compatibility with Pydantic v1
    ConfigDict = None
    from pydantic import validator as field_validator

    _PYDANTIC_V2 = False


_ClientT = TypeVar("_ClientT")
_ConfigT = TypeVar("_ConfigT", bound=BaseModel)
_StrategyT = TypeVar("_StrategyT", bound="Strategy[Any, BaseModel]")
_RegisteredStrategy = type["Strategy[Any, BaseModel]"]


def _before_field_validator(field: str) -> Any:
    """Return a Pydantic v1/v2 compatible pre-validation decorator."""
    if _PYDANTIC_V2:
        return field_validator(field, mode="before")
    return field_validator(field, pre=True)


class TickData(BaseModel):
    """Normalized market tick data passed to strategies."""

    timestamp: int = Field(ge=0)
    symbol: str = Field(min_length=1)
    bid: float = Field(gt=0)
    ask: float = Field(gt=0)
    last_price: float = Field(gt=0)
    volume: float = Field(ge=0)

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class FillData(BaseModel):
    """Normalized order fill data passed to strategies."""

    order_id: str | int
    symbol: str = Field(min_length=1)
    side: Literal["buy", "sell"]
    price: float = Field(gt=0)
    quantity: float = Field(gt=0)
    commission: float = Field(ge=0)
    timestamp: int = Field(ge=0)

    @_before_field_validator("side")
    def side_must_be_lowercase(cls, value: Any) -> Any:
        """Normalize side names from exchange payloads."""
        if isinstance(value, str):
            return value.strip().lower()
        return value

    if ConfigDict is not None:
        model_config = ConfigDict(extra="forbid", validate_assignment=True)
    else:

        class Config:
            """Pydantic model configuration."""

            extra = "forbid"
            validate_assignment = True


class Strategy(ABC, Generic[_ClientT, _ConfigT]):
    """Abstract base class for all trading strategy plugins.

    Concrete strategies receive a Binance client-like object and their own
    validated Pydantic configuration. Subclasses are automatically registered
    as plugins once they implement the abstract event hooks.
    """

    plugin_name: ClassVar[str] = "strategy"
    registry: ClassVar[dict[str, _RegisteredStrategy]] = {}

    def __init_subclass__(
        cls,
        *,
        plugin_name: str | None = None,
        name: str | None = None,
        register: bool = True,
        **kwargs: Any,
    ) -> None:
        """Register concrete subclasses for plugin discovery."""
        super().__init_subclass__(**kwargs)

        resolved_name = _resolve_plugin_name(cls, plugin_name, name)
        cls.plugin_name = resolved_name
        if register and not inspect.isabstract(cls):
            Strategy.register_plugin(cls, plugin_name=resolved_name)

    def __init__(self, client: _ClientT, config: _ConfigT) -> None:
        """Initialize a strategy with an exchange client and configuration."""
        self._client: _ClientT = client
        self.config = config
        self._is_running: bool = False
        self._logger = logger.bind(strategy=self.name)

    @property
    def name(self) -> str:
        """Return the strategy name used for logs and plugin discovery."""
        return type(self).plugin_name

    @property
    def client(self) -> _ClientT:
        """Return the exchange client assigned to this strategy."""
        return self._client

    @property
    def is_running(self) -> bool:
        """Return whether the strategy lifecycle is active."""
        return self._is_running

    async def start(self) -> None:
        """Run setup logic before ticks and fills are dispatched."""
        if self._is_running:
            self._logger.debug("Strategy start skipped; already running")
            return

        self._is_running = True
        self._logger.info("Strategy started")

    async def stop(self) -> None:
        """Run graceful teardown logic for the strategy."""
        if not self._is_running:
            self._logger.debug("Strategy stop skipped; already stopped")
            return

        self._is_running = False
        self._logger.info("Strategy stopped")

    @abstractmethod
    async def on_tick(self, tick: TickData) -> None:
        """Handle a normalized market price update."""

    @abstractmethod
    async def on_fill(self, fill: FillData) -> None:
        """Handle a private order execution event."""

    @abstractmethod
    async def on_error(self, error: Exception) -> None:
        """Handle a strategy-specific runtime error."""

    @classmethod
    @overload
    def register_plugin(
        cls,
        strategy_type: type[_StrategyT],
        *,
        plugin_name: str | None = None,
        name: str | None = None,
    ) -> type[_StrategyT]: ...

    @classmethod
    @overload
    def register_plugin(
        cls,
        strategy_type: None = None,
        *,
        plugin_name: str | None = None,
        name: str | None = None,
    ) -> Callable[[type[_StrategyT]], type[_StrategyT]]: ...

    @classmethod
    def register_plugin(
        cls,
        strategy_type: type[_StrategyT] | None = None,
        *,
        plugin_name: str | None = None,
        name: str | None = None,
    ) -> type[_StrategyT] | Callable[[type[_StrategyT]], type[_StrategyT]]:
        """Register a concrete strategy class and return it unchanged."""

        def register(strategy_candidate: type[_StrategyT]) -> type[_StrategyT]:
            if inspect.isabstract(strategy_candidate):
                raise TypeError("abstract strategy plugins cannot be registered")

            resolved_name = _resolve_plugin_name(strategy_candidate, plugin_name, name)
            existing_strategy = Strategy.registry.get(resolved_name)
            if (
                existing_strategy is not None
                and existing_strategy is not strategy_candidate
            ):
                raise ValueError(
                    f"strategy plugin already registered: {resolved_name!r}"
                )

            strategy_candidate.plugin_name = resolved_name
            Strategy.registry[resolved_name] = cast(
                _RegisteredStrategy,
                strategy_candidate,
            )
            return strategy_candidate

        if strategy_type is None:
            return register
        return register(strategy_type)

    @classmethod
    def registered_plugins(cls) -> Mapping[str, _RegisteredStrategy]:
        """Return a read-only snapshot of registered strategy plugins."""
        return MappingProxyType(dict(Strategy.registry))

    @classmethod
    def get_plugin(cls, plugin_name: str) -> _RegisteredStrategy:
        """Return the strategy class registered under ``plugin_name``."""
        resolved_name = _normalize_plugin_name(plugin_name)
        try:
            return Strategy.registry[resolved_name]
        except KeyError as exc:
            raise KeyError(
                f"strategy plugin is not registered: {resolved_name!r}"
            ) from exc

    @classmethod
    def create(
        cls,
        plugin_name: str,
        client: _ClientT,
        config: _ConfigT,
    ) -> "Strategy[_ClientT, _ConfigT]":
        """Instantiate a registered strategy plugin."""
        strategy_type = cls.get_plugin(plugin_name)
        return cast("Strategy[_ClientT, _ConfigT]", strategy_type(client, config))


def _resolve_plugin_name(
    strategy_type: type[Strategy[Any, BaseModel]],
    plugin_name: str | None,
    name: str | None,
) -> str:
    """Resolve the explicit or class-defined plugin name."""
    if plugin_name is not None and name is not None and plugin_name != name:
        raise ValueError("plugin_name and name must match when both are provided")

    raw_name = plugin_name if plugin_name is not None else name
    if raw_name is None:
        raw_name = strategy_type.__dict__.get("plugin_name", strategy_type.__name__)
    if not isinstance(raw_name, str):
        raise TypeError("strategy plugin name must be a string")
    return _normalize_plugin_name(raw_name)


def _normalize_plugin_name(plugin_name: str) -> str:
    """Validate and normalize a strategy plugin name."""
    normalized_name = plugin_name.strip()
    if not normalized_name:
        raise ValueError("strategy plugin name must not be blank")
    return normalized_name


class BaseStrategy(Strategy[_ClientT, _ConfigT], register=False):
    """Backward-compatible alias for the strategy plugin base class."""


__all__ = ["BaseStrategy", "FillData", "Strategy", "TickData"]

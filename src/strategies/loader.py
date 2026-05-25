"""Runtime strategy loading through the strategy plugin registry."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from pydantic import BaseModel

from src.config import Settings
from src.strategies.base import Strategy
from src.strategies.demo_sma import SMASettings


StrategyConfigBuilder = Callable[[Settings], BaseModel]


_CONFIG_BUILDERS: dict[str, StrategyConfigBuilder] = {}


def register_strategy_config(
    strategy_name: str,
    builder: StrategyConfigBuilder,
) -> None:
    """Register the settings adapter for a strategy plugin."""
    normalized_name = _normalize_strategy_name(strategy_name)
    _CONFIG_BUILDERS[normalized_name] = builder


def unregister_strategy_config(strategy_name: str) -> None:
    """Remove a strategy settings adapter if it exists."""
    _CONFIG_BUILDERS.pop(_normalize_strategy_name(strategy_name), None)


def create_strategy(
    strategy_name: str,
    client: Any,
    settings: Settings,
) -> Strategy[Any, BaseModel]:
    """Create a configured strategy instance from the plugin registry."""
    normalized_name = _normalize_strategy_name(strategy_name)
    Strategy.get_plugin(normalized_name)
    config_builder = _CONFIG_BUILDERS.get(normalized_name)
    if config_builder is None:
        raise ValueError(
            f"strategy config builder is not registered: {normalized_name!r}"
        )
    return Strategy.create(normalized_name, client, config_builder(settings))


def build_demo_sma_config(settings: Settings) -> SMASettings:
    """Build demo SMA strategy settings from runtime settings."""
    return SMASettings(
        symbol=settings.trading_symbol,
        fast_period=settings.fast_period,
        slow_period=settings.slow_period,
        timeframe=settings.timeframe,
        order_size=settings.order_size,
    )


def _normalize_strategy_name(strategy_name: str) -> str:
    normalized_name = strategy_name.strip()
    if not normalized_name:
        raise ValueError("strategy name must not be blank")
    return normalized_name


register_strategy_config("demo-sma", build_demo_sma_config)


__all__ = [
    "build_demo_sma_config",
    "create_strategy",
    "register_strategy_config",
    "unregister_strategy_config",
]

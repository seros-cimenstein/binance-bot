import asyncio
import inspect

import pytest
from pydantic import BaseModel

from src.strategies.base import FillData, Strategy, TickData


class DummyConfig(BaseModel):
    symbol: str = "BTC/USDT"


def test_strategy_is_abstract() -> None:
    assert inspect.isabstract(Strategy)


def test_strategy_auto_registers_and_factory_creates_plugin() -> None:
    plugin_name = "test-auto-plugin"

    class ExampleStrategy(Strategy[object, DummyConfig], plugin_name=plugin_name):
        async def on_tick(self, tick: TickData) -> None:
            return None

        async def on_fill(self, fill: FillData) -> None:
            return None

        async def on_error(self, error: Exception) -> None:
            return None

    try:
        assert Strategy.get_plugin(plugin_name) is ExampleStrategy
        assert Strategy.registered_plugins()[plugin_name] is ExampleStrategy

        config = DummyConfig()
        client = object()
        strategy = Strategy.create(plugin_name, client, config)

        assert isinstance(strategy, ExampleStrategy)
        assert strategy.name == plugin_name
        assert strategy.client is client
        assert strategy.config is config
        assert not strategy.is_running

        asyncio.run(strategy.start())
        assert strategy.is_running
        asyncio.run(strategy.stop())
        assert not strategy.is_running
    finally:
        Strategy.registry.pop(plugin_name, None)


def test_strategy_duplicate_plugin_names_are_rejected() -> None:
    plugin_name = "test-duplicate-plugin"

    class FirstStrategy(Strategy[object, DummyConfig], plugin_name=plugin_name):
        async def on_tick(self, tick: TickData) -> None:
            return None

        async def on_fill(self, fill: FillData) -> None:
            return None

        async def on_error(self, error: Exception) -> None:
            return None

    try:
        with pytest.raises(ValueError, match="already registered"):

            class DuplicateStrategy(
                Strategy[object, DummyConfig],
                plugin_name=plugin_name,
            ):
                async def on_tick(self, tick: TickData) -> None:
                    return None

                async def on_fill(self, fill: FillData) -> None:
                    return None

                async def on_error(self, error: Exception) -> None:
                    return None

        assert Strategy.get_plugin(plugin_name) is FirstStrategy
    finally:
        Strategy.registry.pop(plugin_name, None)


from __future__ import annotations

import pytest
from pydantic import BaseModel

from src.config import Settings
from src.strategies.base import FillData, Strategy, TickData
from src.strategies.demo_sma import DemoSMAStrategy
from src.strategies.loader import (
    build_demo_sma_config,
    create_strategy,
    register_strategy_config,
    unregister_strategy_config,
)


class RecordingClient:
    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> dict[str, object]:
        return {
            "id": "order-1",
            "symbol": symbol,
            "type": order_type,
            "side": side,
            "amount": amount,
            "price": price,
            "status": "closed",
        }


def _settings(**overrides: object) -> Settings:
    values = {
        "api_key": "key",
        "api_secret": "secret",
    }
    values.update(overrides)
    return Settings(**values)


def test_create_strategy_loads_demo_sma_from_registry() -> None:
    settings = _settings(
        strategy_name="demo-sma",
        trading_symbol="ethusdt",
        fast_period=3,
        slow_period=8,
        timeframe="5m",
        order_size=2.5,
    )
    client = RecordingClient()

    strategy = create_strategy(settings.strategy_name, client, settings)

    assert isinstance(strategy, DemoSMAStrategy)
    assert strategy.name == "demo-sma"
    assert strategy.client is client
    assert strategy.settings.symbol == "ETH/USDT"
    assert strategy.settings.fast_period == 3
    assert strategy.settings.slow_period == 8
    assert strategy.settings.timeframe == "5m"
    assert strategy.settings.order_size == 2.5


def test_build_demo_sma_config_uses_runtime_settings() -> None:
    config = build_demo_sma_config(
        _settings(
            trading_symbol="btcusdt",
            fast_period=4,
            slow_period=10,
            timeframe="15m",
            order_size=0.2,
        )
    )

    assert config.symbol == "BTC/USDT"
    assert config.fast_period == 4
    assert config.slow_period == 10
    assert config.timeframe == "15m"
    assert config.order_size == 0.2


def test_create_strategy_rejects_unknown_plugin_name() -> None:
    with pytest.raises(KeyError, match="strategy plugin is not registered"):
        create_strategy("does-not-exist", RecordingClient(), _settings())


def test_create_strategy_rejects_registered_plugin_without_config_builder() -> None:
    plugin_name = "loader-test-no-builder"

    class DummyConfig(BaseModel):
        symbol: str

    class DummyStrategy(Strategy[object, DummyConfig], plugin_name=plugin_name):
        async def on_tick(self, tick: TickData) -> None:
            return None

        async def on_fill(self, fill: FillData) -> None:
            return None

        async def on_error(self, error: Exception) -> None:
            return None

    try:
        with pytest.raises(ValueError, match="config builder is not registered"):
            create_strategy(plugin_name, object(), _settings())
    finally:
        Strategy.registry.pop(plugin_name, None)
        unregister_strategy_config(plugin_name)


def test_register_strategy_config_allows_external_strategy_config_builders() -> None:
    plugin_name = "loader-test-with-builder"

    class DummyConfig(BaseModel):
        symbol: str

    class DummyStrategy(Strategy[object, DummyConfig], plugin_name=plugin_name):
        async def on_tick(self, tick: TickData) -> None:
            return None

        async def on_fill(self, fill: FillData) -> None:
            return None

        async def on_error(self, error: Exception) -> None:
            return None

    try:
        register_strategy_config(
            plugin_name,
            lambda settings: DummyConfig(symbol=settings.trading_symbol),
        )
        strategy = create_strategy(plugin_name, object(), _settings())

        assert isinstance(strategy, DummyStrategy)
        assert strategy.config.symbol == "BTC/USDT"
    finally:
        Strategy.registry.pop(plugin_name, None)

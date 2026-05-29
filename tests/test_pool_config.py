from __future__ import annotations

import pytest

import src.pool.config as pool_config
from src.pool.config import PoolSettings


def test_pool_settings_load_benchmarks_routes_and_scorers(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(pool_config, "load_dotenv", lambda: None)
    monkeypatch.setenv("BINANCE_API_KEY", " key ")
    monkeypatch.setenv("BINANCE_API_SECRET", " secret ")
    monkeypatch.setenv("POOL_BENCHMARKS", "USDT,BTC,PAXG,XAUT")
    monkeypatch.setenv("POOL_BENCHMARK_WEIGHTS", "0.1,0.2,0.3,0.4")
    monkeypatch.setenv("POOL_MAX_ROUTE_LEGS", "3")
    monkeypatch.setenv("POOL_SCORERS", "simple_momentum,risk_adjusted_momentum")

    settings = PoolSettings.from_env()

    assert settings.api_key == "key"
    assert settings.benchmark_weights == (0.1, 0.2, 0.3, 0.4)
    assert settings.max_route_legs == 3
    assert settings.mode == "paper"


def test_pool_settings_refuse_live_mode_and_unbounded_routes() -> None:
    with pytest.raises(RuntimeError, match="live pool execution is disabled"):
        PoolSettings(api_key="key", api_secret="secret", mode="live")

    with pytest.raises(RuntimeError, match="must not exceed 3"):
        PoolSettings(api_key="key", api_secret="secret", max_route_legs=4)

    assert PoolSettings(api_key="key", api_secret="secret").max_scoring_assets == 0

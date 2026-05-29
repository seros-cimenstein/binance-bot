from __future__ import annotations

import time
from pathlib import Path
from typing import Any

import pytest

from src.pool.config import PoolSettings
from src.pool.ledger import PaperLedger
from src.pool.runtime import PaperPoolRuntime


class ReadOnlyPoolClient:
    def __init__(self) -> None:
        self.balance_reads = 0
        self.commission_reads = 0

    async def get_balance(self) -> dict[str, dict[str, float]]:
        self.balance_reads += 1
        return {"total": {"BNB": 0.0169745}, "free": {"BNB": 0.0169745}}

    async def get_commission_rates(self, symbol: str) -> dict[str, Any]:
        self.commission_reads += 1
        return {
            "symbol": symbol,
            "standardCommission": {"taker": "0.001"},
            "discount": {"enabledForAccount": False, "enabledForSymbol": False},
        }

    async def request(self, method: str, *args: Any, **kwargs: Any) -> dict[str, Any]:
        symbols = {
            "BNB/USDT": ("BNB", "USDT", 660.0),
            "BTC/USDT": ("BTC", "USDT", 77_000.0),
            "PAXG/USDT": ("PAXG", "USDT", 4_500.0),
            "XAUT/USDT": ("XAUT", "USDT", 4_490.0),
        }
        if method == "load_markets":
            return {
                symbol: {
                    "symbol": symbol,
                    "base": values[0],
                    "quote": values[1],
                    "spot": True,
                    "active": True,
                    "precision": {"amount": 0.00001},
                    "limits": {"amount": {"min": 0.00001}, "cost": {"min": 5}},
                }
                for symbol, values in symbols.items()
            }
        now = int(time.time() * 1000)
        return {
            symbol: {
                "bid": price,
                "ask": price + 0.01,
                "last": price,
                "quoteVolume": 5_000_000,
                "timestamp": now,
            }
            for symbol, (_, _, price) in symbols.items()
        }


@pytest.mark.asyncio
async def test_paper_runtime_seeds_real_snapshot_and_never_places_orders(
    tmp_path: Path,
) -> None:
    client = ReadOnlyPoolClient()
    settings = PoolSettings(
        api_key="key",
        api_secret="secret",
        db_path=str(tmp_path / "paper.db"),
        min_quote_volume_usdt=0,
        max_spread_bps=50,
        slippage_buffer_bps=0,
        max_scoring_assets=4,
    )
    runtime = PaperPoolRuntime(client, settings, ledger=PaperLedger(settings.db_path))

    report = await runtime.cycle()

    assert report.catalog_markets == 4
    assert report.eligible_markets == 4
    assert report.seeded_balances == {"BNB": pytest.approx(0.0169745)}
    assert report.benchmark_totals["simple_momentum"]["USDT"] > 0
    assert {decision.action for decision in report.decisions} == {"hold"}
    assert all("insufficient benchmark history" in d.reason for d in report.decisions)
    assert client.balance_reads == 1
    assert client.commission_reads == 0
    runtime.close()


@pytest.mark.asyncio
async def test_paper_runtime_requires_configured_observation_warmup(
    tmp_path: Path,
) -> None:
    client = ReadOnlyPoolClient()
    settings = PoolSettings(
        api_key="key",
        api_secret="secret",
        db_path=str(tmp_path / "warmup.db"),
        min_quote_volume_usdt=0,
        max_spread_bps=50,
        slippage_buffer_bps=0,
        max_scoring_assets=4,
        momentum_observations=4,
    )
    runtime = PaperPoolRuntime(client, settings, ledger=PaperLedger(settings.db_path))

    reports = [await runtime.cycle() for _ in range(3)]

    assert all(
        decision.action == "hold"
        and "requires 4 cycles" in decision.reason
        for report in reports
        for decision in report.decisions
    )
    assert runtime.ledger.load_portfolio("simple_momentum").balances == {
        "BNB": pytest.approx(0.0169745)
    }
    assert client.commission_reads == 0
    runtime.close()

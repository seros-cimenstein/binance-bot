from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from src.pool.history_replay import HistoricalReplayError, run_historical_replay
from src.pool.report import build_report


class FakeCandleFetcher:
    def __init__(self, *, misalign: bool = False) -> None:
        self.symbols: list[str] = []
        self.misalign = misalign

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str,
        start_time: datetime,
        end_time: datetime | None = None,
    ) -> list[list[float | int]]:
        self.symbols.append(symbol)
        assert timeframe == "15m"
        base = symbol.split("/", 1)[0]
        prices = {
            "BNB": [600, 600, 600, 600, 600, 600],
            "BTC": [70_000, 70_000, 70_000, 70_000, 70_000, 70_000],
            "PAXG": [4_500, 4_500, 4_500, 4_500, 4_500, 4_500],
            "XAUT": [4_490, 4_490, 4_490, 4_490, 4_490, 4_490],
            "EDEN": [1, 1.05, 1.10, 1.20, 1.30, 1.40],
        }[base]
        offset = 1 if self.misalign and base == "EDEN" else 0
        return [
            [
                int((start_time + timedelta(minutes=15 * (index + offset))).timestamp() * 1000),
                price,
                price,
                price,
                price,
                1_000_000,
            ]
            for index, price in enumerate(prices)
        ]


@pytest.mark.asyncio
async def test_historical_replay_uses_public_candles_in_isolated_ledger(
    tmp_path: Path,
) -> None:
    start = datetime(2026, 5, 1, tzinfo=timezone.utc)
    fetcher = FakeCandleFetcher()
    db_path = tmp_path / "history.db"

    summary = await run_historical_replay(
        symbols=("EDEN/USDT",),
        initial_balances={"BNB": 0.2},
        start_time=start,
        end_time=start + timedelta(hours=2),
        db_path=db_path,
        required_observations=4,
        fetcher=fetcher,
    )

    assert set(fetcher.symbols) == {
        "BNB/USDT",
        "BTC/USDT",
        "EDEN/USDT",
        "PAXG/USDT",
        "XAUT/USDT",
    }
    assert summary.candle_cycles == 6
    assert summary.replay.decisions["rebalance"] >= 1
    assert "research only" in summary.scope
    assert build_report(db_path, required_observations=4).ranking_available is True


@pytest.mark.asyncio
async def test_historical_replay_requires_enough_aligned_candles(tmp_path: Path) -> None:
    start = datetime(2026, 5, 1, tzinfo=timezone.utc)

    with pytest.raises(HistoricalReplayError, match="aligned candles"):
        await run_historical_replay(
            symbols=("EDEN/USDT",),
            initial_balances={"BNB": 0.2},
            start_time=start,
            end_time=start + timedelta(hours=2),
            db_path=tmp_path / "misaligned.db",
            required_observations=6,
            fetcher=FakeCandleFetcher(misalign=True),
        )


@pytest.mark.asyncio
async def test_historical_replay_refuses_production_paper_ledger(tmp_path: Path) -> None:
    start = datetime(2026, 5, 1, tzinfo=timezone.utc)

    with pytest.raises(RuntimeError, match="production paper ledger"):
        await run_historical_replay(
            symbols=("EDEN/USDT",),
            initial_balances={"BNB": 0.2},
            start_time=start,
            end_time=start + timedelta(hours=2),
            db_path=tmp_path / "pool-paper.db",
            required_observations=4,
            fetcher=FakeCandleFetcher(),
        )


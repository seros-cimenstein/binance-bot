from __future__ import annotations

import json
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

from src.pool.replay import load_replay_fixture, run_replay


FIXTURE = Path(__file__).parent / "fixtures" / "pool_market_replay.json"
STRESS_FIXTURE = Path(__file__).parent / "fixtures" / "pool_market_replay_stress.json"
ROOT = Path(__file__).parents[1]


@pytest.mark.asyncio
async def test_replay_runs_many_cycles_fast_and_records_rebalance(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "replay.db"

    summary = await run_replay(FIXTURE, cycles=8, db_path=db_path)

    assert summary.cycles == 8
    assert summary.decisions["hold"] > 0
    assert summary.decisions["rebalance"] >= 1
    assert summary.final_balances["simple_momentum"]["EDEN"] > 0
    assert summary.balance_reads == 8
    assert summary.commission_reads > 0
    assert set(summary.request_methods) == {"load_markets", "fetch_tickers"}

    with sqlite3.connect(db_path) as connection:
        decision_count = connection.execute(
            "SELECT COUNT(*) FROM paper_decisions"
        ).fetchone()[0]
        catalog_count = connection.execute(
            "SELECT COUNT(*) FROM market_catalog_cycles"
        ).fetchone()[0]

    assert decision_count == 16
    assert catalog_count == 8


@pytest.mark.asyncio
async def test_replay_keeps_tiny_balances_below_market_minimums(
    tmp_path: Path,
) -> None:
    fixture = load_replay_fixture(FIXTURE)
    fixture["initial_balances"] = {"BNB": 0.001}
    fixture_path = tmp_path / "tiny-balance.json"
    fixture_path.write_text(json.dumps(fixture), encoding="utf-8")
    db_path = tmp_path / "tiny-replay.db"

    summary = await run_replay(fixture_path, cycles=6, db_path=db_path)

    assert summary.decisions.get("rebalance", 0) == 0
    assert summary.final_balances["simple_momentum"] == {"BNB": pytest.approx(0.001)}

    with sqlite3.connect(db_path) as connection:
        reasons = [
            row[0]
            for row in connection.execute(
                "SELECT reason FROM paper_decisions ORDER BY decided_at"
            ).fetchall()
        ]

    assert any("does not satisfy routed market minimums" in reason for reason in reasons)


@pytest.mark.asyncio
async def test_replay_refuses_production_paper_db_path(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError, match="production paper ledger"):
        await run_replay(FIXTURE, cycles=1, db_path=tmp_path / "pool-paper.db")


@pytest.mark.asyncio
async def test_replay_stress_fixture_records_market_rejections_and_no_rebalance(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "stress.db"

    summary = await run_replay(STRESS_FIXTURE, cycles=6, db_path=db_path)

    assert summary.decisions.get("rebalance", 0) == 0
    assert summary.final_balances["simple_momentum"] == {"BNB": pytest.approx(0.001)}

    with sqlite3.connect(db_path) as connection:
        rejections = {
            row[0]: row[1]
            for row in connection.execute(
                "SELECT symbol, rejection_reasons FROM market_catalog_latest"
            ).fetchall()
        }

    assert "spread exceeds configured threshold" in rejections["WIDE/USDT"]
    assert "quote volume below configured USDT threshold" in rejections["THIN/USDT"]


def test_pool_replay_script_runs_default_fixture_set(tmp_path: Path) -> None:
    result = subprocess.run(
        [str(ROOT / "scripts" / "pool_replay.sh")],
        check=True,
        capture_output=True,
        text=True,
        env={
            "BINANCE_BOT_PROJECT_DIR": str(ROOT),
            "BINANCE_BOT_PYTHON": sys.executable,
            "POOL_REPLAY_CYCLES": "6",
            "POOL_REPLAY_DB_DIR": str(tmp_path),
        },
    )

    assert "pool_market_replay.json" in result.stdout
    assert "pool_market_replay_stress.json" in result.stdout
    assert (tmp_path / "binance-bot-pool_market_replay.db").exists()
    assert (tmp_path / "binance-bot-pool_market_replay_stress.db").exists()

from __future__ import annotations

from pathlib import Path

import pytest

from src.pool.replay import run_replay
from src.pool.report import build_report, render_report


FIXTURE = Path(__file__).parent / "fixtures" / "pool_market_replay.json"
STRESS_FIXTURE = Path(__file__).parent / "fixtures" / "pool_market_replay_stress.json"


@pytest.mark.asyncio
async def test_report_explains_warmup_balances_and_rejections_without_writes(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "stress.db"
    await run_replay(STRESS_FIXTURE, cycles=2, db_path=db_path)
    modified_before = db_path.stat().st_mtime_ns

    report = build_report(db_path, required_observations=4)
    rendered = render_report(report)

    assert report.completed_cycles == 2
    assert report.remaining_observations == 2
    assert report.ranking_available is False
    assert report.balances["simple_momentum"] == {"BNB": pytest.approx(0.001)}
    assert report.rejection_reasons["spread exceeds configured threshold"] == 1
    assert report.rejection_reasons["quote volume below configured USDT threshold"] == 1
    assert "Ranked candidates: unavailable" in rendered
    assert "Decision reasons:" in rendered
    assert "Live execution: disabled" in rendered
    assert db_path.stat().st_mtime_ns == modified_before


@pytest.mark.asyncio
async def test_report_ranks_candidates_after_replay_observations_exist(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "ranked.db"
    await run_replay(FIXTURE, cycles=6, db_path=db_path)

    report = build_report(db_path, required_observations=4)

    assert report.ranking_available is True
    assert report.rankings["simple_momentum"][0].asset == "EDEN"
    assert report.decision_counts["rebalance"] >= 1

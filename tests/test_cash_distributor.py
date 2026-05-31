"""Tests for the performance-weighted cash distributor."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.cash_distributor import (
    Allocation,
    BotSnapshot,
    DistributionPlan,
    DistributorConfig,
    _sum_ledger_pnl,
    compute_plan,
    render_plan,
)


def _snap(name: str, *, pnl: float, current: float = 0.0,
          min_pct: float = 10.0, max_pct: float = 60.0) -> BotSnapshot:
    return BotSnapshot(
        name=name, lifetime_pnl_usdt=pnl, trades_lifetime=10,
        current_value_usdt=current,
        min_share_pct=min_pct, max_share_pct=max_pct,
    )


def test_plan_with_empty_treasury_returns_no_allocations() -> None:
    plan = compute_plan((_snap("a", pnl=10.0),),
                        total_treasury_usdt=0.0, free_usdt=0.0)
    assert plan.allocations == ()
    assert any("treasury" in n for n in plan.notes)


def test_plan_with_zero_bots_returns_no_allocations() -> None:
    plan = compute_plan((), total_treasury_usdt=100.0, free_usdt=100.0)
    assert plan.allocations == ()


def test_plan_floor_applied_when_all_bots_unprofitable() -> None:
    snaps = (
        _snap("a", pnl=-5.0, min_pct=10.0, max_pct=60.0),
        _snap("b", pnl=-3.0, min_pct=10.0, max_pct=60.0),
    )
    plan = compute_plan(snaps, total_treasury_usdt=100.0, free_usdt=100.0)
    # No bot has positive score → performance pool split equally.
    # mins sum to 20, pool 80, each bot gets 10 + 40 = 50%.
    shares = {a.bot: a.final_share_pct for a in plan.allocations}
    assert shares["a"] == pytest.approx(50.0)
    assert shares["b"] == pytest.approx(50.0)


def test_plan_winner_takes_performance_pool() -> None:
    snaps = (
        _snap("winner", pnl=80.0, min_pct=10.0, max_pct=60.0),
        _snap("loser", pnl=-20.0, min_pct=10.0, max_pct=60.0),
    )
    plan = compute_plan(snaps, total_treasury_usdt=100.0, free_usdt=100.0)
    shares = {a.bot: a.final_share_pct for a in plan.allocations}
    # Performance pool = 80%. winner gets 10 floor + 80 bonus = 90 → capped at 60.
    # Overflow 30% redistributes to "loser" (the only one not capped),
    # but loser also has cap of 60% — its room is 60 - 10 = 50, sufficient
    # to absorb 30 → final shares: winner 60, loser 40.
    assert shares["winner"] == pytest.approx(60.0)
    assert shares["loser"] == pytest.approx(40.0)


def test_plan_caps_a_runaway_winner() -> None:
    snaps = (
        _snap("a", pnl=100.0, min_pct=10.0, max_pct=40.0),  # would-be hog
        _snap("b", pnl=0.0,   min_pct=10.0, max_pct=80.0),  # absorber
    )
    plan = compute_plan(snaps, total_treasury_usdt=200.0, free_usdt=200.0)
    shares = {a.bot: a.final_share_pct for a in plan.allocations}
    assert shares["a"] == pytest.approx(40.0)  # hit max
    assert shares["b"] + shares["a"] == pytest.approx(100.0)


def test_plan_renormalises_min_shares_that_exceed_100() -> None:
    snaps = (
        _snap("a", pnl=10.0, min_pct=60.0, max_pct=70.0),
        _snap("b", pnl=10.0, min_pct=60.0, max_pct=70.0),
    )
    plan = compute_plan(snaps, total_treasury_usdt=100.0, free_usdt=100.0)
    shares = {a.bot: a.final_share_pct for a in plan.allocations}
    # Mins sum to 120%; renormalised to 60/60 → 50/50 after scaling.
    assert shares["a"] == pytest.approx(50.0)
    assert shares["b"] == pytest.approx(50.0)
    assert any("renormalised" in n for n in plan.notes)


def test_plan_delta_computes_topup_or_drain() -> None:
    snaps = (
        _snap("a", pnl=50.0, current=10.0, min_pct=20.0, max_pct=80.0),
        _snap("b", pnl=50.0, current=90.0, min_pct=20.0, max_pct=80.0),
    )
    plan = compute_plan(snaps, total_treasury_usdt=100.0, free_usdt=100.0)
    by_bot = {a.bot: a for a in plan.allocations}
    # Equal scores so equal shares (50%): both target $50.
    assert by_bot["a"].delta_usdt == pytest.approx(40.0)  # +40 top-up
    assert by_bot["b"].delta_usdt == pytest.approx(-40.0)  # drain


def test_plan_serialises_as_json() -> None:
    snaps = (_snap("a", pnl=10.0), _snap("b", pnl=-3.0))
    plan = compute_plan(snaps, total_treasury_usdt=100.0, free_usdt=100.0)
    decoded = json.loads(json.dumps(plan.as_dict()))
    assert decoded["total_treasury_usdt"] == 100.0
    assert len(decoded["allocations"]) == 2


def test_render_plan_includes_action_column() -> None:
    snaps = (_snap("a", pnl=50.0, current=0.0),)
    plan = compute_plan(snaps, total_treasury_usdt=100.0, free_usdt=100.0)
    output = render_plan(plan, threshold_usdt=1.0)
    assert "BOT" in output and "SHARE" in output
    assert "TOP UP" in output


def test_sum_ledger_pnl_returns_zero_on_missing_file(tmp_path: Path) -> None:
    total, count = _sum_ledger_pnl(tmp_path / "nope.jsonl", fields=("net_pnl_usdt",))
    assert total == 0.0 and count == 0


def test_sum_ledger_pnl_reads_first_present_field(tmp_path: Path) -> None:
    p = tmp_path / "l.jsonl"
    p.write_text(
        '{"usd_pnl": 5.0}\n'
        '{"net_pnl_usdt": 2.5}\n'
        '{"other_field": 99}\n'        # counted but no P&L picked up
        '{"net_pnl_usdt": "not-a-num"}\n'  # fall through, still counted
    )
    total, count = _sum_ledger_pnl(p, fields=("net_pnl_usdt", "usd_pnl"))
    assert total == pytest.approx(7.5)
    assert count == 4


def test_sum_ledger_pnl_ignores_corrupt_lines(tmp_path: Path) -> None:
    p = tmp_path / "l.jsonl"
    p.write_text('not valid json\n{"net_pnl_usdt": 1.0}\n')
    total, count = _sum_ledger_pnl(p, fields=("net_pnl_usdt",))
    assert total == pytest.approx(1.0)
    assert count == 1

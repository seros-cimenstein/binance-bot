"""Tests for the sniper forward-paper executor."""

from __future__ import annotations

import json
import random
from pathlib import Path

import pytest

from src.sniper import paper as paper_mod
from src.sniper.detector import PUMP_FUN_PROGRAM_ID, PoolCreationEvent
from src.sniper.jupiter import JupiterQuote, JupiterQuoteError, SOL_MINT
from src.sniper.paper import ExecutionFriction
from src.sniper.pipeline import TradeCandidate
from src.sniper.pump_fun_decoder import PumpFunCreateEvent
from src.sniper.scorer import TokenSafetyScore


ZERO_FRICTION = ExecutionFriction.zero()


def _candidate(mint: str = "TESTMINT", symbol: str = "TST", name: str = "Test") -> TradeCandidate:
    return TradeCandidate(
        pool_event=PoolCreationEvent(
            program_label="pump_fun", program_id=PUMP_FUN_PROGRAM_ID,
            signature="sig", slot=1, detected_at_ms=0,
            matched_markers=(), log_excerpt=(),
        ),
        create_event=PumpFunCreateEvent(
            name=name, symbol=symbol, uri="https://x/m.json",
            mint=mint, bonding_curve="bc", user="u",
        ),
        safety=TokenSafetyScore(
            mint=mint, has_mint_authority=False, has_freeze_authority=False,
            supply=1_000_000, decimals=6,
            top_holder_pct=2.0, top_10_pct=20.0, holder_count_top_20=50,
            score=80.0, rejection_reason=None,
        ),
        evaluated_at_ms=0,
    )


def _quote(in_amount: int, out_amount: int, *, usd: float = 1.0,
           labels: tuple[str, ...] = ("Pump.fun",)) -> JupiterQuote:
    return JupiterQuote(
        input_mint=SOL_MINT, output_mint="X",
        in_amount=in_amount, out_amount=out_amount,
        price_impact_pct=0.0, swap_usd_value=usd,
        route_labels=labels, context_slot=1, raw={},
    )


@pytest.mark.asyncio
async def test_simulate_snipe_records_profitable_round_trip(
    tmp_path: Path, monkeypatch
) -> None:
    ledger = tmp_path / "ledger.jsonl"
    buy = _quote(in_amount=10_000_000, out_amount=100_000_000, usd=1.0)
    # Token went up; same token amount returns more SOL on exit.
    sell = _quote(in_amount=100_000_000, out_amount=12_000_000, usd=1.2)

    async def _fake_quote(*, input_mint, output_mint, amount_atoms, **kw):
        return buy if input_mint == SOL_MINT else sell

    monkeypatch.setattr(paper_mod, "get_quote", _fake_quote)
    # Skip the real sleep so the test runs in milliseconds.
    monkeypatch.setattr(paper_mod.asyncio, "sleep",
                        lambda *_args, **_kw: _aw_pass())
    rec = await paper_mod.simulate_snipe(
        candidate=_candidate(), entry_sol_atoms=10_000_000,
        hold_seconds=1, slippage_bps=500,
        quote_url="x", session=None, ledger_path=str(ledger),
        friction=ZERO_FRICTION,
    )
    assert rec is not None
    assert rec.exit_status == "round_trip"
    assert rec.sol_pnl_atoms == 2_000_000
    assert rec.sol_pnl_pct == pytest.approx(20.0)
    assert rec.usd_pnl == pytest.approx(0.2)
    assert rec.gas_paid_lamports == 0
    assert rec.friction_profile_id == "zero"
    # Ledger row written.
    lines = ledger.read_text().splitlines()
    assert len(lines) == 1
    assert json.loads(lines[0])["sol_pnl_atoms"] == 2_000_000


@pytest.mark.asyncio
async def test_simulate_snipe_returns_none_when_entry_quote_fails(
    tmp_path: Path, monkeypatch
) -> None:
    async def _failing_quote(**_kw):
        raise JupiterQuoteError("no route")

    monkeypatch.setattr(paper_mod, "get_quote", _failing_quote)
    rec = await paper_mod.simulate_snipe(
        candidate=_candidate(), entry_sol_atoms=10_000_000,
        hold_seconds=1, slippage_bps=500,
        quote_url="x", session=None,
        ledger_path=str(tmp_path / "ledger.jsonl"),
        friction=ZERO_FRICTION,
    )
    assert rec is None
    assert not (tmp_path / "ledger.jsonl").exists()


@pytest.mark.asyncio
async def test_simulate_snipe_records_no_exit_route_failure(
    tmp_path: Path, monkeypatch
) -> None:
    buy = _quote(in_amount=10_000_000, out_amount=100_000_000)
    calls = {"n": 0}

    async def _quote_then_fail(*, input_mint, output_mint, amount_atoms, **kw):
        calls["n"] += 1
        if calls["n"] == 1:
            return buy
        raise JupiterQuoteError("liquidity rug-pulled")

    monkeypatch.setattr(paper_mod, "get_quote", _quote_then_fail)
    monkeypatch.setattr(paper_mod.asyncio, "sleep",
                        lambda *_args, **_kw: _aw_pass())
    rec = await paper_mod.simulate_snipe(
        candidate=_candidate(), entry_sol_atoms=10_000_000,
        hold_seconds=1, slippage_bps=500,
        quote_url="x", session=None,
        ledger_path=str(tmp_path / "ledger.jsonl"),
        friction=ZERO_FRICTION,
    )
    assert rec is not None
    assert rec.exit_status == "no_exit_route"
    assert rec.sol_pnl_atoms == -10_000_000  # spent SOL, got nothing back
    assert "liquidity" in rec.error


@pytest.mark.asyncio
async def test_simulate_snipe_records_entry_failed_tx_when_fill_prob_zero(
    tmp_path: Path, monkeypatch
) -> None:
    """Friction with entry_fill_prob=0 → tx never lands, only base fee burned."""
    ledger = tmp_path / "ledger.jsonl"
    buy = _quote(in_amount=10_000_000, out_amount=100_000_000, usd=1.0)

    async def _fake_quote(**_kw):
        return buy

    monkeypatch.setattr(paper_mod, "get_quote", _fake_quote)
    monkeypatch.setattr(paper_mod.asyncio, "sleep",
                        lambda *_args, **_kw: _aw_pass())
    friction = ExecutionFriction(
        profile_id="entry_always_fails",
        entry_latency_ms=0, entry_latency_jitter_ms=0,
        entry_fill_prob=0.0, exit_fill_prob=1.0,
        base_fee_lamports=5_000, priority_fee_lamports=20_000,
    )
    rec = await paper_mod.simulate_snipe(
        candidate=_candidate(), entry_sol_atoms=10_000_000,
        hold_seconds=1, slippage_bps=500,
        quote_url="x", session=None, ledger_path=str(ledger),
        friction=friction,
    )
    assert rec is not None
    assert rec.exit_status == "entry_failed_tx"
    assert rec.entry_landed is False
    assert rec.exit_landed is False
    assert rec.entry_in_sol_atoms == 0
    assert rec.gas_paid_lamports == 5_000  # base fee only on failed entry
    assert rec.sol_pnl_atoms == -5_000
    assert rec.friction_profile_id == "entry_always_fails"


@pytest.mark.asyncio
async def test_simulate_snipe_records_exit_failed_tx_as_full_position_loss(
    tmp_path: Path, monkeypatch
) -> None:
    """Friction with exit_fill_prob=0 → exit tx never lands, position lost."""
    ledger = tmp_path / "ledger.jsonl"
    buy = _quote(in_amount=10_000_000, out_amount=100_000_000, usd=1.0)
    sell = _quote(in_amount=100_000_000, out_amount=12_000_000, usd=1.2)

    async def _fake_quote(*, input_mint, **_kw):
        return buy if input_mint == SOL_MINT else sell

    monkeypatch.setattr(paper_mod, "get_quote", _fake_quote)
    monkeypatch.setattr(paper_mod.asyncio, "sleep",
                        lambda *_args, **_kw: _aw_pass())
    friction = ExecutionFriction(
        profile_id="exit_always_fails",
        entry_latency_ms=0, entry_latency_jitter_ms=0,
        entry_fill_prob=1.0, exit_fill_prob=0.0,
        base_fee_lamports=5_000, priority_fee_lamports=20_000,
    )
    rec = await paper_mod.simulate_snipe(
        candidate=_candidate(), entry_sol_atoms=10_000_000,
        hold_seconds=1, slippage_bps=500,
        quote_url="x", session=None, ledger_path=str(ledger),
        friction=friction,
    )
    assert rec is not None
    assert rec.exit_status == "exit_failed_tx"
    assert rec.entry_landed is True
    assert rec.exit_landed is False
    assert rec.exit_out_sol_atoms == 0
    # gas = entry(base+priority) + exit base on failed land = 25k + 5k = 30k
    assert rec.gas_paid_lamports == 30_000
    # sol pnl = 0 - 10_000_000 - 30_000 = full entry loss + gas
    assert rec.sol_pnl_atoms == -10_030_000


@pytest.mark.asyncio
async def test_simulate_snipe_landed_round_trip_subtracts_gas(
    tmp_path: Path, monkeypatch
) -> None:
    """Friction with both fills landing → gas deducted from a winning trade."""
    ledger = tmp_path / "ledger.jsonl"
    buy = _quote(in_amount=10_000_000, out_amount=100_000_000, usd=1.0)
    sell = _quote(in_amount=100_000_000, out_amount=12_000_000, usd=1.2)

    async def _fake_quote(*, input_mint, **_kw):
        return buy if input_mint == SOL_MINT else sell

    monkeypatch.setattr(paper_mod, "get_quote", _fake_quote)
    monkeypatch.setattr(paper_mod.asyncio, "sleep",
                        lambda *_args, **_kw: _aw_pass())
    friction = ExecutionFriction(
        profile_id="all_landed",
        entry_latency_ms=0, entry_latency_jitter_ms=0,
        entry_fill_prob=1.0, exit_fill_prob=1.0,
        base_fee_lamports=5_000, priority_fee_lamports=20_000,
    )
    rec = await paper_mod.simulate_snipe(
        candidate=_candidate(), entry_sol_atoms=10_000_000,
        hold_seconds=1, slippage_bps=500,
        quote_url="x", session=None, ledger_path=str(ledger),
        friction=friction,
    )
    assert rec is not None
    assert rec.exit_status == "round_trip"
    # gas = 2 × (base + priority) = 50_000
    assert rec.gas_paid_lamports == 50_000
    # sol_pnl = 12_000_000 - 10_000_000 - 50_000 = 1_950_000
    assert rec.sol_pnl_atoms == 1_950_000


@pytest.mark.asyncio
async def test_simulate_snipe_applies_entry_latency(
    tmp_path: Path, monkeypatch
) -> None:
    """Latency from friction is recorded in the ledger entry."""
    buy = _quote(in_amount=10_000_000, out_amount=100_000_000, usd=1.0)
    sell = _quote(in_amount=100_000_000, out_amount=12_000_000, usd=1.2)
    sleeps: list[float] = []

    async def _record_sleep(secs: float = 0.0) -> None:
        sleeps.append(secs)

    async def _fake_quote(*, input_mint, **_kw):
        return buy if input_mint == SOL_MINT else sell

    monkeypatch.setattr(paper_mod, "get_quote", _fake_quote)
    monkeypatch.setattr(paper_mod.asyncio, "sleep", _record_sleep)
    friction = ExecutionFriction(
        profile_id="latency_only",
        entry_latency_ms=500, entry_latency_jitter_ms=0,
        entry_fill_prob=1.0, exit_fill_prob=1.0,
        base_fee_lamports=0, priority_fee_lamports=0,
    )
    rec = await paper_mod.simulate_snipe(
        candidate=_candidate(), entry_sol_atoms=10_000_000,
        hold_seconds=7, slippage_bps=500,
        quote_url="x", session=None,
        ledger_path=str(tmp_path / "ledger.jsonl"),
        friction=friction,
    )
    assert rec is not None
    assert rec.entry_latency_ms_applied == 500
    # First sleep = latency (0.5s), second sleep = hold (7s).
    assert sleeps == [0.5, 7]


async def _aw_pass() -> None:
    return None

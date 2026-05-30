"""Tests for the sniper detector→scorer pipeline."""

from __future__ import annotations

import base64

import pytest

from src.sniper import pipeline as pipeline_mod
from src.sniper.detector import (
    PUMP_FUN_PROGRAM_ID,
    RAYDIUM_AMM_V4_PROGRAM_ID,
    PoolCreationEvent,
)
from src.sniper.pump_fun_decoder import CREATE_EVENT_DISCRIMINATOR
from src.sniper.scorer import MintInfo, TokenSafetyScore


def _build_create_payload(name: str, symbol: str, uri: str, mint_bytes: bytes) -> str:
    body = CREATE_EVENT_DISCRIMINATOR
    for s in (name, symbol, uri):
        encoded = s.encode("utf-8")
        body += len(encoded).to_bytes(4, "little") + encoded
    body += mint_bytes + bytes(32) + bytes(32)  # bonding curve + user
    return base64.b64encode(body).decode()


def _pump_event(*, mint_bytes: bytes) -> PoolCreationEvent:
    payload = _build_create_payload("Test", "TST", "https://x/m.json", mint_bytes)
    return PoolCreationEvent(
        program_label="pump_fun", program_id=PUMP_FUN_PROGRAM_ID,
        signature="testsig", slot=1, detected_at_ms=0,
        matched_markers=("Program log: Instruction: Create",),
        log_excerpt=(f"Program data: {payload}",),
    )


def _raydium_event() -> PoolCreationEvent:
    return PoolCreationEvent(
        program_label="raydium_amm_v4", program_id=RAYDIUM_AMM_V4_PROGRAM_ID,
        signature="raysig", slot=2, detected_at_ms=0,
        matched_markers=("Program log: initialize2",),
        log_excerpt=("Program log: initialize2",),
    )


@pytest.mark.asyncio
async def test_pipeline_skips_raydium_events(monkeypatch) -> None:
    async def _fake_stream(**_kw):
        yield _raydium_event()

    monkeypatch.setattr(pipeline_mod, "stream_pool_creations", _fake_stream)

    async def _no_score(*a, **kw):
        raise AssertionError("scorer should not be called for skipped events")

    monkeypatch.setattr(pipeline_mod, "score_token", _no_score)
    seen = [c async for c in pipeline_mod.candidate_stream()]
    assert seen == []


@pytest.mark.asyncio
async def test_pipeline_decodes_pump_event_and_calls_scorer(monkeypatch) -> None:
    mint_bytes = bytes(range(32))

    async def _fake_stream(**_kw):
        yield _pump_event(mint_bytes=mint_bytes)

    scored_mints: list[str] = []

    async def _fake_score(mint, *, rpc_url, session, **kw):
        scored_mints.append(mint)
        return TokenSafetyScore(
            mint=mint, has_mint_authority=False, has_freeze_authority=False,
            supply=1_000_000, decimals=6,
            top_holder_pct=2.0, top_10_pct=20.0,
            holder_count_top_20=50, score=80.0, rejection_reason=None,
        )

    monkeypatch.setattr(pipeline_mod, "stream_pool_creations", _fake_stream)
    monkeypatch.setattr(pipeline_mod, "score_token", _fake_score)

    candidates = [c async for c in pipeline_mod.candidate_stream()]
    assert len(candidates) == 1
    assert len(scored_mints) == 1
    candidate = candidates[0]
    assert candidate.create_event.name == "Test"
    assert candidate.safety.is_tradeable
    assert candidate.safety.score == 80.0


@pytest.mark.asyncio
async def test_pipeline_filters_rejected_unless_emit_rejected(monkeypatch) -> None:
    mint_bytes = bytes(range(32))

    async def _fake_stream(**_kw):
        yield _pump_event(mint_bytes=mint_bytes)

    async def _fake_score(mint, *, rpc_url, session, **kw):
        return TokenSafetyScore(
            mint=mint, has_mint_authority=True, has_freeze_authority=False,
            supply=1_000, decimals=6, top_holder_pct=0.0, top_10_pct=0.0,
            holder_count_top_20=0, score=0.0,
            rejection_reason="mint authority not renounced",
        )

    monkeypatch.setattr(pipeline_mod, "stream_pool_creations", _fake_stream)
    monkeypatch.setattr(pipeline_mod, "score_token", _fake_score)

    # Default: rejected candidates are filtered out.
    candidates = [c async for c in pipeline_mod.candidate_stream()]
    assert candidates == []

    # With emit_rejected, the rejected one comes through.
    async def _fake_stream2(**_kw):
        yield _pump_event(mint_bytes=mint_bytes)

    monkeypatch.setattr(pipeline_mod, "stream_pool_creations", _fake_stream2)
    candidates = [c async for c in pipeline_mod.candidate_stream(emit_rejected=True)]
    assert len(candidates) == 1
    assert not candidates[0].safety.is_tradeable


@pytest.mark.asyncio
async def test_pipeline_does_not_crash_when_scorer_raises(monkeypatch) -> None:
    mint_bytes = bytes(range(32))

    async def _fake_stream(**_kw):
        yield _pump_event(mint_bytes=mint_bytes)

    async def _fake_score(mint, *, rpc_url, session, **kw):
        raise RuntimeError("RPC timeout")

    monkeypatch.setattr(pipeline_mod, "stream_pool_creations", _fake_stream)
    monkeypatch.setattr(pipeline_mod, "score_token", _fake_score)
    candidates = [c async for c in pipeline_mod.candidate_stream()]
    assert candidates == []  # crash swallowed; stream continues


@pytest.mark.asyncio
async def test_pipeline_skips_pump_events_without_decodable_data(monkeypatch) -> None:
    pump = PoolCreationEvent(
        program_label="pump_fun", program_id=PUMP_FUN_PROGRAM_ID,
        signature="testsig", slot=1, detected_at_ms=0,
        matched_markers=("Program log: Instruction: Create",),
        log_excerpt=("Program ComputeBudget invoke [1]",),  # no Program data line
    )

    async def _fake_stream(**_kw):
        yield pump

    async def _no_score(*a, **kw):
        raise AssertionError("scorer should not run when decode fails")

    monkeypatch.setattr(pipeline_mod, "stream_pool_creations", _fake_stream)
    monkeypatch.setattr(pipeline_mod, "score_token", _no_score)
    candidates = [c async for c in pipeline_mod.candidate_stream()]
    assert candidates == []

"""Tests for the Solana pool-creation detector."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.sniper.detector import (
    DEFAULT_INIT_MARKERS,
    PUMP_FUN_PROGRAM_ID,
    RAYDIUM_AMM_V4_PROGRAM_ID,
    PoolCreationEvent,
    SniperDetectorError,
    _matches_any,
    parse_log_notification,
    run_detector,
)


# Sample logsNotification payloads shaped like real Solana RPC output.
def _notification(*, signature: str, slot: int, logs: list[str], sub_id: int = 1, err=None) -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "logsNotification",
        "params": {
            "subscription": sub_id,
            "result": {
                "context": {"slot": slot},
                "value": {"signature": signature, "err": err, "logs": logs},
            },
        },
    }


def test_matches_any_finds_first_marker_present() -> None:
    logs = ["Program log: starting", "Program log: initialize2 detected here", "Program log: done"]
    matched = _matches_any(logs, ["initialize2", "InitializeNotPresent"])
    assert "initialize2" in matched


def test_matches_any_returns_empty_when_nothing_matches() -> None:
    assert _matches_any(["Program log: swap"], ["initialize2"]) == ()


def test_parse_raydium_initialization_yields_event() -> None:
    payload = _notification(
        signature="5sigRaydium",
        slot=999,
        logs=[
            "Program 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 invoke [1]",
            "Program log: initialize2: amm_open_time=now",
            "Program 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 success",
        ],
    )
    event = parse_log_notification(
        payload, program_label="raydium_amm_v4", program_id=RAYDIUM_AMM_V4_PROGRAM_ID,
        init_markers=DEFAULT_INIT_MARKERS["raydium_amm_v4"],
    )
    assert event is not None
    assert event.signature == "5sigRaydium"
    assert event.slot == 999
    assert event.program_label == "raydium_amm_v4"
    assert any("initialize2" in m for m in event.matched_markers)


def test_parse_pump_fun_create_yields_event() -> None:
    payload = _notification(
        signature="5sigPump",
        slot=1234,
        logs=[
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]",
            "Program log: Instruction: Create",
            "Program log: bonding curve initialized",
        ],
    )
    event = parse_log_notification(
        payload, program_label="pump_fun", program_id=PUMP_FUN_PROGRAM_ID,
        init_markers=DEFAULT_INIT_MARKERS["pump_fun"],
    )
    assert event is not None
    assert event.signature == "5sigPump"
    assert event.program_label == "pump_fun"


def test_parse_returns_none_for_swap_only_logs() -> None:
    payload = _notification(
        signature="swapSig", slot=1,
        logs=["Program log: swap_base_in", "Program 675k... success"],
    )
    event = parse_log_notification(
        payload, program_label="raydium_amm_v4", program_id=RAYDIUM_AMM_V4_PROGRAM_ID,
        init_markers=DEFAULT_INIT_MARKERS["raydium_amm_v4"],
    )
    assert event is None


def test_parse_returns_none_for_failed_transactions() -> None:
    payload = _notification(
        signature="failedSig", slot=1,
        logs=["Program log: initialize2"],
        err={"InstructionError": [0, "InvalidAccountData"]},
    )
    event = parse_log_notification(
        payload, program_label="raydium_amm_v4", program_id=RAYDIUM_AMM_V4_PROGRAM_ID,
        init_markers=DEFAULT_INIT_MARKERS["raydium_amm_v4"],
    )
    assert event is None


def test_parse_returns_none_for_unrelated_method() -> None:
    payload = {"jsonrpc": "2.0", "method": "accountNotification", "params": {}}
    event = parse_log_notification(
        payload, program_label="raydium_amm_v4", program_id=RAYDIUM_AMM_V4_PROGRAM_ID,
        init_markers=DEFAULT_INIT_MARKERS["raydium_amm_v4"],
    )
    assert event is None


def test_parse_handles_malformed_payload_safely() -> None:
    for bad in (None, {}, {"method": "logsNotification"}, "not a dict"):
        event = parse_log_notification(
            bad if isinstance(bad, dict) else {}, program_label="raydium_amm_v4",
            program_id=RAYDIUM_AMM_V4_PROGRAM_ID,
            init_markers=DEFAULT_INIT_MARKERS["raydium_amm_v4"],
        )
        assert event is None


def test_event_dict_round_trips_as_json() -> None:
    event = PoolCreationEvent(
        program_label="raydium_amm_v4", program_id=RAYDIUM_AMM_V4_PROGRAM_ID,
        signature="sig1", slot=42, detected_at_ms=1_000_000,
        matched_markers=("Program log: initialize2",),
        log_excerpt=("Program log: initialize2",),
    )
    decoded = json.loads(json.dumps(event.as_dict()))
    assert decoded["signature"] == "sig1"
    assert decoded["matched_markers"] == ["Program log: initialize2"]


@pytest.mark.asyncio
async def test_run_detector_refuses_when_emergency_stop_present(tmp_path: Path) -> None:
    stop = tmp_path / "stop"
    stop.write_text("halt")
    with pytest.raises(SniperDetectorError, match="emergency stop"):
        await run_detector(
            ws_url="wss://example.invalid",
            audit_path=str(tmp_path / "audit.jsonl"),
            emergency_stop_path=str(stop),
            run_duration_seconds=0.1,
        )

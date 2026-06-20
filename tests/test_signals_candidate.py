"""Tests for signal candidate serialization."""

from __future__ import annotations

import json

import pytest

from src.signals.candidate import SignalCandidate


def _candidate() -> SignalCandidate:
    return SignalCandidate(
        source="dexscreener",
        chain="solana",
        token_address="Token111",
        token_symbol="TOK",
        detected_at_ms=123,
        raw_signal={"pair": {"url": "https://example.test"}},
        confidence=0.75,
    )


def test_signal_candidate_is_frozen() -> None:
    candidate = _candidate()
    with pytest.raises(TypeError):
        candidate.source = "telegram"  # type: ignore[misc]


def test_signal_candidate_as_dict_json_round_trip() -> None:
    data = _candidate().as_dict()
    assert json.loads(json.dumps(data)) == data


def test_signal_candidate_as_dict_returns_fresh_dict() -> None:
    candidate = _candidate()
    first = candidate.as_dict()
    second = candidate.as_dict()
    first["source"] = "mutated"
    first["raw_signal"]["pair"]["url"] = "mutated"
    assert second["source"] == "dexscreener"
    assert second["raw_signal"]["pair"]["url"] == "https://example.test"

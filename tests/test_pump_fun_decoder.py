"""Tests for the pump.fun CreateEvent decoder + base58 encoder."""

from __future__ import annotations

import base64

import pytest

from src.sniper.pump_fun_decoder import (
    CREATE_EVENT_DISCRIMINATOR,
    PumpFunDecodeError,
    base58_encode,
    decode_create_event_from_logs,
)


# ---------------------------------------------------------------------------
# base58
# ---------------------------------------------------------------------------


def test_base58_known_vectors() -> None:
    # Bitcoin-style base58 alphabet — well-known test vectors.
    assert base58_encode(b"Hello World!") == "2NEpo7TZRRrLZSi2U"
    assert base58_encode(b"") == ""
    # Pure-zero pubkey encodes to 32 '1' chars (the Solana system program).
    assert base58_encode(b"\x00" * 32) == "1" * 32


def test_base58_preserves_leading_zeros() -> None:
    assert base58_encode(b"\x00\x00\x01") == "112"


# ---------------------------------------------------------------------------
# CreateEvent decode
# ---------------------------------------------------------------------------


def _build_create_event_payload(
    *, name: str, symbol: str, uri: str,
    mint: bytes, bonding_curve: bytes, user: bytes,
) -> str:
    """Construct a base64 CreateEvent payload matching pump.fun's layout."""
    assert len(mint) == 32 and len(bonding_curve) == 32 and len(user) == 32
    body = CREATE_EVENT_DISCRIMINATOR
    for s in (name, symbol, uri):
        encoded = s.encode("utf-8")
        body += len(encoded).to_bytes(4, "little") + encoded
    body += mint + bonding_curve + user
    return base64.b64encode(body).decode()


def test_decode_synthetic_event_roundtrip() -> None:
    mint = bytes(range(32))
    bonding = bytes(range(32, 64))
    user = bytes(range(64, 96))
    payload = _build_create_event_payload(
        name="My Coin", symbol="MYC", uri="https://example.com/meta.json",
        mint=mint, bonding_curve=bonding, user=user,
    )
    logs = [
        "Program ComputeBudget111... invoke [1]",
        f"Program data: {payload}",
        "Program 6EF8rrec... success",
    ]
    event = decode_create_event_from_logs(logs)
    assert event is not None
    assert event.name == "My Coin"
    assert event.symbol == "MYC"
    assert event.uri == "https://example.com/meta.json"
    # Mint base58 should decode back to the original bytes — sanity-check the encoder.
    assert event.mint == base58_encode(mint)
    assert event.bonding_curve == base58_encode(bonding)
    assert event.user == base58_encode(user)


def test_decode_real_world_event() -> None:
    """Decode the actual on-chain event captured during live development.

    This locks the discriminator + parser against real pump.fun emissions.
    """
    # Full Program data line from a real pump.fun CreateV2 transaction:
    # Artificial God Intelligence (AGI), mint EXwTiUzN6uEp...pump
    payload = (
        "G3KpTd7rY3YbAAAAQXJ0aWZpY2lhbCBHb2QgSW50ZWxsaWdlbmNlAwAAAEFHSTYAAABo"
        "dHRwczovL21ldGFkYXRhLmo3dHJhY2tlci5pby9tZXRhZGF0YS85N3FGVUk0ajJBLmpz"
        "b27JFO7g7qHu7QxcLPLNwt2j/dGqxM8+f8hp08sKdWLZf/Z7AKbeNzMQXg0J1zRyhwBl"
        "GgZJsZmpHrf16WoiPN/EDgNABSfFdZjhcdGbzonmWk944q0wzrWh8PItSgBoGndmWxs="
    )
    logs = [f"Program data: {payload}"]
    event = decode_create_event_from_logs(logs)
    assert event is not None
    assert event.name == "Artificial God Intelligence"
    assert event.symbol == "AGI"
    assert "metadata.j7tracker" in event.uri
    assert event.mint == "EXwTiUzN6uEpDimj3kD6A97w8rHjEMkNA2fDu29gpump"


def test_decode_returns_none_when_no_program_data_lines() -> None:
    logs = ["Program ComputeBudget invoke [1]", "Program ComputeBudget success"]
    assert decode_create_event_from_logs(logs) is None


def test_decode_returns_none_when_discriminator_mismatch() -> None:
    bogus = base64.b64encode(b"\x00" * 8 + b"\x05\x00\x00\x00hello" + b"\x00" * 16).decode()
    logs = [f"Program data: {bogus}"]
    assert decode_create_event_from_logs(logs) is None


def test_decode_skips_unrelated_program_data_and_finds_real_event() -> None:
    # Mix in a non-CreateEvent Program data line; decoder should keep scanning.
    mint = bytes(range(32))
    real_payload = _build_create_event_payload(
        name="X", symbol="X", uri="x",
        mint=mint, bonding_curve=bytes(32), user=bytes(32),
    )
    logs = [
        "Program data: AAECAwQFBgcEAAAA",  # unrelated buy/sell event
        f"Program data: {real_payload}",
    ]
    event = decode_create_event_from_logs(logs)
    assert event is not None and event.name == "X"


def test_decode_handles_invalid_base64_safely() -> None:
    logs = ["Program data: not-valid-base64-!@#$"]
    # Should not raise; should return None since no valid event is found.
    assert decode_create_event_from_logs(logs) is None


def test_decode_handles_truncated_payload() -> None:
    # Discriminator present but the body is truncated.
    truncated = base64.b64encode(CREATE_EVENT_DISCRIMINATOR + b"\x05\x00\x00").decode()
    logs = [f"Program data: {truncated}"]
    assert decode_create_event_from_logs(logs) is None

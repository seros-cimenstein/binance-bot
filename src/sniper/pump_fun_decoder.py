"""Decode pump.fun CreateEvent emissions directly from on-chain log strings.

When a new pump.fun token is launched, the program emits a Borsh-encoded
``CreateEvent`` as a ``Program data: ...`` log line. The event payload is:

    8 bytes  Anchor event discriminator (constant for CreateEvent)
    u32 LE   length of name + UTF-8 bytes
    u32 LE   length of symbol + UTF-8 bytes
    u32 LE   length of uri + UTF-8 bytes
    32 bytes mint pubkey
    32 bytes bonding-curve pubkey
    32 bytes user (deployer) pubkey

This module extracts that event from the captured log lines of a
``PoolCreationEvent``, decodes it, and base58-encodes the mint pubkey so
downstream scorer / executor code can use it. No solana-py dependency.
"""

from __future__ import annotations

import base64
from dataclasses import dataclass
from typing import Sequence


_BASE58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

# Anchor discriminator for pump.fun's CreateEvent. First 8 bytes of the
# event payload are constant. Observed from real on-chain events.
CREATE_EVENT_DISCRIMINATOR = bytes(
    [0x1B, 0x72, 0xA9, 0x4D, 0xDE, 0xEB, 0x63, 0x76]
)


class PumpFunDecodeError(RuntimeError):
    """Raised when a pump.fun event cannot be decoded."""


def base58_encode(data: bytes) -> str:
    """Encode bytes as Bitcoin-style base58 (Solana convention)."""
    # Count leading zero bytes; preserved as leading '1's.
    n_leading_zeros = 0
    for b in data:
        if b == 0:
            n_leading_zeros += 1
        else:
            break
    n = int.from_bytes(data, "big")
    chars: list[str] = []
    while n > 0:
        n, rem = divmod(n, 58)
        chars.append(_BASE58_ALPHABET[rem])
    chars.reverse()
    return "1" * n_leading_zeros + "".join(chars)


@dataclass(frozen=True, slots=True)
class PumpFunCreateEvent:
    name: str
    symbol: str
    uri: str
    mint: str  # base58 pubkey
    bonding_curve: str
    user: str


def _extract_program_data_lines(logs: Sequence[str]) -> list[str]:
    """Return the base64 payload of every ``Program data: <payload>`` log."""
    out: list[str] = []
    for line in logs:
        if line.startswith("Program data: "):
            out.append(line[len("Program data: "):].strip())
    return out


def _decode_create_event_payload(payload_b64: str) -> PumpFunCreateEvent:
    try:
        data = base64.b64decode(payload_b64, validate=False)
    except (TypeError, ValueError) as exc:
        raise PumpFunDecodeError(f"base64 decode failed: {exc}") from exc
    if len(data) < 8:
        raise PumpFunDecodeError("payload too short to contain discriminator")
    if data[:8] != CREATE_EVENT_DISCRIMINATOR:
        raise PumpFunDecodeError("discriminator does not match CreateEvent")

    cursor = 8

    def _read_string() -> str:
        nonlocal cursor
        if cursor + 4 > len(data):
            raise PumpFunDecodeError("truncated before string length")
        length = int.from_bytes(data[cursor:cursor + 4], "little")
        cursor += 4
        if cursor + length > len(data):
            raise PumpFunDecodeError(f"truncated string body (need {length})")
        s = data[cursor:cursor + length].decode("utf-8", errors="replace")
        cursor += length
        return s

    def _read_pubkey() -> str:
        nonlocal cursor
        if cursor + 32 > len(data):
            raise PumpFunDecodeError("truncated pubkey")
        pk = data[cursor:cursor + 32]
        cursor += 32
        return base58_encode(pk)

    name = _read_string()
    symbol = _read_string()
    uri = _read_string()
    mint = _read_pubkey()
    bonding_curve = _read_pubkey()
    user = _read_pubkey()
    return PumpFunCreateEvent(
        name=name, symbol=symbol, uri=uri,
        mint=mint, bonding_curve=bonding_curve, user=user,
    )


def decode_create_event_from_logs(logs: Sequence[str]) -> PumpFunCreateEvent | None:
    """Find and decode the first valid CreateEvent in the supplied log lines.

    Returns ``None`` if no candidate payload decodes successfully. Iterates
    every Program data line because pump.fun transactions emit additional
    event types (Buy etc.) in the same tx.
    """
    for payload in _extract_program_data_lines(logs):
        try:
            return _decode_create_event_payload(payload)
        except PumpFunDecodeError:
            continue
    return None

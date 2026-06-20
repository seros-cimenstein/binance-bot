"""Shared signal candidate model for source detectors."""

from __future__ import annotations

import copy
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True, slots=True)
class SignalCandidate:
    """A source signal that is ready for downstream monitoring."""

    source: str
    chain: str
    token_address: str
    token_symbol: str
    detected_at_ms: int
    raw_signal: dict[str, Any]
    confidence: float

    def as_dict(self) -> dict[str, Any]:
        return {
            "source": self.source,
            "chain": self.chain,
            "token_address": self.token_address,
            "token_symbol": self.token_symbol,
            "detected_at_ms": self.detected_at_ms,
            "raw_signal": copy.deepcopy(self.raw_signal),
            "confidence": self.confidence,
        }


def _raise_type_error_on_mutation(
    self: SignalCandidate,
    name: str,
    value: object,
) -> None:
    raise TypeError(f"cannot assign to field {name!r}")


SignalCandidate.__setattr__ = _raise_type_error_on_mutation

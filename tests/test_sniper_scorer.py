"""Tests for the Solana token safety scorer."""

from __future__ import annotations

import base64
import json

import pytest

from src.sniper.scorer import (
    MintInfo,
    SniperScorerError,
    TopHolder,
    TokenSafetyScore,
    _compute_top_concentration,
    decode_account_data,
    parse_mint_account,
    score_safety,
)


# ---------------------------------------------------------------------------
# Fixture builders for SPL Token mint accounts
# ---------------------------------------------------------------------------


def _build_mint_payload(
    *,
    has_mint_authority: bool,
    has_freeze_authority: bool,
    supply: int,
    decimals: int,
    is_initialized: bool = True,
) -> bytes:
    """Construct a valid 82-byte SPL Token v3 mint account payload."""
    # mint authority option (4 bytes flag + 32 bytes pubkey)
    mint_flag = (1 if has_mint_authority else 0).to_bytes(4, "little")
    mint_pk = bytes([1] * 32) if has_mint_authority else bytes(32)
    supply_bytes = supply.to_bytes(8, "little")
    decimals_byte = bytes([decimals])
    init_byte = bytes([1 if is_initialized else 0])
    freeze_flag = (1 if has_freeze_authority else 0).to_bytes(4, "little")
    freeze_pk = bytes([2] * 32) if has_freeze_authority else bytes(32)
    return mint_flag + mint_pk + supply_bytes + decimals_byte + init_byte + freeze_flag + freeze_pk


# ---------------------------------------------------------------------------
# Mint account parsing
# ---------------------------------------------------------------------------


def test_parse_safe_mint() -> None:
    payload = _build_mint_payload(
        has_mint_authority=False, has_freeze_authority=False,
        supply=1_000_000_000_000, decimals=9,
    )
    info = parse_mint_account(payload)
    assert info.has_mint_authority is False
    assert info.has_freeze_authority is False
    assert info.supply == 1_000_000_000_000
    assert info.decimals == 9
    assert info.is_initialized is True


def test_parse_unsafe_mint_with_authorities() -> None:
    payload = _build_mint_payload(
        has_mint_authority=True, has_freeze_authority=True,
        supply=1_000_000_000, decimals=6,
    )
    info = parse_mint_account(payload)
    assert info.has_mint_authority is True
    assert info.has_freeze_authority is True


def test_parse_mint_account_rejects_short_payload() -> None:
    with pytest.raises(SniperScorerError, match="too short"):
        parse_mint_account(b"\x00" * 50)


def test_decode_account_data_extracts_base64() -> None:
    payload = _build_mint_payload(
        has_mint_authority=False, has_freeze_authority=False,
        supply=1000, decimals=6,
    )
    encoded = base64.b64encode(payload).decode()
    raw = decode_account_data({"data": [encoded, "base64"]})
    assert raw == payload


def test_decode_account_data_rejects_unsupported_encoding() -> None:
    with pytest.raises(SniperScorerError, match="unsupported"):
        decode_account_data({"data": ["abc", "base58"]})


def test_decode_account_data_rejects_missing_array() -> None:
    with pytest.raises(SniperScorerError, match="missing data array"):
        decode_account_data({"data": "not-a-list"})


# ---------------------------------------------------------------------------
# Holder concentration math
# ---------------------------------------------------------------------------


def test_concentration_with_no_holders() -> None:
    top, top10, count, holders = _compute_top_concentration([], 1000)
    assert top == 0.0 and top10 == 0.0 and count == 0 and holders == ()


def test_concentration_with_zero_supply() -> None:
    holders = [{"address": "A", "amount": "10"}]
    top, top10, count, parsed = _compute_top_concentration(holders, 0)
    assert top == 0.0 and parsed == ()


def test_concentration_sorts_and_caps_top_20() -> None:
    holders = [{"address": f"acct{i}", "amount": str(i + 1)} for i in range(30)]
    total = sum(i + 1 for i in range(30))
    top, top10, count, parsed = _compute_top_concentration(holders, total)
    assert len(parsed) == 20
    # Top holder has the largest amount (30).
    assert parsed[0].amount == 30
    # Top-10 percentages should sum to the largest-10 sums.
    assert top10 == pytest.approx(sum(i for i in range(21, 31)) / total * 100.0)


# ---------------------------------------------------------------------------
# End-to-end scoring
# ---------------------------------------------------------------------------


def _info(*, mint_auth: bool, freeze_auth: bool, supply: int = 1_000_000,
          decimals: int = 6) -> MintInfo:
    return MintInfo(
        has_mint_authority=mint_auth, has_freeze_authority=freeze_auth,
        supply=supply, decimals=decimals, is_initialized=True,
    )


def test_score_rejects_when_mint_account_missing() -> None:
    s = score_safety("M1", mint_info=None, holders=None)
    assert s.rejection_reason == "mint account not found"
    assert s.score == 0.0
    assert not s.is_tradeable


def test_score_rejects_unrevoked_mint_authority() -> None:
    s = score_safety("M1", mint_info=_info(mint_auth=True, freeze_auth=False), holders=[])
    assert "mint authority" in s.rejection_reason
    assert not s.is_tradeable


def test_score_rejects_unrevoked_freeze_authority() -> None:
    s = score_safety("M1", mint_info=_info(mint_auth=False, freeze_auth=True), holders=[])
    assert "freeze authority" in s.rejection_reason


def test_score_rejects_concentrated_top_holder() -> None:
    holders = [
        {"address": "whale", "amount": "600000"},  # 60% of supply
        {"address": "small", "amount": "1000"},
    ]
    s = score_safety("M1", mint_info=_info(mint_auth=False, freeze_auth=False), holders=holders)
    assert "top holder" in s.rejection_reason


def test_score_rejects_concentrated_top_10() -> None:
    # 12 holders each with 8% (= top-10 sum 80%) but top single is only 8%.
    holders = [{"address": f"h{i}", "amount": "80000"} for i in range(12)]
    s = score_safety("M1", mint_info=_info(mint_auth=False, freeze_auth=False,
                                            supply=1_000_000), holders=holders,
                      max_top_holder_pct=30.0, max_top_10_pct=75.0)
    assert s.rejection_reason is not None and "top-10" in s.rejection_reason


def test_score_passes_with_distributed_supply() -> None:
    # 50 holders each with 2% → top single 2%, top-10 20%.
    holders = [{"address": f"h{i}", "amount": "20000"} for i in range(50)]
    s = score_safety("M1", mint_info=_info(mint_auth=False, freeze_auth=False,
                                            supply=1_000_000), holders=holders)
    assert s.rejection_reason is None
    assert s.score > 0
    assert s.is_tradeable
    assert s.top_holder_pct == pytest.approx(2.0)
    assert s.top_10_pct == pytest.approx(20.0)


def test_score_dataclass_round_trips_as_json() -> None:
    holders = [{"address": f"h{i}", "amount": "20000"} for i in range(50)]
    s = score_safety("M1", mint_info=_info(mint_auth=False, freeze_auth=False,
                                            supply=1_000_000), holders=holders)
    decoded = json.loads(json.dumps(s.as_dict()))
    assert decoded["mint"] == "M1"
    assert decoded["score"] == s.score
    assert len(decoded["top_holders"]) == 20


def test_score_rejects_unreasonable_decimals() -> None:
    s = score_safety("M1", mint_info=_info(mint_auth=False, freeze_auth=False, decimals=99),
                      holders=[])
    assert "decimals" in (s.rejection_reason or "")

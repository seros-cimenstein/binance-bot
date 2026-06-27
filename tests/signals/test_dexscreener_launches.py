from __future__ import annotations

import json
from collections import OrderedDict
from pathlib import Path
from typing import Any

import pytest

from src.signals import dexscreener_launches as launches


NOW_MS = 1_800_000_000_000


def pair_payload(**overrides: Any) -> dict[str, Any]:
    pair: dict[str, Any] = {
        "chainId": "solana",
        "pairAddress": "PAIR123",
        "pairCreatedAt": NOW_MS - 5 * 60 * 1000,
        "baseToken": {
            "address": "TOKEN123",
            "symbol": "MOONX",
            "name": "Moon X",
        },
        "quoteToken": {
            "address": launches.WSOL_MINT,
            "symbol": "SOL",
        },
        "priceUsd": "0.0001",
        "liquidity": {"usd": 20_000},
        "volume": {"m5": 1_000, "h1": 2_000, "h6": 3_000, "h24": 4_000},
        "info": {"websites": [{"url": "https://example.com"}]},
        "url": "https://dexscreener.com/solana/PAIR123",
    }
    for key, value in overrides.items():
        if key == "baseToken":
            pair["baseToken"] = {**pair["baseToken"], **value}
        elif key == "quoteToken":
            pair["quoteToken"] = {**pair["quoteToken"], **value}
        elif key == "liquidity":
            pair["liquidity"] = {**pair["liquidity"], **value}
        elif key == "volume":
            pair["volume"] = {**pair["volume"], **value}
        else:
            pair[key] = value
    return pair


def features_for(pair: dict[str, Any]) -> dict[str, Any]:
    return launches.extract_launch_features(pair, now_ms=NOW_MS)


def settings(tmp_path: Path | None = None) -> launches.LaunchDetectorSettings:
    return launches.LaunchDetectorSettings(
        ledger_path=str(tmp_path / "ledger.jsonl") if tmp_path else "unused.jsonl",
        filter_version="test-filter",
    )


def test_filter_accepts_clean_launch_payload() -> None:
    accepted, reasons = launches.qualifies_launch_pair(
        features_for(pair_payload()),
        settings=settings(),
    )

    assert accepted is True
    assert reasons == []


@pytest.mark.parametrize(
    ("pair", "reason"),
    [
        (pair_payload(pairCreatedAt=NOW_MS - 31 * 60 * 1000), "pair_age_out_of_range"),
        (pair_payload(liquidity={"usd": 2_999}), "liquidity_too_low"),
        (pair_payload(liquidity={"usd": 200_001}), "liquidity_too_high"),
        (pair_payload(info={}), "missing_social_link"),
        (pair_payload(quoteToken={"address": "USDC", "symbol": "USDC"}), "non_wsol_quote"),
        (pair_payload(chainId="ethereum"), "non_solana_chain"),
        (pair_payload(baseToken={"symbol": "", "name": "Moon X"}), "scam_text_pattern"),
        (pair_payload(baseToken={"symbol": "TOOLONGSYMBOL", "name": "Moon X"}), "scam_text_pattern"),
        (pair_payload(baseToken={"symbol": "BTC$", "name": "Moon X"}), "scam_text_pattern"),
        (pair_payload(volume={"m5": 500_000}), "wash_volume_spike"),
    ],
)
def test_filter_rejects_bad_launch_payloads(pair: dict[str, Any], reason: str) -> None:
    accepted, reasons = launches.qualifies_launch_pair(features_for(pair), settings=settings())

    assert accepted is False
    assert reason in reasons


@pytest.mark.asyncio
async def test_dedup_same_token_seen_twice_in_cycle_emits_one_detection(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def fake_profiles(_session: object) -> list[dict[str, str]]:
        return [
            {"chainId": "solana", "tokenAddress": "TOKEN123"},
            {"chainId": "solana", "tokenAddress": "TOKEN123"},
        ]

    calls = 0

    async def fake_pairs(_session: object, token_address: str) -> list[dict[str, Any]]:
        nonlocal calls
        calls += 1
        assert token_address == "TOKEN123"
        return [pair_payload()]

    monkeypatch.setattr(launches, "fetch_latest_profiles", fake_profiles)
    monkeypatch.setattr(launches, "fetch_token_pairs", fake_pairs)
    monkeypatch.setattr(launches, "utc_ms", lambda: NOW_MS)

    detections = await launches.run_cycle(
        session=object(),  # type: ignore[arg-type]
        settings=settings(tmp_path),
        seen=OrderedDict(),
    )

    assert calls == 1
    assert len(detections) == 1
    rows = (tmp_path / "ledger.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(rows) == 1


def test_detection_schema_uses_expected_id_and_filter_version() -> None:
    pair = pair_payload()
    features = features_for(pair)
    record = launches.build_detection_record(
        pair,
        features,
        detected_at_ms=NOW_MS,
        filter_version="custom-filter",
    )

    assert record["detection_id"] == f"v2-solana-PAIR123-{NOW_MS}"
    assert record["filter_version"] == "custom-filter"
    assert record["creator_recent_rug"] == "unknown"
    assert record["features"]["price_usd"] == 0.0001
    assert json.dumps(record)

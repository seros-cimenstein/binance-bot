from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from src.signals import snapshotter


NOW_MS = 1_800_000_000_000


def detection(
    detection_id: str = "d1",
    *,
    detected_at_ms: int = NOW_MS - 3_600_000,
    token_address: str = "TOKEN1",
    pair_address: str = "PAIR1",
) -> dict[str, Any]:
    return {
        "detection_id": detection_id,
        "detected_at_ms": detected_at_ms,
        "token_address": token_address,
        "pair_address": pair_address,
    }


def pair(
    *,
    token_address: str = "TOKEN1",
    pair_address: str = "PAIR1",
    chain: str = "solana",
) -> dict[str, Any]:
    return {
        "chainId": chain,
        "pairAddress": pair_address,
        "baseToken": {"address": token_address},
        "priceUsd": "2.5",
        "liquidity": {"usd": "15000"},
        "volume": {"m5": 10, "h1": 20, "h6": 30, "h24": 40},
        "fdv": 100_000,
        "marketCap": 90_000,
        "url": "https://dexscreener.com/solana/PAIR1",
    }


def write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.write_text(
        "".join(json.dumps(row, sort_keys=True) + "\n" for row in rows),
        encoding="utf-8",
    )


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def test_computes_due_offsets_relative_to_now() -> None:
    detections = [detection(detected_at_ms=NOW_MS - 3_600_000)]

    due = snapshotter.compute_due_snapshots(detections, [], now_ms=NOW_MS)

    assert [item.scheduled_offset_seconds for item in due] == [300, 900, 3600]


def test_skips_already_captured_detection_offset_pairs() -> None:
    detections = [detection(detected_at_ms=NOW_MS - 3_600_000)]
    snapshots = [{"detection_id": "d1", "scheduled_offset_seconds": 300, "status": "ok"}]

    due = snapshotter.compute_due_snapshots(detections, snapshots, now_ms=NOW_MS)

    assert [item.scheduled_offset_seconds for item in due] == [900, 3600]


def test_api_error_snapshot_does_not_count_as_captured() -> None:
    detections = [detection(detected_at_ms=NOW_MS - 300_000)]
    snapshots = [{"detection_id": "d1", "scheduled_offset_seconds": 300, "status": "api_error"}]

    due = snapshotter.compute_due_snapshots(detections, snapshots, now_ms=NOW_MS)

    assert [item.scheduled_offset_seconds for item in due] == [300]


def test_handles_pair_gone() -> None:
    due = snapshotter.DueSnapshot("d1", "TOKEN1", "PAIR1", 300, NOW_MS)

    record = snapshotter.build_snapshot_record(due, [], captured_at_ms=NOW_MS + 1)

    assert record["pair_exists"] is False
    assert record["status"] == "pair_gone"
    assert record["price_usd"] is None
    assert record["liquidity_usd"] is None


@pytest.mark.asyncio
async def test_per_run_cap_honored(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    ledger_path = tmp_path / "ledger.jsonl"
    snapshots_path = tmp_path / "snapshots.jsonl"
    write_jsonl(
        ledger_path,
        [
            detection(f"d{i}", token_address=f"TOKEN{i}", pair_address=f"PAIR{i}")
            for i in range(3)
        ],
    )

    async def fake_batch(_session: object, token_addresses: list[str]) -> dict[str, list[dict[str, Any]]]:
        return {token: [pair(token_address=token, pair_address=f"PAIR{token.removeprefix('TOKEN')}")] for token in token_addresses}

    monkeypatch.setattr(snapshotter, "fetch_token_pairs_batch", fake_batch)
    settings = snapshotter.SnapshotterSettings(
        ledger_path=str(ledger_path),
        snapshots_path=str(snapshots_path),
        max_snapshots_per_run=2,
    )

    written = await snapshotter.run_snapshotter_once(settings=settings, now_ms=NOW_MS)

    assert written == 2
    assert len(read_jsonl(snapshots_path)) == 2


@pytest.mark.asyncio
async def test_idempotent_second_run_writes_no_new_rows(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    ledger_path = tmp_path / "ledger.jsonl"
    snapshots_path = tmp_path / "snapshots.jsonl"
    write_jsonl(ledger_path, [detection(detected_at_ms=NOW_MS - 300_000)])

    async def fake_batch(_session: object, token_addresses: list[str]) -> dict[str, list[dict[str, Any]]]:
        assert token_addresses == ["TOKEN1"]
        return {"TOKEN1": [pair()]}

    monkeypatch.setattr(snapshotter, "fetch_token_pairs_batch", fake_batch)
    settings = snapshotter.SnapshotterSettings(
        ledger_path=str(ledger_path),
        snapshots_path=str(snapshots_path),
    )

    first = await snapshotter.run_snapshotter_once(settings=settings, now_ms=NOW_MS)
    second = await snapshotter.run_snapshotter_once(settings=settings, now_ms=NOW_MS)

    assert first == 1
    assert second == 0
    assert len(read_jsonl(snapshots_path)) == 1


@pytest.mark.asyncio
async def test_api_error_is_written_but_retried_next_run(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    ledger_path = tmp_path / "ledger.jsonl"
    snapshots_path = tmp_path / "snapshots.jsonl"
    write_jsonl(ledger_path, [detection(detected_at_ms=NOW_MS - 300_000)])

    async def failing_batch(_session: object, _token_addresses: list[str]) -> dict[str, list[dict[str, Any]]]:
        raise RuntimeError("boom")

    monkeypatch.setattr(snapshotter, "fetch_token_pairs_batch", failing_batch)
    settings = snapshotter.SnapshotterSettings(
        ledger_path=str(ledger_path),
        snapshots_path=str(snapshots_path),
    )

    written = await snapshotter.run_snapshotter_once(settings=settings, now_ms=NOW_MS)
    due_after = snapshotter.compute_due_snapshots(
        snapshotter.load_detections(ledger_path),
        snapshotter.load_snapshots(snapshots_path),
        now_ms=NOW_MS,
    )

    assert written == 1
    assert read_jsonl(snapshots_path)[0]["status"] == "api_error"
    assert [item.scheduled_offset_seconds for item in due_after] == [300]

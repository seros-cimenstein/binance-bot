"""V2 post-detection snapshot collection."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import aiohttp
from loguru import logger

from src.signals.common import append_ledger, utc_ms
TOKEN_PAIRS_URL = "https://api.dexscreener.com/latest/dex/tokens/{token_addresses}"
SNAPSHOT_OFFSETS_SECONDS: tuple[int, ...] = (
    5 * 60,
    15 * 60,
    60 * 60,
    6 * 60 * 60,
    24 * 60 * 60,
    3 * 24 * 60 * 60,
    7 * 24 * 60 * 60,
)


@dataclass(frozen=True, slots=True)
class SnapshotterSettings:
    """Configuration for V2 forward snapshots."""

    ledger_path: str = "data/signals-v2-ledger.jsonl"
    snapshots_path: str = "data/signals-v2-snapshots.jsonl"
    max_snapshots_per_run: int = 500
    request_timeout_seconds: float = 10.0
    snapshot_offsets_seconds: tuple[int, ...] = SNAPSHOT_OFFSETS_SECONDS


@dataclass(frozen=True, slots=True)
class DueSnapshot:
    """A scheduled snapshot that has not been captured yet."""

    detection_id: str
    token_address: str
    pair_address: str
    scheduled_offset_seconds: int
    scheduled_for_ms: int


def settings_from_env() -> SnapshotterSettings:
    """Build snapshotter settings from environment variables."""

    return SnapshotterSettings(
        ledger_path=os.getenv("SIGNALS_V2_LEDGER_PATH", "data/signals-v2-ledger.jsonl"),
        snapshots_path=os.getenv(
            "SIGNALS_V2_SNAPSHOTS_PATH",
            "data/signals-v2-snapshots.jsonl",
        ),
        max_snapshots_per_run=_env_int("SIGNALS_V2_SNAPSHOTTER_MAX_PER_RUN", 500),
        snapshot_offsets_seconds=_env_offsets(
            "SIGNALS_V2_SNAPSHOT_OFFSETS_SECONDS",
            SNAPSHOT_OFFSETS_SECONDS,
        ),
    )


def load_detections(path: str | Path) -> list[dict[str, Any]]:
    """Load V2 detection ledger rows."""

    return _load_jsonl(path)


def load_snapshots(path: str | Path) -> list[dict[str, Any]]:
    """Load V2 snapshot rows."""

    return _load_jsonl(path)


def compute_due_snapshots(
    detections: list[dict[str, Any]],
    snapshots: list[dict[str, Any]],
    *,
    now_ms: int,
    offsets_seconds: Iterable[int] = SNAPSHOT_OFFSETS_SECONDS,
) -> list[DueSnapshot]:
    """Find due snapshots that do not already have a persisted row."""

    captured = {
        (str(snapshot.get("detection_id", "")), _as_int(snapshot.get("scheduled_offset_seconds")))
        for snapshot in snapshots
        if str(snapshot.get("status", "")) != "api_error"
    }
    due: list[DueSnapshot] = []
    for detection in detections:
        detection_id = str(detection.get("detection_id", ""))
        token_address = str(detection.get("token_address", ""))
        pair_address = str(detection.get("pair_address", ""))
        detected_at_ms = _as_int(detection.get("detected_at_ms"))
        if not detection_id or not token_address or detected_at_ms <= 0:
            continue
        for offset in offsets_seconds:
            scheduled_for_ms = detected_at_ms + int(offset) * 1000
            key = (detection_id, int(offset))
            if now_ms >= scheduled_for_ms and key not in captured:
                due.append(DueSnapshot(
                    detection_id=detection_id,
                    token_address=token_address,
                    pair_address=pair_address,
                    scheduled_offset_seconds=int(offset),
                    scheduled_for_ms=scheduled_for_ms,
                ))
    return due


async def fetch_pair_snapshot(
    session: aiohttp.ClientSession,
    due: DueSnapshot,
) -> dict[str, Any]:
    """Fetch current market state for a scheduled snapshot."""

    pairs_by_token = await fetch_token_pairs_batch(session, [due.token_address])
    return build_snapshot_record(due, pairs_by_token.get(due.token_address, []), captured_at_ms=utc_ms())


async def fetch_token_pairs_batch(
    session: aiohttp.ClientSession,
    token_addresses: list[str],
) -> dict[str, list[dict[str, Any]]]:
    """Fetch DexScreener pairs for up to 30 token addresses."""

    if not token_addresses:
        return {}
    url = TOKEN_PAIRS_URL.format(token_addresses=",".join(token_addresses))
    data = await _get_json(session, url)
    if not isinstance(data, dict):
        raise RuntimeError("DexScreener token pairs response was not an object")
    pairs = data.get("pairs", [])
    if not isinstance(pairs, list):
        raise RuntimeError("DexScreener token pairs list malformed")
    grouped: dict[str, list[dict[str, Any]]] = {address: [] for address in token_addresses}
    requested = set(token_addresses)
    for pair in pairs:
        if not isinstance(pair, dict):
            continue
        base_token = pair.get("baseToken", {})
        if not isinstance(base_token, dict):
            continue
        token_address = str(base_token.get("address", ""))
        if token_address in requested:
            grouped.setdefault(token_address, []).append(pair)
    return grouped


def build_snapshot_record(
    due: DueSnapshot,
    pairs: list[dict[str, Any]],
    *,
    captured_at_ms: int,
) -> dict[str, Any]:
    pair = _select_snapshot_pair(pairs, due.pair_address)
    if pair is None:
        return {
            "detection_id": due.detection_id,
            "scheduled_offset_seconds": due.scheduled_offset_seconds,
            "scheduled_for_ms": due.scheduled_for_ms,
            "captured_at_ms": captured_at_ms,
            "status": "pair_gone",
            "pair_exists": False,
            "price_usd": None,
            "liquidity_usd": None,
            "volume": {"m5": None, "h1": None, "h6": None, "h24": None},
            "fdv": None,
            "market_cap": None,
            "pair_url": None,
            "raw_pair": None,
        }
    return {
        "detection_id": due.detection_id,
        "scheduled_offset_seconds": due.scheduled_offset_seconds,
        "scheduled_for_ms": due.scheduled_for_ms,
        "captured_at_ms": captured_at_ms,
        "status": "ok",
        "pair_exists": True,
        "price_usd": _as_float_or_none(pair.get("priceUsd")),
        "liquidity_usd": _as_float_or_none(_nested(pair, "liquidity", "usd")),
        "volume": {
            "m5": _as_float_or_none(_nested(pair, "volume", "m5")),
            "h1": _as_float_or_none(_nested(pair, "volume", "h1")),
            "h6": _as_float_or_none(_nested(pair, "volume", "h6")),
            "h24": _as_float_or_none(_nested(pair, "volume", "h24")),
        },
        "fdv": _as_float_or_none(pair.get("fdv")),
        "market_cap": _as_float_or_none(pair.get("marketCap")),
        "pair_url": pair.get("url"),
        "raw_pair": pair,
    }


def build_api_error_snapshot(
    due: DueSnapshot,
    *,
    captured_at_ms: int,
    error: str,
) -> dict[str, Any]:
    return {
        "detection_id": due.detection_id,
        "scheduled_offset_seconds": due.scheduled_offset_seconds,
        "scheduled_for_ms": due.scheduled_for_ms,
        "captured_at_ms": captured_at_ms,
        "status": "api_error",
        "error": error,
        "pair_exists": None,
        "price_usd": None,
        "liquidity_usd": None,
        "volume": {"m5": None, "h1": None, "h6": None, "h24": None},
        "raw_pair": None,
    }


def append_snapshot(path: str | Path, snapshot: dict[str, Any]) -> None:
    """Append one snapshot row to the V2 snapshots JSONL file."""

    append_ledger(path, snapshot)


async def run_snapshotter_once(
    *,
    settings: SnapshotterSettings | None = None,
    now_ms: int | None = None,
) -> int:
    """Capture currently due snapshots and return the number written."""

    if settings is None:
        settings = settings_from_env()
    if now_ms is None:
        now_ms = utc_ms()

    detections = load_detections(settings.ledger_path)
    snapshots = load_snapshots(settings.snapshots_path)
    due = compute_due_snapshots(
        detections,
        snapshots,
        now_ms=now_ms,
        offsets_seconds=settings.snapshot_offsets_seconds,
    )
    if len(due) > settings.max_snapshots_per_run:
        logger.warning(
            "V2 snapshotter due backlog exceeds per-run cap: "
            f"{len(due)} due, processing {settings.max_snapshots_per_run}"
        )
    due = due[:settings.max_snapshots_per_run]
    if not due:
        logger.info("V2 snapshotter found no due snapshots")
        return 0

    timeout = aiohttp.ClientTimeout(total=settings.request_timeout_seconds)
    written = 0
    async with aiohttp.ClientSession(timeout=timeout) as session:
        for index, batch in enumerate(_chunks(due, 30)):
            token_addresses = sorted({item.token_address for item in batch})
            captured_at_ms = utc_ms()
            try:
                pairs_by_token = await fetch_token_pairs_batch(session, token_addresses)
                records = [
                    build_snapshot_record(
                        item,
                        pairs_by_token.get(item.token_address, []),
                        captured_at_ms=captured_at_ms,
                    )
                    for item in batch
                ]
            except Exception as exc:  # noqa: BLE001 - record retryable API failures
                logger.warning(f"V2 snapshotter batch fetch failed: {exc}")
                records = [
                    build_api_error_snapshot(
                        item,
                        captured_at_ms=captured_at_ms,
                        error=str(exc),
                    )
                    for item in batch
                ]
            for record in records:
                append_snapshot(settings.snapshots_path, record)
                written += 1
            if index < (len(due) - 1) // 30:
                await asyncio.sleep(1)
    logger.info(f"V2 snapshotter wrote {written} snapshot rows")
    return written


def run(argv: list[str] | None = None) -> int:
    """CLI entry point for the snapshotter cron command."""

    parser = argparse.ArgumentParser(description="Capture due V2 signal snapshots.")
    parser.add_argument("--once", action="store_true", help="Accepted for cron readability.")
    parser.parse_args(argv)
    asyncio.run(run_snapshotter_once())
    return 0


def main() -> None:
    raise SystemExit(run(sys.argv[1:]))


async def _get_json(session: aiohttp.ClientSession, url: str) -> Any:
    async with session.get(url) as resp:
        resp.raise_for_status()
        return await resp.json()


def _select_snapshot_pair(
    pairs: list[dict[str, Any]],
    pair_address: str,
) -> dict[str, Any] | None:
    solana_pairs = [
        pair for pair in pairs
        if isinstance(pair, dict) and str(pair.get("chainId", "")).lower() == "solana"
    ]
    if not solana_pairs:
        return None
    for pair in solana_pairs:
        if str(pair.get("pairAddress", "")) == pair_address:
            return pair
    return max(solana_pairs, key=lambda pair: _as_float(_nested(pair, "liquidity", "usd")))


def _load_jsonl(path: str | Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        lines = Path(path).read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    except OSError as exc:
        logger.warning(f"Unable to read {path}: {exc}")
        return []
    for line in lines:
        if not line.strip():
            continue
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            logger.warning(f"Skipping malformed JSONL row in {path}")
            continue
        if isinstance(item, dict):
            rows.append(item)
    return rows


def _chunks(items: list[DueSnapshot], size: int) -> Iterable[list[DueSnapshot]]:
    for index in range(0, len(items), size):
        yield items[index:index + size]


def _nested(data: dict[str, Any], key: str, subkey: str) -> Any:
    item = data.get(key, {})
    if not isinstance(item, dict):
        return None
    return item.get(subkey)


def _as_float(value: Any) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return 0.0


def _as_float_or_none(value: Any) -> float | None:
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def _as_int(value: Any) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def _env_int(key: str, default: int) -> int:
    value = os.getenv(key)
    if value is None:
        return default
    try:
        return int(value)
    except ValueError:
        logger.warning(f"Ignoring invalid integer env {key}={value!r}")
        return default


def _env_offsets(key: str, default: tuple[int, ...]) -> tuple[int, ...]:
    value = os.getenv(key)
    if value is None:
        return default
    offsets: list[int] = []
    for part in value.split(","):
        try:
            offsets.append(int(part.strip()))
        except ValueError:
            logger.warning(f"Ignoring invalid snapshot offset {part!r}")
    return tuple(offsets) if offsets else default


if __name__ == "__main__":
    main()

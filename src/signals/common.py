"""Shared utilities for signal detectors."""

from __future__ import annotations

import json
import os
from collections import OrderedDict
from collections.abc import Iterable
from datetime import datetime, timezone
from pathlib import Path


def utc_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def append_ledger(path: str | Path, record: dict) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(record, sort_keys=True))
        fh.write("\n")


def atomic_write_json(path: str | Path, payload: object) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2, sort_keys=True), encoding="utf-8")
    os.replace(tmp, p)


def emergency_stop_active(path: str | Path) -> bool:
    return Path(path).exists()


def dedupe_seen_addresses(
    seen: OrderedDict[str, None],
    new: Iterable[str],
) -> list[str]:
    fresh: list[str] = []
    for address in new:
        if address in seen:
            continue
        seen[address] = None
        fresh.append(address)
        while len(seen) > 10_000:
            seen.popitem(last=False)
    return fresh

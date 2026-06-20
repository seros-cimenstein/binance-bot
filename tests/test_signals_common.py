"""Tests for shared signal helpers."""

from __future__ import annotations

import json
from collections import OrderedDict
from pathlib import Path

from src.signals.common import (
    append_ledger,
    atomic_write_json,
    dedupe_seen_addresses,
    emergency_stop_active,
    utc_ms,
)


def test_utc_ms_returns_millisecond_timestamp() -> None:
    before = utc_ms()
    after = utc_ms()
    assert isinstance(before, int)
    assert before <= after
    assert before > 1_700_000_000_000


def test_append_ledger_writes_one_json_line_per_record(tmp_path: Path) -> None:
    path = tmp_path / "nested" / "ledger.jsonl"
    append_ledger(path, {"b": 2, "a": 1})
    append_ledger(path, {"c": 3})
    lines = path.read_text(encoding="utf-8").splitlines()
    assert [json.loads(line) for line in lines] == [{"a": 1, "b": 2}, {"c": 3}]


def test_atomic_write_json_replaces_file(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    atomic_write_json(path, {"old": True})
    atomic_write_json(path, {"new": [1, 2, 3]})
    assert json.loads(path.read_text(encoding="utf-8")) == {"new": [1, 2, 3]}
    assert not path.with_suffix(".json.tmp").exists()


def test_emergency_stop_active(tmp_path: Path) -> None:
    path = tmp_path / "stop"
    assert emergency_stop_active(path) is False
    path.touch()
    assert emergency_stop_active(path) is True


def test_dedupe_seen_addresses_mutates_seen_and_trims_oldest() -> None:
    seen: OrderedDict[str, None] = OrderedDict((f"old-{i}", None) for i in range(9_999))
    fresh = dedupe_seen_addresses(seen, ["old-1", "new-1", "new-2"])
    assert fresh == ["new-1", "new-2"]
    assert "new-1" in seen
    assert "new-2" in seen
    assert len(seen) == 10_000
    assert "old-0" not in seen

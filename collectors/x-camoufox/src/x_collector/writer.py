"""Append-only JSONL output with fsync, tail-restored dedupe, atomic JSON."""

from __future__ import annotations

import json
import os
from collections import deque
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

TAIL_BYTES = 8 * 1024 * 1024
MAX_REMEMBERED_KEYS = 250_000

KeyFn = Callable[[dict[str, Any]], str | None]


def dumps_row(row: dict[str, Any]) -> str:
    """One key-sorted compact JSON object (no trailing newline)."""
    return json.dumps(row, sort_keys=True, ensure_ascii=False, separators=(",", ":"))


def read_tail_rows(path: Path, tail_bytes: int = TAIL_BYTES) -> list[dict[str, Any]]:
    """Parse the complete JSON lines in the last `tail_bytes` of `path`."""
    try:
        with path.open("rb") as handle:
            size = handle.seek(0, os.SEEK_END)
            start = max(0, size - tail_bytes)
            handle.seek(start)
            chunk = handle.read()
    except FileNotFoundError:
        return []
    lines = chunk.split(b"\n")
    if start > 0:
        lines = lines[1:]  # first line is cut mid-object
    rows: list[dict[str, Any]] = []
    for line in lines:
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            continue
        if isinstance(value, dict):
            rows.append(value)
    return rows


class SeenKeys:
    """Bounded insertion-ordered set (oldest keys forgotten first)."""

    def __init__(self, capacity: int = MAX_REMEMBERED_KEYS) -> None:
        self._capacity = capacity
        self._order: deque[str] = deque()
        self._keys: set[str] = set()

    def __contains__(self, key: str) -> bool:
        return key in self._keys

    def __len__(self) -> int:
        return len(self._keys)

    def add(self, key: str) -> None:
        if key in self._keys:
            return
        self._keys.add(key)
        self._order.append(key)
        while len(self._order) > self._capacity:
            self._keys.discard(self._order.popleft())


class JsonlWriter:
    """Append-only JSONL file. With `key_fn`, rows whose key was already
    written (in this run or in the file tail at startup) are dropped."""

    def __init__(self, path: Path, key_fn: KeyFn | None = None, tail_bytes: int = TAIL_BYTES) -> None:
        self.path = path
        self._key_fn = key_fn
        self._seen = SeenKeys()
        if key_fn is not None:
            for row in read_tail_rows(path, tail_bytes):
                key = key_fn(row)
                if key:
                    self._seen.add(key)

    def is_new(self, row: dict[str, Any]) -> bool:
        if self._key_fn is None:
            return True
        key = self._key_fn(row)
        return bool(key) and key not in self._seen

    def append(self, rows: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
        """Write new rows, fsync, and return the rows actually written."""
        fresh: list[dict[str, Any]] = []
        batch_keys: set[str] = set()
        for row in rows:
            if self._key_fn is not None:
                key = self._key_fn(row)
                if not key or key in self._seen or key in batch_keys:
                    continue
                batch_keys.add(key)
            fresh.append(row)
        if not fresh:
            return []
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = "".join(dumps_row(row) + "\n" for row in fresh).encode("utf-8")
        with self.path.open("ab+") as handle:
            # Repair a torn last line from a crash so the new rows stay parseable.
            size = handle.seek(0, os.SEEK_END)
            if size > 0:
                handle.seek(size - 1)
                if handle.read(1) != b"\n":
                    payload = b"\n" + payload
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        for key in batch_keys:
            self._seen.add(key)
        return fresh


def post_key(row: dict[str, Any]) -> str | None:
    value = row.get("post_id")
    return value if isinstance(value, str) and value else None


def reply_key(row: dict[str, Any]) -> str | None:
    parent, post = row.get("parent_post_id"), row.get("post_id")
    if isinstance(parent, str) and isinstance(post, str) and parent and post:
        return f"{parent}/{post}"
    return None


def atomic_write_json(path: Path, value: Any) -> None:
    """Write `value` to a temp file in the same dir, fsync, then rename over `path`."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    with tmp.open("w", encoding="utf-8") as handle:
        handle.write(json.dumps(value, sort_keys=True, ensure_ascii=False, indent=2) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp, path)
    try:
        dir_fd = os.open(path.parent, os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)

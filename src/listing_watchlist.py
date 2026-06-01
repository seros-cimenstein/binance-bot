"""Shared watchlist of upcoming Binance spot listings.

The ``listing_detector`` polls Binance's announcement API and appends one
entry per fresh ``spot_list`` event to the watchlist. The ``listing_sniper``
reads the watchlist each cycle and arms the corresponding symbols so it
fires the moment the pair appears in ``exchangeInfo`` as TRADING —
bypassing the bootstrap "already known" filter that would otherwise
suppress the snipe.

The watchlist file is a single JSON document:

    {"watching": [
        {"token": "AIGENSYN",
         "article_id": 274005,
         "announced_at_iso": "2026-05-14T10:00:03+00:00",
         "expires_at_iso":   "2026-05-21T10:00:03+00:00"},
        ...
    ]}

Each entry expires after ``DEFAULT_EXPIRY_DAYS`` so stale announcements
that never went live don't pollute the file forever. Pruning happens
opportunistically each time the watchlist is loaded.
"""

from __future__ import annotations

import json
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


DEFAULT_EXPIRY_DAYS = 7
DEFAULT_PATH = os.path.expanduser(
    "~/.local/share/binance-bot/data/listing-watchlist.json"
)


class WatchlistError(RuntimeError):
    """Raised when the watchlist file is malformed."""


@dataclass(frozen=True, slots=True)
class WatchEntry:
    token: str
    article_id: int
    announced_at_iso: str
    expires_at_iso: str

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "WatchEntry":
        try:
            return cls(
                token=str(payload["token"]),
                article_id=int(payload["article_id"]),
                announced_at_iso=str(payload["announced_at_iso"]),
                expires_at_iso=str(payload["expires_at_iso"]),
            )
        except KeyError as exc:
            raise WatchlistError(f"watch entry missing field: {exc}") from exc

    def is_expired(self, *, now: datetime) -> bool:
        try:
            expires = datetime.fromisoformat(self.expires_at_iso)
        except ValueError:
            return False
        return now >= expires


@dataclass(slots=True)
class Watchlist:
    entries: list[WatchEntry] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {"watching": [e.to_dict() for e in self.entries]}

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "Watchlist":
        raw = payload.get("watching") if isinstance(payload, dict) else None
        if not isinstance(raw, list):
            return cls()
        entries = [WatchEntry.from_dict(e) for e in raw if isinstance(e, dict)]
        return cls(entries=entries)

    def article_ids(self) -> set[int]:
        return {e.article_id for e in self.entries}

    def add(self, entry: WatchEntry) -> bool:
        """Append ``entry`` if its ``article_id`` isn't already present.
        Returns ``True`` if added, ``False`` if it was a duplicate."""
        if entry.article_id in self.article_ids():
            return False
        self.entries.append(entry)
        return True

    def remove_token(self, token: str) -> int:
        """Drop every entry matching ``token``. Returns the count removed."""
        before = len(self.entries)
        self.entries = [e for e in self.entries if e.token != token]
        return before - len(self.entries)

    def prune_expired(self, *, now: datetime | None = None) -> int:
        """Drop expired entries. Returns the count removed."""
        n = now or datetime.now(tz=timezone.utc)
        before = len(self.entries)
        self.entries = [e for e in self.entries if not e.is_expired(now=n)]
        return before - len(self.entries)


def build_watch_entry(
    *, token: str, article_id: int,
    announced_at_ms: int, expiry_days: int = DEFAULT_EXPIRY_DAYS,
) -> WatchEntry:
    announced = datetime.fromtimestamp(announced_at_ms / 1000.0, tz=timezone.utc)
    expires = announced + timedelta(days=expiry_days)
    return WatchEntry(
        token=token,
        article_id=article_id,
        announced_at_iso=announced.isoformat(),
        expires_at_iso=expires.isoformat(),
    )


def load_watchlist(
    path: Path | str | None = None, *, prune: bool = True,
    now: datetime | None = None,
) -> Watchlist:
    p = Path(path) if path is not None else Path(DEFAULT_PATH)
    if not p.exists():
        return Watchlist()
    try:
        wl = Watchlist.from_dict(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise WatchlistError(f"could not read watchlist at {p}: {exc}") from exc
    if prune:
        wl.prune_expired(now=now)
    return wl


def save_watchlist(watchlist: Watchlist, path: Path | str | None = None) -> None:
    p = Path(path) if path is not None else Path(DEFAULT_PATH)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(watchlist.to_dict(), indent=2, sort_keys=True))
    tmp.replace(p)

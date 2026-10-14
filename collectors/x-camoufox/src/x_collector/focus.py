"""`x-focus.json` (x-focus-v1, engine-written) and the focus work it asks for.

- `resample_post_ids`: open the status page, write an `x-engagement` row.
- `reply_sample_post_ids`: newest replies (TweetDetail) and quotes
  (`quoted_tweet_id:<id>` search) as `x-reply` rows, at most 20 per parent.
- `coins[].search_queries`: Latest-tab searches written as `focus_search` posts,
  every `X_COLLECTOR_FOCUS_POLL_SECONDS` (jittered).

The engine stops listing an id once a newer row exists for it, so an id is due
again only when a focus file generated after our last sample still lists it.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

FOCUS_SCHEMA = "x-focus-v1"
MAX_RESAMPLE_IDS = 40
MAX_REPLY_SAMPLE_IDS = 6
MAX_REPLIES_PER_PARENT = 20
# A focus file older than this is ignored (engine down): no work from it.
FOCUS_STALE_MS = 15 * 60_000
# Never re-read the same post more often than this, whatever the file says.
MIN_RESAMPLE_GAP_MS = 60_000
MIN_REPLY_SAMPLE_GAP_MS = 5 * 60_000


@dataclass(frozen=True)
class FocusCoin:
    token_key: str
    tier: str
    cashtag: str | None
    search_queries: tuple[str, ...]


@dataclass(frozen=True)
class Focus:
    generated_at_ms: int
    coins: tuple[FocusCoin, ...] = ()
    resample_post_ids: tuple[str, ...] = ()
    reply_sample_post_ids: tuple[str, ...] = ()

    @property
    def search_queries(self) -> list[str]:
        return list(dict.fromkeys(query for coin in self.coins for query in coin.search_queries))


def _ids(value: Any, limit: int) -> tuple[str, ...]:
    if not isinstance(value, list):
        return ()
    ids = [item for item in value if isinstance(item, str) and item.isdigit()]
    return tuple(dict.fromkeys(ids))[:limit]


def parse_focus(value: Any) -> Focus | None:
    """Lenient parse; None when the object is not an x-focus-v1 document."""
    if not isinstance(value, dict) or value.get("schema") != FOCUS_SCHEMA:
        return None
    generated = value.get("generated_at_ms")
    if not isinstance(generated, int) or isinstance(generated, bool):
        return None
    coins: list[FocusCoin] = []
    for raw in value.get("coins") or []:
        if not isinstance(raw, dict):
            continue
        queries = tuple(
            query.strip() for query in raw.get("search_queries") or [] if isinstance(query, str) and query.strip()
        )
        cashtag = raw.get("cashtag")
        coins.append(
            FocusCoin(
                token_key=str(raw.get("token_key") or ""),
                tier=str(raw.get("tier") or ""),
                cashtag=cashtag if isinstance(cashtag, str) else None,
                search_queries=queries,
            )
        )
    return Focus(
        generated_at_ms=generated,
        coins=tuple(coins),
        resample_post_ids=_ids(value.get("resample_post_ids"), MAX_RESAMPLE_IDS),
        reply_sample_post_ids=_ids(value.get("reply_sample_post_ids"), MAX_REPLY_SAMPLE_IDS),
    )


def load_focus(path: Path) -> Focus | None:
    try:
        return parse_focus(json.loads(path.read_text(encoding="utf-8")))
    except (FileNotFoundError, ValueError, UnicodeDecodeError):
        return None


def quote_search_query(post_id: str) -> str:
    return f"quoted_tweet_id:{post_id}"


@dataclass
class FocusTracker:
    """Remembers when focus work was last done, to decide what is due."""

    last_resample_ms: dict[str, int] = field(default_factory=dict)
    last_reply_sample_ms: dict[str, int] = field(default_factory=dict)
    next_search_ms: dict[str, int] = field(default_factory=dict)

    @staticmethod
    def active(focus: Focus | None, now_ms: int) -> Focus | None:
        if focus is None or now_ms - focus.generated_at_ms > FOCUS_STALE_MS:
            return None
        return focus

    @staticmethod
    def _due(listed: tuple[str, ...], last: dict[str, int], focus: Focus, now_ms: int, gap_ms: int) -> list[str]:
        due: list[str] = []
        for post_id in listed:
            done = last.get(post_id)
            if done is None or (done < focus.generated_at_ms and now_ms - done >= gap_ms):
                due.append(post_id)
        return due

    def due_resamples(self, focus: Focus | None, now_ms: int) -> list[str]:
        focus = self.active(focus, now_ms)
        if focus is None:
            return []
        return self._due(focus.resample_post_ids, self.last_resample_ms, focus, now_ms, MIN_RESAMPLE_GAP_MS)

    def due_reply_samples(self, focus: Focus | None, now_ms: int) -> list[str]:
        focus = self.active(focus, now_ms)
        if focus is None:
            return []
        return self._due(focus.reply_sample_post_ids, self.last_reply_sample_ms, focus, now_ms, MIN_REPLY_SAMPLE_GAP_MS)

    def due_searches(self, focus: Focus | None, now_ms: int) -> list[str]:
        focus = self.active(focus, now_ms)
        if focus is None:
            return []
        return [query for query in focus.search_queries if self.next_search_ms.get(query, 0) <= now_ms]

    def mark_resampled(self, post_id: str, now_ms: int) -> None:
        self.last_resample_ms[post_id] = now_ms

    def mark_reply_sampled(self, post_id: str, now_ms: int) -> None:
        self.last_reply_sample_ms[post_id] = now_ms

    def mark_searched(self, query: str, next_due_ms: int) -> None:
        self.next_search_ms[query] = next_due_ms

    def forget_unlisted(self, focus: Focus | None) -> None:
        """Drop state for ids/queries no longer in the focus file (bounded memory)."""
        if focus is None:
            return
        resample = set(focus.resample_post_ids)
        replies = set(focus.reply_sample_post_ids)
        queries = set(focus.search_queries)
        self.last_resample_ms = {k: v for k, v in self.last_resample_ms.items() if k in resample}
        self.last_reply_sample_ms = {k: v for k, v in self.last_reply_sample_ms.items() if k in replies}
        self.next_search_ms = {k: v for k, v in self.next_search_ms.items() if k in queries}

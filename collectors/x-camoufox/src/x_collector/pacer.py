"""Page budget, jitter, block classification and exponential backoff.

Design rules (docs/signals/x-insighter-design.md, Collector):
- hard cap of `X_COLLECTOR_MAX_PAGES_PER_HOUR` page loads in any rolling hour;
- poll intervals get 20-40% jitter;
- on a login wall, challenge or HTTP 429: exponential backoff from 5 min to 2h;
- after 3 consecutive challenges: status `challenge`, no more page loads until restart.
"""

from __future__ import annotations

import random
from collections import deque
from collections.abc import Iterable
from enum import StrEnum
from urllib.parse import urlsplit

HOUR_MS = 3_600_000
BACKOFF_START_MS = 5 * 60_000
BACKOFF_MAX_MS = 2 * HOUR_MS
CHALLENGES_BEFORE_STOP = 3

# X error codes: 88 rate limit; 32/215 bad auth; 326 locked; 64 suspended.
_RATE_LIMIT_CODES = frozenset({88})
_LOGIN_CODES = frozenset({32, 215})
_CHALLENGE_CODES = frozenset({64, 326})
_LOGIN_PATHS = ("/i/flow/login", "/login", "/i/flow/signup", "/logout")
_CHALLENGE_PATHS = ("/account/access", "/account/suspended", "/account/locked", "/i/flow/consent")


class BlockKind(StrEnum):
    LOGIN_WALL = "login_wall"
    CHALLENGE = "challenge"
    RATE_LIMIT = "rate_limit"


def _under(path: str, prefixes: tuple[str, ...]) -> bool:
    return any(path == prefix or path.startswith(prefix + "/") for prefix in prefixes)


def classify_block(
    page_url: str,
    statuses: Iterable[int],
    error_codes: Iterable[int] = (),
    logged_in: bool = True,
) -> BlockKind | None:
    """Block kind seen on a page load, or None for a normal page.

    `statuses` are HTTP statuses of the document and the X API responses,
    `error_codes` X's JSON `errors[].code` values, `logged_in` whether the
    session cookie (`auth_token`) is present after the load.
    """
    path = urlsplit(page_url).path.rstrip("/") or "/"
    codes = set(error_codes)
    status_set = set(statuses)
    if _under(path, _CHALLENGE_PATHS) or codes & _CHALLENGE_CODES:
        return BlockKind.CHALLENGE
    if not logged_in or _under(path, _LOGIN_PATHS) or codes & _LOGIN_CODES:
        return BlockKind.LOGIN_WALL
    if 429 in status_set or codes & _RATE_LIMIT_CODES:
        return BlockKind.RATE_LIMIT
    return None


class RollingCounter:
    """Event counts inside a rolling one-hour window."""

    def __init__(self) -> None:
        self._events: deque[tuple[int, int]] = deque()
        self._total = 0

    def _expire(self, now_ms: int) -> None:
        while self._events and self._events[0][0] <= now_ms - HOUR_MS:
            _, count = self._events.popleft()
            self._total -= count

    def add(self, now_ms: int, count: int = 1) -> None:
        if count > 0:
            self._events.append((now_ms, count))
            self._total += count

    def total(self, now_ms: int) -> int:
        self._expire(now_ms)
        return self._total

    def oldest_ms(self, now_ms: int) -> int | None:
        self._expire(now_ms)
        return self._events[0][0] if self._events else None


class Pacer:
    def __init__(
        self,
        max_pages_per_hour: int,
        max_replies_per_hour: int,
        jitter_min: float = 0.20,
        jitter_max: float = 0.40,
        rng: random.Random | None = None,
    ) -> None:
        self.max_pages_per_hour = max(0, max_pages_per_hour)
        self.max_replies_per_hour = max(0, max_replies_per_hour)
        self.jitter_min = jitter_min
        self.jitter_max = max(jitter_min, jitter_max)
        self._rng = rng or random.Random()
        self.pages = RollingCounter()
        self.posts = RollingCounter()
        self.replies = RollingCounter()
        self.consecutive_blocks = 0
        self.consecutive_challenges = 0
        self.backoff_until_ms: int | None = None
        self.last_block: BlockKind | None = None
        self.stopped = False

    # ------------------------------------------------------------ budgets

    def page_budget_left(self, now_ms: int) -> int:
        return max(0, self.max_pages_per_hour - self.pages.total(now_ms))

    def next_page_slot_ms(self, now_ms: int) -> int:
        """Earliest time a page load fits the hourly cap."""
        if self.page_budget_left(now_ms) > 0:
            return now_ms
        oldest = self.pages.oldest_ms(now_ms)
        return now_ms if oldest is None else oldest + HOUR_MS

    def record_page(self, now_ms: int) -> None:
        self.pages.add(now_ms)

    def reply_budget_left(self, now_ms: int) -> int:
        return max(0, self.max_replies_per_hour - self.replies.total(now_ms))

    def jittered_ms(self, base_ms: int) -> int:
        """`base * (1 + U[jitter_min, jitter_max])`: never faster than the base interval."""
        return int(base_ms * (1.0 + self._rng.uniform(self.jitter_min, self.jitter_max)))

    def page_gap_ms(self) -> int:
        """Pause between consecutive page loads inside one round."""
        return int(self._rng.uniform(4_000, 11_000))

    # ------------------------------------------------------------ blocks

    def can_load(self, now_ms: int) -> bool:
        if self.stopped:
            return False
        if self.backoff_until_ms is not None and now_ms < self.backoff_until_ms:
            return False
        return self.page_budget_left(now_ms) > 0

    def on_success(self) -> None:
        self.consecutive_blocks = 0
        self.consecutive_challenges = 0
        self.backoff_until_ms = None
        self.last_block = None

    def on_block(self, kind: BlockKind, now_ms: int) -> None:
        self.last_block = kind
        if kind is BlockKind.CHALLENGE:
            self.consecutive_challenges += 1
            if self.consecutive_challenges >= CHALLENGES_BEFORE_STOP:
                self.stopped = True
                self.backoff_until_ms = None
                return
        delay = min(BACKOFF_START_MS * (2**self.consecutive_blocks), BACKOFF_MAX_MS)
        self.consecutive_blocks += 1
        self.backoff_until_ms = now_ms + delay

    def status(self, now_ms: int) -> str:
        """Heartbeat status (`stopped` is written by the runner on shutdown)."""
        if self.stopped:
            return "challenge"
        if self.last_block is BlockKind.LOGIN_WALL:
            return "login_required"
        if self.backoff_until_ms is not None and now_ms < self.backoff_until_ms:
            return "backoff"
        return "ok"

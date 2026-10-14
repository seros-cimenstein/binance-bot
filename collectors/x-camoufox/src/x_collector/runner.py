"""Browser loop: page loads, GraphQL interception, scheduling and heartbeat.

The collector only navigates (`page.goto`) to profile, search and status URLs.
It never clicks, never types and never sends a write request: every request
goes through `guard.decide` before it leaves the browser.
"""

from __future__ import annotations

import asyncio
import json
import logging
import random
import signal
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from importlib.metadata import PackageNotFoundError, version
from typing import Any
from urllib.parse import quote

from . import parse
from .config import Config, read_accounts, read_list_file
from .focus import (
    MAX_REPLIES_PER_PARENT,
    FocusTracker,
    load_focus,
    quote_search_query,
)
from .guard import Policy, decide, graphql_operation
from .pacer import BlockKind, Pacer, classify_block
from .writer import JsonlWriter, atomic_write_json, post_key, reply_key

log = logging.getLogger("x_collector")

HEARTBEAT_SCHEMA = "x-heartbeat-v1"
PARSED_OPERATIONS = frozenset({"SearchTimeline", "TweetDetail", "TweetResultByRestId"}) | parse.USER_TIMELINE_OPERATIONS
STATUS_OPERATIONS = frozenset({"TweetDetail", "TweetResultByRestId"})
IDLE_TICK_SECONDS = 30.0
SETTLE_SECONDS = 2.0
LOGIN_TIMEOUT_SECONDS = 20 * 60


def now_ms() -> int:
    return int(time.time() * 1000)


def camoufox_version() -> str | None:
    try:
        return version("camoufox")
    except PackageNotFoundError:
        return None


def browser_version() -> str | None:
    try:
        from camoufox.pkgman import installed_verstr

        return installed_verstr()
    except Exception:  # noqa: BLE001 - not installed / unreadable: heartbeat shows null
        return None


def search_url(query: str) -> str:
    """Latest-tab search, navigated directly (nothing is typed)."""
    return f"https://x.com/search?q={quote(query, safe='')}&src=typed_query&f=live"


def profile_url(handle: str) -> str:
    return f"https://x.com/{quote(handle, safe='')}"


def status_url(post_id: str) -> str:
    return f"https://x.com/i/status/{post_id}"


@dataclass
class PageLoad:
    url: str
    responses: list[tuple[str, Any]] = field(default_factory=list)
    block: BlockKind | None = None
    error: str | None = None

    def of(self, operations: frozenset[str]) -> list[tuple[str, Any]]:
        return [(op, payload) for op, payload in self.responses if op in operations]


@dataclass(frozen=True)
class Job:
    kind: str  # "status", "focus_search", "account", "search"
    value: str
    resample: bool = False
    reply_sample: bool = False


def load_fingerprint(config: Config) -> dict[str, Any]:
    """One stable fingerprint per profile, generated on first use."""
    path = config.fingerprint_path
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(value, dict) and value:
            return value
    except (FileNotFoundError, ValueError):
        pass
    from camoufox.fingerprints import generate_fingerprint

    fingerprint = generate_fingerprint(os="linux")
    atomic_write_json(path, fingerprint)
    return fingerprint


def browser_options(config: Config, headless: bool | str) -> dict[str, Any]:
    config.profile_dir.mkdir(parents=True, exist_ok=True)
    return {
        "persistent_context": True,
        "user_data_dir": str(config.profile_dir),
        "headless": headless,
        "fingerprint": load_fingerprint(config),
        "i_know_what_im_doing": True,
        "geoip": True,
        "block_images": config.block_images,
        # Service-worker fetches bypass context routing; block workers so the
        # read-only guard sees every request.
        "service_workers": "block",
    }


async def install_guard(context: Any, policy: Policy) -> None:
    async def route_handler(route: Any) -> None:
        request = route.request
        decision = decide(request.method, request.url, policy)
        if decision.allowed:
            await route.continue_()
        else:
            log.warning("guard blocked %s %s: %s", request.method, request.url, decision.reason)
            await route.abort("blockedbyclient")

    await context.route("**/*", route_handler)


async def is_logged_in(context: Any) -> bool:
    cookies = await context.cookies(["https://x.com"])
    return any(cookie.get("name") == "auth_token" and cookie.get("value") for cookie in cookies)


class Collector:
    def __init__(self, config: Config, clock: Callable[[], int] = now_ms, rng: random.Random | None = None) -> None:
        self.config = config
        self.clock = clock
        self.rng = rng or random.Random()
        self.pacer = Pacer(
            config.max_pages_per_hour,
            config.max_replies_per_hour,
            config.jitter_min,
            config.jitter_max,
            self.rng,
        )
        self.tracker = FocusTracker()
        self.posts = JsonlWriter(config.posts_path, post_key)
        self.engagement = JsonlWriter(config.engagement_path)
        self.replies = JsonlWriter(config.replies_path, reply_key)
        self.next_source_ms: dict[tuple[str, str], int] = {}
        self.last_error: str | None = None
        self.camoufox_version = camoufox_version()
        self.browser_version = browser_version()
        self.stop_event = asyncio.Event()
        self._last_status: str | None = None
        self._context: Any = None
        self._page: Any = None
        self._capture: list[tuple[str, Any]] = []
        self._statuses: list[int] = []
        self._pending: set[asyncio.Task[None]] = set()

    # ------------------------------------------------------------ heartbeat

    def heartbeat(self, status: str | None = None) -> None:
        now = self.clock()
        status = status or self.pacer.status(now)
        row = {
            "schema": HEARTBEAT_SCHEMA,
            "updated_at_ms": now,
            "status": status,
            "pages_last_hour": self.pacer.pages.total(now),
            "posts_last_hour": self.pacer.posts.total(now),
            "replies_last_hour": self.pacer.replies.total(now),
            "last_error": self.last_error,
            "backoff_until_ms": self.pacer.backoff_until_ms,
            "camoufox_version": self.camoufox_version,
            "browser_version": self.browser_version,
        }
        atomic_write_json(self.config.heartbeat_path, row)
        if status != self._last_status:
            log.info("collector status: %s", status)
            self._last_status = status

    # ------------------------------------------------------------ page loads

    def _on_response(self, response: Any) -> None:
        operation = graphql_operation(response.url)
        if operation is None:
            return
        self._statuses.append(response.status)
        if operation in PARSED_OPERATIONS:
            task = asyncio.ensure_future(self._read_json(operation, response))
            self._pending.add(task)
            task.add_done_callback(self._pending.discard)

    async def _read_json(self, operation: str, response: Any) -> None:
        try:
            payload = await response.json()
        except Exception as exc:  # noqa: BLE001 - body gone / not JSON: skip this response
            log.debug("unreadable %s response: %s", operation, exc)
            return
        self._capture.append((operation, payload))

    async def load(self, url: str, expect: frozenset[str]) -> PageLoad:
        """Navigate once and collect the intercepted GraphQL JSON."""
        self._capture = []
        self._statuses = []
        self.pacer.record_page(self.clock())
        result = PageLoad(url=url)
        try:
            response = await self._page.goto(
                url, wait_until="domcontentloaded", timeout=self.config.page_timeout_seconds * 1000
            )
        except Exception as exc:  # noqa: BLE001 - playwright timeout/network error
            result.error = f"navigation failed: {exc}".splitlines()[0][:300]
            return result
        if response is not None:
            self._statuses.append(response.status)
        deadline = time.monotonic() + self.config.page_timeout_seconds
        while time.monotonic() < deadline:
            await asyncio.sleep(0.5)
            if any(op in expect for op, _ in self._capture):
                break
            if classify_block(self._page.url, self._statuses) is not None:
                break
        await asyncio.sleep(SETTLE_SECONDS)
        if self._pending:
            await asyncio.gather(*list(self._pending), return_exceptions=True)
        result.responses = list(self._capture)
        codes = [code for _, payload in result.responses for code in parse.error_codes(payload)]
        result.block = classify_block(
            self._page.url, self._statuses, codes, logged_in=await is_logged_in(self._context)
        )
        if result.block is None and not result.of(expect):
            result.error = f"no {'/'.join(sorted(expect))} response from {url}"
        return result

    def _blocked(self, load: PageLoad) -> bool:
        """Apply backoff for a blocked load; True when the job must be retried later."""
        now = self.clock()
        if load.block is not None:
            self.pacer.on_block(load.block, now)
            self.last_error = f"{load.block.value} at {load.url}"
            log.warning("blocked (%s) at %s; backoff until %s", load.block.value, load.url, self.pacer.backoff_until_ms)
            self.heartbeat()
            return True
        if load.error is not None:
            self.last_error = load.error
            log.warning("%s", load.error)
        else:
            self.pacer.on_success()
        return False

    # ------------------------------------------------------------ jobs

    def _write_posts(self, tweets: list[parse.Tweet], kind: parse.SourceKind, query: str | None) -> int:
        curated = frozenset(read_accounts(self.config.accounts_file))
        seen_ms = self.clock()
        rows = [
            parse.post_row(tweet, curated_handles=curated, source_kind=kind, query=query, first_seen_ms=seen_ms)
            for tweet in tweets
        ]
        written = self.posts.append(rows)
        self.pacer.posts.add(seen_ms, len(written))
        return len(written)

    async def run_source(self, job: Job) -> bool:
        """Account timeline or (focus) search. Returns False when blocked."""
        if job.kind == "account":
            load = await self.load(profile_url(job.value), parse.USER_TIMELINE_OPERATIONS)
            kind: parse.SourceKind = "account"
        else:
            load = await self.load(search_url(job.value), frozenset({"SearchTimeline"}))
            kind = "focus_search" if job.kind == "focus_search" else "search"
        if self._blocked(load):
            return False
        tweets = [tweet for op, payload in load.of(PARSED_OPERATIONS) for tweet in parse.parse_tweets(op, payload)]
        written = self._write_posts(tweets, kind, job.value)
        log.info("%s %r: %d posts, %d new", job.kind, job.value, len(tweets), written)
        return True

    async def run_status(self, job: Job) -> bool:
        """Engagement re-read and/or reply/quote sample of one post."""
        post_id = job.value
        load = await self.load(status_url(post_id), STATUS_OPERATIONS)
        if self._blocked(load):
            return False
        sampled_at = self.clock()
        detail = load.of(STATUS_OPERATIONS)
        state, tweet = parse.post_state(detail, post_id) if detail else ("error", None)
        if job.resample:
            self.engagement.append([parse.engagement_row(post_id, sampled_at_ms=sampled_at, status=state, tweet=tweet)])
            self.tracker.mark_resampled(post_id, sampled_at)
        if not job.reply_sample:
            return True
        if state != "ok":
            self.tracker.mark_reply_sampled(post_id, sampled_at)
            return True
        quotes: list[tuple[str, Any]] = []
        if self.pacer.can_load(self.clock()) and not self.stop_event.is_set():
            await self.pause()
            quote_load = await self.load(search_url(quote_search_query(post_id)), frozenset({"SearchTimeline"}))
            if self._blocked(quote_load):
                return False
            quotes = quote_load.of(frozenset({"SearchTimeline"}))
        now = self.clock()
        limit = min(MAX_REPLIES_PER_PARENT, self.pacer.reply_budget_left(now))
        picked = parse.newest_replies_and_quotes(post_id, detail, quotes, limit)
        rows = [parse.reply_row(reply, parent_post_id=post_id, kind=kind, first_seen_ms=now) for kind, reply in picked]
        written = self.replies.append(rows)
        self.pacer.replies.add(now, len(written))
        self.tracker.mark_reply_sampled(post_id, now)
        log.info("replies for %s: %d sampled, %d new", post_id, len(picked), len(written))
        return True

    async def run_job(self, job: Job) -> bool:
        if job.kind == "status":
            return await self.run_status(job)
        ok = await self.run_source(job)
        if ok:
            now = self.clock()
            if job.kind == "focus_search":
                self.tracker.mark_searched(
                    job.value, now + self.pacer.jittered_ms(self.config.focus_poll_seconds * 1000)
                )
            else:
                self.next_source_ms[(job.kind, job.value)] = now + self.pacer.jittered_ms(
                    self.config.poll_seconds * 1000
                )
        return ok

    # ------------------------------------------------------------ scheduling

    def _sources(self) -> list[tuple[str, str]]:
        accounts = [("account", handle) for handle in read_accounts(self.config.accounts_file)]
        searches = [("search", query) for query in read_list_file(self.config.searches_file)]
        return accounts + searches

    def focus_jobs(self, now: int) -> list[Job]:
        focus = load_focus(self.config.focus_path)
        self.tracker.forget_unlisted(self.tracker.active(focus, now))
        resample = self.tracker.due_resamples(focus, now)
        replies = self.tracker.due_reply_samples(focus, now) if self.pacer.reply_budget_left(now) > 0 else []
        ids = list(dict.fromkeys(replies + resample))
        jobs = [
            Job("status", post_id, resample=post_id in resample, reply_sample=post_id in replies) for post_id in ids
        ]
        jobs.extend(Job("focus_search", query) for query in self.tracker.due_searches(focus, now))
        return jobs

    def next_job(self, now: int) -> tuple[Job | None, int]:
        """Most urgent due job, or (None, time of the next due source)."""
        focus = self.focus_jobs(now)
        if focus:
            return focus[0], now
        sources = self._sources()
        known = set(sources)
        for key in list(self.next_source_ms):
            if key not in known:
                del self.next_source_ms[key]
        poll_ms = self.config.poll_seconds * 1000
        for key in sources:
            if key not in self.next_source_ms:
                # Stagger new sources over one poll interval instead of a burst.
                self.next_source_ms[key] = now + int(self.rng.uniform(0, poll_ms))
        if not self.next_source_ms:
            return None, now + poll_ms
        key, due = min(self.next_source_ms.items(), key=lambda item: item[1])
        if due <= now:
            return Job(key[0], key[1]), now
        return None, due

    async def pause(self, seconds: float | None = None) -> None:
        """Interruptible sleep (default: the jittered gap between page loads)."""
        delay = self.pacer.page_gap_ms() / 1000 if seconds is None else max(0.0, seconds)
        try:
            await asyncio.wait_for(self.stop_event.wait(), timeout=delay)
        except TimeoutError:
            pass

    # ------------------------------------------------------------ entry points

    async def _with_browser(self, body: Callable[[], Any], headless: bool | str, policy: Policy) -> int:
        from camoufox.async_api import AsyncCamoufox

        async with AsyncCamoufox(**browser_options(self.config, headless)) as context:
            self._context = context
            await install_guard(context, policy)
            self._page = context.pages[0] if context.pages else await context.new_page()
            self._page.on("response", self._on_response)
            return await body()

    def _install_signal_handlers(self) -> None:
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            try:
                loop.add_signal_handler(sig, self.stop_event.set)
            except (NotImplementedError, RuntimeError):
                pass

    async def run(self, once: bool) -> int:
        self._install_signal_handlers()
        self.heartbeat()
        try:
            code = await self._with_browser(
                (lambda: self._run_once()) if once else (lambda: self._run_loop()),
                self.config.headless,
                "collect",
            )
        except Exception as exc:
            self.last_error = f"collector crashed: {exc}".splitlines()[0][:300]
            self.heartbeat("stopped")
            raise
        self.heartbeat("stopped")
        return code

    async def _run_once(self) -> int:
        """One pass over all focus work and every source, within the page cap."""
        now = self.clock()
        jobs = self.focus_jobs(now) + [Job(kind, value) for kind, value in self._sources()]
        for index, job in enumerate(jobs):
            if self.stop_event.is_set():
                break
            if not self.pacer.can_load(self.clock()):
                log.warning("page cap or backoff reached; %d jobs left for the next run", len(jobs) - index)
                break
            if index:
                await self.pause()
            if not await self.run_job(job):
                self.heartbeat()
                return 2
            self.heartbeat()
        return 0

    async def _run_loop(self) -> int:
        while not self.stop_event.is_set():
            now = self.clock()
            self.heartbeat()
            if self.pacer.stopped:
                await self.pause(IDLE_TICK_SECONDS * 2)
                continue
            backoff_until = self.pacer.backoff_until_ms
            if backoff_until is not None and now < backoff_until:
                await self.pause(min(IDLE_TICK_SECONDS * 2, (backoff_until - now) / 1000))
                continue
            slot = self.pacer.next_page_slot_ms(now)
            if slot > now:
                await self.pause(min(IDLE_TICK_SECONDS * 2, (slot - now) / 1000))
                continue
            job, due = self.next_job(now)
            if job is None:
                await self.pause(min(IDLE_TICK_SECONDS, max(1.0, (due - now) / 1000)))
                continue
            await self.run_job(job)
            await self.pause()
        return 0

    async def login(self) -> int:
        """Headful one-time login; the session is kept in the persistent profile."""

        async def body() -> int:
            await self._page.goto("https://x.com/i/flow/login", wait_until="domcontentloaded")
            print(
                "Log in to the burner X account in the browser window. Do not like, follow,\n"
                "post or reply. This window closes by itself once the session cookie exists.",
                flush=True,
            )
            deadline = time.monotonic() + LOGIN_TIMEOUT_SECONDS
            while time.monotonic() < deadline and not self.stop_event.is_set():
                if await is_logged_in(self._context):
                    await asyncio.sleep(5)  # let X finish setting cookies before the profile is closed
                    print(f"Logged in; session stored in {self.config.profile_dir}", flush=True)
                    return 0
                if not self._context.pages:
                    break
                await asyncio.sleep(2)
            print("Login not completed (timeout or window closed).", flush=True)
            return 1

        self._install_signal_handlers()
        return await self._with_browser(body, False, "login")

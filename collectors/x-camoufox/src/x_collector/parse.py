"""Parse intercepted X GraphQL JSON into the Phase 43 collector rows.

Row shapes (`x-post-v1`, `x-reply-v1`, `x-engagement-v1`) follow
docs/signals/x-insighter-design.md exactly. Privacy reduction: authors are
reduced to `author_hash` (first 16 hex of SHA-256 of the numeric user id),
follower count, account creation time and verified flag; handles are kept
only for curated accounts.
"""

from __future__ import annotations

import hashlib
import html
import re
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any, Literal

POST_SCHEMA = "x-post-v1"
REPLY_SCHEMA = "x-reply-v1"
ENGAGEMENT_SCHEMA = "x-engagement-v1"
EXCERPT_MAX_CHARS = 280

SourceKind = Literal["account", "search", "focus_search"]
ReplyKind = Literal["reply", "quote"]

_BASE58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
_BASE58_INDEX = {char: index for index, char in enumerate(_BASE58_ALPHABET)}
_CASHTAG = re.compile(r"(?<![A-Za-z0-9_$])\$([A-Za-z0-9]{1,10})(?![A-Za-z0-9_])")
_SOLANA = re.compile(r"(?<![1-9A-HJ-NP-Za-km-z])[1-9A-HJ-NP-Za-km-z]{32,44}(?![1-9A-HJ-NP-Za-km-z])")
_EVM = re.compile(r"(?<![0-9A-Za-z])0x[0-9a-fA-F]{40}(?![0-9A-Za-z])")
_WHITESPACE = re.compile(r"\s+")
_URL = re.compile(r"https?://\S+")
_LEADING_MENTIONS = re.compile(r"^(?:@\w{1,15}\s+)+")
_MONTHS = {
    name: index + 1
    for index, name in enumerate(["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"])
}
_X_DATETIME = re.compile(r"^[A-Za-z]{3} ([A-Za-z]{3}) (\d{1,2}) (\d{2}):(\d{2}):(\d{2}) ([+-])(\d{2})(\d{2}) (\d{4})$")
_GONE_TYPENAMES = frozenset({"TweetTombstone", "TweetUnavailable"})
# X error codes meaning the status does not exist / is not visible.
_GONE_ERROR_CODES = frozenset({8, 34, 144, 179, 385, 421, 422})


# ---------------------------------------------------------------- primitives


def author_hash(user_id: str) -> str:
    """Same as `bb_social::hash_identity`: first 16 hex of SHA-256."""
    if not user_id:
        return ""
    return hashlib.sha256(user_id.encode("utf-8")).hexdigest()[:16]


def parse_x_datetime(value: Any) -> int | None:
    """`Wed Oct 10 20:19:24 +0000 2018` -> epoch ms (locale-independent)."""
    if not isinstance(value, str):
        return None
    match = _X_DATETIME.match(value.strip())
    if not match:
        return None
    month_name, day, hour, minute, second, sign, off_h, off_m, year = match.groups()
    month = _MONTHS.get(month_name.title())
    if month is None:
        return None
    offset = timedelta(hours=int(off_h), minutes=int(off_m))
    if sign == "-":
        offset = -offset
    try:
        moment = datetime(int(year), month, int(day), int(hour), int(minute), int(second), tzinfo=timezone(offset))
    except ValueError:
        return None
    return int(moment.timestamp()) * 1000


def collapse_excerpt(text: str) -> str:
    """Whitespace-collapsed, at most 280 characters."""
    return _WHITESPACE.sub(" ", text).strip()[:EXCERPT_MAX_CHARS]


def _dedupe(values: Iterable[str]) -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    for value in values:
        if value not in seen:
            seen.add(value)
            out.append(value)
    return out


def normalize_cashtag(raw: str) -> str | None:
    """Uppercase without `$`: 1-10 chars of `[A-Z0-9]` with at least one letter."""
    tag = raw.strip().lstrip("$").upper()
    if not 1 <= len(tag) <= 10 or not tag.isascii() or not tag.isalnum():
        return None
    if not any(char.isalpha() for char in tag):
        return None
    return tag


def extract_cashtags(text: str, entity_symbols: Iterable[str] = ()) -> list[str]:
    """Cashtags from `text` (URLs ignored) plus X's own `entities.symbols`."""
    without_urls = _URL.sub(" ", text)
    candidates = [match.group(1) for match in _CASHTAG.finditer(without_urls)]
    candidates.extend(entity_symbols)
    return _dedupe(tag for tag in (normalize_cashtag(raw) for raw in candidates) if tag)


def base58_decoded_length(value: str) -> int | None:
    """Byte length of a base58 string, or None when it is not base58."""
    number = 0
    for char in value:
        digit = _BASE58_INDEX.get(char)
        if digit is None:
            return None
        number = number * 58 + digit
    leading_zeros = len(value) - len(value.lstrip("1"))
    return leading_zeros + (number.bit_length() + 7) // 8


def extract_solana_addresses(*texts: str) -> list[str]:
    """Base58 strings of 32-44 chars that decode to a 32-byte public key."""
    found: list[str] = []
    for text in texts:
        for match in _SOLANA.finditer(text):
            candidate = match.group(0)
            if base58_decoded_length(candidate) == 32:
                found.append(candidate)
    return _dedupe(found)


def extract_evm_addresses(*texts: str) -> list[str]:
    """`0x` + 40 hex, lowercased and deduplicated."""
    return _dedupe(match.group(0).lower() for text in texts for match in _EVM.finditer(text))


# ---------------------------------------------------------------- tweet model


@dataclass(frozen=True)
class Metrics:
    replies: int = 0
    reposts: int = 0
    quotes: int = 0
    likes: int = 0
    bookmarks: int = 0
    views: int | None = None

    def as_row(self) -> dict[str, int | None]:
        return {
            "replies": self.replies,
            "reposts": self.reposts,
            "quotes": self.quotes,
            "likes": self.likes,
            "bookmarks": self.bookmarks,
            "views": self.views,
        }


@dataclass(frozen=True)
class Tweet:
    """A parsed, privacy-reduced post. `author_handle` is never written unless curated."""

    post_id: str
    author_id: str
    author_handle: str | None
    author_followers: int
    author_created_at_ms: int | None
    author_verified: bool
    created_at_ms: int
    excerpt: str
    lang: str | None
    cashtags: list[str]
    solana_addresses: list[str]
    evm_addresses: list[str]
    urls: list[str]
    in_reply_to_post_id: str | None
    quoted_post_id: str | None
    is_quote: bool
    conversation_id: str | None
    metrics: Metrics = field(default_factory=Metrics)


@dataclass(frozen=True)
class TimelineItem:
    """One timeline entry: a parsed tweet, or a tombstone for `post_id`."""

    post_id: str
    tweet: Tweet | None


def _int(value: Any) -> int:
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return 0


def _views(raw: Any) -> int | None:
    if not isinstance(raw, dict):
        return None
    count = raw.get("count")
    if isinstance(count, int) and not isinstance(count, bool):
        return count
    if isinstance(count, str) and count.strip().isdigit():
        return int(count.strip())
    return None


def _dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _list(value: Any) -> list[Any]:
    return value if isinstance(value, list) else []


def _str_or_none(value: Any) -> str | None:
    if isinstance(value, str) and value:
        return value
    if isinstance(value, int) and not isinstance(value, bool):
        return str(value)
    return None


def unwrap_tweet_result(result: Any) -> dict[str, Any] | None:
    """`TweetWithVisibilityResults` -> inner `Tweet`; tombstones -> None."""
    node = _dict(result)
    typename = node.get("__typename")
    if typename == "TweetWithVisibilityResults":
        node = _dict(node.get("tweet"))
        typename = node.get("__typename", "Tweet")
    if typename in _GONE_TYPENAMES or not node.get("legacy"):
        return None
    return node


def _text_and_entities(node: dict[str, Any]) -> tuple[str, dict[str, Any]]:
    """Full text, preferring the long-form `note_tweet` body."""
    legacy = _dict(node.get("legacy"))
    note = _dict(_dict(_dict(node.get("note_tweet")).get("note_tweet_results")).get("result"))
    if isinstance(note.get("text"), str) and note["text"]:
        return note["text"], _dict(note.get("entity_set"))
    return str(legacy.get("full_text") or legacy.get("text") or ""), _dict(legacy.get("entities"))


def parse_tweet(result: Any) -> Tweet | None:
    """Parse a GraphQL tweet result (Tweet or TweetWithVisibilityResults)."""
    node = unwrap_tweet_result(result)
    if node is None:
        return None
    legacy = _dict(node.get("legacy"))
    post_id = _str_or_none(node.get("rest_id")) or _str_or_none(legacy.get("id_str"))
    created_at_ms = parse_x_datetime(legacy.get("created_at"))
    if post_id is None or created_at_ms is None:
        return None

    user = _dict(_dict(_dict(node.get("core")).get("user_results")).get("result"))
    user_legacy = _dict(user.get("legacy"))
    user_core = _dict(user.get("core"))
    author_id = _str_or_none(user.get("rest_id")) or _str_or_none(legacy.get("user_id_str")) or ""
    handle = user_core.get("screen_name") or user_legacy.get("screen_name")
    verified = bool(
        user.get("is_blue_verified") or user_legacy.get("verified") or _dict(user.get("verification")).get("verified")
    )

    text, entities = _text_and_entities(node)
    text = html.unescape(text)
    legacy_entities = _dict(legacy.get("entities"))
    urls: list[str] = []
    for entity in _list(entities.get("urls")) + _list(legacy_entities.get("urls")):
        entity = _dict(entity)
        short, expanded = entity.get("url"), entity.get("expanded_url")
        if isinstance(expanded, str) and expanded:
            urls.append(expanded)
            if isinstance(short, str) and short:
                text = text.replace(short, expanded)
    media = _list(legacy_entities.get("media")) + _list(_dict(legacy.get("extended_entities")).get("media"))
    for entity in media:
        short = _dict(entity).get("url")
        if isinstance(short, str) and short:
            text = text.replace(short, " ")
    urls = _dedupe(urls)

    in_reply_to = _str_or_none(legacy.get("in_reply_to_status_id_str"))
    if in_reply_to:
        text = _LEADING_MENTIONS.sub("", text.lstrip())
    quoted_result = unwrap_tweet_result(_dict(node.get("quoted_status_result")).get("result"))
    quoted_id = _str_or_none(legacy.get("quoted_status_id_str")) or (
        _str_or_none(quoted_result.get("rest_id")) if quoted_result else None
    )
    symbols = [
        str(_dict(symbol).get("text") or "")
        for symbol in _list(entities.get("symbols")) + _list(legacy_entities.get("symbols"))
    ]
    metrics = Metrics(
        replies=_int(legacy.get("reply_count")),
        reposts=_int(legacy.get("retweet_count")),
        quotes=_int(legacy.get("quote_count")),
        likes=_int(legacy.get("favorite_count")),
        bookmarks=_int(legacy.get("bookmark_count")),
        views=_views(node.get("views")),
    )
    return Tweet(
        post_id=post_id,
        author_id=author_id,
        author_handle=handle if isinstance(handle, str) and handle else None,
        # X moved user counters out of `legacy` (2026-10: `relationship_counts.followers`).
        author_followers=_int(
            _dict(user.get("relationship_counts")).get("followers", user_legacy.get("followers_count"))
        ),
        author_created_at_ms=parse_x_datetime(user_core.get("created_at") or user_legacy.get("created_at")),
        author_verified=verified,
        created_at_ms=created_at_ms,
        excerpt=collapse_excerpt(text),
        lang=_str_or_none(legacy.get("lang")),
        cashtags=extract_cashtags(text, symbols),
        solana_addresses=extract_solana_addresses(text, *urls),
        evm_addresses=extract_evm_addresses(text, *urls),
        urls=urls,
        in_reply_to_post_id=in_reply_to,
        quoted_post_id=quoted_id,
        is_quote=bool(legacy.get("is_quote_status")) or quoted_id is not None,
        conversation_id=_str_or_none(legacy.get("conversation_id_str")),
        metrics=metrics,
    )


# ---------------------------------------------------------------- timelines

# Profile timeline query names. X's web app renamed `UserTweets` to
# `UserOriginalsTimeline` (seen 2026-10-13); both share the payload shape.
USER_TIMELINE_OPERATIONS = frozenset({"UserTweets", "UserOriginalsTimeline"})


def _timeline_instructions(operation: str, payload: dict[str, Any]) -> list[Any]:
    data = _dict(payload.get("data"))
    if operation == "SearchTimeline":
        timeline = _dict(_dict(_dict(data.get("search_by_raw_query")).get("search_timeline")).get("timeline"))
        return _list(timeline.get("instructions"))
    if operation in USER_TIMELINE_OPERATIONS:
        result = _dict(_dict(data.get("user")).get("result"))
        for key in ("timeline", "timeline_v2"):
            timeline = _dict(_dict(result.get(key)).get("timeline"))
            if timeline:
                return _list(timeline.get("instructions"))
        return []
    if operation == "TweetDetail":
        return _list(_dict(data.get("threaded_conversation_with_injections_v2")).get("instructions"))
    return []


def _instruction_entries(instruction: dict[str, Any]) -> Iterator[dict[str, Any]]:
    kind = instruction.get("type")
    if kind == "TimelineAddEntries":
        yield from (_dict(entry) for entry in _list(instruction.get("entries")))
    elif kind in ("TimelinePinEntry", "TimelineReplaceEntry"):
        entry = _dict(instruction.get("entry"))
        if entry:
            yield entry


def _entry_item_contents(entry: dict[str, Any]) -> Iterator[tuple[str, dict[str, Any]]]:
    """(entry id, itemContent) for single items and module items."""
    entry_id = str(entry.get("entryId") or "")
    content = _dict(entry.get("content"))
    item_content = _dict(content.get("itemContent"))
    if item_content:
        yield entry_id, item_content
    for module_item in _list(content.get("items")):
        module_item = _dict(module_item)
        item_entry_id = str(module_item.get("entryId") or entry_id)
        nested = _dict(_dict(module_item.get("item")).get("itemContent"))
        if nested:
            yield item_entry_id, nested


def _post_id_from_entry_id(entry_id: str) -> str | None:
    tail = entry_id.rsplit("-", 1)[-1]
    return tail if tail.isdigit() else None


def timeline_items(operation: str, payload: Any) -> list[TimelineItem]:
    """Tweets (and tombstones) in a SearchTimeline / UserTweets / TweetDetail /
    TweetResultByRestId response. Promoted entries are skipped; reposts are
    unwrapped to the original post."""
    payload = _dict(payload)
    if operation == "TweetResultByRestId":
        result = _dict(_dict(_dict(payload.get("data")).get("tweetResult")).get("result"))
        tweet = parse_tweet(_repost_original(result))
        if tweet is not None:
            return [TimelineItem(tweet.post_id, tweet)]
        legacy_id = _str_or_none(_dict(unwrap_or_raw(result).get("legacy")).get("id_str"))
        rest_id = _str_or_none(result.get("rest_id")) or legacy_id
        return [TimelineItem(rest_id, None)] if rest_id else []

    items: list[TimelineItem] = []
    for instruction in _timeline_instructions(operation, payload):
        for entry in _instruction_entries(_dict(instruction)):
            for entry_id, item_content in _entry_item_contents(entry):
                if item_content.get("itemType", "TimelineTweet") != "TimelineTweet":
                    continue
                if item_content.get("promotedMetadata") or "promoted" in entry_id:
                    continue
                result = _dict(_dict(item_content.get("tweet_results")).get("result"))
                if not result:
                    continue
                tweet = parse_tweet(_repost_original(result))
                if tweet is not None:
                    items.append(TimelineItem(tweet.post_id, tweet))
                    continue
                post_id = _str_or_none(result.get("rest_id")) or _post_id_from_entry_id(entry_id)
                if post_id and unwrap_tweet_result(result) is None:
                    items.append(TimelineItem(post_id, None))
    return items


def unwrap_or_raw(result: dict[str, Any]) -> dict[str, Any]:
    if result.get("__typename") == "TweetWithVisibilityResults":
        return _dict(result.get("tweet"))
    return result


def _repost_original(result: dict[str, Any]) -> dict[str, Any]:
    """A repost's timeline entry carries the original in `retweeted_status_result`."""
    inner = unwrap_or_raw(result)
    original = _dict(_dict(_dict(inner.get("legacy")).get("retweeted_status_result")).get("result"))
    return original or result


def parse_tweets(operation: str, payload: Any) -> list[Tweet]:
    return [item.tweet for item in timeline_items(operation, payload) if item.tweet is not None]


def error_codes(payload: Any) -> list[int]:
    codes: list[int] = []
    for error in _list(_dict(payload).get("errors")):
        code = _dict(error).get("code")
        if isinstance(code, int) and not isinstance(code, bool):
            codes.append(code)
    return codes


def post_state(
    responses: Iterable[tuple[str, Any]], post_id: str
) -> tuple[Literal["ok", "gone", "error"], Tweet | None]:
    """Status of `post_id` across the status-page responses (`(operation, json)`)."""
    gone = False
    for operation, payload in responses:
        for item in timeline_items(operation, payload):
            if item.post_id != post_id:
                continue
            if item.tweet is not None:
                return "ok", item.tweet
            gone = True
        if any(code in _GONE_ERROR_CODES for code in error_codes(payload)):
            gone = True
        if operation == "TweetResultByRestId":
            data = _dict(_dict(payload).get("data"))
            tweet_result = _dict(data.get("tweetResult"))
            result = _dict(tweet_result.get("result"))
            if "tweetResult" in data and (
                not tweet_result or unwrap_or_raw(result).get("__typename") in _GONE_TYPENAMES
            ):
                gone = True
    return ("gone", None) if gone else ("error", None)


# ---------------------------------------------------------------- rows


def post_row(
    tweet: Tweet,
    *,
    curated_handles: frozenset[str],
    source_kind: SourceKind,
    query: str | None,
    first_seen_ms: int,
) -> dict[str, Any]:
    handle = (tweet.author_handle or "").lower()
    curated = bool(handle) and handle in curated_handles
    return {
        "schema": POST_SCHEMA,
        "post_id": tweet.post_id,
        "author_hash": author_hash(tweet.author_id),
        "author_handle": tweet.author_handle if curated else None,
        "curated": curated,
        "author_followers": tweet.author_followers,
        "author_created_at_ms": tweet.author_created_at_ms,
        "author_verified": tweet.author_verified,
        "created_at_ms": tweet.created_at_ms,
        "first_seen_ms": first_seen_ms,
        "excerpt": tweet.excerpt,
        "lang": tweet.lang,
        "cashtags": list(tweet.cashtags),
        "solana_addresses": list(tweet.solana_addresses),
        "evm_addresses": list(tweet.evm_addresses),
        "urls": list(tweet.urls),
        "is_reply": tweet.in_reply_to_post_id is not None,
        "is_quote": tweet.is_quote,
        "in_reply_to_post_id": tweet.in_reply_to_post_id,
        "quoted_post_id": tweet.quoted_post_id,
        "metrics": tweet.metrics.as_row(),
        "source": {"kind": source_kind, "query": query},
    }


def reply_row(tweet: Tweet, *, parent_post_id: str, kind: ReplyKind, first_seen_ms: int) -> dict[str, Any]:
    return {
        "schema": REPLY_SCHEMA,
        "parent_post_id": parent_post_id,
        "post_id": tweet.post_id,
        "kind": kind,
        "author_hash": author_hash(tweet.author_id),
        "author_followers": tweet.author_followers,
        "author_created_at_ms": tweet.author_created_at_ms,
        "created_at_ms": tweet.created_at_ms,
        "first_seen_ms": first_seen_ms,
        "excerpt": tweet.excerpt,
        "metrics": tweet.metrics.as_row(),
    }


def engagement_row(
    post_id: str, *, sampled_at_ms: int, status: Literal["ok", "gone", "error"], tweet: Tweet | None
) -> dict[str, Any]:
    return {
        "schema": ENGAGEMENT_SCHEMA,
        "post_id": post_id,
        "sampled_at_ms": sampled_at_ms,
        "status": status,
        "metrics": tweet.metrics.as_row() if status == "ok" and tweet is not None else None,
    }


def newest_replies_and_quotes(
    parent_post_id: str,
    detail_responses: Iterable[tuple[str, Any]],
    quote_responses: Iterable[tuple[str, Any]],
    limit: int,
) -> list[tuple[ReplyKind, Tweet]]:
    """Newest direct replies (TweetDetail) and quotes (quote search) of a parent,
    at most `limit` in total, newest first."""
    picked: dict[str, tuple[ReplyKind, Tweet]] = {}
    for operation, payload in detail_responses:
        for tweet in parse_tweets(operation, payload):
            if tweet.post_id != parent_post_id and tweet.in_reply_to_post_id == parent_post_id:
                picked.setdefault(tweet.post_id, ("reply", tweet))
    for operation, payload in quote_responses:
        for tweet in parse_tweets(operation, payload):
            if tweet.post_id != parent_post_id and tweet.quoted_post_id == parent_post_id:
                picked.setdefault(tweet.post_id, ("quote", tweet))
    ordered = sorted(picked.values(), key=lambda pair: (pair[1].created_at_ms, pair[1].post_id), reverse=True)
    return ordered[: max(limit, 0)]

"""Read-only social-signal collection and asset evidence.

This module ingests only authorized Telegram Bot API updates and official X API
search results. It writes local, privacy-reduced audit records and has no order
submission path.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import math
import os
import re
import shlex
import subprocess
from dataclasses import asdict, dataclass, field, replace
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

import aiohttp
from dotenv import load_dotenv


SOCIAL_SIGNAL_MODE_READ_ONLY = "read_only"
SOCIAL_LABEL_BULLISH = "bullish_call"
SOCIAL_LABEL_BEARISH = "bearish_warning"
SOCIAL_LABEL_MANIPULATION = "manipulation_risk"
SOCIAL_LABEL_NEUTRAL = "neutral_mention"

_BULLISH_TERMS = ("buy", "bull", "breakout", "moon", "gem", "long", "accumulate", "listing")
_BEARISH_TERMS = ("dump", "sell", "exit", "crash", "tank", "rug", "scam", "drain")
_MANIPULATION_TERMS = (
    "pump",
    "pump group",
    "pump and dump",
    "signal group",
    "guaranteed",
    "100x",
    "1000x",
    "ape now",
    "buy now",
    "coordinate",
    "rug",
    "scam",
)
_URL_RE = re.compile(r"https?://[^\s<>()]+", re.IGNORECASE)


class SocialSignalError(RuntimeError):
    """Raised when read-only social-signal processing cannot proceed."""


@dataclass(frozen=True, slots=True)
class SocialSignalSettings:
    """Configuration for optional read-only social evidence."""

    enabled: bool = False
    telegram_enabled: bool = False
    x_enabled: bool = False
    mode: str = SOCIAL_SIGNAL_MODE_READ_ONLY
    audit_path: str = "data/social-signals.jsonl"
    max_age_minutes: int = 180
    min_source_diversity: int = 2
    block_manipulation_risk: bool = True
    manipulation_block_score: float = 0.8
    max_records_per_asset: int = 12
    telegram_bot_token: str = ""
    x_bearer_token: str = ""
    x_query: str = ""
    ai_enabled: bool = False
    ai_command: str = "claude"
    ai_model: str = "sonnet"
    ai_timeout_seconds: float = 120.0

    def __post_init__(self) -> None:
        if self.mode.strip().lower() != SOCIAL_SIGNAL_MODE_READ_ONLY:
            raise SocialSignalError("social signals support read_only mode only")
        if not self.audit_path.strip():
            raise SocialSignalError("social signal audit_path must not be blank")
        for name in ("max_age_minutes", "min_source_diversity", "max_records_per_asset"):
            if getattr(self, name) < 0:
                raise SocialSignalError(f"{name} must be non-negative")
        if not math.isfinite(self.manipulation_block_score) or not (
            0.0 <= self.manipulation_block_score <= 1.0
        ):
            raise SocialSignalError("manipulation_block_score must be between 0 and 1")
        if not math.isfinite(self.ai_timeout_seconds) or self.ai_timeout_seconds <= 0:
            raise SocialSignalError("ai_timeout_seconds must be greater than zero")

    @classmethod
    def from_env(cls) -> "SocialSignalSettings":
        load_dotenv()
        return cls(
            enabled=_env_bool("SOCIAL_SIGNAL_ENABLED", False),
            telegram_enabled=_env_bool("SOCIAL_SIGNAL_TELEGRAM_ENABLED", False),
            x_enabled=_env_bool("SOCIAL_SIGNAL_X_ENABLED", False),
            mode=os.getenv("SOCIAL_SIGNAL_MODE", SOCIAL_SIGNAL_MODE_READ_ONLY),
            audit_path=os.getenv("SOCIAL_SIGNAL_AUDIT_PATH", "data/social-signals.jsonl"),
            max_age_minutes=_env_int("SOCIAL_SIGNAL_MAX_AGE_MINUTES", 180),
            min_source_diversity=_env_int("SOCIAL_SIGNAL_MIN_SOURCE_DIVERSITY", 2),
            block_manipulation_risk=_env_bool(
                "SOCIAL_SIGNAL_BLOCK_MANIPULATION_RISK",
                True,
            ),
            manipulation_block_score=_env_float(
                "SOCIAL_SIGNAL_MANIPULATION_BLOCK_SCORE",
                0.8,
            ),
            max_records_per_asset=_env_int("SOCIAL_SIGNAL_MAX_RECORDS_PER_ASSET", 12),
            telegram_bot_token=os.getenv("SOCIAL_SIGNAL_TELEGRAM_BOT_TOKEN", ""),
            x_bearer_token=os.getenv("SOCIAL_SIGNAL_X_BEARER_TOKEN", ""),
            x_query=os.getenv("SOCIAL_SIGNAL_X_QUERY", ""),
            ai_enabled=_env_bool("SOCIAL_SIGNAL_AI_ENABLED", False),
            ai_command=os.getenv("SOCIAL_SIGNAL_AI_COMMAND", "claude"),
            ai_model=os.getenv("SOCIAL_SIGNAL_AI_MODEL", "sonnet"),
            ai_timeout_seconds=_env_float("SOCIAL_SIGNAL_AI_TIMEOUT_SECONDS", 120.0),
        )


@dataclass(frozen=True, slots=True)
class SocialSignalRecord:
    """Privacy-reduced record derived from one permitted social post."""

    source: str
    message_id: str
    timestamp: str
    author_hash: str
    channel_hash: str
    excerpt: str
    urls: tuple[str, ...]
    assets: tuple[str, ...]
    label: str
    confidence: float
    manipulation_risk: float
    engagement: dict[str, int] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        payload = asdict(self)
        payload["urls"] = list(self.urls)
        payload["assets"] = list(self.assets)
        return payload


class HeadlessSocialAnalyzer:
    """Use headless Claude to classify already-collected read-only posts."""

    def __init__(self, settings: SocialSignalSettings) -> None:
        self.settings = settings

    def classify(self, records: Sequence[SocialSignalRecord]) -> list[SocialSignalRecord]:
        if not records:
            return []
        argv = shlex.split(self.settings.ai_command)
        if not argv:
            raise SocialSignalError("social signal AI command is empty")
        schema = {
            "type": "object",
            "required": ["signals"],
            "properties": {
                "signals": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["message_id", "label", "confidence", "manipulation_risk"],
                        "properties": {
                            "message_id": {"type": "string"},
                            "label": {
                                "type": "string",
                                "enum": [
                                    SOCIAL_LABEL_BULLISH,
                                    SOCIAL_LABEL_BEARISH,
                                    SOCIAL_LABEL_MANIPULATION,
                                    SOCIAL_LABEL_NEUTRAL,
                                ],
                            },
                            "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                            "manipulation_risk": {"type": "number", "minimum": 0, "maximum": 1},
                        },
                    },
                }
            },
        }
        prompt = (
            "Classify read-only crypto social posts for trading evidence. "
            "Detect hype, coordinated pump/dump or rug/scam risk conservatively. "
            "Do not give trading instructions. Return JSON matching the schema.\n"
            + json.dumps(
                [
                    {
                        "message_id": record.message_id,
                        "source": record.source,
                        "assets": list(record.assets),
                        "excerpt": record.excerpt,
                    }
                    for record in records
                ],
                sort_keys=True,
            )
        )
        argv.extend(
            [
                "-p",
                "--no-session-persistence",
                "--tools",
                "",
                "--output-format",
                "json",
                "--json-schema",
                json.dumps(schema, sort_keys=True),
                "--model",
                self.settings.ai_model,
                prompt,
            ]
        )
        completed = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=self.settings.ai_timeout_seconds,
            check=False,
        )
        if completed.returncode != 0:
            raise SocialSignalError("social signal AI classification failed")
        payload = _json_object(completed.stdout)
        output = payload.get("signals") if isinstance(payload, Mapping) else None
        if not isinstance(output, list):
            raise SocialSignalError("social signal AI returned no classified signals")
        classified = {
            str(item.get("message_id")): item
            for item in output
            if isinstance(item, Mapping)
        }
        enriched: list[SocialSignalRecord] = []
        for record in records:
            item = classified.get(record.message_id)
            if item is None:
                enriched.append(record)
                continue
            enriched.append(
                replace(
                    record,
                    label=str(item.get("label") or record.label),
                    confidence=_bounded_float(item.get("confidence"), record.confidence),
                    manipulation_risk=_bounded_float(
                        item.get("manipulation_risk"),
                        record.manipulation_risk,
                    ),
                )
            )
        return enriched


def parse_telegram_updates(
    payload: Mapping[str, Any],
    assets: Iterable[str],
) -> list[SocialSignalRecord]:
    """Parse Telegram updates visible to a configured bot."""
    records: list[SocialSignalRecord] = []
    for update in payload.get("result", []) if isinstance(payload.get("result"), list) else []:
        if not isinstance(update, Mapping):
            continue
        message = update.get("channel_post") or update.get("message")
        if not isinstance(message, Mapping):
            continue
        text = str(message.get("text") or message.get("caption") or "").strip()
        mentioned = _extract_assets(text, assets)
        if not text or not mentioned:
            continue
        chat = message.get("chat") if isinstance(message.get("chat"), Mapping) else {}
        author = message.get("from") if isinstance(message.get("from"), Mapping) else {}
        records.append(
            _record(
                source="telegram",
                message_id=str(message.get("message_id") or update.get("update_id") or ""),
                timestamp=_unix_timestamp(message.get("date")),
                author_key=str(author.get("id") or chat.get("id") or ""),
                channel_key=str(chat.get("id") or ""),
                text=text,
                assets=mentioned,
            )
        )
    return records


def parse_x_recent_search(
    payload: Mapping[str, Any],
    assets: Iterable[str],
) -> list[SocialSignalRecord]:
    """Parse posts from the official X recent-search API response."""
    records: list[SocialSignalRecord] = []
    data = payload.get("data")
    if not isinstance(data, list):
        return records
    for post in data:
        if not isinstance(post, Mapping):
            continue
        text = str(post.get("text") or "").strip()
        mentioned = _extract_assets(text, assets)
        if not text or not mentioned:
            continue
        metrics = post.get("public_metrics")
        engagement = {
            str(key): int(value)
            for key, value in metrics.items()
            if isinstance(metrics, Mapping)
            and isinstance(key, str)
            and isinstance(value, (int, float))
        }
        records.append(
            _record(
                source="x",
                message_id=str(post.get("id") or ""),
                timestamp=str(post.get("created_at") or _utc_timestamp()),
                author_key=str(post.get("author_id") or ""),
                channel_key="public_search",
                text=text,
                assets=mentioned,
                engagement=engagement,
            )
        )
    return records


async def collect_authorized_signals(
    settings: SocialSignalSettings,
    assets: Iterable[str],
    *,
    session: aiohttp.ClientSession | None = None,
) -> list[SocialSignalRecord]:
    """Read permitted official APIs and append normalized evidence records."""
    if not settings.enabled:
        return []
    owns_session = session is None
    session = session or aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=15))
    try:
        records: list[SocialSignalRecord] = []
        if settings.telegram_enabled:
            if not settings.telegram_bot_token.strip():
                raise SocialSignalError("Telegram collection enabled without bot token")
            url = (
                "https://api.telegram.org/bot"
                + settings.telegram_bot_token.strip()
                + "/getUpdates"
            )
            params = {
                "timeout": "0",
                "allowed_updates": json.dumps(["message", "channel_post"]),
            }
            async with session.get(url, params=params) as response:
                response.raise_for_status()
                records.extend(parse_telegram_updates(await response.json(), assets))
        if settings.x_enabled:
            if not settings.x_bearer_token.strip() or not settings.x_query.strip():
                raise SocialSignalError("X collection enabled without bearer token/query")
            headers = {"Authorization": f"Bearer {settings.x_bearer_token.strip()}"}
            params = {
                "query": settings.x_query.strip(),
                "max_results": "100",
                "tweet.fields": "created_at,author_id,public_metrics",
            }
            async with session.get(
                "https://api.x.com/2/tweets/search/recent",
                params=params,
                headers=headers,
            ) as response:
                response.raise_for_status()
                records.extend(parse_x_recent_search(await response.json(), assets))
        if settings.ai_enabled and records:
            records = HeadlessSocialAnalyzer(settings).classify(records)
        write_social_audit(Path(settings.audit_path), records)
        return records
    finally:
        if owns_session:
            await session.close()


def write_social_audit(path: Path, records: Sequence[SocialSignalRecord]) -> Path:
    """Append local read-only social records without storing author identifiers."""
    if not records:
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record.as_dict(), sort_keys=True) + "\n")
    return path


def social_evidence_for_assets(
    settings: SocialSignalSettings,
    assets: Iterable[str],
    *,
    now: datetime | None = None,
) -> dict[str, dict[str, Any]]:
    """Summarize recent stored records for candidate destination assets."""
    normalized_assets = tuple(dict.fromkeys(item.strip().upper() for item in assets if item.strip()))
    if not settings.enabled:
        return {asset: {"status": "disabled"} for asset in normalized_assets}
    rows = _load_records(Path(settings.audit_path))
    current = now or datetime.now(timezone.utc)
    since = current - timedelta(minutes=settings.max_age_minutes)
    results: dict[str, dict[str, Any]] = {}
    for asset in normalized_assets:
        matching = [
            row for row in rows
            if asset in row.assets and (_parse_timestamp(row.timestamp) or current) >= since
        ]
        matching = matching[-settings.max_records_per_asset :] if settings.max_records_per_asset else matching
        source_types = sorted({row.source for row in matching})
        manipulation = max((row.manipulation_risk for row in matching), default=0.0)
        bullish = sum(row.label == SOCIAL_LABEL_BULLISH for row in matching)
        bearish = sum(row.label == SOCIAL_LABEL_BEARISH for row in matching)
        blocked = settings.block_manipulation_risk and manipulation >= settings.manipulation_block_score
        results[asset] = {
            "status": "ready" if matching else "empty",
            "asset": asset,
            "mention_count": len(matching),
            "source_diversity": len(source_types),
            "source_types": source_types,
            "bullish_mentions": bullish,
            "bearish_mentions": bearish,
            "bullish_bearish_balance": bullish - bearish,
            "manipulation_risk_score": manipulation,
            "min_source_diversity_met": len(source_types) >= settings.min_source_diversity,
            "block_route": blocked,
            "recent_signals": [
                {
                    "timestamp": row.timestamp,
                    "source": row.source,
                    "label": row.label,
                    "confidence": row.confidence,
                    "excerpt": row.excerpt,
                }
                for row in matching[-3:]
            ],
        }
    return results


def _record(
    *,
    source: str,
    message_id: str,
    timestamp: str,
    author_key: str,
    channel_key: str,
    text: str,
    assets: tuple[str, ...],
    engagement: dict[str, int] | None = None,
) -> SocialSignalRecord:
    label, confidence, manipulation = _classify_text(text)
    return SocialSignalRecord(
        source=source,
        message_id=message_id,
        timestamp=timestamp,
        author_hash=_hash_identity(author_key),
        channel_hash=_hash_identity(channel_key),
        excerpt=" ".join(text.split())[:280],
        urls=tuple(_URL_RE.findall(text)[:4]),
        assets=assets,
        label=label,
        confidence=confidence,
        manipulation_risk=manipulation,
        engagement=engagement or {},
    )


def _classify_text(text: str) -> tuple[str, float, float]:
    lowered = text.lower()
    manip_hits = sum(term in lowered for term in _MANIPULATION_TERMS)
    bull_hits = sum(term in lowered for term in _BULLISH_TERMS)
    bear_hits = sum(term in lowered for term in _BEARISH_TERMS)
    if manip_hits:
        return SOCIAL_LABEL_MANIPULATION, min(0.65 + manip_hits * 0.12, 1.0), min(0.65 + manip_hits * 0.12, 1.0)
    if bull_hits > bear_hits:
        return SOCIAL_LABEL_BULLISH, min(0.55 + bull_hits * 0.1, 0.95), 0.0
    if bear_hits > bull_hits:
        return SOCIAL_LABEL_BEARISH, min(0.55 + bear_hits * 0.1, 0.95), 0.2
    return SOCIAL_LABEL_NEUTRAL, 0.4, 0.0


def _extract_assets(text: str, assets: Iterable[str]) -> tuple[str, ...]:
    found: list[str] = []
    for asset in assets:
        normalized = asset.strip().upper()
        if not normalized:
            continue
        pattern = re.compile(rf"(?<![A-Z0-9])[$#]?{re.escape(normalized)}(?![A-Z0-9])")
        if pattern.search(text.upper()):
            found.append(normalized)
    return tuple(dict.fromkeys(found))


def _load_records(path: Path) -> list[SocialSignalRecord]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    records: list[SocialSignalRecord] = []
    for line in lines[-2000:]:
        try:
            item = json.loads(line)
            records.append(
                SocialSignalRecord(
                    source=str(item["source"]),
                    message_id=str(item.get("message_id") or ""),
                    timestamp=str(item["timestamp"]),
                    author_hash=str(item.get("author_hash") or ""),
                    channel_hash=str(item.get("channel_hash") or ""),
                    excerpt=str(item.get("excerpt") or ""),
                    urls=tuple(item.get("urls") or ()),
                    assets=tuple(str(asset).upper() for asset in item.get("assets") or ()),
                    label=str(item.get("label") or SOCIAL_LABEL_NEUTRAL),
                    confidence=float(item.get("confidence") or 0.0),
                    manipulation_risk=float(item.get("manipulation_risk") or 0.0),
                    engagement=dict(item.get("engagement") or {}),
                )
            )
        except (KeyError, TypeError, ValueError, json.JSONDecodeError):
            continue
    return records


def _hash_identity(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:16] if value else ""


def _unix_timestamp(value: Any) -> str:
    try:
        return datetime.fromtimestamp(float(value), tz=timezone.utc).isoformat()
    except (TypeError, ValueError, OSError):
        return _utc_timestamp()


def _parse_timestamp(value: str) -> datetime | None:
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc)
    except ValueError:
        return None


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


def _json_object(raw: str) -> Mapping[str, Any]:
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SocialSignalError("social signal AI output was not JSON") from exc
    if isinstance(payload, Mapping) and isinstance(payload.get("structured_output"), Mapping):
        payload = payload["structured_output"]
    if not isinstance(payload, Mapping):
        raise SocialSignalError("social signal AI output was not an object")
    return payload


def _bounded_float(value: Any, default: float) -> float:
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return default
    return min(max(parsed, 0.0), 1.0) if math.isfinite(parsed) else default


def _env_bool(name: str, default: bool) -> bool:
    value = os.getenv(name)
    return default if value is None else value.strip().lower() in {"1", "true", "yes", "on"}


def _env_int(name: str, default: int) -> int:
    value = os.getenv(name)
    return default if value is None else int(value.strip())


def _env_float(name: str, default: float) -> float:
    value = os.getenv(name)
    return default if value is None else float(value.strip())


def run(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Collect read-only crypto social evidence.")
    parser.add_argument("--collect", action="store_true", help="collect enabled official API sources")
    parser.add_argument("--asset", action="append", default=[], help="asset ticker to extract")
    parser.add_argument("--evidence-only", action="store_true", help="print stored evidence only")
    args = parser.parse_args(argv)
    settings = SocialSignalSettings.from_env()
    if args.collect:
        records = asyncio.run(collect_authorized_signals(settings, args.asset))
        print(json.dumps({"records_collected": len(records), "audit_path": settings.audit_path}))
    if args.evidence_only or not args.collect:
        print(json.dumps(social_evidence_for_assets(settings, args.asset), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

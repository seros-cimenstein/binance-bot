"""Telegram channel signal ingester."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import signal
import sys
from collections import OrderedDict
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from loguru import logger

from src.alerts import maybe_telegram
from src.signals.candidate import SignalCandidate
from src.signals.common import (
    AlertRateLimiter,
    append_ledger,
    atomic_write_json,
    emergency_stop_active,
    utc_ms,
)


EVM_ADDRESS_RE = re.compile(r"\b0x[a-fA-F0-9]{40}\b")
SOLANA_ADDRESS_RE = re.compile(r"\b[1-9A-HJ-NP-Za-km-z]{32,44}\b")
DEDUP_WINDOW_MS = 3_600_000
CHANNEL_RELOAD_INTERVAL_SECONDS = 300.0
STOP_CHECK_INTERVAL_SECONDS = 30.0
DEFAULT_CHANNELS_PAYLOAD = {
    "channels": [
        {"username": "binance_announcements", "weight": 0.4},
        {"username": "WatcherGuru", "weight": 0.3},
        {"username": "coingecko", "weight": 0.2},
    ],
    "_note": (
        "Edit this file to add private/curated alpha groups. weight \u2208 [0,1] "
        "feeds confidence. Username = @handle without the @."
    ),
}


@dataclass(frozen=True, slots=True)
class TelegramSettings:
    api_id: str
    api_hash: str
    session_path: str = "data/telegram-session.session"
    channels_path: str = "data/telegram-channels.json"
    max_alerts_per_hour: int = 30
    stop_path: str = "data/signals-telegram.stop"
    ledger_path: str = "data/signals-ledger.jsonl"
    watchlist_path: str = "data/signals-watchlist.json"
    watchlist_ttl_hours: float = 12.0


@dataclass(frozen=True, slots=True)
class TelegramChannel:
    username: str
    weight: float


@dataclass(frozen=True, slots=True)
class TelegramChannelList:
    channels: tuple[TelegramChannel, ...]
    mtime_ns: int | None = None

    def usernames(self) -> list[str]:
        return [channel.username for channel in self.channels]

    def weight_for(self, username: str) -> float:
        normalized = _normalize_username(username)
        for channel in self.channels:
            if channel.username.lower() == normalized.lower():
                return channel.weight
        return 0.5


@dataclass(frozen=True, slots=True)
class DetectedAddress:
    chain: str
    address: str


class TelegramConfigError(RuntimeError):
    pass


def settings_from_env() -> TelegramSettings:
    return TelegramSettings(
        api_id=os.getenv("TELEGRAM_API_ID", ""),
        api_hash=os.getenv("TELEGRAM_API_HASH", ""),
        session_path=os.getenv(
            "TELEGRAM_SESSION_PATH",
            "data/telegram-session.session",
        ),
        channels_path=os.getenv(
            "SIGNALS_TELEGRAM_CHANNELS_PATH",
            "data/telegram-channels.json",
        ),
        max_alerts_per_hour=_env_int("SIGNALS_TELEGRAM_MAX_ALERTS_PER_HOUR", 30),
        stop_path=os.getenv(
            "SIGNALS_TELEGRAM_STOP_PATH",
            "data/signals-telegram.stop",
        ),
        ledger_path=os.getenv("SIGNALS_LEDGER_PATH", "data/signals-ledger.jsonl"),
        watchlist_path=os.getenv(
            "SIGNALS_WATCHLIST_PATH",
            "data/signals-watchlist.json",
        ),
        watchlist_ttl_hours=_env_float("SIGNALS_WATCHLIST_TTL_HOURS", 12.0),
    )


def extract_contract_addresses(text: str) -> list[DetectedAddress]:
    detections: list[DetectedAddress] = []
    seen: set[tuple[str, str]] = set()
    matches: list[tuple[int, DetectedAddress]] = []

    for match in EVM_ADDRESS_RE.finditer(text):
        matches.append((
            match.start(),
            DetectedAddress(chain="ethereum", address=match.group(0)),
        ))

    for match in SOLANA_ADDRESS_RE.finditer(text):
        address = match.group(0)
        if not _looks_like_solana_pubkey(address):
            continue
        matches.append((match.start(), DetectedAddress(chain="solana", address=address)))

    for _, detected in sorted(matches, key=lambda item: item[0]):
        key = (detected.chain, detected.address)
        if key in seen:
            continue
        seen.add(key)
        detections.append(detected)
    return detections


def load_channel_list(path: str | Path) -> TelegramChannelList:
    p = Path(path)
    if not p.exists():
        atomic_write_json(p, DEFAULT_CHANNELS_PAYLOAD)
        logger.warning(
            "Created default Telegram channel list at "
            f"{p}. Signal quality from these public news channels will be LOW; "
            "replace them with vetted alpha groups."
        )

    try:
        data = json.loads(p.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as exc:
        logger.warning(f"Telegram channel list malformed at {p}: {exc}")
        return TelegramChannelList(channels=(), mtime_ns=_mtime_ns(p))

    raw_channels = data.get("channels", []) if isinstance(data, dict) else []
    if not isinstance(raw_channels, list):
        return TelegramChannelList(channels=(), mtime_ns=_mtime_ns(p))

    channels: list[TelegramChannel] = []
    seen_usernames: set[str] = set()
    for item in raw_channels:
        channel = _parse_channel(item)
        if channel is None:
            continue
        key = channel.username.lower()
        if key in seen_usernames:
            continue
        seen_usernames.add(key)
        channels.append(channel)
    return TelegramChannelList(channels=tuple(channels), mtime_ns=_mtime_ns(p))


def dedup_allows(
    seen: OrderedDict[tuple[str, str], int],
    *,
    channel_id: str,
    address: str,
    now_ms: int,
) -> bool:
    cutoff = now_ms - DEDUP_WINDOW_MS
    while seen:
        oldest_key = next(iter(seen))
        if seen[oldest_key] > cutoff:
            break
        seen.popitem(last=False)

    key = (str(channel_id), address)
    if key in seen:
        return False
    seen[key] = now_ms
    return True


async def process_new_message(
    event: Any,
    *,
    settings: TelegramSettings,
    channels: TelegramChannelList,
    seen: OrderedDict[tuple[str, str], int],
    watchlist: list[dict[str, str]],
    alert_limiter: AlertRateLimiter,
) -> list[SignalCandidate]:
    text = _event_text(event)
    detected_addresses = extract_contract_addresses(text)
    if not detected_addresses:
        return []

    channel_username = _event_channel_username(event)
    channel_id = _event_channel_id(event)
    message_id = _event_message_id(event)
    candidates: list[SignalCandidate] = []

    for detected in detected_addresses:
        now_ms = utc_ms()
        if not dedup_allows(
            seen,
            channel_id=channel_id,
            address=detected.address,
            now_ms=now_ms,
        ):
            continue

        candidate = SignalCandidate(
            source="telegram",
            chain=detected.chain,
            token_address=detected.address,
            token_symbol="",
            detected_at_ms=now_ms,
            raw_signal={
                "channel_username": channel_username,
                "channel_id": channel_id,
                "message_id": message_id,
                "message_text": text[:500],
                "message_url": f"https://t.me/{channel_username}/{message_id}",
            },
            confidence=channels.weight_for(channel_username),
        )
        append_ledger(settings.ledger_path, candidate.as_dict())
        _add_watchlist_entry(watchlist, candidate, settings=settings)
        candidates.append(candidate)
        await _alert_candidate(candidate, alert_limiter)

    if candidates:
        _prune_watchlist(watchlist, utc_ms())
        _write_watchlist(settings.watchlist_path, watchlist)
    return candidates


async def run_detector(
    *,
    settings: TelegramSettings | None = None,
    once: bool = False,
) -> None:
    if settings is None:
        settings = settings_from_env()

    channels = load_channel_list(settings.channels_path)
    _validate_settings(settings)
    seen: OrderedDict[tuple[str, str], int] = OrderedDict()
    watchlist = _load_watchlist(settings.watchlist_path)
    alert_limiter = AlertRateLimiter(settings.max_alerts_per_hour)
    telegram_client_cls, events = _load_telethon()
    client = telegram_client_cls(
        settings.session_path,
        int(settings.api_id),
        settings.api_hash,
    )

    async def _handle(event: Any) -> None:
        try:
            await process_new_message(
                event,
                settings=settings,
                channels=channels,
                seen=seen,
                watchlist=watchlist,
                alert_limiter=alert_limiter,
            )
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 - daemon should keep listening
            logger.warning(f"Telegram signal message processing failed: {exc}")

    event_builder = None
    try:
        await client.connect()
        if not await client.is_user_authorized():
            raise TelegramConfigError(
                "Telegram session is not authorized. Run "
                "python -m src.signals.telegram_setup in an interactive shell."
            )

        event_builder = _register_channel_handler(client, events, _handle, channels)
        if once:
            await _catch_up_if_available(client)
            return

        last_channel_poll_ms = 0
        while True:
            if emergency_stop_active(settings.stop_path):
                logger.info(f"Telegram signal stop file present: {settings.stop_path}")
                return
            now_ms = utc_ms()
            if now_ms - last_channel_poll_ms >= int(
                CHANNEL_RELOAD_INTERVAL_SECONDS * 1000
            ):
                next_channels = load_channel_list(settings.channels_path)
                if next_channels.mtime_ns != channels.mtime_ns:
                    if event_builder is not None:
                        client.remove_event_handler(_handle, event_builder)
                    channels = next_channels
                    event_builder = _register_channel_handler(
                        client,
                        events,
                        _handle,
                        channels,
                    )
                    logger.info(
                        "Reloaded Telegram channel list: "
                        f"{len(channels.channels)} channels"
                    )
                last_channel_poll_ms = now_ms
            try:
                await asyncio.wait_for(
                    asyncio.shield(client.disconnected),
                    timeout=STOP_CHECK_INTERVAL_SECONDS,
                )
                return
            except TimeoutError:
                continue
    finally:
        if event_builder is not None:
            try:
                client.remove_event_handler(_handle, event_builder)
            except Exception:  # noqa: BLE001 - best effort cleanup
                pass
        await client.disconnect()


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the Telegram signal ingester.")
    parser.add_argument(
        "--once",
        action="store_true",
        help="Process pending Telegram events and exit.",
    )
    parser.add_argument("--daemon", action="store_true", help="Run continuously (default).")
    args = parser.parse_args(argv)

    async def _main() -> None:
        task = asyncio.create_task(run_detector(once=args.once))
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                loop.add_signal_handler(sig, task.cancel)
            except NotImplementedError:
                pass
        try:
            await task
        except asyncio.CancelledError:
            logger.info("Telegram signal ingester stopped by signal")

    try:
        asyncio.run(_main())
    except TelegramConfigError as exc:
        logger.error(str(exc))
        return 2
    return 0


async def _alert_candidate(
    candidate: SignalCandidate,
    alert_limiter: AlertRateLimiter,
) -> None:
    now_ms = utc_ms()
    if not alert_limiter.allow(now_ms):
        logger.debug(
            "Telegram signal alert suppressed by detector rate limit "
            f"for {candidate.token_address}"
        )
        return
    msg = (
        "Telegram signal\n"
        f"Channel: {candidate.raw_signal.get('channel_username', '')}\n"
        f"Chain: {candidate.chain}\n"
        f"Address: {candidate.token_address}\n"
        f"Confidence: {candidate.confidence:.3f}"
    )
    await maybe_telegram(msg)


def _validate_settings(settings: TelegramSettings) -> None:
    if not settings.api_id or not settings.api_hash:
        raise TelegramConfigError(
            "TELEGRAM_API_ID and TELEGRAM_API_HASH are required. Create them at "
            "my.telegram.org/apps, then run python -m src.signals.telegram_setup."
        )
    try:
        int(settings.api_id)
    except ValueError as exc:
        raise TelegramConfigError("TELEGRAM_API_ID must be an integer.") from exc
    if not Path(settings.session_path).exists():
        raise TelegramConfigError(
            f"Telegram session file missing: {settings.session_path}. Run "
            "python -m src.signals.telegram_setup in an interactive shell first."
        )


def _register_channel_handler(
    client: Any,
    events: Any,
    handler: Callable[[Any], Any],
    channels: TelegramChannelList,
) -> Any | None:
    usernames = channels.usernames()
    if not usernames:
        logger.warning("Telegram channel list is empty; no channels subscribed")
        return None
    builder = events.NewMessage(chats=usernames)
    client.add_event_handler(handler, builder)
    logger.info(f"Listening for Telegram signals on {len(usernames)} channels")
    return builder


async def _catch_up_if_available(client: Any) -> None:
    catch_up = getattr(client, "catch_up", None)
    if callable(catch_up):
        result = catch_up()
        if hasattr(result, "__await__"):
            await result
    await asyncio.sleep(0)


def _load_telethon() -> tuple[Any, Any]:
    try:
        from telethon import TelegramClient, events
    except ImportError as exc:
        raise TelegramConfigError(
            "telethon is required for the Telegram signal ingester. "
            "Install requirements.txt first."
        ) from exc
    return TelegramClient, events


def _parse_channel(item: Any) -> TelegramChannel | None:
    if not isinstance(item, dict):
        return None
    username = item.get("username")
    weight = item.get("weight", 0.5)
    if not isinstance(username, str):
        return None
    username = _normalize_username(username)
    if not username:
        return None
    if not isinstance(weight, int | float) or isinstance(weight, bool):
        return None
    weight_float = float(weight)
    if weight_float < 0.0 or weight_float > 1.0:
        return None
    return TelegramChannel(username=username, weight=weight_float)


def _looks_like_solana_pubkey(address: str) -> bool:
    return (
        len(address) >= 40
        and any(char.isupper() for char in address)
        and any(char.isdigit() for char in address)
    )


def _event_text(event: Any) -> str:
    message = getattr(event, "message", None)
    for value in (
        getattr(message, "text", None),
        getattr(message, "message", None),
        getattr(event, "raw_text", None),
        getattr(event, "text", None),
    ):
        if isinstance(value, str):
            return value
    return ""


def _event_channel_username(event: Any) -> str:
    for obj in (getattr(event, "chat", None), getattr(event, "sender", None)):
        username = getattr(obj, "username", None)
        if isinstance(username, str):
            return _normalize_username(username)
    return ""


def _event_channel_id(event: Any) -> str:
    message = getattr(event, "message", None)
    for value in (
        getattr(event, "chat_id", None),
        getattr(message, "chat_id", None),
        getattr(getattr(event, "chat", None), "id", None),
        getattr(event, "sender_id", None),
    ):
        if value is not None:
            return str(value)
    return ""


def _event_message_id(event: Any) -> str:
    message = getattr(event, "message", None)
    for value in (getattr(event, "id", None), getattr(message, "id", None)):
        if value is not None:
            return str(value)
    return ""


def _normalize_username(username: str) -> str:
    return username.strip().lstrip("@")


def _add_watchlist_entry(
    watchlist: list[dict[str, str]],
    candidate: SignalCandidate,
    *,
    settings: TelegramSettings,
) -> None:
    detected_at_ms = candidate.detected_at_ms
    expires_at_ms = detected_at_ms + int(settings.watchlist_ttl_hours * 3_600_000)
    watchlist.append({
        "token": candidate.token_address,
        "chain": candidate.chain,
        "source": candidate.source,
        "detected_at_iso": _iso_from_ms(detected_at_ms),
        "expires_at_iso": _iso_from_ms(expires_at_ms),
    })


def _write_watchlist(path: str | Path, watchlist: list[dict[str, str]]) -> None:
    atomic_write_json(path, {"watching": watchlist})


def _load_watchlist(path: str | Path) -> list[dict[str, str]]:
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return []
    watching = data.get("watching", []) if isinstance(data, dict) else []
    if not isinstance(watching, list):
        return []
    return [item for item in watching if isinstance(item, dict)]


def _prune_watchlist(watchlist: list[dict[str, str]], now_ms: int) -> None:
    kept: list[dict[str, str]] = []
    for item in watchlist:
        expires_at = _parse_iso_ms(str(item.get("expires_at_iso", "")))
        if expires_at is None or expires_at > now_ms:
            kept.append(item)
    watchlist[:] = kept


def _iso_from_ms(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).isoformat()


def _parse_iso_ms(value: str) -> int | None:
    if not value:
        return None
    try:
        dt = datetime.fromisoformat(value)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1000)


def _mtime_ns(path: Path) -> int | None:
    try:
        return path.stat().st_mtime_ns
    except OSError:
        return None


def _env_float(key: str, default: float) -> float:
    value = os.getenv(key)
    if value is None:
        return default
    try:
        return float(value)
    except ValueError:
        logger.warning(f"Ignoring invalid float env {key}={value!r}")
        return default


def _env_int(key: str, default: int) -> int:
    value = os.getenv(key)
    if value is None:
        return default
    try:
        return int(value)
    except ValueError:
        logger.warning(f"Ignoring invalid integer env {key}={value!r}")
        return default


if __name__ == "__main__":
    raise SystemExit(run(sys.argv[1:]))

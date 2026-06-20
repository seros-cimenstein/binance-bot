"""DexScreener signal detector."""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
import signal
import sys
from collections import OrderedDict, deque
from collections.abc import Iterable
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import aiohttp
from loguru import logger

from src.alerts import maybe_telegram
from src.signals.candidate import SignalCandidate
from src.signals.common import (
    append_ledger,
    atomic_write_json,
    dedupe_seen_addresses,
    emergency_stop_active,
    utc_ms,
)


TOKEN_PROFILES_URL = "https://api.dexscreener.com/token-profiles/latest/v1"
TOKEN_PAIRS_URL = "https://api.dexscreener.com/latest/dex/tokens/{token_address}"


@dataclass(frozen=True, slots=True)
class DexScreenerSettings:
    poll_interval_seconds: float = 30.0
    max_age_hours: float = 24.0
    min_liquidity_usd: float = 5_000.0
    min_volume_usd_24h: float = 10_000.0
    ledger_path: str = "data/signals-ledger.jsonl"
    watchlist_path: str = "data/signals-watchlist.json"
    watchlist_ttl_hours: float = 12.0
    max_alerts_per_hour: int = 20
    stop_path: str = "data/signals-dexscreener.stop"


class AlertRateLimiter:
    """Rolling one-hour limiter for detector-local Telegram alerts."""

    def __init__(self, max_per_hour: int) -> None:
        self.max_per_hour = max(0, int(max_per_hour))
        self.sent_at_ms: deque[int] = deque()

    def allow(self, now_ms: int) -> bool:
        cutoff = now_ms - 3_600_000
        while self.sent_at_ms and self.sent_at_ms[0] < cutoff:
            self.sent_at_ms.popleft()
        if len(self.sent_at_ms) >= self.max_per_hour:
            return False
        self.sent_at_ms.append(now_ms)
        return True


def settings_from_env() -> DexScreenerSettings:
    return DexScreenerSettings(
        poll_interval_seconds=_env_float(
            "SIGNALS_DEXSCREENER_POLL_INTERVAL_SECONDS", 30.0,
        ),
        max_age_hours=_env_float("SIGNALS_DEXSCREENER_MAX_AGE_HOURS", 24.0),
        min_liquidity_usd=_env_float(
            "SIGNALS_DEXSCREENER_MIN_LIQUIDITY_USD", 5_000.0,
        ),
        min_volume_usd_24h=_env_float(
            "SIGNALS_DEXSCREENER_MIN_VOLUME_USD_24H", 10_000.0,
        ),
        ledger_path=os.getenv("SIGNALS_LEDGER_PATH", "data/signals-ledger.jsonl"),
        watchlist_path=os.getenv(
            "SIGNALS_WATCHLIST_PATH", "data/signals-watchlist.json",
        ),
        watchlist_ttl_hours=_env_float("SIGNALS_WATCHLIST_TTL_HOURS", 12.0),
        max_alerts_per_hour=_env_int(
            "SIGNALS_DEXSCREENER_MAX_ALERTS_PER_HOUR", 20,
        ),
        stop_path=os.getenv(
            "SIGNALS_DEXSCREENER_STOP_PATH", "data/signals-dexscreener.stop",
        ),
    )


async def fetch_token_profiles(session: aiohttp.ClientSession) -> list[dict[str, Any]]:
    try:
        data = await _get_json(session, TOKEN_PROFILES_URL)
    except Exception as exc:  # noqa: BLE001 - detector should keep running
        logger.warning(f"DexScreener token profiles fetch failed: {exc}")
        return []
    if not isinstance(data, list):
        logger.warning("DexScreener token profiles response was not a list")
        return []
    return [item for item in data if isinstance(item, dict)]


async def fetch_token_pairs(
    session: aiohttp.ClientSession,
    token_address: str,
) -> list[dict[str, Any]]:
    url = TOKEN_PAIRS_URL.format(token_address=token_address)
    try:
        data = await _get_json(session, url)
    except Exception as exc:  # noqa: BLE001 - detector should keep running
        logger.warning(f"DexScreener token pairs fetch failed for {token_address}: {exc}")
        return []
    if not isinstance(data, dict):
        logger.warning(f"DexScreener token pairs response was not an object: {token_address}")
        return []
    pairs = data.get("pairs", [])
    if not isinstance(pairs, list):
        logger.warning(f"DexScreener token pairs list malformed: {token_address}")
        return []
    return [pair for pair in pairs if isinstance(pair, dict)]


async def run_cycle(
    *,
    session: aiohttp.ClientSession,
    settings: DexScreenerSettings,
    seen: OrderedDict[str, None],
    watchlist: list[dict[str, str]],
    alert_limiter: AlertRateLimiter,
) -> list[SignalCandidate]:
    now_ms = utc_ms()
    _prune_watchlist(watchlist, now_ms)

    try:
        profiles = await fetch_token_profiles(session)
    except Exception as exc:  # noqa: BLE001 - tests may monkeypatch fetchers to fail
        logger.warning(f"DexScreener token profiles fetch failed: {exc}")
        _write_watchlist(settings.watchlist_path, watchlist)
        return []
    if not isinstance(profiles, list):
        logger.warning("DexScreener token profiles response was not a list")
        _write_watchlist(settings.watchlist_path, watchlist)
        return []

    addresses = _profile_addresses(profiles)
    new_addresses = dedupe_seen_addresses(seen, addresses)
    candidates: list[SignalCandidate] = []

    for address in new_addresses:
        try:
            pairs = await fetch_token_pairs(session, address)
        except Exception as exc:  # noqa: BLE001 - keep processing other tokens
            logger.warning(f"DexScreener token pairs fetch failed for {address}: {exc}")
            continue
        if not isinstance(pairs, list):
            logger.warning(f"DexScreener token pairs response malformed for {address}")
            continue

        candidate = _candidate_from_pairs(pairs, settings=settings, now_ms=now_ms)
        if candidate is None:
            continue

        append_ledger(settings.ledger_path, candidate.as_dict())
        candidates.append(candidate)
        _add_watchlist_entry(watchlist, candidate, settings=settings)
        await _alert_candidate(candidate, alert_limiter)

    _prune_watchlist(watchlist, utc_ms())
    _write_watchlist(settings.watchlist_path, watchlist)
    return candidates


async def run_detector(
    *,
    settings: DexScreenerSettings | None = None,
    once: bool = False,
) -> None:
    if settings is None:
        settings = settings_from_env()

    seen: OrderedDict[str, None] = OrderedDict()
    watchlist = _load_watchlist(settings.watchlist_path)
    alert_limiter = AlertRateLimiter(settings.max_alerts_per_hour)
    timeout = aiohttp.ClientTimeout(total=10)

    async with aiohttp.ClientSession(timeout=timeout) as session:
        while True:
            if emergency_stop_active(settings.stop_path):
                logger.info(f"DexScreener stop file present: {settings.stop_path}")
                return
            try:
                await run_cycle(
                    session=session,
                    settings=settings,
                    seen=seen,
                    watchlist=watchlist,
                    alert_limiter=alert_limiter,
                )
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001 - daemon should continue
                logger.warning(f"DexScreener cycle failed: {exc}")
            if once:
                return
            await asyncio.sleep(settings.poll_interval_seconds)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the DexScreener signal detector.")
    parser.add_argument("--once", action="store_true", help="Run one polling cycle and exit.")
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
            logger.info("DexScreener detector stopped by signal")

    asyncio.run(_main())
    return 0


async def _get_json(session: aiohttp.ClientSession, url: str) -> Any:
    async with session.get(url) as resp:
        resp.raise_for_status()
        return await resp.json()


def _candidate_from_pairs(
    pairs: Iterable[dict[str, Any]],
    *,
    settings: DexScreenerSettings,
    now_ms: int,
) -> SignalCandidate | None:
    max_age_ms = settings.max_age_hours * 3_600_000
    qualifying: list[tuple[float, dict[str, Any]]] = []
    for pair in pairs:
        if not isinstance(pair, dict):
            continue
        pair_created_at = _as_int(pair.get("pairCreatedAt"))
        liquidity_usd = _as_float(_nested(pair, "liquidity", "usd"))
        volume_h24 = _as_float(_nested(pair, "volume", "h24"))
        if pair_created_at < now_ms - max_age_ms:
            continue
        if liquidity_usd < settings.min_liquidity_usd:
            continue
        if volume_h24 < settings.min_volume_usd_24h:
            continue
        qualifying.append((liquidity_usd, pair))
    if not qualifying:
        return None

    liquidity_usd, pair = max(qualifying, key=lambda item: item[0])
    volume_h24 = _as_float(_nested(pair, "volume", "h24"))
    base_token = pair.get("baseToken", {})
    if not isinstance(base_token, dict):
        base_token = {}
    raw_signal = dict(pair)
    raw_signal["dexscreener_pair_url"] = str(pair.get("url", ""))
    return SignalCandidate(
        source="dexscreener",
        chain=str(pair.get("chainId", "")),
        token_address=str(base_token.get("address", "")),
        token_symbol=str(base_token.get("symbol", "")),
        detected_at_ms=utc_ms(),
        raw_signal=raw_signal,
        # Confidence: liquidity strength above $5k, scaled by 24h volume to $100k.
        confidence=_clamp01(
            min(1.0, math.log10(max(liquidity_usd, 1.0) / 5_000.0) / 2.0)
            * min(1.0, volume_h24 / 100_000.0)
        ),
    )


async def _alert_candidate(
    candidate: SignalCandidate,
    alert_limiter: AlertRateLimiter,
) -> None:
    now_ms = utc_ms()
    if not alert_limiter.allow(now_ms):
        logger.debug(
            "DexScreener alert suppressed by detector rate limit "
            f"for {candidate.token_address}"
        )
        return
    msg = (
        "DexScreener signal\n"
        f"Token: {candidate.token_symbol or candidate.token_address}\n"
        f"Chain: {candidate.chain}\n"
        f"Address: {candidate.token_address}\n"
        f"Confidence: {candidate.confidence:.3f}"
    )
    await maybe_telegram(msg)


def _add_watchlist_entry(
    watchlist: list[dict[str, str]],
    candidate: SignalCandidate,
    *,
    settings: DexScreenerSettings,
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


def _profile_addresses(profiles: Iterable[dict[str, Any]]) -> list[str]:
    addresses: list[str] = []
    for profile in profiles:
        if not isinstance(profile, dict):
            continue
        address = profile.get("tokenAddress")
        if isinstance(address, str) and address:
            addresses.append(address)
    return addresses


def _nested(data: dict[str, Any], key: str, subkey: str) -> Any:
    item = data.get(key, {})
    if not isinstance(item, dict):
        return None
    return item.get(subkey)


def _as_float(value: Any) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return 0.0


def _as_int(value: Any) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def _clamp01(value: float) -> float:
    return max(0.0, min(1.0, value))


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

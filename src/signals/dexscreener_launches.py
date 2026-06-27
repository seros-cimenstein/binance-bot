"""DexScreener V2 early-launch signal detector."""

from __future__ import annotations

import argparse
import asyncio
import os
import signal
import sys
from collections import OrderedDict
from collections.abc import Iterable
from dataclasses import dataclass
from typing import Any

import aiohttp
from loguru import logger

from src.signals.common import append_ledger, emergency_stop_active, utc_ms


TOKEN_PROFILES_URL = "https://api.dexscreener.com/token-profiles/latest/v1"
TOKEN_PAIRS_URL = "https://api.dexscreener.com/latest/dex/tokens/{token_address}"
WSOL_MINT = "So11111111111111111111111111111111111111112"
SEEN_ADDRESS_LIMIT = 5_000
TOP_ASSET_SYMBOLS = {"SOL", "BTC", "ETH", "USDC", "USDT", "BONK", "WIF", "TRUMP"}


@dataclass(frozen=True, slots=True)
class LaunchDetectorSettings:
    """Configuration for the V2 early-launch detector."""

    poll_interval_seconds: float = 60.0
    max_pair_age_seconds: float = 1_800.0
    min_liquidity_usd: float = 3_000.0
    max_liquidity_usd: float = 200_000.0
    ledger_path: str = "data/signals-v2-ledger.jsonl"
    filter_version: str = "v2-launch-2026-06-27"
    stop_path: str = "data/signals-v2-stop"


def settings_from_env() -> LaunchDetectorSettings:
    """Build detector settings from environment variables."""

    return LaunchDetectorSettings(
        poll_interval_seconds=_env_float("SIGNALS_V2_POLL_INTERVAL_SECONDS", 60.0),
        max_pair_age_seconds=_env_float(
            "SIGNALS_V2_MAX_PAIR_AGE_SECONDS",
            1_800.0,
        ),
        min_liquidity_usd=_env_float("SIGNALS_V2_MIN_LIQUIDITY_USD", 3_000.0),
        max_liquidity_usd=_env_float("SIGNALS_V2_MAX_LIQUIDITY_USD", 200_000.0),
        ledger_path=os.getenv("SIGNALS_V2_LEDGER_PATH", "data/signals-v2-ledger.jsonl"),
        filter_version=os.getenv(
            "SIGNALS_V2_FILTER_VERSION",
            "v2-launch-2026-06-27",
        ),
        stop_path=os.getenv("SIGNALS_V2_STOP_FILE", "data/signals-v2-stop"),
    )


async def fetch_latest_profiles(session: aiohttp.ClientSession) -> list[dict[str, Any]]:
    """Fetch the latest DexScreener token profiles."""

    try:
        data = await _get_json(session, TOKEN_PROFILES_URL)
    except Exception as exc:  # noqa: BLE001 - daemon should keep running
        logger.warning(f"V2 DexScreener token profiles fetch failed: {exc}")
        return []
    if not isinstance(data, list):
        logger.warning("V2 DexScreener token profiles response was not a list")
        return []
    return [item for item in data if isinstance(item, dict)]


async def fetch_token_pairs(
    session: aiohttp.ClientSession,
    token_address: str,
) -> list[dict[str, Any]]:
    """Fetch DexScreener pairs for a token address."""

    url = TOKEN_PAIRS_URL.format(token_address=token_address)
    try:
        data = await _get_json(session, url)
    except Exception as exc:  # noqa: BLE001 - keep processing other tokens
        logger.warning(f"V2 DexScreener token pairs fetch failed for {token_address}: {exc}")
        return []
    if not isinstance(data, dict):
        logger.warning(f"V2 DexScreener token pairs response was not an object: {token_address}")
        return []
    pairs = data.get("pairs", [])
    if not isinstance(pairs, list):
        logger.warning(f"V2 DexScreener token pairs list malformed: {token_address}")
        return []
    return [pair for pair in pairs if isinstance(pair, dict)]


def extract_launch_features(
    pair: dict[str, Any],
    *,
    now_ms: int,
) -> dict[str, Any]:
    """Extract V2 launch-quality features from a pair payload."""

    base_token = pair.get("baseToken", {})
    if not isinstance(base_token, dict):
        base_token = {}
    quote_token = pair.get("quoteToken", {})
    if not isinstance(quote_token, dict):
        quote_token = {}

    pair_created_at_ms = _as_int(pair.get("pairCreatedAt"))
    pair_age_seconds = (now_ms - pair_created_at_ms) / 1000.0 if pair_created_at_ms else -1.0
    liquidity_usd = _as_float(_nested(pair, "liquidity", "usd"))
    volume_m5 = _as_float(_nested(pair, "volume", "m5"))

    return {
        "chain": str(pair.get("chainId", "")),
        "token_address": str(base_token.get("address", "")),
        "token_name": str(base_token.get("name", "")),
        "token_symbol": str(base_token.get("symbol", "")),
        "pair_address": str(pair.get("pairAddress", "")),
        "quote_address": str(quote_token.get("address", "")),
        "quote_symbol": str(quote_token.get("symbol", "")),
        "pair_created_at_ms": pair_created_at_ms,
        "pair_age_seconds": pair_age_seconds,
        "liquidity_usd": liquidity_usd,
        "price_usd": _as_float(pair.get("priceUsd")),
        "volume_m5": volume_m5,
        "volume_h1": _as_float(_nested(pair, "volume", "h1")),
        "volume_h6": _as_float(_nested(pair, "volume", "h6")),
        "volume_h24": _as_float(_nested(pair, "volume", "h24")),
        "has_social_link": _has_social_link(pair),
        "creator_recent_rug": "unknown",
        "liquidity_lock": "unknown",
        "wash_volume_ratio_m5_liquidity": (
            volume_m5 / liquidity_usd if liquidity_usd > 0.0 else None
        ),
    }


def qualifies_launch_pair(
    features: dict[str, Any],
    *,
    settings: LaunchDetectorSettings,
) -> tuple[bool, list[str]]:
    """Return whether a pair passes V2 filters and any rejection reasons."""

    reasons: list[str] = []
    if str(features.get("chain", "")).lower() != "solana":
        reasons.append("non_solana_chain")
    if not _is_wsol_quote(
        str(features.get("quote_address", "")),
        str(features.get("quote_symbol", "")),
    ):
        reasons.append("non_wsol_quote")

    pair_age_seconds = _as_float(features.get("pair_age_seconds"))
    if pair_age_seconds < 0.0 or pair_age_seconds > settings.max_pair_age_seconds:
        reasons.append("pair_age_out_of_range")

    liquidity_usd = _as_float(features.get("liquidity_usd"))
    if liquidity_usd < settings.min_liquidity_usd:
        reasons.append("liquidity_too_low")
    if liquidity_usd > settings.max_liquidity_usd:
        reasons.append("liquidity_too_high")

    if not bool(features.get("has_social_link")):
        reasons.append("missing_social_link")

    if _scam_text_reject(
        str(features.get("token_symbol", "")),
        str(features.get("token_name", "")),
    ):
        reasons.append("scam_text_pattern")

    volume_m5 = _as_float(features.get("volume_m5"))
    if 0.0 <= pair_age_seconds <= 600.0 and liquidity_usd > 0.0:
        if volume_m5 > 20.0 * liquidity_usd:
            reasons.append("wash_volume_spike")

    return not reasons, reasons


def build_detection_record(
    pair: dict[str, Any],
    features: dict[str, Any],
    *,
    detected_at_ms: int,
    filter_version: str = "v2-launch-2026-06-27",
) -> dict[str, Any]:
    """Build a V2 ledger row for a qualifying launch."""

    pair_address = str(features.get("pair_address", ""))
    return {
        "detection_id": f"v2-solana-{pair_address}-{detected_at_ms}",
        "filter_version": filter_version,
        "detected_at_ms": detected_at_ms,
        "chain": "solana",
        "token_address": str(features.get("token_address", "")),
        "pair_address": pair_address,
        "quote_address": str(features.get("quote_address", "")),
        "features": dict(features),
        "rejection_reasons": [],
        "creator_recent_rug": "unknown",
        "raw_pair": _public_pair_payload(pair),
    }


async def run_cycle(
    *,
    session: aiohttp.ClientSession,
    settings: LaunchDetectorSettings,
    seen: OrderedDict[str, None],
) -> list[dict[str, Any]]:
    """Run one V2 polling cycle and return newly detected launches."""

    now_ms = utc_ms()
    profiles = await fetch_latest_profiles(session)
    profile_by_address = _solana_profiles_by_address(profiles)
    addresses = _dedupe_seen_addresses(seen, profile_by_address.keys())
    detections: list[dict[str, Any]] = []

    for address in addresses:
        pairs = await fetch_token_pairs(session, address)
        pair = _best_wsol_solana_pair(pairs)
        if pair is None:
            continue
        pair = dict(pair)
        pair["__token_profile"] = profile_by_address.get(address, {})
        features = extract_launch_features(pair, now_ms=now_ms)
        qualifies, reasons = qualifies_launch_pair(features, settings=settings)
        if not qualifies:
            logger.debug(f"V2 launch rejected {address}: {','.join(reasons)}")
            continue
        record = build_detection_record(
            pair,
            features,
            detected_at_ms=now_ms,
            filter_version=settings.filter_version,
        )
        append_ledger(settings.ledger_path, record)
        detections.append(record)

    if detections:
        logger.info(f"V2 DexScreener emitted {len(detections)} launch detections")
    return detections


async def run_detector(
    *,
    settings: LaunchDetectorSettings | None = None,
    once: bool = False,
) -> None:
    """Run the V2 detector loop."""

    if settings is None:
        settings = settings_from_env()

    seen: OrderedDict[str, None] = OrderedDict()
    timeout = aiohttp.ClientTimeout(total=10)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        while True:
            if emergency_stop_active(settings.stop_path):
                logger.info(f"V2 DexScreener stop file present: {settings.stop_path}")
                return
            try:
                await run_cycle(session=session, settings=settings, seen=seen)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001 - daemon should continue
                logger.warning(f"V2 DexScreener cycle failed: {exc}")
            if once:
                return
            await asyncio.sleep(settings.poll_interval_seconds)


def run(argv: list[str] | None = None) -> int:
    """CLI entry point for the V2 launch detector."""

    parser = argparse.ArgumentParser(description="Run the V2 DexScreener launch detector.")
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
            logger.info("V2 DexScreener detector stopped by signal")

    asyncio.run(_main())
    return 0


def main() -> None:
    raise SystemExit(run(sys.argv[1:]))


async def _get_json(session: aiohttp.ClientSession, url: str) -> Any:
    async with session.get(url) as resp:
        resp.raise_for_status()
        return await resp.json()


def _solana_profiles_by_address(
    profiles: Iterable[dict[str, Any]],
) -> OrderedDict[str, dict[str, Any]]:
    profile_by_address: OrderedDict[str, dict[str, Any]] = OrderedDict()
    for profile in profiles:
        chain = str(profile.get("chainId", "")).lower()
        if chain and chain != "solana":
            continue
        address = profile.get("tokenAddress")
        if isinstance(address, str) and address:
            profile_by_address.setdefault(address, profile)
    return profile_by_address


def _dedupe_seen_addresses(
    seen: OrderedDict[str, None],
    addresses: Iterable[str],
) -> list[str]:
    fresh: list[str] = []
    for address in addresses:
        if address in seen:
            continue
        seen[address] = None
        fresh.append(address)
        while len(seen) > SEEN_ADDRESS_LIMIT:
            seen.popitem(last=False)
    return fresh


def _best_wsol_solana_pair(pairs: Iterable[dict[str, Any]]) -> dict[str, Any] | None:
    qualifying: list[tuple[float, dict[str, Any]]] = []
    for pair in pairs:
        if str(pair.get("chainId", "")).lower() != "solana":
            continue
        quote_token = pair.get("quoteToken", {})
        if not isinstance(quote_token, dict):
            quote_token = {}
        if not _is_wsol_quote(
            str(quote_token.get("address", "")),
            str(quote_token.get("symbol", "")),
        ):
            continue
        qualifying.append((_as_float(_nested(pair, "liquidity", "usd")), pair))
    if not qualifying:
        return None
    return max(qualifying, key=lambda item: item[0])[1]


def _is_wsol_quote(address: str, symbol: str) -> bool:
    if address == WSOL_MINT:
        return True
    return not address and symbol.upper() in {"SOL", "WSOL"}


def _has_social_link(pair: dict[str, Any]) -> bool:
    info = pair.get("info", {})
    if isinstance(info, dict) and _payload_has_url(info):
        return True
    profile = pair.get("__token_profile", {})
    return isinstance(profile, dict) and _payload_has_url(profile)


def _payload_has_url(value: Any) -> bool:
    if isinstance(value, str):
        lowered = value.lower()
        return lowered.startswith(("http://", "https://", "t.me/"))
    if isinstance(value, dict):
        return any(_payload_has_url(item) for item in value.values())
    if isinstance(value, list):
        return any(_payload_has_url(item) for item in value)
    return False


def _scam_text_reject(symbol: str, name: str) -> bool:
    symbol_clean = symbol.strip()
    name_clean = name.strip()
    if not symbol_clean or not name_clean:
        return True
    symbol_upper = symbol_clean.upper()
    if len(symbol_upper) > 12:
        return True
    if len(symbol_upper) >= 6 and len(set(symbol_upper)) == 1:
        return True
    if any(fragment in symbol_upper for fragment in ("HTTP", "WWW", ".COM", "T.ME")):
        return True
    if symbol_upper in TOP_ASSET_SYMBOLS:
        return True
    for asset in TOP_ASSET_SYMBOLS:
        if symbol_upper in {f"{asset}$", f"{asset}2.0", f"${asset}"}:
            return True
    return False


def _public_pair_payload(pair: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in pair.items() if not str(key).startswith("__")}


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


def _env_float(key: str, default: float) -> float:
    value = os.getenv(key)
    if value is None:
        return default
    try:
        return float(value)
    except ValueError:
        logger.warning(f"Ignoring invalid float env {key}={value!r}")
        return default


if __name__ == "__main__":
    main()

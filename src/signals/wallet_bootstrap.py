"""Smart-money wallet list bootstrapper."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import aiohttp
from loguru import logger

from src.signals.common import atomic_write_json, utc_ms


BIRDEYE_TOP_TRADERS_URL = (
    "https://public-api.birdeye.so/trader/gainers-losers"
    "?type=1W&sort_by=PnL&sort_type=desc&offset=0&limit=10"
)
EMPTY_WALLET_NOTE = (
    "No smart-money wallets are configured. Set BIRDEYE_API_KEY to refresh from "
    "Birdeye, or hand-edit this file and add wallets with source='manual'."
)


def load_wallets(path: str | Path) -> list[dict]:
    """Read current wallets from disk; returns [] if file missing/malformed."""

    payload = _load_payload(path)
    wallets = payload.get("wallets", []) if isinstance(payload, dict) else []
    if not isinstance(wallets, list):
        return []
    return [item for item in wallets if isinstance(item, dict)]


async def refresh_wallets_if_stale(
    path: str | Path,
    *,
    session: aiohttp.ClientSession,
    max_age_hours: float = 168.0,
    birdeye_api_key: str | None = None,
) -> list[dict]:
    """Refresh from Birdeye if stale; preserve manual entries; return new list."""

    p = Path(path)
    payload = _load_payload(p)
    current_wallets = load_wallets(p)
    last_refreshed_ms = _as_int(payload.get("last_refreshed_ms")) if payload else 0
    max_age_ms = int(max_age_hours * 3_600_000)
    if last_refreshed_ms and last_refreshed_ms >= utc_ms() - max_age_ms:
        return current_wallets

    manual_wallets = [
        wallet for wallet in current_wallets if wallet.get("source") == "manual"
    ]

    birdeye_wallets: list[dict] = []
    if birdeye_api_key:
        try:
            birdeye_wallets = await _fetch_birdeye_wallets(session, birdeye_api_key)
        except Exception as exc:  # noqa: BLE001 - bootstrap should not wipe on outage
            logger.warning(f"Birdeye smart-money wallet refresh failed: {exc}")
            return current_wallets

    wallets = _dedupe_wallets([*manual_wallets, *birdeye_wallets])
    if not wallets and not birdeye_api_key:
        logger.warning(
            "Smart-money wallet list is empty and BIRDEYE_API_KEY is not set; "
            "set the key or hand-edit the wallet list."
        )
        payload_to_write = {
            "wallets": [],
            "last_refreshed_ms": utc_ms(),
            "_note": EMPTY_WALLET_NOTE,
        }
        atomic_write_json(p, payload_to_write)
        return []

    if not wallets:
        logger.warning(
            "Birdeye smart-money wallet refresh returned no usable wallets; "
            "hand-edit the wallet list if this schema changed."
        )

    payload_to_write = {
        "wallets": wallets,
        "last_refreshed_ms": utc_ms(),
        "_note": (
            "Manual entries are preserved across refreshes. Birdeye entries are "
            "rebuilt from the 7-day top-traders endpoint when stale."
        ),
    }
    atomic_write_json(p, payload_to_write)
    return wallets


async def _fetch_birdeye_wallets(
    session: aiohttp.ClientSession,
    api_key: str,
) -> list[dict]:
    headers = {"X-API-KEY": api_key}
    async with session.get(BIRDEYE_TOP_TRADERS_URL, headers=headers) as resp:
        resp.raise_for_status()
        data = await resp.json()

    rows = _extract_birdeye_rows(data)
    wallets: list[dict] = []
    for index, row in enumerate(rows[:10], start=1):
        pubkey = _extract_pubkey(row)
        if not pubkey:
            logger.warning(f"Birdeye trader row #{index} did not include a wallet pubkey")
            continue
        wallets.append({
            "pubkey": pubkey,
            "label": f"birdeye-top-7d-#{index}",
            "source": "birdeye",
            "weight": 0.6,
        })
    return wallets


def _extract_birdeye_rows(data: Any) -> list[dict[str, Any]]:
    if isinstance(data, list):
        return [item for item in data if isinstance(item, dict)]
    if not isinstance(data, dict):
        logger.warning("Birdeye smart-money response was not a list/object")
        return []

    candidates: list[Any] = [data.get("data"), data.get("result")]
    for candidate in candidates:
        if isinstance(candidate, list):
            return [item for item in candidate if isinstance(item, dict)]
        if isinstance(candidate, dict):
            nested = _first_list(candidate, ("items", "list", "traders", "wallets", "data"))
            if nested:
                return nested

    nested = _first_list(data, ("items", "list", "traders", "wallets"))
    if nested:
        return nested
    logger.warning("Birdeye smart-money response schema did not include trader rows")
    return []


def _first_list(data: dict[str, Any], keys: tuple[str, ...]) -> list[dict[str, Any]]:
    for key in keys:
        value = data.get(key)
        if isinstance(value, list):
            return [item for item in value if isinstance(item, dict)]
    return []


def _extract_pubkey(row: dict[str, Any]) -> str:
    keys = (
        "pubkey",
        "wallet",
        "walletAddress",
        "wallet_address",
        "owner",
        "ownerAddress",
        "trader",
        "traderAddress",
        "address",
    )
    for key in keys:
        value = row.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
        if isinstance(value, dict):
            nested = _extract_pubkey(value)
            if nested:
                return nested
    return ""


def _dedupe_wallets(wallets: list[dict]) -> list[dict]:
    deduped: list[dict] = []
    seen: set[str] = set()
    for wallet in wallets:
        pubkey = wallet.get("pubkey")
        if not isinstance(pubkey, str) or not pubkey:
            continue
        if pubkey in seen:
            continue
        seen.add(pubkey)
        deduped.append(dict(wallet))
    return deduped


def _load_payload(path: str | Path) -> dict[str, Any]:
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return {}
    return data if isinstance(data, dict) else {}


def _as_int(value: Any) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0

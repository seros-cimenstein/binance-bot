"""On-chain activity snapshots + Telegram digest of pending airdrop tasks.

Two ideas, both deliberately low-tech:

1. **Activity detection** — every wallet has a monotonic counter on-chain
   (EVM transaction count, Solana signature count). We snapshot it weekly.
   If this week's counter > the most-recent snapshot from a prior week,
   the wallet did something — auto-mark every pending opportunity for
   that wallet's chain as completed. The operator did the work; we save
   them the bookkeeping.

2. **Telegram digest** — render a compact HTML message listing what is
   still pending per wallet, including the *official* URL so the operator
   can verify before connecting MetaMask.

Wire-up: ``python -m src.airdrop.scheduler remind`` calls ``run_reminder()``
once. Drive it from a daily systemd timer.
"""

from __future__ import annotations

import asyncio
import json
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Sequence

import aiohttp
from loguru import logger

from src.airdrop.catalog import AirdropOpportunity, filter_by
from src.airdrop.scheduler import (
    CEX_CHAIN,
    ChecklistRow,
    CompletionState,
    TaskKey,
    build_checklist,
    current_iso_week,
    load_state,
    save_state,
)
from src.airdrop.wallet import (
    CHAIN_EVM,
    CHAIN_SOLANA,
    AirdropWalletError,
    EncryptedWallet,
    WalletStore,
    load_store,
)


_EVM_BASE_RPC_DEFAULT = "https://mainnet.base.org"
_SOLANA_RPC_DEFAULT = "https://api.mainnet-beta.solana.com"
_HTTP_TIMEOUT = aiohttp.ClientTimeout(total=15)
_TELEGRAM_API_URL = "https://api.telegram.org/bot{token}/sendMessage"


# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class ReminderSettings:
    evm_rpc_url: str = _EVM_BASE_RPC_DEFAULT
    solana_rpc_url: str = _SOLANA_RPC_DEFAULT
    min_confidence: str = "medium"
    telegram_bot_token: str = ""
    telegram_chat_id: str = ""

    @classmethod
    def from_env(cls) -> "ReminderSettings":
        return cls(
            evm_rpc_url=os.environ.get("AIRDROP_EVM_RPC_URL", _EVM_BASE_RPC_DEFAULT),
            solana_rpc_url=os.environ.get("AIRDROP_SOLANA_RPC_URL", _SOLANA_RPC_DEFAULT),
            min_confidence=os.environ.get("AIRDROP_MIN_CONFIDENCE", "medium"),
            telegram_bot_token=os.environ.get("TELEGRAM_BOT_TOKEN", ""),
            telegram_chat_id=os.environ.get("TELEGRAM_CHAT_ID", ""),
        )


# ---------------------------------------------------------------------------
# Activity snapshot store
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class ActivitySnapshot:
    """One wallet's activity counter at a point in time."""

    wallet_name: str
    chain: str
    address: str
    counter: int            # EVM nonce or Solana signature count
    captured_at_iso: str    # full ISO timestamp
    iso_week: str           # e.g. "2026-W22"

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ActivitySnapshot":
        try:
            return cls(
                wallet_name=str(payload["wallet_name"]),
                chain=str(payload["chain"]),
                address=str(payload["address"]),
                counter=int(payload["counter"]),
                captured_at_iso=str(payload["captured_at_iso"]),
                iso_week=str(payload["iso_week"]),
            )
        except KeyError as exc:
            raise AirdropWalletError(f"activity snapshot malformed: missing {exc}") from exc


@dataclass(slots=True)
class ActivityStore:
    """Per-wallet history of snapshots."""

    snapshots: dict[str, list[ActivitySnapshot]] = field(default_factory=dict)

    def latest(self, wallet_name: str) -> ActivitySnapshot | None:
        history = self.snapshots.get(wallet_name)
        return history[-1] if history else None

    def previous_in_iso_week(
        self, wallet_name: str, *, iso_week: str,
    ) -> ActivitySnapshot | None:
        """Most-recent snapshot from a week STRICTLY older than ``iso_week``."""
        history = self.snapshots.get(wallet_name) or []
        for snap in reversed(history):
            if snap.iso_week != iso_week:
                return snap
        return None

    def record(self, snap: ActivitySnapshot) -> None:
        history = self.snapshots.setdefault(snap.wallet_name, [])
        # Repeated runs within the same ISO week overwrite the tail
        # rather than appending.
        if history and history[-1].iso_week == snap.iso_week:
            history[-1] = snap
        else:
            history.append(snap)
        # Keep ~3 months of history per wallet to bound the file size.
        del history[:-12]

    def to_dict(self) -> dict[str, Any]:
        return {
            "snapshots": {
                name: [s.to_dict() for s in history]
                for name, history in self.snapshots.items()
            },
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ActivityStore":
        raw = payload.get("snapshots") if isinstance(payload, dict) else {}
        if not isinstance(raw, dict):
            return cls()
        snapshots: dict[str, list[ActivitySnapshot]] = {}
        for name, entries in raw.items():
            if not isinstance(entries, list):
                continue
            snapshots[str(name)] = [ActivitySnapshot.from_dict(e) for e in entries]
        return cls(snapshots=snapshots)


def load_activity_store(path: Path | str) -> ActivityStore:
    p = Path(path)
    if not p.exists():
        return ActivityStore()
    try:
        return ActivityStore.from_dict(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise AirdropWalletError(f"could not read activity store at {p}: {exc}") from exc


def save_activity_store(store: ActivityStore, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(store.to_dict(), indent=2, sort_keys=True))
    tmp.replace(p)


# ---------------------------------------------------------------------------
# RPC counter fetchers
# ---------------------------------------------------------------------------


async def _post_rpc(session: aiohttp.ClientSession, url: str, payload: dict) -> Any:
    async with session.post(url, json=payload) as resp:
        resp.raise_for_status()
        data = await resp.json()
        if "error" in data:
            raise AirdropWalletError(f"RPC error from {url}: {data['error']}")
        return data.get("result")


async def fetch_evm_nonce(
    *, session: aiohttp.ClientSession, rpc_url: str, address: str,
) -> int:
    """``eth_getTransactionCount`` returns the wallet's outgoing-tx nonce."""
    result = await _post_rpc(session, rpc_url, {
        "jsonrpc": "2.0", "id": 1, "method": "eth_getTransactionCount",
        "params": [address, "latest"],
    })
    if not isinstance(result, str):
        raise AirdropWalletError(f"unexpected nonce response: {result!r}")
    return int(result, 16)


async def fetch_solana_signature_count(
    *, session: aiohttp.ClientSession, rpc_url: str, address: str,
) -> int:
    """``getSignaturesForAddress`` returns the most-recent N sigs; we use
    ``len`` as a monotonic counter. Capped at 1000 by the RPC; for low-
    activity airdrop wallets that is effectively lifetime."""
    result = await _post_rpc(session, rpc_url, {
        "jsonrpc": "2.0", "id": 1, "method": "getSignaturesForAddress",
        "params": [address, {"limit": 1000}],
    })
    if not isinstance(result, list):
        raise AirdropWalletError(f"unexpected sigs response: {type(result)}")
    return len(result)


async def fetch_counter_for_wallet(
    *, session: aiohttp.ClientSession,
    wallet: EncryptedWallet, evm_rpc_url: str, solana_rpc_url: str,
) -> int:
    if wallet.chain == CHAIN_EVM:
        return await fetch_evm_nonce(session=session, rpc_url=evm_rpc_url,
                                      address=wallet.address)
    if wallet.chain == CHAIN_SOLANA:
        return await fetch_solana_signature_count(
            session=session, rpc_url=solana_rpc_url, address=wallet.address,
        )
    raise AirdropWalletError(f"unknown chain: {wallet.chain}")


# ---------------------------------------------------------------------------
# Detection + auto-mark
# ---------------------------------------------------------------------------


def wallet_was_active_this_week(
    *, current_counter: int, previous_snapshot: ActivitySnapshot | None,
) -> bool:
    """True if the wallet ran at least one new transaction since the
    most-recent snapshot from a prior week. Returns ``False`` on the very
    first run (no baseline to compare against)."""
    if previous_snapshot is None:
        return False
    return current_counter > previous_snapshot.counter


def auto_mark_completed_from_activity(
    *, completion_state: CompletionState,
    wallets_with_activity: Sequence[EncryptedWallet],
    iso_week: str, min_confidence: str | None = None,
) -> list[TaskKey]:
    """Mark every PENDING opportunity for every active wallet's chain as
    completed-this-week. Returns the keys we newly marked (so the caller
    can log / surface them)."""
    newly_marked: list[TaskKey] = []
    for wallet in wallets_with_activity:
        opps = filter_by(chain=wallet.chain, status="active",
                          min_confidence=min_confidence)
        for opp in opps:
            key = TaskKey(wallet_name=wallet.name, opportunity_name=opp.name)
            if completion_state.is_completed_this_week(
                key, current_iso_week=iso_week,
            ):
                continue
            completion_state.mark(key, iso_week=iso_week)
            newly_marked.append(key)
    return newly_marked


# ---------------------------------------------------------------------------
# Snapshot orchestration
# ---------------------------------------------------------------------------


async def snapshot_all_wallets(
    *, settings: ReminderSettings, store: WalletStore,
    activity_store: ActivityStore, iso_week: str,
    session: aiohttp.ClientSession | None = None,
) -> list[EncryptedWallet]:
    """Snapshot every wallet's counter, record into ``activity_store``,
    and return the subset of wallets that were ACTIVE this week."""
    own_session = session is None
    if own_session:
        session = aiohttp.ClientSession(timeout=_HTTP_TIMEOUT)
    try:
        active: list[EncryptedWallet] = []
        for wallet in store.wallets:
            prior = activity_store.previous_in_iso_week(
                wallet.name, iso_week=iso_week,
            )
            try:
                counter = await fetch_counter_for_wallet(
                    session=session, wallet=wallet,
                    evm_rpc_url=settings.evm_rpc_url,
                    solana_rpc_url=settings.solana_rpc_url,
                )
            except Exception as exc:
                # Public RPCs flake in many shapes (DNS, timeouts, JSON
                # parse errors, rate-limit 429s). Log + skip; the next run
                # will retry.
                logger.warning(
                    f"airdrop reminder: failed to fetch counter for {wallet.name}: {exc}"
                )
                continue
            snap = ActivitySnapshot(
                wallet_name=wallet.name, chain=wallet.chain,
                address=wallet.address, counter=counter, iso_week=iso_week,
                captured_at_iso=datetime.now(tz=timezone.utc).isoformat(),
            )
            activity_store.record(snap)
            if wallet_was_active_this_week(
                current_counter=counter, previous_snapshot=prior,
            ):
                active.append(wallet)
        return active
    finally:
        if own_session and session is not None:
            await session.close()


# ---------------------------------------------------------------------------
# Telegram digest rendering + send
# ---------------------------------------------------------------------------


def render_telegram_digest(
    *, iso_week: str, pending_rows: Sequence[ChecklistRow],
    active_wallet_names: Sequence[str],
) -> str:
    """Compact HTML message for Telegram. Lists pending rows grouped by
    wallet, and calls out wallets where activity was auto-detected."""
    lines: list[str] = [f"<b>Airdrop check-in</b> ({iso_week})", ""]
    if active_wallet_names:
        names = ", ".join(sorted(set(active_wallet_names)))
        lines.append(
            f"✅ Auto-detected on-chain activity: <code>{names}</code> — "
            "their pending tasks were marked done."
        )
        lines.append("")

    if not pending_rows:
        lines.append("Nothing pending. Nice.")
        return "\n".join(lines).strip()

    by_wallet: dict[str, list[ChecklistRow]] = {}
    for row in pending_rows:
        by_wallet.setdefault(row.wallet_name, []).append(row)

    for wallet_name, rows in sorted(by_wallet.items()):
        chain = rows[0].chain
        label = "Binance account" if chain == CEX_CHAIN else chain
        lines.append(f"<b>{wallet_name}</b> ({label}) — {len(rows)} pending:")
        for row in rows:
            lines.append(
                f"  • <a href=\"{row.url}\">{row.opportunity}</a>: "
                f"{row.activity_kind} "
                f"(min ${row.min_size_usd:.0f}, {row.confidence} confidence)"
            )
        lines.append("")
    return "\n".join(lines).strip()


async def send_telegram_digest(
    *, settings: ReminderSettings, message: str,
    session: aiohttp.ClientSession | None = None,
) -> bool:
    if not (settings.telegram_bot_token and settings.telegram_chat_id):
        logger.info(
            "airdrop reminder: TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID unset — "
            "skipping push and printing the digest instead."
        )
        print(message)
        return False
    own_session = session is None
    if own_session:
        session = aiohttp.ClientSession(timeout=_HTTP_TIMEOUT)
    try:
        url = _TELEGRAM_API_URL.format(token=settings.telegram_bot_token)
        payload = {
            "chat_id": settings.telegram_chat_id, "text": message,
            "parse_mode": "HTML", "disable_web_page_preview": True,
        }
        async with session.post(url, json=payload) as resp:
            if not resp.ok:
                body = await resp.text()
                logger.warning(
                    f"airdrop reminder: Telegram push failed "
                    f"({resp.status}): {body[:200]}"
                )
                return False
        return True
    except (aiohttp.ClientError, asyncio.TimeoutError) as exc:
        logger.warning(f"airdrop reminder: Telegram push error: {exc}")
        return False
    finally:
        if own_session and session is not None:
            await session.close()


# ---------------------------------------------------------------------------
# Top-level orchestrator
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class ReminderResult:
    iso_week: str
    active_wallets: list[str]
    auto_marked: list[str]
    pending_rows: int
    telegram_pushed: bool


async def run_reminder(
    *, settings: ReminderSettings, store_path: Path,
    state_path: Path, activity_path: Path,
    session: aiohttp.ClientSession | None = None,
    now: datetime | None = None,
) -> ReminderResult:
    """One-shot: snapshot → detect activity → auto-mark → push digest."""
    iso = current_iso_week(now)
    store = load_store(store_path)
    completion_state = load_state(state_path)
    activity_store = load_activity_store(activity_path)

    active_wallets = await snapshot_all_wallets(
        settings=settings, store=store, activity_store=activity_store,
        iso_week=iso, session=session,
    )
    save_activity_store(activity_store, activity_path)

    auto_marked = auto_mark_completed_from_activity(
        completion_state=completion_state,
        wallets_with_activity=active_wallets, iso_week=iso,
        min_confidence=settings.min_confidence,
    )
    save_state(completion_state, state_path)

    pending = [
        row for row in build_checklist(
            store=store, state=completion_state, iso_week=iso,
            min_confidence=settings.min_confidence, status_filter="active",
        )
        if row.status == "pending"
    ]

    digest = render_telegram_digest(
        iso_week=iso, pending_rows=pending,
        active_wallet_names=[w.name for w in active_wallets],
    )
    pushed = await send_telegram_digest(
        settings=settings, message=digest, session=session,
    )

    return ReminderResult(
        iso_week=iso,
        active_wallets=[w.name for w in active_wallets],
        auto_marked=[k.as_str() for k in auto_marked],
        pending_rows=len(pending),
        telegram_pushed=pushed,
    )

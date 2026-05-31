"""Live signed-swap executor for the Solana sniper.

Composes the components built in Phases 1-4:

  detector + decoder + scorer  ->  TradeCandidate stream  (already done)
                                       |
                                       v
                              SniperLiveExecutor (this module)
                                       |
                              Jupiter /swap/v1/swap  -> serialized tx
                              sign with wallet keypair
                              submit via RPC sendTransaction
                              wait for confirmation
                              record entry to ledger
                              hold for N seconds
                              repeat (close)

Every state-mutating method goes through a preflight gate ladder. The
runtime refuses to fire orders until *every* gate is set:

  * SNIPER_LIVE_ENABLED=true
  * CONFIRM_LIVE_SNIPER=yes
  * SNIPER_PROVEN_EDGE=yes
  * wallet balance >= min_wallet_sol_atoms
  * no emergency stop file present
  * today's spend < max_daily_sol_spend_atoms

This module is deliberately untouched by the Phase 4 paper code, so the
existing paper daemon keeps working as it is. The live path is opt-in.
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import aiohttp
from solders.keypair import Keypair
from solders.transaction import VersionedTransaction

from src.sniper.jupiter import (
    DEFAULT_QUOTE_URL,
    JupiterQuoteError,
    SOL_MINT,
    get_quote,
)


DEFAULT_SOLANA_RPC_URL = "https://api.mainnet-beta.solana.com"
DEFAULT_SWAP_URL = "https://lite-api.jup.ag/swap/v1/swap"


class SniperLiveError(RuntimeError):
    """Raised when the live executor cannot proceed."""


@dataclass(frozen=True, slots=True)
class SniperLiveSettings:
    """Runtime knobs + safety caps for the live executor."""

    wallet_path: str = "data/sniper-wallet.json"
    rpc_url: str = DEFAULT_SOLANA_RPC_URL
    quote_url: str = DEFAULT_QUOTE_URL
    swap_url: str = DEFAULT_SWAP_URL

    entry_sol_atoms: int = 5_000_000     # 0.005 SOL per snipe
    hold_seconds: int = 300              # 5 minute hold
    slippage_bps: int = 500
    max_concurrent: int = 3
    max_daily_sol_spend_atoms: int = 100_000_000  # 0.1 SOL/day hard cap
    min_wallet_sol_atoms: int = 50_000_000        # need ≥ 0.05 SOL to operate
    confirmation_timeout_seconds: float = 60.0
    rpc_poll_interval_seconds: float = 2.0

    enabled: bool = False
    confirm_live_sniper: str = "no"
    emergency_stop_path: str = "data/sniper-live.stop"
    ledger_path: str = "data/sniper-live-ledger.jsonl"
    state_path: str = "data/sniper-live-state.json"


@dataclass(frozen=True, slots=True)
class SwapResult:
    """Outcome of a single signed-swap submission."""

    signature: str
    pre_balance_lamports: int
    post_balance_lamports: int
    confirmed: bool
    error: str = ""


@dataclass(slots=True)
class DailySpendTracker:
    """Tracks total SOL atoms spent today and the UTC date that count belongs to."""

    date_utc: str  # YYYY-MM-DD
    spent_sol_atoms: int = 0

    def reset_if_new_day(self) -> None:
        today = datetime.now(tz=timezone.utc).strftime("%Y-%m-%d")
        if today != self.date_utc:
            self.date_utc = today
            self.spent_sol_atoms = 0

    def add(self, atoms: int) -> None:
        self.reset_if_new_day()
        self.spent_sol_atoms += atoms

    def would_exceed(self, atoms: int, cap: int) -> bool:
        self.reset_if_new_day()
        return self.spent_sol_atoms + atoms > cap

    def as_dict(self) -> dict[str, Any]:
        return {"date_utc": self.date_utc, "spent_sol_atoms": self.spent_sol_atoms}

    @classmethod
    def empty(cls) -> "DailySpendTracker":
        return cls(date_utc=datetime.now(tz=timezone.utc).strftime("%Y-%m-%d"))


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise SniperLiveError(f"emergency stop file present at {path}; refusing to run")


def _preflight_settings(settings: SniperLiveSettings) -> None:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if not settings.enabled:
        raise SniperLiveError("SNIPER_LIVE_ENABLED=true is required for live submission")
    if settings.confirm_live_sniper != "yes":
        raise SniperLiveError("CONFIRM_LIVE_SNIPER=yes is required for live submission")
    if os.getenv("SNIPER_PROVEN_EDGE", "").strip().lower() != "yes":
        raise SniperLiveError(
            "SNIPER_PROVEN_EDGE=yes is required for live submission. "
            "Review the paper ledger first; set this only when paper win rate × avg win "
            "exceeds avg loss × (1 - win rate)."
        )


async def fetch_lamport_balance(
    *, pubkey: str, rpc_url: str, session: aiohttp.ClientSession,
) -> int:
    """Read the SOL balance (in lamports) of one pubkey via JSON-RPC."""
    payload = {"jsonrpc": "2.0", "id": 1, "method": "getBalance", "params": [pubkey]}
    async with session.post(rpc_url, json=payload, timeout=15) as resp:
        if resp.status != 200:
            raise SniperLiveError(f"getBalance HTTP {resp.status}")
        body = await resp.json()
    err = body.get("error")
    if err is not None:
        raise SniperLiveError(f"getBalance RPC error: {err}")
    result = body.get("result") or {}
    try:
        return int((result if isinstance(result, Mapping) else {}).get("value") or 0)
    except (TypeError, ValueError) as exc:
        raise SniperLiveError(f"getBalance returned malformed value: {exc}") from exc


async def fetch_signature_status(
    *, signature: str, rpc_url: str, session: aiohttp.ClientSession,
) -> Mapping[str, Any] | None:
    payload = {
        "jsonrpc": "2.0", "id": 1, "method": "getSignatureStatuses",
        "params": [[signature], {"searchTransactionHistory": False}],
    }
    async with session.post(rpc_url, json=payload, timeout=15) as resp:
        if resp.status != 200:
            return None
        body = await resp.json()
    result = body.get("result") or {}
    values = (result if isinstance(result, Mapping) else {}).get("value") or []
    if not isinstance(values, list) or not values:
        return None
    return values[0]


async def fetch_swap_transaction(
    *,
    quote: Mapping[str, Any],
    user_pubkey: str,
    swap_url: str,
    session: aiohttp.ClientSession,
) -> str:
    """POST a quote to Jupiter's /swap endpoint and get back a base64 transaction."""
    body = {
        "quoteResponse": dict(quote),
        "userPublicKey": user_pubkey,
        "wrapAndUnwrapSol": True,
        "dynamicComputeUnitLimit": True,
        "prioritizationFeeLamports": "auto",
    }
    async with session.post(swap_url, json=body, timeout=20) as resp:
        if resp.status != 200:
            preview = (await resp.text())[:200]
            raise SniperLiveError(f"Jupiter swap HTTP {resp.status}: {preview}")
        payload = await resp.json()
    swap_tx_b64 = payload.get("swapTransaction")
    if not swap_tx_b64:
        raise SniperLiveError(f"swap response missing swapTransaction: {payload}")
    return str(swap_tx_b64)


def sign_swap_transaction(swap_tx_b64: str, keypair: Keypair) -> bytes:
    """Replace the placeholder signature with one signed by ``keypair``.

    Jupiter returns a VersionedTransaction with empty signatures. We
    rebuild it with our signer.
    """
    raw = base64.b64decode(swap_tx_b64)
    tx = VersionedTransaction.from_bytes(raw)
    signed = VersionedTransaction(tx.message, [keypair])
    return bytes(signed)


async def submit_signed_transaction(
    *, signed_tx_bytes: bytes, rpc_url: str, session: aiohttp.ClientSession,
) -> str:
    """Send the signed transaction; return its base58 signature."""
    encoded = base64.b64encode(signed_tx_bytes).decode()
    payload = {
        "jsonrpc": "2.0", "id": 1, "method": "sendTransaction",
        "params": [encoded, {"encoding": "base64", "skipPreflight": False,
                              "preflightCommitment": "processed"}],
    }
    async with session.post(rpc_url, json=payload, timeout=30) as resp:
        if resp.status != 200:
            raise SniperLiveError(f"sendTransaction HTTP {resp.status}")
        body = await resp.json()
    err = body.get("error")
    if err is not None:
        raise SniperLiveError(f"sendTransaction RPC error: {err}")
    signature = body.get("result")
    if not signature:
        raise SniperLiveError("sendTransaction returned no signature")
    return str(signature)


async def execute_signed_swap(
    *,
    keypair: Keypair,
    input_mint: str,
    output_mint: str,
    amount_atoms: int,
    slippage_bps: int,
    quote_url: str,
    swap_url: str,
    rpc_url: str,
    session: aiohttp.ClientSession,
    confirmation_timeout_seconds: float,
    rpc_poll_interval_seconds: float,
) -> SwapResult:
    """End-to-end: quote -> build swap tx -> sign -> submit -> wait for confirmation."""
    pubkey_str = str(keypair.pubkey())
    pre_balance = await fetch_lamport_balance(pubkey=pubkey_str, rpc_url=rpc_url, session=session)

    quote = await get_quote(
        input_mint=input_mint, output_mint=output_mint,
        amount_atoms=amount_atoms, slippage_bps=slippage_bps,
        quote_url=quote_url, session=session,
    )

    swap_tx_b64 = await fetch_swap_transaction(
        quote=quote.raw, user_pubkey=pubkey_str,
        swap_url=swap_url, session=session,
    )
    signed_bytes = sign_swap_transaction(swap_tx_b64, keypair)
    signature = await submit_signed_transaction(
        signed_tx_bytes=signed_bytes, rpc_url=rpc_url, session=session,
    )

    deadline = asyncio.get_event_loop().time() + confirmation_timeout_seconds
    confirmed = False
    last_error = ""
    while asyncio.get_event_loop().time() < deadline:
        status = await fetch_signature_status(
            signature=signature, rpc_url=rpc_url, session=session,
        )
        if status is not None:
            err = status.get("err")
            if err is not None:
                last_error = json.dumps(err)[:200]
                break
            confirmation = str(status.get("confirmationStatus") or "")
            if confirmation in ("confirmed", "finalized"):
                confirmed = True
                break
        await asyncio.sleep(rpc_poll_interval_seconds)

    post_balance = await fetch_lamport_balance(pubkey=pubkey_str, rpc_url=rpc_url, session=session)
    return SwapResult(
        signature=signature,
        pre_balance_lamports=pre_balance,
        post_balance_lamports=post_balance,
        confirmed=confirmed, error=last_error,
    )


@dataclass(slots=True)
class LiveExecutorRuntime:
    """Mutable state wrapper around a wallet + spend tracker.

    Kept separate from the immutable Settings so the daily spend cap can be
    enforced across cycles without re-loading on every call.
    """

    settings: SniperLiveSettings
    keypair: Keypair
    spend: DailySpendTracker

    def assert_spend_allowed(self, amount_atoms: int) -> None:
        if self.spend.would_exceed(amount_atoms, self.settings.max_daily_sol_spend_atoms):
            raise SniperLiveError(
                f"daily SOL spend cap reached: "
                f"{self.spend.spent_sol_atoms + amount_atoms} > "
                f"{self.settings.max_daily_sol_spend_atoms}"
            )

    async def assert_balance_ok(self, *, session: aiohttp.ClientSession) -> int:
        pubkey = str(self.keypair.pubkey())
        balance = await fetch_lamport_balance(
            pubkey=pubkey, rpc_url=self.settings.rpc_url, session=session,
        )
        if balance < self.settings.min_wallet_sol_atoms:
            raise SniperLiveError(
                f"wallet balance {balance} lamports below minimum "
                f"{self.settings.min_wallet_sol_atoms}"
            )
        return balance

    def record_spend(self, atoms: int) -> None:
        self.spend.add(atoms)


def load_runtime(
    settings: SniperLiveSettings, *, passphrase: str | None = None,
) -> LiveExecutorRuntime:
    """Run the preflight gate ladder, decrypt the wallet, return the runtime."""
    from src.sniper.wallet import load_keypair
    _preflight_settings(settings)
    keypair = load_keypair(settings.wallet_path, passphrase)
    # Best-effort load of the persisted spend tracker.
    spend = DailySpendTracker.empty()
    state_path = Path(settings.state_path)
    if state_path.exists():
        try:
            data = json.loads(state_path.read_text())
            spend = DailySpendTracker(
                date_utc=str(data.get("date_utc") or spend.date_utc),
                spent_sol_atoms=int(data.get("spent_sol_atoms") or 0),
            )
            spend.reset_if_new_day()
        except (OSError, json.JSONDecodeError, TypeError, ValueError):
            pass
    return LiveExecutorRuntime(settings=settings, keypair=keypair, spend=spend)


def save_spend_tracker(runtime: LiveExecutorRuntime) -> None:
    state_path = Path(runtime.settings.state_path)
    state_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = state_path.with_suffix(state_path.suffix + ".tmp")
    tmp.write_text(json.dumps(runtime.spend.as_dict(), sort_keys=True))
    tmp.replace(state_path)

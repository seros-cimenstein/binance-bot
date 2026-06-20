"""Solana smart-money wallet signal tracker."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import signal
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any

import aiohttp
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
from src.signals.wallet_bootstrap import refresh_wallets_if_stale


HELIUS_RPC_URL = "https://mainnet.helius-rpc.com/?api-key={api_key}"
TOKEN_PROGRAM_ID = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
WSOL_MINT = "So11111111111111111111111111111111111111112"
USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
USDT_MINT = "Es9vMFrzaCERmJfrF4H2FYD4KCo1cCfZwpz35K7RV8"
STABLE_OR_SOL_MINTS = {WSOL_MINT, USDC_MINT, USDT_MINT}


@dataclass(frozen=True, slots=True)
class SmartMoneySettings:
    helius_api_key: str
    poll_interval_seconds: float = 60.0
    wallet_list_path: str = "data/smart-money-wallets.json"
    state_path: str = "data/smart-money-state.json"
    max_alerts_per_hour: int = 15
    stop_path: str = "data/signals-smart-money.stop"
    refresh_hours: float = 168.0
    birdeye_api_key: str | None = None
    ledger_path: str = "data/signals-ledger.jsonl"
    watchlist_path: str = "data/signals-watchlist.json"
    watchlist_ttl_hours: float = 12.0


@dataclass(frozen=True, slots=True)
class WalletBuy:
    mint: str
    amount_atoms: str


class SmartMoneyConfigError(RuntimeError):
    pass


def settings_from_env() -> SmartMoneySettings:
    return SmartMoneySettings(
        helius_api_key=os.getenv("HELIUS_API_KEY", ""),
        poll_interval_seconds=_env_float(
            "SIGNALS_SMART_MONEY_POLL_INTERVAL_SECONDS", 60.0,
        ),
        wallet_list_path=os.getenv(
            "SIGNALS_SMART_MONEY_WALLET_LIST_PATH",
            "data/smart-money-wallets.json",
        ),
        state_path=os.getenv(
            "SIGNALS_SMART_MONEY_STATE_PATH",
            "data/smart-money-state.json",
        ),
        max_alerts_per_hour=_env_int("SIGNALS_SMART_MONEY_MAX_ALERTS_PER_HOUR", 15),
        stop_path=os.getenv(
            "SIGNALS_SMART_MONEY_STOP_PATH",
            "data/signals-smart-money.stop",
        ),
        refresh_hours=_env_float("SIGNALS_SMART_MONEY_REFRESH_HOURS", 168.0),
        birdeye_api_key=os.getenv("BIRDEYE_API_KEY") or None,
        ledger_path=os.getenv("SIGNALS_LEDGER_PATH", "data/signals-ledger.jsonl"),
        watchlist_path=os.getenv(
            "SIGNALS_WATCHLIST_PATH", "data/signals-watchlist.json",
        ),
        watchlist_ttl_hours=_env_float("SIGNALS_WATCHLIST_TTL_HOURS", 12.0),
    )


def detect_buy_mint(transaction: dict[str, Any], wallet_pubkey: str) -> str | None:
    buys = detect_wallet_buys(transaction, wallet_pubkey)
    return buys[0].mint if buys else None


def detect_wallet_buys(
    transaction: dict[str, Any],
    wallet_pubkey: str,
) -> list[WalletBuy]:
    result = transaction.get("result", transaction)
    if not isinstance(result, dict):
        return []

    own_accounts, account_mints = _wallet_token_accounts(result, wallet_pubkey)
    if not own_accounts:
        return []
    positive_mints = _positive_wallet_mints(result, wallet_pubkey)

    incoming: dict[str, str] = {}
    outgoing_mints: set[str] = set()

    # Helius returns jsonParsed Solana transactions. We intentionally use parsed
    # SPL transfer instructions instead of full swap decoding: an incoming
    # non-stable SPL transfer to a token account owned by the watched wallet is a
    # practical "buy" signal, unless the same transaction also sends that mint
    # from the wallet's token accounts.
    for instruction in _iter_token_transfer_instructions(result):
        info = _instruction_info(instruction)
        destination = _as_str(info.get("destination"))
        source = _as_str(info.get("source"))
        mint = (
            _as_str(info.get("mint"))
            or account_mints.get(destination, "")
            or account_mints.get(source, "")
        )
        amount = _amount_atoms(info)
        if not mint:
            continue
        if destination in own_accounts:
            incoming[mint] = _sum_amounts(incoming.get(mint, "0"), amount)
        if source in own_accounts:
            outgoing_mints.add(mint)

    buys: list[WalletBuy] = []
    for mint, amount in incoming.items():
        if mint in STABLE_OR_SOL_MINTS:
            continue
        if mint in outgoing_mints:
            continue
        if positive_mints and mint not in positive_mints:
            continue
        buys.append(WalletBuy(mint=mint, amount_atoms=amount))
    return buys


async def run_cycle(
    *,
    session: aiohttp.ClientSession,
    settings: SmartMoneySettings,
    watchlist: list[dict[str, str]],
    alert_limiter: AlertRateLimiter,
) -> list[SignalCandidate]:
    wallets = await refresh_wallets_if_stale(
        settings.wallet_list_path,
        session=session,
        max_age_hours=settings.refresh_hours,
        birdeye_api_key=settings.birdeye_api_key,
    )
    if not wallets:
        logger.warning("Smart-money wallet list is empty; sleeping until next cycle")
        _prune_watchlist(watchlist, utc_ms())
        _write_watchlist(settings.watchlist_path, watchlist)
        atomic_write_json(settings.state_path, _load_state(settings.state_path))
        return []

    state = _load_state(settings.state_path)
    candidates: list[SignalCandidate] = []

    for wallet in wallets:
        pubkey = _wallet_pubkey(wallet)
        if not pubkey:
            continue
        try:
            signatures = await fetch_wallet_signatures(
                session,
                settings=settings,
                wallet_pubkey=pubkey,
                until=state.get(pubkey),
            )
        except Exception as exc:  # noqa: BLE001 - keep processing other wallets
            logger.warning(f"Helius signature fetch failed for {pubkey}: {exc}")
            continue
        if signatures:
            state[pubkey] = signatures[0]

        for signature in signatures:
            try:
                transaction = await fetch_transaction(
                    session,
                    settings=settings,
                    signature=signature,
                )
            except Exception as exc:  # noqa: BLE001 - one bad tx should not stop cycle
                logger.warning(f"Helius transaction fetch failed for {signature}: {exc}")
                continue

            block_time_ms = _block_time_ms(transaction)
            for buy in detect_wallet_buys(transaction, pubkey):
                candidate = _candidate_from_buy(
                    wallet=wallet,
                    mint=buy.mint,
                    amount_atoms=buy.amount_atoms,
                    signature=signature,
                    block_time_ms=block_time_ms,
                )
                append_ledger(settings.ledger_path, candidate.as_dict())
                _add_watchlist_entry(watchlist, candidate, settings=settings)
                candidates.append(candidate)
                await _alert_candidate(candidate, alert_limiter)

    _prune_watchlist(watchlist, utc_ms())
    _write_watchlist(settings.watchlist_path, watchlist)
    atomic_write_json(settings.state_path, state)
    return candidates


async def fetch_wallet_signatures(
    session: aiohttp.ClientSession,
    *,
    settings: SmartMoneySettings,
    wallet_pubkey: str,
    until: str | None,
) -> list[str]:
    config: dict[str, Any] = {"limit": 25}
    # Solana RPC: `until` returns signatures NEWER than the cursor (search stops
    # when the cursor is reached). `before` would walk OLDER toward genesis, which
    # is the opposite of what we want for tracking new wallet activity.
    if until:
        config["until"] = until
    result = await _rpc(
        session,
        settings=settings,
        method="getSignaturesForAddress",
        params=[wallet_pubkey, config],
    )
    if not isinstance(result, list):
        logger.warning(f"Helius signatures response malformed for {wallet_pubkey}")
        return []
    signatures: list[str] = []
    for item in result:
        if isinstance(item, dict) and isinstance(item.get("signature"), str):
            signatures.append(item["signature"])
    return signatures


async def fetch_transaction(
    session: aiohttp.ClientSession,
    *,
    settings: SmartMoneySettings,
    signature: str,
) -> dict[str, Any]:
    result = await _rpc(
        session,
        settings=settings,
        method="getTransaction",
        params=[
            signature,
            {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0},
        ],
    )
    return {"result": result}


async def run_detector(
    *,
    settings: SmartMoneySettings | None = None,
    once: bool = False,
) -> None:
    if settings is None:
        settings = settings_from_env()
    _validate_settings(settings)

    watchlist = _load_watchlist(settings.watchlist_path)
    alert_limiter = AlertRateLimiter(settings.max_alerts_per_hour)
    timeout = aiohttp.ClientTimeout(total=15)

    async with aiohttp.ClientSession(timeout=timeout) as session:
        while True:
            if emergency_stop_active(settings.stop_path):
                logger.info(f"Smart-money stop file present: {settings.stop_path}")
                return
            try:
                await run_cycle(
                    session=session,
                    settings=settings,
                    watchlist=watchlist,
                    alert_limiter=alert_limiter,
                )
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001 - daemon should continue
                logger.warning(f"Smart-money cycle failed: {exc}")
            if once:
                return
            await asyncio.sleep(settings.poll_interval_seconds)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the smart-money signal tracker.")
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
            logger.info("Smart-money signal tracker stopped by signal")

    try:
        asyncio.run(_main())
    except SmartMoneyConfigError as exc:
        logger.error(str(exc))
        return 2
    return 0


async def _rpc(
    session: aiohttp.ClientSession,
    *,
    settings: SmartMoneySettings,
    method: str,
    params: list[Any],
) -> Any:
    payload = {
        "jsonrpc": "2.0",
        "id": method,
        "method": method,
        "params": params,
    }
    url = HELIUS_RPC_URL.format(api_key=settings.helius_api_key)
    async with session.post(url, json=payload) as resp:
        resp.raise_for_status()
        data = await resp.json()
    if not isinstance(data, dict):
        raise RuntimeError(f"Helius {method} response was not an object")
    if data.get("error"):
        raise RuntimeError(f"Helius {method} RPC error: {data['error']}")
    return data.get("result")


async def _alert_candidate(
    candidate: SignalCandidate,
    alert_limiter: AlertRateLimiter,
) -> None:
    now_ms = utc_ms()
    if not alert_limiter.allow(now_ms):
        logger.debug(
            "Smart-money alert suppressed by detector rate limit "
            f"for {candidate.token_address}"
        )
        return
    msg = (
        "Smart-money signal\n"
        f"Wallet: {candidate.raw_signal.get('wallet_label', '')}\n"
        f"Token: {candidate.token_address}\n"
        f"Signature: {candidate.raw_signal.get('signature', '')}\n"
        f"Confidence: {candidate.confidence:.3f}"
    )
    await maybe_telegram(msg)


def _candidate_from_buy(
    *,
    wallet: dict,
    mint: str,
    amount_atoms: str,
    signature: str,
    block_time_ms: int | None,
) -> SignalCandidate:
    weight = _wallet_weight(wallet)
    return SignalCandidate(
        source="smart_money",
        chain="solana",
        token_address=mint,
        token_symbol="",
        detected_at_ms=utc_ms(),
        raw_signal={
            "wallet_pubkey": _wallet_pubkey(wallet),
            "wallet_label": str(wallet.get("label", "")),
            "wallet_weight": weight,
            "signature": signature,
            "block_time_ms": block_time_ms,
            "amount_atoms": amount_atoms,
            "helius_tx_url": f"https://xray.helius.xyz/tx/{signature}",
        },
        confidence=weight,
    )


def _wallet_token_accounts(
    tx: dict[str, Any],
    wallet_pubkey: str,
) -> tuple[set[str], dict[str, str]]:
    account_keys = _account_keys(tx)
    own_accounts: set[str] = set()
    account_mints: dict[str, str] = {}

    meta = tx.get("meta", {})
    if not isinstance(meta, dict):
        return own_accounts, account_mints
    balances = []
    for key in ("preTokenBalances", "postTokenBalances"):
        value = meta.get(key, [])
        if isinstance(value, list):
            balances.extend(item for item in value if isinstance(item, dict))

    for balance in balances:
        account = _account_for_index(account_keys, balance.get("accountIndex"))
        mint = _as_str(balance.get("mint"))
        owner = _as_str(balance.get("owner"))
        if account and mint:
            account_mints[account] = mint
        if account and owner == wallet_pubkey:
            own_accounts.add(account)
    return own_accounts, account_mints


def _positive_wallet_mints(tx: dict[str, Any], wallet_pubkey: str) -> set[str]:
    pre = _wallet_mint_amounts(tx, wallet_pubkey, "preTokenBalances")
    post = _wallet_mint_amounts(tx, wallet_pubkey, "postTokenBalances")
    mints = set(pre) | set(post)
    return {mint for mint in mints if post.get(mint, Decimal(0)) > pre.get(mint, Decimal(0))}


def _wallet_mint_amounts(
    tx: dict[str, Any],
    wallet_pubkey: str,
    key: str,
) -> dict[str, Decimal]:
    meta = tx.get("meta", {})
    if not isinstance(meta, dict):
        return {}
    balances = meta.get(key, [])
    if not isinstance(balances, list):
        return {}

    amounts: dict[str, Decimal] = {}
    for balance in balances:
        if not isinstance(balance, dict):
            continue
        if _as_str(balance.get("owner")) != wallet_pubkey:
            continue
        mint = _as_str(balance.get("mint"))
        if not mint:
            continue
        amount = _ui_token_amount(balance)
        amounts[mint] = amounts.get(mint, Decimal(0)) + amount
    return amounts


def _ui_token_amount(balance: dict[str, Any]) -> Decimal:
    token_amount = balance.get("uiTokenAmount", {})
    if not isinstance(token_amount, dict):
        return Decimal(0)
    value = token_amount.get("amount", "0")
    try:
        return Decimal(str(value))
    except InvalidOperation:
        return Decimal(0)


def _iter_token_transfer_instructions(tx: dict[str, Any]) -> list[dict[str, Any]]:
    instructions: list[dict[str, Any]] = []
    message = tx.get("transaction", {}).get("message", {})
    if isinstance(message, dict):
        root = message.get("instructions", [])
        if isinstance(root, list):
            instructions.extend(item for item in root if isinstance(item, dict))

    meta = tx.get("meta", {})
    inner = meta.get("innerInstructions", []) if isinstance(meta, dict) else []
    if isinstance(inner, list):
        for group in inner:
            if not isinstance(group, dict):
                continue
            group_instructions = group.get("instructions", [])
            if isinstance(group_instructions, list):
                instructions.extend(
                    item for item in group_instructions if isinstance(item, dict)
                )

    return [item for item in instructions if _is_token_transfer_instruction(item)]


def _is_token_transfer_instruction(instruction: dict[str, Any]) -> bool:
    program = _as_str(instruction.get("program"))
    program_id = _as_str(instruction.get("programId"))
    parsed = instruction.get("parsed", {})
    if not isinstance(parsed, dict):
        return False
    instr_type = _as_str(parsed.get("type"))
    return (
        (program == "spl-token" or program_id == TOKEN_PROGRAM_ID)
        and instr_type in {"transfer", "transferChecked"}
    )


def _instruction_info(instruction: dict[str, Any]) -> dict[str, Any]:
    parsed = instruction.get("parsed", {})
    if not isinstance(parsed, dict):
        return {}
    info = parsed.get("info", {})
    return info if isinstance(info, dict) else {}


def _amount_atoms(info: dict[str, Any]) -> str:
    token_amount = info.get("tokenAmount")
    if isinstance(token_amount, dict):
        amount = token_amount.get("amount")
        if isinstance(amount, str) and amount:
            return amount
    amount = info.get("amount")
    if isinstance(amount, str) and amount:
        return amount
    if isinstance(amount, int):
        return str(amount)
    return "0"


def _sum_amounts(left: str, right: str) -> str:
    try:
        return str(int(left) + int(right))
    except ValueError:
        return right


def _account_keys(tx: dict[str, Any]) -> list[str]:
    message = tx.get("transaction", {}).get("message", {})
    if not isinstance(message, dict):
        return []
    raw_keys = message.get("accountKeys", [])
    if not isinstance(raw_keys, list):
        return []
    keys: list[str] = []
    for item in raw_keys:
        if isinstance(item, str):
            keys.append(item)
        elif isinstance(item, dict) and isinstance(item.get("pubkey"), str):
            keys.append(item["pubkey"])
        else:
            keys.append("")
    return keys


def _account_for_index(account_keys: list[str], index: Any) -> str:
    try:
        idx = int(index)
    except (TypeError, ValueError):
        return ""
    if idx < 0 or idx >= len(account_keys):
        return ""
    return account_keys[idx]


def _block_time_ms(transaction: dict[str, Any]) -> int | None:
    result = transaction.get("result", transaction)
    if not isinstance(result, dict):
        return None
    block_time = result.get("blockTime")
    try:
        return int(block_time) * 1000
    except (TypeError, ValueError):
        return None


def _load_state(path: str | Path) -> dict[str, str]:
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return {}
    if not isinstance(data, dict):
        return {}
    return {str(key): value for key, value in data.items() if isinstance(value, str)}


def _add_watchlist_entry(
    watchlist: list[dict[str, str]],
    candidate: SignalCandidate,
    *,
    settings: SmartMoneySettings,
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


def _validate_settings(settings: SmartMoneySettings) -> None:
    if not settings.helius_api_key:
        raise SmartMoneyConfigError(
            "HELIUS_API_KEY is required for the smart-money tracker. "
            "Create a free-tier key at helius.dev and set HELIUS_API_KEY."
        )


def _wallet_pubkey(wallet: dict) -> str:
    value = wallet.get("pubkey")
    return value.strip() if isinstance(value, str) else ""


def _wallet_weight(wallet: dict) -> float:
    value = wallet.get("weight", 0.5)
    if not isinstance(value, int | float) or isinstance(value, bool):
        return 0.5
    return max(0.0, min(1.0, float(value)))


def _as_str(value: Any) -> str:
    return value if isinstance(value, str) else ""


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
        logger.warning(f"Invalid float for {key}: {value!r}; using {default}")
        return default


def _env_int(key: str, default: int) -> int:
    value = os.getenv(key)
    if value is None:
        return default
    try:
        return int(value)
    except ValueError:
        logger.warning(f"Invalid integer for {key}: {value!r}; using {default}")
        return default


if __name__ == "__main__":
    raise SystemExit(run())

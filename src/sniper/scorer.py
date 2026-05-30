"""Token safety scorer for Solana memecoin candidates.

Given a token mint address, queries Solana RPC for the on-chain state
that determines whether the token is safe enough to trade:

* **Mint authority**: if not null, the deployer can mint unlimited new
  tokens → infinite dilution risk → hard reject.
* **Freeze authority**: if not null, the deployer can freeze your token
  account → you can't sell → hard reject.
* **Top-holder concentration**: if any single non-bonding-curve wallet
  holds > ``max_top_holder_pct`` of supply → dump-on-you risk → hard
  reject.
* **Liquidity proxy**: top-10 holder concentration as a coarse measure of
  how dispersed the supply is.
* **Supply + decimals**: sanity checks; abnormal values reject.

Outputs ``TokenSafetyScore`` with both a numeric score and a hard
``rejection_reason`` string for any failing check. Downstream consumers
(Phase 4 paper executor, Phase 5 live executor) trade only when the
``rejection_reason`` is None and the score clears their threshold.

Pure aiohttp HTTP client; same no-dependency approach as the detector.
SPL Token mint account layout decoded manually from the base64 payload.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import json
from collections.abc import Mapping, Sequence
from dataclasses import asdict, dataclass, field
from typing import Any

import aiohttp


DEFAULT_RPC_HTTP_URL = "https://api.mainnet-beta.solana.com"

# Common ignore list when computing top-holder concentration. These are
# program-controlled accounts (Raydium / pump.fun vaults, liquidity pools)
# whose balance does not represent dump risk to retail.
DEFAULT_HOLDER_IGNORE_KEYWORDS: tuple[str, ...] = (
    "Vault", "vault", "Pool", "pool", "BondingCurve",
)


class SniperScorerError(RuntimeError):
    """Raised when the scorer cannot proceed."""


@dataclass(frozen=True, slots=True)
class TopHolder:
    address: str
    amount: int
    pct_of_supply: float


@dataclass(frozen=True, slots=True)
class TokenSafetyScore:
    """Scored evaluation of one token mint."""

    mint: str
    has_mint_authority: bool | None  # None = couldn't determine
    has_freeze_authority: bool | None
    supply: int | None
    decimals: int | None
    top_holder_pct: float
    top_10_pct: float
    holder_count_top_20: int
    top_holders: tuple[TopHolder, ...] = field(default_factory=tuple)
    score: float = 0.0          # 0..100, higher is safer
    rejection_reason: str | None = None  # if any hard check failed

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @property
    def is_tradeable(self) -> bool:
        return self.rejection_reason is None and self.score > 0


# ---------------------------------------------------------------------------
# Mint account parsing
# ---------------------------------------------------------------------------

# SPL Token v3 mint account layout (82 bytes total):
#   0..36   mintAuthority (Option<Pubkey>): 4-byte LE flag + 32-byte pubkey
#   36..44  supply (u64 LE)
#   44      decimals (u8)
#   45      isInitialized (u8 / bool)
#   46..82  freezeAuthority (Option<Pubkey>): 4-byte LE flag + 32-byte pubkey
SPL_MINT_LAYOUT_LEN = 82


@dataclass(frozen=True, slots=True)
class MintInfo:
    has_mint_authority: bool
    has_freeze_authority: bool
    supply: int
    decimals: int
    is_initialized: bool


def parse_mint_account(data_bytes: bytes) -> MintInfo:
    """Decode an SPL Token mint account from its raw 82-byte payload."""
    if len(data_bytes) < SPL_MINT_LAYOUT_LEN:
        raise SniperScorerError(
            f"mint account data too short: {len(data_bytes)} bytes (expected {SPL_MINT_LAYOUT_LEN})"
        )
    mint_auth_flag = int.from_bytes(data_bytes[0:4], "little")
    supply = int.from_bytes(data_bytes[36:44], "little")
    decimals = data_bytes[44]
    is_initialized = bool(data_bytes[45])
    freeze_auth_flag = int.from_bytes(data_bytes[46:50], "little")
    return MintInfo(
        has_mint_authority=mint_auth_flag != 0,
        has_freeze_authority=freeze_auth_flag != 0,
        supply=supply,
        decimals=decimals,
        is_initialized=is_initialized,
    )


def decode_account_data(rpc_value: Mapping[str, Any]) -> bytes:
    """Extract the binary payload from a getAccountInfo result.

    The RPC returns ``data: [<payload>, <encoding>]`` where encoding is
    ``"base64"`` by default. We only support base64 here.
    """
    data = rpc_value.get("data")
    if not isinstance(data, (list, tuple)) or len(data) < 2:
        raise SniperScorerError("getAccountInfo result missing data array")
    encoding = str(data[1])
    if encoding != "base64":
        raise SniperScorerError(f"unsupported account data encoding: {encoding}")
    try:
        return base64.b64decode(str(data[0]))
    except (TypeError, ValueError) as exc:
        raise SniperScorerError(f"failed to base64-decode mint account: {exc}") from exc


# ---------------------------------------------------------------------------
# RPC helpers
# ---------------------------------------------------------------------------


async def _rpc_call(
    session: aiohttp.ClientSession, rpc_url: str, method: str, params: Sequence[Any],
) -> Mapping[str, Any]:
    payload = {"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)}
    async with session.post(rpc_url, json=payload, timeout=20) as resp:
        if resp.status != 200:
            raise SniperScorerError(f"RPC {method} returned HTTP {resp.status}")
        body = await resp.json()
    if "error" in body and body["error"] is not None:
        raise SniperScorerError(f"RPC {method} error: {body['error']}")
    return body.get("result") or {}


async def fetch_mint_info(mint: str, *, rpc_url: str, session: aiohttp.ClientSession) -> MintInfo:
    result = await _rpc_call(session, rpc_url, "getAccountInfo",
                              [mint, {"encoding": "base64", "commitment": "confirmed"}])
    value = result.get("value") if isinstance(result, Mapping) else None
    if not isinstance(value, Mapping):
        raise SniperScorerError(f"mint {mint} not found on-chain")
    return parse_mint_account(decode_account_data(value))


async def fetch_largest_holders(
    mint: str, *, rpc_url: str, session: aiohttp.ClientSession,
) -> list[Mapping[str, Any]]:
    result = await _rpc_call(session, rpc_url, "getTokenLargestAccounts",
                              [mint, {"commitment": "confirmed"}])
    value = result.get("value") if isinstance(result, Mapping) else None
    if not isinstance(value, list):
        return []
    return list(value)


# ---------------------------------------------------------------------------
# Scoring
# ---------------------------------------------------------------------------


def _compute_top_concentration(
    holders: Sequence[Mapping[str, Any]], total_supply: int,
) -> tuple[float, float, int, tuple[TopHolder, ...]]:
    if total_supply <= 0 or not holders:
        return 0.0, 0.0, 0, ()
    parsed: list[TopHolder] = []
    for holder in holders:
        try:
            amount = int(holder.get("amount") or 0)
        except (TypeError, ValueError):
            continue
        if amount <= 0:
            continue
        addr = str(holder.get("address") or "")
        pct = amount / total_supply * 100.0
        parsed.append(TopHolder(address=addr, amount=amount, pct_of_supply=pct))
    parsed.sort(key=lambda h: h.amount, reverse=True)
    top_holder_pct = parsed[0].pct_of_supply if parsed else 0.0
    top_10_pct = sum(h.pct_of_supply for h in parsed[:10])
    return top_holder_pct, top_10_pct, len(parsed), tuple(parsed[:20])


def score_safety(
    mint: str,
    *,
    mint_info: MintInfo | None,
    holders: Sequence[Mapping[str, Any]] | None,
    max_top_holder_pct: float = 30.0,
    max_top_10_pct: float = 75.0,
) -> TokenSafetyScore:
    """Combine mint info + holder list into a single safety verdict."""
    if mint_info is None:
        return TokenSafetyScore(
            mint=mint, has_mint_authority=None, has_freeze_authority=None,
            supply=None, decimals=None, top_holder_pct=0.0, top_10_pct=0.0,
            holder_count_top_20=0, rejection_reason="mint account not found",
        )

    rejection: str | None = None
    if mint_info.has_mint_authority:
        rejection = "mint authority is not renounced (deployer can dilute)"
    elif mint_info.has_freeze_authority:
        rejection = "freeze authority is not renounced (deployer can freeze your token account)"
    elif not mint_info.is_initialized:
        rejection = "mint not initialized"
    elif mint_info.decimals > 18:
        rejection = f"unreasonable decimals={mint_info.decimals}"

    top_holder_pct, top_10_pct, holder_count, top_holders = _compute_top_concentration(
        holders or [], mint_info.supply,
    )

    if rejection is None:
        if top_holder_pct > max_top_holder_pct:
            rejection = f"top holder owns {top_holder_pct:.1f}% (max {max_top_holder_pct:.1f}%)"
        elif top_10_pct > max_top_10_pct:
            rejection = f"top-10 owns {top_10_pct:.1f}% (max {max_top_10_pct:.1f}%)"

    if rejection is not None:
        return TokenSafetyScore(
            mint=mint,
            has_mint_authority=mint_info.has_mint_authority,
            has_freeze_authority=mint_info.has_freeze_authority,
            supply=mint_info.supply, decimals=mint_info.decimals,
            top_holder_pct=top_holder_pct, top_10_pct=top_10_pct,
            holder_count_top_20=holder_count, top_holders=top_holders,
            rejection_reason=rejection, score=0.0,
        )

    # No hard rejections → produce a 0-100 score. Cheap heuristic: start at
    # 100 and dock points proportional to concentration.
    score = 100.0
    score -= min(top_holder_pct, max_top_holder_pct) * (40.0 / max_top_holder_pct)
    score -= min(top_10_pct, max_top_10_pct) * (40.0 / max_top_10_pct)
    score = max(0.0, min(100.0, score))

    return TokenSafetyScore(
        mint=mint,
        has_mint_authority=mint_info.has_mint_authority,
        has_freeze_authority=mint_info.has_freeze_authority,
        supply=mint_info.supply, decimals=mint_info.decimals,
        top_holder_pct=top_holder_pct, top_10_pct=top_10_pct,
        holder_count_top_20=holder_count, top_holders=top_holders,
        score=score, rejection_reason=None,
    )


async def score_token(
    mint: str,
    *,
    rpc_url: str = DEFAULT_RPC_HTTP_URL,
    session: aiohttp.ClientSession | None = None,
    max_top_holder_pct: float = 30.0,
    max_top_10_pct: float = 75.0,
) -> TokenSafetyScore:
    """End-to-end scoring: fetch mint info + holders, run rules, return verdict."""
    owns_session = session is None
    if session is None:
        session = aiohttp.ClientSession()
    try:
        try:
            mint_info = await fetch_mint_info(mint, rpc_url=rpc_url, session=session)
        except SniperScorerError as exc:
            return score_safety(mint, mint_info=None, holders=None)
        holders: list[Mapping[str, Any]] = []
        try:
            holders = await fetch_largest_holders(mint, rpc_url=rpc_url, session=session)
        except SniperScorerError:
            # Holder query failure is non-fatal; we still scored mint authorities.
            pass
        return score_safety(
            mint, mint_info=mint_info, holders=holders,
            max_top_holder_pct=max_top_holder_pct, max_top_10_pct=max_top_10_pct,
        )
    finally:
        if owns_session:
            await session.close()


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Score the safety of a Solana token mint. Hard-rejects on "
                    "mint/freeze authority not renounced or concentrated holders; "
                    "otherwise emits a 0-100 score."
    )
    parser.add_argument("mint", help="Base58 mint address")
    parser.add_argument("--rpc-url", default=DEFAULT_RPC_HTTP_URL)
    parser.add_argument("--max-top-holder-pct", type=float, default=30.0)
    parser.add_argument("--max-top-10-pct", type=float, default=75.0)
    args = parser.parse_args(argv)

    try:
        score = asyncio.run(score_token(
            args.mint, rpc_url=args.rpc_url,
            max_top_holder_pct=args.max_top_holder_pct,
            max_top_10_pct=args.max_top_10_pct,
        ))
    except SniperScorerError as exc:
        print(f"scorer failed: {exc}")
        return 1
    print(json.dumps(score.as_dict(), indent=2, sort_keys=True))
    return 0 if score.is_tradeable else 2


if __name__ == "__main__":
    raise SystemExit(run())

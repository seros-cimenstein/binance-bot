"""Minimal async client for the Jupiter v6 swap-quote API.

Endpoint: ``https://lite-api.jup.ag/swap/v1/quote`` — public free tier,
no auth. Used by the paper executor to price simulated entries / exits
against real on-chain liquidity (including pump.fun bonding curves).

No solana-py dependency; just aiohttp.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping

import aiohttp


SOL_MINT = "So11111111111111111111111111111111111111112"
DEFAULT_QUOTE_URL = "https://lite-api.jup.ag/swap/v1/quote"


class JupiterQuoteError(RuntimeError):
    """Raised when Jupiter cannot produce a quote."""


@dataclass(frozen=True, slots=True)
class JupiterQuote:
    input_mint: str
    output_mint: str
    in_amount: int   # raw atoms of input mint
    out_amount: int  # raw atoms of output mint
    price_impact_pct: float
    swap_usd_value: float
    route_labels: tuple[str, ...]
    context_slot: int
    raw: Mapping[str, Any]


def _parse_quote(payload: Mapping[str, Any]) -> JupiterQuote:
    try:
        in_amount = int(payload["inAmount"])
        out_amount = int(payload["outAmount"])
    except (KeyError, TypeError, ValueError) as exc:
        raise JupiterQuoteError(f"missing/invalid amount fields: {exc}") from exc
    try:
        price_impact_pct = float(payload.get("priceImpactPct") or 0.0)
    except (TypeError, ValueError):
        price_impact_pct = 0.0
    try:
        swap_usd_value = float(payload.get("swapUsdValue") or 0.0)
    except (TypeError, ValueError):
        swap_usd_value = 0.0
    try:
        context_slot = int(payload.get("contextSlot") or 0)
    except (TypeError, ValueError):
        context_slot = 0
    routes = payload.get("routePlan") or []
    labels: list[str] = []
    if isinstance(routes, list):
        for hop in routes:
            if not isinstance(hop, Mapping):
                continue
            info = hop.get("swapInfo")
            if isinstance(info, Mapping) and info.get("label"):
                labels.append(str(info["label"]))
    return JupiterQuote(
        input_mint=str(payload.get("inputMint", "")),
        output_mint=str(payload.get("outputMint", "")),
        in_amount=in_amount, out_amount=out_amount,
        price_impact_pct=price_impact_pct,
        swap_usd_value=swap_usd_value,
        route_labels=tuple(labels),
        context_slot=context_slot,
        raw=payload,
    )


async def get_quote(
    *,
    input_mint: str, output_mint: str, amount_atoms: int,
    slippage_bps: int = 500,
    quote_url: str = DEFAULT_QUOTE_URL,
    session: aiohttp.ClientSession | None = None,
) -> JupiterQuote:
    """Request one exact-in swap quote from Jupiter v6.

    Returns a parsed ``JupiterQuote``. Raises ``JupiterQuoteError`` on
    HTTP failure, malformed response, or zero-output (no route found).
    """
    if amount_atoms <= 0:
        raise JupiterQuoteError("amount_atoms must be positive")
    params = {
        "inputMint": input_mint, "outputMint": output_mint,
        "amount": str(amount_atoms), "slippageBps": str(slippage_bps),
    }
    owns_session = session is None
    if session is None:
        session = aiohttp.ClientSession()
    try:
        async with session.get(quote_url, params=params, timeout=10) as resp:
            if resp.status != 200:
                body_preview = (await resp.text())[:200]
                raise JupiterQuoteError(f"HTTP {resp.status}: {body_preview}")
            payload = await resp.json()
    finally:
        if owns_session:
            await session.close()
    quote = _parse_quote(payload)
    if quote.out_amount <= 0:
        raise JupiterQuoteError("zero out_amount — no route available")
    return quote

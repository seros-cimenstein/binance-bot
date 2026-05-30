"""Tests for the Jupiter quote client."""

from __future__ import annotations

import pytest

from src.sniper.jupiter import (
    JupiterQuote,
    JupiterQuoteError,
    SOL_MINT,
    _parse_quote,
    get_quote,
)


def test_parse_quote_extracts_amounts_and_route() -> None:
    payload = {
        "inputMint": SOL_MINT,
        "outputMint": "MINT",
        "inAmount": "10000000",
        "outAmount": "176520444997",
        "priceImpactPct": "0.5",
        "swapUsdValue": "0.4148",
        "contextSlot": 12345,
        "routePlan": [
            {"swapInfo": {"label": "Pump.fun", "ammKey": "X", "inputMint": SOL_MINT,
                          "outputMint": "MINT", "inAmount": "10000000",
                          "outAmount": "176520444997"}},
        ],
    }
    q = _parse_quote(payload)
    assert q.in_amount == 10_000_000
    assert q.out_amount == 176_520_444_997
    assert q.price_impact_pct == pytest.approx(0.5)
    assert q.swap_usd_value == pytest.approx(0.4148)
    assert q.route_labels == ("Pump.fun",)


def test_parse_quote_defaults_on_missing_optional_fields() -> None:
    q = _parse_quote({"inAmount": "1", "outAmount": "2"})
    assert q.price_impact_pct == 0.0
    assert q.swap_usd_value == 0.0
    assert q.route_labels == ()
    assert q.context_slot == 0


def test_parse_quote_raises_on_missing_amounts() -> None:
    with pytest.raises(JupiterQuoteError, match="amount fields"):
        _parse_quote({})
    with pytest.raises(JupiterQuoteError, match="amount fields"):
        _parse_quote({"inAmount": "not a number", "outAmount": "5"})


@pytest.mark.asyncio
async def test_get_quote_rejects_non_positive_amount() -> None:
    with pytest.raises(JupiterQuoteError, match="amount_atoms must be positive"):
        await get_quote(input_mint=SOL_MINT, output_mint="X", amount_atoms=0)


class _FakeResp:
    def __init__(self, *, status: int, json_payload=None, text_payload: str = "") -> None:
        self.status = status
        self._json = json_payload
        self._text = text_payload

    async def __aenter__(self): return self
    async def __aexit__(self, *a): return None
    async def json(self): return self._json
    async def text(self): return self._text


class _FakeSession:
    def __init__(self, resp: _FakeResp) -> None:
        self._resp = resp
        self.requests: list[dict] = []

    def get(self, url: str, *, params=None, timeout=None):
        self.requests.append({"url": url, "params": params, "timeout": timeout})
        return self._resp


@pytest.mark.asyncio
async def test_get_quote_returns_parsed_quote_on_success() -> None:
    payload = {"inputMint": SOL_MINT, "outputMint": "MINT",
               "inAmount": "10000000", "outAmount": "999",
               "priceImpactPct": "0", "swapUsdValue": "0.5",
               "contextSlot": 1, "routePlan": []}
    session = _FakeSession(_FakeResp(status=200, json_payload=payload))
    quote = await get_quote(
        input_mint=SOL_MINT, output_mint="MINT", amount_atoms=10_000_000,
        session=session,
    )
    assert isinstance(quote, JupiterQuote)
    assert quote.out_amount == 999
    assert session.requests and session.requests[0]["params"]["amount"] == "10000000"


@pytest.mark.asyncio
async def test_get_quote_raises_on_http_error() -> None:
    session = _FakeSession(_FakeResp(status=500, text_payload="server boom"))
    with pytest.raises(JupiterQuoteError, match="HTTP 500"):
        await get_quote(input_mint=SOL_MINT, output_mint="MINT",
                         amount_atoms=1, session=session)


@pytest.mark.asyncio
async def test_get_quote_raises_on_zero_output() -> None:
    payload = {"inputMint": SOL_MINT, "outputMint": "MINT",
               "inAmount": "10000000", "outAmount": "0"}
    session = _FakeSession(_FakeResp(status=200, json_payload=payload))
    with pytest.raises(JupiterQuoteError, match="no route available"):
        await get_quote(input_mint=SOL_MINT, output_mint="MINT",
                         amount_atoms=10_000_000, session=session)

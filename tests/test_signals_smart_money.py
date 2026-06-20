"""Tests for the Solana smart-money signal tracker."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest

from src.signals import smart_money as sm_mod


WALLET = "Wallet111111111111111111111111111111111111"
ATA = "Ata111111111111111111111111111111111111111"
MINT_A = "MintA111111111111111111111111111111111111"
MINT_B = "MintB111111111111111111111111111111111111"
OTHER_ATA = "OtherAta111111111111111111111111111111111"


class DummyResponse:
    def __init__(self, payload: Any) -> None:
        self.payload = payload

    async def __aenter__(self) -> "DummyResponse":
        return self

    async def __aexit__(self, *_args: Any) -> None:
        return None

    def raise_for_status(self) -> None:
        return None

    async def json(self) -> Any:
        return self.payload


class RpcSession:
    def __init__(self, signatures: list[str], transactions: dict[str, dict[str, Any]]) -> None:
        self.signatures = signatures
        self.transactions = transactions
        self.calls: list[dict[str, Any]] = []

    def post(self, _url: str, *, json: dict[str, Any]) -> DummyResponse:
        self.calls.append(json)
        if json["method"] == "getSignaturesForAddress":
            return DummyResponse({
                "result": [{"signature": signature} for signature in self.signatures],
            })
        signature = json["params"][0]
        return DummyResponse({"result": self.transactions[signature]})


def _settings(tmp_path: Path, **overrides: Any) -> sm_mod.SmartMoneySettings:
    values = {
        "helius_api_key": "helius-key",
        "poll_interval_seconds": 0.0,
        "wallet_list_path": str(tmp_path / "wallets.json"),
        "state_path": str(tmp_path / "state.json"),
        "max_alerts_per_hour": 15,
        "stop_path": str(tmp_path / "stop"),
        "refresh_hours": 168.0,
        "birdeye_api_key": None,
        "ledger_path": str(tmp_path / "ledger.jsonl"),
        "watchlist_path": str(tmp_path / "watchlist.json"),
        "watchlist_ttl_hours": 12.0,
    }
    values.update(overrides)
    return sm_mod.SmartMoneySettings(**values)


def _wallet_file(settings: sm_mod.SmartMoneySettings, now_ms: int = 2_000_000) -> None:
    Path(settings.wallet_list_path).write_text(
        json.dumps({
            "wallets": [{
                "pubkey": WALLET,
                "label": "ansem-public",
                "source": "manual",
                "weight": 0.8,
            }],
            "last_refreshed_ms": now_ms,
        }),
        encoding="utf-8",
    )


def _tx(
    mint: str = MINT_A,
    *,
    incoming: bool = True,
    outgoing_same_mint: bool = False,
    post_amount: str = "100",
    block_time: int = 123,
) -> dict[str, Any]:
    instructions: list[dict[str, Any]] = []
    if incoming:
        instructions.append(_transfer_instruction(
            source=OTHER_ATA,
            destination=ATA,
            mint=mint,
            amount="100",
        ))
    if outgoing_same_mint:
        instructions.append(_transfer_instruction(
            source=ATA,
            destination=OTHER_ATA,
            mint=mint,
            amount="25",
        ))
    return {
        "blockTime": block_time,
        "transaction": {
            "message": {
                "accountKeys": [
                    {"pubkey": WALLET},
                    {"pubkey": ATA},
                    {"pubkey": OTHER_ATA},
                ],
                "instructions": instructions,
            },
        },
        "meta": {
            "preTokenBalances": [{
                "accountIndex": 1,
                "mint": mint,
                "owner": WALLET,
                "uiTokenAmount": {"amount": "0"},
            }],
            "postTokenBalances": [{
                "accountIndex": 1,
                "mint": mint,
                "owner": WALLET,
                "uiTokenAmount": {"amount": post_amount},
            }],
            "innerInstructions": [],
        },
    }


def _transfer_instruction(
    *,
    source: str,
    destination: str,
    mint: str,
    amount: str,
) -> dict[str, Any]:
    return {
        "program": "spl-token",
        "programId": sm_mod.TOKEN_PROGRAM_ID,
        "parsed": {
            "type": "transferChecked",
            "info": {
                "source": source,
                "destination": destination,
                "mint": mint,
                "tokenAmount": {"amount": amount},
            },
        },
    }


def test_buy_detection_incoming_non_stable_returns_mint() -> None:
    assert sm_mod.detect_buy_mint(_tx(MINT_A), WALLET) == MINT_A


def test_buy_detection_outgoing_same_mint_returns_none() -> None:
    assert sm_mod.detect_buy_mint(_tx(MINT_A, outgoing_same_mint=True), WALLET) is None


def test_buy_detection_stable_incoming_returns_none() -> None:
    assert sm_mod.detect_buy_mint(_tx(sm_mod.USDC_MINT), WALLET) is None


@pytest.mark.asyncio
async def test_full_cycle_emits_two_candidates_and_updates_state(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    _wallet_file(settings)
    monkeypatch.setattr(sm_mod, "utc_ms", lambda: 2_000_000)
    maybe_telegram = AsyncMock(return_value=True)
    monkeypatch.setattr(sm_mod, "maybe_telegram", maybe_telegram)
    session = RpcSession(["sig-new", "sig-old"], {
        "sig-new": _tx(MINT_A, block_time=111),
        "sig-old": _tx(MINT_B, block_time=110),
    })

    candidates = await sm_mod.run_cycle(
        session=session,
        settings=settings,
        watchlist=[],
        alert_limiter=sm_mod.AlertRateLimiter(15),
    )

    assert [candidate.token_address for candidate in candidates] == [MINT_A, MINT_B]
    ledger_rows = [
        json.loads(line)
        for line in Path(settings.ledger_path).read_text(encoding="utf-8").splitlines()
    ]
    assert len(ledger_rows) == 2
    assert ledger_rows[0]["source"] == "smart_money"
    assert ledger_rows[0]["raw_signal"]["wallet_pubkey"] == WALLET
    assert ledger_rows[0]["raw_signal"]["helius_tx_url"].endswith("/sig-new")
    assert json.loads(Path(settings.state_path).read_text(encoding="utf-8")) == {
        WALLET: "sig-new",
    }
    maybe_telegram.assert_awaited()


@pytest.mark.asyncio
async def test_dedup_via_state_second_cycle_no_new_signatures(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    _wallet_file(settings)
    monkeypatch.setattr(sm_mod, "utc_ms", lambda: 2_000_000)
    monkeypatch.setattr(sm_mod, "maybe_telegram", AsyncMock(return_value=True))
    first = RpcSession(["sig-new"], {"sig-new": _tx(MINT_A)})

    emitted = await sm_mod.run_cycle(
        session=first,
        settings=settings,
        watchlist=[],
        alert_limiter=sm_mod.AlertRateLimiter(15),
    )
    assert len(emitted) == 1

    second = RpcSession([], {})
    emitted = await sm_mod.run_cycle(
        session=second,
        settings=settings,
        watchlist=[],
        alert_limiter=sm_mod.AlertRateLimiter(15),
    )

    assert emitted == []
    params = second.calls[0]["params"][1]
    assert params["until"] == "sig-new"


def test_missing_helius_api_key_run_exits_2(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    logs: list[str] = []
    monkeypatch.delenv("HELIUS_API_KEY", raising=False)
    monkeypatch.setenv("SIGNALS_SMART_MONEY_WALLET_LIST_PATH", str(tmp_path / "wallets.json"))
    monkeypatch.setattr(sm_mod.logger, "error", logs.append)

    assert sm_mod.run(["--once"]) == 2
    assert "HELIUS_API_KEY" in logs[0]


@pytest.mark.asyncio
async def test_empty_wallet_list_cycle_returns_zero_without_raising(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    monkeypatch.setattr(sm_mod, "utc_ms", lambda: 2_000_000)
    Path(settings.wallet_list_path).write_text(
        json.dumps({"wallets": [], "last_refreshed_ms": 2_000_000}),
        encoding="utf-8",
    )

    candidates = await sm_mod.run_cycle(
        session=RpcSession([], {}),
        settings=settings,
        watchlist=[],
        alert_limiter=sm_mod.AlertRateLimiter(15),
    )

    assert candidates == []
    assert json.loads(Path(settings.watchlist_path).read_text(encoding="utf-8")) == {
        "watching": []
    }


@pytest.mark.asyncio
async def test_stop_file_present_daemon_exits_cleanly(tmp_path: Path) -> None:
    settings = _settings(tmp_path)
    Path(settings.stop_path).touch()
    await sm_mod.run_detector(settings=settings, once=True)


@pytest.mark.asyncio
async def test_telegram_alert_rate_limit_honored(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    _wallet_file(settings)
    monkeypatch.setattr(sm_mod, "utc_ms", lambda: 2_000_000)
    maybe_telegram = AsyncMock(return_value=True)
    monkeypatch.setattr(sm_mod, "maybe_telegram", maybe_telegram)
    session = RpcSession(["sig-1", "sig-2"], {
        "sig-1": _tx(MINT_A),
        "sig-2": _tx(MINT_B),
    })

    candidates = await sm_mod.run_cycle(
        session=session,
        settings=settings,
        watchlist=[],
        alert_limiter=sm_mod.AlertRateLimiter(1),
    )

    assert len(candidates) == 2
    maybe_telegram.assert_awaited_once()

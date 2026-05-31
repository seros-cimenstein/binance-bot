"""Tests for the sniper live executor (mocked Jupiter + Solana RPC)."""

from __future__ import annotations

import base64
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Any

import pytest
from solders.keypair import Keypair

from src.sniper import live_executor as live_mod
from src.sniper.jupiter import JupiterQuote, SOL_MINT
from src.sniper.live_executor import (
    DailySpendTracker,
    LiveExecutorRuntime,
    SniperLiveError,
    SniperLiveSettings,
    SwapResult,
    _preflight_settings,
    fetch_lamport_balance,
    fetch_signature_status,
    fetch_swap_transaction,
    load_runtime,
    save_spend_tracker,
    submit_signed_transaction,
)
from src.sniper.wallet import generate_wallet


def _enabled_settings(tmp_path: Path) -> SniperLiveSettings:
    return SniperLiveSettings(
        wallet_path=str(tmp_path / "wallet.json"),
        enabled=True, confirm_live_sniper="yes",
        emergency_stop_path=str(tmp_path / "stop"),
        ledger_path=str(tmp_path / "ledger.jsonl"),
        state_path=str(tmp_path / "state.json"),
    )


# ---------------------------------------------------------------------------
# Preflight gate ladder
# ---------------------------------------------------------------------------


def test_preflight_rejects_when_enabled_false(tmp_path: Path) -> None:
    s = SniperLiveSettings(wallet_path=str(tmp_path / "w.json"),
                            emergency_stop_path=str(tmp_path / "stop"),
                            enabled=False)
    with pytest.raises(SniperLiveError, match="ENABLED"):
        _preflight_settings(s)


def test_preflight_rejects_when_confirm_missing(tmp_path: Path) -> None:
    s = SniperLiveSettings(wallet_path=str(tmp_path / "w.json"),
                            emergency_stop_path=str(tmp_path / "stop"),
                            enabled=True, confirm_live_sniper="no")
    with pytest.raises(SniperLiveError, match="CONFIRM_LIVE_SNIPER"):
        _preflight_settings(s)


def test_preflight_rejects_when_proven_edge_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    s = _enabled_settings(tmp_path)
    monkeypatch.delenv("SNIPER_PROVEN_EDGE", raising=False)
    with pytest.raises(SniperLiveError, match="PROVEN_EDGE"):
        _preflight_settings(s)


def test_preflight_passes_when_all_gates_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("SNIPER_PROVEN_EDGE", "yes")
    _preflight_settings(_enabled_settings(tmp_path))


def test_preflight_blocks_when_emergency_stop_present(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("SNIPER_PROVEN_EDGE", "yes")
    s = _enabled_settings(tmp_path)
    Path(s.emergency_stop_path).write_text("halt")
    with pytest.raises(SniperLiveError, match="emergency stop"):
        _preflight_settings(s)


# ---------------------------------------------------------------------------
# Daily spend tracker
# ---------------------------------------------------------------------------


def test_spend_tracker_blocks_above_cap() -> None:
    s = DailySpendTracker(date_utc="2026-05-31", spent_sol_atoms=80_000_000)
    assert s.would_exceed(30_000_000, cap=100_000_000) is True
    assert s.would_exceed(20_000_000, cap=100_000_000) is False


def test_spend_tracker_resets_on_new_day(monkeypatch: pytest.MonkeyPatch) -> None:
    s = DailySpendTracker(date_utc="1999-01-01", spent_sol_atoms=99_999_999)
    s.reset_if_new_day()
    assert s.date_utc != "1999-01-01"
    assert s.spent_sol_atoms == 0


# ---------------------------------------------------------------------------
# JSON-RPC helpers (mocked HTTP)
# ---------------------------------------------------------------------------


class _FakeResp:
    def __init__(self, status: int, json_payload: Any = None, text: str = "") -> None:
        self.status = status
        self._json = json_payload
        self._text = text

    async def __aenter__(self): return self
    async def __aexit__(self, *a): return None
    async def json(self): return self._json
    async def text(self): return self._text


class _FakeSession:
    def __init__(self, responses) -> None:
        self._responses = list(responses) if isinstance(responses, list) else [responses]
        self.requests: list[dict] = []

    def post(self, url, *, json=None, timeout=None):
        self.requests.append({"url": url, "json": json, "method": "POST", "timeout": timeout})
        return self._responses.pop(0)

    def get(self, url, *, params=None, timeout=None):
        self.requests.append({"url": url, "params": params, "method": "GET", "timeout": timeout})
        return self._responses.pop(0)


@pytest.mark.asyncio
async def test_fetch_lamport_balance_returns_value() -> None:
    session = _FakeSession(_FakeResp(200, {"jsonrpc": "2.0", "id": 1,
                                            "result": {"value": 12345}}))
    bal = await fetch_lamport_balance(pubkey="X", rpc_url="rpc", session=session)
    assert bal == 12345


@pytest.mark.asyncio
async def test_fetch_lamport_balance_raises_on_rpc_error() -> None:
    session = _FakeSession(_FakeResp(200, {"jsonrpc": "2.0", "id": 1,
                                            "error": {"code": -32600, "message": "bad"}}))
    with pytest.raises(SniperLiveError, match="getBalance RPC error"):
        await fetch_lamport_balance(pubkey="X", rpc_url="rpc", session=session)


@pytest.mark.asyncio
async def test_fetch_signature_status_returns_row_or_none() -> None:
    session = _FakeSession([
        _FakeResp(200, {"result": {"value": [{"confirmationStatus": "confirmed", "err": None}]}}),
        _FakeResp(200, {"result": {"value": []}}),
    ])
    row = await fetch_signature_status(signature="s", rpc_url="r", session=session)
    assert row is not None and row["confirmationStatus"] == "confirmed"
    assert await fetch_signature_status(signature="s", rpc_url="r", session=session) is None


@pytest.mark.asyncio
async def test_fetch_swap_transaction_returns_swap_tx() -> None:
    session = _FakeSession(_FakeResp(200, {"swapTransaction": "abc=="}))
    tx = await fetch_swap_transaction(
        quote={"inputMint": "x"}, user_pubkey="P",
        swap_url="https://example/swap", session=session,
    )
    assert tx == "abc=="
    # The POST body must wrap the quote and include the user pubkey.
    assert session.requests[0]["json"]["userPublicKey"] == "P"


@pytest.mark.asyncio
async def test_fetch_swap_transaction_raises_on_missing_field() -> None:
    session = _FakeSession(_FakeResp(200, {"oops": "no swapTransaction"}))
    with pytest.raises(SniperLiveError, match="swapTransaction"):
        await fetch_swap_transaction(quote={}, user_pubkey="P",
                                     swap_url="x", session=session)


@pytest.mark.asyncio
async def test_submit_signed_transaction_returns_signature() -> None:
    session = _FakeSession(_FakeResp(200, {"result": "5sigBase58"}))
    sig = await submit_signed_transaction(
        signed_tx_bytes=b"\x00" * 16, rpc_url="rpc", session=session,
    )
    assert sig == "5sigBase58"
    # Body must include base64-encoded tx + encoding=base64.
    body = session.requests[0]["json"]
    assert body["method"] == "sendTransaction"
    assert body["params"][1]["encoding"] == "base64"
    # base64 round-trip check.
    assert base64.b64decode(body["params"][0]) == b"\x00" * 16


# ---------------------------------------------------------------------------
# Runtime wiring
# ---------------------------------------------------------------------------


def test_load_runtime_blocks_when_preflight_fails(tmp_path: Path) -> None:
    s = SniperLiveSettings(wallet_path=str(tmp_path / "w.json"),
                            emergency_stop_path=str(tmp_path / "stop"),
                            enabled=False)
    with pytest.raises(SniperLiveError):
        load_runtime(s)


def test_load_runtime_loads_keypair_and_spend_state(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    s = _enabled_settings(tmp_path)
    generate_wallet(s.wallet_path, "pp")
    Path(s.state_path).write_text(
        '{"date_utc": "2026-05-31", "spent_sol_atoms": 12345}'
    )
    monkeypatch.setenv("SNIPER_PROVEN_EDGE", "yes")
    monkeypatch.setenv("SNIPER_WALLET_PASSPHRASE", "pp")

    runtime = load_runtime(s)
    assert isinstance(runtime, LiveExecutorRuntime)
    # State was persisted from disk (may have reset_if_new_day on load when date stale).
    assert runtime.spend.spent_sol_atoms in (0, 12345)


def test_runtime_assert_spend_blocks_over_cap(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    s = _enabled_settings(tmp_path)
    generate_wallet(s.wallet_path, "pp")
    monkeypatch.setenv("SNIPER_PROVEN_EDGE", "yes")
    monkeypatch.setenv("SNIPER_WALLET_PASSPHRASE", "pp")
    runtime = load_runtime(s)
    runtime.spend.spent_sol_atoms = s.max_daily_sol_spend_atoms
    with pytest.raises(SniperLiveError, match="daily SOL spend cap"):
        runtime.assert_spend_allowed(1)


@pytest.mark.asyncio
async def test_runtime_assert_balance_ok_blocks_when_too_low(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    s = _enabled_settings(tmp_path)
    generate_wallet(s.wallet_path, "pp")
    monkeypatch.setenv("SNIPER_PROVEN_EDGE", "yes")
    monkeypatch.setenv("SNIPER_WALLET_PASSPHRASE", "pp")
    runtime = load_runtime(s)
    session = _FakeSession(_FakeResp(200, {"result": {"value": 1_000}}))  # well under floor
    with pytest.raises(SniperLiveError, match="below minimum"):
        await runtime.assert_balance_ok(session=session)


@pytest.mark.asyncio
async def test_runtime_assert_balance_ok_passes_above_floor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    s = _enabled_settings(tmp_path)
    generate_wallet(s.wallet_path, "pp")
    monkeypatch.setenv("SNIPER_PROVEN_EDGE", "yes")
    monkeypatch.setenv("SNIPER_WALLET_PASSPHRASE", "pp")
    runtime = load_runtime(s)
    plenty = s.min_wallet_sol_atoms * 10
    session = _FakeSession(_FakeResp(200, {"result": {"value": plenty}}))
    bal = await runtime.assert_balance_ok(session=session)
    assert bal == plenty


def test_save_spend_tracker_round_trips(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    s = _enabled_settings(tmp_path)
    generate_wallet(s.wallet_path, "pp")
    monkeypatch.setenv("SNIPER_PROVEN_EDGE", "yes")
    monkeypatch.setenv("SNIPER_WALLET_PASSPHRASE", "pp")
    runtime = load_runtime(s)
    runtime.spend.spent_sol_atoms = 99999
    save_spend_tracker(runtime)
    import json as _json
    persisted = _json.loads(Path(s.state_path).read_text())
    assert persisted["spent_sol_atoms"] == 99999

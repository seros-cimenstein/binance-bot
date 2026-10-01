"""Tests for the airdrop reminder: snapshots, auto-mark, digest."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.airdrop.catalog import filter_by
from src.airdrop.scheduler import (
    CEX_WALLET_NAME,
    ChecklistRow,
    CompletionState,
    TaskKey,
)
from src.airdrop.wallet import (
    CHAIN_EVM,
    CHAIN_SOLANA,
    EncryptedWallet,
    WalletStore,
    add_wallet,
    save_store,
)
from src.airdrop.reminder import (
    ActivitySnapshot,
    ActivityStore,
    ReminderSettings,
    auto_mark_completed_from_activity,
    fetch_counter_for_wallet,
    fetch_evm_nonce,
    fetch_solana_signature_count,
    load_activity_store,
    render_telegram_digest,
    run_reminder,
    save_activity_store,
    snapshot_all_wallets,
    wallet_was_active_this_week,
)


# ---------------------------------------------------------------------------
# ActivityStore plumbing
# ---------------------------------------------------------------------------


def _snap(name: str, counter: int, iso_week: str, *, chain="evm") -> ActivitySnapshot:
    return ActivitySnapshot(
        wallet_name=name, chain=chain, address="0xabc",
        counter=counter, iso_week=iso_week,
        captured_at_iso="2026-06-01T00:00:00+00:00",
    )


def test_activity_store_records_and_returns_latest() -> None:
    store = ActivityStore()
    store.record(_snap("w1", 5, "2026-W22"))
    assert store.latest("w1").counter == 5


def test_activity_store_replaces_within_same_iso_week() -> None:
    store = ActivityStore()
    store.record(_snap("w1", 5, "2026-W22"))
    store.record(_snap("w1", 6, "2026-W22"))   # same week — overwrite
    assert len(store.snapshots["w1"]) == 1
    assert store.latest("w1").counter == 6


def test_activity_store_appends_on_new_iso_week() -> None:
    store = ActivityStore()
    store.record(_snap("w1", 5, "2026-W22"))
    store.record(_snap("w1", 8, "2026-W23"))
    assert len(store.snapshots["w1"]) == 2


def test_activity_store_previous_in_iso_week_skips_current() -> None:
    store = ActivityStore()
    store.record(_snap("w1", 5, "2026-W22"))
    store.record(_snap("w1", 6, "2026-W23"))   # current iso_week
    prior = store.previous_in_iso_week("w1", iso_week="2026-W23")
    assert prior is not None
    assert prior.iso_week == "2026-W22"
    assert prior.counter == 5


def test_activity_store_previous_returns_none_when_no_prior_week() -> None:
    store = ActivityStore()
    store.record(_snap("w1", 5, "2026-W22"))
    assert store.previous_in_iso_week("w1", iso_week="2026-W22") is None


def test_activity_store_caps_history_at_twelve_entries() -> None:
    store = ActivityStore()
    for i in range(20):
        store.record(_snap("w1", i, f"2026-W{i:02d}"))
    assert len(store.snapshots["w1"]) == 12


def test_activity_store_round_trips_through_json(tmp_path: Path) -> None:
    path = tmp_path / "activity.json"
    store = ActivityStore()
    store.record(_snap("w1", 5, "2026-W22", chain="solana"))
    store.record(_snap("w1", 6, "2026-W23", chain="solana"))
    save_activity_store(store, path)
    loaded = load_activity_store(path)
    assert loaded.snapshots["w1"][-1].counter == 6
    assert loaded.snapshots["w1"][-1].chain == "solana"


def test_load_activity_store_missing_file_returns_empty(tmp_path: Path) -> None:
    s = load_activity_store(tmp_path / "nope.json")
    assert s.snapshots == {}


# ---------------------------------------------------------------------------
# Detection
# ---------------------------------------------------------------------------


def test_wallet_active_when_counter_increased() -> None:
    prior = _snap("w1", 5, "2026-W22")
    assert wallet_was_active_this_week(current_counter=7, previous_snapshot=prior) is True


def test_wallet_inactive_when_counter_unchanged() -> None:
    prior = _snap("w1", 5, "2026-W22")
    assert wallet_was_active_this_week(current_counter=5, previous_snapshot=prior) is False


def test_wallet_inactive_on_first_run_with_no_prior_snapshot() -> None:
    # First run can't infer activity — we have no baseline.
    assert wallet_was_active_this_week(current_counter=99, previous_snapshot=None) is False


# ---------------------------------------------------------------------------
# Auto-mark
# ---------------------------------------------------------------------------


def test_auto_mark_marks_all_pending_for_active_wallets() -> None:
    store = WalletStore()
    sol_wallet = add_wallet(store, name="sol-1", chain=CHAIN_SOLANA, passphrase="pp")
    completion_state = CompletionState()
    marked = auto_mark_completed_from_activity(
        completion_state=completion_state,
        wallets_with_activity=[sol_wallet],
        iso_week="2026-W22",
    )
    sol_active = filter_by(chain="solana", status="active")
    # All solana-active opportunities should be marked.
    assert len(marked) == len(sol_active)
    for opp in sol_active:
        key = TaskKey(wallet_name="sol-1", opportunity_name=opp.name)
        assert completion_state.is_completed_this_week(
            key, current_iso_week="2026-W22"
        ) is True


def test_auto_mark_skips_already_marked_tasks() -> None:
    store = WalletStore()
    sol_wallet = add_wallet(store, name="sol-1", chain=CHAIN_SOLANA, passphrase="pp")
    completion_state = CompletionState()
    sol_active = filter_by(chain="solana", status="active")
    # Pre-mark one opportunity.
    completion_state.mark(
        TaskKey("sol-1", sol_active[0].name), iso_week="2026-W22",
    )
    marked = auto_mark_completed_from_activity(
        completion_state=completion_state,
        wallets_with_activity=[sol_wallet],
        iso_week="2026-W22",
    )
    # One fewer than total — the pre-marked one is skipped.
    assert len(marked) == len(sol_active) - 1


def test_auto_mark_only_touches_opportunities_on_matching_chain() -> None:
    store = WalletStore()
    sol_wallet = add_wallet(store, name="sol-1", chain=CHAIN_SOLANA, passphrase="pp")
    state = CompletionState()
    auto_mark_completed_from_activity(
        completion_state=state, wallets_with_activity=[sol_wallet],
        iso_week="2026-W22",
    )
    # No EVM opportunities should be marked.
    for opp in filter_by(chain="evm", status="active"):
        key = TaskKey("sol-1", opp.name)
        assert state.is_completed_this_week(key, current_iso_week="2026-W22") is False


def test_auto_mark_never_marks_cex_programs() -> None:
    store = WalletStore()
    evm_wallet = add_wallet(store, name="evm-1", chain=CHAIN_EVM, passphrase="pp")
    state = CompletionState()
    auto_mark_completed_from_activity(
        completion_state=state, wallets_with_activity=[evm_wallet],
        iso_week="2026-W22",
    )
    cex_active = filter_by(chain="cex", status="active")
    assert cex_active, "catalog must carry active cex programs"
    for opp in cex_active:
        key = TaskKey(wallet_name=CEX_WALLET_NAME, opportunity_name=opp.name)
        assert state.is_completed_this_week(
            key, current_iso_week="2026-W22"
        ) is False


# ---------------------------------------------------------------------------
# Digest rendering
# ---------------------------------------------------------------------------


def _row(wallet: str, chain: str, opp: str, *, url="https://x.io",
         activity="swap", min_size=10.0, conf="medium") -> ChecklistRow:
    return ChecklistRow(
        wallet_name=wallet, wallet_address="0xabc", chain=chain,
        opportunity=opp, url=url, activity_kind=activity,
        min_actions_per_week=1, min_size_usd=min_size, confidence=conf,
        status="pending", notes="",
    )


def test_digest_lists_pending_grouped_by_wallet() -> None:
    rows = [
        _row("sol-1", "solana", "Kamino"),
        _row("sol-1", "solana", "Drift"),
        _row("evm-1", "evm", "Linea"),
    ]
    msg = render_telegram_digest(iso_week="2026-W22", pending_rows=rows,
                                  active_wallet_names=[])
    assert "<b>sol-1</b> (solana) — 2 pending" in msg
    assert "<b>evm-1</b> (evm) — 1 pending" in msg
    assert "Kamino" in msg and "Drift" in msg and "Linea" in msg


def test_digest_highlights_active_wallets() -> None:
    msg = render_telegram_digest(
        iso_week="2026-W22", pending_rows=[],
        active_wallet_names=["sol-1", "evm-1"],
    )
    assert "Auto-detected on-chain activity" in msg
    assert "evm-1" in msg and "sol-1" in msg


def test_digest_handles_empty_pending_with_no_activity() -> None:
    msg = render_telegram_digest(iso_week="2026-W22", pending_rows=[],
                                  active_wallet_names=[])
    assert "Nothing pending" in msg


def test_digest_labels_cex_group_as_binance_account() -> None:
    rows = [_row(CEX_WALLET_NAME, "cex", "Binance Alpha Points & airdrops")]
    msg = render_telegram_digest(iso_week="2026-W22", pending_rows=rows,
                                  active_wallet_names=[])
    assert f"<b>{CEX_WALLET_NAME}</b> (Binance account) — 1 pending" in msg


# ---------------------------------------------------------------------------
# RPC fetchers (mocked aiohttp session)
# ---------------------------------------------------------------------------


class _MockResponse:
    def __init__(self, payload, status=200) -> None:
        self._payload = payload
        self.status = status
        self.ok = 200 <= status < 300

    async def json(self):
        return self._payload

    async def text(self):
        return json.dumps(self._payload)

    def raise_for_status(self) -> None:
        if not self.ok:
            raise RuntimeError(f"HTTP {self.status}")

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        return False


class _MockSession:
    """Records POSTs and replies with scripted JSON payloads."""

    def __init__(self, scripted_payloads: list[dict]) -> None:
        self._scripted = list(scripted_payloads)
        self.posted: list[tuple[str, dict]] = []

    def post(self, url, *, json=None, **_kw):
        self.posted.append((url, json or {}))
        return _MockResponse(self._scripted.pop(0))


@pytest.mark.asyncio
async def test_fetch_evm_nonce_parses_hex_response() -> None:
    session = _MockSession([{"jsonrpc": "2.0", "id": 1, "result": "0x2a"}])
    nonce = await fetch_evm_nonce(session=session, rpc_url="http://rpc",
                                    address="0xabc")
    assert nonce == 42
    posted_url, body = session.posted[0]
    assert posted_url == "http://rpc"
    assert body["method"] == "eth_getTransactionCount"
    assert body["params"] == ["0xabc", "latest"]


@pytest.mark.asyncio
async def test_fetch_solana_signature_count_returns_list_len() -> None:
    sigs = [{"signature": f"sig{i}"} for i in range(7)]
    session = _MockSession([{"jsonrpc": "2.0", "id": 1, "result": sigs}])
    n = await fetch_solana_signature_count(session=session, rpc_url="http://rpc",
                                            address="abc")
    assert n == 7


@pytest.mark.asyncio
async def test_fetch_counter_dispatches_on_chain() -> None:
    wallet_evm = EncryptedWallet(
        name="evm-1", chain=CHAIN_EVM, address="0xabc",
        salt_b64="x", iv_b64="x", ciphertext_b64="x", mac_b64="x",
    )
    session = _MockSession([{"jsonrpc": "2.0", "id": 1, "result": "0x10"}])
    n = await fetch_counter_for_wallet(
        session=session, wallet=wallet_evm,
        evm_rpc_url="http://evm", solana_rpc_url="http://sol",
    )
    assert n == 16
    assert session.posted[0][0] == "http://evm"


# ---------------------------------------------------------------------------
# Snapshot orchestration + full run
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_snapshot_records_first_run_but_reports_no_active() -> None:
    store = WalletStore()
    add_wallet(store, name="evm-1", chain=CHAIN_EVM, passphrase="pp")
    activity = ActivityStore()
    settings = ReminderSettings(
        evm_rpc_url="http://evm", solana_rpc_url="http://sol",
        min_confidence="medium",
    )
    session = _MockSession([{"jsonrpc": "2.0", "id": 1, "result": "0x05"}])
    active = await snapshot_all_wallets(
        settings=settings, store=store, activity_store=activity,
        iso_week="2026-W22", session=session,
    )
    # First snapshot — no prior baseline, so wallet reported as not-active.
    assert active == []
    # ...but the snapshot is recorded for next time.
    assert activity.latest("evm-1").counter == 5


@pytest.mark.asyncio
async def test_snapshot_detects_active_wallet_on_second_week() -> None:
    store = WalletStore()
    add_wallet(store, name="evm-1", chain=CHAIN_EVM, passphrase="pp")
    activity = ActivityStore()
    # Pre-seed: last week the counter was 5.
    activity.record(ActivitySnapshot(
        wallet_name="evm-1", chain=CHAIN_EVM, address="0x",
        counter=5, iso_week="2026-W21",
        captured_at_iso="2026-05-25T00:00:00+00:00",
    ))
    settings = ReminderSettings(
        evm_rpc_url="http://evm", solana_rpc_url="http://sol",
        min_confidence="medium",
    )
    # This week the counter rose to 7 — wallet was active.
    session = _MockSession([{"jsonrpc": "2.0", "id": 1, "result": "0x07"}])
    active = await snapshot_all_wallets(
        settings=settings, store=store, activity_store=activity,
        iso_week="2026-W22", session=session,
    )
    assert [w.name for w in active] == ["evm-1"]


@pytest.mark.asyncio
async def test_snapshot_skips_wallets_whose_rpc_fails() -> None:
    store = WalletStore()
    add_wallet(store, name="evm-1", chain=CHAIN_EVM, passphrase="pp")
    activity = ActivityStore()
    settings = ReminderSettings(
        evm_rpc_url="http://evm", solana_rpc_url="http://sol",
        min_confidence="medium",
    )

    class _FailingSession:
        def post(self, *_a, **_kw):
            raise RuntimeError("boom")

    # Should NOT raise — RPC failures are logged + skipped.
    active = await snapshot_all_wallets(
        settings=settings, store=store, activity_store=activity,
        iso_week="2026-W22", session=_FailingSession(),
    )
    assert active == []


@pytest.mark.asyncio
async def test_run_reminder_end_to_end(tmp_path: Path) -> None:
    # Wallet store: one EVM wallet.
    store_path = tmp_path / "wallets.json"
    state_path = tmp_path / "state.json"
    activity_path = tmp_path / "activity.json"
    store = WalletStore()
    add_wallet(store, name="evm-1", chain=CHAIN_EVM, passphrase="pp")
    save_store(store, store_path)

    # Pre-seed activity from a prior week.
    activity = ActivityStore()
    activity.record(ActivitySnapshot(
        wallet_name="evm-1", chain=CHAIN_EVM, address="0x",
        counter=10, iso_week="2026-W21",
        captured_at_iso="2026-05-25T00:00:00+00:00",
    ))
    save_activity_store(activity, activity_path)

    settings = ReminderSettings(
        evm_rpc_url="http://evm", solana_rpc_url="http://sol",
        min_confidence="medium", telegram_bot_token="", telegram_chat_id="",
    )

    # Wallet's counter rose this week.
    session = _MockSession([{"jsonrpc": "2.0", "id": 1, "result": "0x14"}])

    # Patch current_iso_week so the run is deterministic.
    import src.airdrop.reminder as reminder_mod
    original = reminder_mod.current_iso_week
    reminder_mod.current_iso_week = lambda *_a, **_kw: "2026-W22"
    try:
        result = await run_reminder(
            settings=settings, store_path=store_path, state_path=state_path,
            activity_path=activity_path, session=session,
        )
    finally:
        reminder_mod.current_iso_week = original

    assert result.iso_week == "2026-W22"
    assert result.active_wallets == ["evm-1"]
    # Auto-marked at least one EVM-active opportunity.
    assert len(result.auto_marked) >= 1
    # Telegram unset → push reports False but the orchestrator didn't crash.
    assert result.telegram_pushed is False
    # State file written.
    assert state_path.exists()
    # Activity file updated with the new snapshot.
    reloaded = load_activity_store(activity_path)
    assert reloaded.latest("evm-1").counter == 0x14
    assert reloaded.latest("evm-1").iso_week == "2026-W22"

"""Tests for airdrop wallet manager, opportunity catalog, and scheduler."""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from eth_account import Account
from solders.keypair import Keypair as SolKeypair

from src.airdrop.catalog import filter_by, opportunities
from src.airdrop.scheduler import (
    ChecklistRow,
    CompletionState,
    TaskKey,
    build_checklist,
    current_iso_week,
    load_state,
    render_checklist,
    save_state,
)
from src.airdrop.wallet import (
    CHAIN_EVM,
    CHAIN_SOLANA,
    AirdropWalletError,
    EncryptedWallet,
    WalletStore,
    add_wallet,
    load_store,
    reveal_evm_account,
    reveal_secret,
    reveal_solana_keypair,
    save_store,
)


# ---------------------------------------------------------------------------
# wallet.py
# ---------------------------------------------------------------------------


def test_add_solana_wallet_creates_loadable_keypair() -> None:
    store = WalletStore()
    w = add_wallet(store, name="sol-1", chain=CHAIN_SOLANA, passphrase="pp")
    assert w.chain == CHAIN_SOLANA
    # Address is the Solana pubkey in base58 (32 < len < 44).
    assert 32 < len(w.address) < 50
    kp = reveal_solana_keypair(w, store=store, passphrase="pp")
    assert isinstance(kp, SolKeypair)
    assert str(kp.pubkey()) == w.address


def test_add_evm_wallet_creates_loadable_account() -> None:
    store = WalletStore()
    w = add_wallet(store, name="eth-1", chain=CHAIN_EVM, passphrase="pp")
    assert w.chain == CHAIN_EVM
    assert w.address.startswith("0x") and len(w.address) == 42
    acct = reveal_evm_account(w, store=store, passphrase="pp")
    assert acct.address == w.address


def test_add_wallet_rejects_unknown_chain() -> None:
    store = WalletStore()
    with pytest.raises(AirdropWalletError, match="chain must be"):
        add_wallet(store, name="x", chain="bitcoin", passphrase="pp")


def test_add_wallet_rejects_empty_name() -> None:
    store = WalletStore()
    with pytest.raises(AirdropWalletError, match="name must not"):
        add_wallet(store, name="", chain=CHAIN_SOLANA, passphrase="pp")


def test_add_wallet_rejects_duplicate_name() -> None:
    store = WalletStore()
    add_wallet(store, name="dup", chain=CHAIN_SOLANA, passphrase="pp")
    with pytest.raises(AirdropWalletError, match="already exists"):
        add_wallet(store, name="dup", chain=CHAIN_EVM, passphrase="pp")


def test_reveal_with_wrong_passphrase_raises() -> None:
    store = WalletStore()
    w = add_wallet(store, name="x", chain=CHAIN_SOLANA, passphrase="right")
    with pytest.raises(AirdropWalletError, match="decryption failed"):
        reveal_secret(w, store=store, passphrase="wrong")


def test_reveal_evm_rejects_solana_wallet() -> None:
    store = WalletStore()
    w = add_wallet(store, name="x", chain=CHAIN_SOLANA, passphrase="pp")
    with pytest.raises(AirdropWalletError, match="not EVM"):
        reveal_evm_account(w, store=store, passphrase="pp")


def test_reveal_solana_rejects_evm_wallet() -> None:
    store = WalletStore()
    w = add_wallet(store, name="x", chain=CHAIN_EVM, passphrase="pp")
    with pytest.raises(AirdropWalletError, match="not Solana"):
        reveal_solana_keypair(w, store=store, passphrase="pp")


def test_store_round_trip_to_disk(tmp_path: Path) -> None:
    path = tmp_path / "store.json"
    store = WalletStore()
    add_wallet(store, name="a", chain=CHAIN_SOLANA, passphrase="pp")
    add_wallet(store, name="b", chain=CHAIN_EVM, passphrase="pp")
    save_store(store, path)
    loaded = load_store(path)
    assert [w.name for w in loaded.wallets] == ["a", "b"]
    assert reveal_secret(loaded.wallets[0], store=loaded, passphrase="pp")


def test_store_save_writes_0600(tmp_path: Path) -> None:
    path = tmp_path / "s.json"
    store = WalletStore()
    add_wallet(store, name="x", chain=CHAIN_SOLANA, passphrase="pp")
    save_store(store, path)
    import stat
    mode = path.stat().st_mode & 0o777
    assert mode == 0o600


def test_load_store_returns_empty_when_missing(tmp_path: Path) -> None:
    store = load_store(tmp_path / "missing.json")
    assert store.wallets == []


def test_load_store_raises_on_corrupt_json(tmp_path: Path) -> None:
    p = tmp_path / "s.json"
    p.write_text("not json")
    with pytest.raises(AirdropWalletError, match="could not read"):
        load_store(p)


# ---------------------------------------------------------------------------
# catalog.py
# ---------------------------------------------------------------------------


def test_catalog_has_solana_and_evm_entries() -> None:
    chains = {opp.chain for opp in opportunities}
    assert "solana" in chains and "evm" in chains


def test_filter_by_chain() -> None:
    sol = filter_by(chain="solana")
    assert all(o.chain == "solana" for o in sol)
    assert len(sol) > 0


def test_filter_by_status() -> None:
    active = filter_by(status="active")
    assert all(o.status == "active" for o in active)


def test_filter_by_min_confidence_medium() -> None:
    medium_plus = filter_by(min_confidence="medium")
    assert all(o.confidence in {"medium", "high"} for o in medium_plus)


# ---------------------------------------------------------------------------
# scheduler.py
# ---------------------------------------------------------------------------


def test_current_iso_week_has_year_w_format() -> None:
    label = current_iso_week()
    assert "-W" in label and len(label) == 8  # YYYY-Wnn


def test_task_key_round_trip() -> None:
    k = TaskKey(wallet_name="w1", opportunity_name="Kamino Finance")
    assert TaskKey.from_str(k.as_str()) == k


def test_completion_state_mark_then_check_returns_true() -> None:
    state = CompletionState()
    k = TaskKey("w", "o")
    state.mark(k, iso_week="2026-W22")
    assert state.is_completed_this_week(k, current_iso_week="2026-W22") is True
    assert state.is_completed_this_week(k, current_iso_week="2026-W23") is False


def test_completion_state_unmark_removes_entry() -> None:
    state = CompletionState()
    k = TaskKey("w", "o")
    state.mark(k, iso_week="2026-W22")
    state.unmark(k)
    assert state.is_completed_this_week(k, current_iso_week="2026-W22") is False


def test_build_checklist_emits_one_row_per_wallet_opp_pair() -> None:
    store = WalletStore()
    add_wallet(store, name="sol-1", chain=CHAIN_SOLANA, passphrase="pp")
    add_wallet(store, name="evm-1", chain=CHAIN_EVM, passphrase="pp")
    rows = build_checklist(store=store, state=CompletionState(),
                            iso_week="2026-W22", status_filter="active")
    # Each wallet × opportunities-for-its-chain.
    sol_active = filter_by(chain="solana", status="active")
    evm_active = filter_by(chain="evm", status="active")
    assert len(rows) == len(sol_active) + len(evm_active)


def test_build_checklist_marks_completed_rows() -> None:
    store = WalletStore()
    add_wallet(store, name="sol-1", chain=CHAIN_SOLANA, passphrase="pp")
    state = CompletionState()
    # Mark the first solana-active opportunity as done.
    sol_active = filter_by(chain="solana", status="active")
    target = sol_active[0]
    state.mark(TaskKey("sol-1", target.name), iso_week="2026-W22")
    rows = build_checklist(store=store, state=state, iso_week="2026-W22",
                             status_filter="active")
    completed = [r for r in rows if r.status == "completed"]
    pending = [r for r in rows if r.status == "pending"]
    assert len(completed) == 1
    assert completed[0].opportunity == target.name
    assert all(r.opportunity != target.name for r in pending)


def test_state_save_load_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "state.json"
    state = CompletionState()
    state.mark(TaskKey("w1", "Kamino"), iso_week="2026-W22")
    save_state(state, path)
    loaded = load_state(path)
    assert loaded.is_completed_this_week(TaskKey("w1", "Kamino"),
                                          current_iso_week="2026-W22") is True


def test_render_checklist_includes_address_and_status_marks() -> None:
    store = WalletStore()
    add_wallet(store, name="sol-1", chain=CHAIN_SOLANA, passphrase="pp")
    rows = build_checklist(store=store, state=CompletionState(),
                            iso_week="2026-W22", status_filter="active")
    text = render_checklist(rows, iso_week="2026-W22")
    assert "AIRDROP CHECKLIST" in text
    assert "WALLET: sol-1" in text
    assert "[ ]" in text  # pending mark

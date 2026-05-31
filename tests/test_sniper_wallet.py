"""Tests for sniper wallet generation, encryption, and load round-trip."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from solders.keypair import Keypair

from src.sniper.wallet import (
    SniperWalletError,
    WalletFile,
    decrypt_payload,
    encrypt_payload,
    generate_wallet,
    load_keypair,
    load_wallet_file,
    save_wallet,
)


def test_encrypt_decrypt_round_trip() -> None:
    secret = bytes(Keypair().to_bytes())
    wf, pubkey = encrypt_payload(secret, "correct-horse-battery-staple")
    assert wf.pubkey == pubkey
    plaintext = decrypt_payload(wf, "correct-horse-battery-staple")
    assert plaintext == secret


def test_decrypt_with_wrong_passphrase_raises() -> None:
    secret = bytes(Keypair().to_bytes())
    wf, _ = encrypt_payload(secret, "right")
    with pytest.raises(SniperWalletError, match="wallet decryption failed"):
        decrypt_payload(wf, "wrong")


def test_decrypt_with_tampered_ciphertext_raises() -> None:
    secret = bytes(Keypair().to_bytes())
    wf, _ = encrypt_payload(secret, "pp")
    tampered = WalletFile(
        version=wf.version, pubkey=wf.pubkey, kdf_iterations=wf.kdf_iterations,
        salt_b64=wf.salt_b64, iv_b64=wf.iv_b64,
        # Flip a byte in the ciphertext.
        ciphertext_b64=("A" + wf.ciphertext_b64[1:]
                          if wf.ciphertext_b64[0] != "A" else "B" + wf.ciphertext_b64[1:]),
        mac_b64=wf.mac_b64,
    )
    with pytest.raises(SniperWalletError, match="wallet decryption failed"):
        decrypt_payload(tampered, "pp")


def test_save_then_load_round_trip(tmp_path: Path) -> None:
    secret = bytes(Keypair().to_bytes())
    wf, _ = encrypt_payload(secret, "pp")
    path = tmp_path / "wallet.json"
    save_wallet(path, wf)
    loaded = load_wallet_file(path)
    assert loaded.pubkey == wf.pubkey
    assert decrypt_payload(loaded, "pp") == secret


def test_save_refuses_to_overwrite_existing(tmp_path: Path) -> None:
    secret = bytes(Keypair().to_bytes())
    wf, _ = encrypt_payload(secret, "pp")
    path = tmp_path / "w.json"
    save_wallet(path, wf)
    with pytest.raises(SniperWalletError, match="already exists"):
        save_wallet(path, wf)
    save_wallet(path, wf, overwrite=True)  # allowed with explicit overwrite


def test_save_writes_with_0o600_permissions(tmp_path: Path) -> None:
    secret = bytes(Keypair().to_bytes())
    wf, _ = encrypt_payload(secret, "pp")
    path = tmp_path / "w.json"
    save_wallet(path, wf)
    import stat
    mode = path.stat().st_mode & 0o777
    assert mode == 0o600


def test_load_wallet_file_raises_when_missing(tmp_path: Path) -> None:
    with pytest.raises(SniperWalletError, match="no wallet"):
        load_wallet_file(tmp_path / "nope.json")


def test_load_wallet_file_raises_on_corrupt_json(tmp_path: Path) -> None:
    path = tmp_path / "w.json"
    path.write_text("not valid json")
    with pytest.raises(SniperWalletError, match="could not read"):
        load_wallet_file(path)


def test_load_wallet_file_raises_on_missing_fields(tmp_path: Path) -> None:
    path = tmp_path / "w.json"
    path.write_text(json.dumps({"version": 1, "pubkey": "X"}))
    with pytest.raises(SniperWalletError, match="malformed"):
        load_wallet_file(path)


def test_generate_wallet_creates_loadable_keypair(tmp_path: Path) -> None:
    path = tmp_path / "wallet.json"
    pubkey = generate_wallet(path, "mypp")
    kp = load_keypair(path, "mypp")
    assert str(kp.pubkey()) == pubkey


def test_load_keypair_reads_passphrase_from_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "wallet.json"
    pubkey = generate_wallet(path, "envpp")
    monkeypatch.setenv("SNIPER_WALLET_PASSPHRASE", "envpp")
    kp = load_keypair(path)
    assert str(kp.pubkey()) == pubkey


def test_encrypt_rejects_empty_passphrase() -> None:
    secret = bytes(Keypair().to_bytes())
    with pytest.raises(SniperWalletError, match="passphrase"):
        encrypt_payload(secret, "")


def test_encrypt_rejects_wrong_size_secret() -> None:
    with pytest.raises(SniperWalletError, match="64 bytes"):
        encrypt_payload(b"short", "pp")

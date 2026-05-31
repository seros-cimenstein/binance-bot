"""Solana wallet generation + encrypted storage for the sniper.

The wallet's private key never appears in plain text on disk. It is
encrypted at-rest with a key derived from a user-supplied passphrase via
PBKDF2-SHA256; the JSON file stores only the salt, IV, and ciphertext.

Public address is exposed in plaintext alongside the encrypted payload so
operators can fund the wallet without unlocking it.

A wallet file is generated once. Subsequent loads require the passphrase
(supplied via ``SNIPER_WALLET_PASSPHRASE`` env var or interactive prompt).
"""

from __future__ import annotations

import base64
import getpass
import hashlib
import hmac
import json
import os
import secrets
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from solders.keypair import Keypair


class SniperWalletError(RuntimeError):
    """Raised when the wallet cannot be loaded, decrypted, or saved."""


_PBKDF2_ITERATIONS = 200_000
_KDF_ALG = "sha256"
_VERSION = 1


@dataclass(frozen=True, slots=True)
class WalletFile:
    """Serialised on-disk representation of an encrypted Solana wallet."""

    version: int
    pubkey: str
    kdf_iterations: int
    salt_b64: str
    iv_b64: str
    ciphertext_b64: str
    mac_b64: str

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "WalletFile":
        try:
            return cls(
                version=int(payload["version"]),
                pubkey=str(payload["pubkey"]),
                kdf_iterations=int(payload["kdf_iterations"]),
                salt_b64=str(payload["salt_b64"]),
                iv_b64=str(payload["iv_b64"]),
                ciphertext_b64=str(payload["ciphertext_b64"]),
                mac_b64=str(payload["mac_b64"]),
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise SniperWalletError(f"wallet file is malformed: {exc}") from exc

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": self.version, "pubkey": self.pubkey,
            "kdf_iterations": self.kdf_iterations,
            "salt_b64": self.salt_b64, "iv_b64": self.iv_b64,
            "ciphertext_b64": self.ciphertext_b64, "mac_b64": self.mac_b64,
        }


def _derive_key(passphrase: str, salt: bytes, iterations: int) -> bytes:
    if not passphrase:
        raise SniperWalletError("passphrase must not be empty")
    return hashlib.pbkdf2_hmac(_KDF_ALG, passphrase.encode("utf-8"),
                                salt, iterations, dklen=32)


def _xor_stream(key: bytes, iv: bytes, data: bytes) -> bytes:
    """AES-style stream cipher using HMAC-CTR construction.

    We deliberately avoid the optional ``cryptography`` dep. SHA256-HMAC keyed
    by (key, iv || counter) gives an authenticated keystream that is a
    drop-in for symmetric encryption + MAC at our small message sizes
    (a 64-byte Solana keypair). For bigger files prefer a real AEAD.
    """
    out = bytearray()
    counter = 0
    while len(out) < len(data):
        block = hmac.new(key, iv + counter.to_bytes(8, "big"), hashlib.sha256).digest()
        out.extend(block)
        counter += 1
    keystream = bytes(out[: len(data)])
    return bytes(a ^ b for a, b in zip(data, keystream))


def _mac(key: bytes, *parts: bytes) -> bytes:
    h = hmac.new(key, b"", hashlib.sha256)
    for p in parts:
        h.update(p)
    return h.digest()


def encrypt_payload(secret_key: bytes, passphrase: str) -> tuple[WalletFile, str]:
    if len(secret_key) != 64:
        raise SniperWalletError(f"secret_key must be 64 bytes (got {len(secret_key)})")
    salt = secrets.token_bytes(16)
    iv = secrets.token_bytes(12)
    derived = _derive_key(passphrase, salt, _PBKDF2_ITERATIONS)
    enc_key = derived  # full 32 bytes used for the stream cipher
    mac_key = hashlib.sha256(derived + b"\x01mac").digest()
    ciphertext = _xor_stream(enc_key, iv, secret_key)
    mac = _mac(mac_key, salt, iv, ciphertext)
    pubkey = str(Keypair.from_bytes(secret_key).pubkey())
    wf = WalletFile(
        version=_VERSION, pubkey=pubkey,
        kdf_iterations=_PBKDF2_ITERATIONS,
        salt_b64=base64.b64encode(salt).decode(),
        iv_b64=base64.b64encode(iv).decode(),
        ciphertext_b64=base64.b64encode(ciphertext).decode(),
        mac_b64=base64.b64encode(mac).decode(),
    )
    return wf, pubkey


def decrypt_payload(wf: WalletFile, passphrase: str) -> bytes:
    salt = base64.b64decode(wf.salt_b64)
    iv = base64.b64decode(wf.iv_b64)
    ciphertext = base64.b64decode(wf.ciphertext_b64)
    expected_mac = base64.b64decode(wf.mac_b64)
    derived = _derive_key(passphrase, salt, wf.kdf_iterations)
    mac_key = hashlib.sha256(derived + b"\x01mac").digest()
    actual_mac = _mac(mac_key, salt, iv, ciphertext)
    if not hmac.compare_digest(expected_mac, actual_mac):
        raise SniperWalletError("wallet decryption failed — wrong passphrase or file tampered")
    return _xor_stream(derived, iv, ciphertext)


def save_wallet(path: Path | str, wallet_file: WalletFile, *, overwrite: bool = False) -> None:
    p = Path(path)
    if p.exists() and not overwrite:
        raise SniperWalletError(f"wallet already exists at {p}; pass overwrite=True to replace")
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(wallet_file.to_dict(), indent=2, sort_keys=True))
    os.chmod(tmp, 0o600)
    tmp.replace(p)


def load_wallet_file(path: Path | str) -> WalletFile:
    p = Path(path)
    if not p.exists():
        raise SniperWalletError(f"no wallet at {p}; generate one first")
    try:
        return WalletFile.from_dict(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise SniperWalletError(f"could not read wallet at {p}: {exc}") from exc


def load_keypair(path: Path | str, passphrase: str | None = None) -> Keypair:
    """Decrypt the wallet file and return the runtime Keypair.

    If ``passphrase`` is None, looks at ``SNIPER_WALLET_PASSPHRASE`` env, then
    falls back to an interactive prompt via ``getpass`` (TTY only).
    """
    wf = load_wallet_file(path)
    pp = passphrase
    if pp is None:
        pp = os.environ.get("SNIPER_WALLET_PASSPHRASE", "")
    if not pp:
        pp = getpass.getpass("Sniper wallet passphrase: ")
    secret = decrypt_payload(wf, pp)
    return Keypair.from_bytes(secret)


def generate_wallet(path: Path | str, passphrase: str, *, overwrite: bool = False) -> str:
    """Generate a fresh Solana keypair, encrypt + save it, return the public address."""
    kp = Keypair()
    wf, pubkey = encrypt_payload(bytes(kp.to_bytes()), passphrase)
    save_wallet(path, wf, overwrite=overwrite)
    return pubkey

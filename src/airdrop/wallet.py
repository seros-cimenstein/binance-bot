"""Multi-chain wallet manager for the airdrop farmer.

Generates and encrypts Solana + EVM keypairs. Uses the same PBKDF2 +
HMAC-stream + MAC scheme as ``src.sniper.wallet`` so we have one
encryption story across the repo.

A wallet store is a single JSON file containing many encrypted wallets,
each tagged with a name and the chain it belongs to. Pubkeys / addresses
stay in cleartext so the operator can fund or audit without unlocking.
"""

from __future__ import annotations

import base64
import getpass
import hashlib
import hmac
import json
import os
import secrets
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from eth_account import Account
from solders.keypair import Keypair as SolKeypair


class AirdropWalletError(RuntimeError):
    """Raised when wallet generation, loading, or saving fails."""


_PBKDF2_ITERATIONS = 200_000
_VERSION = 1
CHAIN_SOLANA = "solana"
CHAIN_EVM = "evm"
_VALID_CHAINS = frozenset({CHAIN_SOLANA, CHAIN_EVM})


@dataclass(frozen=True, slots=True)
class EncryptedWallet:
    """One encrypted keypair inside the multi-wallet store."""

    name: str
    chain: str               # "solana" | "evm"
    address: str             # cleartext, fundable
    salt_b64: str
    iv_b64: str
    ciphertext_b64: str
    mac_b64: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name, "chain": self.chain, "address": self.address,
            "salt_b64": self.salt_b64, "iv_b64": self.iv_b64,
            "ciphertext_b64": self.ciphertext_b64, "mac_b64": self.mac_b64,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "EncryptedWallet":
        try:
            return cls(
                name=str(payload["name"]),
                chain=str(payload["chain"]),
                address=str(payload["address"]),
                salt_b64=str(payload["salt_b64"]),
                iv_b64=str(payload["iv_b64"]),
                ciphertext_b64=str(payload["ciphertext_b64"]),
                mac_b64=str(payload["mac_b64"]),
            )
        except KeyError as exc:
            raise AirdropWalletError(f"wallet record is malformed: {exc}") from exc


@dataclass(slots=True)
class WalletStore:
    """A JSON-on-disk collection of encrypted wallets."""

    version: int = _VERSION
    kdf_iterations: int = _PBKDF2_ITERATIONS
    wallets: list[EncryptedWallet] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": self.version,
            "kdf_iterations": self.kdf_iterations,
            "wallets": [w.to_dict() for w in self.wallets],
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "WalletStore":
        try:
            wallets = [EncryptedWallet.from_dict(w) for w in payload.get("wallets") or []]
            return cls(
                version=int(payload.get("version", _VERSION)),
                kdf_iterations=int(payload.get("kdf_iterations", _PBKDF2_ITERATIONS)),
                wallets=wallets,
            )
        except (TypeError, ValueError) as exc:
            raise AirdropWalletError(f"wallet store is malformed: {exc}") from exc

    def by_name(self, name: str) -> EncryptedWallet | None:
        for w in self.wallets:
            if w.name == name:
                return w
        return None


# ---------------------------------------------------------------------------
# Crypto primitives (shared with sniper.wallet style)
# ---------------------------------------------------------------------------


def _derive_key(passphrase: str, salt: bytes, iterations: int) -> bytes:
    if not passphrase:
        raise AirdropWalletError("passphrase must not be empty")
    return hashlib.pbkdf2_hmac("sha256", passphrase.encode("utf-8"),
                                salt, iterations, dklen=32)


def _xor_stream(key: bytes, iv: bytes, data: bytes) -> bytes:
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


def _encrypt_secret(secret: bytes, passphrase: str, iterations: int) -> tuple[str, str, str, str]:
    """Returns (salt_b64, iv_b64, ciphertext_b64, mac_b64)."""
    salt = secrets.token_bytes(16)
    iv = secrets.token_bytes(12)
    derived = _derive_key(passphrase, salt, iterations)
    mac_key = hashlib.sha256(derived + b"\x01mac").digest()
    ciphertext = _xor_stream(derived, iv, secret)
    mac = _mac(mac_key, salt, iv, ciphertext)
    return (
        base64.b64encode(salt).decode(),
        base64.b64encode(iv).decode(),
        base64.b64encode(ciphertext).decode(),
        base64.b64encode(mac).decode(),
    )


def _decrypt_secret(wallet: EncryptedWallet, passphrase: str, iterations: int) -> bytes:
    salt = base64.b64decode(wallet.salt_b64)
    iv = base64.b64decode(wallet.iv_b64)
    ciphertext = base64.b64decode(wallet.ciphertext_b64)
    expected_mac = base64.b64decode(wallet.mac_b64)
    derived = _derive_key(passphrase, salt, iterations)
    mac_key = hashlib.sha256(derived + b"\x01mac").digest()
    actual_mac = _mac(mac_key, salt, iv, ciphertext)
    if not hmac.compare_digest(expected_mac, actual_mac):
        raise AirdropWalletError(
            f"decryption failed for wallet '{wallet.name}' — wrong passphrase or tampered file"
        )
    return _xor_stream(derived, iv, ciphertext)


# ---------------------------------------------------------------------------
# Generation + storage
# ---------------------------------------------------------------------------


def _generate_secret_for_chain(chain: str) -> tuple[str, bytes]:
    """Return ``(address, raw_secret_bytes)`` for a fresh key on this chain."""
    if chain == CHAIN_SOLANA:
        kp = SolKeypair()
        return str(kp.pubkey()), bytes(kp.to_bytes())
    if chain == CHAIN_EVM:
        acct = Account.create()
        return acct.address, bytes(acct.key)
    raise AirdropWalletError(f"unknown chain: {chain}")


def add_wallet(
    store: WalletStore, *, name: str, chain: str, passphrase: str,
) -> EncryptedWallet:
    if chain not in _VALID_CHAINS:
        raise AirdropWalletError(f"chain must be one of {sorted(_VALID_CHAINS)}")
    if not name:
        raise AirdropWalletError("name must not be empty")
    if store.by_name(name) is not None:
        raise AirdropWalletError(f"wallet name '{name}' already exists in this store")
    address, secret = _generate_secret_for_chain(chain)
    salt_b64, iv_b64, ct_b64, mac_b64 = _encrypt_secret(secret, passphrase, store.kdf_iterations)
    wallet = EncryptedWallet(
        name=name, chain=chain, address=address,
        salt_b64=salt_b64, iv_b64=iv_b64, ciphertext_b64=ct_b64, mac_b64=mac_b64,
    )
    store.wallets.append(wallet)
    return wallet


def load_store(path: Path | str) -> WalletStore:
    p = Path(path)
    if not p.exists():
        return WalletStore()
    try:
        return WalletStore.from_dict(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise AirdropWalletError(f"could not read store at {p}: {exc}") from exc


def save_store(store: WalletStore, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(store.to_dict(), indent=2, sort_keys=True))
    os.chmod(tmp, 0o600)
    tmp.replace(p)


def reveal_secret(
    wallet: EncryptedWallet, *, store: WalletStore, passphrase: str | None = None,
) -> bytes:
    if passphrase is None:
        passphrase = os.environ.get("AIRDROP_WALLET_PASSPHRASE", "")
    if not passphrase:
        passphrase = getpass.getpass(f"Passphrase for '{wallet.name}': ")
    return _decrypt_secret(wallet, passphrase, store.kdf_iterations)


def reveal_evm_account(
    wallet: EncryptedWallet, *, store: WalletStore, passphrase: str | None = None,
):
    if wallet.chain != CHAIN_EVM:
        raise AirdropWalletError(f"wallet '{wallet.name}' is not EVM (chain={wallet.chain})")
    secret = reveal_secret(wallet, store=store, passphrase=passphrase)
    return Account.from_key(secret)


def reveal_solana_keypair(
    wallet: EncryptedWallet, *, store: WalletStore, passphrase: str | None = None,
) -> SolKeypair:
    if wallet.chain != CHAIN_SOLANA:
        raise AirdropWalletError(f"wallet '{wallet.name}' is not Solana (chain={wallet.chain})")
    secret = reveal_secret(wallet, store=store, passphrase=passphrase)
    return SolKeypair.from_bytes(secret)

//! Multi-chain wallet manager for the airdrop farmer.
//!
//! Generates and encrypts Solana + EVM keypairs with a PBKDF2 + HMAC-stream +
//! MAC scheme (byte-compatible with stores written by the Python
//! implementation). A wallet store is a single JSON file containing many
//! encrypted wallets, each tagged with a name and the chain it belongs to.
//! Pubkeys / addresses stay in cleartext so the operator can fund or audit
//! without unlocking.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use sha3::Keccak256;
use zeroize::Zeroizing;

use crate::catalog::Chain;
use crate::error::{AirdropError, Result};

pub const PBKDF2_ITERATIONS: u32 = 200_000;
pub const STORE_VERSION: u32 = 1;
pub const PASSPHRASE_ENV: &str = "AIRDROP_WALLET_PASSPHRASE";

const SALT_LEN: usize = 16;
const IV_LEN: usize = 12;
const SOLANA_KEYPAIR_LEN: usize = 64;
const EVM_KEY_LEN: usize = 32;

/// One encrypted keypair inside the multi-wallet store.
///
/// Field order is alphabetical so the serialized JSON matches the Python
/// writer (`json.dumps(..., sort_keys=True)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedWallet {
    /// Cleartext, fundable address (EIP-55 hex for EVM, base58 for Solana).
    pub address: String,
    pub chain: Chain,
    pub ciphertext_b64: String,
    pub iv_b64: String,
    pub mac_b64: String,
    pub name: String,
    pub salt_b64: String,
}

/// A JSON-on-disk collection of encrypted wallets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletStore {
    #[serde(default = "default_iterations")]
    pub kdf_iterations: u32,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub wallets: Vec<EncryptedWallet>,
}

fn default_iterations() -> u32 {
    PBKDF2_ITERATIONS
}

fn default_version() -> u32 {
    STORE_VERSION
}

fn null_as_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<EncryptedWallet>, D::Error> {
    Ok(Option::<Vec<EncryptedWallet>>::deserialize(d)?.unwrap_or_default())
}

impl Default for WalletStore {
    fn default() -> Self {
        Self {
            kdf_iterations: PBKDF2_ITERATIONS,
            version: STORE_VERSION,
            wallets: Vec::new(),
        }
    }
}

impl WalletStore {
    pub fn by_name(&self, name: &str) -> Option<&EncryptedWallet> {
        self.wallets.iter().find(|w| w.name == name)
    }
}

/// An unlocked EVM account: secp256k1 key plus its EIP-55 address.
pub struct EvmAccount {
    key: k256::SecretKey,
    address: String,
}

impl EvmAccount {
    pub fn from_key(secret: &[u8]) -> std::result::Result<Self, k256::elliptic_curve::Error> {
        let key = k256::SecretKey::from_slice(secret)?;
        let address = evm_address(&key);
        Ok(Self { key, address })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn secret_key(&self) -> &k256::SecretKey {
        &self.key
    }
}

impl std::fmt::Debug for EvmAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvmAccount")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// EIP-55 checksummed address of a secp256k1 key.
pub fn evm_address(key: &k256::SecretKey) -> String {
    use k256::elliptic_curve::sec1::ToEncodedPoint as _;
    let point = key.public_key().to_encoded_point(false);
    // Uncompressed SEC1 = 0x04 || X || Y; the address hashes X || Y.
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    to_checksum_address(&hash[12..])
}

fn to_checksum_address(addr: &[u8]) -> String {
    let lower = hex_lower(addr);
    let hash = Keccak256::digest(lower.as_bytes());
    let mut out = String::with_capacity(2 + lower.len());
    out.push_str("0x");
    for (i, c) in lower.chars().enumerate() {
        let nibble = (hash[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0x0f;
        out.push(if c.is_ascii_alphabetic() && nibble >= 8 {
            c.to_ascii_uppercase()
        } else {
            c
        });
    }
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Base58 Solana address of an ed25519 key.
pub fn solana_address(key: &ed25519_dalek::SigningKey) -> String {
    bs58::encode(key.verifying_key().as_bytes()).into_string()
}

// ---------------------------------------------------------------------------
// Crypto primitives
// ---------------------------------------------------------------------------

fn derive_key(passphrase: &str, salt: &[u8], iterations: u32) -> Result<Zeroizing<[u8; 32]>> {
    if passphrase.is_empty() {
        return Err(AirdropError::EmptyPassphrase);
    }
    let mut out = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, iterations, out.as_mut());
    Ok(out)
}

/// HMAC-SHA256 keyed with a 32-byte key. RFC 2104 zero-pads keys shorter than
/// the 64-byte block, so padding here is exactly HMAC(key) — and infallible.
fn hmac_sha256(key: &[u8; 32]) -> Hmac<Sha256> {
    let mut block = Zeroizing::new([0u8; 64]);
    block[..32].copy_from_slice(key);
    <Hmac<Sha256> as KeyInit>::new(&(*block).into())
}

fn mac_key(derived: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut h = Sha256::new();
    h.update(derived);
    h.update(b"\x01mac");
    Zeroizing::new(h.finalize().into())
}

/// XOR `data` with the keystream `HMAC(key, iv || counter_be64)` blocks.
fn xor_stream(key: &[u8; 32], iv: &[u8], data: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(data.len()));
    for (counter, chunk) in (0u64..).zip(data.chunks(32)) {
        let mut mac = hmac_sha256(key);
        mac.update(iv);
        mac.update(&counter.to_be_bytes());
        let block = mac.finalize().into_bytes();
        out.extend(chunk.iter().zip(block.iter()).map(|(a, b)| a ^ b));
    }
    out
}

fn record_mac(mac_key: &[u8; 32], salt: &[u8], iv: &[u8], ciphertext: &[u8]) -> Hmac<Sha256> {
    let mut mac = hmac_sha256(mac_key);
    mac.update(salt);
    mac.update(iv);
    mac.update(ciphertext);
    mac
}

struct Sealed {
    salt_b64: String,
    iv_b64: String,
    ciphertext_b64: String,
    mac_b64: String,
}

fn encrypt_secret(secret: &[u8], passphrase: &str, iterations: u32) -> Result<Sealed> {
    let mut salt = [0u8; SALT_LEN];
    let mut iv = [0u8; IV_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut iv);
    let derived = derive_key(passphrase, &salt, iterations)?;
    let ciphertext = xor_stream(&derived, &iv, secret);
    let mac = record_mac(&mac_key(&derived), &salt, &iv, &ciphertext)
        .finalize()
        .into_bytes();
    Ok(Sealed {
        salt_b64: B64.encode(salt),
        iv_b64: B64.encode(iv),
        ciphertext_b64: B64.encode(&*ciphertext),
        mac_b64: B64.encode(mac),
    })
}

fn decode_field(wallet: &EncryptedWallet, field: &str, value: &str) -> Result<Vec<u8>> {
    B64.decode(value)
        .map_err(|e| AirdropError::MalformedRecord {
            name: wallet.name.clone(),
            reason: format!("{field}: {e}"),
        })
}

fn decrypt_secret(
    wallet: &EncryptedWallet,
    passphrase: &str,
    iterations: u32,
) -> Result<Zeroizing<Vec<u8>>> {
    let salt = decode_field(wallet, "salt_b64", &wallet.salt_b64)?;
    let iv = decode_field(wallet, "iv_b64", &wallet.iv_b64)?;
    let ciphertext = decode_field(wallet, "ciphertext_b64", &wallet.ciphertext_b64)?;
    let expected = decode_field(wallet, "mac_b64", &wallet.mac_b64)?;
    let derived = derive_key(passphrase, &salt, iterations)?;
    record_mac(&mac_key(&derived), &salt, &iv, &ciphertext)
        .verify_slice(&expected)
        .map_err(|_| AirdropError::DecryptionFailed(wallet.name.clone()))?;
    Ok(xor_stream(&derived, &iv, &ciphertext))
}

// ---------------------------------------------------------------------------
// Generation + storage
// ---------------------------------------------------------------------------

/// `(address, raw_secret_bytes)` for a fresh key on `chain`.
///
/// Solana secrets are the 64-byte `secret || pubkey` keypair encoding; EVM
/// secrets are the 32-byte secp256k1 scalar.
fn generate_secret_for_chain(chain: Chain) -> Result<(String, Zeroizing<Vec<u8>>)> {
    match chain {
        Chain::Solana => {
            let key = ed25519_dalek::SigningKey::generate(&mut OsRng);
            Ok((
                solana_address(&key),
                Zeroizing::new(key.to_keypair_bytes().to_vec()),
            ))
        }
        Chain::Evm => {
            let key = k256::SecretKey::random(&mut OsRng);
            Ok((evm_address(&key), Zeroizing::new(key.to_bytes().to_vec())))
        }
        Chain::Cex => Err(AirdropError::UnsupportedWalletChain),
    }
}

/// Generate a keypair for `chain`, encrypt it under `passphrase`, append it.
pub fn add_wallet(
    store: &mut WalletStore,
    name: &str,
    chain: Chain,
    passphrase: &str,
) -> Result<EncryptedWallet> {
    if chain == Chain::Cex {
        return Err(AirdropError::UnsupportedWalletChain);
    }
    if name.is_empty() {
        return Err(AirdropError::EmptyName);
    }
    if store.by_name(name).is_some() {
        return Err(AirdropError::DuplicateName(name.to_owned()));
    }
    let (address, secret) = generate_secret_for_chain(chain)?;
    let sealed = encrypt_secret(&secret, passphrase, store.kdf_iterations)?;
    let wallet = EncryptedWallet {
        address,
        chain,
        ciphertext_b64: sealed.ciphertext_b64,
        iv_b64: sealed.iv_b64,
        mac_b64: sealed.mac_b64,
        name: name.to_owned(),
        salt_b64: sealed.salt_b64,
    };
    store.wallets.push(wallet.clone());
    Ok(wallet)
}

/// Empty store when `path` does not exist.
pub fn load_store(path: &Path) -> Result<WalletStore> {
    crate::state::load_json(path, "store")
}

/// Atomic write; the file is created `0600` from the first byte.
pub fn save_store(store: &WalletStore, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(store).map_err(|e| AirdropError::Write {
        path: path.to_owned(),
        source: std::io::Error::other(e),
    })?;
    write_private_atomic(path, &bytes).map_err(|source| AirdropError::Write {
        path: path.to_owned(),
        source,
    })
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        opts.mode(0o600);
        let file = opts.open(&tmp)?;
        // `mode` only applies on creation; tighten a pre-existing temp file too.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        write_and_sync(file, bytes)?;
    }
    #[cfg(not(unix))]
    write_and_sync(opts.open(&tmp)?, bytes)?;
    std::fs::rename(&tmp, path)
}

fn write_and_sync(mut file: std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

/// `AIRDROP_WALLET_PASSPHRASE` verbatim (no trimming: whitespace is key
/// material), else an interactive no-echo prompt.
pub fn read_passphrase(prompt: &str) -> Result<String> {
    match std::env::var(PASSPHRASE_ENV) {
        Ok(pp) if !pp.is_empty() => Ok(pp),
        _ => rpassword::prompt_password(prompt).map_err(AirdropError::Prompt),
    }
}

/// Decrypt a wallet's raw secret. `None` passphrase falls back to
/// [`read_passphrase`].
pub fn reveal_secret(
    wallet: &EncryptedWallet,
    store: &WalletStore,
    passphrase: Option<&str>,
) -> Result<Zeroizing<Vec<u8>>> {
    let owned;
    let passphrase = match passphrase {
        Some(p) => p,
        None => {
            owned = Zeroizing::new(read_passphrase(&format!(
                "Passphrase for '{}': ",
                wallet.name
            ))?);
            owned.as_str()
        }
    };
    decrypt_secret(wallet, passphrase, store.kdf_iterations)
}

pub fn reveal_evm_account(
    wallet: &EncryptedWallet,
    store: &WalletStore,
    passphrase: Option<&str>,
) -> Result<EvmAccount> {
    if wallet.chain != Chain::Evm {
        return Err(wrong_chain(wallet, "EVM"));
    }
    let secret = reveal_secret(wallet, store, passphrase)?;
    if secret.len() != EVM_KEY_LEN {
        return Err(invalid_key(
            wallet,
            "EVM",
            format!("expected {EVM_KEY_LEN} bytes, got {}", secret.len()),
        ));
    }
    EvmAccount::from_key(&secret).map_err(|e| invalid_key(wallet, "EVM", e.to_string()))
}

pub fn reveal_solana_keypair(
    wallet: &EncryptedWallet,
    store: &WalletStore,
    passphrase: Option<&str>,
) -> Result<ed25519_dalek::SigningKey> {
    if wallet.chain != Chain::Solana {
        return Err(wrong_chain(wallet, "Solana"));
    }
    let secret = reveal_secret(wallet, store, passphrase)?;
    let bytes: Zeroizing<[u8; SOLANA_KEYPAIR_LEN]> =
        Zeroizing::new(secret.as_slice().try_into().map_err(|_| {
            invalid_key(
                wallet,
                "Solana",
                format!("expected {SOLANA_KEYPAIR_LEN} bytes, got {}", secret.len()),
            )
        })?);
    ed25519_dalek::SigningKey::from_keypair_bytes(&bytes)
        .map_err(|e| invalid_key(wallet, "Solana", e.to_string()))
}

fn wrong_chain(wallet: &EncryptedWallet, expected: &'static str) -> AirdropError {
    AirdropError::WrongChain {
        name: wallet.name.clone(),
        expected,
        actual: wallet.chain.to_string(),
    }
}

fn invalid_key(wallet: &EncryptedWallet, chain: &'static str, reason: String) -> AirdropError {
    AirdropError::InvalidKey {
        name: wallet.name.clone(),
        chain,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Low KDF cost keeps the suite fast; one test exercises the default.
    fn store() -> WalletStore {
        WalletStore {
            kdf_iterations: 1_000,
            ..WalletStore::default()
        }
    }

    #[test]
    fn add_solana_wallet_creates_loadable_keypair() {
        let mut s = store();
        let w = add_wallet(&mut s, "sol-1", Chain::Solana, "pp").unwrap();
        assert_eq!(w.chain, Chain::Solana);
        assert!(w.address.len() > 32 && w.address.len() < 50);
        let kp = reveal_solana_keypair(&w, &s, Some("pp")).unwrap();
        assert_eq!(solana_address(&kp), w.address);
    }

    #[test]
    fn add_evm_wallet_creates_loadable_account() {
        let mut s = WalletStore::default();
        let w = add_wallet(&mut s, "eth-1", Chain::Evm, "pp").unwrap();
        assert_eq!(w.chain, Chain::Evm);
        assert!(w.address.starts_with("0x") && w.address.len() == 42);
        let acct = reveal_evm_account(&w, &s, Some("pp")).unwrap();
        assert_eq!(acct.address(), w.address);
    }

    #[test]
    fn add_wallet_rejects_cex_chain() {
        let err = add_wallet(&mut store(), "x", Chain::Cex, "pp").unwrap_err();
        assert!(err.to_string().contains("chain must be"));
    }

    #[test]
    fn add_wallet_rejects_empty_name() {
        let err = add_wallet(&mut store(), "", Chain::Solana, "pp").unwrap_err();
        assert!(err.to_string().contains("name must not"));
    }

    #[test]
    fn add_wallet_rejects_duplicate_name() {
        let mut s = store();
        add_wallet(&mut s, "dup", Chain::Solana, "pp").unwrap();
        let err = add_wallet(&mut s, "dup", Chain::Evm, "pp").unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn add_wallet_rejects_empty_passphrase() {
        let err = add_wallet(&mut store(), "x", Chain::Evm, "").unwrap_err();
        assert!(matches!(err, AirdropError::EmptyPassphrase));
    }

    #[test]
    fn reveal_with_wrong_passphrase_raises() {
        let mut s = store();
        let w = add_wallet(&mut s, "x", Chain::Solana, "right").unwrap();
        let err = reveal_secret(&w, &s, Some("wrong")).unwrap_err();
        assert!(err.to_string().contains("decryption failed"));
    }

    #[test]
    fn reveal_detects_tampered_ciphertext() {
        let mut s = store();
        let mut w = add_wallet(&mut s, "x", Chain::Evm, "pp").unwrap();
        let mut ct = B64.decode(&w.ciphertext_b64).unwrap();
        ct[0] ^= 1;
        w.ciphertext_b64 = B64.encode(ct);
        assert!(matches!(
            reveal_secret(&w, &s, Some("pp")),
            Err(AirdropError::DecryptionFailed(_))
        ));
    }

    #[test]
    fn reveal_evm_rejects_solana_wallet() {
        let mut s = store();
        let w = add_wallet(&mut s, "x", Chain::Solana, "pp").unwrap();
        let err = reveal_evm_account(&w, &s, Some("pp")).unwrap_err();
        assert!(err.to_string().contains("not EVM"));
    }

    #[test]
    fn reveal_solana_rejects_evm_wallet() {
        let mut s = store();
        let w = add_wallet(&mut s, "x", Chain::Evm, "pp").unwrap();
        let err = reveal_solana_keypair(&w, &s, Some("pp")).unwrap_err();
        assert!(err.to_string().contains("not Solana"));
    }

    #[test]
    fn store_round_trip_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");
        let mut s = store();
        add_wallet(&mut s, "a", Chain::Solana, "pp").unwrap();
        add_wallet(&mut s, "b", Chain::Evm, "pp").unwrap();
        save_store(&s, &path).unwrap();
        let loaded = load_store(&path).unwrap();
        let names: Vec<_> = loaded.wallets.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert!(
            !reveal_secret(&loaded.wallets[0], &loaded, Some("pp"))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_save_writes_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let mut s = store();
        add_wallet(&mut s, "x", Chain::Solana, "pp").unwrap();
        save_store(&s, &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn load_store_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_store(&dir.path().join("missing.json"))
                .unwrap()
                .wallets
                .is_empty()
        );
    }

    #[test]
    fn load_store_raises_on_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, "not json").unwrap();
        let err = load_store(&p).unwrap_err();
        assert!(err.to_string().contains("could not read"));
    }

    #[test]
    fn load_store_treats_null_wallets_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, r#"{"wallets": null}"#).unwrap();
        let s = load_store(&p).unwrap();
        assert!(s.wallets.is_empty());
        assert_eq!(s.kdf_iterations, PBKDF2_ITERATIONS);
    }

    #[test]
    fn eip55_checksum_matches_reference_vector() {
        // Vector from EIP-55.
        let raw = [
            0x5a, 0xae, 0xb6, 0x05, 0x3f, 0x3e, 0x94, 0xc9, 0xb9, 0xa0, 0x9f, 0x33, 0x66, 0x94,
            0x35, 0xe7, 0xef, 0x1b, 0xea, 0xed,
        ];
        assert_eq!(
            to_checksum_address(&raw),
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        );
    }

    /// Store written by the Python implementation (`eth_account` / `solders`,
    /// passphrase "pp", 1000 KDF iterations) must decrypt to the same addresses.
    #[test]
    fn decrypts_python_written_store() {
        let s: WalletStore = serde_json::from_str(PYTHON_STORE).unwrap();
        let evm = s.by_name("evm-py").unwrap();
        assert_eq!(
            reveal_evm_account(evm, &s, Some("pp")).unwrap().address(),
            evm.address
        );
        let sol = s.by_name("sol-py").unwrap();
        assert_eq!(
            solana_address(&reveal_solana_keypair(sol, &s, Some("pp")).unwrap()),
            sol.address
        );
    }

    /// Serialization is byte-identical to Python's `json.dumps(indent=2, sort_keys=True)`.
    #[test]
    fn serializes_like_python() {
        let s: WalletStore = serde_json::from_str(PYTHON_STORE).unwrap();
        assert_eq!(serde_json::to_string_pretty(&s).unwrap(), PYTHON_STORE);
    }

    const PYTHON_STORE: &str = include_str!("../tests/fixtures/python-wallet-store.json");
}

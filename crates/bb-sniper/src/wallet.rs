//! Encrypted Solana wallet storage for the sniper.
//!
//! The 64-byte keypair is encrypted at rest with a PBKDF2-SHA256 derived key
//! and an HMAC-SHA256 counter-mode keystream, authenticated by an
//! HMAC-SHA256 MAC over `salt || iv || ciphertext`. The JSON file layout and
//! crypto are byte-compatible with the Python `src.sniper.wallet` files.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac as _};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use solana_keypair::Keypair;
use solana_signer::Signer as _;

use crate::error::SniperError;

pub const PASSPHRASE_ENV: &str = "SNIPER_WALLET_PASSPHRASE";
pub const PBKDF2_ITERATIONS: u32 = 200_000;
const VERSION: u32 = 1;
const SECRET_KEY_LEN: usize = 64;

type HmacSha256 = Hmac<Sha256>;

fn wallet_err(msg: impl Into<String>) -> SniperError {
    SniperError::Wallet(msg.into())
}

/// On-disk representation of an encrypted wallet. Fields are declared in
/// alphabetical order so the pretty JSON matches Python's `sort_keys=True`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletFile {
    pub ciphertext_b64: String,
    pub iv_b64: String,
    pub kdf_iterations: u32,
    pub mac_b64: String,
    pub pubkey: String,
    pub salt_b64: String,
    pub version: u32,
}

impl WalletFile {
    pub fn from_value(payload: &Value) -> Result<Self, SniperError> {
        let text = |key: &str| -> Result<String, SniperError> {
            match payload.get(key) {
                Some(Value::String(s)) => Ok(s.clone()),
                Some(Value::Null) | None => Err(wallet_err(format!(
                    "wallet file is malformed: missing '{key}'"
                ))),
                Some(other) => Ok(other.to_string()),
            }
        };
        let int = |key: &str| -> Result<u32, SniperError> {
            let raw = payload
                .get(key)
                .ok_or_else(|| wallet_err(format!("wallet file is malformed: missing '{key}'")))?;
            match raw {
                Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            }
            .ok_or_else(|| wallet_err(format!("wallet file is malformed: invalid '{key}': {raw}")))
        };
        Ok(Self {
            version: int("version")?,
            pubkey: text("pubkey")?,
            kdf_iterations: int("kdf_iterations")?,
            salt_b64: text("salt_b64")?,
            iv_b64: text("iv_b64")?,
            ciphertext_b64: text("ciphertext_b64")?,
            mac_b64: text("mac_b64")?,
        })
    }
}

fn derive_key(passphrase: &str, salt: &[u8], iterations: u32) -> Result<[u8; 32], SniperError> {
    if passphrase.is_empty() {
        return Err(wallet_err("passphrase must not be empty"));
    }
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, iterations, &mut out);
    Ok(out)
}

/// HMAC-SHA256 keyed by a 32-byte key. HMAC zero-pads keys shorter than the
/// 64-byte block, so padding explicitly makes construction infallible.
fn hmac_sha256(key: &[u8; 32]) -> HmacSha256 {
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(key);
    <HmacSha256 as hmac::digest::KeyInit>::new(&block.into())
}

/// XOR `data` with the keystream `HMAC(key, iv || counter_be64)` blocks.
fn xor_stream(key: &[u8; 32], iv: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for (counter, chunk) in data.chunks(32).enumerate() {
        let mut mac = hmac_sha256(key);
        mac.update(iv);
        mac.update(&(counter as u64).to_be_bytes());
        let block = mac.finalize().into_bytes();
        out.extend(chunk.iter().zip(block.iter()).map(|(a, b)| a ^ b));
    }
    out
}

fn mac_key(derived: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(derived);
    hasher.update(b"\x01mac");
    hasher.finalize().into()
}

fn mac_over(key: &[u8; 32], parts: &[&[u8]]) -> HmacSha256 {
    let mut mac = hmac_sha256(key);
    for part in parts {
        mac.update(part);
    }
    mac
}

fn keypair_from_secret(secret: &[u8]) -> Result<Keypair, SniperError> {
    Keypair::try_from(secret).map_err(|err| wallet_err(format!("invalid keypair bytes: {err}")))
}

/// Encrypt a 64-byte Solana secret key; returns the wallet file and its base58 pubkey.
pub fn encrypt_payload(
    secret_key: &[u8],
    passphrase: &str,
) -> Result<(WalletFile, String), SniperError> {
    if secret_key.len() != SECRET_KEY_LEN {
        return Err(wallet_err(format!(
            "secret_key must be 64 bytes (got {})",
            secret_key.len()
        )));
    }
    let mut rng = rand::rngs::OsRng;
    let mut salt = [0u8; 16];
    let mut iv = [0u8; 12];
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut iv);
    let derived = derive_key(passphrase, &salt, PBKDF2_ITERATIONS)?;
    let ciphertext = xor_stream(&derived, &iv, secret_key);
    let mac = mac_over(&mac_key(&derived), &[&salt, &iv, &ciphertext])
        .finalize()
        .into_bytes();
    let pubkey = keypair_from_secret(secret_key)?.pubkey().to_string();
    let wallet = WalletFile {
        version: VERSION,
        pubkey: pubkey.clone(),
        kdf_iterations: PBKDF2_ITERATIONS,
        salt_b64: B64.encode(salt),
        iv_b64: B64.encode(iv),
        ciphertext_b64: B64.encode(&ciphertext),
        mac_b64: B64.encode(mac),
    };
    Ok((wallet, pubkey))
}

fn b64_field(name: &str, value: &str) -> Result<Vec<u8>, SniperError> {
    B64.decode(value).map_err(|err| {
        wallet_err(format!(
            "wallet file is malformed: {name} is not base64: {err}"
        ))
    })
}

/// Verify the MAC and decrypt the secret key.
pub fn decrypt_payload(wallet: &WalletFile, passphrase: &str) -> Result<Vec<u8>, SniperError> {
    let salt = b64_field("salt_b64", &wallet.salt_b64)?;
    let iv = b64_field("iv_b64", &wallet.iv_b64)?;
    let ciphertext = b64_field("ciphertext_b64", &wallet.ciphertext_b64)?;
    let expected_mac = b64_field("mac_b64", &wallet.mac_b64)?;
    let derived = derive_key(passphrase, &salt, wallet.kdf_iterations)?;
    mac_over(&mac_key(&derived), &[&salt, &iv, &ciphertext])
        .verify_slice(&expected_mac)
        .map_err(|_| wallet_err("wallet decryption failed — wrong passphrase or file tampered"))?;
    Ok(xor_stream(&derived, &iv, &ciphertext))
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Write the wallet with `0o600` permissions via temp file + rename.
pub fn save_wallet(path: &Path, wallet: &WalletFile, overwrite: bool) -> Result<(), SniperError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    if path.exists() && !overwrite {
        return Err(wallet_err(format!(
            "wallet already exists at {}; pass overwrite=True to replace",
            path.display()
        )));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| SniperError::io(parent, e))?;
    }
    let tmp = tmp_sibling(path);
    let body = serde_json::to_vec_pretty(wallet).map_err(|e| wallet_err(e.to_string()))?;
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| SniperError::io(path, e))
}

pub fn load_wallet_file(path: &Path) -> Result<WalletFile, SniperError> {
    if !path.exists() {
        return Err(wallet_err(format!(
            "no wallet at {}; generate one first",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| wallet_err(format!("could not read wallet at {}: {e}", path.display())))?;
    let payload: Value = serde_json::from_str(&text)
        .map_err(|e| wallet_err(format!("could not read wallet at {}: {e}", path.display())))?;
    WalletFile::from_value(&payload)
}

/// Decrypt the wallet with an explicit passphrase.
pub fn load_keypair_with_passphrase(path: &Path, passphrase: &str) -> Result<Keypair, SniperError> {
    let wallet = load_wallet_file(path)?;
    keypair_from_secret(&decrypt_payload(&wallet, passphrase)?)
}

/// Decrypt the wallet. Passphrase precedence: argument, then
/// `SNIPER_WALLET_PASSPHRASE`, then an interactive no-echo TTY prompt.
pub fn load_keypair(path: &Path, passphrase: Option<&str>) -> Result<Keypair, SniperError> {
    let wallet = load_wallet_file(path)?;
    let mut pp = passphrase
        .map(str::to_owned)
        .unwrap_or_else(|| std::env::var(PASSPHRASE_ENV).unwrap_or_default());
    if pp.is_empty() {
        pp = rpassword::prompt_password("Sniper wallet passphrase: ")
            .map_err(|e| wallet_err(format!("could not read passphrase: {e}")))?;
    }
    keypair_from_secret(&decrypt_payload(&wallet, &pp)?)
}

/// Generate a fresh keypair, encrypt + save it, return the public address.
pub fn generate_wallet(
    path: &Path,
    passphrase: &str,
    overwrite: bool,
) -> Result<String, SniperError> {
    let keypair = Keypair::new();
    let (wallet, pubkey) = encrypt_payload(&keypair.to_bytes(), passphrase)?;
    save_wallet(path, &wallet, overwrite)?;
    Ok(pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Vec<u8> {
        Keypair::new().to_bytes().to_vec()
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let s = secret();
        let (wf, pubkey) = encrypt_payload(&s, "correct-horse-battery-staple").unwrap();
        assert_eq!(wf.pubkey, pubkey);
        assert_eq!(
            decrypt_payload(&wf, "correct-horse-battery-staple").unwrap(),
            s
        );
    }

    #[test]
    fn decrypt_with_wrong_passphrase_fails() {
        let (wf, _) = encrypt_payload(&secret(), "right").unwrap();
        let err = decrypt_payload(&wf, "wrong").unwrap_err();
        assert!(err.to_string().contains("wallet decryption failed"));
    }

    #[test]
    fn decrypt_with_tampered_ciphertext_fails() {
        let (mut wf, _) = encrypt_payload(&secret(), "pp").unwrap();
        let first = if wf.ciphertext_b64.starts_with('A') {
            "B"
        } else {
            "A"
        };
        wf.ciphertext_b64.replace_range(0..1, first);
        let err = decrypt_payload(&wf, "pp").unwrap_err();
        assert!(err.to_string().contains("wallet decryption failed"));
    }

    /// Fixed vector produced by the Python implementation (PBKDF2 1000 iters,
    /// salt 0..16, iv 0..12, secret 0..64, passphrase "pp").
    #[test]
    fn decrypts_python_generated_vector() {
        let salt: Vec<u8> = (0..16).collect();
        let iv: Vec<u8> = (0..12).collect();
        let plain: Vec<u8> = (0..64).collect();
        let derived = derive_key("pp", &salt, 1000).unwrap();
        let ct = xor_stream(&derived, &iv, &plain);
        let mac = mac_over(&mac_key(&derived), &[&salt, &iv, &ct])
            .finalize()
            .into_bytes();
        assert_eq!(B64.encode(&ct), PY_CT);
        assert_eq!(B64.encode(mac), PY_MAC);
        let wf = WalletFile {
            version: 1,
            pubkey: String::new(),
            kdf_iterations: 1000,
            salt_b64: B64.encode(&salt),
            iv_b64: B64.encode(&iv),
            ciphertext_b64: PY_CT.into(),
            mac_b64: PY_MAC.into(),
        };
        assert_eq!(decrypt_payload(&wf, "pp").unwrap(), plain);
    }

    const PY_CT: &str =
        "2a8DaFwoECzVKRY6UY5GHpH0Zu0aP4LnFrmdBMiSkVp/Rq21/NroWnmmN70HwyC3TpG7vRUMcU2sNL6+sBqHUw==";
    const PY_MAC: &str = "h2kOhyjv8oti2KqOp51a99fZrW2zbfzIE7QnWdoFNd0=";

    #[test]
    fn save_then_load_round_trip_with_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        let s = secret();
        let (wf, _) = encrypt_payload(&s, "pp").unwrap();
        save_wallet(&path, &wf, false).unwrap();
        let loaded = load_wallet_file(&path).unwrap();
        assert_eq!(loaded, wf);
        assert_eq!(decrypt_payload(&loaded, "pp").unwrap(), s);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n  \"ciphertext_b64\""), "{text}");
    }

    #[test]
    fn save_refuses_to_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        let (wf, _) = encrypt_payload(&secret(), "pp").unwrap();
        save_wallet(&path, &wf, false).unwrap();
        let err = save_wallet(&path, &wf, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        save_wallet(&path, &wf, true).unwrap();
    }

    #[test]
    fn load_wallet_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_wallet_file(&dir.path().join("nope.json")).unwrap_err();
        assert!(err.to_string().contains("no wallet"));

        let corrupt = dir.path().join("c.json");
        std::fs::write(&corrupt, "not valid json").unwrap();
        assert!(
            load_wallet_file(&corrupt)
                .unwrap_err()
                .to_string()
                .contains("could not read")
        );

        let partial = dir.path().join("p.json");
        std::fs::write(&partial, r#"{"version": 1, "pubkey": "X"}"#).unwrap();
        assert!(
            load_wallet_file(&partial)
                .unwrap_err()
                .to_string()
                .contains("malformed")
        );
    }

    #[test]
    fn generate_wallet_creates_loadable_keypair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        let pubkey = generate_wallet(&path, "mypp", false).unwrap();
        let kp = load_keypair(&path, Some("mypp")).unwrap();
        assert_eq!(kp.pubkey().to_string(), pubkey);
        let kp = load_keypair_with_passphrase(&path, "mypp").unwrap();
        assert_eq!(kp.pubkey().to_string(), pubkey);
    }

    #[test]
    fn encrypt_rejects_bad_inputs() {
        assert!(
            encrypt_payload(&secret(), "")
                .unwrap_err()
                .to_string()
                .contains("passphrase")
        );
        assert!(
            encrypt_payload(b"short", "pp")
                .unwrap_err()
                .to_string()
                .contains("64 bytes")
        );
    }
}

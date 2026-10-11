//! API credentials and request signing.
//!
//! Binance accepts HMAC-SHA256 secrets (hex signature) and Ed25519 private keys
//! (base64 signature). A secret containing a PEM block (or a path to a `.pem`
//! file) is treated as an Ed25519 PKCS#8 private key; anything else is an HMAC
//! secret. RSA keys are rejected explicitly.

use std::fmt;

use base64::Engine as _;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use ed25519_dalek::{Signer as _, SigningKey};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{BinanceError, Result};

/// Signature algorithm backing a set of [`Credentials`].
#[derive(Clone)]
pub enum Signer {
    /// HMAC-SHA256 with the API secret; signature is lowercase hex.
    Hmac(String),
    /// Ed25519 private key; signature is standard base64.
    Ed25519(Box<SigningKey>),
}

impl Signer {
    /// Parse a secret: PEM / `.pem` path → Ed25519, otherwise HMAC.
    pub fn from_secret(secret: &str) -> Result<Self> {
        let trimmed = secret.trim();
        if trimmed.is_empty() {
            return Err(BinanceError::Config("api secret must not be empty".into()));
        }
        if trimmed.contains("-----BEGIN") {
            return Self::from_pem(&trimmed.replace("\\n", "\n"));
        }
        if trimmed.ends_with(".pem") {
            let pem = std::fs::read_to_string(trimmed).map_err(|err| {
                BinanceError::Config(format!("cannot read private key file {trimmed}: {err}"))
            })?;
            return Self::from_pem(&pem);
        }
        Ok(Self::Hmac(trimmed.to_owned()))
    }

    fn from_pem(pem: &str) -> Result<Self> {
        if pem.contains("RSA PRIVATE KEY") {
            return Err(BinanceError::Config(
                "RSA API keys are not supported; use an HMAC or Ed25519 key".into(),
            ));
        }
        SigningKey::from_pkcs8_pem(pem)
            .map(|key| Self::Ed25519(Box::new(key)))
            .map_err(|_| {
                BinanceError::Config("private key PEM is not a valid Ed25519 PKCS#8 key".into())
            })
    }

    /// Sign `payload` (the exact query string or WS-API parameter string).
    pub fn sign(&self, payload: &str) -> String {
        match self {
            Self::Hmac(secret) => hmac_sha256_hex(secret, payload),
            Self::Ed25519(key) => base64::engine::general_purpose::STANDARD
                .encode(key.sign(payload.as_bytes()).to_bytes()),
        }
    }

    /// `true` for Ed25519 keys.
    pub fn is_ed25519(&self) -> bool {
        matches!(self, Self::Ed25519(_))
    }
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hmac(_) => f.write_str("Signer::Hmac(***)"),
            Self::Ed25519(_) => f.write_str("Signer::Ed25519(***)"),
        }
    }
}

/// Lowercase-hex HMAC-SHA256 of `payload` keyed by `secret`.
pub fn hmac_sha256_hex(secret: &str, payload: &str) -> String {
    // HMAC accepts keys of any length, so construction cannot fail.
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return String::new(),
    };
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// API key + signer. `Debug` never prints either value.
#[derive(Clone)]
pub struct Credentials {
    api_key: String,
    signer: Signer,
}

impl Credentials {
    /// Build credentials; both values must be non-blank.
    pub fn new(api_key: impl Into<String>, secret: &str) -> Result<Self> {
        let api_key = api_key.into().trim().to_owned();
        if api_key.is_empty() {
            return Err(BinanceError::Config("api key must not be empty".into()));
        }
        Ok(Self {
            api_key,
            signer: Signer::from_secret(secret)?,
        })
    }

    /// Read `BINANCE_API_KEY` (alias `API_KEY`) and `BINANCE_API_SECRET`
    /// (aliases `BINANCE_SECRET_KEY`, `API_SECRET`). `Ok(None)` when either is unset.
    pub fn from_env() -> Result<Option<Self>> {
        let key = bb_core::env::optional_string(&API_KEY_ENV);
        let secret = bb_core::env::optional_string(&API_SECRET_ENV);
        match (key, secret) {
            (Some(key), Some(secret)) => Self::new(key, &secret).map(Some),
            _ => Ok(None),
        }
    }

    /// Like [`Credentials::from_env`] but fails naming the missing variable,
    /// with the Python message (`missing required environment variable: ...`).
    pub fn required_from_env() -> Result<Self> {
        let key = bb_core::env::required(&API_KEY_ENV[..1])
            .map_err(|err| BinanceError::Config(err.to_string()))?;
        let secret = bb_core::env::optional_string(&API_SECRET_ENV[..2]).ok_or_else(|| {
            BinanceError::Config(
                "missing required environment variable: BINANCE_API_SECRET or BINANCE_SECRET_KEY"
                    .into(),
            )
        })?;
        Self::new(key, &secret)
    }

    /// The public API key (sent as `X-MBX-APIKEY`).
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The request signer.
    pub fn signer(&self) -> &Signer {
        &self.signer
    }

    /// Sign `payload` with the configured key.
    pub fn sign(&self, payload: &str) -> String {
        self.signer.sign(payload)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"***")
            .field("signer", &self.signer)
            .finish()
    }
}

/// Env names for the API key, in priority order.
pub const API_KEY_ENV: [&str; 2] = ["BINANCE_API_KEY", "API_KEY"];
/// Env names for the API secret / private key, in priority order.
pub const API_SECRET_ENV: [&str; 3] = ["BINANCE_API_SECRET", "BINANCE_SECRET_KEY", "API_SECRET"];

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier as _;
    use ed25519_dalek::pkcs8::EncodePrivateKey as _;

    #[test]
    fn hmac_matches_binance_documentation_vector() {
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        let payload = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
        assert_eq!(
            Signer::from_secret(secret).map(|s| s.sign(payload)).ok(),
            Some("c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71".to_owned())
        );
    }

    #[test]
    fn ed25519_pem_secret_signs_with_base64() -> Result<()> {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let pem = key
            .to_pkcs8_pem(Default::default())
            .map_err(|e| BinanceError::Config(e.to_string()))?;
        let signer = Signer::from_secret(pem.as_str())?;
        assert!(signer.is_ed25519());
        let sig = signer.sign("apiKey=k&timestamp=1");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(sig)
            .map_err(|e| BinanceError::Config(e.to_string()))?;
        let sig = ed25519_dalek::Signature::from_slice(&raw)
            .map_err(|e| BinanceError::Config(e.to_string()))?;
        assert!(
            key.verifying_key()
                .verify(b"apiKey=k&timestamp=1", &sig)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn rsa_and_blank_secrets_are_rejected_and_debug_redacts() {
        assert!(Signer::from_secret("-----BEGIN RSA PRIVATE KEY-----\nabc").is_err());
        assert!(Credentials::new("key", "  ").is_err());
        let creds = Credentials::new("visible-key", "visible-secret");
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("visible"));
    }
}

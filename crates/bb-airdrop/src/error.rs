use std::path::PathBuf;

/// Every failure in wallet generation, loading/saving state, or RPC polling.
#[derive(Debug, thiserror::Error)]
pub enum AirdropError {
    #[error("chain must be one of ['evm', 'solana']")]
    UnsupportedWalletChain,
    #[error("name must not be empty")]
    EmptyName,
    #[error("wallet name '{0}' already exists in this store")]
    DuplicateName(String),
    #[error("passphrase must not be empty")]
    EmptyPassphrase,
    #[error("could not read passphrase: {0}")]
    Prompt(#[source] std::io::Error),
    #[error("decryption failed for wallet '{0}' — wrong passphrase or tampered file")]
    DecryptionFailed(String),
    #[error("wallet '{name}' is not {expected} (chain={actual})")]
    WrongChain {
        name: String,
        expected: &'static str,
        actual: String,
    },
    #[error("wallet '{name}' holds an invalid {chain} key: {reason}")]
    InvalidKey {
        name: String,
        chain: &'static str,
        reason: String,
    },
    #[error("wallet record '{name}' is malformed: {reason}")]
    MalformedRecord { name: String, reason: String },
    #[error("could not read {what} at {}: {reason}", path.display())]
    Read {
        what: &'static str,
        path: PathBuf,
        reason: String,
    },
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("activity snapshot malformed: {0}")]
    MalformedSnapshot(String),
    #[error("task key '{0}' must contain '::'")]
    InvalidTaskKey(String),
    #[error("RPC error from {url}: {error}")]
    Rpc { url: String, error: String },
    #[error("unexpected {what} response: {payload}")]
    UnexpectedRpcResult { what: &'static str, payload: String },
    #[error("unknown chain: {0}")]
    UnknownChain(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

pub type Result<T, E = AirdropError> = std::result::Result<T, E>;

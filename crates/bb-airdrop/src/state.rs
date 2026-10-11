//! JSON state-file plumbing shared by the wallet store, completion state and
//! activity snapshots, plus the `AIRDROP_DATA_DIR` default-path resolution.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{AirdropError, Result};

pub const DATA_DIR_ENV: &str = "AIRDROP_DATA_DIR";
pub const DEFAULT_DATA_DIR: &str = "~/.local/share/binance-bot/data";
pub const STORE_FILE: &str = "airdrop-wallets.json";
pub const STATE_FILE: &str = "airdrop-scheduler.json";
pub const ACTIVITY_FILE: &str = "airdrop-activity.json";

/// `T::default()` when `path` does not exist; `what` labels read errors.
pub(crate) fn load_json<T: DeserializeOwned + Default>(
    path: &Path,
    what: &'static str,
) -> Result<T> {
    bb_core::fsutil::read_json(path)
        .map(Option::unwrap_or_default)
        .map_err(|e| AirdropError::Read {
            what,
            path: path.to_owned(),
            reason: e.to_string(),
        })
}

/// Atomic pretty-printed JSON write (parent dirs created).
pub(crate) fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    bb_core::fsutil::write_json_atomic(path, value).map_err(|source| AirdropError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Base directory for airdrop state given the raw `AIRDROP_DATA_DIR` value.
///
/// The docker image sets it to `/app/data` (the mounted volume) so state
/// survives container recreates. Unset or blank, state lives under
/// `~/.local/share/binance-bot/data`.
pub fn data_dir_from(raw: Option<&str>) -> PathBuf {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_DATA_DIR);
    expand_user(raw)
}

/// [`data_dir_from`] reading `AIRDROP_DATA_DIR` from the process environment.
pub fn default_data_dir() -> PathBuf {
    data_dir_from(std::env::var(DATA_DIR_ENV).ok().as_deref())
}

pub fn default_store_path() -> PathBuf {
    default_data_dir().join(STORE_FILE)
}

pub fn default_state_path() -> PathBuf {
    default_data_dir().join(STATE_FILE)
}

pub fn default_activity_path() -> PathBuf {
    default_data_dir().join(ACTIVITY_FILE)
}

/// `~` / `~/...` → `$HOME/...` (left untouched when `HOME` is unset).
fn expand_user(raw: &str) -> PathBuf {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    match (raw.strip_prefix('~'), home) {
        (Some(""), Some(home)) => PathBuf::from(home),
        (Some(rest), Some(home)) if rest.starts_with('/') => {
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/share/binance-bot/data")
    }

    #[test]
    fn default_dir_uses_legacy_home_dir_without_env() {
        assert_eq!(data_dir_from(None), legacy());
    }

    #[test]
    fn default_dir_follows_env_value() {
        assert_eq!(data_dir_from(Some("/app/data")), PathBuf::from("/app/data"));
    }

    #[test]
    fn default_dir_ignores_blank_env_value() {
        assert_eq!(data_dir_from(Some("   ")), legacy());
    }

    #[test]
    fn env_value_is_user_expanded() {
        assert_eq!(
            data_dir_from(Some("~/x")),
            PathBuf::from(std::env::var_os("HOME").unwrap()).join("x")
        );
    }
}

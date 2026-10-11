//! The subset of `src/config.py` `Settings` used by the product modules.
//!
//! Field names, env var names, defaults and normalisation match the Python
//! dataclass so existing `.env` files keep working.

use bb_binance::{BinanceClient, BinanceConfig, Credentials, normalize_symbol};
use bb_core::env;

pub const DEFAULT_TRADING_SYMBOL: &str = "BTC/USDT";
pub const DEFAULT_USE_TESTNET: bool = true;
pub const DEFAULT_DRY_RUN: bool = false;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("missing required environment variable: {0}")]
    Missing(String),
    #[error("{0} must not be blank")]
    Blank(&'static str),
    #[error("{0} must be a boolean")]
    Boolean(String),
    #[error("{name} must contain at least one {what}")]
    EmptyList {
        name: &'static str,
        what: &'static str,
    },
    #[error("asset whitelist values must be asset symbols")]
    InvalidAsset,
    #[error("invalid symbol {symbol:?}: {source}")]
    Symbol {
        symbol: String,
        #[source]
        source: bb_binance::BinanceError,
    },
    #[error(transparent)]
    Binance(#[from] bb_binance::BinanceError),
}

/// Environment-backed runtime settings (product-module subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub api_key: String,
    pub api_secret: String,
    pub trading_symbol: String,
    pub trading_symbols: Vec<String>,
    pub use_testnet: bool,
    pub dry_run: bool,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub confirm_live_trading: String,
    pub api_symbol_whitelist: Vec<String>,
    pub api_asset_whitelist: Vec<String>,
}

impl Settings {
    /// Settings with Python defaults for everything except the credentials.
    pub fn new(api_key: &str, api_secret: &str) -> Result<Self, SettingsError> {
        SettingsBuilder::new(api_key, api_secret).build()
    }

    /// Port of `Settings.from_env()` (the caller loads `.env` first).
    pub fn from_env() -> Result<Self, SettingsError> {
        let api_key = env::optional_string(&["BINANCE_API_KEY"])
            .ok_or_else(|| SettingsError::Missing("BINANCE_API_KEY".into()))?;
        let api_secret = env::optional_string(&["BINANCE_API_SECRET", "BINANCE_SECRET_KEY"])
            .ok_or_else(|| {
                SettingsError::Missing("BINANCE_API_SECRET or BINANCE_SECRET_KEY".into())
            })?;
        SettingsBuilder {
            api_key,
            api_secret,
            trading_symbol: env::string(&["TRADING_SYMBOL"], DEFAULT_TRADING_SYMBOL),
            trading_symbols: list_env("TRADING_SYMBOLS", "symbol")?,
            use_testnet: bool_env("USE_TESTNET", DEFAULT_USE_TESTNET)?,
            dry_run: bool_env("DRY_RUN", DEFAULT_DRY_RUN)?,
            telegram_bot_token: env::string(&["TELEGRAM_BOT_TOKEN"], ""),
            telegram_chat_id: env::string(&["TELEGRAM_CHAT_ID"], ""),
            confirm_live_trading: env::string(&["CONFIRM_LIVE_TRADING"], "no"),
            api_symbol_whitelist: list_env("API_SYMBOL_WHITELIST", "symbol")?,
            api_asset_whitelist: list_env("API_ASSET_WHITELIST", "asset")?,
        }
        .build()
    }

    /// Port of `Settings.for_symbol`.
    pub fn for_symbol(&self, symbol: &str) -> Result<Self, SettingsError> {
        let normalized = normalize(symbol)?;
        let mut copy = self.clone();
        copy.trading_symbol.clone_from(&normalized);
        copy.trading_symbols = vec![normalized];
        Ok(copy)
    }

    /// Binance credentials built from the settings key/secret.
    pub fn credentials(&self) -> Result<Credentials, SettingsError> {
        Ok(Credentials::new(self.api_key.clone(), &self.api_secret)?)
    }

    /// Authenticated client for the configured network, mirroring the Python
    /// `BinanceClient(api_key, api_secret, testnet, max_retries)` construction.
    pub fn binance_client(&self, max_retries: u32) -> Result<BinanceClient, SettingsError> {
        let config = BinanceConfig::new(self.use_testnet).with_credentials(self.credentials()?);
        let (base, max) = (config.base_backoff, config.max_backoff);
        Ok(BinanceClient::new(config.with_retries(
            max_retries,
            base,
            max,
        ))?)
    }
}

/// Builder used by `from_env` and by tests to override individual fields.
#[derive(Debug, Clone)]
pub struct SettingsBuilder {
    pub api_key: String,
    pub api_secret: String,
    pub trading_symbol: String,
    pub trading_symbols: Vec<String>,
    pub use_testnet: bool,
    pub dry_run: bool,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub confirm_live_trading: String,
    pub api_symbol_whitelist: Vec<String>,
    pub api_asset_whitelist: Vec<String>,
}

impl SettingsBuilder {
    pub fn new(api_key: &str, api_secret: &str) -> Self {
        Self {
            api_key: api_key.to_owned(),
            api_secret: api_secret.to_owned(),
            trading_symbol: DEFAULT_TRADING_SYMBOL.to_owned(),
            trading_symbols: Vec::new(),
            use_testnet: DEFAULT_USE_TESTNET,
            dry_run: DEFAULT_DRY_RUN,
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            confirm_live_trading: "no".to_owned(),
            api_symbol_whitelist: Vec::new(),
            api_asset_whitelist: Vec::new(),
        }
    }

    /// Normalise and validate (port of `Settings.__post_init__` for these fields).
    pub fn build(self) -> Result<Settings, SettingsError> {
        let api_key = non_blank(&self.api_key, "api_key")?;
        let api_secret = non_blank(&self.api_secret, "api_secret")?;
        let primary = normalize(&non_blank(&self.trading_symbol, "trading_symbol")?)?;
        let trading_symbols = if self.trading_symbols.is_empty() {
            vec![primary]
        } else {
            self.trading_symbols
                .iter()
                .map(|symbol| normalize(&non_blank(symbol, "trading_symbols")?))
                .collect::<Result<Vec<_>, _>>()?
        };
        let trading_symbol = trading_symbols
            .first()
            .cloned()
            .ok_or(SettingsError::EmptyList {
                name: "trading_symbols",
                what: "symbol",
            })?;
        let api_symbol_whitelist = self
            .api_symbol_whitelist
            .iter()
            .map(|symbol| normalize(&non_blank(symbol, "api_symbol_whitelist")?))
            .collect::<Result<Vec<_>, _>>()?;
        let api_asset_whitelist = self
            .api_asset_whitelist
            .iter()
            .map(|asset| asset_symbol(&non_blank(asset, "api_asset_whitelist")?))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Settings {
            api_key,
            api_secret,
            trading_symbol,
            trading_symbols,
            use_testnet: self.use_testnet,
            dry_run: self.dry_run,
            telegram_bot_token: self.telegram_bot_token.trim().to_owned(),
            telegram_chat_id: self.telegram_chat_id.trim().to_owned(),
            confirm_live_trading: self.confirm_live_trading.trim().to_lowercase(),
            api_symbol_whitelist,
            api_asset_whitelist,
        })
    }
}

fn normalize(symbol: &str) -> Result<String, SettingsError> {
    normalize_symbol(symbol).map_err(|source| SettingsError::Symbol {
        symbol: symbol.to_owned(),
        source,
    })
}

fn non_blank(value: &str, field: &'static str) -> Result<String, SettingsError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SettingsError::Blank(field));
    }
    Ok(trimmed.to_owned())
}

/// Port of `_asset_symbol`: upper-case alphanumerics with `-`/`_` allowed.
pub(crate) fn is_asset_symbol(asset: &str) -> bool {
    let stripped: String = asset.chars().filter(|c| *c != '-' && *c != '_').collect();
    !stripped.is_empty() && stripped.chars().all(char::is_alphanumeric)
}

fn asset_symbol(value: &str) -> Result<String, SettingsError> {
    let asset = value.trim().to_uppercase();
    if !is_asset_symbol(&asset) {
        return Err(SettingsError::InvalidAsset);
    }
    Ok(asset)
}

fn bool_env(name: &str, default: bool) -> Result<bool, SettingsError> {
    match env::optional_string(&[name]) {
        None => Ok(default),
        Some(value) => {
            env::parse_bool(&value).ok_or_else(|| SettingsError::Boolean(name.to_owned()))
        }
    }
}

fn list_env(name: &'static str, what: &'static str) -> Result<Vec<String>, SettingsError> {
    let Some(raw) = env::optional_string(&[name]) else {
        return Ok(Vec::new());
    };
    let parts: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return Err(SettingsError::EmptyList { name, what });
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_normalisation() {
        let mut builder = SettingsBuilder::new(" key ", " secret ");
        builder.trading_symbols = vec!["btcusdt".into(), "ETH-USDT".into()];
        builder.api_symbol_whitelist = vec!["bnb/usdt".into()];
        builder.api_asset_whitelist = vec![" bnb ".into()];
        builder.confirm_live_trading = " YES ".into();
        let settings = builder.build().unwrap();
        assert_eq!(settings.api_key, "key");
        assert_eq!(settings.trading_symbol, "BTC/USDT");
        assert_eq!(settings.trading_symbols, vec!["BTC/USDT", "ETH/USDT"]);
        assert_eq!(settings.api_symbol_whitelist, vec!["BNB/USDT"]);
        assert_eq!(settings.api_asset_whitelist, vec!["BNB"]);
        assert_eq!(settings.confirm_live_trading, "yes");
        assert!(settings.use_testnet);
        assert!(!settings.dry_run);
    }

    #[test]
    fn rejects_blank_and_invalid_assets() {
        assert!(matches!(
            Settings::new(" ", "s"),
            Err(SettingsError::Blank("api_key"))
        ));
        let mut builder = SettingsBuilder::new("k", "s");
        builder.api_asset_whitelist = vec!["BN B".into()];
        assert!(matches!(builder.build(), Err(SettingsError::InvalidAsset)));
    }

    #[test]
    fn for_symbol_scopes_copy() {
        let settings = Settings::new("k", "s")
            .unwrap()
            .for_symbol("ethusdt")
            .unwrap();
        assert_eq!(settings.trading_symbol, "ETH/USDT");
        assert_eq!(settings.trading_symbols, vec!["ETH/USDT"]);
    }
}

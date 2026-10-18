//! Environment configuration (same variable names and defaults as the refactor `src/config.py`).

use std::path::PathBuf;
use std::time::Duration;

use bb_core::env;
use rust_decimal::Decimal;

use crate::error::{CapitalError, Result};

/// `CAPITAL_RESERVE_PCT` default.
pub const DEFAULT_RESERVE_PCT: Decimal = Decimal::from_parts(40, 0, 0, false, 0);
/// `CAPITAL_MAX_MODULE_ALLOCATION_USD` default.
pub const DEFAULT_MAX_MODULE_ALLOCATION_USD: Decimal = Decimal::from_parts(25, 0, 0, false, 0);
/// `CAPITAL_AI_ADVISOR_TIMEOUT_SECONDS` default.
pub const DEFAULT_ADVISOR_TIMEOUT_SECONDS: f64 = 8.0;
/// `BOT_ALLOCATOR_TIMEOUT_SECONDS` default.
pub const DEFAULT_BOT_TIMEOUT_SECONDS: f64 = 45.0;
/// `CAPITAL_POOL_STATE_FILE` default.
pub const DEFAULT_STATE_FILE: &str = "data/capital-pool-state.json";
/// `BOT_ALLOCATOR_SESSION_DIR` default.
pub const DEFAULT_SESSION_DIR: &str = "data/bot-allocator-session";
/// `AUTO_CONVERT_AUDIT_PATH` default.
pub const DEFAULT_CONVERT_AUDIT_PATH: &str = "data/auto-convert-audit.jsonl";

fn positive_duration(seconds: f64, name: &str) -> Result<Duration> {
    if seconds.is_finite() && seconds > 0.0 {
        Ok(Duration::from_secs_f64(seconds.max(0.1)))
    } else {
        Err(CapitalError::invalid(format!(
            "{name} must be greater than zero"
        )))
    }
}

fn non_negative(value: Decimal, name: &str) -> Result<Decimal> {
    if value < Decimal::ZERO {
        Err(CapitalError::invalid(format!(
            "{name} must be greater than or equal to zero"
        )))
    } else {
        Ok(value)
    }
}

/// Shared capital pool limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapitalPoolConfig {
    /// `CAPITAL_POOL_ENABLED` (false): when off every lease is `bypassed`.
    pub enabled: bool,
    /// `CAPITAL_RESERVE_PCT` (40): equity share never allocated (clamped to 0..=100).
    pub reserve_pct: Decimal,
    /// `CAPITAL_MIN_MODULE_ALLOCATION_USD` (0): smaller allocations pause the module.
    pub min_module_allocation_usdt: Decimal,
    /// `CAPITAL_MAX_MODULE_ALLOCATION_USD` (25): per-module USDT ceiling.
    pub max_module_allocation_usdt: Option<Decimal>,
}

impl Default for CapitalPoolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reserve_pct: DEFAULT_RESERVE_PCT,
            min_module_allocation_usdt: Decimal::ZERO,
            max_module_allocation_usdt: Some(DEFAULT_MAX_MODULE_ALLOCATION_USD),
        }
    }
}

impl CapitalPoolConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            enabled: env::boolean(&["CAPITAL_POOL_ENABLED"], false)?,
            reserve_pct: env::decimal(&["CAPITAL_RESERVE_PCT"], DEFAULT_RESERVE_PCT)?,
            min_module_allocation_usdt: env::decimal(
                &["CAPITAL_MIN_MODULE_ALLOCATION_USD"],
                Decimal::ZERO,
            )?,
            max_module_allocation_usdt: Some(env::decimal(
                &["CAPITAL_MAX_MODULE_ALLOCATION_USD"],
                DEFAULT_MAX_MODULE_ALLOCATION_USD,
            )?),
        };
        config.validate()?;
        Ok(config)
    }

    /// Range checks the refactor performed lazily at plan time.
    pub fn validate(&self) -> Result<()> {
        non_negative(self.reserve_pct, "reserve_pct")?;
        non_negative(
            self.min_module_allocation_usdt,
            "min_module_allocation_usdt",
        )?;
        if self
            .max_module_allocation_usdt
            .is_some_and(|max| max <= Decimal::ZERO)
        {
            return Err(CapitalError::invalid(
                "max_module_allocation_usdt must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// Guarded AI advisor switches (`CAPITAL_AI_ADVISOR_*`).
#[derive(Debug, Clone, PartialEq)]
pub struct AdvisorConfig {
    /// `CAPITAL_AI_ADVISOR_ENABLED` (false).
    pub enabled: bool,
    /// `CAPITAL_AI_ADVISOR_FILE`: proposal JSON read before asking the bot.
    pub proposal_file: Option<PathBuf>,
    /// `CAPITAL_AI_ADVISOR_TIMEOUT_SECONDS` (8): budget for one advisory call from the pool.
    pub timeout: Duration,
}

impl AdvisorConfig {
    pub fn from_env() -> Result<Self> {
        if let Some(cmd) = env::optional_string(&["CAPITAL_AI_ADVISOR_CMD"]) {
            tracing::warn!(
                %cmd,
                "CAPITAL_AI_ADVISOR_CMD is ignored: the bot allocator now runs through omp (BOT_ALLOCATOR_*)"
            );
        }
        Ok(Self {
            enabled: env::boolean(&["CAPITAL_AI_ADVISOR_ENABLED"], false)?,
            proposal_file: env::optional_string(&["CAPITAL_AI_ADVISOR_FILE"]).map(PathBuf::from),
            timeout: positive_duration(
                env::float(
                    &["CAPITAL_AI_ADVISOR_TIMEOUT_SECONDS"],
                    DEFAULT_ADVISOR_TIMEOUT_SECONDS,
                )?,
                "CAPITAL_AI_ADVISOR_TIMEOUT_SECONDS",
            )?,
        })
    }
}

/// The omp-backed bot allocator (`BOT_ALLOCATOR_*`).
#[derive(Debug, Clone, PartialEq)]
pub struct BotAllocatorConfig {
    /// `BOT_ALLOCATOR_OMP_COMMAND` (`omp`).
    pub command: String,
    /// `BOT_ALLOCATOR_MODEL` (`sonnet`).
    pub model: String,
    /// `BOT_ALLOCATOR_TIMEOUT_SECONDS` (45): one bot turn.
    pub timeout: Duration,
    /// `BOT_ALLOCATOR_SESSION_DIR` (`data/bot-allocator-session`): persistent context.
    pub session_dir: PathBuf,
    /// Compact the persistent context once it passes this share of the window.
    pub compact_at_percent: f64,
}

impl BotAllocatorConfig {
    pub fn from_env() -> Result<Self> {
        if env::optional_string(&["BOT_ALLOCATOR_MAX_BUDGET_USD"]).is_some() {
            tracing::warn!(
                "BOT_ALLOCATOR_MAX_BUDGET_USD is ignored by the omp allocator session"
            );
        }
        Ok(Self {
            command: env::string(&["BOT_ALLOCATOR_OMP_COMMAND"], "omp"),
            model: env::string(&["BOT_ALLOCATOR_MODEL"], "sonnet"),
            timeout: positive_duration(
                env::float(
                    &["BOT_ALLOCATOR_TIMEOUT_SECONDS"],
                    DEFAULT_BOT_TIMEOUT_SECONDS,
                )?,
                "BOT_ALLOCATOR_TIMEOUT_SECONDS",
            )?,
            session_dir: PathBuf::from(env::string(
                &["BOT_ALLOCATOR_SESSION_DIR"],
                DEFAULT_SESSION_DIR,
            )),
            compact_at_percent: 70.0,
        })
    }
}

/// Quote funding conversion limits (`AUTO_CONVERT_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoConvertConfig {
    /// `AUTO_CONVERT_TO_QUOTE` (false).
    pub enabled: bool,
    /// Quote asset funded by conversions (USDT).
    pub quote_asset: String,
    /// `AUTO_CONVERT_ASSETS` (`BNB`): assets that may be sold; empty allows every asset.
    pub allowed_assets: Vec<String>,
    /// `AUTO_CONVERT_MAX_QUOTE_USD` (100).
    pub max_quote_value: Decimal,
    /// `AUTO_CONVERT_MIN_QUOTE_USD` (5).
    pub min_quote_value: Decimal,
    /// `AUTO_CONVERT_BUFFER_PCT` (1).
    pub buffer_pct: Decimal,
}

impl Default for AutoConvertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quote_asset: "USDT".into(),
            allowed_assets: vec!["BNB".into()],
            max_quote_value: Decimal::ONE_HUNDRED,
            min_quote_value: Decimal::from(5),
            buffer_pct: Decimal::ONE,
        }
    }
}

impl AutoConvertConfig {
    pub fn from_env() -> Result<Self> {
        let defaults = Self::default();
        let allowed_assets = match env::raw(&["AUTO_CONVERT_ASSETS"]) {
            None => defaults.allowed_assets,
            Some((_, value)) => parse_assets(&value)?,
        };
        let config = Self {
            enabled: env::boolean(&["AUTO_CONVERT_TO_QUOTE"], false)?,
            quote_asset: defaults.quote_asset,
            allowed_assets,
            max_quote_value: env::decimal(
                &["AUTO_CONVERT_MAX_QUOTE_USD"],
                defaults.max_quote_value,
            )?,
            min_quote_value: env::decimal(
                &["AUTO_CONVERT_MIN_QUOTE_USD"],
                defaults.min_quote_value,
            )?,
            buffer_pct: env::decimal(&["AUTO_CONVERT_BUFFER_PCT"], defaults.buffer_pct)?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_quote_value <= Decimal::ZERO {
            return Err(CapitalError::invalid(
                "max_quote_value must be greater than zero",
            ));
        }
        non_negative(self.min_quote_value, "min_quote_value")?;
        non_negative(self.buffer_pct, "buffer_pct")?;
        if self.quote_asset.trim().is_empty() {
            return Err(CapitalError::invalid("quote asset must not be blank"));
        }
        Ok(())
    }
}

/// `AUTO_CONVERT_ASSETS` syntax: comma or semicolon separated, upper-cased, at least one.
pub fn parse_assets(value: &str) -> Result<Vec<String>> {
    let assets: Vec<String> = value
        .replace(';', ",")
        .split(',')
        .map(str::trim)
        .filter(|asset| !asset.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    if assets.is_empty() {
        return Err(CapitalError::invalid(
            "AUTO_CONVERT_ASSETS must include at least one asset",
        ));
    }
    Ok(assets)
}

/// `CAPITAL_POOL_STATE_FILE` (`data/capital-pool-state.json`).
pub fn state_file() -> PathBuf {
    PathBuf::from(env::string(
        &["CAPITAL_POOL_STATE_FILE"],
        DEFAULT_STATE_FILE,
    ))
}

/// `AUTO_CONVERT_AUDIT_PATH` (`data/auto-convert-audit.jsonl`).
pub fn convert_audit_path() -> PathBuf {
    PathBuf::from(env::string(
        &["AUTO_CONVERT_AUDIT_PATH"],
        DEFAULT_CONVERT_AUDIT_PATH,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_lists_accept_commas_and_semicolons() {
        assert_eq!(parse_assets(" bnb; eth ,").unwrap(), vec!["BNB", "ETH"]);
        assert!(parse_assets(" , ;").is_err());
    }

    #[test]
    fn defaults_match_the_refactor_settings() {
        let pool = CapitalPoolConfig::default();
        assert_eq!(pool.reserve_pct, Decimal::from(40));
        assert_eq!(pool.max_module_allocation_usdt, Some(Decimal::from(25)));
        let convert = AutoConvertConfig::default();
        assert_eq!(convert.allowed_assets, vec!["BNB"]);
        assert_eq!(convert.max_quote_value, Decimal::from(100));
        assert_eq!(convert.min_quote_value, Decimal::from(5));
        assert_eq!(convert.buffer_pct, Decimal::ONE);
        assert!(
            CapitalPoolConfig {
                max_module_allocation_usdt: Some(Decimal::ZERO),
                ..CapitalPoolConfig::default()
            }
            .validate()
            .is_err()
        );
    }
}

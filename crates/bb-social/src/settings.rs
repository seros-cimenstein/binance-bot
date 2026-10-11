//! `SOCIAL_SIGNAL_*` configuration.

use std::path::PathBuf;
use std::time::Duration;

use bb_core::env;

use crate::SocialSignalError;

pub const MODE_READ_ONLY: &str = "read_only";
pub const DEFAULT_AUDIT_PATH: &str = "data/social-signals.jsonl";

/// Configuration for optional read-only social evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct SocialSettings {
    pub enabled: bool,
    pub telegram_enabled: bool,
    pub x_enabled: bool,
    pub audit_path: PathBuf,
    pub max_age_minutes: u32,
    pub min_source_diversity: usize,
    pub block_manipulation_risk: bool,
    /// Manipulation-risk score in `[0, 1]` at or above which a route is blocked.
    pub manipulation_block_score: f64,
    /// Newest records kept per asset; `0` keeps every recent record.
    pub max_records_per_asset: usize,
    pub telegram_bot_token: String,
    pub x_bearer_token: String,
    pub x_query: String,
    pub ai_enabled: bool,
    pub ai_command: String,
    pub ai_model: String,
    pub ai_timeout: Duration,
}

impl Default for SocialSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            telegram_enabled: false,
            x_enabled: false,
            audit_path: PathBuf::from(DEFAULT_AUDIT_PATH),
            max_age_minutes: 180,
            min_source_diversity: 2,
            block_manipulation_risk: true,
            manipulation_block_score: 0.8,
            max_records_per_asset: 12,
            telegram_bot_token: String::new(),
            x_bearer_token: String::new(),
            x_query: String::new(),
            ai_enabled: false,
            ai_command: "omp".to_owned(),
            ai_model: "sonnet".to_owned(),
            ai_timeout: Duration::from_secs(120),
        }
    }
}

impl SocialSettings {
    pub fn from_env() -> Result<Self, SocialSignalError> {
        let defaults = Self::default();
        let mode = env::string(&["SOCIAL_SIGNAL_MODE"], MODE_READ_ONLY);
        if !mode.eq_ignore_ascii_case(MODE_READ_ONLY) {
            return Err(SocialSignalError::InvalidSettings(
                "social signals support read_only mode only".to_owned(),
            ));
        }
        let ai_timeout_seconds = env::float(&["SOCIAL_SIGNAL_AI_TIMEOUT_SECONDS"], 120.0)?;
        if ai_timeout_seconds <= 0.0 {
            return Err(SocialSignalError::InvalidSettings(
                "ai_timeout_seconds must be greater than zero".to_owned(),
            ));
        }
        let settings = Self {
            enabled: env::boolean(&["SOCIAL_SIGNAL_ENABLED"], defaults.enabled)?,
            telegram_enabled: env::boolean(
                &["SOCIAL_SIGNAL_TELEGRAM_ENABLED"],
                defaults.telegram_enabled,
            )?,
            x_enabled: env::boolean(&["SOCIAL_SIGNAL_X_ENABLED"], defaults.x_enabled)?,
            audit_path: PathBuf::from(env::string(
                &["SOCIAL_SIGNAL_AUDIT_PATH"],
                DEFAULT_AUDIT_PATH,
            )),
            max_age_minutes: env::integer(
                &["SOCIAL_SIGNAL_MAX_AGE_MINUTES"],
                defaults.max_age_minutes,
            )?,
            min_source_diversity: env::integer(
                &["SOCIAL_SIGNAL_MIN_SOURCE_DIVERSITY"],
                defaults.min_source_diversity,
            )?,
            block_manipulation_risk: env::boolean(
                &["SOCIAL_SIGNAL_BLOCK_MANIPULATION_RISK"],
                defaults.block_manipulation_risk,
            )?,
            manipulation_block_score: env::float(
                &["SOCIAL_SIGNAL_MANIPULATION_BLOCK_SCORE"],
                defaults.manipulation_block_score,
            )?,
            max_records_per_asset: env::integer(
                &["SOCIAL_SIGNAL_MAX_RECORDS_PER_ASSET"],
                defaults.max_records_per_asset,
            )?,
            telegram_bot_token: env::string(&["SOCIAL_SIGNAL_TELEGRAM_BOT_TOKEN"], ""),
            x_bearer_token: env::string(&["SOCIAL_SIGNAL_X_BEARER_TOKEN"], ""),
            x_query: env::string(&["SOCIAL_SIGNAL_X_QUERY"], ""),
            ai_enabled: env::boolean(&["SOCIAL_SIGNAL_AI_ENABLED"], defaults.ai_enabled)?,
            ai_command: env::string(&["SOCIAL_SIGNAL_AI_COMMAND"], &defaults.ai_command),
            ai_model: env::string(&["SOCIAL_SIGNAL_AI_MODEL"], &defaults.ai_model),
            ai_timeout: Duration::try_from_secs_f64(ai_timeout_seconds).map_err(|_| {
                SocialSignalError::InvalidSettings(
                    "ai_timeout_seconds must be greater than zero".to_owned(),
                )
            })?,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Invariants the typed fields cannot express on their own.
    pub fn validate(&self) -> Result<(), SocialSignalError> {
        if self.audit_path.as_os_str().is_empty() {
            return Err(SocialSignalError::InvalidSettings(
                "social signal audit_path must not be blank".to_owned(),
            ));
        }
        if !(0.0..=1.0).contains(&self.manipulation_block_score) {
            return Err(SocialSignalError::InvalidSettings(
                "manipulation_block_score must be between 0 and 1".to_owned(),
            ));
        }
        if self.ai_timeout.is_zero() {
            return Err(SocialSignalError::InvalidSettings(
                "ai_timeout_seconds must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_out_of_range_block_score() {
        let settings = SocialSettings {
            manipulation_block_score: 1.5,
            ..SocialSettings::default()
        };
        assert!(settings.validate().is_err());
        assert!(SocialSettings::default().validate().is_ok());
    }
}

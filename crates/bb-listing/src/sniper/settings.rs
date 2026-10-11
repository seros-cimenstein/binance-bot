//! Sniper configuration (`LISTING_SNIPER_*`).

use std::path::{Path, PathBuf};

use rust_decimal::Decimal;
use serde::Serialize;

use crate::error::{ListingError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldMode {
    /// Sell at the first poll after `hold_seconds`.
    Flat,
    /// Sell once price retraces `trailing_stop_pct` from the running peak.
    Trailing,
    /// Velocity / no-new-high / MA-cross exit (see [`super::exit::momentum_should_exit`]).
    Momentum,
}

impl HoldMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "flat" => Ok(Self::Flat),
            "trailing" => Ok(Self::Trailing),
            "momentum" => Ok(Self::Momentum),
            _ => Err(ListingError::InvalidSettings(
                "hold_mode must be 'flat', 'trailing', or 'momentum'".into(),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Trailing => "trailing",
            Self::Momentum => "momentum",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SniperSettings {
    pub use_testnet: bool,
    pub dry_run: bool,
    pub enabled: bool,
    pub confirm_live: String,
    /// `LISTING_SNIPER_PROVEN_EDGE=yes`.
    pub proven_edge: bool,

    pub quote_asset: String,
    /// exchangeInfo poll cadence (daemon).
    pub poll_interval_seconds: f64,
    pub notional_usdt: Decimal,

    pub hold_mode: HoldMode,
    /// Flat hold, or the noise-filter floor for trailing/momentum.
    pub hold_seconds: u64,
    pub trailing_stop_pct: Decimal,
    pub max_hold_seconds: u64,
    pub trailing_poll_interval_seconds: u64,
    /// Universal stop from tick 1 in every mode; `>= 0` disables it.
    pub hard_stop_loss_pct: Decimal,

    pub momentum_negative_polls_to_exit: usize,
    pub momentum_no_new_high_polls_to_exit: usize,
    pub momentum_ma_short_samples: usize,
    pub momentum_ma_long_samples: usize,
    pub momentum_ma_cross_min_pullback_pct: Decimal,

    pub daily_spend_cap_usdt: Decimal,
    /// Live: free quote balance below this skips the fire.
    pub min_balance_usdt: Decimal,
    /// Live: query the free quote balance before each fire.
    pub dynamic_balance: bool,

    pub state_path: PathBuf,
    pub audit_path: PathBuf,
    pub ledger_path: PathBuf,
    pub emergency_stop_path: PathBuf,
    /// Shared watchlist written by the detector.
    pub watchlist_path: PathBuf,
}

impl Default for SniperSettings {
    fn default() -> Self {
        Self {
            use_testnet: false,
            dry_run: true,
            enabled: false,
            confirm_live: "no".into(),
            proven_edge: false,
            quote_asset: "USDT".into(),
            poll_interval_seconds: 5.0,
            notional_usdt: Decimal::from(5),
            hold_mode: HoldMode::Trailing,
            hold_seconds: 60,
            trailing_stop_pct: Decimal::from(15),
            max_hold_seconds: 14_400,
            trailing_poll_interval_seconds: 10,
            hard_stop_loss_pct: Decimal::from(-25),
            momentum_negative_polls_to_exit: 2,
            momentum_no_new_high_polls_to_exit: 5,
            momentum_ma_short_samples: 3,
            momentum_ma_long_samples: 6,
            momentum_ma_cross_min_pullback_pct: Decimal::from(5),
            daily_spend_cap_usdt: Decimal::from(50),
            min_balance_usdt: Decimal::from(10),
            dynamic_balance: true,
            state_path: "data/listing-sniper-state.json".into(),
            audit_path: "data/listing-sniper-events.jsonl".into(),
            ledger_path: "data/listing-sniper-ledger.jsonl".into(),
            emergency_stop_path: "data/listing-sniper.stop".into(),
            watchlist_path: "data/listing-watchlist.json".into(),
        }
    }
}

impl SniperSettings {
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: &str| Err(ListingError::InvalidSettings(msg.to_owned()));
        if self.poll_interval_seconds.is_nan() || self.poll_interval_seconds < 1.0 {
            return invalid("poll_interval_seconds must be >= 1.0");
        }
        if self.notional_usdt <= Decimal::ZERO {
            return invalid("notional_usdt must be positive");
        }
        if self.momentum_ma_short_samples >= self.momentum_ma_long_samples {
            return invalid(
                "momentum_ma_short_samples must be strictly less than momentum_ma_long_samples",
            );
        }
        if self.daily_spend_cap_usdt <= Decimal::ZERO {
            return invalid("daily_spend_cap_usdt must be positive");
        }
        if self.min_balance_usdt < Decimal::ZERO {
            return invalid("min_balance_usdt must be non-negative");
        }
        let paths: [(&str, &Path); 4] = [
            ("state_path", &self.state_path),
            ("audit_path", &self.audit_path),
            ("ledger_path", &self.ledger_path),
            ("emergency_stop_path", &self.emergency_stop_path),
        ];
        for (name, path) in paths {
            if path.to_string_lossy().trim().is_empty() {
                return invalid(&format!("{name} must not be blank"));
            }
        }
        Ok(())
    }

    /// Read `LISTING_SNIPER_*` (plus `USE_TESTNET`, `DRY_RUN`, `LISTING_WATCHLIST_PATH`)
    /// and validate. Binance credentials are required in every mode, as before.
    pub fn from_env() -> Result<Self> {
        use bb_core::env::{boolean, confirmed, decimal, float, integer, string};
        if bb_binance::Credentials::from_env().ok().flatten().is_none() {
            return Err(ListingError::InvalidSettings(
                "BINANCE_API_KEY and BINANCE_API_SECRET must be set".into(),
            ));
        }
        let d = Self::default();
        let path =
            |name: &str, default: &Path| PathBuf::from(string(&[name], &default.to_string_lossy()));
        let settings = Self {
            use_testnet: boolean(&["USE_TESTNET"], false)?,
            dry_run: boolean(&["DRY_RUN"], true)?,
            enabled: boolean(&["LISTING_SNIPER_ENABLED"], false)?,
            confirm_live: string(&["CONFIRM_LIVE_LISTING_SNIPER"], "no"),
            proven_edge: confirmed(&["LISTING_SNIPER_PROVEN_EDGE"]),
            quote_asset: string(&["LISTING_SNIPER_QUOTE_ASSET"], &d.quote_asset),
            poll_interval_seconds: float(
                &["LISTING_SNIPER_POLL_INTERVAL_SECONDS"],
                d.poll_interval_seconds,
            )?,
            notional_usdt: decimal(&["LISTING_SNIPER_NOTIONAL_USDT"], d.notional_usdt)?,
            hold_mode: HoldMode::parse(&string(
                &["LISTING_SNIPER_HOLD_MODE"],
                d.hold_mode.as_str(),
            ))?,
            hold_seconds: integer(&["LISTING_SNIPER_HOLD_SECONDS"], d.hold_seconds)?,
            trailing_stop_pct: decimal(&["LISTING_SNIPER_TRAILING_STOP_PCT"], d.trailing_stop_pct)?,
            max_hold_seconds: integer(&["LISTING_SNIPER_MAX_HOLD_SECONDS"], d.max_hold_seconds)?,
            trailing_poll_interval_seconds: integer(
                &["LISTING_SNIPER_TRAILING_POLL_INTERVAL_SECONDS"],
                d.trailing_poll_interval_seconds,
            )?,
            hard_stop_loss_pct: decimal(
                &["LISTING_SNIPER_HARD_STOP_LOSS_PCT"],
                d.hard_stop_loss_pct,
            )?,
            momentum_negative_polls_to_exit: integer(
                &["LISTING_SNIPER_MOMENTUM_NEGATIVE_POLLS_TO_EXIT"],
                d.momentum_negative_polls_to_exit,
            )?,
            momentum_no_new_high_polls_to_exit: integer(
                &["LISTING_SNIPER_MOMENTUM_NO_NEW_HIGH_POLLS_TO_EXIT"],
                d.momentum_no_new_high_polls_to_exit,
            )?,
            momentum_ma_short_samples: integer(
                &["LISTING_SNIPER_MOMENTUM_MA_SHORT_SAMPLES"],
                d.momentum_ma_short_samples,
            )?,
            momentum_ma_long_samples: integer(
                &["LISTING_SNIPER_MOMENTUM_MA_LONG_SAMPLES"],
                d.momentum_ma_long_samples,
            )?,
            momentum_ma_cross_min_pullback_pct: decimal(
                &["LISTING_SNIPER_MOMENTUM_MA_CROSS_MIN_PULLBACK_PCT"],
                d.momentum_ma_cross_min_pullback_pct,
            )?,
            daily_spend_cap_usdt: decimal(
                &["LISTING_SNIPER_DAILY_SPEND_CAP_USDT"],
                d.daily_spend_cap_usdt,
            )?,
            min_balance_usdt: decimal(&["LISTING_SNIPER_MIN_BALANCE_USDT"], d.min_balance_usdt)?,
            dynamic_balance: boolean(&["LISTING_SNIPER_DYNAMIC_BALANCE"], d.dynamic_balance)?,
            state_path: path("LISTING_SNIPER_STATE_PATH", &d.state_path),
            audit_path: path("LISTING_SNIPER_AUDIT_PATH", &d.audit_path),
            ledger_path: path("LISTING_SNIPER_LEDGER_PATH", &d.ledger_path),
            emergency_stop_path: path("LISTING_SNIPER_EMERGENCY_STOP_PATH", &d.emergency_stop_path),
            watchlist_path: path("LISTING_WATCHLIST_PATH", &d.watchlist_path),
        };
        settings.validate()?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn rejects(s: SniperSettings, needle: &str) {
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains(needle), "{err}");
    }

    #[test]
    fn validation() {
        let ok = SniperSettings::default();
        ok.validate().unwrap();
        rejects(
            SniperSettings {
                poll_interval_seconds: 0.5,
                ..ok.clone()
            },
            "poll_interval",
        );
        rejects(
            SniperSettings {
                poll_interval_seconds: f64::NAN,
                ..ok.clone()
            },
            "poll_interval",
        );
        rejects(
            SniperSettings {
                notional_usdt: dec!(0),
                ..ok.clone()
            },
            "notional_usdt",
        );
        rejects(
            SniperSettings {
                daily_spend_cap_usdt: dec!(0),
                ..ok.clone()
            },
            "daily_spend_cap",
        );
        rejects(
            SniperSettings {
                min_balance_usdt: dec!(-1),
                ..ok.clone()
            },
            "min_balance_usdt",
        );
        rejects(
            SniperSettings {
                momentum_ma_short_samples: 6,
                momentum_ma_long_samples: 6,
                ..ok.clone()
            },
            "momentum_ma_short_samples",
        );
        rejects(
            SniperSettings {
                ledger_path: " ".into(),
                ..ok
            },
            "ledger_path must not be blank",
        );
    }

    #[test]
    fn hold_mode_parsing() {
        assert_eq!(HoldMode::parse("momentum").unwrap(), HoldMode::Momentum);
        let err = HoldMode::parse("bogus").unwrap_err().to_string();
        assert!(err.contains("hold_mode"), "{err}");
    }
}

//! Environment configuration for the modular paper asset-pool runtime.
//!
//! Pool evaluation intentionally has no live-execution switch. A future live
//! allocator requires a separate approved design and entrypoint.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{PoolError, Result};

/// Scorer names accepted in `POOL_SCORERS`.
pub const SUPPORTED_SCORERS: [&str; 2] = ["simple_momentum", "risk_adjusted_momentum"];

/// Field names accepted in a replay fixture `settings` block (the Python dataclass fields,
/// including the credential fields that replays override).
pub const SETTINGS_FIELDS: [&str; 26] = [
    "api_key",
    "api_secret",
    "use_testnet",
    "mode",
    "db_path",
    "benchmarks",
    "benchmark_weights",
    "max_route_legs",
    "max_route_expansions",
    "max_candidate_routes",
    "max_scoring_assets",
    "min_quote_volume_usdt",
    "max_spread_bps",
    "slippage_buffer_bps",
    "ticker_stale_seconds",
    "fee_cache_seconds",
    "bnb_fee_buffer_multiplier",
    "min_bnb_fee_reserve",
    "scorers",
    "momentum_observations",
    "cycle_seconds",
    "max_allocation_percent",
    "max_reallocation_percent",
    "min_improvement_bps",
    "max_drawdown_percent",
    "decision_cooldown_seconds",
];

/// Validated paper-only pool settings (`POOL_*` environment variables).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSettings {
    pub use_testnet: bool,
    /// Always `paper`.
    pub mode: String,
    pub db_path: String,
    pub benchmarks: Vec<String>,
    pub benchmark_weights: Vec<f64>,
    pub max_route_legs: usize,
    pub max_route_expansions: usize,
    pub max_candidate_routes: usize,
    /// `0` = unbounded.
    pub max_scoring_assets: usize,
    pub min_quote_volume_usdt: Decimal,
    pub max_spread_bps: Decimal,
    pub slippage_buffer_bps: Decimal,
    /// `0` disables the staleness gate.
    pub ticker_stale_seconds: f64,
    pub fee_cache_seconds: f64,
    pub bnb_fee_buffer_multiplier: Decimal,
    pub min_bnb_fee_reserve: Decimal,
    pub scorers: Vec<String>,
    pub momentum_observations: usize,
    pub cycle_seconds: f64,
    pub max_allocation_percent: Decimal,
    pub max_reallocation_percent: Decimal,
    pub min_improvement_bps: f64,
    pub max_drawdown_percent: f64,
    pub decision_cooldown_seconds: f64,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            use_testnet: false,
            mode: "paper".into(),
            db_path: "data/pool-paper.db".into(),
            benchmarks: ["USDT", "BTC", "PAXG", "XAUT"].map(String::from).to_vec(),
            benchmark_weights: vec![0.25; 4],
            max_route_legs: 3,
            max_route_expansions: 10_000,
            max_candidate_routes: 8,
            max_scoring_assets: 0,
            min_quote_volume_usdt: Decimal::from(100_000),
            max_spread_bps: Decimal::from(50),
            slippage_buffer_bps: Decimal::from(25),
            ticker_stale_seconds: 300.0,
            fee_cache_seconds: 60.0,
            bnb_fee_buffer_multiplier: Decimal::from(3),
            min_bnb_fee_reserve: Decimal::new(5, 5),
            scorers: SUPPORTED_SCORERS.map(String::from).to_vec(),
            momentum_observations: 96,
            cycle_seconds: 900.0,
            max_allocation_percent: Decimal::from(25),
            max_reallocation_percent: Decimal::from(10),
            min_improvement_bps: 50.0,
            max_drawdown_percent: 10.0,
            decision_cooldown_seconds: 3600.0,
        }
    }
}

fn config_error(message: impl Into<String>) -> PoolError {
    PoolError::Config(message.into())
}

impl PoolSettings {
    /// Normalize and validate in place (Python `__post_init__`).
    pub fn validate(&mut self) -> Result<()> {
        if !self.mode.trim().eq_ignore_ascii_case("paper") {
            return Err(config_error(
                "POOL_MODE must be paper; live pool execution is disabled",
            ));
        }
        self.mode = "paper".into();
        if self.db_path.trim().is_empty() {
            return Err(config_error("POOL_DB_PATH must not be blank"));
        }
        self.db_path = self.db_path.trim().to_owned();
        let benchmarks: Vec<String> = self
            .benchmarks
            .iter()
            .map(|asset| asset.trim().to_uppercase())
            .filter(|asset| !asset.is_empty())
            .collect();
        let mut unique = benchmarks.clone();
        unique.sort();
        unique.dedup();
        if benchmarks.is_empty() || unique.len() != benchmarks.len() {
            return Err(config_error("POOL_BENCHMARKS must contain unique assets"));
        }
        if benchmarks.len() != self.benchmark_weights.len() {
            return Err(config_error(
                "POOL_BENCHMARK_WEIGHTS must match POOL_BENCHMARKS",
            ));
        }
        if self
            .benchmark_weights
            .iter()
            .any(|w| !w.is_finite() || *w <= 0.0)
        {
            return Err(config_error(
                "POOL_BENCHMARK_WEIGHTS must contain positive finite numbers",
            ));
        }
        let weight_sum: f64 = self.benchmark_weights.iter().sum();
        if (weight_sum - 1.0).abs() > 1e-8 {
            return Err(config_error("POOL_BENCHMARK_WEIGHTS must sum to 1"));
        }
        self.benchmarks = benchmarks;
        for (name, value) in [
            ("max_route_legs", self.max_route_legs),
            ("max_route_expansions", self.max_route_expansions),
            ("max_candidate_routes", self.max_candidate_routes),
            ("momentum_observations", self.momentum_observations),
        ] {
            if value == 0 {
                return Err(config_error(format!(
                    "{} must be a positive integer",
                    name.to_uppercase()
                )));
            }
        }
        if self.max_route_legs > 3 {
            return Err(config_error("POOL_MAX_ROUTE_LEGS must not exceed 3"));
        }
        for (name, value) in [
            ("min_quote_volume_usdt", self.min_quote_volume_usdt),
            ("max_spread_bps", self.max_spread_bps),
            ("slippage_buffer_bps", self.slippage_buffer_bps),
            ("bnb_fee_buffer_multiplier", self.bnb_fee_buffer_multiplier),
            ("min_bnb_fee_reserve", self.min_bnb_fee_reserve),
            ("max_allocation_percent", self.max_allocation_percent),
            ("max_reallocation_percent", self.max_reallocation_percent),
        ] {
            if value.is_sign_negative() && !value.is_zero() {
                return Err(non_negative_error(name));
            }
        }
        for (name, value) in [
            ("ticker_stale_seconds", self.ticker_stale_seconds),
            ("fee_cache_seconds", self.fee_cache_seconds),
            ("cycle_seconds", self.cycle_seconds),
            ("min_improvement_bps", self.min_improvement_bps),
            ("max_drawdown_percent", self.max_drawdown_percent),
            ("decision_cooldown_seconds", self.decision_cooldown_seconds),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(non_negative_error(name));
            }
        }
        if self.bnb_fee_buffer_multiplier < Decimal::ONE {
            return Err(config_error("BNB_FEE_BUFFER_MULTIPLIER must be at least 1"));
        }
        let scorers: Vec<String> = self
            .scorers
            .iter()
            .map(|name| name.trim().to_lowercase())
            .filter(|name| !name.is_empty())
            .collect();
        if scorers.is_empty()
            || scorers
                .iter()
                .any(|name| !SUPPORTED_SCORERS.contains(&name.as_str()))
        {
            return Err(config_error("POOL_SCORERS contains an unsupported scorer"));
        }
        self.scorers = scorers;
        Ok(())
    }

    /// Validated copy (builder-style convenience for tests and replays).
    pub fn validated(mut self) -> Result<Self> {
        self.validate()?;
        Ok(self)
    }

    /// `(benchmark, weight)` pairs in configured order.
    pub fn weights(&self) -> Vec<(String, f64)> {
        self.benchmarks
            .iter()
            .cloned()
            .zip(self.benchmark_weights.iter().copied())
            .collect()
    }

    /// Load from the process environment (the `bb` binary loads `.env` first).
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Load from any variable source (Python `PoolSettings.from_env`).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let defaults = Self::default();
        let env = Lookup(&lookup);
        let weights = env
            .csv("POOL_BENCHMARK_WEIGHTS", "0.25,0.25,0.25,0.25")
            .iter()
            .map(|value| {
                value
                    .parse::<f64>()
                    .map_err(|_| config_error("POOL_BENCHMARK_WEIGHTS must be a number"))
            })
            .collect::<Result<Vec<f64>>>()?;
        let mut settings = Self {
            use_testnet: env.boolean("USE_TESTNET", false)?,
            mode: lookup("POOL_MODE").unwrap_or_else(|| "paper".into()),
            db_path: lookup("POOL_DB_PATH").unwrap_or(defaults.db_path),
            benchmarks: env.csv("POOL_BENCHMARKS", "USDT,BTC,PAXG,XAUT"),
            benchmark_weights: weights,
            max_route_legs: env.count("POOL_MAX_ROUTE_LEGS", 3)?,
            max_route_expansions: env.count("POOL_MAX_ROUTE_EXPANSIONS", 10_000)?,
            max_candidate_routes: env.count("POOL_MAX_CANDIDATE_ROUTES", 8)?,
            max_scoring_assets: env.count("POOL_MAX_SCORING_ASSETS", 0)?,
            min_quote_volume_usdt: env
                .decimal("POOL_MIN_QUOTE_VOLUME_USDT", defaults.min_quote_volume_usdt)?,
            max_spread_bps: env.decimal("POOL_MAX_SPREAD_BPS", defaults.max_spread_bps)?,
            slippage_buffer_bps: env
                .decimal("POOL_SLIPPAGE_BUFFER_BPS", defaults.slippage_buffer_bps)?,
            ticker_stale_seconds: env.float("POOL_TICKER_STALE_SECONDS", 300.0)?,
            fee_cache_seconds: env.float("POOL_FEE_CACHE_SECONDS", 60.0)?,
            bnb_fee_buffer_multiplier: env.decimal(
                "POOL_BNB_FEE_BUFFER_MULTIPLIER",
                defaults.bnb_fee_buffer_multiplier,
            )?,
            min_bnb_fee_reserve: env
                .decimal("POOL_MIN_BNB_FEE_RESERVE", defaults.min_bnb_fee_reserve)?,
            scorers: env.csv("POOL_SCORERS", "simple_momentum,risk_adjusted_momentum"),
            momentum_observations: env.count("POOL_MOMENTUM_OBSERVATIONS", 96)?,
            cycle_seconds: env.float("POOL_CYCLE_SECONDS", 900.0)?,
            max_allocation_percent: env.decimal(
                "POOL_MAX_ALLOCATION_PERCENT",
                defaults.max_allocation_percent,
            )?,
            max_reallocation_percent: env.decimal(
                "POOL_MAX_REALLOCATION_PERCENT",
                defaults.max_reallocation_percent,
            )?,
            min_improvement_bps: env.float("POOL_MIN_IMPROVEMENT_BPS", 50.0)?,
            max_drawdown_percent: env.float("POOL_MAX_DRAWDOWN_PERCENT", 10.0)?,
            decision_cooldown_seconds: env.float("POOL_DECISION_COOLDOWN_SECONDS", 3600.0)?,
        };
        settings.validate()?;
        Ok(settings)
    }
}

fn non_negative_error(name: &str) -> PoolError {
    config_error(format!(
        "{} must be a finite non-negative number",
        name.to_uppercase()
    ))
}

/// Read-only Binance credentials required by the pool runtime: `BINANCE_API_KEY` and
/// `BINANCE_API_SECRET` (or `BINANCE_SECRET_KEY`), trimmed.
pub fn pool_credentials(lookup: impl Fn(&str) -> Option<String>) -> Result<(String, String)> {
    let api_key = lookup("BINANCE_API_KEY")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| config_error("missing required environment variable: BINANCE_API_KEY"))?;
    let secret = lookup("BINANCE_API_SECRET")
        .filter(|v| !v.is_empty())
        .or_else(|| lookup("BINANCE_SECRET_KEY"))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            config_error(
                "missing required environment variable: BINANCE_API_SECRET or BINANCE_SECRET_KEY",
            )
        })?;
    Ok((api_key, secret))
}

struct Lookup<'a, F: Fn(&str) -> Option<String>>(&'a F);

impl<F: Fn(&str) -> Option<String>> Lookup<'_, F> {
    fn csv(&self, name: &str, default: &str) -> Vec<String> {
        (self.0)(name)
            .unwrap_or_else(|| default.to_owned())
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn float(&self, name: &str, default: f64) -> Result<f64> {
        match (self.0)(name) {
            None => Ok(default),
            Some(raw) => raw
                .trim()
                .parse::<f64>()
                .map_err(|_| config_error(format!("{name} must be a number"))),
        }
    }

    fn decimal(&self, name: &str, default: Decimal) -> Result<Decimal> {
        match (self.0)(name) {
            None => Ok(default),
            Some(raw) => {
                let raw = raw.trim();
                Decimal::from_str_exact(raw)
                    .or_else(|_| Decimal::from_scientific(raw))
                    .map_err(|_| config_error(format!("{name} must be a number")))
            }
        }
    }

    fn count(&self, name: &str, default: usize) -> Result<usize> {
        let Some(raw) = (self.0)(name) else {
            return Ok(default);
        };
        let value: i64 = raw
            .trim()
            .parse()
            .map_err(|_| config_error(format!("{name} must be an integer")))?;
        usize::try_from(value).map_err(|_| match name {
            "POOL_MAX_SCORING_ASSETS" => {
                config_error("POOL_MAX_SCORING_ASSETS must be non-negative")
            }
            _ => config_error(format!(
                "{} must be a positive integer",
                name.trim_start_matches("POOL_")
            )),
        })
    }

    fn boolean(&self, name: &str, default: bool) -> Result<bool> {
        let Some(raw) = (self.0)(name) else {
            return Ok(default);
        };
        match raw.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(config_error(format!("{name} must be a boolean"))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn loads_benchmarks_routes_and_scorers() {
        let env = lookup(&[
            ("BINANCE_API_KEY", " key "),
            ("BINANCE_API_SECRET", " secret "),
            ("POOL_BENCHMARKS", "USDT,BTC,PAXG,XAUT"),
            ("POOL_BENCHMARK_WEIGHTS", "0.1,0.2,0.3,0.4"),
            ("POOL_MAX_ROUTE_LEGS", "3"),
            ("POOL_SCORERS", "simple_momentum,risk_adjusted_momentum"),
        ]);
        let settings = PoolSettings::from_lookup(&env).unwrap();
        assert_eq!(settings.benchmark_weights, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(settings.max_route_legs, 3);
        assert_eq!(settings.mode, "paper");
        assert_eq!(
            pool_credentials(&env).unwrap(),
            ("key".into(), "secret".into())
        );
        let secret_alias = lookup(&[("BINANCE_API_KEY", "k"), ("BINANCE_SECRET_KEY", "s")]);
        assert_eq!(pool_credentials(secret_alias).unwrap().1, "s");
        assert!(pool_credentials(lookup(&[("BINANCE_API_KEY", "k")])).is_err());
    }

    #[test]
    fn refuses_live_mode_and_unbounded_routes() {
        let live = PoolSettings {
            mode: "live".into(),
            ..PoolSettings::default()
        }
        .validated();
        assert!(
            live.unwrap_err()
                .to_string()
                .contains("live pool execution is disabled")
        );
        let legs = PoolSettings {
            max_route_legs: 4,
            ..PoolSettings::default()
        }
        .validated();
        assert!(legs.unwrap_err().to_string().contains("must not exceed 3"));
        assert_eq!(
            PoolSettings::default()
                .validated()
                .unwrap()
                .max_scoring_assets,
            0
        );
        let weights = PoolSettings {
            benchmark_weights: vec![0.5, 0.5],
            ..PoolSettings::default()
        }
        .validated();
        assert!(weights.unwrap_err().to_string().contains("must match"));
        let scorer = PoolSettings {
            scorers: vec!["magic".into()],
            ..PoolSettings::default()
        }
        .validated();
        assert!(
            scorer
                .unwrap_err()
                .to_string()
                .contains("unsupported scorer")
        );
        let bad_float = PoolSettings::from_lookup(lookup(&[("POOL_CYCLE_SECONDS", "soon")]));
        assert_eq!(
            bad_float.unwrap_err().to_string(),
            "POOL_CYCLE_SECONDS must be a number"
        );
    }
}

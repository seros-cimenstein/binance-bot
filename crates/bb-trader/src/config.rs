//! Environment-backed trader configuration (`BOT_SPOT_*`, same names and defaults as
//! `src/claude_spot.py` for every setting that survived the guardrail removal).

use std::path::PathBuf;
use std::str::FromStr;

use bb_core::env::{ConfigError, parse_bool};
use bb_pool::{CatalogConfig, RouteConfig};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

use crate::error::{Result, TraderError};
use crate::scope::ScopeConfig;

/// The only accepted value of `BOT_SPOT_DYNAMIC_SCOPE` for live trading.
pub const DYNAMIC_SCOPE_ALL: &str = "all_eligible_spot";

/// Default `BOT_SPOT_AUDIT_PATH`.
pub const DEFAULT_AUDIT_PATH: &str = "data/bot-spot-decisions.jsonl";
/// Default `BOT_SPOT_STATE_PATH`.
pub const DEFAULT_STATE_PATH: &str = "data/bot-spot-agent-state.json";
/// Default `BOT_SPOT_STOP_PATH`.
pub const DEFAULT_STOP_PATH: &str = "data/bot-spot.stop";

/// Settings that no longer exist (Python-era guardrails; Phase 52 removals). Setting them
/// has no effect; [`TraderConfig::from_lookup`] reports them once.
pub const REMOVED_VARS: &[&str] = &[
    "BOT_SPOT_ALLOW_NEGATIVE_FRICTION_BPS",
    "BOT_SPOT_REQUIRE_MARKET_EVIDENCE_FOR_LIVE",
    "BOT_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS",
    "BOT_SPOT_SPECULATIVE_MODE_ENABLED",
    "BOT_SPOT_SPECULATIVE_MIN_EDGE_BPS",
    "BOT_SPOT_SPECULATIVE_REQUIRE_THESIS",
    "BOT_SPOT_MAX_RECENT_LOSS_STREAK",
    "BOT_SPOT_MIN_TRADE_INTERVAL_SECONDS",
    "BOT_SPOT_MAX_SUBMITTED_TRADES_PER_DAY",
    "BOT_SPOT_REQUIRE_PROVEN_EDGE",
    "BOT_SPOT_PROVEN_EDGE",
    "BOT_SPOT_DECISION_MODE",
    "BOT_SPOT_MAX_BUDGET_USD",
    // The failed-route halt is hard safety now: always on, cleared only by operator review.
    "BOT_SPOT_HALT_AFTER_FAILED_SUBMISSION",
    // Phase 52 (operator 2026-10-29): no Telegram or phone alerts.
    "BOT_SPOT_TELEGRAM_EXPLAIN",
    "BOT_SPOT_TELEGRAM_REASON_CHARS",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_CHAT_ID",
    "TELEGRAM_COOLDOWN_PATH",
    // Phase 52: no social-signal (Telegram/X) evidence; /biz/ comes from the biz-app feed.
    "SOCIAL_SIGNAL_ENABLED",
    // Phase 52: the algorithm always decides; no LLM, omp session, journal prompt,
    // model failover, LLM cost cap or market-evidence service.
    "BOT_SPOT_DECIDER",
    "BOT_SPOT_COMMAND",
    "BOT_SPOT_MODEL",
    "BOT_SPOT_THINKING",
    "BOT_SPOT_WEB_SEARCH",
    "BOT_SPOT_TIMEOUT_SECONDS",
    "BOT_SPOT_FALLBACK_MODELS",
    "BOT_SPOT_PRIMARY_TIMEOUT_SECONDS",
    "BOT_SPOT_FALLBACK_COOLDOWN_SECONDS",
    "BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES",
    "BOT_SPOT_PROMPT_VERSION",
    "BOT_SPOT_SESSION_DIR",
    "BOT_SPOT_JOURNAL_PROMPT_ENTRIES",
    "BOT_SPOT_COMPACT_AT_PERCENT",
    "BOT_SPOT_COMPACT_AT_TOKENS",
    "BOT_SPOT_MAX_DAILY_LLM_COST_USD",
    "BOT_SPOT_MARKET_EVIDENCE_ENABLED",
    "BOT_SPOT_MARKET_EVIDENCE_LOOKBACK_HOURS",
    "BOT_SPOT_MARKET_EVIDENCE_MAX_SYMBOLS",
    "BOT_SPOT_MARKET_EVIDENCE_TIMEFRAME",
    "BOT_SPOT_JOURNAL_PATH",
    // Phase 52: no GCP2 random-number input.
    "BOT_SPOT_GCP2_ENABLED",
    "BOT_SPOT_GCP2_BASE_URL",
    // Phase 52 (operator, A5): no Markov value model.
    "BOT_SPOT_MDP_ENABLED",
    "BOT_SPOT_MDP_MODEL_PATH",
    // Phase 52: no exit plans (live or paper) between cycles.
    "BOT_SPOT_EXIT_PLANS_PATH",
    "BOT_SPOT_EXIT_WATCH_STATE_PATH",
    "BOT_SPOT_EXIT_WATCH_LOG_PATH",
    "BOT_SPOT_EXIT_PLANS_LIVE",
    "BOT_SPOT_DEFAULT_EXIT_PLAN",
    "BOT_SPOT_DEFAULT_EXIT_PLAN_FROM",
    "BOT_SPOT_EXIT_POLL_SECONDS",
];

/// How destination assets are picked for the candidate table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketPickMode {
    /// Top USDT-volume bases plus held assets plus USDT, ranked by estimated USDT output.
    LiquidTopValue,
    /// Small, cheap-to-enter markets ranked by the small-market score.
    SmallMarketAggressive,
}

impl MarketPickMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiquidTopValue => "liquid_top_value",
            Self::SmallMarketAggressive => "small_market_aggressive",
        }
    }
}

impl FromStr for MarketPickMode {
    type Err = TraderError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "liquid_top_value" => Ok(Self::LiquidTopValue),
            "small_market_aggressive" => Ok(Self::SmallMarketAggressive),
            _ => Err(TraderError::config(
                "BOT_SPOT_MARKET_PICK_MODE must be liquid_top_value or small_market_aggressive",
            )),
        }
    }
}

/// Every trader setting.
#[derive(Debug, Clone)]
pub struct TraderConfig {
    pub api_key: String,
    pub api_secret: String,
    // live arming
    pub use_testnet: bool,
    pub dry_run: bool,
    pub confirm_live_trading: String,
    pub enabled: bool,
    pub dynamic_scope: String,
    /// `BOT_SPOT_RULES_VERSION`: the operator's name for the rule set in force, recorded
    /// on every audit row (required with the rule).
    pub rules_version: String,
    pub state_path: PathBuf,
    pub route_stale_seconds: f64,
    // files
    pub audit_path: PathBuf,
    pub lock_path: PathBuf,
    pub stop_path: PathBuf,
    // universe / routing
    pub max_route_legs: usize,
    pub max_route_expansions: usize,
    pub max_candidate_routes: usize,
    pub candidate_limit: usize,
    pub market_pick_mode: MarketPickMode,
    pub min_quote_volume_usdt: Decimal,
    pub max_spread_bps: Decimal,
    pub small_min_quote_volume_usdt: Decimal,
    pub small_max_spread_bps: Decimal,
    pub small_candidate_limit: usize,
    pub slippage_buffer_bps: Decimal,
    pub leg_rounding_reserve_bps: Decimal,
    pub min_leg_quote_reserve_bps: Decimal,
    pub ticker_stale_seconds: f64,
    pub fee_cache_seconds: f64,
    // memory (information only)
    pub trade_memory_limit: usize,
    /// Phase 51: the /biz/ feed (`crate::biz`) recorded every cycle, information only.
    pub biz_feed_path: PathBuf,
    /// A feed whose `as_of` is older than this is recorded as unavailable.
    pub biz_max_age_minutes: u64,
    /// The operator's rule (`BOT_SPOT_ALGO_WEIGHTS`, `BOT_SPOT_ALGO_MIN_POSTERS`); required:
    /// the trader never runs on rules the code picked.
    pub algo_rule: crate::decider::Rule,
    // operator scope
    pub scope: ScopeConfig,
}

impl TraderConfig {
    /// From the process environment (the caller loads `.env` first).
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// From any variable source (tests).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let env = Env { lookup: &lookup };
        let removed: Vec<&str> = REMOVED_VARS
            .iter()
            .copied()
            .filter(|name| env.raw(name).is_some())
            .collect();
        if !removed.is_empty() {
            tracing::warn!(
                vars = %removed.join(","),
                "ignoring removed Bot Spot settings"
            );
        }
        let api_key = env
            .raw("BINANCE_API_KEY")
            .ok_or_else(|| ConfigError::Missing("BINANCE_API_KEY".into()))?;
        let api_secret = env
            .raw("BINANCE_API_SECRET")
            .or_else(|| env.raw("BINANCE_SECRET_KEY"))
            .ok_or_else(|| {
                ConfigError::Missing("BINANCE_API_SECRET or BINANCE_SECRET_KEY".into())
            })?;
        let allowed_symbols_default = env.raw("API_SYMBOL_WHITELIST").unwrap_or_default();
        let allowed_assets_default = env.raw("API_ASSET_WHITELIST").unwrap_or_default();
        let scope = ScopeConfig::new(
            &env.csv("BOT_SPOT_ALLOWED_SYMBOLS", &allowed_symbols_default),
            &env.csv("BOT_SPOT_ALLOWED_ASSETS", &allowed_assets_default),
            &env.csv("BOT_SPOT_BLOCKED_SYMBOLS", ""),
            &env.csv("BOT_SPOT_BLOCKED_ASSETS", ""),
            &env.csv("BOT_SPOT_BLOCKED_QUOTES", "IDR"),
            env.boolean("BOT_SPOT_BLOCK_FIAT_ASSETS", true)?,
        )?;
        let spec = env.raw("BOT_SPOT_ALGO_WEIGHTS").ok_or_else(|| {
            TraderError::config(
                "BOT_SPOT_ALGO_WEIGHTS is required (talk,stance,momentum,buy_above,sell_below): the operator's rule",
            )
        })?;
        let algo_rule = crate::decider::Rule {
            weights: crate::decider::Weights::parse(&spec).map_err(TraderError::config)?,
            min_posters: env.integer("BOT_SPOT_ALGO_MIN_POSTERS", 2)?,
        };
        let rules_version = env.raw("BOT_SPOT_RULES_VERSION").ok_or_else(|| {
            TraderError::config(
                "BOT_SPOT_RULES_VERSION is required: the operator's name for the rule set",
            )
        })?;
        let config = Self {
            api_key,
            api_secret,
            use_testnet: env.boolean("USE_TESTNET", true)?,
            dry_run: env.boolean("DRY_RUN", true)?,
            confirm_live_trading: env
                .string("CONFIRM_LIVE_TRADING", "no")
                .to_ascii_lowercase(),
            enabled: env.boolean("BOT_SPOT_ENABLED", false)?,
            dynamic_scope: env
                .string("BOT_SPOT_DYNAMIC_SCOPE", "")
                .to_ascii_lowercase(),
            rules_version,
            state_path: env.path("BOT_SPOT_STATE_PATH", DEFAULT_STATE_PATH),
            route_stale_seconds: env.float("BOT_SPOT_ROUTE_STALE_SECONDS", 60.0)?,
            audit_path: env.path("BOT_SPOT_AUDIT_PATH", DEFAULT_AUDIT_PATH),
            lock_path: env.path("BOT_SPOT_LOCK_PATH", "data/bot-spot.lock"),
            stop_path: env.path("BOT_SPOT_STOP_PATH", DEFAULT_STOP_PATH),
            max_route_legs: env.integer("BOT_SPOT_MAX_ROUTE_LEGS", 3)?,
            max_route_expansions: env.integer("BOT_SPOT_MAX_ROUTE_EXPANSIONS", 10_000)?,
            max_candidate_routes: env.integer("BOT_SPOT_MAX_CANDIDATE_ROUTES", 1)?,
            candidate_limit: env.integer("BOT_SPOT_CANDIDATE_LIMIT", 5)?,
            market_pick_mode: env
                .string("BOT_SPOT_MARKET_PICK_MODE", "liquid_top_value")
                .parse()?,
            min_quote_volume_usdt: env
                .decimal("BOT_SPOT_MIN_QUOTE_VOLUME_USDT", Decimal::from(100_000))?,
            max_spread_bps: env.decimal("BOT_SPOT_MAX_SPREAD_BPS", Decimal::from(50))?,
            small_min_quote_volume_usdt: env.decimal(
                "BOT_SPOT_SMALL_MIN_QUOTE_VOLUME_USDT",
                Decimal::from(5_000),
            )?,
            small_max_spread_bps: env
                .decimal("BOT_SPOT_SMALL_MAX_SPREAD_BPS", Decimal::from(250))?,
            small_candidate_limit: env.integer("BOT_SPOT_SMALL_CANDIDATE_LIMIT", 10)?,
            slippage_buffer_bps: env
                .decimal("BOT_SPOT_SLIPPAGE_BUFFER_BPS", Decimal::from(25))?,
            leg_rounding_reserve_bps: env
                .decimal("BOT_SPOT_LEG_ROUNDING_RESERVE_BPS", Decimal::from(5))?,
            min_leg_quote_reserve_bps: env
                .decimal("BOT_SPOT_MIN_LEG_QUOTE_RESERVE_BPS", Decimal::from(25))?,
            ticker_stale_seconds: env.float("BOT_SPOT_TICKER_STALE_SECONDS", 300.0)?,
            fee_cache_seconds: env.float("BOT_SPOT_FEE_CACHE_SECONDS", 60.0)?,
            trade_memory_limit: env.integer("BOT_SPOT_TRADE_MEMORY_LIMIT", 8)?,
            biz_feed_path: env.path("BOT_SPOT_BIZ_FEED_PATH", crate::biz::DEFAULT_FEED_PATH),
            biz_max_age_minutes: env.integer(
                "BOT_SPOT_BIZ_MAX_AGE_MINUTES",
                crate::biz::DEFAULT_MAX_AGE_MINUTES,
            )?,
            algo_rule,
            scope,
        };
        config.validate()?;
        Ok(config)
    }

    /// Value checks shared by every constructor.
    pub fn validate(&self) -> Result<()> {
        if self.api_key.trim().is_empty() || self.api_secret.trim().is_empty() {
            return Err(TraderError::config(
                "Bot Spot requires Binance API credentials",
            ));
        }
        for (name, path) in [
            ("audit_path", &self.audit_path),
            ("lock_path", &self.lock_path),
            ("emergency_stop_path", &self.stop_path),
            ("state_path", &self.state_path),
        ] {
            if path.as_os_str().is_empty() {
                return Err(TraderError::config(format!("{name} must not be blank")));
            }
        }
        if self.rules_version.trim().is_empty() {
            return Err(TraderError::config("BOT_SPOT_RULES_VERSION must not be blank"));
        }
        if self.biz_max_age_minutes == 0 {
            return Err(TraderError::config(
                "biz_max_age_minutes must be positive",
            ));
        }
        if !(1..=3).contains(&self.max_route_legs) {
            return Err(TraderError::config(
                "max_route_legs must be between 1 and 3",
            ));
        }
        for (name, value) in [
            ("min_quote_volume_usdt", self.min_quote_volume_usdt),
            ("max_spread_bps", self.max_spread_bps),
            (
                "small_min_quote_volume_usdt",
                self.small_min_quote_volume_usdt,
            ),
            ("small_max_spread_bps", self.small_max_spread_bps),
            ("slippage_buffer_bps", self.slippage_buffer_bps),
            ("leg_rounding_reserve_bps", self.leg_rounding_reserve_bps),
            ("min_leg_quote_reserve_bps", self.min_leg_quote_reserve_bps),
        ] {
            if value < Decimal::ZERO {
                return Err(TraderError::config(format!(
                    "{name} must be finite and non-negative"
                )));
            }
        }
        for (name, value) in [
            ("ticker_stale_seconds", self.ticker_stale_seconds),
            ("fee_cache_seconds", self.fee_cache_seconds),
            ("route_stale_seconds", self.route_stale_seconds),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(TraderError::config(format!(
                    "{name} must be finite and non-negative"
                )));
            }
        }
        Ok(())
    }

    /// `DRY_RUN`, `testnet` or `LIVE` (the environment, independent of plan-only).
    pub fn mode(&self) -> &'static str {
        if self.dry_run {
            "DRY_RUN"
        } else if self.use_testnet {
            "testnet"
        } else {
            "LIVE"
        }
    }

    /// Non-secret run configuration, recorded on every cycle summary so the dashboard can
    /// group cycles and trades by the setup that produced them. Never includes credentials.
    pub fn run_config(&self, nonstop: bool) -> serde_json::Value {
        let decimal = |value: Decimal| value.normalize().to_string();
        serde_json::json!({
            "mode": self.mode(),
            "rules_version": self.rules_version,
            "tick_seconds": nonstop.then_some(crate::nonstop::TICK.as_secs()),
            "routing": {
                "market_pick_mode": self.market_pick_mode.as_str(),
                "max_route_legs": self.max_route_legs,
                "max_candidate_routes": self.max_candidate_routes,
                "candidate_limit": self.candidate_limit,
                "small_candidate_limit": self.small_candidate_limit,
                "min_quote_volume_usdt": decimal(self.min_quote_volume_usdt),
                "max_spread_bps": decimal(self.max_spread_bps),
                "small_min_quote_volume_usdt": decimal(self.small_min_quote_volume_usdt),
                "small_max_spread_bps": decimal(self.small_max_spread_bps),
                "slippage_buffer_bps": decimal(self.slippage_buffer_bps),
                "leg_rounding_reserve_bps": decimal(self.leg_rounding_reserve_bps),
            },
            "trade_memory_limit": self.trade_memory_limit,
            "biz": {
                "feed_path": self.biz_feed_path,
                "max_age_minutes": self.biz_max_age_minutes,
            },
            "rule": {
                "weights": self.algo_rule.weights.to_string(),
                "min_posters": self.algo_rule.min_posters,
            },
            "scope": self.scope,
        })
    }

    /// Hard safety: every live arming flag must be set. Checked at cycle start and again
    /// before every route execution.
    pub fn check_live_arming(&self) -> Result<()> {
        if !self.enabled {
            return Err(TraderError::safety(
                "BOT_SPOT_ENABLED=true is required for live submission",
            ));
        }
        if self.dynamic_scope != DYNAMIC_SCOPE_ALL {
            return Err(TraderError::safety(
                "BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot is required for dynamic live scope",
            ));
        }
        if self.use_testnet {
            return Err(TraderError::safety(
                "Bot Spot live submission requires USE_TESTNET=false",
            ));
        }
        if self.dry_run {
            return Err(TraderError::safety(
                "Bot Spot live submission requires DRY_RUN=false",
            ));
        }
        if self.confirm_live_trading != "yes" {
            return Err(TraderError::safety(
                "Bot Spot live submission requires CONFIRM_LIVE_TRADING=yes",
            ));
        }
        Ok(())
    }

    pub fn effective_min_quote_volume_usdt(&self) -> Decimal {
        match self.market_pick_mode {
            MarketPickMode::SmallMarketAggressive => self.small_min_quote_volume_usdt,
            MarketPickMode::LiquidTopValue => self.min_quote_volume_usdt,
        }
    }

    pub fn effective_max_spread_bps(&self) -> Decimal {
        match self.market_pick_mode {
            MarketPickMode::SmallMarketAggressive => self.small_max_spread_bps,
            MarketPickMode::LiquidTopValue => self.max_spread_bps,
        }
    }

    /// Candidate routes shown in the cycle prompt.
    pub fn effective_candidate_limit(&self) -> usize {
        match self.market_pick_mode {
            MarketPickMode::SmallMarketAggressive => self.small_candidate_limit,
            MarketPickMode::LiquidTopValue => self.candidate_limit,
        }
    }

    pub fn catalog_config(&self) -> CatalogConfig {
        CatalogConfig {
            min_quote_volume_usdt: self.effective_min_quote_volume_usdt(),
            max_spread_bps: self.effective_max_spread_bps(),
            ticker_stale_seconds: self.ticker_stale_seconds,
            max_scoring_assets: match self.market_pick_mode {
                MarketPickMode::LiquidTopValue => self.candidate_limit,
                MarketPickMode::SmallMarketAggressive => 0,
            },
        }
    }

    pub fn route_config(&self) -> RouteConfig {
        RouteConfig {
            max_route_legs: self.max_route_legs,
            max_route_expansions: self.max_route_expansions,
            max_candidate_routes: self.max_candidate_routes.max(1),
            slippage_buffer_bps: self.slippage_buffer_bps,
        }
    }
}

/// Typed readers over a variable lookup. Blank values count as unset, except for the
/// comma lists where an explicitly empty value clears the default (Python `_csv_env`).
struct Env<'a> {
    lookup: &'a dyn Fn(&str) -> Option<String>,
}

impl Env<'_> {
    fn raw(&self, name: &str) -> Option<String> {
        (self.lookup)(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn string(&self, name: &str, default: &str) -> String {
        self.raw(name).unwrap_or_else(|| default.to_owned())
    }

    fn path(&self, name: &str, default: &str) -> PathBuf {
        PathBuf::from(self.string(name, default))
    }

    fn invalid(name: &str, expected: &'static str, value: String) -> ConfigError {
        ConfigError::Invalid {
            name: name.to_owned(),
            expected,
            value,
        }
    }

    fn boolean(&self, name: &str, default: bool) -> Result<bool, ConfigError> {
        match self.raw(name) {
            None => Ok(default),
            Some(value) => {
                parse_bool(&value).ok_or_else(|| Self::invalid(name, "a boolean", value))
            }
        }
    }

    fn float(&self, name: &str, default: f64) -> Result<f64, ConfigError> {
        match self.raw(name) {
            None => Ok(default),
            Some(value) => match value.parse::<f64>() {
                Ok(number) if number.is_finite() => Ok(number),
                _ => Err(Self::invalid(name, "a finite number", value)),
            },
        }
    }

    fn decimal(&self, name: &str, default: Decimal) -> Result<Decimal, ConfigError> {
        match self.raw(name) {
            None => Ok(default),
            Some(value) => Decimal::from_str_exact(&value)
                .or_else(|_| Decimal::from_scientific(&value))
                .or_else(|_| {
                    value
                        .parse::<f64>()
                        .ok()
                        .and_then(Decimal::from_f64)
                        .ok_or(())
                })
                .map_err(|()| Self::invalid(name, "a decimal number", value)),
        }
    }

    fn integer<T: FromStr>(&self, name: &str, default: T) -> Result<T, ConfigError> {
        match self.raw(name) {
            None => Ok(default),
            Some(value) => value
                .parse::<T>()
                .map_err(|_| Self::invalid(name, "a non-negative integer", value)),
        }
    }

    fn csv(&self, name: &str, default: &str) -> Vec<String> {
        let text = (self.lookup)(name).unwrap_or_else(|| default.to_owned());
        text.split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use super::*;

    pub(crate) fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let mut map: HashMap<String, String> = HashMap::from([
            ("BINANCE_API_KEY".to_owned(), "key".to_owned()),
            ("BINANCE_API_SECRET".to_owned(), "secret".to_owned()),
            // Tests run on a fixed rule; production requires the operator's.
            ("BOT_SPOT_ALGO_WEIGHTS".to_owned(), "1,0.5,0.5,0.5,0".to_owned()),
            ("BOT_SPOT_RULES_VERSION".to_owned(), "test-rules".to_owned()),
        ]);
        for (name, value) in pairs {
            map.insert((*name).to_owned(), (*value).to_owned());
        }
        move |name| map.get(name).cloned()
    }

    fn armed() -> Vec<(&'static str, &'static str)> {
        vec![
            ("BOT_SPOT_ENABLED", "true"),
            ("BOT_SPOT_DYNAMIC_SCOPE", "all_eligible_spot"),
            ("USE_TESTNET", "false"),
            ("DRY_RUN", "false"),
            ("CONFIRM_LIVE_TRADING", "yes"),
        ]
    }

    #[test]
    fn run_config_records_the_setup_without_credentials() {
        let config = TraderConfig::from_lookup(lookup(&[
            ("BINANCE_API_KEY", "KEY-must-not-leak"),
            ("BINANCE_API_SECRET", "SECRET-must-not-leak"),
            ("CONFIRM_LIVE_TRADING", "yes"),
            ("BOT_SPOT_RULES_VERSION", "rules-2026-10-30"),
        ]))
        .unwrap();
        let snapshot = config.run_config(true);
        let text = snapshot.to_string();
        assert!(!text.contains("must-not-leak"), "{text}");
        assert_eq!(snapshot["rules_version"], "rules-2026-10-30");
        assert_eq!(snapshot["tick_seconds"], 15);
    }

    #[test]
    fn defaults_match_the_python_trader_and_agent_spec() {
        let config = TraderConfig::from_lookup(lookup(&[])).unwrap();
        assert!(config.use_testnet && config.dry_run && !config.enabled);
        assert_eq!(config.confirm_live_trading, "no");
        assert_eq!(
            config.audit_path,
            PathBuf::from("data/bot-spot-decisions.jsonl")
        );
        assert_eq!(config.max_route_legs, 3);
        assert_eq!(config.market_pick_mode, MarketPickMode::LiquidTopValue);
        assert_eq!(config.slippage_buffer_bps, Decimal::from(25));
        assert_eq!(config.scope.blocked_quotes, vec!["IDR".to_owned()]);
        assert!(config.scope.block_fiat_assets);
        assert_eq!(config.mode(), "DRY_RUN");
    }

    #[test]
    fn removed_guardrail_vars_are_ignored() {
        let config = TraderConfig::from_lookup(lookup(&[
            ("BOT_SPOT_ALLOW_NEGATIVE_FRICTION_BPS", "not-a-number"),
            ("BOT_SPOT_DECISION_MODE", "freestyle"),
            ("BOT_SPOT_MAX_SUBMITTED_TRADES_PER_DAY", "-3"),
            // Phase 52 removals: an old LLM env file still loads.
            ("BOT_SPOT_THINKING", "loud"),
            ("BOT_SPOT_TIMEOUT_SECONDS", "0"),
            ("BOT_SPOT_FALLBACK_MODELS", "not a model"),
            ("BOT_SPOT_MARKET_EVIDENCE_TIMEFRAME", "4h"),
        ]));
        assert!(config.is_ok());
    }

    #[test]
    fn live_arming_refuses_each_missing_flag() {
        let full = armed();
        TraderConfig::from_lookup(lookup(&full))
            .unwrap()
            .check_live_arming()
            .unwrap();
        let cases = [
            ("BOT_SPOT_ENABLED", "false", "BOT_SPOT_ENABLED=true"),
            ("BOT_SPOT_DYNAMIC_SCOPE", "", "all_eligible_spot"),
            ("USE_TESTNET", "true", "USE_TESTNET=false"),
            ("DRY_RUN", "true", "DRY_RUN=false"),
            ("CONFIRM_LIVE_TRADING", "no", "CONFIRM_LIVE_TRADING=yes"),
        ];
        for (name, value, expected) in cases {
            let mut pairs = full.clone();
            pairs.retain(|(key, _)| *key != name);
            pairs.push((name, value));
            let error = TraderConfig::from_lookup(lookup(&pairs))
                .unwrap()
                .check_live_arming()
                .unwrap_err();
            assert!(matches!(error, TraderError::Safety(_)), "{name}");
            assert!(error.to_string().contains(expected), "{name}: {error}");
            // Unset (not only wrong) also refuses.
            pairs.retain(|(key, _)| *key != name);
            assert!(
                TraderConfig::from_lookup(lookup(&pairs))
                    .unwrap()
                    .check_live_arming()
                    .is_err(),
                "{name} unset"
            );
        }
    }

    #[test]
    fn validation_rejects_bad_values() {
        for pairs in [
            [("BOT_SPOT_MAX_ROUTE_LEGS", "4")],
            [("BOT_SPOT_MAX_ROUTE_LEGS", "0")],
            [("BOT_SPOT_MARKET_PICK_MODE", "yolo")],
            [("BOT_SPOT_RULES_VERSION", " ")],
            [("BOT_SPOT_CANDIDATE_LIMIT", "-1")],
            [("BOT_SPOT_BLOCKED_ASSETS", "B@D")],
            [("USE_TESTNET", "maybe")],
        ] {
            assert!(
                TraderConfig::from_lookup(lookup(&pairs)).is_err(),
                "{pairs:?}"
            );
        }
        assert!(TraderConfig::from_lookup(|_| None).is_err());
    }

    #[test]
    fn explicitly_empty_list_clears_default_and_whitelist_falls_back() {
        let config = TraderConfig::from_lookup(lookup(&[
            ("BOT_SPOT_BLOCKED_QUOTES", ""),
            ("API_SYMBOL_WHITELIST", "btc/usdt, ETHUSDT"),
        ]))
        .unwrap();
        assert!(config.scope.blocked_quotes.is_empty());
        assert_eq!(
            config.scope.allowed_symbols,
            vec!["BTC/USDT".to_owned(), "ETHUSDT".to_owned()]
        );
    }

    #[test]
    fn the_rule_is_required_and_parsed_from_the_operators_weights() {
        let without = |name: &str| {
            let base = lookup(&[]);
            let skip = name.to_owned();
            move |key: &str| if key == skip { None } else { base(key) }
        };
        let err = TraderConfig::from_lookup(without("BOT_SPOT_ALGO_WEIGHTS"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("BOT_SPOT_ALGO_WEIGHTS is required"), "{err}");
        let err = TraderConfig::from_lookup(without("BOT_SPOT_RULES_VERSION"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("BOT_SPOT_RULES_VERSION is required"), "{err}");
        let config = TraderConfig::from_lookup(lookup(&[
            ("BOT_SPOT_ALGO_WEIGHTS", "1,0.25,0.5,0.5,0"),
            ("BOT_SPOT_ALGO_MIN_POSTERS", "3"),
        ]))
        .unwrap();
        assert_eq!((config.algo_rule.min_posters, config.algo_rule.weights.stance), (3, 0.25));
        assert_eq!(config.run_config(false)["rule"]["min_posters"], 3);
        assert!(TraderConfig::from_lookup(lookup(&[("BOT_SPOT_ALGO_WEIGHTS", "1,2")])).is_err());
        // An env file still carrying the six-number LLM-era rule (with a value weight)
        // refuses to start instead of shifting the levels into the wrong slots.
        let err = TraderConfig::from_lookup(lookup(&[("BOT_SPOT_ALGO_WEIGHTS", "1,0.5,0.5,0,0.5,0")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("need 5 numbers"), "{err}");
    }
}

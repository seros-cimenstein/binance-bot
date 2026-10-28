//! Environment-backed trader configuration (`BOT_SPOT_*`, same names and defaults as
//! `src/claude_spot.py` for every setting that survived the guardrail removal).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use bb_core::env::{ConfigError, parse_bool};
use bb_omp::ThinkingLevel;
use bb_pool::{CatalogConfig, RouteConfig};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

use crate::error::{Result, TraderError};
use crate::scope::ScopeConfig;

/// Prompt/audit version tag of the agentic trader. v11: the v7 prompt restored on the
/// old-gold-way branch, plus the stretch fee floor and closed-position feedback. v12
/// (2026-10-18, LOOP_MONEY section B): information-only changes to the cycle message.
/// v13 (2026-10-18, Phase 47): 20-coin market breadth in the cycle prompt, hold-duration and
/// +4h entry timing in "Your record", liquid-leader candidate filtering, and asymmetric
/// regime/swing guidance in the system prompt. v14 (2026-10-19, operator decision): live, the
/// exit-plan section ends with the real results of the positions a plan sold instead of
/// "plan vs your own exits" (in live mode only trigger price vs fill). v15 (2026-10-20,
/// Phase 49, operator decision): the schizo trader. With `BOT_SPOT_GCP2_ENABLED` (on by
/// default) the system prompt makes the bot act out a GCP2-driven mood among the four
/// personas, hear their votes, bet one coin with all cash and obey YOLO (removed in v17), and the cycle
/// message opens with `## Your schizo state`. With GCP2 off both prompts are the v14 text.
/// v16 (2026-10-20, operator decision): the mood follows GCP2 instead of running inverted
/// (it switches when the GCP2 zone changes and holds while it stays the same). v17
/// (2026-10-21, Phase 50, operator decision): with `BOT_SPOT_MDP_ENABLED` (on by default) the
/// system prompt adds the long-term value voice and the cycle message its
/// `## Your long-term value` section (`crate::mdp`, γ=0.95 per cycle); YOLO is removed.
pub const PROMPT_VERSION: &str = "bot-spot-v17-schizo";
/// The only accepted value of `BOT_SPOT_DYNAMIC_SCOPE` for live trading.
pub const DYNAMIC_SCOPE_ALL: &str = "all_eligible_spot";

/// Default `BOT_SPOT_AUDIT_PATH`.
pub const DEFAULT_AUDIT_PATH: &str = "data/bot-spot-decisions.jsonl";
/// Default `BOT_SPOT_JOURNAL_PATH`.
pub const DEFAULT_JOURNAL_PATH: &str = "data/bot-spot-journal.jsonl";
/// Default `BOT_SPOT_STATE_PATH`.
pub const DEFAULT_STATE_PATH: &str = "data/bot-spot-agent-state.json";
/// Default `BOT_SPOT_STOP_PATH`.
pub const DEFAULT_STOP_PATH: &str = "data/bot-spot.stop";

/// Guardrail variables of the Python trader that no longer exist. Setting them has no
/// effect; [`TraderConfig::from_lookup`] reports them once.
pub const REMOVED_VARS: [&str; 14] = [
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

/// Who decides each cycle (`BOT_SPOT_DECIDER`, Phase 51).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decider {
    /// The omp LLM session with its host tools (the default).
    Llm,
    /// The rule-based decider ([`crate::decider`]); the LLM is never started.
    Algo,
}

impl Decider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Algo => "algo",
        }
    }
}

impl FromStr for Decider {
    type Err = TraderError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "llm" => Ok(Self::Llm),
            "algo" => Ok(Self::Algo),
            _ => Err(TraderError::config("BOT_SPOT_DECIDER must be llm or algo")),
        }
    }
}

/// One model of the trader's chain: `provider/model-id` plus its thinking level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub provider: String,
    pub model_id: String,
    pub thinking: ThinkingLevel,
}

impl ModelSpec {
    /// `provider/model-id[:thinking]`; an omitted thinking level uses `default_thinking`.
    pub fn parse(entry: &str, default_thinking: ThinkingLevel) -> Result<Self> {
        let entry = entry.trim();
        let invalid = |why: &str| {
            TraderError::config(format!(
                "invalid model {entry:?}: {why} (expected provider/model-id[:thinking])"
            ))
        };
        let (name, thinking) = match entry.rsplit_once(':') {
            Some((name, level)) => {
                let level = level.trim().to_ascii_lowercase();
                let thinking =
                    parse_thinking(&level).ok_or_else(|| invalid("unknown thinking level"))?;
                (name.trim(), thinking)
            }
            None => (entry, default_thinking),
        };
        let Some((provider, model_id)) = name.split_once('/') else {
            return Err(invalid("missing provider"));
        };
        let (provider, model_id) = (provider.trim(), model_id.trim());
        if provider.is_empty()
            || model_id.is_empty()
            || provider.contains(char::is_whitespace)
            || model_id.contains(char::is_whitespace)
        {
            return Err(invalid("blank provider or model id"));
        }
        Ok(Self {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
            thinking,
        })
    }

    /// `provider/model-id` (a bare primary `--model` pattern has no provider).
    pub fn name(&self) -> String {
        if self.provider.is_empty() {
            self.model_id.clone()
        } else {
            format!("{}/{}", self.provider, self.model_id)
        }
    }
}

/// A named thinking level (`inherit` is not a level).
fn parse_thinking(level: &str) -> Option<ThinkingLevel> {
    serde_json::from_value(serde_json::Value::String(level.to_owned()))
        .ok()
        .filter(|level| !matches!(level, ThinkingLevel::Inherit))
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
    // agent
    pub command: String,
    pub model: String,
    pub thinking: ThinkingLevel,
    /// Enable omp's built-in `web_search` tool alongside the host tools.
    pub web_search: bool,
    pub timeout: Duration,
    /// `BOT_SPOT_FALLBACK_MODELS`: tried in order when the active model fails a turn
    /// (without an `execute_route` call) or a compaction. Empty = no fallback.
    pub fallback_models: Vec<ModelSpec>,
    /// Per-attempt timeout of the primary while fallbacks exist (fallbacks use `timeout`).
    pub primary_timeout: Duration,
    /// How long to stay on a fallback before the primary is tried again (next cycle start).
    pub fallback_cooldown: Duration,
    /// Consecutive cycles failed on every model before the brain-down alert (0 = never).
    pub brain_down_alert_cycles: u32,
    pub prompt_version: String,
    pub session_dir: PathBuf,
    pub journal_path: PathBuf,
    pub journal_prompt_entries: usize,
    pub compact_at_percent: f64,
    /// `BOT_SPOT_COMPACT_AT_TOKENS`: also compact at this many context tokens (0 = off).
    pub compact_at_tokens: u64,
    /// 0 = unlimited.
    pub max_daily_llm_cost_usd: Decimal,
    pub state_path: PathBuf,
    pub route_stale_seconds: f64,
    // files
    pub audit_path: PathBuf,
    pub lock_path: PathBuf,
    pub stop_path: PathBuf,
    /// Phase 45 exit plans: plan book (written by `set_exit_plan`), watch state and log.
    pub exit_paths: crate::exit_book::ExitPaths,
    /// Phase 45 exit plans: live or paper, default plan, poll interval.
    pub exit: crate::exit_book::ExitSettings,
    // telegram
    pub telegram_explain: bool,
    pub telegram_reason_chars: usize,
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
    // evidence / memory (information only)
    pub market_evidence_enabled: bool,
    pub market_evidence_lookback_hours: u32,
    pub market_evidence_max_symbols: usize,
    pub trade_memory_limit: usize,
    /// Phase 49: read GCP2 live once per cycle and run the schizo state and voice (v15).
    pub gcp2_enabled: bool,
    pub gcp2_base_url: String,
    /// Phase 50: show the long-term value voice (`crate::mdp`) every cycle (v17).
    pub mdp_enabled: bool,
    /// `bb bot-spot-mdp-fit` model file the voice reads every cycle.
    pub mdp_model_path: PathBuf,
    /// Phase 51: the /biz/ feed (`crate::biz`) recorded every cycle, information only.
    pub biz_feed_path: PathBuf,
    /// A feed whose `as_of` is older than this is recorded as unavailable.
    pub biz_max_age_minutes: u64,
    /// Phase 51: who decides each cycle.
    pub decider: Decider,
    /// The rule of [`Decider::Algo`] (`BOT_SPOT_ALGO_WEIGHTS`, `BOT_SPOT_ALGO_MIN_POSTERS`);
    /// `None` with the LLM.
    pub algo_rule: Option<crate::decider::Rule>,
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
                "ignoring removed Bot Spot guardrail settings (the bot sees costs and evidence as information)"
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
        let thinking_raw = env
            .string("BOT_SPOT_THINKING", "high")
            .to_ascii_lowercase();
        let thinking: ThinkingLevel = serde_json::from_value(serde_json::Value::String(
            thinking_raw.clone(),
        ))
        .map_err(|_| ConfigError::Invalid {
            name: "BOT_SPOT_THINKING".into(),
            expected: "off, minimal, low, medium, high, xhigh or max",
            value: thinking_raw,
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
        let timeout_seconds = env.float("BOT_SPOT_TIMEOUT_SECONDS", 300.0)?;
        let fallback_models = env
            .csv("BOT_SPOT_FALLBACK_MODELS", "")
            .iter()
            .map(|entry| ModelSpec::parse(entry, thinking))
            .collect::<Result<Vec<_>>>()?;
        let primary_timeout_seconds = env.float("BOT_SPOT_PRIMARY_TIMEOUT_SECONDS", 150.0)?;
        let cooldown_seconds = env.float("BOT_SPOT_FALLBACK_COOLDOWN_SECONDS", 1800.0)?;
        if cooldown_seconds < 0.0 {
            return Err(TraderError::config(
                "BOT_SPOT_FALLBACK_COOLDOWN_SECONDS must be non-negative",
            ));
        }
        let evidence_timeframe = env.string("BOT_SPOT_MARKET_EVIDENCE_TIMEFRAME", "1h");
        if evidence_timeframe != "1h" {
            return Err(TraderError::config("market_evidence_timeframe must be 1h"));
        }
        let exit = crate::exit_book::ExitSettings::from_lookup(|name| env.raw(name))
            .map_err(TraderError::config)?;
        let decider: Decider = env.string("BOT_SPOT_DECIDER", "llm").parse()?;
        let algo_rule = match decider {
            Decider::Llm => None,
            Decider::Algo => {
                let spec = env.raw("BOT_SPOT_ALGO_WEIGHTS").ok_or_else(|| {
                    TraderError::config(
                        "BOT_SPOT_DECIDER=algo needs BOT_SPOT_ALGO_WEIGHTS (talk,stance,momentum,value,buy_above,sell_below)",
                    )
                })?;
                Some(crate::decider::Rule {
                    weights: crate::decider::Weights::parse(&spec).map_err(TraderError::config)?,
                    min_posters: env.integer("BOT_SPOT_ALGO_MIN_POSTERS", 2)?,
                })
            }
        };
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
            command: env.string("BOT_SPOT_COMMAND", "omp"),
            model: env.string("BOT_SPOT_MODEL", "anthropic/claude-haiku-5-5"),
            thinking,
            web_search: env.boolean("BOT_SPOT_WEB_SEARCH", true)?,
            timeout: positive_duration(timeout_seconds, "bot_timeout_seconds")?,
            fallback_models,
            primary_timeout: positive_duration(
                primary_timeout_seconds,
                "BOT_SPOT_PRIMARY_TIMEOUT_SECONDS",
            )?,
            fallback_cooldown: Duration::from_secs_f64(cooldown_seconds),
            brain_down_alert_cycles: env.integer("BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES", 3)?,
            prompt_version: env.string("BOT_SPOT_PROMPT_VERSION", PROMPT_VERSION),
            session_dir: env.path("BOT_SPOT_SESSION_DIR", "data/bot-spot-session"),
            journal_path: env.path("BOT_SPOT_JOURNAL_PATH", DEFAULT_JOURNAL_PATH),
            journal_prompt_entries: env.integer("BOT_SPOT_JOURNAL_PROMPT_ENTRIES", 8)?,
            compact_at_percent: env.float("BOT_SPOT_COMPACT_AT_PERCENT", 70.0)?,
            compact_at_tokens: env.integer("BOT_SPOT_COMPACT_AT_TOKENS", 0)?,
            max_daily_llm_cost_usd: env
                .decimal("BOT_SPOT_MAX_DAILY_LLM_COST_USD", Decimal::ZERO)?,
            state_path: env.path("BOT_SPOT_STATE_PATH", DEFAULT_STATE_PATH),
            route_stale_seconds: env.float("BOT_SPOT_ROUTE_STALE_SECONDS", 60.0)?,
            audit_path: env.path("BOT_SPOT_AUDIT_PATH", DEFAULT_AUDIT_PATH),
            lock_path: env.path("BOT_SPOT_LOCK_PATH", "data/bot-spot.lock"),
            stop_path: env.path("BOT_SPOT_STOP_PATH", DEFAULT_STOP_PATH),
            exit_paths: crate::exit_book::ExitPaths::from_lookup(|name| env.raw(name)),
            exit,
            telegram_explain: env.boolean("BOT_SPOT_TELEGRAM_EXPLAIN", true)?,
            telegram_reason_chars: env.integer("BOT_SPOT_TELEGRAM_REASON_CHARS", 0)?,
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
            market_evidence_enabled: env.boolean("BOT_SPOT_MARKET_EVIDENCE_ENABLED", true)?,
            market_evidence_lookback_hours: env
                .integer("BOT_SPOT_MARKET_EVIDENCE_LOOKBACK_HOURS", 168)?,
            market_evidence_max_symbols: env
                .integer("BOT_SPOT_MARKET_EVIDENCE_MAX_SYMBOLS", 12)?,
            trade_memory_limit: env.integer("BOT_SPOT_TRADE_MEMORY_LIMIT", 8)?,
            gcp2_enabled: env.boolean("BOT_SPOT_GCP2_ENABLED", true)?,
            gcp2_base_url: env.string("BOT_SPOT_GCP2_BASE_URL", crate::gcp2::DEFAULT_BASE_URL),
            mdp_enabled: env.boolean("BOT_SPOT_MDP_ENABLED", true)?,
            mdp_model_path: env.path("BOT_SPOT_MDP_MODEL_PATH", crate::mdp::DEFAULT_MODEL_PATH),
            biz_feed_path: env.path("BOT_SPOT_BIZ_FEED_PATH", crate::biz::DEFAULT_FEED_PATH),
            biz_max_age_minutes: env.integer(
                "BOT_SPOT_BIZ_MAX_AGE_MINUTES",
                crate::biz::DEFAULT_MAX_AGE_MINUTES,
            )?,
            decider,
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
            ("session_dir", &self.session_dir),
            ("journal_path", &self.journal_path),
            ("state_path", &self.state_path),
        ] {
            if path.as_os_str().is_empty() {
                return Err(TraderError::config(format!("{name} must not be blank")));
            }
        }
        if self.command.trim().is_empty() {
            return Err(TraderError::config("bot_command must not be blank"));
        }
        if self.model.trim().is_empty() {
            return Err(TraderError::config("bot_model must not be blank"));
        }
        if self.prompt_version.trim().is_empty() {
            return Err(TraderError::config("prompt_version must not be blank"));
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
            ("max_daily_llm_cost_usd", self.max_daily_llm_cost_usd),
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
        if !(self.compact_at_percent > 0.0 && self.compact_at_percent <= 100.0) {
            return Err(TraderError::config(
                "compact_at_percent must be in (0, 100]",
            ));
        }
        if matches!(self.thinking, ThinkingLevel::Inherit) {
            return Err(TraderError::config(
                "BOT_SPOT_THINKING must name a level",
            ));
        }
        self.model_chain()?;
        Ok(())
    }

    /// `[primary, fallbacks...]`. Without fallbacks the primary stays an opaque `--model`
    /// pattern; with fallbacks it must be `provider/model-id` so the trader can switch back
    /// to it, and no fallback may repeat the primary or another fallback.
    pub fn model_chain(&self) -> Result<Vec<ModelSpec>> {
        if self.fallback_models.is_empty() {
            let (provider, model_id) = self.model.split_once('/').unwrap_or(("", &self.model));
            return Ok(vec![ModelSpec {
                provider: provider.trim().to_owned(),
                model_id: model_id.trim().to_owned(),
                thinking: self.thinking,
            }]);
        }
        let primary = ModelSpec::parse(&self.model, self.thinking).map_err(|error| {
            TraderError::config(format!(
                "BOT_SPOT_MODEL must be provider/model-id when BOT_SPOT_FALLBACK_MODELS is set: {error}"
            ))
        })?;
        let mut chain = vec![primary];
        for fallback in &self.fallback_models {
            if chain.iter().any(|model| model.name() == fallback.name()) {
                return Err(TraderError::config(format!(
                    "BOT_SPOT_FALLBACK_MODELS entry {} repeats the primary or another fallback",
                    fallback.name()
                )));
            }
            chain.push(fallback.clone());
        }
        Ok(chain)
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
    pub fn run_config(&self, cycle_interval_seconds: Option<u64>) -> serde_json::Value {
        let decimal = |value: Decimal| value.normalize().to_string();
        serde_json::json!({
            "mode": self.mode(),
            "prompt_version": self.prompt_version,
            "model": self.model,
            "thinking": self.thinking.as_str(),
            "fallback_models": self
                .fallback_models
                .iter()
                .map(|m| format!("{}:{}", m.name(), m.thinking.as_str()))
                .collect::<Vec<_>>(),
            "turn_timeout_seconds": self.timeout.as_secs(),
            "primary_timeout_seconds": self.primary_timeout.as_secs(),
            "fallback_cooldown_seconds": self.fallback_cooldown.as_secs(),
            "web_search": self.web_search,
            "cycle_interval_seconds": cycle_interval_seconds,
            "journal_prompt_entries": self.journal_prompt_entries,
            "compact_at_percent": self.compact_at_percent,
            "compact_at_tokens": self.compact_at_tokens,
            "max_daily_llm_cost_usd": decimal(self.max_daily_llm_cost_usd),
            "exit_plans": {
                "live": self.exit.live,
                "default_plan": self.exit.default_plan.map(|d| d.plan.to_string()),
                "default_plan_from_ms": self.exit.default_plan.map(|d| d.from_ms),
                "poll_seconds": self.exit.poll_seconds,
            },
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
            "market_evidence": {
                "enabled": self.market_evidence_enabled,
                "lookback_hours": self.market_evidence_lookback_hours,
                "max_symbols": self.market_evidence_max_symbols,
            },
            "trade_memory_limit": self.trade_memory_limit,
            "gcp2": {
                "enabled": self.gcp2_enabled,
                "base_url": self.gcp2_base_url,
            },
            "mdp": {
                "enabled": self.mdp_enabled,
                "model_path": self.mdp_model_path,
            },
            "biz": {
                "feed_path": self.biz_feed_path,
                "max_age_minutes": self.biz_max_age_minutes,
            },
            "decider": {
                "name": self.decider.as_str(),
                "rule": self.algo_rule.map(|rule| rule.weights.to_string()),
                "min_posters": self.algo_rule.map(|rule| rule.min_posters),
            },
            "scope": self.scope,
        })
    }

    /// Hard safety: every live arming flag must be set. Checked before any LLM call and
    /// again before every route execution.
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

fn positive_duration(seconds: f64, name: &str) -> Result<Duration> {
    if !(seconds.is_finite() && seconds > 0.0) {
        return Err(TraderError::config(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(Duration::from_secs_f64(seconds))
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
            ("BOT_SPOT_MODEL", "openai-codex/gpt-6.1-sol"),
            ("BOT_SPOT_DEFAULT_EXIT_PLAN", "deadline=120"),
            ("BOT_SPOT_DEFAULT_EXIT_PLAN_FROM", "2026-10-15T09:17:34Z"),
        ]))
        .unwrap();
        let snapshot = config.run_config(Some(1200));
        let text = snapshot.to_string();
        assert!(!text.contains("must-not-leak"), "{text}");
        assert_eq!(snapshot["model"], "openai-codex/gpt-6.1-sol");
        assert_eq!(snapshot["cycle_interval_seconds"], 1200);
        assert_eq!(snapshot["exit_plans"]["default_plan"], "deadline=120");
    }

    #[test]
    fn defaults_match_the_python_trader_and_agent_spec() {
        let config = TraderConfig::from_lookup(lookup(&[])).unwrap();
        assert!(config.use_testnet && config.dry_run && !config.enabled);
        assert_eq!(config.confirm_live_trading, "no");
        assert_eq!(config.command, "omp");
        assert_eq!(config.prompt_version, PROMPT_VERSION);
        assert_eq!(
            config.session_dir,
            PathBuf::from("data/bot-spot-session")
        );
        assert_eq!(
            config.audit_path,
            PathBuf::from("data/bot-spot-decisions.jsonl")
        );
        assert_eq!(config.timeout, Duration::from_secs(300));
        assert_eq!(config.max_route_legs, 3);
        assert_eq!(config.market_pick_mode, MarketPickMode::LiquidTopValue);
        assert_eq!(config.slippage_buffer_bps, Decimal::from(25));
        assert_eq!(config.scope.blocked_quotes, vec!["IDR".to_owned()]);
        assert!(config.scope.block_fiat_assets);
        assert_eq!(config.mode(), "DRY_RUN");
        assert_eq!(config.compact_at_percent, 70.0);
        assert_eq!(config.max_daily_llm_cost_usd, Decimal::ZERO);
    }

    #[test]
    fn removed_guardrail_vars_are_ignored() {
        let config = TraderConfig::from_lookup(lookup(&[
            ("BOT_SPOT_ALLOW_NEGATIVE_FRICTION_BPS", "not-a-number"),
            ("BOT_SPOT_DECISION_MODE", "freestyle"),
            ("BOT_SPOT_MAX_SUBMITTED_TRADES_PER_DAY", "-3"),
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
            [("BOT_SPOT_MARKET_EVIDENCE_TIMEFRAME", "4h")],
            [("BOT_SPOT_TIMEOUT_SECONDS", "0")],
            [("BOT_SPOT_THINKING", "loud")],
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
    fn fallback_models_parse_thinking_suffix_and_defaults() {
        let config = TraderConfig::from_lookup(lookup(&[])).unwrap();
        assert!(config.fallback_models.is_empty());
        assert_eq!(config.primary_timeout, Duration::from_secs(150));
        assert_eq!(config.fallback_cooldown, Duration::from_secs(1800));
        assert_eq!(config.brain_down_alert_cycles, 3);
        let chain = config.model_chain().unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "anthropic/claude-haiku-5-5");

        let config = TraderConfig::from_lookup(lookup(&[
            (
                "BOT_SPOT_FALLBACK_MODELS",
                " openai-codex/gpt-6.1-sol:LOW , google/gemini-x ",
            ),
            ("BOT_SPOT_PRIMARY_TIMEOUT_SECONDS", "90"),
            ("BOT_SPOT_FALLBACK_COOLDOWN_SECONDS", "0"),
            ("BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES", "5"),
        ]))
        .unwrap();
        assert_eq!(
            config.fallback_models,
            vec![
                ModelSpec {
                    provider: "openai-codex".into(),
                    model_id: "gpt-6.1-sol".into(),
                    thinking: ThinkingLevel::Low,
                },
                // Omitted thinking = the primary's.
                ModelSpec {
                    provider: "google".into(),
                    model_id: "gemini-x".into(),
                    thinking: ThinkingLevel::High,
                },
            ]
        );
        assert_eq!(config.primary_timeout, Duration::from_secs(90));
        assert_eq!(config.fallback_cooldown, Duration::ZERO);
        assert_eq!(config.brain_down_alert_cycles, 5);
        let chain = config.model_chain().unwrap();
        assert_eq!(chain[0].name(), "anthropic/claude-haiku-5-5");
        assert_eq!(chain[0].thinking, ThinkingLevel::High);
        assert_eq!(chain[1].name(), "openai-codex/gpt-6.1-sol");
    }

    #[test]
    fn fallback_models_reject_bad_entries() {
        for (fallbacks, extra) in [
            ("gpt-6.1-sol", None),
            ("openai-codex/", None),
            ("/gpt", None),
            ("openai-codex/gpt-6.1-sol:loud", None),
            ("openai-codex/gpt-6.1-sol:inherit", None),
            ("openai codex/gpt", None),
            // Equal to the primary (thinking does not make it a different model).
            ("anthropic/claude-haiku-5-5:low", None),
            (
                "openai-codex/gpt-6.1-sol,openai-codex/gpt-6.1-sol:low",
                None,
            ),
            // A bare primary pattern cannot be switched back to.
            (
                "openai-codex/gpt-6.1-sol",
                Some(("BOT_SPOT_MODEL", "haiku")),
            ),
        ] {
            let mut pairs = vec![("BOT_SPOT_FALLBACK_MODELS", fallbacks)];
            pairs.extend(extra);
            assert!(
                TraderConfig::from_lookup(lookup(&pairs)).is_err(),
                "{fallbacks} {extra:?}"
            );
        }
        for pairs in [
            [("BOT_SPOT_PRIMARY_TIMEOUT_SECONDS", "0")],
            [("BOT_SPOT_FALLBACK_COOLDOWN_SECONDS", "-1")],
            [("BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES", "-1")],
        ] {
            assert!(
                TraderConfig::from_lookup(lookup(&pairs)).is_err(),
                "{pairs:?}"
            );
        }
        // Without fallbacks a bare --model pattern stays valid (today's behavior).
        assert!(TraderConfig::from_lookup(lookup(&[("BOT_SPOT_MODEL", "haiku")])).is_ok());
    }

    #[test]
    fn the_algo_decider_needs_its_weights_and_the_llm_stays_the_default() {
        let default = TraderConfig::from_lookup(lookup(&[])).unwrap();
        assert_eq!((default.decider, default.algo_rule), (Decider::Llm, None));
        let err = TraderConfig::from_lookup(lookup(&[("BOT_SPOT_DECIDER", "algo")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("BOT_SPOT_ALGO_WEIGHTS"), "{err}");
        let algo = TraderConfig::from_lookup(lookup(&[
            ("BOT_SPOT_DECIDER", "ALGO"),
            ("BOT_SPOT_ALGO_WEIGHTS", "1,0.5,0.5,0,0.5,0"),
        ]))
        .unwrap();
        let rule = algo.algo_rule.unwrap();
        assert_eq!((algo.decider, rule.min_posters, rule.weights.stance), (Decider::Algo, 2, 0.5));
        assert_eq!(algo.run_config(None)["decider"]["name"], "algo");
        assert!(TraderConfig::from_lookup(lookup(&[("BOT_SPOT_DECIDER", "rules")])).is_err());
    }
}

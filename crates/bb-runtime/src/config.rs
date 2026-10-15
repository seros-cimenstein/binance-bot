//! Port of `src/config.py`: the full environment-backed runtime `Settings`.
//!
//! Env var names, aliases, defaults, normalisation and validation messages
//! match the Python dataclass so existing `.env` files keep working.

use std::str::FromStr;

use bb_binance::normalize_symbol;
use bb_core::Decimal;
use rust_decimal_macros::dec;

pub const DEFAULT_TRADING_SYMBOL: &str = "BTC/USDT";
pub const DEFAULT_ORDER_SIZE: Decimal = dec!(0.001);
pub const DEFAULT_ORDER_SIZE_PERCENT: Decimal = dec!(1.0);
pub const DEFAULT_TIMEFRAME: &str = "1m";
pub const DEFAULT_FAST_PERIOD: u32 = 20;
pub const DEFAULT_SLOW_PERIOD: u32 = 50;
pub const DEFAULT_USE_TESTNET: bool = true;
pub const DEFAULT_LOG_LEVEL: &str = "INFO";
pub const DEFAULT_FEED_QUEUE_MAXSIZE: usize = 1_000;
pub const DEFAULT_STRATEGY: &str = "demo-sma";
pub const DEFAULT_WS_ALERT_COOLDOWN_SECONDS: f64 = 300.0;
pub const DEFAULT_MAX_POSITION_SIZE_USD: Decimal = dec!(500.0);
pub const DEFAULT_DB_PATH: &str = "data/bot.db";

/// Python raised `RuntimeError(message)` for every configuration problem.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SettingsError(pub String);

impl SettingsError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// `ORDER_SIZE_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderSizeMode {
    Fixed,
    QuotePercent,
    EquityPercent,
    RiskPercent,
}

impl OrderSizeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::QuotePercent => "quote_percent",
            Self::EquityPercent => "equity_percent",
            Self::RiskPercent => "risk_percent",
        }
    }
}

impl FromStr for OrderSizeMode {
    type Err = SettingsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "fixed" => Ok(Self::Fixed),
            "quote_percent" => Ok(Self::QuotePercent),
            "equity_percent" => Ok(Self::EquityPercent),
            "risk_percent" => Ok(Self::RiskPercent),
            _ => Err(SettingsError::new(
                "ORDER_SIZE_MODE must be one of: fixed, quote_percent, equity_percent, risk_percent",
            )),
        }
    }
}

/// `MARKET_DATA_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarketDataMode {
    Polling,
    Websocket,
}

impl FromStr for MarketDataMode {
    type Err = SettingsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "polling" => Ok(Self::Polling),
            "websocket" => Ok(Self::Websocket),
            _ => Err(SettingsError::new(
                "MARKET_DATA_MODE must be either 'polling' or 'websocket'",
            )),
        }
    }
}

/// `ORDER_UPDATE_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderUpdateMode {
    Disabled,
    Websocket,
}

impl FromStr for OrderUpdateMode {
    type Err = SettingsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "disabled" => Ok(Self::Disabled),
            "websocket" => Ok(Self::Websocket),
            _ => Err(SettingsError::new(
                "ORDER_UPDATE_MODE must be either 'disabled' or 'websocket'",
            )),
        }
    }
}

/// Environment-backed runtime settings for the trading bot.
///
/// Build with [`Settings::defaults`] (Python dataclass defaults), adjust the
/// public fields, then call [`Settings::validate`] (the `__post_init__` port).
/// [`Settings::from_env`] does both.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub api_key: String,
    pub api_secret: String,
    pub trading_symbol: String,
    pub trading_symbols: Vec<String>,
    pub order_size: Decimal,
    pub order_size_mode: OrderSizeMode,
    pub order_size_percent: Decimal,
    pub min_order_notional_usd: Decimal,
    pub max_order_notional_usd: Decimal,
    pub min_balance_usdt: Decimal,
    pub max_total_exposure_usd: Option<Decimal>,
    pub allowed_leverage: Option<Decimal>,
    pub max_open_orders: Option<i64>,
    pub max_price_deviation_pct: Option<Decimal>,
    pub timeframe: String,
    pub fast_period: i64,
    pub slow_period: i64,
    pub use_testnet: bool,
    pub log_level: String,
    pub feed_queue_maxsize: i64,
    pub dry_run: bool,
    pub market_data_mode: MarketDataMode,
    pub strategy_name: String,
    pub order_update_mode: OrderUpdateMode,
    pub ws_alert_cooldown_seconds: f64,
    pub max_position_size_usd: Decimal,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub db_path: String,
    pub confirm_live_trading: String,
    pub api_symbol_whitelist: Vec<String>,
    pub api_asset_whitelist: Vec<String>,
}

impl Settings {
    /// Unvalidated settings carrying the Python defaults.
    pub fn defaults(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            trading_symbol: DEFAULT_TRADING_SYMBOL.to_owned(),
            trading_symbols: Vec::new(),
            order_size: DEFAULT_ORDER_SIZE,
            order_size_mode: OrderSizeMode::Fixed,
            order_size_percent: DEFAULT_ORDER_SIZE_PERCENT,
            min_order_notional_usd: Decimal::ZERO,
            max_order_notional_usd: Decimal::ZERO,
            min_balance_usdt: Decimal::ZERO,
            max_total_exposure_usd: None,
            allowed_leverage: None,
            max_open_orders: None,
            max_price_deviation_pct: None,
            timeframe: DEFAULT_TIMEFRAME.to_owned(),
            fast_period: i64::from(DEFAULT_FAST_PERIOD),
            slow_period: i64::from(DEFAULT_SLOW_PERIOD),
            use_testnet: DEFAULT_USE_TESTNET,
            log_level: DEFAULT_LOG_LEVEL.to_owned(),
            feed_queue_maxsize: DEFAULT_FEED_QUEUE_MAXSIZE as i64,
            dry_run: false,
            market_data_mode: MarketDataMode::Polling,
            strategy_name: DEFAULT_STRATEGY.to_owned(),
            order_update_mode: OrderUpdateMode::Disabled,
            ws_alert_cooldown_seconds: DEFAULT_WS_ALERT_COOLDOWN_SECONDS,
            max_position_size_usd: DEFAULT_MAX_POSITION_SIZE_USD,
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            db_path: DEFAULT_DB_PATH.to_owned(),
            confirm_live_trading: "no".to_owned(),
            api_symbol_whitelist: Vec::new(),
            api_asset_whitelist: Vec::new(),
        }
    }

    /// Validated defaults (`Settings(api_key=..., api_secret=...)`).
    pub fn new(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
    ) -> Result<Self, SettingsError> {
        Self::defaults(api_key, api_secret).validate()
    }

    /// Normalise and validate every field (port of `__post_init__`).
    pub fn validate(mut self) -> Result<Self, SettingsError> {
        self.api_key = non_blank(&self.api_key, "api_key")?;
        self.api_secret = non_blank(&self.api_secret, "api_secret")?;
        let first = symbol(&non_blank(&self.trading_symbol, "trading_symbol")?)?;
        let raw_symbols = if self.trading_symbols.is_empty() {
            vec![first]
        } else {
            std::mem::take(&mut self.trading_symbols)
        };
        let symbols = raw_symbols
            .iter()
            .map(|s| non_blank(s, "trading_symbols").and_then(|s| symbol(&s)))
            .collect::<Result<Vec<_>, _>>()?;
        let Some(first) = symbols.first().cloned() else {
            return Err(SettingsError::new("trading_symbols must not be empty"));
        };
        self.trading_symbols = symbols;
        self.trading_symbol = first;
        self.timeframe = non_blank(&self.timeframe, "timeframe")?;
        self.log_level = non_blank(&self.log_level, "log_level")?.to_uppercase();
        self.db_path = non_blank(&self.db_path, "db_path")?;
        self.strategy_name = non_blank(&self.strategy_name, "strategy_name")?;
        self.telegram_bot_token = self.telegram_bot_token.trim().to_owned();
        self.telegram_chat_id = self.telegram_chat_id.trim().to_owned();
        self.confirm_live_trading = self.confirm_live_trading.trim().to_lowercase();
        self.api_symbol_whitelist = self
            .api_symbol_whitelist
            .iter()
            .map(|s| non_blank(s, "api_symbol_whitelist").and_then(|s| symbol(&s)))
            .collect::<Result<_, _>>()?;
        self.api_asset_whitelist = self
            .api_asset_whitelist
            .iter()
            .map(|a| non_blank(a, "api_asset_whitelist").and_then(|a| asset_symbol(&a)))
            .collect::<Result<_, _>>()?;

        at_least(self.order_size, "order_size")?;
        at_least(self.order_size_percent, "order_size_percent")?;
        if self.order_size_mode != OrderSizeMode::Fixed && self.order_size_percent <= Decimal::ZERO
        {
            return Err(SettingsError::new(
                "ORDER_SIZE_PERCENT must be greater than 0",
            ));
        }
        at_least(self.min_order_notional_usd, "min_order_notional_usd")?;
        at_least(self.max_order_notional_usd, "max_order_notional_usd")?;
        at_least(self.min_balance_usdt, "min_balance_usdt")?;
        optional_greater(self.max_total_exposure_usd, "max_total_exposure_usd")?;
        optional_greater(self.allowed_leverage, "allowed_leverage")?;
        if self.max_open_orders.is_some_and(|n| n <= 0) {
            return Err(SettingsError::new(
                "max_open_orders must be a positive integer",
            ));
        }
        if let Some(value) = self.max_price_deviation_pct {
            at_least(value, "max_price_deviation_pct")?;
        }
        if self.max_order_notional_usd > Decimal::ZERO
            && self.max_order_notional_usd < self.min_order_notional_usd
        {
            return Err(SettingsError::new(
                "MAX_ORDER_NOTIONAL_USD must be greater than or equal to MIN_ORDER_NOTIONAL_USD",
            ));
        }
        if self.fast_period <= 0 {
            return Err(SettingsError::new("fast_period must be a positive integer"));
        }
        if self.slow_period <= 0 {
            return Err(SettingsError::new("slow_period must be a positive integer"));
        }
        if self.fast_period >= self.slow_period {
            return Err(SettingsError::new(
                "SMA_FAST_PERIOD must be less than SMA_SLOW_PERIOD",
            ));
        }
        if !self.ws_alert_cooldown_seconds.is_finite() {
            return Err(SettingsError::new(
                "ws_alert_cooldown_seconds must be a finite number",
            ));
        }
        if self.ws_alert_cooldown_seconds < 0.0 {
            return Err(SettingsError::new(
                "ws_alert_cooldown_seconds must be at least 0",
            ));
        }
        if self.feed_queue_maxsize < 0 {
            return Err(SettingsError::new(
                "feed_queue_maxsize must be a non-negative integer",
            ));
        }
        greater(self.max_position_size_usd, "max_position_size_usd")?;
        Ok(self)
    }

    /// Build settings from the process environment (the caller loads `.env`).
    pub fn from_env() -> Result<Self, SettingsError> {
        Self::from_source(&|name| std::env::var(name).ok())
    }

    /// Build settings from any variable lookup (`from_env` uses the process
    /// environment; tests pass a map).
    pub fn from_source(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, SettingsError> {
        let env = Env(lookup);
        let api_key = env.str(&["BINANCE_API_KEY"], "");
        if api_key.is_empty() {
            return Err(SettingsError::new(
                "missing required environment variable: BINANCE_API_KEY",
            ));
        }
        let api_secret = env.str(&["BINANCE_API_SECRET", "BINANCE_SECRET_KEY"], "");
        if api_secret.is_empty() {
            return Err(SettingsError::new(
                "missing required environment variable: BINANCE_API_SECRET or BINANCE_SECRET_KEY",
            ));
        }
        let settings = Self {
            api_key,
            api_secret,
            trading_symbol: env.str(&["TRADING_SYMBOL"], DEFAULT_TRADING_SYMBOL),
            trading_symbols: env.list("TRADING_SYMBOLS", "symbol")?,
            order_size: env.decimal(&["ORDER_SIZE", "TRADE_QUANTITY"], DEFAULT_ORDER_SIZE)?,
            order_size_mode: env.str(&["ORDER_SIZE_MODE"], "fixed").parse()?,
            order_size_percent: env.decimal(&["ORDER_SIZE_PERCENT"], DEFAULT_ORDER_SIZE_PERCENT)?,
            min_order_notional_usd: env.decimal(&["MIN_ORDER_NOTIONAL_USD"], Decimal::ZERO)?,
            max_order_notional_usd: env.decimal(&["MAX_ORDER_NOTIONAL_USD"], Decimal::ZERO)?,
            min_balance_usdt: env.decimal(&["MIN_BALANCE_USDT"], Decimal::ZERO)?,
            max_total_exposure_usd: env.optional_decimal("MAX_TOTAL_EXPOSURE_USD")?,
            allowed_leverage: env.optional_decimal("ALLOWED_LEVERAGE")?,
            max_open_orders: env.optional_int("MAX_OPEN_ORDERS")?,
            max_price_deviation_pct: env.optional_decimal("MAX_PRICE_DEVIATION_PCT")?,
            timeframe: env.str(&["TRADING_TIMEFRAME", "TIMEFRAME"], DEFAULT_TIMEFRAME),
            fast_period: env.int(&["SMA_FAST_PERIOD"], i64::from(DEFAULT_FAST_PERIOD))?,
            slow_period: env.int(&["SMA_SLOW_PERIOD"], i64::from(DEFAULT_SLOW_PERIOD))?,
            use_testnet: env.boolean(&["USE_TESTNET"], DEFAULT_USE_TESTNET)?,
            log_level: env.str(&["LOG_LEVEL"], DEFAULT_LOG_LEVEL).to_uppercase(),
            feed_queue_maxsize: env
                .int(&["FEED_QUEUE_MAXSIZE"], DEFAULT_FEED_QUEUE_MAXSIZE as i64)?,
            dry_run: env.boolean(&["DRY_RUN"], false)?,
            market_data_mode: env.str(&["MARKET_DATA_MODE"], "polling").parse()?,
            strategy_name: env.str(&["STRATEGY"], DEFAULT_STRATEGY),
            order_update_mode: env.str(&["ORDER_UPDATE_MODE"], "disabled").parse()?,
            ws_alert_cooldown_seconds: env.float(
                &["WS_ALERT_COOLDOWN_SECONDS"],
                DEFAULT_WS_ALERT_COOLDOWN_SECONDS,
            )?,
            max_position_size_usd: env
                .decimal(&["MAX_POSITION_SIZE_USD"], DEFAULT_MAX_POSITION_SIZE_USD)?,
            telegram_bot_token: env.str(&["TELEGRAM_BOT_TOKEN"], ""),
            telegram_chat_id: env.str(&["TELEGRAM_CHAT_ID"], ""),
            db_path: env.str(&["DB_PATH"], DEFAULT_DB_PATH),
            confirm_live_trading: env.str(&["CONFIRM_LIVE_TRADING"], "no").to_lowercase(),
            api_symbol_whitelist: env.list("API_SYMBOL_WHITELIST", "symbol")?,
            api_asset_whitelist: env.list("API_ASSET_WHITELIST", "asset")?,
        };
        settings.validate()
    }

    /// A copy scoped to one trading symbol.
    pub fn for_symbol(&self, symbol_name: &str) -> Result<Self, SettingsError> {
        let normalized = symbol(symbol_name)?;
        Ok(Self {
            trading_symbol: normalized.clone(),
            trading_symbols: vec![normalized],
            ..self.clone()
        })
    }

    /// `DRY_RUN` / `LIVE` / `testnet` label used in logs and alerts.
    pub fn mode(&self) -> &'static str {
        if self.dry_run {
            "DRY_RUN"
        } else if !self.use_testnet {
            "LIVE"
        } else {
            "testnet"
        }
    }
}

fn symbol(value: &str) -> Result<String, SettingsError> {
    normalize_symbol(value).map_err(|err| SettingsError::new(err.to_string()))
}

fn non_blank(value: &str, field: &str) -> Result<String, SettingsError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SettingsError::new(format!("{field} must not be blank")));
    }
    Ok(trimmed.to_owned())
}

fn asset_symbol(value: &str) -> Result<String, SettingsError> {
    let asset = value.trim().to_uppercase();
    let compact: String = asset.chars().filter(|c| *c != '-' && *c != '_').collect();
    if compact.is_empty() || !compact.chars().all(char::is_alphanumeric) {
        return Err(SettingsError::new(
            "asset whitelist values must be asset symbols",
        ));
    }
    Ok(asset)
}

fn at_least(value: Decimal, field: &str) -> Result<(), SettingsError> {
    if value < Decimal::ZERO {
        return Err(SettingsError::new(format!("{field} must be at least 0")));
    }
    Ok(())
}

fn greater(value: Decimal, field: &str) -> Result<(), SettingsError> {
    if value <= Decimal::ZERO {
        return Err(SettingsError::new(format!(
            "{field} must be greater than 0"
        )));
    }
    Ok(())
}

fn optional_greater(value: Option<Decimal>, field: &str) -> Result<(), SettingsError> {
    value.map_or(Ok(()), |v| greater(v, field))
}

fn joined(names: &[&str]) -> String {
    names.join(" or ")
}

pub(crate) fn parse_decimal(value: &str) -> Option<Decimal> {
    let value = value.trim();
    Decimal::from_str_exact(value)
        .or_else(|_| Decimal::from_scientific(value))
        .or_else(|_| Decimal::from_str(value))
        .ok()
}

/// Python `_bool_env` values (also accepts `t`/`f`).
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Typed readers over a variable lookup (Python `_env`, `_float_env`, ...).
/// Blank values count as unset; values are trimmed.
struct Env<'a>(&'a dyn Fn(&str) -> Option<String>);

impl Env<'_> {
    fn raw(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| {
            (self.0)(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
    }

    fn str(&self, names: &[&str], default: &str) -> String {
        self.raw(names).unwrap_or_else(|| default.to_owned())
    }

    fn decimal(&self, names: &[&str], default: Decimal) -> Result<Decimal, SettingsError> {
        match self.raw(names) {
            None => Ok(default),
            Some(raw) => parse_decimal(&raw)
                .ok_or_else(|| SettingsError::new(format!("{} must be a number", joined(names)))),
        }
    }

    fn optional_decimal(&self, name: &str) -> Result<Option<Decimal>, SettingsError> {
        self.raw(&[name])
            .map(|raw| {
                parse_decimal(&raw)
                    .ok_or_else(|| SettingsError::new(format!("{name} must be a number")))
            })
            .transpose()
    }

    fn float(&self, names: &[&str], default: f64) -> Result<f64, SettingsError> {
        match self.raw(names) {
            None => Ok(default),
            Some(raw) => raw
                .parse::<f64>()
                .map_err(|_| SettingsError::new(format!("{} must be a number", joined(names)))),
        }
    }

    fn int(&self, names: &[&str], default: i64) -> Result<i64, SettingsError> {
        match self.raw(names) {
            None => Ok(default),
            Some(raw) => raw
                .parse::<i64>()
                .map_err(|_| SettingsError::new(format!("{} must be an integer", joined(names)))),
        }
    }

    fn optional_int(&self, name: &str) -> Result<Option<i64>, SettingsError> {
        self.raw(&[name])
            .map(|raw| {
                raw.parse::<i64>()
                    .map_err(|_| SettingsError::new(format!("{name} must be an integer")))
            })
            .transpose()
    }

    fn boolean(&self, names: &[&str], default: bool) -> Result<bool, SettingsError> {
        match self.raw(names) {
            None => Ok(default),
            Some(raw) => parse_bool(&raw)
                .ok_or_else(|| SettingsError::new(format!("{} must be a boolean", joined(names)))),
        }
    }

    /// Comma-separated list; blanks dropped. Unset or blank → empty; only
    /// separators → error (Python `_symbols_env` / `_assets_env`).
    fn list(&self, name: &str, what: &str) -> Result<Vec<String>, SettingsError> {
        let Some(raw) = self.raw(&[name]) else {
            return Ok(Vec::new());
        };
        let items: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
        if items.is_empty() {
            return Err(SettingsError::new(format!(
                "{name} must contain at least one {what}"
            )));
        }
        Ok(items)
    }
}

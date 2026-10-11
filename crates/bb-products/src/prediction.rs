//! Prediction-market parsing and local stake risk modelling
//! (port of `src/prediction.py`). Market data comes from a local JSON payload;
//! no prediction write endpoint exists.

use std::path::Path;

use bb_binance::normalize_symbol;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde_json::Value;

use crate::common::{self, fmt_dec, jfloat, jtext, num, num_opt, optional_positive, yes_no};
use crate::settings::Settings;

pub const AVAILABLE_STATUSES: [&str; 3] = ["OPEN", "TRADING", "LIVE"];
pub const WRITE_ENDPOINTS_BLOCKED: [&str; 6] = [
    "predictionPostOrder",
    "predictionPostBet",
    "predictionPostClaim",
    "predictionPostSettle",
    "bapiPredictionPostOrder",
    "bapiPredictionPostBet",
];

#[derive(Debug, thiserror::Error)]
pub enum PredictionModelError {
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

fn invalid(msg: impl Into<String>) -> PredictionModelError {
    PredictionModelError::Invalid(msg.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "UPPERCASE")]
#[value(rename_all = "UPPER")]
pub enum Direction {
    Up,
    Down,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "UP",
            Self::Down => "DOWN",
        }
    }
}

/// One parsed prediction market or round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionMarket {
    pub market_id: String,
    pub symbol: String,
    pub underlying: String,
    pub quote_asset: String,
    pub status: String,
    pub round_id: String,
    pub lock_time: Option<i64>,
    pub close_time: Option<i64>,
    pub settlement_time: Option<i64>,
    #[serde(with = "num_opt")]
    pub lock_price: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub mark_price: Option<Decimal>,
    #[serde(with = "num")]
    pub pool_up_usdt: Decimal,
    #[serde(with = "num")]
    pub pool_down_usdt: Decimal,
    #[serde(with = "num")]
    pub total_pool_usdt: Decimal,
    #[serde(with = "num_opt")]
    pub payout_up: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub payout_down: Option<Decimal>,
    pub source: String,
}

impl PredictionMarket {
    pub fn seconds_to_lock(&self, now: i64) -> Option<i64> {
        self.lock_time.map(|t| t - now)
    }

    pub fn seconds_to_settlement(&self, now: i64) -> Option<i64> {
        self.settlement_time.or(self.close_time).map(|t| t - now)
    }

    pub fn is_available(&self, now: i64) -> bool {
        AVAILABLE_STATUSES.contains(&self.status.as_str())
            && self.seconds_to_lock(now).is_none_or(|s| s > 0)
    }

    fn pool_imbalance_percent(&self) -> Option<Decimal> {
        let total = self.total_pool_usdt;
        (total > Decimal::ZERO)
            .then(|| (self.pool_up_usdt - self.pool_down_usdt).abs() / total * dec!(100))
    }
}

/// Read-only prediction market discovery result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionDiscovery {
    pub timestamp: String,
    pub markets: Vec<PredictionMarket>,
    pub live_trading_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

impl PredictionDiscovery {
    /// Look up by market id, round id or symbol.
    pub fn market(&self, key: &str) -> Option<&PredictionMarket> {
        let normalized = market_key(key);
        let symbol = prediction_symbol(key).ok();
        self.markets.iter().find(|m| {
            market_key(&m.market_id) == normalized
                || market_key(&m.round_id) == normalized
                || symbol.as_deref() == Some(m.symbol.as_str())
        })
    }
}

/// Conservative local limits for prediction dry-run plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionRiskLimits {
    #[serde(with = "num")]
    pub max_stake_usdt: Decimal,
    #[serde(with = "num")]
    pub max_total_stake_usdt: Decimal,
    #[serde(with = "num")]
    pub max_stake_bankroll_percent: Decimal,
    pub min_seconds_to_lock: i64,
    pub max_seconds_to_settlement: i64,
    #[serde(with = "num")]
    pub min_total_pool_usdt: Decimal,
    #[serde(with = "num")]
    pub max_pool_imbalance_percent: Decimal,
    #[serde(with = "num")]
    pub max_settlement_risk_score: Decimal,
    #[serde(with = "num")]
    pub max_drawdown_percent: Decimal,
    pub require_available_market: bool,
    pub emergency_stop: bool,
}

impl Default for PredictionRiskLimits {
    fn default() -> Self {
        Self {
            max_stake_usdt: dec!(5.0),
            max_total_stake_usdt: dec!(10.0),
            max_stake_bankroll_percent: dec!(5.0),
            min_seconds_to_lock: 60,
            max_seconds_to_settlement: 3600,
            min_total_pool_usdt: dec!(25.0),
            max_pool_imbalance_percent: dec!(80.0),
            max_settlement_risk_score: dec!(50.0),
            max_drawdown_percent: dec!(10.0),
            require_available_market: true,
            emergency_stop: false,
        }
    }
}

impl PredictionRiskLimits {
    fn validate(&self) -> Result<(), PredictionModelError> {
        positive(self.max_stake_usdt, "max_stake_usdt")?;
        positive(self.max_total_stake_usdt, "max_total_stake_usdt")?;
        positive(
            self.max_stake_bankroll_percent,
            "max_stake_bankroll_percent",
        )?;
        non_negative(
            Decimal::from(self.min_seconds_to_lock),
            "min_seconds_to_lock",
        )?;
        positive(
            Decimal::from(self.max_seconds_to_settlement),
            "max_seconds_to_settlement",
        )?;
        non_negative(self.min_total_pool_usdt, "min_total_pool_usdt")?;
        positive(
            self.max_pool_imbalance_percent,
            "max_pool_imbalance_percent",
        )?;
        positive(self.max_settlement_risk_score, "max_settlement_risk_score")?;
        non_negative(self.max_drawdown_percent, "max_drawdown_percent")?;
        Ok(())
    }
}

/// Requested prediction stake modelling inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictionIntent {
    pub market: String,
    pub direction: Option<Direction>,
    pub stake_usdt: Option<Decimal>,
    pub bankroll_usdt: Option<Decimal>,
    pub current_open_stake_usdt: Decimal,
    pub current_drawdown_percent: Decimal,
    pub emergency_stop: bool,
    /// Unix seconds; defaults to the current time.
    pub now: Option<i64>,
}

/// Dry-run prediction stake and risk result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionPlan {
    pub timestamp: String,
    pub market_id: String,
    pub symbol: String,
    pub round_id: String,
    pub direction: Direction,
    #[serde(with = "num")]
    pub stake_usdt: Decimal,
    #[serde(with = "num")]
    pub bankroll_usdt: Decimal,
    #[serde(with = "num")]
    pub current_open_stake_usdt: Decimal,
    #[serde(with = "num")]
    pub projected_open_stake_usdt: Decimal,
    #[serde(with = "num")]
    pub stake_bankroll_percent: Decimal,
    pub status: String,
    pub seconds_to_lock: Option<i64>,
    pub seconds_to_settlement: Option<i64>,
    #[serde(with = "num")]
    pub total_pool_usdt: Decimal,
    #[serde(with = "num_opt")]
    pub pool_imbalance_percent: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub payout_estimate: Option<Decimal>,
    #[serde(with = "num")]
    pub settlement_risk_score: Decimal,
    pub emergency_stop_active: bool,
    pub market_available: bool,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub market: PredictionMarket,
    pub risk_limits: PredictionRiskLimits,
}

/// Summary audit block of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionAudit {
    pub timestamp: String,
    pub market_data_available: bool,
    pub markets_discovered: usize,
    pub live_trading_enabled: bool,
    pub write_endpoints_used: Vec<String>,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
}

/// Full prediction dry-run report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionDryRunReport {
    pub timestamp: String,
    pub mode: String,
    pub discovery: PredictionDiscovery,
    pub plan: Option<PredictionPlan>,
    pub audit: PredictionAudit,
    pub live_trading_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

/// Parse markets from a payload; fails when nothing matches.
pub fn discover_prediction_markets(
    payload: &Value,
    symbols: &[String],
) -> Result<PredictionDiscovery, PredictionModelError> {
    let markets = parse_prediction_markets(payload, symbols)?;
    if markets.is_empty() {
        return Err(invalid(
            "prediction market data did not include any markets",
        ));
    }
    Ok(PredictionDiscovery {
        timestamp: common::utc_timestamp(),
        markets,
        live_trading_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

fn first_present<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|k| item.get(*k))
        .find(|v| !v.is_null() && v.as_str() != Some(""))
}

/// Parse prediction market data from Binance-like or local JSON payloads.
pub fn parse_prediction_markets(
    raw: &Value,
    symbols: &[String],
) -> Result<Vec<PredictionMarket>, PredictionModelError> {
    let wanted = symbols
        .iter()
        .map(|s| prediction_symbol(s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut markets = Vec::new();
    for item in payload_items(raw) {
        let Some(symbol_value) =
            first_present(item, &["symbol", "pair", "underlyingSymbol", "asset"])
        else {
            continue;
        };
        let symbol = prediction_symbol(&jtext(Some(symbol_value)))?;
        if !wanted.is_empty() && !wanted.contains(&symbol) {
            continue;
        }
        let text =
            |keys: &[&str]| first_present(item, keys).map(|v| jtext(Some(v)).trim().to_owned());
        let num = |keys: &[&str]| jfloat(first_present(item, keys));
        let market_id =
            text(&["marketId", "id", "roundId", "epoch"]).unwrap_or_else(|| symbol.clone());
        let round_id = text(&["roundId", "epoch", "round"]).unwrap_or_else(|| market_id.clone());
        let pool_up = num(&["poolUp", "bullAmount", "upPool", "longPool"]);
        let pool_down = num(&["poolDown", "bearAmount", "downPool", "shortPool"]);
        let mut total = num(&["totalPool", "totalAmount", "pool"]);
        if total <= Decimal::ZERO {
            total = pool_up + pool_down;
        }
        markets.push(PredictionMarket {
            market_id,
            underlying: asset(
                &text(&["underlying", "baseAsset"]).unwrap_or_else(|| symbol.clone()),
            )?,
            quote_asset: asset(
                &text(&["quoteAsset", "marginAsset"]).unwrap_or_else(|| "USDT".into()),
            )?,
            status: text(&["status", "state"])
                .unwrap_or_else(|| "UNKNOWN".into())
                .to_uppercase(),
            round_id,
            lock_time: timestamp(first_present(
                item,
                &["lockTime", "lockTimestamp", "lockDate", "startTime"],
            )),
            close_time: timestamp(first_present(
                item,
                &["closeTime", "closeTimestamp", "endTime"],
            )),
            settlement_time: timestamp(first_present(
                item,
                &["settlementTime", "settleTime", "settleTimestamp"],
            )),
            lock_price: optional_positive(first_present(item, &["lockPrice", "lockedPrice"])),
            mark_price: optional_positive(first_present(
                item,
                &["markPrice", "currentPrice", "price"],
            )),
            pool_up_usdt: pool_up,
            pool_down_usdt: pool_down,
            total_pool_usdt: total,
            payout_up: optional_positive(first_present(
                item,
                &["payoutUp", "bullPayout", "upPayout"],
            )),
            payout_down: optional_positive(first_present(
                item,
                &["payoutDown", "bearPayout", "downPayout"],
            )),
            source: text(&["source"]).unwrap_or_else(|| "payload".into()),
            symbol,
        });
    }
    Ok(markets)
}

/// Build a dry-run prediction stake plan without submitting anything.
pub fn build_prediction_plan(
    discovery: &PredictionDiscovery,
    intent: &PredictionIntent,
    limits: &PredictionRiskLimits,
) -> Result<PredictionPlan, PredictionModelError> {
    limits.validate()?;
    let now = intent
        .now
        .unwrap_or_else(|| bb_core::time::now().timestamp());
    let market = discovery
        .market(&intent.market)
        .ok_or_else(|| invalid(format!("prediction market not found for {}", intent.market)))?;
    let direction = intent
        .direction
        .ok_or_else(|| invalid("direction must be UP or DOWN"))?;
    let stake = positive(
        intent
            .stake_usdt
            .ok_or_else(|| invalid("stake_usdt must be a number"))?,
        "stake_usdt",
    )?;
    let bankroll = positive(
        intent
            .bankroll_usdt
            .ok_or_else(|| invalid("bankroll_usdt must be a number"))?,
        "bankroll_usdt",
    )?;
    let open_stake = non_negative(intent.current_open_stake_usdt, "current_open_stake_usdt")?;
    let drawdown = non_negative(intent.current_drawdown_percent, "current_drawdown_percent")?;
    let projected = open_stake + stake;
    let stake_percent = stake / bankroll * dec!(100);
    let to_lock = market.seconds_to_lock(now);
    let to_settlement = market.seconds_to_settlement(now);
    let imbalance = market.pool_imbalance_percent();
    let payout = match direction {
        Direction::Up => market.payout_up,
        Direction::Down => market.payout_down,
    };
    let risk = settlement_risk_score(market, to_lock, to_settlement, imbalance);
    let emergency = intent.emergency_stop || limits.emergency_stop;
    let available = market.is_available(now);

    let mut rejections: Vec<&str> = Vec::new();
    let mut warnings: Vec<&str> = Vec::new();
    if emergency {
        rejections.push("emergency stop is active");
    }
    if limits.require_available_market && !available {
        rejections.push("prediction market is not available");
    }
    if stake > limits.max_stake_usdt {
        rejections.push("stake exceeds max_stake_usdt");
    }
    if projected > limits.max_total_stake_usdt {
        rejections.push("projected open stake exceeds max_total_stake_usdt");
    }
    if stake_percent > limits.max_stake_bankroll_percent {
        rejections.push("stake exceeds bankroll percent limit");
    }
    if drawdown > limits.max_drawdown_percent {
        rejections.push("current drawdown exceeds configured maximum");
    }
    match to_lock {
        None => rejections.push("market lock time is unavailable"),
        Some(s) if s < limits.min_seconds_to_lock => rejections.push("market is too close to lock"),
        Some(_) => {}
    }
    match to_settlement {
        None => rejections.push("market settlement time is unavailable"),
        Some(s) if s <= 0 => rejections.push("market settlement window has elapsed"),
        Some(s) if s > limits.max_seconds_to_settlement => {
            rejections.push("settlement window exceeds configured maximum");
        }
        Some(_) => {}
    }
    if market.total_pool_usdt < limits.min_total_pool_usdt {
        rejections.push("market liquidity is below minimum");
    }
    if imbalance.is_some_and(|i| i > limits.max_pool_imbalance_percent) {
        rejections.push("market pool imbalance exceeds configured maximum");
    }
    if risk > limits.max_settlement_risk_score {
        rejections.push("settlement risk exceeds configured maximum");
    }
    if payout.is_none() {
        warnings.push("payout estimate is unavailable");
    }

    Ok(PredictionPlan {
        timestamp: common::utc_timestamp(),
        market_id: market.market_id.clone(),
        symbol: market.symbol.clone(),
        round_id: market.round_id.clone(),
        direction,
        stake_usdt: stake,
        bankroll_usdt: bankroll,
        current_open_stake_usdt: open_stake,
        projected_open_stake_usdt: projected,
        stake_bankroll_percent: stake_percent,
        status: market.status.clone(),
        seconds_to_lock: to_lock,
        seconds_to_settlement: to_settlement,
        total_pool_usdt: market.total_pool_usdt,
        pool_imbalance_percent: imbalance,
        payout_estimate: payout,
        settlement_risk_score: risk,
        emergency_stop_active: emergency,
        market_available: available,
        allowed: rejections.is_empty(),
        rejection_reasons: rejections.into_iter().map(str::to_owned).collect(),
        warnings: warnings.into_iter().map(str::to_owned).collect(),
        market: market.clone(),
        risk_limits: limits.clone(),
    })
}

/// Build a full prediction dry-run report from local market data.
pub fn build_prediction_dry_run_report(
    settings: &Settings,
    payload: &Value,
    symbols: &[String],
    intent: Option<&PredictionIntent>,
    limits: &PredictionRiskLimits,
) -> Result<PredictionDryRunReport, PredictionModelError> {
    let discovery = discover_prediction_markets(payload, symbols)?;
    let plan = intent
        .map(|i| build_prediction_plan(&discovery, i, limits))
        .transpose()?;
    let audit = PredictionAudit {
        timestamp: common::utc_timestamp(),
        market_data_available: !discovery.markets.is_empty(),
        markets_discovered: discovery.markets.len(),
        live_trading_enabled: false,
        write_endpoints_used: Vec::new(),
        allowed: plan
            .as_ref()
            .map_or(!discovery.markets.is_empty(), |p| p.allowed),
        rejection_reasons: plan
            .as_ref()
            .map(|p| p.rejection_reasons.clone())
            .unwrap_or_default(),
    };
    Ok(PredictionDryRunReport {
        timestamp: common::utc_timestamp(),
        mode: common::mode(settings).to_owned(),
        discovery,
        plan,
        audit,
        live_trading_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

/// Fail closed for any attempted live prediction activation.
pub fn assert_prediction_live_disabled(
    confirm_live_prediction: bool,
) -> Result<(), PredictionModelError> {
    if confirm_live_prediction {
        return Err(invalid(
            "live prediction activation is not implemented; Phase 28 is dry-run only",
        ));
    }
    Ok(())
}

/// Load the `--market-data-file` JSON object or array.
pub fn load_json_payload(path: Option<&Path>) -> Result<Value, PredictionModelError> {
    let path = path.ok_or_else(|| invalid("local --market-data-file is required"))?;
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| PredictionModelError::Io {
        path: display.clone(),
        source,
    })?;
    let payload: Value =
        serde_json::from_str(&text).map_err(|source| PredictionModelError::Json {
            path: display,
            source,
        })?;
    if !payload.is_object() && !payload.is_array() {
        return Err(invalid("prediction market JSON must be an object or array"));
    }
    Ok(payload)
}

/// Concise human-readable prediction dry-run report.
pub fn render_prediction_report(report: &PredictionDryRunReport) -> String {
    let audit = &report.audit;
    let mut lines = vec![
        format!("Mode: {}", report.mode),
        "Live prediction trading: disabled".to_owned(),
        "Write endpoints used: none".to_owned(),
        format!("Markets discovered: {}", report.discovery.markets.len()),
        "Audit:".to_owned(),
        format!(
            "  - market data available: {}",
            yes_no(audit.market_data_available)
        ),
        format!("  - allowed: {}", yes_no(audit.allowed)),
    ];
    if !audit.rejection_reasons.is_empty() {
        lines.push(format!(
            "  - rejection reasons: {}",
            audit.rejection_reasons.join(", ")
        ));
    }
    if let Some(plan) = &report.plan {
        lines.extend([
            "Prediction plan:".to_owned(),
            format!("  - market: {}", plan.market_id),
            format!("  - symbol: {}", plan.symbol),
            format!("  - direction: {}", plan.direction.as_str()),
            format!("  - stake USDT: {}", fmt_dec(plan.stake_usdt)),
            format!(
                "  - seconds to lock: {}",
                plan.seconds_to_lock
                    .map_or_else(|| "None".to_owned(), |s| s.to_string())
            ),
            format!(
                "  - settlement risk score: {}",
                fmt_dec(plan.settlement_risk_score)
            ),
            format!("  - emergency stop: {}", yes_no(plan.emergency_stop_active)),
            format!("  - allowed: {}", yes_no(plan.allowed)),
        ]);
        if !plan.rejection_reasons.is_empty() {
            lines.push(format!(
                "  - rejection reasons: {}",
                plan.rejection_reasons.join(", ")
            ));
        }
        if !plan.warnings.is_empty() {
            lines.push(format!("  - warnings: {}", plan.warnings.join(", ")));
        }
    }
    lines.join("\n")
}

fn payload_items(raw: &Value) -> Vec<&Value> {
    match raw {
        Value::Object(map) => {
            for key in ["markets", "data", "items", "rows", "list"] {
                if let Some(nested) = map.get(key).and_then(Value::as_array) {
                    return nested.iter().filter(|i| i.is_object()).collect();
                }
            }
            vec![raw]
        }
        Value::Array(items) => items.iter().filter(|i| i.is_object()).collect(),
        _ => Vec::new(),
    }
}

fn settlement_risk_score(
    market: &PredictionMarket,
    to_lock: Option<i64>,
    to_settlement: Option<i64>,
    imbalance: Option<Decimal>,
) -> Decimal {
    let mut score = Decimal::ZERO;
    score += match to_lock {
        None => dec!(30),
        Some(s) if s < 60 => dec!(25),
        Some(s) if s < 300 => dec!(10),
        Some(_) => Decimal::ZERO,
    };
    score += match to_settlement {
        None => dec!(30),
        Some(s) if s > 3600 => dec!(15),
        Some(s) if s <= 0 => dec!(50),
        Some(_) => Decimal::ZERO,
    };
    if market.lock_price.is_none() || market.mark_price.is_none() {
        score += dec!(10);
    }
    if market.total_pool_usdt <= Decimal::ZERO {
        score += dec!(25);
    }
    if let Some(imbalance) = imbalance {
        score += (imbalance / dec!(4)).min(dec!(25));
    }
    score.min(dec!(100))
}

/// Unix seconds from a number (ms auto-detected) or an ISO string.
fn timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Value::String(s) = value {
        let trimmed = s.trim();
        let numeric = trimmed.replacen('.', "", 1);
        if numeric.is_empty() || !numeric.chars().all(|c| c.is_ascii_digit()) {
            return common::parse_iso_timestamp(trimmed).map(|t| t.timestamp());
        }
    }
    let mut number = jfloat(Some(value));
    if number <= Decimal::ZERO {
        return None;
    }
    if number > dec!(10_000_000_000) {
        number /= dec!(1000);
    }
    i64::try_from(number.trunc()).ok()
}

fn market_key(value: &str) -> String {
    common::exchange_symbol(value)
}

fn prediction_symbol(symbol: &str) -> Result<String, PredictionModelError> {
    let text = symbol.trim();
    if text.is_empty() {
        return Err(invalid("symbol must not be blank"));
    }
    let normalized = normalize_symbol(text).unwrap_or_else(|_| text.to_owned());
    Ok(common::exchange_symbol(&normalized))
}

fn asset(value: &str) -> Result<String, PredictionModelError> {
    let text = value.trim().to_uppercase();
    if text.is_empty() {
        return Err(invalid("asset must not be blank"));
    }
    Ok(text)
}

fn positive(value: Decimal, field: &str) -> Result<Decimal, PredictionModelError> {
    if value <= Decimal::ZERO {
        return Err(invalid(format!("{field} must be greater than zero")));
    }
    Ok(value)
}

fn non_negative(value: Decimal, field: &str) -> Result<Decimal, PredictionModelError> {
    if value < Decimal::ZERO {
        return Err(invalid(format!("{field} must be at least zero")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn payload() -> Value {
        json!({"markets": [
            {"marketId": "btc-5m", "symbol": "BTCUSDT", "status": "open", "roundId": "42",
             "lockTime": (NOW + 600) * 1000, "closeTime": NOW + 900, "lockPrice": "50000", "markPrice": "50010",
             "poolUp": "60", "poolDown": "40", "payoutUp": "1.6", "payoutDown": "2.4"},
            {"marketId": "eth-5m", "symbol": "ETH/USDT", "status": "LOCKED", "lockTime": NOW - 10, "closeTime": NOW + 100,
             "poolUp": "1", "poolDown": "0"}
        ]})
    }

    fn intent(market: &str, stake: Decimal) -> PredictionIntent {
        PredictionIntent {
            market: market.into(),
            direction: Some(Direction::Up),
            stake_usdt: Some(stake),
            bankroll_usdt: Some(dec!(100)),
            current_open_stake_usdt: Decimal::ZERO,
            current_drawdown_percent: Decimal::ZERO,
            emergency_stop: false,
            now: Some(NOW),
        }
    }

    #[test]
    fn parses_markets_and_symbol_filters() {
        let markets = parse_prediction_markets(&payload(), &[]).unwrap();
        assert_eq!(markets.len(), 2);
        assert_eq!(markets[0].status, "OPEN");
        assert_eq!(markets[0].lock_time, Some(NOW + 600));
        assert_eq!(markets[0].total_pool_usdt, dec!(100));
        assert_eq!(markets[1].symbol, "ETHUSDT");
        assert_eq!(markets[1].round_id, "eth-5m");
        let filtered = parse_prediction_markets(&payload(), &["BTC/USDT".into()]).unwrap();
        assert_eq!(filtered.len(), 1);
        assert!(discover_prediction_markets(&payload(), &["SOL/USDT".into()]).is_err());
    }

    #[test]
    fn allowed_plan_models_stake_and_settlement_risk() {
        let discovery = discover_prediction_markets(&payload(), &[]).unwrap();
        let plan = build_prediction_plan(
            &discovery,
            &intent("BTC/USDT", dec!(1)),
            &PredictionRiskLimits::default(),
        )
        .unwrap();
        assert!(plan.allowed, "{:?}", plan.rejection_reasons);
        assert_eq!(plan.market_id, "btc-5m");
        assert_eq!(plan.seconds_to_lock, Some(600));
        assert_eq!(plan.stake_bankroll_percent, dec!(1));
        assert_eq!(plan.pool_imbalance_percent, Some(dec!(20)));
        assert_eq!(plan.settlement_risk_score, dec!(5));
        assert_eq!(plan.payout_estimate, Some(dec!(1.6)));
        let by_round = build_prediction_plan(
            &discovery,
            &intent("42", dec!(1)),
            &PredictionRiskLimits::default(),
        )
        .unwrap();
        assert_eq!(by_round.market_id, "btc-5m");
    }

    #[test]
    fn unsafe_plan_is_rejected() {
        let discovery = discover_prediction_markets(&payload(), &[]).unwrap();
        let mut request = intent("eth-5m", dec!(6));
        request.emergency_stop = true;
        request.current_drawdown_percent = dec!(11);
        request.current_open_stake_usdt = dec!(5);
        let plan =
            build_prediction_plan(&discovery, &request, &PredictionRiskLimits::default()).unwrap();
        for reason in [
            "emergency stop is active",
            "prediction market is not available",
            "stake exceeds max_stake_usdt",
            "projected open stake exceeds max_total_stake_usdt",
            "stake exceeds bankroll percent limit",
            "current drawdown exceeds configured maximum",
            "market is too close to lock",
            "market liquidity is below minimum",
            "market pool imbalance exceeds configured maximum",
            "settlement risk exceeds configured maximum",
        ] {
            assert!(
                plan.rejection_reasons.iter().any(|r| r == reason),
                "{reason}"
            );
        }
        assert!(
            plan.warnings
                .contains(&"payout estimate is unavailable".to_owned())
        );
    }

    #[test]
    fn report_audit_render_and_live_guard() {
        let settings = crate::testutil::settings_builder().build().unwrap();
        let report = build_prediction_dry_run_report(
            &settings,
            &payload(),
            &["BTC/USDT".into()],
            Some(&intent("BTC/USDT", dec!(1))),
            &PredictionRiskLimits::default(),
        )
        .unwrap();
        assert!(report.audit.allowed);
        assert_eq!(report.audit.markets_discovered, 1);
        let text = render_prediction_report(&report);
        assert!(text.contains("  - direction: UP"));
        assert!(text.contains("Markets discovered: 1"));
        assert!(assert_prediction_live_disabled(true).is_err());
        assert_eq!(timestamp(Some(&json!("2023-11-14T22:13:20Z"))), Some(NOW));
        assert_eq!(timestamp(Some(&json!("1700000000"))), Some(NOW));
    }
}

//! Fast deterministic replay harness for the paper-only asset pool.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bb_binance::spot::{AccountCommission, CommissionDiscount, CommissionRates};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{PoolSettings, SETTINGS_FIELDS};
use crate::error::{PoolError, Result};
use crate::fees::seconds;
use crate::ledger::PaperLedger;
use crate::pyfmt::{num, parse_py_iso};
use crate::runtime::{Clock, PaperPoolRuntime};
use crate::source::{BalanceSource, CommissionSource, MarketRules, MarketSource, TickerQuote};

/// Default replay ledger path.
pub const DEFAULT_REPLAY_DB: &str = "data/pool-replay.db";
/// File name reserved for the production paper ledger.
pub const PRODUCTION_LEDGER_NAME: &str = "pool-paper.db";

fn replay_error(message: impl Into<String>) -> PoolError {
    PoolError::Replay(message.into())
}

/// Replay fixture (`tests/fixtures/pool_market_replay*.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayFixture {
    #[serde(default = "default_started_at")]
    pub started_at: String,
    #[serde(default = "default_cycle_seconds")]
    pub cycle_seconds: Decimal,
    #[serde(default)]
    pub initial_balances: BTreeMap<String, Decimal>,
    #[serde(default)]
    pub commission: FixtureCommission,
    /// [`PoolSettings`] overrides (Python dataclass field names).
    #[serde(default)]
    pub settings: Map<String, Value>,
    #[serde(default)]
    pub markets: Vec<FixtureMarket>,
}

fn default_started_at() -> String {
    "2026-01-01T00:00:00+00:00".into()
}

fn default_cycle_seconds() -> Decimal {
    Decimal::from(900)
}

/// Commission schedule served for every symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FixtureCommission {
    /// Defaults to `taker`.
    pub maker: Option<Decimal>,
    pub taker: Decimal,
    pub buyer: Decimal,
    pub seller: Decimal,
    pub discount_enabled: bool,
    pub discount_asset: String,
    pub discount_multiplier: Decimal,
}

impl Default for FixtureCommission {
    fn default() -> Self {
        Self {
            maker: None,
            taker: Decimal::new(1, 3),
            buyer: Decimal::ZERO,
            seller: Decimal::ZERO,
            discount_enabled: true,
            discount_asset: "BNB".into(),
            discount_multiplier: Decimal::new(75, 2),
        }
    }
}

/// One synthetic market with a per-cycle price series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureMarket {
    /// `BASE/QUOTE`.
    pub symbol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_series: Option<Vec<Decimal>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prices: Option<Vec<Decimal>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_volume_series: Option<Vec<Decimal>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_volume: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_amount: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_notional: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_precision: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_bps: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

impl FixtureMarket {
    fn series(&self) -> Result<&[Decimal]> {
        let series = match (&self.price_series, &self.prices, &self.price) {
            (Some(series), _, _) | (None, Some(series), _) => series.as_slice(),
            (None, None, Some(price)) => std::slice::from_ref(price),
            (None, None, None) => &[],
        };
        if series.is_empty() {
            return Err(replay_error(format!(
                "market {} requires price_series",
                self.symbol
            )));
        }
        if series.iter().any(|v| *v <= Decimal::ZERO) {
            return Err(replay_error("replay values must be positive"));
        }
        Ok(series)
    }

    fn base_quote(&self) -> Result<(String, String)> {
        let (base, quote) = self.symbol.split_once('/').ok_or_else(|| {
            replay_error(format!(
                "replay market symbol must look like BASE/QUOTE: {}",
                self.symbol
            ))
        })?;
        Ok((
            self.base.as_deref().unwrap_or(base).to_uppercase(),
            self.quote.as_deref().unwrap_or(quote).to_uppercase(),
        ))
    }

    fn validate(&self) -> Result<()> {
        self.series()?;
        self.base_quote()?;
        let non_negative =
            |v: Option<Decimal>| v.is_none_or(|v| !v.is_sign_negative() || v.is_zero());
        let volumes_ok = self
            .quote_volume_series
            .as_ref()
            .is_none_or(|s| !s.is_empty() && s.iter().all(|v| non_negative(Some(*v))));
        if !volumes_ok || !non_negative(self.quote_volume) || !non_negative(self.spread_bps) {
            return Err(replay_error("replay values must be non-negative"));
        }
        Ok(())
    }
}

impl ReplayFixture {
    /// Load and validate a fixture file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let value: Value = serde_json::from_str(&text)?;
        if !value.is_object() {
            return Err(replay_error("replay fixture must be a JSON object"));
        }
        let fixture: Self = serde_json::from_value(value)?;
        fixture.validate()?;
        Ok(fixture)
    }

    /// Validate balances, markets and series (Python `_positive_balances` / `_required_list`).
    pub fn validate(&self) -> Result<()> {
        if self
            .initial_balances
            .values()
            .any(|amount| *amount <= Decimal::ZERO)
        {
            return Err(replay_error("replay values must be positive"));
        }
        if self.initial_balances.is_empty() {
            return Err(replay_error(
                "replay fixture requires at least one positive balance",
            ));
        }
        if self.markets.is_empty() {
            return Err(replay_error(
                "replay fixture requires non-empty list: markets",
            ));
        }
        if self.cycle_seconds <= Decimal::ZERO {
            return Err(replay_error("replay values must be positive"));
        }
        parse_py_iso(&self.started_at).ok_or_else(|| {
            replay_error(format!("invalid replay started_at: {}", self.started_at))
        })?;
        self.markets.iter().try_for_each(FixtureMarket::validate)
    }

    /// Longest price series.
    pub fn fixture_cycles(&self) -> usize {
        self.markets
            .iter()
            .filter_map(|m| m.series().ok())
            .map(<[Decimal]>::len)
            .max()
            .unwrap_or(1)
    }

    /// Settings with fixture overrides, replay credentials and `db_path`.
    pub fn settings(&self, db_path: &Path) -> Result<PoolSettings> {
        let mut unknown: Vec<&str> = self
            .settings
            .keys()
            .map(String::as_str)
            .filter(|key| !SETTINGS_FIELDS.contains(key))
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            return Err(replay_error(format!(
                "unknown replay PoolSettings field(s): {}",
                unknown.join(", ")
            )));
        }
        let mut overrides = self.settings.clone();
        overrides.remove("api_key");
        overrides.remove("api_secret");
        overrides.insert(
            "db_path".into(),
            Value::String(db_path.display().to_string()),
        );
        let settings: PoolSettings = serde_json::from_value(Value::Object(overrides))
            .map_err(|err| replay_error(format!("invalid replay PoolSettings: {err}")))?;
        settings.validated()
    }
}

/// Read-only fake Binance client backed by a replay fixture.
#[derive(Debug)]
pub struct ReplayFixtureClient {
    fixture: ReplayFixture,
    started_at: DateTime<Utc>,
    cycle_index: AtomicUsize,
    balance_reads: AtomicUsize,
    commission_reads: AtomicUsize,
    request_methods: Mutex<Vec<&'static str>>,
}

impl ReplayFixtureClient {
    /// Client over a validated fixture.
    pub fn new(fixture: ReplayFixture) -> Result<Self> {
        fixture.validate()?;
        let started_at = parse_py_iso(&fixture.started_at).ok_or_else(|| {
            replay_error(format!("invalid replay started_at: {}", fixture.started_at))
        })?;
        Ok(Self {
            fixture,
            started_at,
            cycle_index: AtomicUsize::new(0),
            balance_reads: AtomicUsize::new(0),
            commission_reads: AtomicUsize::new(0),
            request_methods: Mutex::new(Vec::new()),
        })
    }

    fn index(&self) -> usize {
        self.cycle_index.load(Ordering::SeqCst)
    }

    /// Simulated time of the current cycle.
    pub fn now(&self) -> DateTime<Utc> {
        let index = Decimal::from(self.index());
        self.started_at + seconds(bb_core::money::to_f64(index * self.fixture.cycle_seconds))
    }

    /// Move to the next cycle.
    pub fn advance(&self) {
        self.cycle_index.fetch_add(1, Ordering::SeqCst);
    }

    fn ticker(&self, market: &FixtureMarket) -> Result<TickerQuote> {
        let index = self.index();
        let series = market.series()?;
        let price = series[index.min(series.len() - 1)];
        let spread = market.spread_bps.unwrap_or(Decimal::TWO);
        let half_spread = price * spread / Decimal::from(20_000);
        let quote_volume = match &market.quote_volume_series {
            Some(volumes) if !volumes.is_empty() => volumes[index.min(volumes.len() - 1)],
            _ => market.quote_volume.unwrap_or(Decimal::from(1_000_000)),
        };
        Ok(TickerQuote {
            bid: Some(price - half_spread),
            ask: Some(price + half_spread),
            last: Some(price),
            quote_volume: Some(quote_volume),
            timestamp_ms: Some(self.now().timestamp_millis()),
        })
    }
}

#[async_trait]
impl MarketSource for ReplayFixtureClient {
    async fn load_markets(&self) -> Result<Vec<MarketRules>> {
        self.request_methods.lock().push("load_markets");
        self.fixture
            .markets
            .iter()
            .map(|market| {
                let (base, quote) = market.base_quote()?;
                Ok(MarketRules {
                    symbol: market.symbol.clone(),
                    base,
                    quote,
                    spot: market.spot.unwrap_or(true),
                    active: market.active.unwrap_or(true),
                    min_amount: Some(market.min_amount.unwrap_or(Decimal::new(1, 5))),
                    min_notional: Some(market.min_notional.unwrap_or(Decimal::from(5))),
                    amount_precision: Some(market.amount_precision.unwrap_or(Decimal::new(1, 5))),
                    amount_step: None,
                })
            })
            .collect()
    }

    async fn fetch_tickers(&self) -> Result<HashMap<String, TickerQuote>> {
        self.request_methods.lock().push("fetch_tickers");
        self.fixture
            .markets
            .iter()
            .map(|market| Ok((market.symbol.clone(), self.ticker(market)?)))
            .collect()
    }
}

#[async_trait]
impl CommissionSource for ReplayFixtureClient {
    async fn account_commission(&self, symbol: &str) -> Result<AccountCommission> {
        self.commission_reads.fetch_add(1, Ordering::SeqCst);
        let commission = &self.fixture.commission;
        Ok(AccountCommission {
            symbol: symbol.to_owned(),
            standard_commission: CommissionRates {
                maker: commission.maker.unwrap_or(commission.taker),
                taker: commission.taker,
                buyer: commission.buyer,
                seller: commission.seller,
            },
            tax_commission: CommissionRates::default(),
            special_commission: Some(CommissionRates::default()),
            discount: Some(CommissionDiscount {
                enabled_for_account: commission.discount_enabled,
                enabled_for_symbol: commission.discount_enabled,
                discount_asset: commission.discount_asset.clone(),
                discount: commission.discount_multiplier,
            }),
        })
    }
}

#[async_trait]
impl BalanceSource for ReplayFixtureClient {
    async fn nonzero_balances(&self) -> Result<BTreeMap<String, Decimal>> {
        self.balance_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .fixture
            .initial_balances
            .iter()
            .map(|(asset, amount)| (asset.to_uppercase(), *amount))
            .collect())
    }
}

/// Compact outcome of a fixture replay run (Python `ReplaySummary.as_dict()` shape).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReplaySummary {
    pub cycles: usize,
    pub db_path: String,
    /// `action -> count`.
    pub decisions: BTreeMap<String, usize>,
    /// `portfolio -> asset -> amount`.
    #[serde(serialize_with = "num::nested_map")]
    pub final_balances: BTreeMap<String, BTreeMap<String, Decimal>>,
    /// Exchange method -> call count (only `load_markets` / `fetch_tickers` are allowed).
    pub request_counts: BTreeMap<String, usize>,
    pub balance_reads: usize,
    pub commission_reads: usize,
}

/// Load `fixture_path` and replay it.
pub async fn run_replay(
    fixture_path: impl AsRef<Path>,
    cycles: Option<usize>,
    db_path: impl AsRef<Path>,
    reset_db: bool,
) -> Result<ReplaySummary> {
    let fixture = ReplayFixture::load(fixture_path)?;
    run_replay_fixture(fixture, cycles, db_path, reset_db).await
}

/// Run the paper runtime against deterministic market data in an isolated ledger.
pub async fn run_replay_fixture(
    fixture: ReplayFixture,
    cycles: Option<usize>,
    db_path: impl AsRef<Path>,
    reset_db: bool,
) -> Result<ReplaySummary> {
    fixture.validate()?;
    let db: PathBuf = db_path.as_ref().to_path_buf();
    ensure_replay_db_path(&db)?;
    if reset_db && db.exists() {
        if db.is_dir() {
            return Err(replay_error(format!(
                "replay db path is a directory: {}",
                db.display()
            )));
        }
        std::fs::remove_file(&db)?;
    }
    let total_cycles = cycles.unwrap_or_else(|| fixture.fixture_cycles());
    if total_cycles == 0 {
        return Err(replay_error("replay cycles must be positive"));
    }
    let settings = fixture.settings(&db)?;
    let client = Arc::new(ReplayFixtureClient::new(fixture)?);
    let ledger = PaperLedger::open(&db)?;
    let clock_client = client.clone();
    let clock: Clock = Arc::new(move || clock_client.now());
    let scorers = settings.scorers.clone();
    let mut runtime = PaperPoolRuntime::new(client.clone(), settings, ledger, Some(clock));
    let mut decisions: BTreeMap<String, usize> = BTreeMap::new();
    for _ in 0..total_cycles {
        let report = runtime.cycle().await?;
        for decision in &report.decisions {
            *decisions
                .entry(decision.action.as_str().to_owned())
                .or_default() += 1;
        }
        client.advance();
    }
    let final_balances = scorers
        .iter()
        .map(|scorer| {
            Ok((
                scorer.clone(),
                runtime.ledger().load_portfolio(scorer)?.balances,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut request_counts: BTreeMap<String, usize> = BTreeMap::new();
    for method in client.request_methods.lock().iter() {
        *request_counts.entry((*method).to_owned()).or_default() += 1;
    }
    Ok(ReplaySummary {
        cycles: total_cycles,
        db_path: db.display().to_string(),
        decisions,
        final_balances,
        request_counts,
        balance_reads: client.balance_reads.load(Ordering::SeqCst),
        commission_reads: client.commission_reads.load(Ordering::SeqCst),
    })
}

/// Refuse the production paper ledger file name.
pub fn ensure_replay_db_path(path: &Path) -> Result<()> {
    if path
        .file_name()
        .is_some_and(|name| name == PRODUCTION_LEDGER_NAME)
    {
        return Err(replay_error(
            "replay db path must not use the production paper ledger",
        ));
    }
    Ok(())
}

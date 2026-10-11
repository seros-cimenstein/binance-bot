//! SQLite persistence for paper portfolio balances, observations, and decisions.
//!
//! The schema is byte-for-byte the Python one so existing `pool-paper.db` files open.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use bb_core::money::to_f64;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use rust_decimal::Decimal;
use serde_json::{Map, Value};

use crate::error::{PoolError, Result};
use crate::models::{MarketSnapshot, PaperDecision, RouteQuote, VirtualPortfolio};
use crate::pyfmt::{decimal_from_f64, decimal_number, parse_py_iso, py_isoformat, py_json_dumps};

/// Ledger DDL (identical to Python `_SCHEMA`).
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS virtual_portfolios (
    portfolio_id TEXT PRIMARY KEY,
    scorer_name TEXT NOT NULL,
    seeded_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS virtual_balances (
    portfolio_id TEXT NOT NULL,
    asset TEXT NOT NULL,
    amount REAL NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (portfolio_id, asset)
);
CREATE TABLE IF NOT EXISTS pool_observations (
    observed_at TEXT NOT NULL,
    asset TEXT NOT NULL,
    benchmark TEXT NOT NULL,
    rate REAL NOT NULL,
    PRIMARY KEY (observed_at, asset, benchmark)
);
CREATE TABLE IF NOT EXISTS market_catalog_latest (
    symbol TEXT PRIMARY KEY,
    observed_at TEXT NOT NULL,
    base_asset TEXT NOT NULL,
    quote_asset TEXT NOT NULL,
    eligible INTEGER NOT NULL,
    quote_volume_usdt REAL,
    spread_bps REAL NOT NULL,
    rejection_reasons TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS market_catalog_cycles (
    observed_at TEXT PRIMARY KEY,
    markets_count INTEGER NOT NULL,
    eligible_count INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS pool_snapshots (
    observed_at TEXT NOT NULL,
    portfolio_id TEXT NOT NULL,
    benchmark TEXT NOT NULL,
    value REAL NOT NULL,
    basket_score REAL,
    PRIMARY KEY (observed_at, portfolio_id, benchmark)
);
CREATE TABLE IF NOT EXISTS paper_decisions (
    decided_at TEXT NOT NULL,
    portfolio_id TEXT NOT NULL,
    scorer_name TEXT NOT NULL,
    action TEXT NOT NULL,
    source_asset TEXT,
    destination_asset TEXT,
    input_amount REAL,
    output_amount REAL,
    score REAL,
    reason TEXT NOT NULL,
    route_json TEXT NOT NULL
);
";

/// Tolerance when checking a simulated spend against a virtual balance.
const OVERDRAW_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 12);

/// Persistent virtual state, isolated from any live order storage.
#[derive(Debug)]
pub struct PaperLedger {
    connection: Mutex<Connection>,
}

fn real(value: Decimal) -> f64 {
    to_f64(value)
}

fn amount(value: f64) -> Decimal {
    decimal_from_f64(value).unwrap_or_default()
}

impl PaperLedger {
    /// Open (creating parent directories and tables as needed).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Create the portfolio with `balances` (positive amounts only) unless it exists.
    pub fn seed_if_absent(
        &self,
        portfolio_id: &str,
        scorer_name: &str,
        balances: &BTreeMap<String, Decimal>,
    ) -> Result<VirtualPortfolio> {
        {
            let mut connection = self.connection.lock();
            let exists = connection
                .query_row(
                    "SELECT portfolio_id FROM virtual_portfolios WHERE portfolio_id = ?",
                    [portfolio_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                let timestamp = py_isoformat(bb_core::time::now());
                let tx = connection.transaction()?;
                tx.execute(
                    "INSERT INTO virtual_portfolios (portfolio_id, scorer_name, seeded_at) VALUES (?, ?, ?)",
                    params![portfolio_id, scorer_name, timestamp],
                )?;
                for (asset, amount) in balances {
                    if *amount > Decimal::ZERO {
                        tx.execute(
                            "INSERT INTO virtual_balances (portfolio_id, asset, amount, updated_at) VALUES (?, ?, ?, ?)",
                            params![portfolio_id, asset.to_uppercase(), real(*amount), timestamp],
                        )?;
                    }
                }
                tx.commit()?;
            }
        }
        self.load_portfolio(portfolio_id)
    }

    /// Load one portfolio and its balances.
    pub fn load_portfolio(&self, portfolio_id: &str) -> Result<VirtualPortfolio> {
        let connection = self.connection.lock();
        let row: Option<(String, String, String)> = connection
            .query_row(
                "SELECT portfolio_id, scorer_name, seeded_at FROM virtual_portfolios WHERE portfolio_id = ?",
                [portfolio_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((id, scorer_name, seeded_at)) = row else {
            return Err(PoolError::UnknownPortfolio(portfolio_id.to_owned()));
        };
        let mut statement = connection
            .prepare("SELECT asset, amount FROM virtual_balances WHERE portfolio_id = ?")?;
        let balances = statement
            .query_map([portfolio_id], |row| {
                Ok((row.get::<_, String>(0)?, amount(row.get(1)?)))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(VirtualPortfolio {
            portfolio_id: id,
            scorer_name,
            balances,
            seeded_at: parse_py_iso(&seeded_at).ok_or_else(|| {
                PoolError::Config(format!("invalid seeded_at timestamp: {seeded_at}"))
            })?,
        })
    }

    /// Apply a simulated fill to virtual balances. Never calls an exchange.
    pub fn apply_route(&self, portfolio_id: &str, route: &RouteQuote) -> Result<VirtualPortfolio> {
        let portfolio = self.load_portfolio(portfolio_id)?;
        let available = portfolio
            .balances
            .get(&route.source_asset)
            .copied()
            .unwrap_or_default();
        if route.input_amount > available + OVERDRAW_TOLERANCE {
            return Err(PoolError::Overdraw);
        }
        let mut balances = portfolio.balances;
        balances.insert(
            route.source_asset.clone(),
            (available - route.input_amount).max(Decimal::ZERO),
        );
        *balances.entry(route.destination_asset.clone()).or_default() += route.output_amount;
        let timestamp = py_isoformat(bb_core::time::now());
        {
            let mut connection = self.connection.lock();
            let tx = connection.transaction()?;
            for (asset, amount) in &balances {
                tx.execute(
                    "INSERT OR REPLACE INTO virtual_balances (portfolio_id, asset, amount, updated_at) VALUES (?, ?, ?, ?)",
                    params![portfolio_id, asset, real(*amount), timestamp],
                )?;
            }
            tx.commit()?;
        }
        self.load_portfolio(portfolio_id)
    }

    /// Persist indicative `(asset, benchmark) -> rate` observations and keep only the
    /// newest `retain_latest` per pair.
    pub fn record_observations(
        &self,
        observed_at: &str,
        rates: &[((String, String), Decimal)],
        retain_latest: usize,
    ) -> Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        for ((asset, benchmark), rate) in rates {
            tx.execute(
                "INSERT OR REPLACE INTO pool_observations (observed_at, asset, benchmark, rate) VALUES (?, ?, ?, ?)",
                params![observed_at, asset, benchmark, real(*rate)],
            )?;
        }
        tx.execute(
            "DELETE FROM pool_observations
             WHERE rowid IN (
                 SELECT rowid FROM (
                     SELECT rowid,
                            ROW_NUMBER() OVER (
                                PARTITION BY asset, benchmark
                                ORDER BY observed_at DESC
                            ) AS observation_rank
                     FROM pool_observations
                 )
                 WHERE observation_rank > ?
             )",
            [i64::try_from(retain_latest).unwrap_or(i64::MAX)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Persist market admission decisions for paper-run auditability.
    pub fn record_catalog(&self, observed_at: &str, markets: &[MarketSnapshot]) -> Result<()> {
        let eligible = markets.iter().filter(|m| m.eligible).count();
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO market_catalog_cycles (observed_at, markets_count, eligible_count) VALUES (?, ?, ?)",
            params![observed_at, i64::try_from(markets.len()).unwrap_or(i64::MAX), i64::try_from(eligible).unwrap_or(i64::MAX)],
        )?;
        for market in markets {
            let reasons = Value::Array(
                market
                    .rejection_reasons
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            );
            tx.execute(
                "INSERT OR REPLACE INTO market_catalog_latest
                    (observed_at, symbol, base_asset, quote_asset, eligible,
                     quote_volume_usdt, spread_bps, rejection_reasons)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    observed_at,
                    market.symbol,
                    market.base,
                    market.quote,
                    i64::from(market.eligible),
                    market.quote_volume_usdt.map(real),
                    market.spread_bps.map_or(f64::INFINITY, real),
                    py_json_dumps(&reasons, false),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Newest `limit` rates per benchmark, oldest first, in `benchmarks` order.
    pub fn observation_history(
        &self,
        asset: &str,
        benchmarks: &[String],
        limit: usize,
    ) -> Result<Vec<(String, Vec<f64>)>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT rate FROM pool_observations WHERE asset = ? AND benchmark = ? ORDER BY observed_at DESC LIMIT ?",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        benchmarks
            .iter()
            .map(|benchmark| {
                let mut rates = statement
                    .query_map(params![asset, benchmark, limit], |row| row.get::<_, f64>(0))?
                    .collect::<rusqlite::Result<Vec<f64>>>()?;
                rates.reverse();
                Ok((benchmark.clone(), rates))
            })
            .collect()
    }

    /// Persist one portfolio's benchmark totals.
    pub fn record_snapshot(
        &self,
        observed_at: &str,
        portfolio_id: &str,
        totals: &[(String, Decimal)],
        basket_score: Option<f64>,
    ) -> Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        for (benchmark, value) in totals {
            tx.execute(
                "INSERT OR REPLACE INTO pool_snapshots (observed_at, portfolio_id, benchmark, value, basket_score) VALUES (?, ?, ?, ?, ?)",
                params![observed_at, portfolio_id, benchmark, real(*value), basket_score],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// First recorded total per benchmark.
    pub fn initial_totals(&self, portfolio_id: &str) -> Result<Vec<(String, f64)>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT s.benchmark, s.value
             FROM pool_snapshots s
             JOIN (
                 SELECT benchmark, MIN(observed_at) AS first_at
                 FROM pool_snapshots WHERE portfolio_id = ? GROUP BY benchmark
             ) first
             ON first.benchmark = s.benchmark AND first.first_at = s.observed_at
             WHERE s.portfolio_id = ?",
        )?;
        let rows = statement
            .query_map([portfolio_id, portfolio_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<(String, f64)>>>()?;
        Ok(rows)
    }

    /// Append one decision; the route is stored as the Python `route_json` leg list.
    pub fn record_decision(&self, decision: &PaperDecision) -> Result<()> {
        let route_payload: Vec<Value> = decision
            .route
            .iter()
            .flat_map(|route| &route.legs)
            .map(|leg| {
                let mut entry = Map::new();
                entry.insert("symbol".into(), leg.symbol.clone().into());
                entry.insert("side".into(), leg.side.as_str().into());
                entry.insert("source".into(), leg.source_asset.clone().into());
                entry.insert("destination".into(), leg.destination_asset.clone().into());
                entry.insert(
                    "input".into(),
                    Value::Number(decimal_number(leg.input_amount)),
                );
                entry.insert(
                    "output".into(),
                    Value::Number(decimal_number(leg.net_output_amount)),
                );
                entry.insert(
                    "fee_rate".into(),
                    Value::Number(decimal_number(leg.fee_rate)),
                );
                entry.insert(
                    "fee".into(),
                    Value::Number(decimal_number(leg.fee_output_amount)),
                );
                Value::Object(entry)
            })
            .collect();
        let connection = self.connection.lock();
        connection.execute(
            "INSERT INTO paper_decisions
                (decided_at, portfolio_id, scorer_name, action,
                 source_asset, destination_asset, input_amount,
                 output_amount, score, reason, route_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                py_isoformat(decision.decided_at),
                decision.portfolio_id,
                decision.scorer_name,
                decision.action.as_str(),
                decision.source_asset,
                decision.destination_asset,
                decision.input_amount.map(real),
                decision.route.as_ref().map(|r| real(r.output_amount)),
                decision.score,
                decision.reason,
                py_json_dumps(&Value::Array(route_payload), true),
            ],
        )?;
        Ok(())
    }

    /// Time of the newest `rebalance` decision.
    pub fn last_rebalance_at(&self, portfolio_id: &str) -> Result<Option<DateTime<Utc>>> {
        let connection = self.connection.lock();
        let decided_at: Option<String> = connection
            .query_row(
                "SELECT decided_at FROM paper_decisions WHERE portfolio_id = ? AND action = 'rebalance' ORDER BY decided_at DESC LIMIT 1",
                [portfolio_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(decided_at.as_deref().and_then(parse_py_iso))
    }
}

/// Convenience: `(asset, benchmark) -> rate` lookup over recorded observations.
pub fn rate_lookup(rates: &[((String, String), Decimal)]) -> HashMap<(&str, &str), Decimal> {
    rates
        .iter()
        .map(|((asset, benchmark), rate)| ((asset.as_str(), benchmark.as_str()), *rate))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn market() -> MarketSnapshot {
        MarketSnapshot {
            symbol: "BNB/USDT".into(),
            base: "BNB".into(),
            quote: "USDT".into(),
            bid: dec!(600),
            ask: dec!(600.1),
            last: dec!(600),
            quote_volume: dec!(1000000),
            quote_volume_usdt: Some(dec!(1000000)),
            min_amount: dec!(0.00001),
            min_notional: dec!(5),
            amount_precision: Some(dec!(0.00001)),
            timestamp_ms: Some(0),
            spread_bps: Some(dec!(1)),
            eligible: true,
            amount_step: None,
            rejection_reasons: Vec::new(),
        }
    }

    #[test]
    fn applies_routes_without_live_order_state() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = PaperLedger::open(dir.path().join("nested/pool.db")).unwrap();
        let seeded = BTreeMap::from([
            ("bnb".to_owned(), dec!(1)),
            ("ZERO".to_owned(), Decimal::ZERO),
        ]);
        let portfolio = ledger
            .seed_if_absent("simple", "simple_momentum", &seeded)
            .unwrap();
        assert_eq!(
            portfolio.balances,
            BTreeMap::from([("BNB".to_owned(), dec!(1))])
        );
        let route = RouteQuote {
            source_asset: "BNB".into(),
            destination_asset: "USDT".into(),
            input_amount: dec!(0.5),
            output_amount: dec!(299),
            legs: Vec::new(),
            slippage_bps: Decimal::ZERO,
            generated_at: bb_core::time::now(),
        };
        let updated = ledger.apply_route("simple", &route).unwrap();
        assert_eq!(
            updated.balances,
            BTreeMap::from([
                ("BNB".to_owned(), dec!(0.5)),
                ("USDT".to_owned(), dec!(299))
            ])
        );
        let overdraw = RouteQuote {
            input_amount: dec!(5),
            ..route
        };
        assert!(matches!(
            ledger.apply_route("simple", &overdraw),
            Err(PoolError::Overdraw)
        ));
        assert!(matches!(
            ledger.load_portfolio("missing"),
            Err(PoolError::UnknownPortfolio(_))
        ));
    }

    #[test]
    fn keeps_latest_catalog_and_rolling_observations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool.db");
        let ledger = PaperLedger::open(&path).unwrap();
        let mut rejected = market();
        rejected.symbol = "OLD/USDT".into();
        rejected.eligible = false;
        rejected.spread_bps = None;
        rejected.rejection_reasons = vec!["market is not active".into(), "ticker is stale".into()];
        ledger
            .record_catalog("2026-05-25T01:00:00+00:00", &[market()])
            .unwrap();
        ledger
            .record_catalog("2026-05-25T01:15:00+00:00", &[market(), rejected])
            .unwrap();
        for index in 0..3 {
            ledger
                .record_observations(
                    &format!("2026-05-25T01:{index:02}:00+00:00"),
                    &[(
                        ("BNB".into(), "USDT".into()),
                        dec!(600) + Decimal::from(index),
                    )],
                    2,
                )
                .unwrap();
        }
        let history = ledger
            .observation_history("BNB", &["USDT".into()], 5)
            .unwrap();
        assert_eq!(history, vec![("USDT".to_owned(), vec![601.0, 602.0])]);
        drop(ledger);

        let connection = Connection::open(&path).unwrap();
        let count = |sql: &str| {
            connection
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(count("select count(*) from market_catalog_latest"), 2);
        assert_eq!(count("select count(*) from market_catalog_cycles"), 2);
        let (reasons, spread): (String, f64) = connection
            .query_row(
                "select rejection_reasons, spread_bps from market_catalog_latest where symbol = 'OLD/USDT'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(reasons, r#"["market is not active", "ticker is stale"]"#);
        assert!(spread.is_infinite());
    }
}

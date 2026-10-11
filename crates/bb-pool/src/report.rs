//! Read-only evidence report for the paper-only Spot allocation pool.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bb_core::money::to_f64;
use rusqlite::{Connection, OpenFlags};
use rust_decimal::Decimal;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use crate::error::{PoolError, Result};
use crate::pyfmt::{decimal_from_f64, fmt_g, num};
use crate::scoring::Scorer;

/// Default production paper ledger.
pub const DEFAULT_PAPER_DB: &str = "data/pool-paper.db";

fn report_error(message: impl Into<String>) -> PoolError {
    PoolError::Report(message.into())
}

/// Report parameters (Python `build_report` keyword arguments).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportOptions {
    pub required_observations: usize,
    pub benchmarks: Vec<String>,
    pub benchmark_weights: Vec<f64>,
    pub scorers: Vec<String>,
    pub limit: usize,
}

impl Default for ReportOptions {
    fn default() -> Self {
        Self {
            required_observations: 96,
            benchmarks: ["USDT", "BTC", "PAXG", "XAUT"].map(String::from).to_vec(),
            benchmark_weights: vec![0.25; 4],
            scorers: ["simple_momentum", "risk_adjusted_momentum"]
                .map(String::from)
                .to_vec(),
            limit: 10,
        }
    }
}

/// Currently eligible asset represented by its most liquid market.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateMarket {
    pub asset: String,
    pub routes: i64,
    #[serde(serialize_with = "num::decimal")]
    pub best_quote_volume_usdt: Decimal,
}

/// A candidate ranked from the retained paper observation window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoredCandidate {
    pub asset: String,
    pub score: f64,
}

/// Most recently persisted paper decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatestDecision {
    pub decided_at: String,
    pub portfolio_id: String,
    pub action: String,
    pub reason: String,
    pub source_asset: Option<String>,
    pub destination_asset: Option<String>,
}

/// Read-only view over one paper pool ledger.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaperPoolReport {
    pub db_path: String,
    pub required_observations: usize,
    pub completed_cycles: i64,
    pub latest_cycle_at: Option<String>,
    pub total_markets: i64,
    pub eligible_markets: i64,
    pub eligible_assets: i64,
    #[serde(serialize_with = "num::nested_map")]
    pub balances: BTreeMap<String, BTreeMap<String, Decimal>>,
    pub decision_counts: BTreeMap<String, i64>,
    /// Most common reasons first.
    #[serde(serialize_with = "ordered_map")]
    pub decision_reasons: Vec<(String, i64)>,
    pub latest_decisions: Vec<LatestDecision>,
    pub top_eligible_assets: Vec<CandidateMarket>,
    /// Most common rejection reasons first.
    #[serde(serialize_with = "ordered_map")]
    pub rejection_reasons: Vec<(String, i64)>,
    /// Per scorer, in configured order.
    #[serde(serialize_with = "ordered_map")]
    pub rankings: Vec<(String, Vec<ScoredCandidate>)>,
}

fn ordered_map<S: Serializer, V: Serialize>(
    value: &[(String, V)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut out = serializer.serialize_map(Some(value.len()))?;
    for (key, item) in value {
        out.serialize_entry(key, item)?;
    }
    out.end()
}

#[derive(Serialize)]
struct ReportJson<'a> {
    #[serde(flatten)]
    report: &'a PaperPoolReport,
    remaining_observations: usize,
    ranking_available: bool,
}

impl PaperPoolReport {
    /// Warm-up cycles still missing.
    pub fn remaining_observations(&self) -> usize {
        let completed = usize::try_from(self.completed_cycles).unwrap_or(0);
        self.required_observations.saturating_sub(completed)
    }

    /// Whether any scorer produced a ranking.
    pub fn ranking_available(&self) -> bool {
        self.rankings.iter().any(|(_, ranked)| !ranked.is_empty())
    }

    /// Python `as_dict()` rendered as `json.dumps(indent=2, sort_keys=True)`.
    pub fn to_json(&self) -> Result<String> {
        Ok(crate::pyfmt::pretty_sorted(&ReportJson {
            report: self,
            remaining_observations: self.remaining_observations(),
            ranking_available: self.ranking_available(),
        })?)
    }
}

fn expand_user(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// Read persisted paper evidence without opening the ledger for writes.
pub fn build_report(db_path: impl AsRef<Path>, options: &ReportOptions) -> Result<PaperPoolReport> {
    let path = expand_user(db_path.as_ref());
    if !path.is_file() {
        return Err(report_error(format!(
            "paper ledger does not exist: {}",
            path.display()
        )));
    }
    if options.required_observations == 0 {
        return Err(report_error("required observations must be positive"));
    }
    if options.limit == 0 {
        return Err(report_error("report limit must be positive"));
    }
    if options.benchmarks.is_empty() || options.benchmarks.len() != options.benchmark_weights.len()
    {
        return Err(report_error(
            "benchmarks and weights must have matching non-empty values",
        ));
    }
    let scorers = options
        .scorers
        .iter()
        .map(|name| Scorer::from_name(name).map(|scorer| (name.clone(), scorer)))
        .collect::<Result<Vec<_>>>()?;
    let benchmarks: Vec<String> = options
        .benchmarks
        .iter()
        .map(|b| b.trim().to_uppercase())
        .collect();
    let weights: Vec<(String, f64)> = benchmarks
        .iter()
        .cloned()
        .zip(options.benchmark_weights.iter().copied())
        .collect();
    read_report(&path, options, &scorers, &benchmarks, &weights)
        .map_err(|err| report_error(format!("failed to read paper ledger: {err}")))
}

fn read_report(
    path: &Path,
    options: &ReportOptions,
    scorers: &[(String, Scorer)],
    benchmarks: &[String],
    weights: &[(String, f64)],
) -> rusqlite::Result<PaperPoolReport> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let limit = i64::try_from(options.limit).unwrap_or(i64::MAX);
    let (completed_cycles, latest_cycle_at): (i64, Option<String>) = connection.query_row(
        "SELECT COUNT(*) AS count, MAX(observed_at) AS latest FROM market_catalog_cycles",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (total_markets, eligible_markets, eligible_assets): (i64, i64, i64) = connection
        .query_row(
            "SELECT COUNT(*) AS total,
                COALESCE(SUM(eligible), 0) AS eligible,
                COUNT(DISTINCT CASE WHEN eligible = 1 THEN base_asset END) AS assets
         FROM market_catalog_latest",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

    let mut balances: BTreeMap<String, BTreeMap<String, Decimal>> = BTreeMap::new();
    let mut statement = connection.prepare(
        "SELECT portfolio_id, asset, amount FROM virtual_balances ORDER BY portfolio_id, asset",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    for row in rows {
        let (portfolio, asset, amount) = row?;
        balances
            .entry(portfolio)
            .or_default()
            .insert(asset, decimal_from_f64(amount).unwrap_or_default());
    }

    let mut statement = connection
        .prepare("SELECT action, COUNT(*) AS count FROM paper_decisions GROUP BY action")?;
    let decision_counts = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<String, i64>>>()?;
    let mut statement = connection.prepare(
        "SELECT reason, COUNT(*) AS count FROM paper_decisions GROUP BY reason ORDER BY count DESC, reason LIMIT ?",
    )?;
    let decision_reasons = statement
        .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, i64)>>>()?;
    let mut statement = connection.prepare(
        "SELECT decided_at, portfolio_id, action, reason, source_asset, destination_asset
         FROM paper_decisions ORDER BY decided_at DESC, portfolio_id LIMIT ?",
    )?;
    let latest_decisions = statement
        .query_map([i64::try_from(scorers.len()).unwrap_or(i64::MAX)], |row| {
            Ok(LatestDecision {
                decided_at: row.get(0)?,
                portfolio_id: row.get(1)?,
                action: row.get(2)?,
                reason: row.get(3)?,
                source_asset: row.get(4)?,
                destination_asset: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut statement = connection.prepare(
        "SELECT base_asset, COUNT(*) AS routes, MAX(COALESCE(quote_volume_usdt, 0)) AS best_volume
         FROM market_catalog_latest WHERE eligible = 1
         GROUP BY base_asset ORDER BY best_volume DESC, base_asset LIMIT ?",
    )?;
    let top_eligible_assets = statement
        .query_map([limit], |row| {
            Ok(CandidateMarket {
                asset: row.get(0)?,
                routes: row.get(1)?,
                best_quote_volume_usdt: decimal_from_f64(row.get(2)?).unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let rejection_reasons = read_rejection_reasons(&connection, options.limit)?;
    let rankings = read_rankings(
        &connection,
        options.required_observations,
        benchmarks,
        weights,
        scorers,
        options.limit,
    )?;
    Ok(PaperPoolReport {
        db_path: path.display().to_string(),
        required_observations: options.required_observations,
        completed_cycles,
        latest_cycle_at,
        total_markets,
        eligible_markets,
        eligible_assets,
        balances,
        decision_counts,
        decision_reasons,
        latest_decisions,
        top_eligible_assets,
        rejection_reasons,
        rankings,
    })
}

/// `Counter.most_common(limit)`: count descending, ties in first-seen order.
fn read_rejection_reasons(
    connection: &Connection,
    limit: usize,
) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut statement = connection
        .prepare("SELECT rejection_reasons FROM market_catalog_latest WHERE eligible = 0")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut counts: Vec<(String, i64)> = Vec::new();
    for row in rows {
        let raw = row?;
        let reasons: Vec<String> = match serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
            Ok(values) => values
                .into_iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                .collect(),
            Err(_) => vec!["invalid persisted rejection reason".into()],
        };
        for reason in reasons {
            match counts.iter_mut().find(|(name, _)| *name == reason) {
                Some((_, count)) => *count += 1,
                None => counts.push((reason, 1)),
            }
        }
    }
    counts.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    counts.truncate(limit);
    Ok(counts)
}

fn read_rankings(
    connection: &Connection,
    required: usize,
    benchmarks: &[String],
    weights: &[(String, f64)],
    scorers: &[(String, Scorer)],
    limit: usize,
) -> rusqlite::Result<Vec<(String, Vec<ScoredCandidate>)>> {
    let mut statement = connection
        .prepare("SELECT DISTINCT base_asset FROM market_catalog_latest WHERE eligible = 1")?;
    let eligible: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut statement = connection
        .prepare("SELECT asset, benchmark, observed_at, rate FROM pool_observations ORDER BY asset, benchmark, observed_at")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(3)?,
        ))
    })?;
    let mut observations: BTreeMap<String, HashMap<String, Vec<f64>>> = BTreeMap::new();
    for row in rows {
        let (asset, benchmark, rate) = row?;
        if eligible.contains(&asset) && benchmarks.contains(&benchmark) {
            observations
                .entry(asset)
                .or_default()
                .entry(benchmark)
                .or_default()
                .push(rate);
        }
    }
    Ok(scorers
        .iter()
        .map(|(name, scorer)| {
            let mut scored: Vec<ScoredCandidate> = observations
                .iter()
                .filter_map(|(asset, history)| {
                    let window = benchmarks
                        .iter()
                        .map(|b| {
                            let values = history.get(b).filter(|v| v.len() >= required)?;
                            Some((b.clone(), values[values.len() - required..].to_vec()))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    scorer
                        .score(asset, &window, weights)
                        .map(|s| ScoredCandidate {
                            asset: s.asset,
                            score: s.score,
                        })
                })
                .collect();
            scored.sort_by(|a, b| b.score.total_cmp(&a.score));
            scored.truncate(limit);
            (name.clone(), scored)
        })
        .collect())
}

/// Operator-facing explanation of current paper evidence.
pub fn render_report(report: &PaperPoolReport) -> String {
    let mut lines = vec![
        "Paper Spot pool report: read-only".to_owned(),
        format!("Ledger: {}", report.db_path),
        format!(
            "Warm-up cycles: {}/{} ({} remaining)",
            report.completed_cycles,
            report.required_observations,
            report.remaining_observations()
        ),
        format!(
            "Latest cycle: {}",
            report.latest_cycle_at.as_deref().unwrap_or("none")
        ),
        format!(
            "Markets: {}/{} eligible; {} candidate assets",
            report.eligible_markets, report.total_markets, report.eligible_assets
        ),
        "Balances:".to_owned(),
    ];
    for (portfolio, balances) in &report.balances {
        let holdings: Vec<String> = balances
            .iter()
            .map(|(asset, amount)| format!("{asset}={}", fmt_g(to_f64(*amount), 12)))
            .collect();
        let holdings = if holdings.is_empty() {
            "none".to_owned()
        } else {
            holdings.join(", ")
        };
        lines.push(format!("  {portfolio}: {holdings}"));
    }
    let counts: Vec<String> = report
        .decision_counts
        .iter()
        .map(|(a, c)| format!("{a}={c}"))
        .collect();
    lines.push(format!(
        "Decisions: {}",
        if counts.is_empty() {
            "none".to_owned()
        } else {
            counts.join(", ")
        }
    ));
    if !report.decision_reasons.is_empty() {
        lines.push("Decision reasons:".into());
        for (reason, count) in &report.decision_reasons {
            lines.push(format!("  {count}: {reason}"));
        }
    }
    if !report.latest_decisions.is_empty() {
        lines.push("Latest decisions:".into());
        for decision in &report.latest_decisions {
            lines.push(format!(
                "  {}: {} - {}",
                decision.portfolio_id, decision.action, decision.reason
            ));
        }
    }
    if report.ranking_available() {
        lines.push("Ranked candidates from retained observations:".into());
        for (scorer, candidates) in &report.rankings {
            let values: Vec<String> = candidates
                .iter()
                .map(|c| format!("{} ({})", c.asset, fmt_g(c.score, 6)))
                .collect();
            let values = if values.is_empty() {
                "none".to_owned()
            } else {
                values.join(", ")
            };
            lines.push(format!("  {scorer}: {values}"));
        }
    } else {
        lines.push(
            "Ranked candidates: unavailable until candidate assets have the required benchmark history.".into(),
        );
    }
    if !report.top_eligible_assets.is_empty() {
        lines.push("Current most-liquid eligible candidate assets:".into());
        let mut line = String::from("  ");
        for (index, candidate) in report.top_eligible_assets.iter().enumerate() {
            if index > 0 {
                line.push_str(", ");
            }
            let _ = write!(
                line,
                "{} ({:.2} USDT)",
                candidate.asset,
                to_f64(candidate.best_quote_volume_usdt)
            );
        }
        lines.push(line);
    }
    if !report.rejection_reasons.is_empty() {
        lines.push("Most common market rejection reasons:".into());
        let reasons: Vec<String> = report
            .rejection_reasons
            .iter()
            .map(|(r, c)| format!("{r}={c}"))
            .collect();
        lines.push(format!("  {}", reasons.join(", ")));
    }
    lines.push("Live execution: disabled; this command only reads the paper ledger.".into());
    lines.join("\n")
}

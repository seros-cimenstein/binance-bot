//! Audit rows (Python schema + the rule's verdict), the failed-route halt and the operator review
//! marker that clears it.

use std::path::{Path, PathBuf};

use bb_core::audit::JsonlLog;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::TraderConfig;
use crate::error::{Result, TraderError};
use crate::executor::LegResult;
use crate::memory::{
    AuditRows, load_audit_rows, row_attempted_submission, row_failed_submission_reviewed,
    row_route_id, row_status,
};
use crate::universe::RouteCandidate;

/// Default `--review-reason`.
pub const DEFAULT_REVIEW_REASON: &str =
    "operator reviewed failed submitted route and approved live re-enable";

/// Python status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RowStatus {
    Held,
    Planned,
    Submitted,
    Failed,
    Blocked,
    Reviewed,
}

impl RowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Planned => "planned",
            Self::Submitted => "submitted",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Reviewed => "reviewed",
        }
    }
}

/// What a row describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowType {
    /// One planned / executed / refused route.
    Route,
    /// One per cycle: The bot's end-of-cycle statement, tool trace and usage.
    CycleSummary,
    /// `--mark-failed-route-reviewed`.
    Review,
}

/// Python `BotSpotDecision.as_dict`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub action: String,
    pub reason: String,
    pub route_id: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub spend_mode: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub spend_amount: Option<Decimal>,
}

impl Decision {
    pub fn hold(reason: impl Into<String>) -> Self {
        Self::action("hold", reason)
    }

    /// Cycle-level action (`hold` or `traded`).
    pub fn action(action: &str, reason: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            reason: reason.into(),
            route_id: String::new(),
            source_asset: String::new(),
            destination_asset: String::new(),
            spend_mode: String::new(),
            spend_amount: None,
        }
    }

    /// One `execute_route` request.
    pub fn rebalance(
        candidate: &RouteCandidate,
        reason: &str,
        spend_amount: Option<Decimal>,
    ) -> Self {
        Self {
            action: "rebalance".into(),
            reason: reason.into(),
            route_id: candidate.route_id.clone(),
            source_asset: candidate.route.source_asset.clone(),
            destination_asset: candidate.route.destination_asset.clone(),
            spend_mode: if spend_amount.is_some() {
                "amount"
            } else {
                "all_available"
            }
            .into(),
            spend_amount,
        }
    }
}

/// What a cycle ended with: `hold` or `traded`, and the summary in words.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct EndCycle {
    pub action: String,
    pub summary: String,
}

/// `--mark-failed-route-reviewed` marker.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OperatorReview {
    pub failed_submission_reviewed: bool,
    pub reviewed_failed_timestamp: String,
    pub reviewed_failed_route_id: String,
    pub reason: String,
}

/// One audit JSONL row. Every Python field is kept.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditRow {
    pub timestamp: String,
    pub mode: String,
    pub status: RowStatus,
    /// The operator's rule-set name (the rule file's `rules_version`); LLM-era rows have
    /// `prompt_version` instead.
    pub rules_version: String,
    pub context_hash: String,
    pub decision: Decision,
    pub candidate_route_count: usize,
    pub market_pick_mode: String,
    pub candidate_routes: Vec<Value>,
    /// Python `selected_route` (`{}` when none).
    pub selected_route: Value,
    pub submitted_legs: Vec<LegResult>,
    pub submission_attempted: bool,
    pub error: String,
    pub audit_path: String,
    pub row_type: RowType,
    pub cycle_id: String,
    pub session_id: String,
    /// The rule sleeve the row belongs to (Phase 53 D5: decision rows and their route rows;
    /// absent on older rows and on rows for the whole trader, such as halts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleeve: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_review: Option<OperatorReview>,
    /// Cycle summaries: the trader's non-secret run configuration
    /// ([`TraderConfig::run_config`]), so each cycle says what setup produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_config: Option<Value>,
    /// Cycle summaries (Phase 53): the tags feed the cycle read, summarised
    /// ([`crate::tags::TagsNow`]: `as_of`, age, digest, sources, tags per kind, tradable and
    /// scored-only asset names).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Value>,
    /// Cycle summaries: the rule, the best candidate, the held coins' scores and the
    /// orders it asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<Value>,
    /// Sleeve halt and review rows (Phase 53, [`crate::sleeves`]): `kind`
    /// (`sleeve_mismatch` or `sleeves_reviewed`), the reason and the holdings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleeve_check: Option<Value>,
    /// Detail left out to keep the row under [`ROW_BUDGET_BYTES`] ([`fit_budget`]), one
    /// note per step; absent when nothing was.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trimmed: Vec<String>,
}

impl AuditRow {
    /// Row skeleton with the config-derived fields filled.
    pub fn new(
        config: &TraderConfig,
        row_type: RowType,
        status: RowStatus,
        decision: Decision,
    ) -> Self {
        Self {
            timestamp: utc_timestamp(),
            mode: config.mode().into(),
            status,
            rules_version: config.algo_rule.rules_version.clone(),
            context_hash: String::new(),
            decision,
            candidate_route_count: 0,
            market_pick_mode: config.market_pick_mode.as_str().into(),
            candidate_routes: Vec::new(),
            selected_route: json!({}),
            submitted_legs: Vec::new(),
            submission_attempted: false,
            error: String::new(),
            audit_path: config.audit_path.display().to_string(),
            row_type,
            cycle_id: String::new(),
            session_id: String::new(),
            sleeve: None,
            operator_review: None,
            run_config: None,
            tags: None,
            algo: None,
            sleeve_check: None,
            trimmed: Vec::new(),
        }
    }
}

/// The most a decision row may take in the decisions file, serialized (Phase 53 D6: the
/// dashboard reads up to 120 rows per poll).
pub const ROW_BUDGET_BYTES: usize = 32 * 1024;

/// One way to shorten a row toward a budget; the note it records, or `None` when it
/// changed nothing.
type Trim = fn(&mut AuditRow, usize) -> Option<String>;

fn row_size(row: &AuditRow) -> usize {
    serde_json::to_vec(row).map_or(usize::MAX, |bytes| bytes.len())
}

/// Shorten `row` until it serializes to at most `budget` bytes, least useful detail first:
/// the copies of the rule (`algo.rule`, the weights in `run_config.rule`; its `sha256`
/// names the file), the candidates' signals worth 0, the loudest tags down to 3 per kind,
/// the feed's name lists as counts, the listed candidates one by one down to 5 (held coins
/// stay), the loudest tags, the candidates down to 1, the candidates' signal terms, the
/// market-wide signals, the edge evidence, the candidates, the feed summary beyond its
/// fingerprint and kinds, `run_config` beyond the rule's identity. Each step's note goes
/// to `trimmed`. The decision (action, reason), `algo`'s verdict, orders, sizing and
/// sleeve are never cut; a row still over budget is written whole with a warning.
pub fn fit_budget(row: &mut AuditRow, budget: usize) {
    const STEPS: [Trim; 14] = [
        |r, _| drop_key(r.algo.as_mut(), "rule", "algo.rule"),
        |r, _| weights_as_counts(r),
        |r, _| drop_zero_signals(r),
        |r, _| keep_top_tags(r, 3),
        |r, _| names_as_counts(r),
        |r, budget| fewer_candidates(r, budget, 5),
        |r, _| keep_top_tags(r, 0),
        |r, budget| fewer_candidates(r, budget, 1),
        |r, _| drop_coin_signals(r),
        |r, _| drop_key(r.algo.as_mut(), "market_signals", "algo.market_signals"),
        |r, _| drop_key(r.algo.as_mut(), "edge", "algo.edge"),
        |r, _| drop_key(r.algo.as_mut(), "coins", "algo.coins"),
        |r, _| tags_fingerprint_only(r),
        |r, _| run_config_rule_only(r),
    ];
    for step in STEPS {
        if row_size(row) <= budget {
            return;
        }
        if let Some(note) = step(row, budget) {
            row.trimmed.push(note);
        }
    }
    let bytes = row_size(row);
    if bytes > budget {
        tracing::warn!(bytes, budget, "decision row over its size budget after every trim");
    }
}

/// Unlist the lowest candidates (held coins stay) one by one until the row fits or `min`
/// remain.
fn fewer_candidates(row: &mut AuditRow, budget: usize, min: usize) -> Option<String> {
    let mut cut = false;
    while row_size(row) > budget {
        let coins = coins(row)?;
        let listed = coins.iter().filter(|c| c["held"] != Value::Bool(true)).count();
        if listed <= min {
            break;
        }
        let last = coins.iter().rposition(|c| c["held"] != Value::Bool(true))?;
        coins.remove(last);
        cut = true;
    }
    let listed = coins(row)?.iter().filter(|c| c["held"] != Value::Bool(true)).count();
    cut.then(|| format!("algo.coins: {listed} candidate(s) listed"))
}

fn coins(row: &mut AuditRow) -> Option<&mut Vec<Value>> {
    row.algo.as_mut()?.get_mut("coins")?.as_array_mut()
}

fn keep_top_tags(row: &mut AuditRow, n: usize) -> Option<String> {
    let top = row.tags.as_mut()?.get_mut("top")?.as_object_mut()?;
    let mut cut = false;
    for tags in top.values_mut().filter_map(Value::as_array_mut) {
        cut |= tags.len() > n;
        tags.truncate(n);
    }
    cut.then(|| format!("tags.top: {n} per kind"))
}

/// The feed's name lists (`tradable`, `scored_only`, `proxied`) replaced by their lengths
/// (`tradable_count`, ...).
fn names_as_counts(row: &mut AuditRow) -> Option<String> {
    let tags = row.tags.as_mut()?.as_object_mut()?;
    let mut cut = false;
    for key in ["tradable", "scored_only", "proxied"] {
        if let Some(list) = tags.remove(key) {
            let count = match &list {
                Value::Array(items) => items.len(),
                Value::Object(items) => items.len(),
                _ => 0,
            };
            tags.insert(format!("{key}_count"), json!(count));
            cut = true;
        }
    }
    cut.then(|| "tags: tradable, scored_only, proxied as counts".to_owned())
}

/// Each sleeve's weights in `run_config.rule` as their number.
fn weights_as_counts(row: &mut AuditRow) -> Option<String> {
    let sleeves = row
        .run_config
        .as_mut()?
        .get_mut("rule")?
        .get_mut("sleeves")?
        .as_array_mut()?;
    let mut cut = false;
    for sleeve in sleeves.iter_mut().filter_map(Value::as_object_mut) {
        if let Some(weights) = sleeve.get("weights").and_then(Value::as_object) {
            let count = weights.len();
            sleeve.insert("weights".to_owned(), json!(count));
            cut = true;
        }
    }
    cut.then(|| "run_config.rule.sleeves: weights as counts (see rule.sha256)".to_owned())
}

fn drop_zero_signals(row: &mut AuditRow) -> Option<String> {
    let mut cut = false;
    for coin in coins(row)?.iter_mut() {
        if let Some(signals) = coin.get_mut("signals").and_then(Value::as_object_mut) {
            let before = signals.len();
            signals.retain(|_, v| v.as_f64() != Some(0.0));
            cut |= signals.len() < before;
        }
    }
    cut.then(|| "algo.coins: signals worth 0 left out".to_owned())
}

fn drop_coin_signals(row: &mut AuditRow) -> Option<String> {
    let mut cut = false;
    for coin in coins(row)?.iter_mut().filter_map(Value::as_object_mut) {
        cut |= coin.remove("signals").is_some();
    }
    cut.then(|| "algo.coins: signal terms left out".to_owned())
}

fn drop_key(value: Option<&mut Value>, key: &str, note: &str) -> Option<String> {
    let value = value?.as_object_mut()?.remove(key)?;
    (!value.is_null()).then(|| format!("{note} left out"))
}

/// Keep of the feed summary its status, `as_of`, age, digest and tags per kind.
fn tags_fingerprint_only(row: &mut AuditRow) -> Option<String> {
    let tags = row.tags.as_mut()?.as_object_mut()?;
    let before = tags.len();
    tags.retain(|key, _| matches!(key.as_str(), "status" | "reason" | "as_of" | "age_minutes" | "digest" | "kinds"));
    (tags.len() < before).then(|| "tags: only status, as_of, age, digest and kinds".to_owned())
}

/// Keep of `run_config` its mode, `rules_version` and the rule's path, version, style and
/// hash.
fn run_config_rule_only(row: &mut AuditRow) -> Option<String> {
    let config = row.run_config.as_mut()?.as_object_mut()?;
    let before = serde_json::to_vec(&*config).map_or(0, |b| b.len());
    config.retain(|key, _| matches!(key.as_str(), "mode" | "rules_version" | "rule"));
    if let Some(rule) = config.get_mut("rule").and_then(Value::as_object_mut) {
        rule.retain(|key, _| matches!(key.as_str(), "path" | "rules_version" | "style" | "sha256"));
    }
    let after = serde_json::to_vec(&*config).map_or(0, |b| b.len());
    (after < before).then(|| "run_config: only mode and the rule's identity".to_owned())
}

/// Python `datetime.now(timezone.utc).isoformat()`.
pub fn utc_timestamp() -> String {
    bb_core::time::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

/// Durable append-only writer for the decisions file.
#[derive(Debug, Clone)]
pub struct AuditLog {
    log: JsonlLog,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            log: JsonlLog::new(path),
        }
    }

    pub fn path(&self) -> &Path {
        self.log.path()
    }

    /// fsync'd append.
    pub fn append(&self, row: &AuditRow) -> Result<()> {
        Ok(self.log.append(row)?)
    }

    pub fn rows(&self) -> AuditRows {
        load_audit_rows(self.log.path())
    }
}

/// Hard safety: the reason live trading is halted, if it is. A failed route that submitted
/// at least one order (or whose submission was ambiguous) halts until an operator review row
/// follows it; an unreadable audit history also halts.
pub fn failed_route_halt(rows: &AuditRows) -> Option<String> {
    if let Some(error) = &rows.error {
        return Some(format!(
            "could not read Bot Spot audit history: {error}; no order submitted"
        ));
    }
    let last = rows.rows.iter().rposition(row_attempted_submission)?;
    let reviewed = rows.rows[last + 1..]
        .iter()
        .any(row_failed_submission_reviewed);
    (row_status(&rows.rows[last]) == "failed" && !reviewed).then(|| {
        "previous live route failed after submitting at least one order; holding until operator review".to_owned()
    })
}

/// Append the operator review marker that clears the failed-route halt.
pub fn mark_failed_route_reviewed(config: &TraderConfig, reason: &str) -> Result<PathBuf> {
    let log = AuditLog::new(&config.audit_path);
    let rows = log.rows();
    if let Some(error) = rows.error {
        return Err(TraderError::safety(format!(
            "could not read Bot Spot audit history: {error}"
        )));
    }
    let failed = rows
        .rows
        .iter()
        .rposition(|row| row_status(row) == "failed" && row_attempted_submission(row))
        .ok_or_else(|| TraderError::safety("no failed submitted Bot Spot route needs review"))?;
    if rows.rows[failed + 1..]
        .iter()
        .any(row_failed_submission_reviewed)
    {
        return Err(TraderError::safety(
            "latest failed submitted Bot Spot route is already reviewed",
        ));
    }
    let failed_row = &rows.rows[failed];
    let mut row = AuditRow::new(
        config,
        RowType::Review,
        RowStatus::Reviewed,
        Decision::hold(reason),
    );
    row.operator_review = Some(OperatorReview {
        failed_submission_reviewed: true,
        reviewed_failed_timestamp: failed_row
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        reviewed_failed_route_id: row_route_id(failed_row),
        reason: reason.to_owned(),
    });
    log.append(&row)?;
    Ok(config.audit_path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::universe::tests::config;

    fn failed_row() -> Value {
        json!({"timestamp": "2026-10-01T00:00:00+00:00", "status": "failed", "submission_attempted": true,
               "decision": {"route_id": "USDT->ETH:abc"}, "submitted_legs": [{"symbol": "ETH/USDT"}]})
    }

    #[test]
    fn failed_route_halts_until_operator_review() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.jsonl");
        let audit_text = audit.to_string_lossy().into_owned();
        let config = config(&[("BOT_SPOT_AUDIT_PATH", &audit_text)]);

        assert!(failed_route_halt(&AuditLog::new(&audit).rows()).is_none());
        assert!(
            mark_failed_route_reviewed(&config, DEFAULT_REVIEW_REASON)
                .unwrap_err()
                .to_string()
                .contains("no failed submitted")
        );

        std::fs::write(
            &audit,
            format!("{}\n{}\n", failed_row(), json!({"status": "held"})),
        )
        .unwrap();
        let reason = failed_route_halt(&AuditLog::new(&audit).rows()).unwrap();
        assert!(reason.contains("previous live route failed"), "{reason}");

        mark_failed_route_reviewed(&config, "checked balances").unwrap();
        let rows = AuditLog::new(&audit).rows();
        assert!(failed_route_halt(&rows).is_none());
        let review = rows.rows.last().unwrap();
        assert_eq!(review["status"], "reviewed");
        assert_eq!(review["row_type"], "review");
        assert_eq!(
            review["operator_review"]["reviewed_failed_route_id"],
            "USDT->ETH:abc"
        );
        assert_eq!(review["operator_review"]["reason"], "checked balances");
        assert_eq!(review["decision"]["spend_amount"], Value::Null);
        assert!(
            mark_failed_route_reviewed(&config, DEFAULT_REVIEW_REASON)
                .unwrap_err()
                .to_string()
                .contains("already reviewed")
        );
    }

    #[test]
    fn failures_without_submission_and_later_successes_do_not_halt() {
        let not_submitted =
            json!({"status": "failed", "submission_attempted": false, "submitted_legs": []});
        let rows = AuditRows {
            rows: vec![not_submitted],
            error: None,
        };
        assert!(failed_route_halt(&rows).is_none());
        let rows = AuditRows {
            rows: vec![
                failed_row(),
                json!({"status": "submitted", "submission_attempted": true}),
            ],
            error: None,
        };
        assert!(failed_route_halt(&rows).is_none());
        let unreadable = AuditRows {
            rows: Vec::new(),
            error: Some("permission denied".into()),
        };
        assert!(
            failed_route_halt(&unreadable)
                .unwrap()
                .contains("could not read")
        );
    }
}

//! Audit rows (Python schema + agent fields), the failed-route halt and the operator review
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

/// One recorded host tool call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCallEntry {
    pub name: String,
    pub args: Value,
    pub is_error: bool,
    pub ms: u64,
}

/// Token and cost usage of one cycle turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UsageEntry {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub cost_usd: Decimal,
}

/// The bot's explicit end-of-cycle statement.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct EndCycle {
    pub action: String,
    pub summary: String,
}

/// One failed model attempt of a cycle turn (the turn was retried on the next model).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttemptEntry {
    /// `provider/model-id` the attempt ran on.
    pub model: String,
    /// `timeout`, `error`, `crashed`, `completed` (usage-limited) or `compaction`.
    pub status: String,
    pub error: String,
}

/// Agent fields of a cycle summary row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentRecord {
    /// The model that produced the final outcome (its last assistant message).
    pub model: String,
    pub thinking: String,
    /// `completed`, `aborted`, `error`, `timeout`, `crashed`, `skipped`.
    pub turn_status: String,
    pub tool_calls: Vec<ToolCallEntry>,
    pub usage: UsageEntry,
    pub end_cycle: Option<EndCycle>,
    /// Failed attempts before the final one (model failover), oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<AttemptEntry>,
    /// The final outcome came from a model other than the primary.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fallback_active: bool,
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
    pub prompt_version: String,
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
    /// Route rows: always `false` on disk (the alert is sent after the durable write).
    /// Cycle summaries: every route alert of the cycle was delivered.
    pub telegram_sent: bool,
    /// Cycle summaries only: route ids whose alert was delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram_sent_routes: Option<Vec<String>>,
    pub row_type: RowType,
    pub cycle_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_review: Option<OperatorReview>,
    /// Cycle summaries: the trader's non-secret run configuration
    /// ([`TraderConfig::run_config`]), so each cycle says what setup produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_config: Option<Value>,
    /// Cycle summaries (Phase 49): the cycle's schizo state ([`crate::schizo::SchizoState`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schizo: Option<Value>,
    /// Cycle summaries (Phase 50): the long-term value voice ([`crate::mdp::MdpNow`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdp: Option<Value>,
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
            prompt_version: config.prompt_version.clone(),
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
            telegram_sent: false,
            telegram_sent_routes: None,
            row_type,
            cycle_id: String::new(),
            session_id: String::new(),
            agent: None,
            operator_review: None,
            run_config: None,
            schizo: None,
            mdp: None,
        }
    }
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

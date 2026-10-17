//! Trade memory from the audit JSONL: closed/open outcomes, PnL, current holding and the
//! loss streak. Information for Claude only; nothing here blocks a trade.

use std::collections::BTreeMap;
use std::path::Path;

use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

use bb_core::money::{BPS_PER_UNIT, to_f64};

/// Python reads the last 500 audit lines.
pub const AUDIT_TAIL_LINES: usize = 500;

/// Audit rows (JSON objects) and the read error, if any.
#[derive(Debug, Clone, Default)]
pub struct AuditRows {
    pub rows: Vec<Value>,
    pub error: Option<String>,
}

/// Last [`AUDIT_TAIL_LINES`] parseable object rows; a missing file is empty history.
pub fn load_audit_rows(path: &Path) -> AuditRows {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return AuditRows::default(),
        Err(error) => {
            return AuditRows {
                rows: Vec::new(),
                error: Some(error.to_string()),
            };
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(AUDIT_TAIL_LINES);
    AuditRows {
        rows: lines[start..]
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(Value::is_object)
            .collect(),
        error: None,
    }
}

/// Lower-case `status` of a row.
pub fn row_status(row: &Value) -> String {
    row.get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase()
}

/// `submission_attempted` truthy or any `submitted_legs`.
pub fn row_attempted_submission(row: &Value) -> bool {
    row.get("submission_attempted").is_some_and(truthy)
        || row.get("submitted_legs").is_some_and(truthy)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// An operator review marker that clears a failed-route halt.
pub fn row_failed_submission_reviewed(row: &Value) -> bool {
    matches!(row_status(row).as_str(), "reviewed" | "operator_reviewed")
        && row.pointer("/operator_review/failed_submission_reviewed") == Some(&Value::Bool(true))
}

/// `decision.route_id`, else `selected_route.route_id`.
pub fn row_route_id(row: &Value) -> String {
    ["/decision/route_id", "/selected_route/route_id"]
        .iter()
        .find_map(|pointer| row.pointer(pointer).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

/// Upper-case `decision.<field>`, else `selected_route.<field>` (`source_asset`, `destination_asset`).
pub fn row_asset(row: &Value, field: &str) -> String {
    ["decision", "selected_route"]
        .iter()
        .find_map(|section| {
            row.get(section)
                .and_then(|s| s.get(field))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default()
        .to_uppercase()
}

/// Finite decimal from a JSON number or numeric string.
pub fn value_decimal(value: Option<&Value>) -> Option<Decimal> {
    match value? {
        Value::Number(number) => {
            let text = number.to_string();
            Decimal::from_str_exact(&text)
                .or_else(|_| Decimal::from_scientific(&text))
                .ok()
        }
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn selected_decimal(row: &Value, field: &str) -> Option<Decimal> {
    value_decimal(row.get("selected_route").and_then(|s| s.get(field)))
}

fn pnl_bps(entry: Decimal, exit: Decimal) -> Option<f64> {
    (entry > Decimal::ZERO).then(|| to_f64((exit / entry - Decimal::ONE) * BPS_PER_UNIT))
}

/// One submitted route and how it has done since.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeOutcome {
    pub timestamp: String,
    pub route_id: String,
    pub source_asset: String,
    pub destination_asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub entry_value_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub exit_value_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub current_value_usdt: Option<Decimal>,
    pub pnl_bps: Option<f64>,
    /// `closed`, `open` or `unknown`.
    pub status: &'static str,
    pub route_friction_bps: Option<f64>,
    /// Set on sells an exit plan made between cycles: the spec, the rule that fired,
    /// the share and the trigger price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_plan: Option<String>,
}

/// The asset bought by the last submitted route.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurrentHolding {
    /// `open`, `not_current` or `unknown`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::arbitrary_precision_option"
    )]
    pub amount: Option<Decimal>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::arbitrary_precision_option"
    )]
    pub entry_value_usdt: Option<Decimal>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::arbitrary_precision_option"
    )]
    pub current_value_usdt: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_pnl_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
}

/// Trade memory packet (Python `_trade_memory_context`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeMemory {
    /// `ready`, `empty` or `unavailable`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub recent_trade_count: usize,
    /// Consecutive trailing losing outcomes (information only).
    pub recent_loss_streak: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_holding: Option<CurrentHolding>,
    pub recent_outcomes: Vec<TradeOutcome>,
}

impl TradeMemory {
    /// Build from audit rows. `value_usdt(asset, amount)` marks open positions to market.
    pub fn from_rows(
        rows: &AuditRows,
        free: &BTreeMap<String, Decimal>,
        value_usdt: impl Fn(&str, Decimal) -> Option<Decimal>,
        limit: usize,
    ) -> Self {
        if let Some(error) = &rows.error {
            return Self::empty("unavailable", error.clone());
        }
        let submitted: Vec<&Value> = rows
            .rows
            .iter()
            .filter(|row| row_status(row) == "submitted" && row_attempted_submission(row))
            .collect();
        let Some(last) = submitted.last() else {
            return Self::empty(
                "empty",
                "no submitted Claude Spot routes in audit history".into(),
            );
        };
        let outcomes = outcomes(&submitted, free, &value_usdt);
        let recent = if limit == 0 {
            outcomes
        } else {
            outcomes[outcomes.len().saturating_sub(limit)..].to_vec()
        };
        Self {
            status: "ready",
            reason: None,
            recent_trade_count: submitted.len(),
            recent_loss_streak: loss_streak(&recent),
            current_holding: Some(current_holding(last, free, &value_usdt)),
            recent_outcomes: recent,
        }
    }

    fn empty(status: &'static str, reason: String) -> Self {
        Self {
            status,
            reason: Some(reason),
            recent_trade_count: 0,
            recent_loss_streak: 0,
            current_holding: None,
            recent_outcomes: Vec::new(),
        }
    }
}

fn outcomes(
    submitted: &[&Value],
    free: &BTreeMap<String, Decimal>,
    value_usdt: &impl Fn(&str, Decimal) -> Option<Decimal>,
) -> Vec<TradeOutcome> {
    submitted
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let destination = row_asset(row, "destination_asset");
            let entry = selected_decimal(row, "estimated_output_usdt");
            let exit = if destination.is_empty() {
                None
            } else {
                submitted[index + 1..]
                    .iter()
                    .find(|later| row_asset(later, "source_asset") == destination)
                    .and_then(|later| selected_decimal(later, "source_value_usdt"))
            };
            let mut outcome = TradeOutcome {
                timestamp: row
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                route_id: row_route_id(row),
                source_asset: row_asset(row, "source_asset"),
                destination_asset: destination.clone(),
                entry_value_usdt: entry,
                exit_value_usdt: exit,
                current_value_usdt: None,
                pnl_bps: None,
                status: "unknown",
                route_friction_bps: selected_decimal(row, "immediate_friction_bps").map(to_f64),
                exit_plan: (row.get("cycle_id").and_then(Value::as_str)
                    == Some(crate::cycle::EXIT_CYCLE_ID))
                .then(|| {
                    row.pointer("/decision/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("exit plan")
                        .to_owned()
                }),
            };
            if let (Some(entry), Some(exit)) = (entry, exit) {
                outcome.status = "closed";
                outcome.pnl_bps = pnl_bps(entry, exit);
            } else if let Some(entry) = entry
                && let Some(amount) = free.get(&destination).filter(|a| **a > Decimal::ZERO)
                && let Some(current) = value_usdt(&destination, *amount)
            {
                outcome.status = "open";
                outcome.current_value_usdt = Some(current);
                outcome.pnl_bps = pnl_bps(entry, current);
            }
            outcome
        })
        .collect()
}

fn current_holding(
    last: &Value,
    free: &BTreeMap<String, Decimal>,
    value_usdt: &impl Fn(&str, Decimal) -> Option<Decimal>,
) -> CurrentHolding {
    let blank = CurrentHolding {
        status: "unknown",
        asset: None,
        reason: Some("last route destination is unknown"),
        amount: None,
        entry_value_usdt: None,
        current_value_usdt: None,
        unrealized_pnl_bps: None,
        entered_at: None,
        route_id: None,
    };
    let destination = row_asset(last, "destination_asset");
    if destination.is_empty() {
        return blank;
    }
    let Some(amount) = free
        .get(&destination)
        .copied()
        .filter(|a| *a > Decimal::ZERO)
    else {
        return CurrentHolding {
            status: "not_current",
            asset: Some(destination),
            reason: Some("last route destination is not a positive free balance"),
            ..blank
        };
    };
    let entry = selected_decimal(last, "estimated_output_usdt");
    let current = value_usdt(&destination, amount);
    CurrentHolding {
        status: "open",
        reason: None,
        amount: Some(amount),
        entry_value_usdt: entry,
        current_value_usdt: current,
        unrealized_pnl_bps: entry
            .zip(current)
            .and_then(|(entry, current)| pnl_bps(entry, current)),
        entered_at: Some(
            last.get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        route_id: Some(row_route_id(last)),
        asset: Some(destination),
    }
}

/// Consecutive trailing outcomes with negative PnL (stops at unknown or non-negative).
pub fn loss_streak(outcomes: &[TradeOutcome]) -> usize {
    outcomes
        .iter()
        .rev()
        .take_while(|outcome| outcome.pnl_bps.is_some_and(|pnl| pnl < 0.0))
        .count()
}

#[cfg(test)]
pub(crate) mod tests {
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;

    pub(crate) fn submitted_row(
        source: &str,
        destination: &str,
        entry: f64,
        source_value: f64,
    ) -> Value {
        json!({
            "timestamp": "2026-10-01T00:00:00+00:00",
            "status": "submitted",
            "submission_attempted": true,
            "decision": {"action": "rebalance", "route_id": format!("{source}->{destination}:abc"),
                         "source_asset": source, "destination_asset": destination},
            "selected_route": {"estimated_output_usdt": entry, "source_value_usdt": source_value,
                               "immediate_friction_bps": 12.5},
            "submitted_legs": [{"symbol": "X"}],
        })
    }

    fn value(asset: &str, amount: Decimal) -> Option<Decimal> {
        match asset {
            "USDT" => Some(amount),
            "ETH" => Some(amount * dec!(3000)),
            _ => None,
        }
    }

    #[test]
    fn trade_memory_records_current_holding_pnl() {
        let rows = AuditRows {
            rows: vec![submitted_row("USDT", "ETH", 100.0, 100.5)],
            error: None,
        };
        let free = BTreeMap::from([("ETH".to_owned(), dec!(0.035))]);
        let memory = TradeMemory::from_rows(&rows, &free, value, 8);
        assert_eq!(memory.status, "ready");
        assert_eq!(memory.recent_trade_count, 1);
        let holding = memory.current_holding.unwrap();
        assert_eq!(holding.status, "open");
        assert_eq!(holding.current_value_usdt, Some(dec!(105.000)));
        assert!((holding.unrealized_pnl_bps.unwrap() - 500.0).abs() < 1e-9);
        assert_eq!(memory.recent_outcomes[0].status, "open");
    }

    #[test]
    fn closed_outcomes_and_loss_streak() {
        let rows = AuditRows {
            rows: vec![
                submitted_row("USDT", "ETH", 100.0, 100.0),
                json!({"status": "held", "decision": {"action": "hold"}}),
                {
                    let mut sell = submitted_row("ETH", "USDT", 98.0, 95.0);
                    sell["cycle_id"] = json!("exit-plan");
                    sell["decision"]["reason"] =
                        json!("exit plan atr=3:1h,stretch=2:1 fired (stretch, live)");
                    sell
                },
                submitted_row("USDT", "BTC", 97.0, 99.0),
            ],
            error: None,
        };
        let memory = TradeMemory::from_rows(&rows, &BTreeMap::new(), value, 8);
        let outcomes = &memory.recent_outcomes;
        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0].status, "closed");
        assert!((outcomes[0].pnl_bps.unwrap() - -500.0).abs() < 1e-9);
        assert_eq!(outcomes[1].status, "closed");
        assert!(outcomes[1].pnl_bps.unwrap() > 0.0);
        assert_eq!(outcomes[2].status, "unknown");
        // The plan's sell says which rule sold; the bot's own routes carry nothing.
        assert_eq!(
            outcomes[1].exit_plan.as_deref(),
            Some("exit plan atr=3:1h,stretch=2:1 fired (stretch, live)")
        );
        assert_eq!(outcomes[0].exit_plan, None);
        assert_eq!(memory.recent_loss_streak, 0);
        assert_eq!(
            memory.current_holding.unwrap().status,
            "not_current",
            "BTC not held any more"
        );

        let losing = vec![
            TradeOutcome {
                pnl_bps: Some(-1.0),
                ..outcomes[0].clone()
            },
            TradeOutcome {
                pnl_bps: Some(-2.0),
                ..outcomes[0].clone()
            },
        ];
        assert_eq!(loss_streak(&losing), 2);
    }

    #[test]
    fn unreadable_and_empty_history() {
        let unavailable = TradeMemory::from_rows(
            &AuditRows {
                rows: Vec::new(),
                error: Some("denied".into()),
            },
            &BTreeMap::new(),
            value,
            8,
        );
        assert_eq!(unavailable.status, "unavailable");
        let empty = TradeMemory::from_rows(&AuditRows::default(), &BTreeMap::new(), value, 8);
        assert_eq!(empty.status, "empty");
    }

    #[test]
    fn load_audit_rows_skips_garbage_and_keeps_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut text = String::new();
        for index in 0..(AUDIT_TAIL_LINES + 5) {
            text.push_str(&format!("{{\"n\": {index}}}\n"));
        }
        text.push_str("not json\n[1]\n");
        std::fs::write(&path, text).unwrap();
        let rows = load_audit_rows(&path);
        assert!(rows.error.is_none());
        assert_eq!(rows.rows.len(), AUDIT_TAIL_LINES - 2);
        assert_eq!(load_audit_rows(&dir.path().join("missing")).rows.len(), 0);
    }
}

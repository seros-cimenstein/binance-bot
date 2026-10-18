//! Dashboard view of the audit log: submitted routes (valued by [`bb_trader::trades`]),
//! failed routes and cycle summaries.
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use bb_trader::memory::{row_asset, row_attempted_submission, row_status, value_decimal};
use bb_trader::trades::{RouteRecord, route_record};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

/// A route that failed after reaching the exchange.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailedRoute {
    pub timestamp: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub error: String,
}

/// One cycle summary: who ran it and under which setup.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleRecord {
    pub timestamp: String,
    pub cycle_id: String,
    pub prompt_version: String,
    pub session_id: String,
    pub model: String,
    pub thinking: String,
    pub market_pick_mode: String,
    pub cost_usd: Option<Decimal>,
    /// The trader's run configuration (cycles from 2026-10-17 on).
    pub run_config: Option<Value>,
}

/// The audit log reduced to what the history needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedAudit {
    pub routes: Vec<RouteRecord>,
    pub failed: Vec<FailedRoute>,
    pub cycles: Vec<CycleRecord>,
    pub error: Option<String>,
}

/// Stream the whole audit file (a missing file is an empty history; torn lines are skipped).
pub fn parse_audit(path: &Path) -> ParsedAudit {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ParsedAudit::default(),
        Err(error) => {
            return ParsedAudit {
                error: Some(format!("cannot read {}: {error}", path.display())),
                ..ParsedAudit::default()
            };
        }
    };
    let mut parsed = ParsedAudit::default();
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                parsed.error = Some(format!("cannot read {}: {error}", path.display()));
                break;
            }
        };
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if text(&row, "/row_type") == "cycle_summary" {
            parsed.cycles.push(CycleRecord {
                timestamp: text(&row, "/timestamp"),
                cycle_id: text(&row, "/cycle_id"),
                prompt_version: text(&row, "/prompt_version"),
                session_id: text(&row, "/session_id"),
                model: text(&row, "/agent/model"),
                thinking: text(&row, "/agent/thinking"),
                market_pick_mode: text(&row, "/market_pick_mode"),
                cost_usd: value_decimal(row.pointer("/agent/usage/cost_usd")),
                run_config: row.get("run_config").filter(|v| v.is_object()).cloned(),
            });
            continue;
        }
        if !row_attempted_submission(&row) {
            continue;
        }
        match row_status(&row).as_str() {
            "submitted" => parsed.routes.push(route_record(&row)),
            "failed" => parsed.failed.push(FailedRoute {
                timestamp: text(&row, "/timestamp"),
                source_asset: row_asset(&row, "source_asset"),
                destination_asset: row_asset(&row, "destination_asset"),
                error: text(&row, "/error"),
            }),
            _ => {}
        }
    }
    parsed
}

fn text(row: &Value, pointer: &str) -> String {
    row.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use bb_trader::trades::Valuation;
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;
    #[test]
    fn parse_keeps_submitted_and_failed_routes_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let rows = [
            json!({"timestamp": "t1", "status": "submitted", "submission_attempted": true,
                   "decision": {"source_asset": "usdt", "destination_asset": "sol", "reason": "why"},
                   "selected_route": {"source_value_usdt": 10, "estimated_output_usdt": "9.9",
                                      "input_amount": 10, "output_amount": 0.05},
                   "submitted_legs": [{}]}),
            json!({"timestamp": "t1b", "status": "submitted", "submission_attempted": true, "cycle_id": "exit-plan",
                   "decision": {"source_asset": "SOL", "destination_asset": "USDT", "reason": "exit plan fired"}}),
            json!({"timestamp": "t2", "status": "submitted", "row_type": "cycle_summary"}),
            json!({"timestamp": "t3", "status": "held"}),
            json!({"timestamp": "t4", "status": "failed", "submission_attempted": true, "error": "boom",
                   "decision": {"source_asset": "SOL", "destination_asset": "ETH"}}),
            json!({"timestamp": "t5", "status": "failed", "error": "network, nothing sent"}),
        ];
        let mut text: String = rows.iter().map(|row| format!("{row}\n")).collect();
        text.push_str("{torn");
        std::fs::write(&path, text).unwrap();

        let parsed = parse_audit(&path);

        assert_eq!(parsed.error, None);
        assert_eq!(parsed.routes.len(), 2);
        let first = &parsed.routes[0];
        assert_eq!((first.source_asset.as_str(), first.destination_asset.as_str()), ("USDT", "SOL"));
        // The leg has no order data, so the quote values the route.
        assert_eq!(first.valuation, Valuation::Quotes);
        assert_eq!(first.quoted_cost_usdt, Some(dec!(0.1)));
        assert_eq!(first.legs, 1);
        assert!(!first.exit_plan);
        assert!(parsed.routes[1].exit_plan);
        assert_eq!(parsed.failed.len(), 1);
        assert_eq!(parsed.failed[0].error, "boom");
        assert_eq!(parse_audit(&dir.path().join("missing")), ParsedAudit::default());
    }
}

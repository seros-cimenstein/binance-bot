//! The trader's persisted state (`BOT_SPOT_STATE_PATH`): the cycle counter, the last
//! cycle's summary and, since Phase 53, the sleeves' holdings ([`crate::sleeves`]).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::audit::EndCycle;
use crate::error::Result;
use crate::sleeves::SleeveState;

/// What one route execution ended as.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionSummary {
    pub route_id: String,
    pub status: String,
    #[serde(default)]
    pub error: String,
}

/// The previous cycle, as remembered across processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastCycle {
    pub cycle_id: String,
    pub finished_at: String,
    pub status: String,
    pub end_cycle: Option<EndCycle>,
    pub executions: Vec<ExecutionSummary>,
    #[serde(default)]
    pub error: String,
}

/// Persisted trader bookkeeping. Fields of the LLM era (cost, session, brain outages) in an
/// older file are ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraderState {
    #[serde(default)]
    pub cycle_count: u64,
    #[serde(default)]
    pub last_cycle: Option<LastCycle>,
    /// `None`: no sleeve state yet (no file, or a file from before Phase 53): migrate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleeves: Option<SleeveState>,
}

impl TraderState {
    /// Read the state file. A missing file is a fresh state and a file without `sleeves`
    /// has none yet (both migrate); an unreadable file, broken JSON or a malformed
    /// `sleeves` (including `null`) is an error: the trader halts rather than forget what
    /// its sleeves own.
    pub fn load_strict(path: &Path) -> std::result::Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
        let sleeves = match value.get("sleeves") {
            None => None,
            Some(sleeves) => Some(
                SleeveState::deserialize(sleeves)
                    .map_err(|e| format!("{}: malformed sleeves: {e}", path.display()))?,
            ),
        };
        let mut state: Self =
            serde_json::from_value(value).map_err(|e| format!("{}: {e}", path.display()))?;
        state.sleeves = sleeves;
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        Ok(bb_core::fsutil::write_json_atomic(path, self)?)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn an_llm_era_state_file_loads_and_saves_without_its_old_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"cycle_count":4,"llm_cost_date":"2026-10-20","llm_cost_usd":"0.3","session_id":"s",
                "brain_down_cycles":2,"last_cycle":{"cycle_id":"c","finished_at":"2026-10-20T10:00:00Z",
                "status":"held","turn_status":"completed","end_cycle":{"action":"hold","summary":"x"},
                "executions":[]}}"#,
        )
        .unwrap();
        let mut state = TraderState::load_strict(&path).unwrap();
        assert_eq!(state.cycle_count, 4);
        assert_eq!(state.last_cycle.as_ref().unwrap().status, "held");
        // Written before Phase 53: no sleeve state yet (the trader migrates it).
        assert_eq!(state.sleeves, None);
        state.cycle_count += 1;
        state.save(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("llm_cost") && !saved.contains("session_id"),
            "{saved}"
        );
        assert_eq!(TraderState::load_strict(&path).unwrap().cycle_count, 5);
    }

    #[test]
    fn sleeve_state_is_absent_only_when_missing_and_malformed_state_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        // No file: fresh, no sleeves (migrate).
        assert_eq!(
            TraderState::load_strict(&path).unwrap(),
            TraderState::default()
        );
        // Saved sleeves load back exactly.
        let mut state = TraderState::default();
        let mut sleeves = SleeveState {
            since: "t0".into(),
            ..Default::default()
        };
        sleeves.book_buy("fast", "ETH", dec!(0.0512345678901234), "t1");
        state.sleeves = Some(sleeves.clone());
        state.save(&path).unwrap();
        assert_eq!(
            TraderState::load_strict(&path).unwrap().sleeves,
            Some(sleeves)
        );
        // Everything else fails closed, with the reason.
        for (text, reason) in [
            ("{", "is not valid JSON"),
            (
                r#"{"cycle_count": 1, "sleeves": null}"#,
                "malformed sleeves",
            ),
            (
                r#"{"sleeves": {"holdings": {"fast": {"asset": "ETH"}}, "since": "t"}}"#,
                "missing field `qty`",
            ),
            (
                r#"{"sleeves": {"holdings": {}, "since": "t", "extra": 1}}"#,
                "unknown field `extra`",
            ),
            (
                r#"{"sleeves": {"holdings": {"fast": {"asset": "ETH", "qty": "x", "opened_at": "t"}}, "since": "t"}}"#,
                "malformed sleeves",
            ),
            (
                r#"{"cycle_count": "many", "sleeves": {"holdings": {}, "since": "t"}}"#,
                "invalid type",
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            let err = TraderState::load_strict(&path).unwrap_err();
            assert!(err.contains(reason), "{text}: {err}");
        }
        // A directory where the file should be: unreadable.
        let err = TraderState::load_strict(dir.path()).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
    }
}

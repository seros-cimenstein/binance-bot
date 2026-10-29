//! The trader's small persisted state (`BOT_SPOT_STATE_PATH`): the cycle counter and the
//! last cycle's summary, read back after a restart (cycle cadence, dashboard).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::audit::EndCycle;
use crate::error::Result;

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
}

impl TraderState {
    /// Missing or unreadable state starts fresh (it holds no safety-relevant data).
    pub fn load(path: &Path) -> Self {
        match bb_core::fsutil::read_json(path) {
            Ok(state) => state.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring unreadable trader state");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        Ok(bb_core::fsutil::write_json_atomic(path, self)?)
    }
}

#[cfg(test)]
mod tests {
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
        let mut state = TraderState::load(&path);
        assert_eq!(state.cycle_count, 4);
        assert_eq!(state.last_cycle.as_ref().unwrap().status, "held");
        state.cycle_count += 1;
        state.save(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("llm_cost") && !saved.contains("session_id"),
            "{saved}"
        );
        assert_eq!(TraderState::load(&path).cycle_count, 5);
        // Unreadable: start fresh.
        std::fs::write(&path, "{").unwrap();
        assert_eq!(TraderState::load(&path), TraderState::default());
    }
}

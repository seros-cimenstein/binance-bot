//! Durable trader notebook (`journal_write` / `journal_read`) and the small persisted agent
//! state (cycle counter, daily LLM cost, last-cycle summary). Both survive compaction,
//! session resets and restarts.

use std::path::{Path, PathBuf};

use bb_core::audit::JsonlLog;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::audit::{EndCycle, utc_timestamp};
use crate::error::Result;

/// Maximum characters of one journal entry.
pub const MAX_ENTRY_CHARS: usize = 4_000;

/// One notebook entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub timestamp: String,
    pub cycle_id: String,
    pub text: String,
}

/// Append-only JSONL notebook.
#[derive(Debug, Clone)]
pub struct Journal {
    log: JsonlLog,
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            log: JsonlLog::new(path),
        }
    }

    pub fn write(&self, cycle_id: &str, text: &str) -> Result<JournalEntry> {
        let entry = JournalEntry {
            timestamp: utc_timestamp(),
            cycle_id: cycle_id.to_owned(),
            text: text.trim().to_owned(),
        };
        self.log.append(&entry)?;
        Ok(entry)
    }

    /// Newest `limit` entries, oldest first.
    pub fn read(&self, limit: usize) -> Result<Vec<JournalEntry>> {
        Ok(self.log.read_tail(limit)?)
    }
}

/// What one route execution ended as (for the next cycle prompt).
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
    pub turn_status: String,
    pub end_cycle: Option<EndCycle>,
    pub executions: Vec<ExecutionSummary>,
    #[serde(default)]
    pub error: String,
}

/// Persisted agent bookkeeping (`CLAUDE_SPOT_STATE_PATH`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub cycle_count: u64,
    /// UTC date (`YYYY-MM-DD`) `llm_cost_usd` belongs to.
    pub llm_cost_date: String,
    pub llm_cost_usd: Decimal,
    pub session_id: Option<String>,
    pub last_cycle: Option<LastCycle>,
}

impl AgentState {
    /// Missing or unreadable state starts fresh (it holds no safety-relevant data).
    pub fn load(path: &Path) -> Self {
        match bb_core::fsutil::read_json(path) {
            Ok(state) => state.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring unreadable agent state");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        Ok(bb_core::fsutil::write_json_atomic(path, self)?)
    }

    /// LLM spend so far on `today`.
    pub fn cost_on(&self, today: &str) -> Decimal {
        if self.llm_cost_date == today {
            self.llm_cost_usd
        } else {
            Decimal::ZERO
        }
    }

    pub fn add_cost(&mut self, today: &str, cost: Decimal) {
        if self.llm_cost_date != today {
            self.llm_cost_date = today.to_owned();
            self.llm_cost_usd = Decimal::ZERO;
        }
        self.llm_cost_usd += cost;
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn journal_round_trip_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path().join("journal.jsonl"));
        for index in 0..5 {
            journal.write("c1", &format!(" thesis {index} ")).unwrap();
        }
        let entries = journal.read(2).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec!["thesis 3", "thesis 4"]
        );
    }

    #[test]
    fn daily_cost_resets_on_new_utc_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = AgentState::load(&path);
        state.add_cost("2026-10-10", dec!(0.5));
        state.add_cost("2026-10-10", dec!(0.25));
        state.save(&path).unwrap();
        let mut state = AgentState::load(&path);
        assert_eq!(state.cost_on("2026-10-10"), dec!(0.75));
        assert_eq!(state.cost_on("2026-10-11"), Decimal::ZERO);
        state.add_cost("2026-10-11", dec!(0.1));
        assert_eq!(state.cost_on("2026-10-11"), dec!(0.1));
    }
}

//! Guarded AI allocation advisor (refactor `src/capital/advisor.py`): the deterministic plan is
//! always computed first; a proposal (file or the bot) may only move budgets inside the
//! deterministic bounds (global cap, allocatable pool, minimum), and any failure falls back.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::allocator::{AllocationPlan, AllocationStatus, ModuleAllocation, Scoring, budget};
use crate::config::CapitalPoolConfig;
use crate::scorecard::{ModuleScorecard, decimal_value, value_decimal};

/// Everything an advisor sees for one decision.
#[derive(Debug, Clone, PartialEq)]
pub struct AllocationContext {
    pub config: CapitalPoolConfig,
    pub scoring: Scoring,
    pub total_equity_usdt: Decimal,
    pub module_ids: Vec<String>,
    pub scorecards: BTreeMap<String, ModuleScorecard>,
    pub fallback_plan: AllocationPlan,
}

impl AllocationContext {
    /// The refactor's advisor context JSON (plus `scoring`).
    pub fn to_json(&self) -> Value {
        json!({
            "config": {
                "reserve_pct": decimal_value(self.config.reserve_pct),
                "min_module_allocation_usdt": decimal_value(self.config.min_module_allocation_usdt),
                "max_module_allocation_usdt": self.config.max_module_allocation_usdt.map(decimal_value),
            },
            "scoring": self.scoring.as_str(),
            "total_equity_usdt": decimal_value(self.total_equity_usdt),
            "module_ids": self.module_ids,
            "scorecards": self.scorecards,
            "fallback_plan": self.fallback_plan,
        })
    }
}

/// Source of raw (unvalidated) allocation proposals.
#[async_trait]
pub trait ProposalProvider: Send + Sync {
    /// A JSON proposal or `None` to keep the fallback. `timeout` bounds the call.
    async fn proposal(&self, context: &AllocationContext, timeout: Duration) -> Option<Value>;
}

/// `CAPITAL_AI_ADVISOR_FILE`: a proposal JSON object on disk.
#[derive(Debug, Clone)]
pub struct FileProposalProvider {
    path: PathBuf,
}

impl FileProposalProvider {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

#[async_trait]
impl ProposalProvider for FileProposalProvider {
    async fn proposal(&self, _context: &AllocationContext, _timeout: Duration) -> Option<Value> {
        let bytes = tokio::fs::read(&self.path).await.ok()?;
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
    }
}

/// Providers tried in order until one returns a proposal.
pub struct FirstAvailable(pub Vec<Arc<dyn ProposalProvider>>);

#[async_trait]
impl ProposalProvider for FirstAvailable {
    async fn proposal(&self, context: &AllocationContext, timeout: Duration) -> Option<Value> {
        for provider in &self.0 {
            if let Some(proposal) = provider.proposal(context, timeout).await {
                return Some(proposal);
            }
        }
        None
    }
}

/// Result of one guarded advisory decision.
#[derive(Debug, Clone, PartialEq)]
pub struct AdvisedPlan {
    pub plan: AllocationPlan,
    /// The raw proposal, when a provider produced one.
    pub proposal: Option<Value>,
    /// Whether the proposal changed the plan (false = deterministic fallback).
    pub applied: bool,
}

/// Deterministic fallback + guarded proposal.
#[derive(Clone)]
pub struct GuardedAdvisor {
    provider: Arc<dyn ProposalProvider>,
    timeout: Duration,
}

impl GuardedAdvisor {
    /// `timeout` is `CAPITAL_AI_ADVISOR_TIMEOUT_SECONDS`.
    pub fn new(provider: Arc<dyn ProposalProvider>, timeout: Duration) -> Self {
        Self { provider, timeout }
    }

    pub async fn build_plan(&self, context: &AllocationContext) -> AdvisedPlan {
        let proposal = self.provider.proposal(context, self.timeout).await;
        let plan = guard_proposal(proposal.as_ref(), &context.fallback_plan, &context.config);
        let applied = plan != context.fallback_plan;
        AdvisedPlan {
            plan,
            proposal,
            applied,
        }
    }
}

/// Python `str(value or default)` for proposal text fields.
fn text_or(value: Option<&Value>, default: &str) -> String {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => default.to_owned(),
        Some(Value::String(s)) if s.is_empty() => default.to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => default.to_owned(),
        Some(other) => other.to_string(),
    }
}

fn non_negative(value: Option<&Value>) -> Option<Decimal> {
    if matches!(value, Some(Value::Bool(_))) {
        return None;
    }
    value_decimal(value).filter(|v| *v >= Decimal::ZERO)
}

/// Clamp a proposal into the deterministic bounds. Unknown or duplicate modules are dropped;
/// invalid amounts/statuses take the fallback's; budgets are capped by the global ceiling, the
/// allocatable pool and the pool remaining after earlier entries; below-minimum budgets pause;
/// budgets under the fallback are marked `reduced`; modules the proposal omits get no budget.
/// No surviving entry → the fallback plan.
pub fn guard_proposal(
    proposal: Option<&Value>,
    fallback: &AllocationPlan,
    config: &CapitalPoolConfig,
) -> AllocationPlan {
    let Some(items) = proposal
        .and_then(|p| p.get("allocations"))
        .and_then(Value::as_array)
    else {
        return fallback.clone();
    };
    let mut remaining = fallback.allocatable_usdt;
    let mut seen = BTreeSet::new();
    let min_usdt = config.min_module_allocation_usdt.max(Decimal::ZERO);
    let mut guarded = Vec::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let module_id = match object.get("module_id") {
            None | Some(Value::Null) => continue,
            Some(Value::String(s)) => s.trim().to_owned(),
            Some(other) => other.to_string(),
        };
        let Some(base) = fallback.allocation_for(&module_id) else {
            continue;
        };
        if !seen.insert(module_id.clone()) {
            continue;
        }
        let mut max_usdt = non_negative(object.get("max_usdt")).unwrap_or(base.max_usdt);
        if let Some(cap) = config.max_module_allocation_usdt {
            max_usdt = max_usdt.min(cap.max(Decimal::ZERO));
        }
        max_usdt = budget(max_usdt.min(fallback.allocatable_usdt).min(remaining));
        remaining = (remaining - max_usdt).max(Decimal::ZERO);

        let mut status = object
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .and_then(AllocationStatus::parse)
            .unwrap_or(base.status);
        let mut reason = text_or(object.get("reason"), "guarded_ai_proposal");
        if max_usdt <= Decimal::ZERO || (min_usdt > Decimal::ZERO && max_usdt < min_usdt) {
            status = AllocationStatus::Paused;
            reason = "guarded_ai_below_minimum".into();
            max_usdt = Decimal::ZERO;
        } else if max_usdt < base.max_usdt {
            status = AllocationStatus::Reduced;
        }
        let final_share_pct = if fallback.allocatable_usdt > Decimal::ZERO {
            max_usdt / fallback.allocatable_usdt * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        guarded.push(ModuleAllocation {
            module_id,
            status,
            max_usdt,
            score: base.score,
            reason,
            raw_share_pct: base.raw_share_pct,
            final_share_pct,
        });
    }
    if guarded.is_empty() {
        return fallback.clone();
    }
    let mut notes = fallback.notes.clone();
    notes.push("guarded AI proposal applied".into());
    AllocationPlan {
        total_equity_usdt: fallback.total_equity_usdt,
        reserve_usdt: fallback.reserve_usdt,
        allocatable_usdt: fallback.allocatable_usdt,
        allocations: guarded,
        notes,
    }
}

/// JSON schema of a proposal (the refactor `ALLOCATION_SCHEMA`).
pub fn allocation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["allocations"],
        "properties": {
            "allocations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["module_id", "status", "max_usdt", "reason"],
                    "properties": {
                        "module_id": {"type": "string"},
                        "status": {"type": "string", "enum": ["active", "reduced", "paused"]},
                        "max_usdt": {"type": "number", "minimum": 0},
                        "reason": {"type": "string"}
                    }
                }
            }
        }
    })
}

const ITEM_FIELDS: [&str; 4] = ["module_id", "status", "max_usdt", "reason"];

/// Validate against [`allocation_schema`] and require every `module_id` to be one of `module_ids`
/// (each at most once). Errors are phrased for the model to correct itself.
pub fn validate_proposal(value: &Value, module_ids: &[String]) -> Result<(), String> {
    let object = value.as_object().ok_or("proposal must be a JSON object")?;
    if let Some(extra) = object.keys().find(|k| k.as_str() != "allocations") {
        return Err(format!(
            "unexpected property {extra:?}; only \"allocations\" is allowed"
        ));
    }
    let items = object
        .get("allocations")
        .ok_or("missing required property \"allocations\"")?
        .as_array()
        .ok_or("\"allocations\" must be an array")?;
    let mut seen = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let entry = item
            .as_object()
            .ok_or_else(|| format!("allocations[{index}] must be an object"))?;
        if let Some(extra) = entry.keys().find(|k| !ITEM_FIELDS.contains(&k.as_str())) {
            return Err(format!(
                "allocations[{index}]: unexpected property {extra:?}"
            ));
        }
        if let Some(missing) = ITEM_FIELDS.iter().find(|f| !entry.contains_key(**f)) {
            return Err(format!(
                "allocations[{index}]: missing required property {missing:?}"
            ));
        }
        let module_id = entry["module_id"]
            .as_str()
            .ok_or_else(|| format!("allocations[{index}].module_id must be a string"))?;
        if !module_ids.iter().any(|id| id == module_id) {
            return Err(format!(
                "allocations[{index}].module_id {module_id:?} is not one of the listed module_ids: {}",
                module_ids.join(", ")
            ));
        }
        if !seen.insert(module_id) {
            return Err(format!(
                "allocations[{index}]: module {module_id:?} appears twice"
            ));
        }
        match entry["status"].as_str() {
            Some("active" | "reduced" | "paused") => {}
            _ => {
                return Err(format!(
                    "allocations[{index}].status must be one of active, reduced, paused"
                ));
            }
        }
        let Value::Number(max) = &entry["max_usdt"] else {
            return Err(format!("allocations[{index}].max_usdt must be a number"));
        };
        if max.as_f64().is_none_or(|v| v < 0.0) {
            return Err(format!("allocations[{index}].max_usdt must be >= 0"));
        }
        if !entry["reason"].is_string() {
            return Err(format!("allocations[{index}].reason must be a string"));
        }
    }
    Ok(())
}

fn strip_code_fence(text: &str) -> &str {
    if text.len() >= 6 && text.starts_with("```") && text.ends_with("```") {
        let inner = &text[3..text.len() - 3];
        return inner.strip_prefix("json").unwrap_or(inner).trim();
    }
    text
}

fn parse_json_object(text: &str) -> Option<Value> {
    let stripped = strip_code_fence(text.trim());
    let parsed = serde_json::from_str::<Value>(stripped).ok().or_else(|| {
        let start = stripped.find('{')?;
        let end = stripped.rfind('}')?;
        (end > start)
            .then(|| serde_json::from_str::<Value>(&stripped[start..=end]).ok())
            .flatten()
    })?;
    parsed.is_object().then_some(parsed)
}

fn is_proposal(value: &Value) -> bool {
    value.get("allocations").is_some_and(Value::is_array)
}

fn nested_text_candidates(payload: &Value, out: &mut Vec<String>) {
    for key in ["result", "content", "text", "message"] {
        match payload.get(key) {
            Some(Value::String(s)) => out.push(s.clone()),
            Some(nested @ Value::Object(_)) => nested_text_candidates(nested, out),
            Some(Value::Array(items)) => {
                for item in items {
                    match item {
                        Value::String(s) => out.push(s.clone()),
                        Value::Object(_) => {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                out.push(text.to_owned());
                            }
                            nested_text_candidates(item, out);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// Recover a proposal from free text: raw JSON (code fences stripped, else the outermost
/// `{...}`), a `structured_output` object, or JSON nested in `result|content|text|message`.
pub fn extract_allocation_proposal(raw: &str) -> Option<Value> {
    let payload = parse_json_object(raw)?;
    if is_proposal(&payload) {
        return Some(payload);
    }
    if let Some(structured) = payload.get("structured_output").filter(|s| is_proposal(s)) {
        return Some(structured.clone());
    }
    let mut candidates = Vec::new();
    nested_text_candidates(&payload, &mut candidates);
    candidates
        .iter()
        .filter_map(|text| parse_json_object(text))
        .find(is_proposal)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::allocator::{AllocationUnit, build_plan};

    fn config(reserve: Decimal, cap: Option<Decimal>) -> CapitalPoolConfig {
        CapitalPoolConfig {
            enabled: true,
            reserve_pct: reserve,
            min_module_allocation_usdt: Decimal::ZERO,
            max_module_allocation_usdt: cap,
        }
    }

    fn fallback(config: &CapitalPoolConfig, ids: &[&str]) -> AllocationPlan {
        let units: Vec<AllocationUnit> = ids
            .iter()
            .map(|id| AllocationUnit::new(ModuleScorecard::new(*id)))
            .collect();
        build_plan(config, dec!(100), &units, Scoring::Scorecard)
    }

    #[test]
    fn guarded_ai_advisor_clamps_proposals_to_pool_limits() {
        let config = config(dec!(50), Some(dec!(25)));
        let base = fallback(&config, &["spot-sma-crossover:BTC/USDT"]);
        let proposal = json!({"allocations": [{"module_id": "spot-sma-crossover:BTC/USDT", "status": "active",
                                               "max_usdt": 1000.0, "reason": "ai_requested_more"}]});
        let plan = guard_proposal(Some(&proposal), &base, &config);
        assert_eq!(
            plan.allocation_for("spot-sma-crossover:BTC/USDT")
                .unwrap()
                .max_usdt,
            dec!(25)
        );
    }

    #[tokio::test]
    async fn file_proposal_feeds_guarded_advisor_and_pauses_zero_budgets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proposal.json");
        std::fs::write(
            &path,
            r#"{"allocations": [{"module_id": "spot-breakout-trader:BTC/USDT", "status": "paused", "max_usdt": 0, "reason": "ai_pause_drawdown"}]}"#,
        )
        .unwrap();
        let config = config(Decimal::ZERO, None);
        let base = fallback(&config, &["spot-breakout-trader:BTC/USDT"]);
        let context = AllocationContext {
            config: config.clone(),
            scoring: Scoring::Scorecard,
            total_equity_usdt: dec!(100),
            module_ids: vec!["spot-breakout-trader:BTC/USDT".into()],
            scorecards: BTreeMap::new(),
            fallback_plan: base,
        };
        let advisor = GuardedAdvisor::new(
            Arc::new(FileProposalProvider::new(&path)),
            Duration::from_secs(1),
        );
        let advised = advisor.build_plan(&context).await;
        let allocation = advised
            .plan
            .allocation_for("spot-breakout-trader:BTC/USDT")
            .unwrap();
        assert_eq!(allocation.status, AllocationStatus::Paused);
        assert_eq!(allocation.reason, "guarded_ai_below_minimum");
        assert!(advised.applied);

        // Missing file → fallback.
        let missing = GuardedAdvisor::new(
            Arc::new(FileProposalProvider::new(dir.path().join("none.json"))),
            Duration::from_secs(1),
        );
        let advised = missing.build_plan(&context).await;
        assert!(!advised.applied && advised.proposal.is_none());
    }

    #[test]
    fn guard_shares_the_pool_first_come_and_marks_reductions() {
        let config = config(Decimal::ZERO, None);
        let base = fallback(&config, &["a", "b", "c"]);
        let proposal = json!({"allocations": [
            {"module_id": "a", "status": "active", "max_usdt": 70, "reason": "ai_scale_winner"},
            {"module_id": "b", "status": "bogus", "max_usdt": "50", "reason": ""},
            {"module_id": "a", "status": "active", "max_usdt": 1, "reason": "dup"},
            {"module_id": "zzz", "status": "active", "max_usdt": 10, "reason": "unknown"},
            {"module_id": "c", "status": "active", "max_usdt": -3, "reason": "neg"},
            "garbage"
        ]});
        let plan = guard_proposal(Some(&proposal), &base, &config);
        assert_eq!(plan.allocations.len(), 3);
        let a = plan.allocation_for("a").unwrap();
        assert_eq!((a.max_usdt, a.status), (dec!(70), AllocationStatus::Active));
        let b = plan.allocation_for("b").unwrap();
        assert_eq!(b.max_usdt, dec!(30));
        assert_eq!(b.status, AllocationStatus::Reduced);
        assert_eq!(b.reason, "guarded_ai_proposal");
        // Negative amount takes the fallback (33.33) but the pool is exhausted → paused.
        let c = plan.allocation_for("c").unwrap();
        assert_eq!(
            (c.status, c.reason.as_str()),
            (AllocationStatus::Paused, "guarded_ai_below_minimum")
        );
        let total: Decimal = plan.allocations.iter().map(|a| a.max_usdt).sum();
        assert!(total <= plan.allocatable_usdt);
    }

    #[test]
    fn unusable_proposals_fall_back() {
        let config = config(Decimal::ZERO, None);
        let base = fallback(&config, &["a"]);
        for proposal in [
            json!({}),
            json!({"allocations": {}}),
            json!({"allocations": [{"module_id": "x"}]}),
        ] {
            assert_eq!(guard_proposal(Some(&proposal), &base, &config), base);
        }
        assert_eq!(guard_proposal(None, &base, &config), base);
    }

    #[test]
    fn schema_validation_rejects_malformed_proposals() {
        let ids = vec!["m:BTC/USDT".to_owned()];
        let good = json!({"allocations": [{"module_id": "m:BTC/USDT", "status": "reduced", "max_usdt": 10, "reason": "ai_reduce_drawdown"}]});
        assert!(validate_proposal(&good, &ids).is_ok());
        for bad in [
            json!([]),
            json!({"allocations": [], "extra": 1}),
            json!({"allocations": [{"module_id": "m:BTC/USDT", "status": "maybe", "max_usdt": 1, "reason": "x"}]}),
            json!({"allocations": [{"module_id": "m:BTC/USDT", "status": "active", "max_usdt": -1, "reason": "x"}]}),
            json!({"allocations": [{"module_id": "m:BTC/USDT", "status": "active", "max_usdt": "1", "reason": "x"}]}),
            json!({"allocations": [{"module_id": "other", "status": "active", "max_usdt": 1, "reason": "x"}]}),
            json!({"allocations": [{"module_id": "m:BTC/USDT", "status": "active", "max_usdt": 1}]}),
            json!({"allocations": [{"module_id": "m:BTC/USDT", "status": "active", "max_usdt": 1, "reason": "x", "why": 1}]}),
        ] {
            assert!(validate_proposal(&bad, &ids).is_err(), "{bad}");
        }
    }

    #[test]
    fn extract_allocation_proposal_from_raw_json() {
        let raw =
            json!({"allocations": [{"module_id": "spot-sma-crossover:BTC/USDT", "status": "active",
                                          "max_usdt": 12.5, "reason": "ai_keep_fallback"}]})
            .to_string();
        let proposal = extract_allocation_proposal(&raw).unwrap();
        assert_eq!(proposal["allocations"][0]["max_usdt"].as_f64(), Some(12.5));
    }

    #[test]
    fn extract_allocation_proposal_from_bot_json_wrapper() {
        let raw = json!({"type": "result", "result": "```json\n{\n  \"allocations\": [\n    {\"module_id\": \"spot-breakout-trader:SOL/USDT\", \"status\": \"paused\", \"max_usdt\": 0, \"reason\": \"ai_no_evidence\"}\n  ]\n}\n```"})
            .to_string();
        let proposal = extract_allocation_proposal(&raw).unwrap();
        assert_eq!(proposal["allocations"][0]["status"], "paused");
    }

    #[test]
    fn extract_allocation_proposal_from_structured_output_and_prose() {
        let raw = json!({"type": "result", "result": "", "structured_output": {"allocations": [
            {"module_id": "spot-sma-crossover:BTC/USDT", "status": "reduced", "max_usdt": 25, "reason": "ai_keep_fallback"}]}})
        .to_string();
        assert_eq!(
            extract_allocation_proposal(&raw).unwrap()["allocations"][0]["reason"],
            "ai_keep_fallback"
        );
        let prose = "Here you go: {\"allocations\": []} -- done";
        assert!(extract_allocation_proposal(prose).is_some());
        assert!(extract_allocation_proposal("no json at all").is_none());
    }
}

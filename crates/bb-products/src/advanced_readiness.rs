//! Local evidence review for advanced-product canary plans
//! (port of `src/advanced_readiness.py`). Never contacts Binance.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::product_modules::{ProductModuleError, get_product_module};

pub const ADVANCED_CANARY_PLANS: [&str; 3] = [
    "margin_canary_plan",
    "futures_canary_plan",
    "transfer_canary_plan",
];

pub const EVIDENCE_FIELDS: [&str; 8] = [
    "focused_tests_passed",
    "full_tests_passed",
    "replay_passed",
    "dry_run_report_reviewed",
    "permission_diagnostics_reviewed",
    "canary_plan_reviewed",
    "audit_reviewed",
    "explicit_execution_approved",
];

#[derive(Debug, thiserror::Error)]
pub enum AdvancedReadinessError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Module(#[from] ProductModuleError),
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Operator-reviewed evidence flags for one canary, keyed by [`EVIDENCE_FIELDS`].
pub type ProductReadinessEvidence = BTreeMap<String, bool>;

/// Readiness outcome for one advanced-product plan module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductReadinessCheck {
    pub module_id: String,
    pub evidence: ProductReadinessEvidence,
    pub missing_evidence: Vec<String>,
    pub plan_review_complete: bool,
    pub execution_approval_recorded: bool,
    pub live_mutation_enabled: bool,
    pub live_canary_allowed: bool,
    pub detail: String,
}

/// Cross-product local readiness and serial-execution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvancedReadinessReport {
    pub products: Vec<ProductReadinessCheck>,
    pub requested_live_canaries: Vec<String>,
    pub serial_execution_allowed: bool,
    pub automation_enabled: bool,
    pub live_mutation_enabled: bool,
}

impl AdvancedReadinessReport {
    pub fn all_plan_reviews_complete(&self) -> bool {
        self.products.iter().all(|p| p.plan_review_complete)
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "products": self.products,
            "requested_live_canaries": self.requested_live_canaries,
            "serial_execution_allowed": self.serial_execution_allowed,
            "all_plan_reviews_complete": self.all_plan_reviews_complete(),
            "automation_enabled": self.automation_enabled,
            "live_mutation_enabled": self.live_mutation_enabled,
        })
    }
}

/// Build a local readiness report from a `{module_id: {field: bool}}` evidence object.
pub fn build_advanced_readiness_report(
    evidence: &serde_json::Map<String, Value>,
    requested_live_canaries: &[String],
) -> Result<AdvancedReadinessReport, AdvancedReadinessError> {
    let requested = requested_live_canaries
        .iter()
        .map(|id| module_id(id))
        .collect::<Result<Vec<_>, _>>()?;
    let mut distinct = requested.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let products = ADVANCED_CANARY_PLANS
        .iter()
        .map(|id| evaluate_product(id, evidence.get(*id)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AdvancedReadinessReport {
        products,
        requested_live_canaries: requested,
        serial_execution_allowed: distinct.len() <= 1,
        automation_enabled: false,
        live_mutation_enabled: false,
    })
}

/// Reject concurrent state-mutating canary approvals.
pub fn ensure_serial_live_canary_execution(
    report: &AdvancedReadinessReport,
) -> Result<(), AdvancedReadinessError> {
    if report.serial_execution_allowed {
        Ok(())
    } else {
        Err(AdvancedReadinessError::Invalid(
            "advanced live canaries must execute serially; only one mutable product may be \
             requested at a time"
                .into(),
        ))
    }
}

pub fn render_advanced_readiness_report(report: &AdvancedReadinessReport) -> String {
    let mut lines = vec![
        "Advanced product readiness:".to_owned(),
        format!(
            "Serial live canary rule: {}",
            if report.serial_execution_allowed {
                "pass"
            } else {
                "blocked"
            }
        ),
        "Live mutation enabled: no".to_owned(),
        "Background automation enabled: no".to_owned(),
    ];
    for check in &report.products {
        let missing = if check.missing_evidence.is_empty() {
            "none".to_owned()
        } else {
            check.missing_evidence.join(", ")
        };
        lines.extend([
            format!("  - {}:", check.module_id),
            format!(
                "    plan review complete: {}",
                if check.plan_review_complete {
                    "yes"
                } else {
                    "no"
                }
            ),
            format!("    missing evidence: {missing}"),
            "    live canary allowed: no".to_owned(),
        ]);
    }
    lines.join("\n")
}

/// Load the `--evidence-file` JSON object (empty when no path is given).
pub fn load_evidence(
    path: Option<&Path>,
) -> Result<serde_json::Map<String, Value>, AdvancedReadinessError> {
    let Some(path) = path else {
        return Ok(serde_json::Map::new());
    };
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| AdvancedReadinessError::Io {
        path: display.clone(),
        source,
    })?;
    let raw: Value =
        serde_json::from_str(&text).map_err(|source| AdvancedReadinessError::Json {
            path: display,
            source,
        })?;
    let Value::Object(map) = raw else {
        return Err(AdvancedReadinessError::Invalid(
            "evidence file must contain an object".into(),
        ));
    };
    let mut unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|key| !ADVANCED_CANARY_PLANS.contains(key))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(AdvancedReadinessError::Invalid(format!(
            "unknown advanced canary modules: {}",
            unknown.join(", ")
        )));
    }
    Ok(map)
}

fn evaluate_product(
    id: &str,
    evidence: Option<&Value>,
) -> Result<ProductReadinessCheck, AdvancedReadinessError> {
    let module = get_product_module(id)?;
    let values = coerce_evidence(evidence)?;
    let flag = |field: &str| values.get(field).copied().unwrap_or(false);
    let missing: Vec<String> = EVIDENCE_FIELDS
        .iter()
        .filter(|f| !flag(f))
        .map(|f| (*f).to_owned())
        .collect();
    let plan_complete = missing.iter().all(|f| f == "explicit_execution_approved");
    let detail = if plan_complete {
        "review evidence is complete, but this planning module exposes no live mutation endpoint"
    } else {
        "canary planning review evidence is incomplete"
    };
    Ok(ProductReadinessCheck {
        module_id: module.module_id.to_owned(),
        execution_approval_recorded: flag("explicit_execution_approved"),
        evidence: values,
        missing_evidence: missing,
        plan_review_complete: plan_complete,
        live_mutation_enabled: module.live_activation_enabled,
        live_canary_allowed: false,
        detail: detail.to_owned(),
    })
}

fn coerce_evidence(
    evidence: Option<&Value>,
) -> Result<ProductReadinessEvidence, AdvancedReadinessError> {
    let empty = serde_json::Map::new();
    let map = match evidence {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(AdvancedReadinessError::Invalid(
                "product evidence must be an object".into(),
            ));
        }
    };
    let mut unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !EVIDENCE_FIELDS.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(AdvancedReadinessError::Invalid(format!(
            "unknown product evidence fields: {}",
            unknown.join(", ")
        )));
    }
    Ok(EVIDENCE_FIELDS
        .iter()
        .map(|field| ((*field).to_owned(), map.get(*field).is_some_and(truthy)))
        .collect())
}

/// Python `bool(value)` for JSON values.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn module_id(value: &str) -> Result<String, AdvancedReadinessError> {
    let normalized = value.trim().to_lowercase().replace('-', "_");
    if ADVANCED_CANARY_PLANS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(AdvancedReadinessError::Invalid(format!(
            "unknown advanced canary module: {value}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete() -> Value {
        Value::Object(
            EVIDENCE_FIELDS
                .iter()
                .map(|f| ((*f).to_owned(), Value::Bool(true)))
                .collect(),
        )
    }

    #[test]
    fn complete_evidence_does_not_enable_live_mutation() {
        let mut evidence = serde_json::Map::new();
        for id in ADVANCED_CANARY_PLANS {
            evidence.insert(id.to_owned(), complete());
        }
        let report = build_advanced_readiness_report(&evidence, &[]).unwrap();
        assert!(report.all_plan_reviews_complete());
        assert!(!report.automation_enabled);
        assert!(!report.live_mutation_enabled);
        for product in &report.products {
            assert!(product.execution_approval_recorded);
            assert!(!product.live_canary_allowed);
            assert!(!product.live_mutation_enabled);
            assert!(product.missing_evidence.is_empty());
        }
        let json = report.to_json();
        assert_eq!(json["all_plan_reviews_complete"], true);
        assert_eq!(json["products"][0]["evidence"]["audit_reviewed"], true);
    }

    #[test]
    fn missing_evidence_reported_per_product() {
        let mut evidence = serde_json::Map::new();
        let mut partial = complete();
        partial["explicit_execution_approved"] = Value::Bool(false);
        evidence.insert("margin_canary_plan".into(), partial);
        evidence.insert(
            "futures_canary_plan".into(),
            serde_json::json!({"replay_passed": 1}),
        );
        let report = build_advanced_readiness_report(&evidence, &[]).unwrap();
        assert!(report.products[0].plan_review_complete);
        assert_eq!(
            report.products[0].missing_evidence,
            vec!["explicit_execution_approved"]
        );
        assert!(!report.products[1].plan_review_complete);
        assert_eq!(report.products[1].missing_evidence.len(), 7);
        assert_eq!(report.products[2].missing_evidence.len(), 8);
        assert!(!report.all_plan_reviews_complete());
        let text = render_advanced_readiness_report(&report);
        assert!(text.contains("    missing evidence: explicit_execution_approved"));
    }

    #[test]
    fn mutable_canary_requests_must_be_serial() {
        let empty = serde_json::Map::new();
        let one = build_advanced_readiness_report(
            &empty,
            &["margin-canary-plan".into(), "margin_canary_plan".into()],
        )
        .unwrap();
        assert!(ensure_serial_live_canary_execution(&one).is_ok());
        let two = build_advanced_readiness_report(
            &empty,
            &["margin_canary_plan".into(), "futures_canary_plan".into()],
        )
        .unwrap();
        assert!(!two.serial_execution_allowed);
        assert!(
            ensure_serial_live_canary_execution(&two)
                .unwrap_err()
                .to_string()
                .contains("serially")
        );
        assert!(build_advanced_readiness_report(&empty, &["nope".into()]).is_err());
    }

    #[test]
    fn evidence_file_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.json");
        std::fs::write(&path, r#"{"margin_canary_plan": {"bogus": true}}"#).unwrap();
        let evidence = load_evidence(Some(&path)).unwrap();
        let err = build_advanced_readiness_report(&evidence, &[]).unwrap_err();
        assert_eq!(err.to_string(), "unknown product evidence fields: bogus");
        std::fs::write(&path, r#"{"spot": {}}"#).unwrap();
        assert!(
            load_evidence(Some(&path))
                .unwrap_err()
                .to_string()
                .contains("unknown advanced canary modules: spot")
        );
        std::fs::write(&path, "[]").unwrap();
        assert!(load_evidence(Some(&path)).is_err());
    }
}

//! Local withdrawal safety planning (port of `src/withdrawals.py`).
//! No exchange call is made and no withdrawal endpoint exists here.

use std::path::Path;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;

use crate::common::{self, append_audit, fmt_dec, num, yes_no};
use crate::settings::Settings;

pub const WRITE_ENDPOINTS_BLOCKED: [&str; 1] = ["sapiPostCapitalWithdrawApply"];
pub const DEFAULT_AUDIT_PATH: &str = "data/withdrawal-plans.jsonl";

#[derive(Debug, thiserror::Error)]
pub enum WithdrawalSafetyError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

fn invalid(msg: impl Into<String>) -> WithdrawalSafetyError {
    WithdrawalSafetyError::Invalid(msg.into())
}

/// One locally approved withdrawal destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WithdrawalAddress {
    pub asset: String,
    pub address: String,
    pub networks: Vec<String>,
    pub label: String,
    pub memo: Option<String>,
    pub active: bool,
}

/// Requested external withdrawal modelling inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalRequest {
    pub asset: String,
    pub amount: Decimal,
    pub address: String,
    pub network: Option<String>,
    pub memo: Option<String>,
    pub operator_id: String,
    pub reason: String,
}

/// Local controls that must pass before a withdrawal could be considered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WithdrawalSafetyPolicy {
    pub allowlist: Vec<WithdrawalAddress>,
    #[serde(with = "num")]
    pub max_per_request_amount: Decimal,
    #[serde(with = "num")]
    pub max_daily_amount: Decimal,
    #[serde(with = "num")]
    pub daily_used_amount: Decimal,
    pub approvals_required: i64,
    pub approvals: Vec<String>,
    pub cooldown_seconds: i64,
    pub last_withdrawal_at: Option<String>,
    pub emergency_stop: bool,
    pub allowed_operators: Vec<String>,
    pub require_operator_identity: bool,
}

impl Default for WithdrawalSafetyPolicy {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            max_per_request_amount: dec!(10.0),
            max_daily_amount: dec!(25.0),
            daily_used_amount: dec!(0.0),
            approvals_required: 2,
            approvals: Vec::new(),
            cooldown_seconds: 3600,
            last_withdrawal_at: None,
            emergency_stop: false,
            allowed_operators: Vec::new(),
            require_operator_identity: true,
        }
    }
}

/// Dry-run withdrawal safety decision and audit payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WithdrawalPlan {
    pub timestamp: String,
    pub asset: String,
    #[serde(with = "num")]
    pub amount: Decimal,
    pub address: String,
    pub network: Option<String>,
    pub memo: Option<String>,
    pub allowlist_label: String,
    pub operator_id: String,
    pub reason: String,
    #[serde(with = "num")]
    pub per_request_cap: Decimal,
    #[serde(with = "num")]
    pub daily_cap: Decimal,
    #[serde(with = "num")]
    pub daily_used: Decimal,
    #[serde(with = "num")]
    pub daily_remaining: Decimal,
    pub approvals_required: i64,
    pub approvals: Vec<String>,
    pub cooldown_seconds_remaining: i64,
    pub emergency_stop: bool,
    pub submission_attempted: bool,
    pub submitted: bool,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub live_withdrawals_enabled: bool,
    pub write_endpoints_used: Vec<String>,
    pub policy: WithdrawalSafetyPolicy,
}

/// Full dry-run withdrawal report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WithdrawalDryRunReport {
    pub timestamp: String,
    pub mode: String,
    pub plan: WithdrawalPlan,
    pub audit_path: String,
    pub live_withdrawals_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

/// Build a dry-run withdrawal safety plan without submitting anything.
pub fn build_withdrawal_plan(
    request: &WithdrawalRequest,
    policy: &WithdrawalSafetyPolicy,
    now: DateTime<Utc>,
) -> Result<WithdrawalPlan, WithdrawalSafetyError> {
    validate_policy(policy)?;
    let asset = asset(&request.asset)?;
    if request.amount <= Decimal::ZERO {
        return Err(invalid("amount must be greater than zero"));
    }
    let address = non_blank(&request.address, "address")?;
    let operator_id = request.operator_id.trim().to_owned();
    let requested_network = optional_network(request.network.as_deref());
    let memo = optional_text(request.memo.as_deref());
    let mut approvals = policy
        .approvals
        .iter()
        .map(|a| non_blank(a, "approval"))
        .collect::<Result<Vec<_>, _>>()?;
    approvals.sort();
    approvals.dedup();
    let entry = policy.allowlist.iter().find(|e| {
        e.asset.trim().to_uppercase() == asset
            && e.address.trim() == address
            && optional_text(e.memo.as_deref()) == memo
    });
    let network = requested_network
        .clone()
        .or_else(|| entry.and_then(|e| e.networks.first().cloned()));
    let daily_remaining = (policy.max_daily_amount - policy.daily_used_amount).max(Decimal::ZERO);
    let cooldown = cooldown_remaining(
        policy.last_withdrawal_at.as_deref(),
        policy.cooldown_seconds,
        now,
    )?;

    let mut rejections: Vec<&str> = Vec::new();
    let mut warnings: Vec<&str> = Vec::new();
    if policy.emergency_stop {
        rejections.push("emergency stop is active");
    }
    match entry {
        None => rejections.push("address is not allowlisted for this asset and memo"),
        Some(e) if !e.active => rejections.push("allowlisted address is inactive"),
        Some(_) => {}
    }
    match &requested_network {
        None if network.is_none() => {
            rejections.push("network is required when allowlist has no default network");
        }
        None => warnings.push("network was selected from the allowlist default"),
        Some(n) if entry.is_some_and(|e| !e.networks.contains(n)) => {
            rejections.push("network is not allowlisted for this address");
        }
        Some(_) => {}
    }
    if request.amount > policy.max_per_request_amount {
        rejections.push("amount exceeds max_per_request_amount");
    }
    if request.amount > daily_remaining {
        rejections.push("amount exceeds remaining daily withdrawal cap");
    }
    if (approvals.len() as i64) < policy.approvals_required {
        rejections.push("approval state does not satisfy approvals_required");
    }
    if cooldown > 0 {
        rejections.push("withdrawal cooldown is still active");
    }
    if policy.require_operator_identity && operator_id.is_empty() {
        rejections.push("operator identity is required");
    }
    if !operator_id.is_empty()
        && !policy.allowed_operators.is_empty()
        && !policy.allowed_operators.contains(&operator_id)
    {
        rejections.push("operator is not allowed by withdrawal policy");
    }

    Ok(WithdrawalPlan {
        timestamp: common::utc_timestamp(),
        asset,
        amount: request.amount,
        address,
        network,
        memo,
        allowlist_label: entry.map(|e| e.label.clone()).unwrap_or_default(),
        operator_id,
        reason: request.reason.clone(),
        per_request_cap: policy.max_per_request_amount,
        daily_cap: policy.max_daily_amount,
        daily_used: policy.daily_used_amount,
        daily_remaining,
        approvals_required: policy.approvals_required,
        approvals,
        cooldown_seconds_remaining: cooldown,
        emergency_stop: policy.emergency_stop,
        submission_attempted: false,
        submitted: false,
        allowed: rejections.is_empty(),
        rejection_reasons: rejections.into_iter().map(str::to_owned).collect(),
        warnings: warnings.into_iter().map(str::to_owned).collect(),
        live_withdrawals_enabled: false,
        write_endpoints_used: Vec::new(),
        policy: policy.clone(),
    })
}

/// Build and optionally audit a dry-run withdrawal report.
pub fn build_withdrawal_dry_run_report(
    settings: &Settings,
    request: &WithdrawalRequest,
    policy: &WithdrawalSafetyPolicy,
    audit_path: Option<&Path>,
) -> Result<WithdrawalDryRunReport, WithdrawalSafetyError> {
    let plan = build_withdrawal_plan(request, policy, bb_core::time::now())?;
    let audit = match audit_path {
        Some(path) => append_audit(path, &plan)?.display().to_string(),
        None => String::new(),
    };
    Ok(WithdrawalDryRunReport {
        timestamp: common::utc_timestamp(),
        mode: common::mode(settings).to_owned(),
        plan,
        audit_path: audit,
        live_withdrawals_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

/// Fail closed for any attempted live withdrawal activation.
pub fn assert_withdrawal_live_disabled(
    confirm_live_withdrawal: bool,
) -> Result<(), WithdrawalSafetyError> {
    if confirm_live_withdrawal {
        return Err(invalid(
            "live withdrawal activation is not implemented; Phase 27 is dry-run only",
        ));
    }
    Ok(())
}

/// Concise human-readable dry-run withdrawal report.
pub fn render_withdrawal_report(report: &WithdrawalDryRunReport) -> String {
    let plan = &report.plan;
    let or = |value: &str, default: &str| {
        if value.is_empty() {
            default.to_owned()
        } else {
            value.to_owned()
        }
    };
    let mut lines = vec![
        format!("Mode: {}", report.mode),
        "Live withdrawals: disabled".to_owned(),
        "Write endpoints used: none".to_owned(),
        "Withdrawal plan:".to_owned(),
        format!("  - asset: {}", plan.asset),
        format!("  - amount: {}", fmt_dec(plan.amount)),
        format!("  - network: {}", plan.network.as_deref().unwrap_or("None")),
        format!("  - allowlist label: {}", or(&plan.allowlist_label, "none")),
        format!("  - operator: {}", or(&plan.operator_id, "missing")),
        format!("  - daily remaining: {}", fmt_dec(plan.daily_remaining)),
        format!(
            "  - cooldown remaining seconds: {}",
            plan.cooldown_seconds_remaining
        ),
        format!("  - allowed: {}", yes_no(plan.allowed)),
    ];
    if !plan.rejection_reasons.is_empty() {
        lines.push(format!(
            "  - rejection reasons: {}",
            plan.rejection_reasons.join(", ")
        ));
    }
    if !plan.warnings.is_empty() {
        lines.push(format!("  - warnings: {}", plan.warnings.join(", ")));
    }
    if !report.audit_path.is_empty() {
        lines.push(format!("Audit path: {}", report.audit_path));
    }
    lines.join("\n")
}

fn validate_policy(policy: &WithdrawalSafetyPolicy) -> Result<(), WithdrawalSafetyError> {
    if policy.max_per_request_amount <= Decimal::ZERO {
        return Err(invalid("max_per_request_amount must be greater than zero"));
    }
    if policy.max_daily_amount <= Decimal::ZERO {
        return Err(invalid("max_daily_amount must be greater than zero"));
    }
    if policy.daily_used_amount < Decimal::ZERO {
        return Err(invalid("daily_used_amount must be at least zero"));
    }
    if policy.approvals_required < 0 {
        return Err(invalid("approvals_required must be at least zero"));
    }
    if policy.cooldown_seconds < 0 {
        return Err(invalid("cooldown_seconds must be at least zero"));
    }
    for entry in &policy.allowlist {
        asset(&entry.asset)?;
        non_blank(&entry.address, "allowlist address")?;
        for n in &entry.networks {
            network(n)?;
        }
    }
    for operator in &policy.allowed_operators {
        non_blank(operator, "allowed operator")?;
    }
    if let Some(ts) = &policy.last_withdrawal_at {
        parse_timestamp(ts)?;
    }
    Ok(())
}

fn cooldown_remaining(
    last: Option<&str>,
    cooldown: i64,
    now: DateTime<Utc>,
) -> Result<i64, WithdrawalSafetyError> {
    let Some(last) = last else {
        return Ok(0);
    };
    if cooldown <= 0 {
        return Ok(0);
    }
    let elapsed_ms = (now - parse_timestamp(last)?).num_milliseconds();
    let remaining_ms = cooldown * 1000 - elapsed_ms;
    // ceil(remaining seconds), clamped at zero.
    Ok(if remaining_ms <= 0 {
        0
    } else {
        (remaining_ms + 999) / 1000
    })
}

/// `last_withdrawal_at` parser (Python `datetime.fromisoformat`; naive = UTC).
pub fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, WithdrawalSafetyError> {
    common::parse_iso_timestamp(value)
        .ok_or_else(|| invalid("last_withdrawal_at must be an ISO timestamp"))
}

fn asset(value: &str) -> Result<String, WithdrawalSafetyError> {
    let text = value.trim().to_uppercase();
    if text.is_empty() {
        return Err(invalid("asset must not be blank"));
    }
    Ok(text)
}

/// Upper-cased, non-blank network code.
pub fn network(value: &str) -> Result<String, WithdrawalSafetyError> {
    let text = value.trim().to_uppercase();
    if text.is_empty() {
        return Err(invalid("network must not be blank"));
    }
    Ok(text)
}

fn optional_network(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_uppercase)
}

pub fn optional_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn non_blank(value: &str, field: &str) -> Result<String, WithdrawalSafetyError> {
    let text = value.trim();
    if text.is_empty() {
        return Err(invalid(format!("{field} must not be blank")));
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn entry() -> WithdrawalAddress {
        WithdrawalAddress {
            asset: "usdt".into(),
            address: "TAllowlistedAddress".into(),
            networks: vec!["TRX".into()],
            label: "treasury".into(),
            memo: None,
            active: true,
        }
    }

    fn request() -> WithdrawalRequest {
        WithdrawalRequest {
            asset: "USDT".into(),
            amount: dec!(1),
            address: "TAllowlistedAddress".into(),
            network: Some("trx".into()),
            memo: None,
            operator_id: "local-review".into(),
            reason: "manual dry-run withdrawal plan".into(),
        }
    }

    fn policy() -> WithdrawalSafetyPolicy {
        WithdrawalSafetyPolicy {
            allowlist: vec![entry()],
            approvals: vec!["risk".into(), "ops".into(), "ops".into()],
            allowed_operators: vec!["local-review".into()],
            ..WithdrawalSafetyPolicy::default()
        }
    }

    #[test]
    fn allowlisted_withdrawal_plan_passes_without_submission() {
        let plan = build_withdrawal_plan(&request(), &policy(), Utc::now()).unwrap();
        assert!(plan.allowed, "{:?}", plan.rejection_reasons);
        assert_eq!(plan.network.as_deref(), Some("TRX"));
        assert_eq!(plan.approvals, vec!["ops", "risk"]);
        assert_eq!(plan.allowlist_label, "treasury");
        assert!(!plan.submission_attempted && !plan.submitted && !plan.live_withdrawals_enabled);
    }

    #[test]
    fn unsafe_requests_are_rejected() {
        let now = Utc::now();
        let mut policy = policy();
        policy.emergency_stop = true;
        policy.approvals = vec!["risk".into()];
        policy.daily_used_amount = dec!(24.5);
        policy.last_withdrawal_at = Some((now - Duration::seconds(600)).to_rfc3339());
        let mut request = request();
        request.amount = dec!(11);
        request.network = Some("ETH".into());
        request.operator_id = "intruder".into();
        let plan = build_withdrawal_plan(&request, &policy, now).unwrap();
        for reason in [
            "emergency stop is active",
            "network is not allowlisted for this address",
            "amount exceeds max_per_request_amount",
            "amount exceeds remaining daily withdrawal cap",
            "approval state does not satisfy approvals_required",
            "withdrawal cooldown is still active",
            "operator is not allowed by withdrawal policy",
        ] {
            assert!(
                plan.rejection_reasons.iter().any(|r| r == reason),
                "{reason}"
            );
        }
        assert_eq!(plan.cooldown_seconds_remaining, 3000);
        let mut request2 = self::request();
        request2.address = "unknown".into();
        request2.operator_id = String::new();
        let plan = build_withdrawal_plan(&request2, &self::policy(), now).unwrap();
        assert!(
            plan.rejection_reasons
                .contains(&"address is not allowlisted for this asset and memo".to_owned())
        );
        assert!(
            plan.rejection_reasons
                .contains(&"operator identity is required".to_owned())
        );
    }

    #[test]
    fn default_network_warning_audit_and_live_guard() {
        let dir = tempfile::tempdir().unwrap();
        let mut request = request();
        request.network = None;
        let settings = crate::testutil::settings_builder().build().unwrap();
        let audit = dir.path().join("withdrawal-plans.jsonl");
        let report =
            build_withdrawal_dry_run_report(&settings, &request, &policy(), Some(&audit)).unwrap();
        assert!(
            report
                .plan
                .warnings
                .contains(&"network was selected from the allowlist default".to_owned())
        );
        assert_eq!(report.plan.network.as_deref(), Some("TRX"));
        assert!(
            std::fs::read_to_string(&audit)
                .unwrap()
                .contains("\"submitted\":false")
        );
        assert!(render_withdrawal_report(&report).contains("  - operator: local-review"));
        assert!(assert_withdrawal_live_disabled(true).is_err());
        assert!(parse_timestamp("2026-01-01T00:00:00Z").is_ok());
        assert!(parse_timestamp("2026-01-01 00:00:00").is_ok());
        assert!(parse_timestamp("yesterday").is_err());
    }
}

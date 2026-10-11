//! Plan-only universal-transfer canary (port of `src/transfer_canary.py`).

use std::path::{Path, PathBuf};

use bb_binance::BinanceClient;
use serde::Serialize;

use crate::common::{self, append_audit, fmt_dec, yes_no};
use crate::scope_audit::{
    ScopeAuditError, ScopeInputs, build_scope_audit_report, ensure_scope_allowed,
};
use crate::settings::Settings;
use crate::transfers::{
    TransferDiscovery, TransferPlan, TransferPlanError, TransferRequest, TransferRiskLimits,
    build_transfer_dry_run_report,
};

pub const DEFAULT_AUDIT_PATH: &str = "data/transfer-canary.jsonl";
pub const DEFAULT_STOP_PATH: &str = "data/transfer-canary.stop";

#[derive(Debug, thiserror::Error)]
pub enum TransferCanaryError {
    #[error("{0}")]
    Blocked(String),
    /// The plan was audited but rejected by local risk checks.
    #[error("{message}")]
    Rejected {
        message: String,
        result: Box<TransferCanaryResult>,
    },
    #[error(transparent)]
    Scope(#[from] ScopeAuditError),
    #[error(transparent)]
    Plan(#[from] TransferPlanError),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

fn blocked(msg: impl Into<String>) -> TransferCanaryError {
    TransferCanaryError::Blocked(msg.into())
}

/// Local controls for one foreground transfer planning attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferCanaryLimits {
    pub risk_limits: TransferRiskLimits,
    pub audit_path: PathBuf,
    pub emergency_stop_path: PathBuf,
}

impl Default for TransferCanaryLimits {
    fn default() -> Self {
        Self {
            risk_limits: TransferRiskLimits::default(),
            audit_path: DEFAULT_AUDIT_PATH.into(),
            emergency_stop_path: DEFAULT_STOP_PATH.into(),
        }
    }
}

/// Auditable outcome of one plan-only transfer canary attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferCanaryResult {
    pub timestamp: String,
    pub mode: String,
    pub status: String,
    pub plan: TransferPlan,
    pub discovery: TransferDiscovery,
    pub submission_attempted: bool,
    pub submitted: bool,
    pub error: String,
    pub audit_path: String,
    pub live_transfers_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

/// Flags that must stay off.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransferCanaryFlags {
    pub submit_transfer: bool,
    pub confirm_live_transfer: bool,
}

/// Build and audit one read-only transfer plan; never submit a transfer.
pub async fn run_transfer_canary(
    settings: &Settings,
    client: &BinanceClient,
    request: &TransferRequest,
    flags: TransferCanaryFlags,
    limits: &TransferCanaryLimits,
) -> Result<TransferCanaryResult, TransferCanaryError> {
    if limits.audit_path.as_os_str().is_empty() {
        return Err(blocked("audit_path must not be blank"));
    }
    if limits.emergency_stop_path.as_os_str().is_empty() {
        return Err(blocked("emergency_stop_path must not be blank"));
    }
    preflight(flags, &limits.emergency_stop_path)?;
    let assets = vec![request.asset.clone()];
    let isolated: Vec<String> = [&request.from_symbol, &request.to_symbol]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect();
    ensure_scope_allowed(&build_scope_audit_report(
        settings,
        &ScopeInputs {
            transfer_assets: &assets,
            margin_symbols: &isolated,
            ..ScopeInputs::default()
        },
    )?)?;

    let report =
        build_transfer_dry_run_report(settings, client, request, &limits.risk_limits, None).await?;
    if report.live_transfers_enabled || !report.write_endpoints_used.is_empty() {
        return Err(blocked(
            "transfer canary rejected unexpected live-transfer activity",
        ));
    }
    let plan = report
        .plan
        .ok_or_else(|| blocked("transfer canary did not produce a plan"))?;
    let error = if plan.allowed {
        String::new()
    } else {
        format!(
            "transfer plan rejected: {}",
            plan.rejection_reasons.join(", ")
        )
    };
    let mut result = TransferCanaryResult {
        timestamp: common::utc_timestamp(),
        mode: "PLAN_ONLY".to_owned(),
        status: if plan.allowed { "planned" } else { "blocked" }.to_owned(),
        plan,
        discovery: report.discovery,
        submission_attempted: false,
        submitted: false,
        error,
        audit_path: String::new(),
        live_transfers_enabled: false,
        write_endpoints_used: Vec::new(),
    };
    result.audit_path = append_audit(&limits.audit_path, &result)?
        .display()
        .to_string();
    if !result.plan.allowed {
        return Err(TransferCanaryError::Rejected {
            message: result.error.clone(),
            result: Box::new(result),
        });
    }
    Ok(result)
}

/// Fail closed on submission/live flags and an active emergency stop.
pub fn preflight(
    flags: TransferCanaryFlags,
    emergency_stop_path: &Path,
) -> Result<(), TransferCanaryError> {
    if flags.submit_transfer {
        return Err(blocked(
            "universal-transfer submission is not implemented; Phase 26 is plan-only",
        ));
    }
    if flags.confirm_live_transfer {
        return Err(blocked(
            "live universal-transfer confirmation is disabled; Phase 26 is plan-only",
        ));
    }
    if emergency_stop_path.exists() {
        return Err(blocked("transfer canary emergency stop is active"));
    }
    Ok(())
}

/// Operator-facing plan-only transfer outcome.
pub fn render_transfer_canary_result(result: &TransferCanaryResult) -> String {
    let plan = &result.plan;
    [
        "Universal-transfer canary: plan only".to_owned(),
        "Live transfer submission: disabled".to_owned(),
        "Write endpoints used: none".to_owned(),
        format!("Status: {}", result.status),
        format!(
            "Route: {} -> {}",
            plan.source_wallet, plan.destination_wallet
        ),
        format!("Type: {}", plan.transfer_type.as_deref().unwrap_or("None")),
        format!("Asset: {}", plan.asset),
        format!("Amount: {}", fmt_dec(plan.amount)),
        format!("Allowed: {}", yes_no(plan.allowed)),
        format!("Audit path: {}", result.audit_path),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use wiremock::MockServer;

    use super::*;
    use crate::testutil::{mock_client, request_paths, settings_builder};
    use crate::transfers::tests::mount_wallets;

    fn settings(assets: &[&str]) -> Settings {
        let mut builder = settings_builder();
        builder.api_asset_whitelist = assets.iter().map(|s| (*s).to_owned()).collect();
        builder.api_symbol_whitelist = vec!["BTC/USDT".into()];
        builder.build().unwrap()
    }

    fn limits(dir: &Path) -> TransferCanaryLimits {
        TransferCanaryLimits {
            audit_path: dir.join("transfer-canary.jsonl"),
            emergency_stop_path: dir.join("transfer-canary.stop"),
            ..TransferCanaryLimits::default()
        }
    }

    #[tokio::test]
    async fn plans_and_audits_without_submission() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_wallets(&server, "50", "0", true).await;
        let result = run_transfer_canary(
            &settings(&["USDT"]),
            &mock_client(&server),
            &TransferRequest::new("spot", "funding", "USDT", dec!(1)),
            TransferCanaryFlags::default(),
            &limits(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(result.status, "planned");
        assert_eq!(result.mode, "PLAN_ONLY");
        assert!(!result.submitted && !result.submission_attempted);
        assert!(
            request_paths(&server)
                .await
                .iter()
                .all(|p| !p.ends_with("/sapi/v1/asset/transfer") || p.starts_with("GET"))
        );
        assert!(
            std::fs::read_to_string(dir.path().join("transfer-canary.jsonl"))
                .unwrap()
                .contains("\"status\":\"planned\"")
        );
        assert!(render_transfer_canary_result(&result).contains("Type: MAIN_FUNDING"));
    }

    #[tokio::test]
    async fn rejected_plan_is_audited_then_raised_and_guards_fail_early() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_wallets(&server, "0.5", "0", true).await;
        let client = mock_client(&server);
        let err = run_transfer_canary(
            &settings(&["USDT"]),
            &client,
            &TransferRequest::new("spot", "funding", "USDT", dec!(1)),
            TransferCanaryFlags::default(),
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        match err {
            TransferCanaryError::Rejected { message, result } => {
                assert!(message.starts_with(
                    "transfer plan rejected: amount exceeds source transferable balance"
                ));
                assert_eq!(result.status, "blocked");
            }
            other => panic!("unexpected {other}"),
        }
        assert!(dir.path().join("transfer-canary.jsonl").exists());

        let before = request_paths(&server).await.len();
        let err = run_transfer_canary(
            &settings(&[]),
            &client,
            &TransferRequest::new("spot", "funding", "USDT", dec!(1)),
            TransferCanaryFlags::default(),
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("API scope audit failed"));
        let err = run_transfer_canary(
            &settings(&["USDT"]),
            &client,
            &TransferRequest::new("spot", "funding", "USDT", dec!(1)),
            TransferCanaryFlags {
                submit_transfer: true,
                ..TransferCanaryFlags::default()
            },
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("plan-only"));
        std::fs::write(dir.path().join("transfer-canary.stop"), "").unwrap();
        let err = run_transfer_canary(
            &settings(&["USDT"]),
            &client,
            &TransferRequest::new("spot", "funding", "USDT", dec!(1)),
            TransferCanaryFlags::default(),
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("emergency stop is active"));
        assert_eq!(request_paths(&server).await.len(), before);
    }
}

//! Foreground plan-only margin canary (port of `src/margin_canary.py`).
//!
//! Builds and audits one read-only margin plan; never submits a borrow, repay,
//! order or transfer.

use std::path::{Path, PathBuf};

use bb_binance::BinanceClient;
use bb_core::telegram::escape;
use serde::Serialize;

use crate::alerts::Alerter;
use crate::common::{self, append_audit};
use crate::margin::{
    MarginDryRunInputs, MarginDryRunReport, MarginModelError, build_margin_dry_run_report,
    render_margin_report,
};
use crate::scope_audit::{
    ScopeAuditError, ScopeInputs, build_scope_audit_report, ensure_scope_allowed,
};
use crate::settings::Settings;

pub const DEFAULT_AUDIT_PATH: &str = "data/margin-canary.jsonl";
pub const DEFAULT_STOP_PATH: &str = "data/margin-canary.stop";

#[derive(Debug, thiserror::Error)]
pub enum MarginCanaryError {
    #[error("{0}")]
    Blocked(String),
    #[error(transparent)]
    Scope(#[from] ScopeAuditError),
    #[error(transparent)]
    Model(#[from] MarginModelError),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

/// Operational controls for one foreground plan-only run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginCanaryLimits {
    pub audit_path: PathBuf,
    pub emergency_stop_path: PathBuf,
}

impl Default for MarginCanaryLimits {
    fn default() -> Self {
        Self {
            audit_path: DEFAULT_AUDIT_PATH.into(),
            emergency_stop_path: DEFAULT_STOP_PATH.into(),
        }
    }
}

/// Auditable outcome of one read-only margin canary plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarginCanaryResult {
    pub timestamp: String,
    pub mode: String,
    pub status: String,
    pub report: MarginDryRunReport,
    pub submission_attempted: bool,
    pub error: String,
    pub audit_path: String,
    pub telegram_sent: bool,
    pub write_endpoints_used: Vec<String>,
}

/// Request flags that must stay off.
#[derive(Debug, Clone, Copy, Default)]
pub struct MarginCanaryFlags {
    pub submit_action: bool,
    pub confirm_live_margin: bool,
}

/// Build and audit one read-only margin plan; never submit an action.
pub async fn run_margin_canary(
    settings: &Settings,
    client: &BinanceClient,
    alerter: &dyn Alerter,
    flags: MarginCanaryFlags,
    inputs: &MarginDryRunInputs,
    margin_symbols: &[String],
    limits: &MarginCanaryLimits,
) -> Result<MarginCanaryResult, MarginCanaryError> {
    preflight(flags)?;
    validate_limits(limits)?;
    if limits.emergency_stop_path.exists() {
        return Err(MarginCanaryError::Blocked(format!(
            "margin canary emergency stop is active: {}",
            limits.emergency_stop_path.display()
        )));
    }
    let requested = requested_margin_symbols(margin_symbols, inputs);
    ensure_scope_allowed(&build_scope_audit_report(
        settings,
        &ScopeInputs {
            margin_symbols: &requested,
            ..ScopeInputs::default()
        },
    )?)?;

    let report = build_margin_dry_run_report(settings, client, inputs).await?;
    if report.live_trading_enabled || !report.write_endpoints_used.is_empty() {
        return Err(MarginCanaryError::Blocked(
            "margin canary received a report containing live writes".into(),
        ));
    }
    let rejections = risk_rejections(&report, inputs);
    let mut result = MarginCanaryResult {
        timestamp: common::utc_timestamp(),
        mode: report.mode.clone(),
        status: if rejections.is_empty() {
            "planned"
        } else {
            "risk_rejected"
        }
        .to_owned(),
        report,
        submission_attempted: false,
        error: rejections.join("; "),
        audit_path: String::new(),
        telegram_sent: false,
        write_endpoints_used: Vec::new(),
    };
    let audit_file = append_audit(&limits.audit_path, &result)?;
    if !rejections.is_empty() {
        result.telegram_sent = send_alert(alerter, &result, &audit_file).await;
    }
    result.audit_path = audit_file.display().to_string();
    Ok(result)
}

/// Concise plan-only canary result.
pub fn render_margin_canary_result(result: &MarginCanaryResult) -> String {
    let mut lines = vec![
        "Margin foreground canary: plan only".to_owned(),
        format!("Status: {}", result.status),
        "Submission attempted: no".to_owned(),
        "Margin write endpoints used: none".to_owned(),
        render_margin_report(&result.report),
    ];
    if !result.error.is_empty() {
        lines.push(format!("Risk rejection: {}", result.error));
    }
    if !result.audit_path.is_empty() {
        lines.push(format!("Audit path: {}", result.audit_path));
    }
    lines.join("\n")
}

pub fn preflight(flags: MarginCanaryFlags) -> Result<(), MarginCanaryError> {
    if flags.submit_action {
        return Err(MarginCanaryError::Blocked(
            "margin canary is plan-only; submit_action is disabled pending explicit live execution approval".into(),
        ));
    }
    if flags.confirm_live_margin {
        return Err(MarginCanaryError::Blocked(
            "margin canary is plan-only; live margin confirmation is disabled pending explicit live execution approval"
                .into(),
        ));
    }
    Ok(())
}

fn validate_limits(limits: &MarginCanaryLimits) -> Result<(), MarginCanaryError> {
    if limits
        .audit_path
        .as_os_str()
        .to_string_lossy()
        .trim()
        .is_empty()
    {
        return Err(MarginCanaryError::Blocked(
            "audit_path must not be blank".into(),
        ));
    }
    if limits
        .emergency_stop_path
        .as_os_str()
        .to_string_lossy()
        .trim()
        .is_empty()
    {
        return Err(MarginCanaryError::Blocked(
            "emergency_stop_path must not be blank".into(),
        ));
    }
    Ok(())
}

fn requested_margin_symbols(margin_symbols: &[String], inputs: &MarginDryRunInputs) -> Vec<String> {
    let mut requested: Vec<String> = Vec::new();
    let borrow = inputs
        .borrow_request
        .as_ref()
        .and_then(|r| r.isolated_symbol.clone());
    let repay = inputs
        .repay_request
        .as_ref()
        .and_then(|r| r.isolated_symbol.clone());
    for symbol in margin_symbols
        .iter()
        .chain(&inputs.isolated_symbols)
        .cloned()
        .chain(borrow)
        .chain(repay)
    {
        if !symbol.is_empty() && !requested.contains(&symbol) {
            requested.push(symbol);
        }
    }
    requested
}

fn risk_rejections(report: &MarginDryRunReport, inputs: &MarginDryRunInputs) -> Vec<String> {
    let mut rejections = Vec::new();
    if inputs.borrow_request.is_some() && inputs.repay_request.is_none() {
        rejections.push("borrow: a documented repayment intent is required".to_owned());
    }
    if let Some(plan) = report.borrow_plan.as_ref().filter(|p| !p.allowed) {
        rejections.extend(
            plan.rejection_reasons
                .iter()
                .map(|r| format!("borrow: {r}")),
        );
    }
    if let Some(plan) = report.repay_plan.as_ref().filter(|p| !p.allowed) {
        rejections.extend(plan.rejection_reasons.iter().map(|r| format!("repay: {r}")));
    }
    rejections
}

async fn send_alert(alerter: &dyn Alerter, result: &MarginCanaryResult, audit: &Path) -> bool {
    alerter
        .send(&format!(
            "<b>MARGIN CANARY PLAN</b>\nStatus: <code>{}</code>\nAction submission: <code>disabled</code>\nRisk: <code>{}</code>\nAudit: <code>{}</code>",
            escape(&result.status),
            escape(&result.error),
            escape(&audit.display().to_string())
        ))
        .await
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::alerts::testing::RecordingAlerter;
    use crate::margin::{AccountType, MarginBorrowRequest, MarginRepayRequest, MarginRiskLimits};
    use crate::testutil::{mock_client, mount_get, request_paths, settings_builder};

    fn settings(whitelist: &[&str]) -> Settings {
        let mut builder = settings_builder();
        builder.trading_symbols = vec!["BNB/USDT".into()];
        builder.api_symbol_whitelist = whitelist.iter().map(|s| (*s).to_owned()).collect();
        builder.build().unwrap()
    }

    async fn server(margin_level: &str, rate: &str) -> MockServer {
        let server = MockServer::start().await;
        mount_get(
            &server,
            "/sapi/v1/margin/account",
            json!({
                "created": true, "borrowEnabled": true, "marginLevel": margin_level,
                "TotalCollateralValueInUSDT": "100", "accountType": "MARGIN_1",
                "userAssets": [{"asset": "USDT", "free": "20", "borrowed": "5", "interest": "0.01"}]
            }),
        )
        .await;
        mount_get(
            &server,
            "/sapi/v1/margin/maxBorrowable",
            json!({"amount": "50", "borrowLimit": "100"}),
        )
        .await;
        mount_get(
            &server,
            "/sapi/v1/margin/next-hourly-interest-rate",
            json!([{"asset": "USDT", "nextHourlyInterestRate": rate}]),
        )
        .await;
        server
    }

    fn inputs(borrow: bool, repay: bool) -> MarginDryRunInputs {
        MarginDryRunInputs {
            borrow_request: borrow.then(|| MarginBorrowRequest {
                account_type: AccountType::Cross,
                asset: "USDT".into(),
                requested_amount: dec!(1),
                isolated_symbol: None,
                asset_usdt_price: None,
            }),
            repay_request: repay.then(|| MarginRepayRequest {
                account_type: AccountType::Cross,
                asset: "USDT".into(),
                requested_amount: dec!(1),
                isolated_symbol: None,
            }),
            limits: MarginRiskLimits::default(),
            ..MarginDryRunInputs::default()
        }
    }

    fn limits(dir: &Path) -> MarginCanaryLimits {
        MarginCanaryLimits {
            audit_path: dir.join("margin-canary.jsonl"),
            emergency_stop_path: dir.join("margin-canary.stop"),
        }
    }

    #[tokio::test]
    async fn allowed_plan_reads_only_and_writes_audit() {
        let dir = tempfile::tempdir().unwrap();
        let server = server("3.2", "0.00001").await;
        let alerter = RecordingAlerter::default();
        let result = run_margin_canary(
            &settings(&["BNB/USDT"]),
            &mock_client(&server),
            &alerter,
            MarginCanaryFlags::default(),
            &inputs(true, true),
            &["BNB/USDT".into()],
            &limits(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(result.status, "planned", "{}", result.error);
        assert!(!result.submission_attempted);
        assert!(!result.telegram_sent);
        assert!(alerter.messages().is_empty());
        assert!(
            request_paths(&server)
                .await
                .iter()
                .all(|p| p.starts_with("GET "))
        );
        let rows = std::fs::read_to_string(dir.path().join("margin-canary.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(rows.lines().next().unwrap()).unwrap();
        assert_eq!(row["status"], "planned");
        assert_eq!(row["write_endpoints_used"], json!([]));
        assert!(
            render_margin_canary_result(&result).contains("Margin foreground canary: plan only")
        );
    }

    #[tokio::test]
    async fn scope_and_emergency_stop_block_before_reads() {
        let dir = tempfile::tempdir().unwrap();
        let server = server("3.2", "0.00001").await;
        let client = mock_client(&server);
        let alerter = RecordingAlerter::default();
        let err = run_margin_canary(
            &settings(&[]),
            &client,
            &alerter,
            MarginCanaryFlags::default(),
            &inputs(true, true),
            &["BNB/USDT".into()],
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("API scope audit failed"));
        std::fs::write(dir.path().join("margin-canary.stop"), "").unwrap();
        let err = run_margin_canary(
            &settings(&["BNB/USDT"]),
            &client,
            &alerter,
            MarginCanaryFlags::default(),
            &inputs(true, true),
            &[],
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("emergency stop is active"));
        for flags in [
            MarginCanaryFlags {
                submit_action: true,
                ..MarginCanaryFlags::default()
            },
            MarginCanaryFlags {
                confirm_live_margin: true,
                ..MarginCanaryFlags::default()
            },
        ] {
            let err = run_margin_canary(
                &settings(&["BNB/USDT"]),
                &client,
                &alerter,
                flags,
                &inputs(true, true),
                &[],
                &limits(dir.path()),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("plan-only"));
        }
        assert!(request_paths(&server).await.is_empty());
        assert!(!dir.path().join("margin-canary.jsonl").exists());
    }

    #[tokio::test]
    async fn risk_rejection_is_audited_and_alerted() {
        let dir = tempfile::tempdir().unwrap();
        let server = server("1.2", "0.01").await;
        let alerter = RecordingAlerter::default();
        let result = run_margin_canary(
            &settings(&["BNB/USDT"]),
            &mock_client(&server),
            &alerter,
            MarginCanaryFlags::default(),
            &inputs(true, false),
            &[],
            &limits(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(result.status, "risk_rejected");
        assert!(
            result
                .error
                .contains("borrow: a documented repayment intent is required")
        );
        assert!(
            result
                .error
                .contains("borrow: current margin level is below the configured minimum")
        );
        assert!(result.telegram_sent);
        let messages = alerter.messages();
        assert!(messages[0].starts_with("<b>MARGIN CANARY PLAN</b>"));
        assert!(messages[0].contains("risk_rejected"));
    }
}

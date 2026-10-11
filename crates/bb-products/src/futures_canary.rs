//! Plan-only foreground USD-M futures canary (port of `src/futures_canary.py`).
//!
//! Plans an entry plus a mandatory reduce-only exit intent; there is no
//! submission branch.

use std::path::PathBuf;

use bb_binance::{BinanceClient, normalize_symbol};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::common::{self, append_audit, fmt_dec, num, num_opt};
use crate::futures::{
    FuturesModelError, FuturesOrderIntent, FuturesOrderPlan, FuturesOrderType,
    FuturesReconciliation, FuturesRiskLimits, OrderSide, PositionSide,
    build_futures_dry_run_report,
};
use crate::scope_audit::{
    ScopeAuditError, ScopeInputs, build_scope_audit_report, ensure_scope_allowed,
};
use crate::settings::Settings;

pub const DEFAULT_AUDIT_PATH: &str = "data/futures-canary.jsonl";
pub const DEFAULT_STOP_PATH: &str = "data/futures-canary.stop";

#[derive(Debug, thiserror::Error)]
pub enum FuturesCanaryError {
    #[error("{0}")]
    Blocked(String),
    #[error(transparent)]
    Scope(#[from] ScopeAuditError),
    #[error(transparent)]
    Model(#[from] FuturesModelError),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

fn blocked(msg: impl Into<String>) -> FuturesCanaryError {
    FuturesCanaryError::Blocked(msg.into())
}

/// Operational controls for one foreground planning run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuturesCanaryControls {
    pub audit_path: PathBuf,
    pub emergency_stop_path: PathBuf,
}

impl Default for FuturesCanaryControls {
    fn default() -> Self {
        Self {
            audit_path: DEFAULT_AUDIT_PATH.into(),
            emergency_stop_path: DEFAULT_STOP_PATH.into(),
        }
    }
}

/// Mandatory closure intent for an entry plan, never submitted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesExitPlanIntent {
    pub symbol: String,
    pub side: OrderSide,
    #[serde(with = "num")]
    pub quantity: Decimal,
    pub trigger: String,
    pub order_type: FuturesOrderType,
    #[serde(with = "num_opt")]
    pub price: Option<Decimal>,
    pub position_side: PositionSide,
    pub reduce_only: bool,
}

/// Auditable result from one plan-only foreground canary run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesCanaryResult {
    pub timestamp: String,
    pub mode: String,
    pub status: String,
    pub symbol: String,
    pub entry_plan: FuturesOrderPlan,
    pub exit_plan_intent: FuturesExitPlanIntent,
    pub reconciliation: FuturesReconciliation,
    pub scope_report: serde_json::Value,
    pub rejection_reasons: Vec<String>,
    pub submission_attempted: bool,
    pub live_execution_enabled: bool,
    pub write_endpoints_used: Vec<String>,
    pub audit_path: String,
}

/// Flags that must stay off.
#[derive(Debug, Clone, Copy, Default)]
pub struct FuturesCanaryFlags {
    pub submit_order: bool,
    pub confirm_live_futures: bool,
}

/// Plan a USD-M canary entry and documented reduce-only exit.
pub async fn run_futures_canary(
    settings: &Settings,
    client: &BinanceClient,
    intent: &FuturesOrderIntent,
    exit_plan: &FuturesExitPlanIntent,
    limits: &FuturesRiskLimits,
    controls: &FuturesCanaryControls,
    flags: FuturesCanaryFlags,
) -> Result<FuturesCanaryResult, FuturesCanaryError> {
    if flags.submit_order {
        return Err(blocked(
            "USD-M futures canary is plan-only; order submission is disabled",
        ));
    }
    if flags.confirm_live_futures {
        return Err(blocked(
            "USD-M futures canary is plan-only; live confirmation is disabled",
        ));
    }
    if controls.audit_path.as_os_str().is_empty() {
        return Err(blocked("audit_path must not be blank"));
    }
    if controls.emergency_stop_path.as_os_str().is_empty() {
        return Err(blocked("emergency_stop_path must not be blank"));
    }
    if controls.emergency_stop_path.exists() {
        return Err(blocked(format!(
            "USD-M futures canary emergency stop is active: {}",
            controls.emergency_stop_path.display()
        )));
    }
    let symbol = canary_symbol(&intent.symbol)?;
    validate_exit_plan(intent, exit_plan)?;
    let futures_symbols = vec![symbol.clone()];
    let scope = build_scope_audit_report(
        settings,
        &ScopeInputs {
            futures_symbols: &futures_symbols,
            ..ScopeInputs::default()
        },
    )?;
    ensure_scope_allowed(&scope)?;

    let report =
        build_futures_dry_run_report(settings, client, &futures_symbols, Some(intent), limits)
            .await?;
    if report.live_trading_enabled || !report.write_endpoints_used.is_empty() {
        return Err(blocked(
            "read-only futures model reported a live write path",
        ));
    }
    let (Some(entry_plan), Some(reconciliation)) = (report.order_plan, report.reconciliation)
    else {
        return Err(blocked(
            "futures canary planning did not produce a complete report",
        ));
    };
    let mut reasons: Vec<String> = Vec::new();
    for reason in entry_plan
        .rejection_reasons
        .iter()
        .chain(&reconciliation.rejection_reasons)
    {
        if !reasons.contains(reason) {
            reasons.push(reason.clone());
        }
    }
    let allowed = entry_plan.allowed && reconciliation.allowed && reasons.is_empty();
    let result = FuturesCanaryResult {
        timestamp: common::utc_timestamp(),
        mode: if settings.use_testnet {
            "testnet-plan-only"
        } else {
            "mainnet-plan-only"
        }
        .to_owned(),
        status: if allowed { "planned" } else { "blocked" }.to_owned(),
        symbol,
        entry_plan,
        exit_plan_intent: FuturesExitPlanIntent {
            symbol: canary_symbol(&exit_plan.symbol)?,
            trigger: exit_plan.trigger.trim().to_owned(),
            ..exit_plan.clone()
        },
        reconciliation,
        scope_report: scope.to_json(),
        rejection_reasons: reasons,
        submission_attempted: false,
        live_execution_enabled: false,
        write_endpoints_used: Vec::new(),
        audit_path: controls.audit_path.display().to_string(),
    };
    append_audit(&controls.audit_path, &result)?;
    Ok(result)
}

/// Concise plan-only canary result.
pub fn render_futures_canary_result(result: &FuturesCanaryResult) -> String {
    let plan = &result.entry_plan;
    let exit = &result.exit_plan_intent;
    let mut lines = vec![
        format!("Mode: {}", result.mode),
        "Live futures execution: disabled".to_owned(),
        "Write endpoints used: none".to_owned(),
        format!("Status: {}", result.status),
        format!("Symbol: {}", result.symbol),
        format!(
            "Entry side/quantity: {} {}",
            plan.side.as_str(),
            fmt_dec(plan.quantity)
        ),
        format!("Entry notional USDT: {}", fmt_dec(plan.notional_usdt)),
        format!("Configured leverage: {}", fmt_dec(plan.configured_leverage)),
        format!(
            "Exit intent: reduce-only {} {}",
            exit.side.as_str(),
            fmt_dec(exit.quantity)
        ),
        format!("Exit trigger: {}", exit.trigger),
        format!("Audit path: {}", result.audit_path),
    ];
    if !result.rejection_reasons.is_empty() {
        lines.push(format!(
            "Rejection reasons: {}",
            result.rejection_reasons.join(", ")
        ));
    }
    lines.join("\n")
}

fn validate_exit_plan(
    intent: &FuturesOrderIntent,
    exit: &FuturesExitPlanIntent,
) -> Result<(), FuturesCanaryError> {
    if canary_symbol(&exit.symbol)? != canary_symbol(&intent.symbol)? {
        return Err(blocked("exit plan symbol must match the entry symbol"));
    }
    if exit.side == intent.side {
        return Err(blocked("exit plan side must oppose the entry side"));
    }
    if !exit.reduce_only {
        return Err(blocked("exit plan intent must be reduce-only"));
    }
    if exit.position_side != intent.position_side {
        return Err(blocked(
            "exit plan position_side must match the entry intent",
        ));
    }
    let entry_quantity = positive(Some(intent.quantity), "entry quantity")?;
    let exit_quantity = positive(Some(exit.quantity), "exit quantity")?;
    if entry_quantity != exit_quantity {
        return Err(blocked(
            "exit plan quantity must fully close the planned entry",
        ));
    }
    if exit.trigger.trim().is_empty() {
        return Err(blocked("exit plan trigger must not be blank"));
    }
    if exit.order_type == FuturesOrderType::Limit {
        positive(exit.price, "exit price")?;
    }
    Ok(())
}

/// Normalised canary symbol (`BTC/USDT`).
pub fn canary_symbol(value: &str) -> Result<String, FuturesCanaryError> {
    normalize_symbol(value).map_err(|_| blocked(format!("invalid futures canary symbol: {value}")))
}

fn positive(value: Option<Decimal>, field: &str) -> Result<Decimal, FuturesCanaryError> {
    value
        .filter(|v| *v > Decimal::ZERO)
        .ok_or_else(|| blocked(format!("{field} must be a positive finite number")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rust_decimal_macros::dec;
    use wiremock::MockServer;

    use super::*;
    use crate::futures::tests::{Fixture, mount_futures};
    use crate::testutil::{mock_client, request_paths, settings_builder};

    fn settings(whitelist: &[&str]) -> Settings {
        let mut builder = settings_builder();
        builder.api_symbol_whitelist = whitelist.iter().map(|s| (*s).to_owned()).collect();
        builder.build().unwrap()
    }

    fn intent() -> FuturesOrderIntent {
        FuturesOrderIntent::market("BTC/USDT", OrderSide::Buy, dec!(0.001))
    }

    fn exit() -> FuturesExitPlanIntent {
        FuturesExitPlanIntent {
            symbol: "BTC/USDT".into(),
            side: OrderSide::Sell,
            quantity: dec!(0.001),
            trigger: " operator approved close ".into(),
            order_type: FuturesOrderType::Market,
            price: None,
            position_side: PositionSide::Both,
            reduce_only: true,
        }
    }

    fn limits() -> FuturesRiskLimits {
        FuturesRiskLimits {
            max_notional_usdt: dec!(100),
            max_account_notional_percent: dec!(100),
            max_initial_margin_usdt: dec!(20),
            ..FuturesRiskLimits::default()
        }
    }

    fn controls(dir: &Path) -> FuturesCanaryControls {
        FuturesCanaryControls {
            audit_path: dir.join("futures-canary.jsonl"),
            emergency_stop_path: dir.join("futures-canary.stop"),
        }
    }

    #[tokio::test]
    async fn plans_entry_and_reduce_only_exit_with_audit() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_futures(&server, Fixture::default()).await;
        let result = run_futures_canary(
            &settings(&["BTC/USDT"]),
            &mock_client(&server),
            &intent(),
            &exit(),
            &limits(),
            &controls(dir.path()),
            FuturesCanaryFlags::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.status, "planned", "{:?}", result.rejection_reasons);
        assert_eq!(result.mode, "testnet-plan-only");
        assert_eq!(result.exit_plan_intent.trigger, "operator approved close");
        assert!(!result.submission_attempted);
        assert!(
            request_paths(&server)
                .await
                .iter()
                .all(|p| p.starts_with("GET "))
        );
        let row: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(dir.path().join("futures-canary.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(row["exit_plan_intent"]["side"], "SELL");
        assert_eq!(row["scope_report"]["allowed"], true);
        assert!(
            render_futures_canary_result(&result).contains("Exit intent: reduce-only SELL 0.001")
        );
    }

    #[tokio::test]
    async fn risk_block_is_audited_and_invalid_exit_fails_before_reads() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_futures(
            &server,
            Fixture {
                config: crate::futures::tests::symbol_config("10"),
                ..Fixture::default()
            },
        )
        .await;
        let client = mock_client(&server);
        let settings = settings(&["BTC/USDT"]);
        let bad_exits = [
            (
                FuturesExitPlanIntent {
                    side: OrderSide::Buy,
                    ..exit()
                },
                "oppose",
            ),
            (
                FuturesExitPlanIntent {
                    reduce_only: false,
                    ..exit()
                },
                "reduce-only",
            ),
            (
                FuturesExitPlanIntent {
                    quantity: dec!(0.002),
                    ..exit()
                },
                "fully close",
            ),
            (
                FuturesExitPlanIntent {
                    trigger: " ".into(),
                    ..exit()
                },
                "trigger",
            ),
            (
                FuturesExitPlanIntent {
                    symbol: "ETH/USDT".into(),
                    ..exit()
                },
                "symbol must match",
            ),
        ];
        for (bad, message) in bad_exits {
            let err = run_futures_canary(
                &settings,
                &client,
                &intent(),
                &bad,
                &limits(),
                &controls(dir.path()),
                FuturesCanaryFlags::default(),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains(message), "{err}");
        }
        let err = run_futures_canary(
            &self::settings(&[]),
            &client,
            &intent(),
            &exit(),
            &limits(),
            &controls(dir.path()),
            FuturesCanaryFlags::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("API scope audit failed"));
        let err = run_futures_canary(
            &settings,
            &client,
            &intent(),
            &exit(),
            &limits(),
            &controls(dir.path()),
            FuturesCanaryFlags {
                submit_order: true,
                ..FuturesCanaryFlags::default()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("order submission is disabled"));
        assert!(request_paths(&server).await.is_empty());

        let result = run_futures_canary(
            &settings,
            &client,
            &intent(),
            &exit(),
            &limits(),
            &controls(dir.path()),
            FuturesCanaryFlags::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.status, "blocked");
        assert!(
            result
                .rejection_reasons
                .contains(&"configured leverage exceeds the risk limit".to_owned())
        );
        assert!(dir.path().join("futures-canary.jsonl").exists());

        std::fs::write(dir.path().join("futures-canary.stop"), "").unwrap();
        let err = run_futures_canary(
            &settings,
            &client,
            &intent(),
            &exit(),
            &limits(),
            &controls(dir.path()),
            FuturesCanaryFlags::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("emergency stop is active"));
    }
}

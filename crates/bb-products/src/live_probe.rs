//! One-shot real-money live probe (port of `src/live_probe.py`).
//!
//! Submits exactly one direct-to-USDT market sell after `--confirm-real-order`,
//! live settings, the local scope audit and a safe `/api/v3/order/test`.

use std::path::{Path, PathBuf};

use bb_binance::fills::FillSummary;
use bb_binance::spot::{AccountCommission, NewOrder};
use bb_binance::{BinanceClient, BinanceError, OrderType, Side};
use bb_core::telegram::escape;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;

use crate::alerts::Alerter;
use crate::api_permissions::{ApiPermissionError, SpotOrderTest, ensure_spot_order_test_allowed};
use crate::common::{self, append_audit, fmt_dec, num, num_opt};
use crate::scope_audit::{
    ScopeAuditError, ScopeInputs, build_scope_audit_report, ensure_scope_allowed,
};
use crate::settings::Settings;
use crate::spot_common::{self, OrderFee, active_market, fetch_balance, fetch_ticker};

pub const DEFAULT_AUDIT_PATH: &str = "data/live-probes.jsonl";

#[derive(Debug, thiserror::Error)]
pub enum LiveProbeError {
    #[error("{0}")]
    Blocked(String),
    #[error(transparent)]
    Permission(#[from] ApiPermissionError),
    #[error(transparent)]
    Scope(#[from] ScopeAuditError),
    #[error(transparent)]
    Binance(#[from] BinanceError),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

fn blocked(msg: impl Into<String>) -> LiveProbeError {
    LiveProbeError::Blocked(msg.into())
}

/// One direct-to-USDT live sell candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveProbeCandidate {
    pub symbol: String,
    pub asset: String,
    #[serde(with = "num")]
    pub free_amount: Decimal,
    #[serde(with = "num")]
    pub sell_amount: Decimal,
    #[serde(with = "num")]
    pub estimated_price: Decimal,
    #[serde(with = "num")]
    pub estimated_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub min_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub min_amount: Decimal,
    #[serde(with = "num")]
    pub cap_usdt: Decimal,
    pub cap_exceeded_to_satisfy_minimum: bool,
}

/// Auditable outcome of one submitted probe order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveProbeResult {
    pub timestamp: String,
    pub mode: String,
    pub symbol: String,
    pub side: String,
    #[serde(with = "num")]
    pub amount: Decimal,
    #[serde(with = "num")]
    pub estimated_notional_usdt: Decimal,
    pub order_id: Option<String>,
    pub order_status: String,
    #[serde(with = "num_opt")]
    pub filled_amount: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub average_price: Option<Decimal>,
    pub fee_summary: Vec<OrderFee>,
    pub commission: Option<AccountCommission>,
    pub audit_path: String,
    pub telegram_sent: bool,
}

/// Submit exactly one direct-to-USDT market sell after live preflight.
pub async fn run_live_probe(
    settings: &Settings,
    client: &BinanceClient,
    alerter: &dyn Alerter,
    confirm_real_order: bool,
    max_notional_usdt: Decimal,
    audit_path: &Path,
) -> Result<LiveProbeResult, LiveProbeError> {
    preflight(settings, confirm_real_order)?;
    if max_notional_usdt <= Decimal::ZERO {
        return Err(blocked("max_notional_usdt must be a positive finite value"));
    }
    let balance = fetch_balance(client).await?;
    let candidate = select_live_sell_candidate(client, &balance.free, max_notional_usdt).await?;
    let canary = vec![candidate.symbol.clone()];
    ensure_scope_allowed(&build_scope_audit_report(
        settings,
        &ScopeInputs {
            canary_symbols: &canary,
            ..ScopeInputs::default()
        },
    )?)?;
    ensure_spot_order_test_allowed(
        client,
        &SpotOrderTest {
            symbol: candidate.symbol.clone(),
            side: Side::Sell,
            order_type: OrderType::Market,
            quantity: candidate.sell_amount,
            price: None,
        },
    )
    .await?;
    let commission = Some(client.account_commission(&candidate.symbol).await?);
    let order = client
        .create_order(&NewOrder::market(
            &candidate.symbol,
            Side::Sell,
            candidate.sell_amount,
        ))
        .await?;
    let final_order = spot_common::final_order_status(client, &candidate.symbol, order).await?;
    let fills = FillSummary::from_order(&final_order);
    let mut result = LiveProbeResult {
        timestamp: common::utc_timestamp(),
        mode: "LIVE".to_owned(),
        symbol: candidate.symbol.clone(),
        side: "sell".to_owned(),
        amount: candidate.sell_amount,
        estimated_notional_usdt: candidate.estimated_notional_usdt,
        order_id: Some(final_order.order_id.to_string()),
        order_status: final_order.status.ccxt().to_owned(),
        filled_amount: Some(final_order.executed_qty),
        average_price: final_order.average_price(),
        fee_summary: spot_common::fee_summary(&fills),
        commission,
        audit_path: String::new(),
        telegram_sent: false,
    };
    let audit = append_audit(audit_path, &result)?;
    result.telegram_sent = send_alert(alerter, &result, &audit).await;
    result.audit_path = audit.display().to_string();
    Ok(result)
}

/// Choose the smallest direct USDT market sell that Binance rules allow
/// (BNB first, then alphabetical).
pub async fn select_live_sell_candidate(
    client: &BinanceClient,
    free_balances: &std::collections::BTreeMap<String, Decimal>,
    max_notional_usdt: Decimal,
) -> Result<LiveProbeCandidate, LiveProbeError> {
    let markets = client.spot_markets(false).await?;
    let mut assets: Vec<&String> = free_balances
        .iter()
        .filter(|(_, amount)| **amount > Decimal::ZERO)
        .map(|(asset, _)| asset)
        .collect();
    assets.sort_by_key(|asset| (asset.as_str() != "BNB", asset.as_str()));
    for asset in assets {
        let free = free_balances[asset];
        let symbol = format!("{asset}/USDT");
        let Some(rules) = active_market(&markets, &symbol) else {
            continue;
        };
        let ticker = fetch_ticker(client, &symbol).await?;
        let Some(price) = ticker.sell_price() else {
            continue;
        };
        let min_amount = spot_common::min_amount(&rules);
        let min_notional = spot_common::min_notional(&rules);
        let mut sell_amount = spot_common::round_up(&rules, min_amount.max(min_notional / price));
        if sell_amount > free {
            let full = spot_common::round_down(&rules, free);
            if full > Decimal::ZERO && full >= min_amount && full * price >= min_notional {
                sell_amount = full;
            } else {
                continue;
            }
        }
        let notional = sell_amount * price;
        return Ok(LiveProbeCandidate {
            symbol,
            asset: asset.clone(),
            free_amount: free,
            sell_amount,
            estimated_price: price,
            estimated_notional_usdt: notional,
            min_notional_usdt: min_notional,
            min_amount,
            cap_usdt: max_notional_usdt,
            cap_exceeded_to_satisfy_minimum: notional > max_notional_usdt,
        });
    }
    Err(blocked(
        "no free asset has a direct USDT market satisfying Binance minimums",
    ))
}

/// Live gate: confirmation flag plus live settings.
pub fn preflight(settings: &Settings, confirm_real_order: bool) -> Result<(), LiveProbeError> {
    if !confirm_real_order {
        return Err(blocked("live probe requires --confirm-real-order"));
    }
    if settings.use_testnet {
        return Err(blocked("live probe requires USE_TESTNET=false"));
    }
    if settings.dry_run {
        return Err(blocked("live probe requires DRY_RUN=false"));
    }
    if settings.confirm_live_trading != "yes" {
        return Err(blocked("live probe requires CONFIRM_LIVE_TRADING=yes"));
    }
    Ok(())
}

async fn send_alert(alerter: &dyn Alerter, result: &LiveProbeResult, audit: &Path) -> bool {
    alerter
        .send(&format!(
            "\u{26a0}\u{fe0f} <b>LIVE PROBE</b>\nSide: <code>{}</code>\nSymbol: <code>{}</code>\nAmount: <code>{}</code>\nStatus: <code>{}</code>\nAudit: <code>{}</code>",
            result.side.to_uppercase(),
            escape(&result.symbol),
            fmt_dec(result.amount),
            escape(&result.order_status),
            escape(&audit.display().to_string()),
        ))
        .await
}

/// Default probe cap (Python `--max-notional-usdt` default).
pub const DEFAULT_MAX_NOTIONAL_USDT: Decimal = dec!(6.0);

/// Default audit path honouring `LIVE_PROBE_AUDIT_PATH`.
pub fn default_audit_path() -> PathBuf {
    bb_core::env::string(&["LIVE_PROBE_AUDIT_PATH"], DEFAULT_AUDIT_PATH).into()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wiremock::MockServer;

    use super::*;
    use crate::alerts::testing::RecordingAlerter;
    use crate::spot_canary::tests::mount_spot;
    use crate::testutil::{mock_client, request_paths, settings_builder};

    fn settings(live: bool) -> Settings {
        let mut builder = settings_builder();
        builder.api_symbol_whitelist = vec!["BNB/USDT".into()];
        if live {
            builder.use_testnet = false;
            builder.confirm_live_trading = "yes".into();
        }
        builder.build().unwrap()
    }

    #[tokio::test]
    async fn selects_smallest_valid_bnb_sell_first() {
        let server = MockServer::start().await;
        mount_spot(&server, "1", "600", "600.1").await;
        let client = mock_client(&server);
        let free = BTreeMap::from([("BNB".to_owned(), dec!(1)), ("BTC".to_owned(), dec!(1))]);
        let candidate = select_live_sell_candidate(&client, &free, dec!(6))
            .await
            .unwrap();
        assert_eq!(candidate.symbol, "BNB/USDT");
        // ceil(5 / 600 = 0.008333) to the 0.001 step.
        assert_eq!(candidate.sell_amount, dec!(0.009));
        assert!(!candidate.cap_exceeded_to_satisfy_minimum);
        let tiny = BTreeMap::from([("BNB".to_owned(), dec!(0.001))]);
        assert!(
            select_live_sell_candidate(&client, &tiny, dec!(6))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn preflight_blocks_without_confirmation_or_live_settings() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_spot(&server, "1", "600", "600.1").await;
        let client = mock_client(&server);
        let alerter = RecordingAlerter::default();
        let audit = dir.path().join("probes.jsonl");
        let err = run_live_probe(&settings(true), &client, &alerter, false, dec!(6), &audit)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "live probe requires --confirm-real-order");
        let err = run_live_probe(&settings(false), &client, &alerter, true, dec!(6), &audit)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "live probe requires USE_TESTNET=false");
        assert!(request_paths(&server).await.is_empty());
        assert!(!audit.exists());
    }
}

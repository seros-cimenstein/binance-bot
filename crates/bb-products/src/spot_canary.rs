//! One tightly gated direct spot canary order (port of `src/spot_canary.py`).
//!
//! `--plan-only` reads balances, rules, the ticker and commissions, then audits
//! a proposed sell-to-USDT. Submission requires every confirmation flag,
//! `USE_TESTNET=false`, `DRY_RUN=false`, `CONFIRM_LIVE_TRADING=yes`, a passing
//! permission report and a safe `/api/v3/order/test` before one market order.

use std::path::{Path, PathBuf};

use bb_binance::spot::NewOrder;
use bb_binance::{BinanceClient, BinanceError, OrderType, Side};
use bb_core::telegram::escape;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;

use crate::alerts::Alerter;
use crate::api_permissions::{
    ApiPermissionError, Feature, InspectOptions, SpotOrderTest, ensure_feature_allowed,
    ensure_spot_order_test_allowed, inspect_api_permissions,
};
use crate::commission::fetch_commission;
use crate::common::{self, append_audit, fmt_dec, num};
use crate::scope_audit::{
    ScopeAuditError, ScopeInputs, build_scope_audit_report, ensure_scope_allowed,
};
use crate::settings::Settings;
use crate::spot_common::{
    self, BalanceSnapshot, RouteLeg, RouteQuote, SanitizedOrder, active_market, daily_usage,
    fetch_balance, fetch_ticker,
};

pub const DEFAULT_AUDIT_PATH: &str = "data/spot-canary.jsonl";
pub const DEFAULT_STOP_PATH: &str = "data/spot-canary.stop";

#[derive(Debug, thiserror::Error)]
pub enum SpotCanaryError {
    #[error("{0}")]
    Blocked(String),
    #[error(transparent)]
    Scope(#[from] ScopeAuditError),
    #[error(transparent)]
    Binance(#[from] BinanceError),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

fn blocked(msg: impl Into<String>) -> SpotCanaryError {
    SpotCanaryError::Blocked(msg.into())
}

/// Configured hard limits for one canary run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotCanaryLimits {
    pub max_order_notional_usdt: Decimal,
    pub max_daily_notional_usdt: Decimal,
    pub max_daily_orders: u32,
    pub max_spread_bps: Decimal,
    pub audit_path: PathBuf,
    pub emergency_stop_path: PathBuf,
}

impl Default for SpotCanaryLimits {
    fn default() -> Self {
        Self {
            max_order_notional_usdt: dec!(6.0),
            max_daily_notional_usdt: dec!(6.0),
            max_daily_orders: 1,
            max_spread_bps: dec!(50.0),
            audit_path: DEFAULT_AUDIT_PATH.into(),
            emergency_stop_path: DEFAULT_STOP_PATH.into(),
        }
    }
}

impl SpotCanaryLimits {
    fn validate(&self) -> Result<(), SpotCanaryError> {
        for (value, name) in [
            (self.max_order_notional_usdt, "max_order_notional_usdt"),
            (self.max_daily_notional_usdt, "max_daily_notional_usdt"),
            (self.max_spread_bps, "max_spread_bps"),
        ] {
            if value <= Decimal::ZERO {
                return Err(blocked(format!("{name} must be a positive finite value")));
            }
        }
        if self.max_daily_orders == 0 {
            return Err(blocked("max_daily_orders must be greater than zero"));
        }
        if self.max_daily_notional_usdt < self.max_order_notional_usdt {
            return Err(blocked(
                "max_daily_notional_usdt must be at least max_order_notional_usdt",
            ));
        }
        if self.audit_path.as_os_str().is_empty() {
            return Err(blocked("audit_path must not be blank"));
        }
        if self.emergency_stop_path.as_os_str().is_empty() {
            return Err(blocked("emergency_stop_path must not be blank"));
        }
        Ok(())
    }
}

/// Commission block of a canary plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanaryCommission {
    pub symbol: String,
    #[serde(with = "num")]
    pub taker_rate_sell: Decimal,
    pub discount_enabled: bool,
    pub discount_asset: String,
    pub retrieved_at: String,
}

/// One direct spot canary conversion plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpotCanaryPlan {
    pub timestamp: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub symbol: String,
    pub side: String,
    #[serde(with = "num")]
    pub amount: Decimal,
    #[serde(with = "num")]
    pub free_source_amount: Decimal,
    #[serde(with = "num")]
    pub estimated_price: Decimal,
    #[serde(with = "num")]
    pub estimated_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub expected_destination_amount: Decimal,
    #[serde(with = "num")]
    pub fee_rate: Decimal,
    #[serde(with = "num")]
    pub estimated_fee_destination: Decimal,
    #[serde(with = "num")]
    pub min_amount: Decimal,
    #[serde(with = "num")]
    pub min_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub spread_bps: Decimal,
    #[serde(with = "num")]
    pub max_spread_bps: Decimal,
    #[serde(with = "num")]
    pub max_order_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub max_daily_notional_usdt: Decimal,
    pub max_daily_orders: u32,
    pub daily_orders_used: u32,
    #[serde(with = "num")]
    pub daily_notional_used_usdt: Decimal,
    pub route_quote: RouteQuote,
    pub commission: CanaryCommission,
}

/// Auditable outcome of a spot canary plan or submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpotCanaryResult {
    pub timestamp: String,
    pub mode: String,
    pub status: String,
    pub plan: SpotCanaryPlan,
    pub submission_attempted: bool,
    pub order: Option<SanitizedOrder>,
    pub final_order: Option<SanitizedOrder>,
    pub balance_before: BalanceSnapshot,
    pub balance_after: BalanceSnapshot,
    pub error: String,
    pub audit_path: String,
    pub telegram_sent: bool,
}

impl SpotCanaryResult {
    fn new(
        settings: &Settings,
        status: &str,
        plan: SpotCanaryPlan,
        balance_before: BalanceSnapshot,
    ) -> Self {
        Self {
            timestamp: common::utc_timestamp(),
            mode: common::mode(settings).to_owned(),
            status: status.to_owned(),
            plan,
            submission_attempted: false,
            order: None,
            final_order: None,
            balance_before,
            balance_after: BalanceSnapshot::default(),
            error: String::new(),
            audit_path: String::new(),
            telegram_sent: false,
        }
    }
}

/// Operator confirmations required for submission.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpotCanaryConfirmations {
    pub submit_order: bool,
    pub confirm_live_canary: bool,
    pub confirm_replay_passed: bool,
    pub confirm_paper_warmup_reviewed: bool,
}

/// Plan or submit one strictly gated direct spot canary order.
pub async fn run_spot_canary(
    settings: &Settings,
    client: &BinanceClient,
    alerter: &dyn Alerter,
    confirmations: SpotCanaryConfirmations,
    source_asset: &str,
    destination_asset: &str,
    limits: &SpotCanaryLimits,
) -> Result<SpotCanaryResult, SpotCanaryError> {
    limits.validate()?;
    preflight(settings, confirmations)?;
    let canary = vec![format!("{source_asset}/{destination_asset}")];
    ensure_scope_allowed(&build_scope_audit_report(
        settings,
        &ScopeInputs {
            canary_symbols: &canary,
            ..ScopeInputs::default()
        },
    )?)?;
    let (plan, balance_before) =
        build_spot_canary_plan(client, source_asset, destination_asset, limits).await?;
    let audit_path = &limits.audit_path;

    if !confirmations.submit_order {
        let mut result = SpotCanaryResult::new(settings, "planned", plan, balance_before);
        result.audit_path = append_audit(audit_path, &result)?.display().to_string();
        return Ok(result);
    }

    if let Err(err) = permission_gate(settings, client, &plan).await {
        let mut result = SpotCanaryResult::new(settings, "blocked", plan, balance_before);
        result.error = err.to_string();
        let audit = append_audit(audit_path, &result)?;
        send_alert(alerter, &result, &audit).await;
        return Err(blocked(err.to_string()));
    }

    let order = match client
        .create_order(&NewOrder::market(&plan.symbol, Side::Sell, plan.amount))
        .await
    {
        Ok(order) => order,
        Err(err) => {
            let mut result =
                SpotCanaryResult::new(settings, "submission_error", plan, balance_before);
            result.submission_attempted = true;
            result.error = err.to_string();
            let audit = append_audit(audit_path, &result)?;
            send_alert(alerter, &result, &audit).await;
            return Err(blocked(
                "spot canary order submission failed; no retry was attempted",
            ));
        }
    };
    let final_order = spot_common::final_order_status(client, &plan.symbol, order.clone()).await?;
    let balance_after = fetch_balance(client).await?;
    let symbol = plan.symbol.clone();
    let mut result = SpotCanaryResult::new(settings, "submitted", plan, balance_before);
    result.submission_attempted = true;
    result.order = Some(SanitizedOrder::from_order(&order, &symbol));
    result.final_order = Some(SanitizedOrder::from_order(&final_order, &symbol));
    result.balance_after = balance_after;
    let audit = append_audit(audit_path, &result)?;
    result.telegram_sent = send_alert(alerter, &result, &audit).await;
    result.audit_path = audit.display().to_string();
    Ok(result)
}

async fn permission_gate(
    settings: &Settings,
    client: &BinanceClient,
    plan: &SpotCanaryPlan,
) -> Result<(), ApiPermissionError> {
    let options = InspectOptions {
        spot_test_symbol: Some(plan.symbol.clone()),
        include_spot_order_test: true,
        features: vec![Feature::SpotLiveCanary],
    };
    let report = inspect_api_permissions(settings, client, &options).await?;
    ensure_feature_allowed(&report, Feature::SpotLiveCanary)?;
    ensure_spot_order_test_allowed(
        client,
        &SpotOrderTest {
            symbol: plan.symbol.clone(),
            side: Side::Sell,
            order_type: OrderType::Market,
            quantity: plan.amount,
            price: None,
        },
    )
    .await
}

/// Build a direct sell-to-USDT canary plan from current Binance data.
pub async fn build_spot_canary_plan(
    client: &BinanceClient,
    source_asset: &str,
    destination_asset: &str,
    limits: &SpotCanaryLimits,
) -> Result<(SpotCanaryPlan, BalanceSnapshot), SpotCanaryError> {
    limits.validate()?;
    if limits.emergency_stop_path.exists() {
        return Err(blocked(format!(
            "spot canary emergency stop is active: {}",
            limits.emergency_stop_path.display()
        )));
    }
    let source = source_asset.trim().to_uppercase();
    let destination = destination_asset.trim().to_uppercase();
    if source.is_empty() {
        return Err(blocked("source_asset must not be blank"));
    }
    if destination != "USDT" {
        return Err(blocked(
            "spot canary v1 only supports destination_asset=USDT",
        ));
    }
    let usage = daily_usage(&limits.audit_path, &spot_common::today());
    if usage.orders >= limits.max_daily_orders {
        return Err(blocked("spot canary daily order limit is exhausted"));
    }
    let remaining_daily = limits.max_daily_notional_usdt - usage.notional;
    if remaining_daily <= Decimal::ZERO {
        return Err(blocked("spot canary daily notional limit is exhausted"));
    }

    let balance = fetch_balance(client).await?;
    let free_source = balance
        .free(&source)
        .ok_or_else(|| blocked(format!("no free {source} balance is available")))?;
    let symbol = format!("{source}/{destination}");
    let markets = client.spot_markets(false).await?;
    let rules = active_market(&markets, &symbol)
        .ok_or_else(|| blocked(format!("{symbol} is not an active direct spot market")))?;
    let ticker = fetch_ticker(client, &symbol).await?;
    let price = ticker
        .sell_price()
        .ok_or_else(|| blocked(format!("{symbol} ticker did not include a sell price")))?;
    let spread = ticker
        .spread_bps()
        .ok_or_else(|| blocked(format!("{symbol} ticker did not include bid/ask spread")))?;
    if spread > limits.max_spread_bps {
        return Err(blocked("spot canary spread exceeds configured limit"));
    }

    let min_amount = spot_common::min_amount(&rules);
    let min_notional = spot_common::min_notional(&rules);
    let cap = limits.max_order_notional_usdt.min(remaining_daily);
    let amount = spot_common::round_down(&rules, free_source.min(cap / price));
    let notional = amount * price;
    if amount <= Decimal::ZERO {
        return Err(blocked("spot canary amount rounded to zero"));
    }
    if amount < min_amount || notional < min_notional {
        return Err(blocked(
            "spot canary order would be below Binance minimum amount or notional",
        ));
    }

    let commission = fetch_commission(client, &symbol).await?;
    let fee_rate = commission.taker_rate("sell", true);
    let fee = notional * fee_rate;
    let expected = notional - fee;
    let discount_asset = if commission.discount_enabled {
        commission.discount_asset.clone()
    } else {
        String::new()
    };
    let route_quote = RouteQuote {
        source_asset: source.clone(),
        destination_asset: destination.clone(),
        input_amount: amount,
        output_amount: expected,
        slippage_bps: spread,
        legs: vec![RouteLeg {
            symbol: symbol.clone(),
            side: "sell".into(),
            source_asset: source.clone(),
            destination_asset: destination.clone(),
            input_amount: amount,
            spent_amount: amount,
            gross_output_amount: notional,
            fee_rate,
            fee_output_amount: fee,
            net_output_amount: expected,
            commission_discount_asset: discount_asset,
        }],
    };
    let plan = SpotCanaryPlan {
        timestamp: common::utc_timestamp(),
        source_asset: source,
        destination_asset: destination,
        symbol: symbol.clone(),
        side: "sell".into(),
        amount,
        free_source_amount: free_source,
        estimated_price: price,
        estimated_notional_usdt: notional,
        expected_destination_amount: expected,
        fee_rate,
        estimated_fee_destination: fee,
        min_amount,
        min_notional_usdt: min_notional,
        spread_bps: spread,
        max_spread_bps: limits.max_spread_bps,
        max_order_notional_usdt: limits.max_order_notional_usdt,
        max_daily_notional_usdt: limits.max_daily_notional_usdt,
        max_daily_orders: limits.max_daily_orders,
        daily_orders_used: usage.orders,
        daily_notional_used_usdt: usage.notional,
        route_quote,
        commission: CanaryCommission {
            symbol: commission.symbol.clone(),
            taker_rate_sell: fee_rate,
            discount_enabled: commission.discount_enabled,
            discount_asset: commission.discount_asset.clone(),
            retrieved_at: common::iso(commission.retrieved_at),
        },
    };
    Ok((plan, balance))
}

/// Submission gate (no-op for plan-only runs).
pub fn preflight(settings: &Settings, c: SpotCanaryConfirmations) -> Result<(), SpotCanaryError> {
    if !c.submit_order {
        return Ok(());
    }
    if !c.confirm_live_canary {
        return Err(blocked("spot canary requires --confirm-live-canary"));
    }
    if !c.confirm_replay_passed {
        return Err(blocked("spot canary requires --confirm-replay-passed"));
    }
    if !c.confirm_paper_warmup_reviewed {
        return Err(blocked(
            "spot canary requires --confirm-paper-warmup-reviewed",
        ));
    }
    if settings.use_testnet {
        return Err(blocked("spot canary requires USE_TESTNET=false"));
    }
    if settings.dry_run {
        return Err(blocked("spot canary requires DRY_RUN=false"));
    }
    if settings.confirm_live_trading != "yes" {
        return Err(blocked("spot canary requires CONFIRM_LIVE_TRADING=yes"));
    }
    Ok(())
}

async fn send_alert(alerter: &dyn Alerter, result: &SpotCanaryResult, audit: &Path) -> bool {
    alerter
        .send(&format!(
            "<b>SPOT LIVE CANARY</b>\nStatus: <code>{}</code>\nSide: <code>{}</code>\nSymbol: <code>{}</code>\nAmount: <code>{}</code>\nAudit: <code>{}</code>",
            escape(&result.status),
            result.plan.side.to_uppercase(),
            escape(&result.plan.symbol),
            fmt_dec(result.plan.amount),
            escape(&audit.display().to_string()),
        ))
        .await
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::alerts::testing::RecordingAlerter;
    use crate::testutil::{
        mock_client, mount_get, mount_post, request_paths, settings_builder, spot_exchange_info,
    };

    pub(crate) async fn mount_spot(server: &MockServer, bnb_free: &str, bid: &str, ask: &str) {
        mount_get(server, "/api/v3/account", json!({
            "accountType": "SPOT", "canTrade": true, "canWithdraw": false, "permissions": ["SPOT"],
            "balances": [{"asset": "BNB", "free": bnb_free, "locked": "0"}, {"asset": "USDT", "free": "10", "locked": "0"},
                {"asset": "BTC", "free": "0.0001", "locked": "0"}]
        }))
        .await;
        mount_get(server, "/api/v3/exchangeInfo", spot_exchange_info()).await;
        let ticker = |symbol: &str, bid: &str, ask: &str, last: &str| {
            json!({
                "symbol": symbol, "priceChange": "0", "priceChangePercent": "0", "weightedAvgPrice": last,
                "lastPrice": last, "bidPrice": bid, "askPrice": ask, "openPrice": last, "highPrice": last,
                "lowPrice": last, "volume": "1", "quoteVolume": "1", "openTime": 0, "closeTime": 0,
                "firstId": 0, "lastId": 0, "count": 0
            })
        };
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v3/ticker/24hr"))
            .and(wiremock::matchers::query_param("symbol", "BTCUSDT"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(ticker("BTCUSDT", "50000", "50001", "50000.5")),
            )
            .mount(server)
            .await;
        mount_get(
            server,
            "/api/v3/ticker/24hr",
            ticker("BNBUSDT", bid, ask, bid),
        )
        .await;
        mount_get(server, "/api/v3/account/commission", json!({
            "symbol": "BNBUSDT",
            "standardCommission": {"maker": "0.001", "taker": "0.001", "buyer": "0", "seller": "0"},
            "taxCommission": {"maker": "0", "taker": "0", "buyer": "0", "seller": "0"},
            "specialCommission": {"maker": "0", "taker": "0", "buyer": "0", "seller": "0"},
            "discount": {"enabledForAccount": true, "enabledForSymbol": true, "discountAsset": "BNB", "discount": "0.75"}
        }))
        .await;
        mount_post(server, "/api/v3/order/test", 200, json!({})).await;
    }

    fn settings(live: bool, whitelist: &[&str]) -> Settings {
        let mut builder = settings_builder();
        builder.api_symbol_whitelist = whitelist.iter().map(|s| (*s).to_owned()).collect();
        builder.trading_symbols = vec!["BNB/USDT".into()];
        if live {
            builder.use_testnet = false;
            builder.confirm_live_trading = "yes".into();
        }
        builder.build().unwrap()
    }

    fn limits(dir: &Path) -> SpotCanaryLimits {
        SpotCanaryLimits {
            audit_path: dir.join("spot-canary.jsonl"),
            emergency_stop_path: dir.join("spot-canary.stop"),
            ..SpotCanaryLimits::default()
        }
    }

    #[tokio::test]
    async fn plan_only_audits_without_order_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_spot(&server, "1", "600", "600.1").await;
        let alerter = RecordingAlerter::default();
        let result = run_spot_canary(
            &settings(false, &["BNB/USDT"]),
            &mock_client(&server),
            &alerter,
            SpotCanaryConfirmations::default(),
            "bnb",
            "usdt",
            &limits(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(result.status, "planned");
        assert!(!result.submission_attempted);
        assert_eq!(result.plan.amount, dec!(0.01));
        assert_eq!(result.plan.estimated_notional_usdt, dec!(6.00));
        assert_eq!(result.plan.fee_rate, dec!(0.00075));
        assert_eq!(
            result.plan.route_quote.legs[0].commission_discount_asset,
            "BNB"
        );
        assert!(alerter.messages().is_empty());
        let paths = request_paths(&server).await;
        assert!(
            !paths.iter().any(|p| p.contains("/api/v3/order")),
            "{paths:?}"
        );
        let audit = std::fs::read_to_string(dir.path().join("spot-canary.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
        assert_eq!(row["submission_attempted"], false);
        assert_eq!(row["balance_before"]["free"]["BNB"].to_string(), "1");
        assert_eq!(
            row["plan"]["commission"]["taker_rate_sell"].to_string(),
            "0.00075"
        );
    }

    #[tokio::test]
    async fn guards_fail_before_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_spot(&server, "1", "600", "610").await;
        let client = mock_client(&server);
        let alerter = RecordingAlerter::default();
        let err = run_spot_canary(
            &settings(false, &["BNB/USDT"]),
            &client,
            &alerter,
            SpotCanaryConfirmations::default(),
            "BNB",
            "USDT",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "spot canary spread exceeds configured limit"
        );

        let submit = SpotCanaryConfirmations {
            submit_order: true,
            confirm_live_canary: true,
            confirm_replay_passed: true,
            confirm_paper_warmup_reviewed: false,
        };
        let err = run_spot_canary(
            &settings(true, &["BNB/USDT"]),
            &client,
            &alerter,
            submit,
            "BNB",
            "USDT",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--confirm-paper-warmup-reviewed"));
        let all = SpotCanaryConfirmations {
            confirm_paper_warmup_reviewed: true,
            ..submit
        };
        let err = run_spot_canary(
            &settings(false, &["BNB/USDT"]),
            &client,
            &alerter,
            all,
            "BNB",
            "USDT",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "spot canary requires USE_TESTNET=false");
        let err = run_spot_canary(
            &settings(true, &[]),
            &client,
            &alerter,
            all,
            "BNB",
            "USDT",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("API scope audit failed"));
        let err = run_spot_canary(
            &settings(false, &["BNB/USDT", "BNB/BTC"]),
            &client,
            &alerter,
            SpotCanaryConfirmations::default(),
            "BNB",
            "BTC",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("only supports destination_asset=USDT")
        );

        std::fs::write(
            dir.path().join("spot-canary.jsonl"),
            json!({"timestamp": format!("{}T00:00:00+00:00", spot_common::today()), "submission_attempted": true, "plan": {"estimated_notional_usdt": 6}}).to_string(),
        )
        .unwrap();
        let err = run_spot_canary(
            &settings(false, &["BNB/USDT"]),
            &client,
            &alerter,
            SpotCanaryConfirmations::default(),
            "BNB",
            "USDT",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "spot canary daily order limit is exhausted"
        );
        std::fs::write(dir.path().join("spot-canary.stop"), "").unwrap();
        let err = run_spot_canary(
            &settings(false, &["BNB/USDT"]),
            &client,
            &alerter,
            SpotCanaryConfirmations::default(),
            "BNB",
            "USDT",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("emergency stop is active"));
        assert!(
            !request_paths(&server)
                .await
                .iter()
                .any(|p| p == "POST /api/v3/order")
        );
    }

    #[tokio::test]
    async fn permission_block_is_audited_and_alerted_without_submission() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v3/order/test"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_json(
                json!({"code": -2015, "msg": "Invalid API-key, IP, or permissions for action."}),
            ))
            .mount(&server)
            .await;
        mount_spot(&server, "1", "600", "600.1").await;
        let alerter = RecordingAlerter::default();
        let all = SpotCanaryConfirmations {
            submit_order: true,
            confirm_live_canary: true,
            confirm_replay_passed: true,
            confirm_paper_warmup_reviewed: true,
        };
        let err = run_spot_canary(
            &settings(true, &["BNB/USDT"]),
            &mock_client(&server),
            &alerter,
            all,
            "BNB",
            "USDT",
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("spot_live_canary is not allowed"),
            "{err}"
        );
        assert!(alerter.messages()[0].contains("blocked"));
        assert!(
            std::fs::read_to_string(dir.path().join("spot-canary.jsonl"))
                .unwrap()
                .contains("\"status\":\"blocked\"")
        );
        assert!(
            !request_paths(&server)
                .await
                .iter()
                .any(|p| p == "POST /api/v3/order")
        );
    }
}

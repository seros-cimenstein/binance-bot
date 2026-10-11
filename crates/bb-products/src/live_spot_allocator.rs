//! Guarded live spot allocator (port of `src/live_spot_allocator.py`).
//!
//! Plans one direct-market conversion with portfolio valuation and hard risk
//! limits. Submission requires five confirmation flags, live settings, a passing
//! `spot_live_pool` permission report and a safe order test.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bb_binance::markets::Markets;
use bb_binance::spot::NewOrder;
use bb_binance::{BinanceClient, BinanceError, OrderType, Side, SymbolRules};
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
use crate::common::{self, append_audit, fmt_dec, num, num_opt};
use crate::scope_audit::{
    ScopeAuditError, ScopeInputs, build_scope_audit_report, ensure_scope_allowed,
};
use crate::settings::Settings;
use crate::spot_common::{
    self, BalanceSnapshot, CommissionSummary, DailyUsage, RouteLeg, RouteQuote, SanitizedOrder,
    active_market, daily_usage, dec_value, fetch_balance, fetch_ticker,
};

pub const DEFAULT_AUDIT_PATH: &str = "data/live-spot-allocations.jsonl";
pub const DEFAULT_STOP_PATH: &str = "data/live-spot.stop";
const EPSILON: Decimal = dec!(0.000000001);

#[derive(Debug, thiserror::Error)]
pub enum LiveSpotAllocatorError {
    #[error("{0}")]
    Blocked(String),
    #[error(transparent)]
    Scope(#[from] ScopeAuditError),
    #[error(transparent)]
    Binance(#[from] BinanceError),
    #[error(transparent)]
    Audit(#[from] bb_core::audit::AuditError),
}

fn blocked(msg: impl Into<String>) -> LiveSpotAllocatorError {
    LiveSpotAllocatorError::Blocked(msg.into())
}

/// Hard risk limits for one live spot allocation cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSpotLimits {
    pub max_order_notional_usdt: Decimal,
    pub max_daily_notional_usdt: Decimal,
    pub max_daily_orders: u32,
    pub max_spread_bps: Decimal,
    pub slippage_buffer_bps: Decimal,
    pub max_turnover_percent: Decimal,
    pub max_asset_exposure_percent: Decimal,
    pub max_total_exposure_usdt: Decimal,
    pub daily_loss_limit_usdt: Decimal,
    pub max_drawdown_percent: Decimal,
    pub min_bnb_reserve: Decimal,
    pub max_route_legs: u32,
    pub audit_path: PathBuf,
    pub emergency_stop_path: PathBuf,
}

impl Default for LiveSpotLimits {
    fn default() -> Self {
        Self {
            max_order_notional_usdt: dec!(6.0),
            max_daily_notional_usdt: dec!(12.0),
            max_daily_orders: 2,
            max_spread_bps: dec!(50.0),
            slippage_buffer_bps: dec!(25.0),
            max_turnover_percent: dec!(100.0),
            max_asset_exposure_percent: dec!(100.0),
            max_total_exposure_usdt: dec!(0.0),
            daily_loss_limit_usdt: dec!(0.0),
            max_drawdown_percent: dec!(0.0),
            min_bnb_reserve: dec!(0.00005),
            max_route_legs: 1,
            audit_path: DEFAULT_AUDIT_PATH.into(),
            emergency_stop_path: DEFAULT_STOP_PATH.into(),
        }
    }
}

impl LiveSpotLimits {
    fn validate(&self) -> Result<(), LiveSpotAllocatorError> {
        for (value, name) in [
            (self.max_order_notional_usdt, "max_order_notional_usdt"),
            (self.max_daily_notional_usdt, "max_daily_notional_usdt"),
            (self.max_spread_bps, "max_spread_bps"),
            (self.max_turnover_percent, "max_turnover_percent"),
            (
                self.max_asset_exposure_percent,
                "max_asset_exposure_percent",
            ),
        ] {
            if value <= Decimal::ZERO {
                return Err(blocked(format!("{name} must be a positive finite value")));
            }
        }
        for (value, name) in [
            (self.slippage_buffer_bps, "slippage_buffer_bps"),
            (self.max_total_exposure_usdt, "max_total_exposure_usdt"),
            (self.daily_loss_limit_usdt, "daily_loss_limit_usdt"),
            (self.max_drawdown_percent, "max_drawdown_percent"),
            (self.min_bnb_reserve, "min_bnb_reserve"),
        ] {
            if value < Decimal::ZERO {
                return Err(blocked(format!(
                    "{name} must be a finite non-negative value"
                )));
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
        if self.max_route_legs != 1 {
            return Err(blocked("max_route_legs must be 1 for allocator v1"));
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

/// Requested live spot allocation intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveSpotIntent {
    pub source_asset: String,
    pub destination_asset: String,
    #[serde(with = "num_opt")]
    pub requested_source_amount: Option<Decimal>,
    pub reason: String,
}

/// Risk-limit snapshot recorded in the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanRisk {
    pub daily_orders_used: u32,
    #[serde(with = "num")]
    pub daily_notional_used_usdt: Decimal,
    #[serde(with = "num")]
    pub daily_realized_loss_usdt: Decimal,
    #[serde(with = "num")]
    pub max_order_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub max_daily_notional_usdt: Decimal,
    pub max_daily_orders: u32,
    #[serde(with = "num")]
    pub max_turnover_percent: Decimal,
    #[serde(with = "num")]
    pub max_asset_exposure_percent: Decimal,
    #[serde(with = "num")]
    pub max_total_exposure_usdt: Decimal,
    #[serde(with = "num")]
    pub daily_loss_limit_usdt: Decimal,
    #[serde(with = "num")]
    pub max_drawdown_percent: Decimal,
    #[serde(with = "num")]
    pub min_bnb_reserve: Decimal,
    pub max_route_legs: u32,
}

/// Auditable live spot allocation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveSpotPlan {
    pub timestamp: String,
    pub intent: LiveSpotIntent,
    pub symbol: String,
    pub side: String,
    #[serde(with = "num")]
    pub order_amount: Decimal,
    #[serde(with = "num")]
    pub source_spend_amount: Decimal,
    #[serde(with = "num")]
    pub estimated_price: Decimal,
    #[serde(with = "num")]
    pub estimated_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub expected_destination_amount: Decimal,
    #[serde(with = "num")]
    pub fee_rate: Decimal,
    #[serde(with = "num")]
    pub estimated_fee_amount: Decimal,
    #[serde(with = "num")]
    pub min_amount: Decimal,
    #[serde(with = "num")]
    pub min_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub spread_bps: Decimal,
    pub route_quote: RouteQuote,
    #[serde(with = "num")]
    pub portfolio_value_usdt_before: Decimal,
    #[serde(with = "num")]
    pub portfolio_value_usdt_after: Decimal,
    #[serde(with = "num")]
    pub destination_value_usdt_after: Decimal,
    #[serde(serialize_with = "dec_map")]
    pub benchmark_values_after: BTreeMap<String, Decimal>,
    pub risk: PlanRisk,
    pub commission: CommissionSummary,
}

fn dec_map<S: serde::Serializer>(
    map: &BTreeMap<String, Decimal>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut out = serializer.serialize_map(Some(map.len()))?;
    for (k, v) in map {
        out.serialize_entry(k, &dec_value(*v))?;
    }
    out.end()
}

/// Post-trade balance reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reconciliation {
    pub source_asset: String,
    pub destination_asset: String,
    #[serde(with = "num")]
    pub expected_source_delta: Decimal,
    #[serde(with = "num")]
    pub actual_source_delta: Decimal,
    #[serde(with = "num")]
    pub expected_destination_delta: Decimal,
    #[serde(with = "num")]
    pub actual_destination_delta: Decimal,
    #[serde(with = "num")]
    pub portfolio_value_usdt_after: Decimal,
}

/// Open order row or the error reading them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum OpenOrderRow {
    Order(Box<SanitizedOrder>),
    Error { error: String },
}

/// Plan or execution result for one guarded allocation cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveSpotResult {
    pub timestamp: String,
    pub mode: String,
    pub status: String,
    pub plan: LiveSpotPlan,
    pub submission_attempted: bool,
    pub order: Option<SanitizedOrder>,
    pub final_order: Option<SanitizedOrder>,
    pub balance_before: BalanceSnapshot,
    pub balance_after: BalanceSnapshot,
    pub open_orders_after: Vec<OpenOrderRow>,
    pub reconciliation: Option<Reconciliation>,
    pub error: String,
    pub audit_path: String,
    pub telegram_sent: bool,
}

impl LiveSpotResult {
    fn new(
        settings: &Settings,
        status: &str,
        plan: LiveSpotPlan,
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
            open_orders_after: Vec::new(),
            reconciliation: None,
            error: String::new(),
            audit_path: String::new(),
            telegram_sent: false,
        }
    }
}

/// Operator confirmations required for submission.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveSpotConfirmations {
    pub submit_order: bool,
    pub confirm_live_allocator: bool,
    pub confirm_canary_reviewed: bool,
    pub confirm_replay_passed: bool,
    pub confirm_paper_warmup_reviewed: bool,
    pub confirm_permission_diagnostics: bool,
}

/// Plan or submit one guarded live spot allocation cycle.
pub async fn run_live_spot_allocator(
    settings: &Settings,
    client: &BinanceClient,
    alerter: &dyn Alerter,
    confirmations: LiveSpotConfirmations,
    intent: &LiveSpotIntent,
    limits: &LiveSpotLimits,
) -> Result<LiveSpotResult, LiveSpotAllocatorError> {
    limits.validate()?;
    preflight(settings, confirmations)?;
    let (plan, balance_before) = build_live_spot_plan(client, intent, limits).await?;
    let canary = vec![plan.symbol.clone()];
    ensure_scope_allowed(&build_scope_audit_report(
        settings,
        &ScopeInputs {
            canary_symbols: &canary,
            ..ScopeInputs::default()
        },
    )?)?;
    let audit_path = &limits.audit_path;
    if !confirmations.submit_order {
        let mut result = LiveSpotResult::new(settings, "planned", plan, balance_before);
        result.audit_path = append_audit(audit_path, &result)?.display().to_string();
        return Ok(result);
    }

    if let Err(err) = permission_gate(settings, client, &plan).await {
        let mut result = LiveSpotResult::new(settings, "blocked", plan, balance_before);
        result.error = err.to_string();
        let audit = append_audit(audit_path, &result)?;
        send_alert(alerter, &result, &audit).await;
        return Err(blocked(err.to_string()));
    }

    let side = if plan.side == "sell" {
        Side::Sell
    } else {
        Side::Buy
    };
    let order = match client
        .create_order(&NewOrder::market(&plan.symbol, side, plan.order_amount))
        .await
    {
        Ok(order) => order,
        Err(err) => {
            let mut result =
                LiveSpotResult::new(settings, "submission_error", plan, balance_before);
            result.submission_attempted = true;
            result.error = err.to_string();
            let audit = append_audit(audit_path, &result)?;
            send_alert(alerter, &result, &audit).await;
            return Err(blocked(
                "live spot allocation order failed; no retry was attempted",
            ));
        }
    };
    let final_order = spot_common::final_order_status(client, &plan.symbol, order.clone()).await?;
    let balance_after = fetch_balance(client).await?;
    let open_orders = match client.open_orders(Some(&plan.symbol)).await {
        Ok(orders) => orders
            .iter()
            .map(|o| OpenOrderRow::Order(Box::new(SanitizedOrder::from_order(o, &plan.symbol))))
            .collect(),
        Err(err) => vec![OpenOrderRow::Error {
            error: err.to_string(),
        }],
    };
    let reconciliation = reconcile_balances(&plan, &balance_before, &balance_after);
    let symbol = plan.symbol.clone();
    let mut result = LiveSpotResult::new(settings, "submitted", plan, balance_before);
    result.submission_attempted = true;
    result.order = Some(SanitizedOrder::from_order(&order, &symbol));
    result.final_order = Some(SanitizedOrder::from_order(&final_order, &symbol));
    result.balance_after = balance_after;
    result.open_orders_after = open_orders;
    result.reconciliation = Some(reconciliation);
    let audit = append_audit(audit_path, &result)?;
    result.telegram_sent = send_alert(alerter, &result, &audit).await;
    result.audit_path = audit.display().to_string();
    Ok(result)
}

async fn permission_gate(
    settings: &Settings,
    client: &BinanceClient,
    plan: &LiveSpotPlan,
) -> Result<(), ApiPermissionError> {
    let options = InspectOptions {
        spot_test_symbol: Some(plan.symbol.clone()),
        include_spot_order_test: true,
        features: vec![Feature::SpotLivePool],
    };
    let report = inspect_api_permissions(settings, client, &options).await?;
    ensure_feature_allowed(&report, Feature::SpotLivePool)?;
    ensure_spot_order_test_allowed(
        client,
        &SpotOrderTest {
            symbol: plan.symbol.clone(),
            side: if plan.side == "sell" {
                Side::Sell
            } else {
                Side::Buy
            },
            order_type: OrderType::Market,
            quantity: plan.order_amount,
            price: None,
        },
    )
    .await
}

/// Build a guarded direct-market allocation plan from live data.
pub async fn build_live_spot_plan(
    client: &BinanceClient,
    intent: &LiveSpotIntent,
    limits: &LiveSpotLimits,
) -> Result<(LiveSpotPlan, BalanceSnapshot), LiveSpotAllocatorError> {
    limits.validate()?;
    if limits.emergency_stop_path.exists() {
        return Err(blocked(format!(
            "live spot emergency stop is active: {}",
            limits.emergency_stop_path.display()
        )));
    }
    let source = intent.source_asset.trim().to_uppercase();
    let destination = intent.destination_asset.trim().to_uppercase();
    if source.is_empty() || destination.is_empty() {
        return Err(blocked("source and destination assets are required"));
    }
    if source == destination {
        return Err(blocked("source and destination assets must differ"));
    }
    let usage = daily_usage(&limits.audit_path, &spot_common::today());
    if usage.orders >= limits.max_daily_orders {
        return Err(blocked("live spot daily order limit is exhausted"));
    }
    if limits.daily_loss_limit_usdt > Decimal::ZERO
        && usage.realized_loss_usdt >= limits.daily_loss_limit_usdt
    {
        return Err(blocked("live spot daily loss limit is exhausted"));
    }
    let remaining_daily = limits.max_daily_notional_usdt - usage.notional;
    if remaining_daily <= Decimal::ZERO {
        return Err(blocked("live spot daily notional limit is exhausted"));
    }

    let balance = fetch_balance(client).await?;
    let free_source = balance
        .free(&source)
        .ok_or_else(|| blocked(format!("no free {source} balance is available")))?;
    let markets = client.spot_markets(false).await?;
    let (rules, symbol, sell) = direct_market(&markets, &source, &destination)?;
    let side = if sell { "sell" } else { "buy" };
    let ticker = fetch_ticker(client, &symbol).await?;
    let price = if sell { ticker.bid } else { ticker.ask }
        .or(ticker.last)
        .ok_or_else(|| blocked(format!("{symbol} ticker did not include a price")))?;
    let spread = ticker
        .spread_bps()
        .ok_or_else(|| blocked(format!("{symbol} ticker did not include bid/ask spread")))?;
    if spread > limits.max_spread_bps {
        return Err(blocked("live spot spread exceeds configured limit"));
    }

    let min_amount = spot_common::min_amount(&rules);
    let min_notional = spot_common::min_notional(&rules);
    let mut spend_cap = limits.max_order_notional_usdt.min(remaining_daily);
    if let Some(requested) = intent.requested_source_amount {
        if requested <= Decimal::ZERO {
            return Err(blocked("requested_source_amount must be positive"));
        }
        spend_cap = spend_cap.min(if sell { requested * price } else { requested });
    }
    let (order_amount, source_spend, notional) = if sell {
        let amount = spot_common::round_down(&rules, free_source.min(spend_cap / price));
        (amount, amount, amount * price)
    } else {
        let amount = spot_common::round_down(&rules, free_source.min(spend_cap) / price);
        (amount, amount * price, amount * price)
    };
    if order_amount <= Decimal::ZERO {
        return Err(blocked("live spot order amount rounded to zero"));
    }
    if order_amount < min_amount || notional < min_notional {
        return Err(blocked(
            "live spot order is below Binance minimum amount or notional",
        ));
    }
    if source == "BNB" && free_source - source_spend < limits.min_bnb_reserve {
        return Err(blocked("BNB reserve would be depleted"));
    }

    let commission = fetch_commission(client, &symbol).await?;
    let fee_rate = commission.taker_rate(side, true);
    let gross = if sell { notional } else { order_amount };
    let fee = gross * fee_rate;
    let expected = gross - fee;
    let valuation = portfolio_valuations(
        client,
        &markets,
        &balance,
        &source,
        &destination,
        -source_spend,
        expected,
    )
    .await?;
    risk_check(limits, &usage, notional, &valuation)?;
    let discount_asset = if commission.discount_enabled {
        commission.discount_asset.clone()
    } else {
        String::new()
    };
    let route_quote = RouteQuote {
        source_asset: source.clone(),
        destination_asset: destination.clone(),
        input_amount: source_spend,
        output_amount: expected,
        slippage_bps: spread + limits.slippage_buffer_bps,
        legs: vec![RouteLeg {
            symbol: symbol.clone(),
            side: side.into(),
            source_asset: source.clone(),
            destination_asset: destination.clone(),
            input_amount: source_spend,
            spent_amount: source_spend,
            gross_output_amount: gross,
            fee_rate,
            fee_output_amount: fee,
            net_output_amount: expected,
            commission_discount_asset: discount_asset,
        }],
    };
    let risk = PlanRisk {
        daily_orders_used: usage.orders,
        daily_notional_used_usdt: usage.notional,
        daily_realized_loss_usdt: usage.realized_loss_usdt,
        max_order_notional_usdt: limits.max_order_notional_usdt,
        max_daily_notional_usdt: limits.max_daily_notional_usdt,
        max_daily_orders: limits.max_daily_orders,
        max_turnover_percent: limits.max_turnover_percent,
        max_asset_exposure_percent: limits.max_asset_exposure_percent,
        max_total_exposure_usdt: limits.max_total_exposure_usdt,
        daily_loss_limit_usdt: limits.daily_loss_limit_usdt,
        max_drawdown_percent: limits.max_drawdown_percent,
        min_bnb_reserve: limits.min_bnb_reserve,
        max_route_legs: limits.max_route_legs,
    };
    let plan = LiveSpotPlan {
        timestamp: common::utc_timestamp(),
        intent: intent.clone(),
        symbol,
        side: side.into(),
        order_amount,
        source_spend_amount: source_spend,
        estimated_price: price,
        estimated_notional_usdt: notional,
        expected_destination_amount: expected,
        fee_rate,
        estimated_fee_amount: fee,
        min_amount,
        min_notional_usdt: min_notional,
        spread_bps: spread,
        route_quote,
        portfolio_value_usdt_before: valuation.total_before,
        portfolio_value_usdt_after: valuation.total_after,
        destination_value_usdt_after: valuation.destination_value_after,
        benchmark_values_after: valuation.benchmarks_after,
        risk,
        commission: CommissionSummary::new(&commission, side),
    };
    Ok((plan, balance))
}

/// Submission gate (no-op for plan-only runs).
pub fn preflight(
    settings: &Settings,
    c: LiveSpotConfirmations,
) -> Result<(), LiveSpotAllocatorError> {
    if !c.submit_order {
        return Ok(());
    }
    for (ok, flag) in [
        (c.confirm_live_allocator, "--confirm-live-allocator"),
        (c.confirm_canary_reviewed, "--confirm-canary-reviewed"),
        (c.confirm_replay_passed, "--confirm-replay-passed"),
        (
            c.confirm_paper_warmup_reviewed,
            "--confirm-paper-warmup-reviewed",
        ),
        (
            c.confirm_permission_diagnostics,
            "--confirm-permission-diagnostics",
        ),
    ] {
        if !ok {
            return Err(blocked(format!("live spot allocator requires {flag}")));
        }
    }
    if settings.use_testnet {
        return Err(blocked("live spot allocator requires USE_TESTNET=false"));
    }
    if settings.dry_run {
        return Err(blocked("live spot allocator requires DRY_RUN=false"));
    }
    if settings.confirm_live_trading != "yes" {
        return Err(blocked(
            "live spot allocator requires CONFIRM_LIVE_TRADING=yes",
        ));
    }
    Ok(())
}

/// `SOURCE/DEST` sells, else `DEST/SOURCE` buys.
fn direct_market(
    markets: &Markets,
    source: &str,
    destination: &str,
) -> Result<(SymbolRules, String, bool), LiveSpotAllocatorError> {
    let sell_symbol = format!("{source}/{destination}");
    if let Some(rules) = active_market(markets, &sell_symbol) {
        return Ok((rules, sell_symbol, true));
    }
    let buy_symbol = format!("{destination}/{source}");
    if let Some(rules) = active_market(markets, &buy_symbol) {
        return Ok((rules, buy_symbol, false));
    }
    Err(blocked(format!(
        "no active direct spot market for {source}->{destination}"
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Valuation {
    total_before: Decimal,
    total_after: Decimal,
    destination_value_after: Decimal,
    benchmarks_after: BTreeMap<String, Decimal>,
}

async fn portfolio_valuations(
    client: &BinanceClient,
    markets: &Markets,
    balance: &BalanceSnapshot,
    source: &str,
    destination: &str,
    source_delta: Decimal,
    destination_delta: Decimal,
) -> Result<Valuation, BinanceError> {
    let balances = balance.total.clone();
    let total_before = value_balances(client, markets, &balances).await?;
    let mut after = balances;
    let source_after =
        (after.get(source).copied().unwrap_or_default() + source_delta).max(Decimal::ZERO);
    after.insert(source.to_owned(), source_after);
    let destination_after = after.get(destination).copied().unwrap_or_default() + destination_delta;
    after.insert(destination.to_owned(), destination_after);
    let total_after = value_balances(client, markets, &after).await?;
    let destination_value_after =
        asset_value_usdt(client, markets, destination, destination_after).await?;
    let mut benchmarks = BTreeMap::from([("USDT".to_owned(), total_after)]);
    for asset in ["BTC", "PAXG", "XAUT"] {
        let symbol = format!("{asset}/USDT");
        if active_market(markets, &symbol).is_none() {
            continue;
        }
        let ticker = fetch_ticker(client, &symbol).await?;
        if let Some(price) = ticker.buy_price() {
            benchmarks.insert(asset.to_owned(), total_after / price);
        }
    }
    Ok(Valuation {
        total_before,
        total_after,
        destination_value_after,
        benchmarks_after: benchmarks,
    })
}

async fn value_balances(
    client: &BinanceClient,
    markets: &Markets,
    balances: &BTreeMap<String, Decimal>,
) -> Result<Decimal, BinanceError> {
    let mut total = Decimal::ZERO;
    for (asset, amount) in balances {
        total += asset_value_usdt(client, markets, asset, *amount).await?;
    }
    Ok(total)
}

async fn asset_value_usdt(
    client: &BinanceClient,
    markets: &Markets,
    asset: &str,
    amount: Decimal,
) -> Result<Decimal, BinanceError> {
    if amount <= Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }
    if asset == "USDT" {
        return Ok(amount);
    }
    let sell_symbol = format!("{asset}/USDT");
    if active_market(markets, &sell_symbol).is_some() {
        let ticker = fetch_ticker(client, &sell_symbol).await?;
        return Ok(ticker.sell_price().map_or(Decimal::ZERO, |p| amount * p));
    }
    let buy_symbol = format!("USDT/{asset}");
    if active_market(markets, &buy_symbol).is_some() {
        let ticker = fetch_ticker(client, &buy_symbol).await?;
        return Ok(ticker.buy_price().map_or(Decimal::ZERO, |p| amount / p));
    }
    Ok(Decimal::ZERO)
}

fn risk_check(
    limits: &LiveSpotLimits,
    usage: &DailyUsage,
    notional: Decimal,
    v: &Valuation,
) -> Result<(), LiveSpotAllocatorError> {
    if notional > limits.max_order_notional_usdt + EPSILON {
        return Err(blocked("order notional exceeds max order limit"));
    }
    if usage.notional + notional > limits.max_daily_notional_usdt + EPSILON {
        return Err(blocked("order notional exceeds daily notional limit"));
    }
    if v.total_before > Decimal::ZERO
        && notional / v.total_before * dec!(100) > limits.max_turnover_percent
    {
        return Err(blocked("turnover exceeds configured limit"));
    }
    if v.total_after > Decimal::ZERO
        && v.destination_value_after / v.total_after * dec!(100) > limits.max_asset_exposure_percent
    {
        return Err(blocked("destination exposure exceeds limit"));
    }
    if limits.max_total_exposure_usdt > Decimal::ZERO
        && v.total_after > limits.max_total_exposure_usdt
    {
        return Err(blocked("total exposure exceeds configured limit"));
    }
    if limits.daily_loss_limit_usdt > Decimal::ZERO
        && usage.realized_loss_usdt >= limits.daily_loss_limit_usdt
    {
        return Err(blocked("daily realized loss limit is active"));
    }
    let high = usage.equity_high_usdt;
    if high > Decimal::ZERO && limits.max_drawdown_percent > Decimal::ZERO {
        let drawdown = ((high - v.total_before) / high).max(Decimal::ZERO);
        if drawdown * dec!(100) > limits.max_drawdown_percent {
            return Err(blocked("drawdown limit is active"));
        }
    }
    Ok(())
}

fn reconcile_balances(
    plan: &LiveSpotPlan,
    before: &BalanceSnapshot,
    after: &BalanceSnapshot,
) -> Reconciliation {
    let source = plan.intent.source_asset.trim().to_uppercase();
    let destination = plan.intent.destination_asset.trim().to_uppercase();
    Reconciliation {
        actual_source_delta: after.total(&source) - before.total(&source),
        actual_destination_delta: after.total(&destination) - before.total(&destination),
        source_asset: source,
        destination_asset: destination,
        expected_source_delta: -plan.source_spend_amount,
        expected_destination_delta: plan.expected_destination_amount,
        portfolio_value_usdt_after: plan.portfolio_value_usdt_after,
    }
}

async fn send_alert(alerter: &dyn Alerter, result: &LiveSpotResult, audit: &Path) -> bool {
    alerter
        .send(&format!(
            "<b>LIVE SPOT ALLOCATOR</b>\nStatus: <code>{}</code>\nSide: <code>{}</code>\nSymbol: <code>{}</code>\nAmount: <code>{}</code>\nAudit: <code>{}</code>",
            escape(&result.status),
            result.plan.side.to_uppercase(),
            escape(&result.plan.symbol),
            fmt_dec(result.plan.order_amount),
            escape(&audit.display().to_string()),
        ))
        .await
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use super::*;
    use crate::alerts::testing::RecordingAlerter;
    use crate::spot_canary::tests::mount_spot;
    use crate::testutil::{mock_client, request_paths, settings_builder};

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

    fn intent(source: &str, destination: &str) -> LiveSpotIntent {
        LiveSpotIntent {
            source_asset: source.into(),
            destination_asset: destination.into(),
            requested_source_amount: None,
            reason: "manual foreground live spot allocation".into(),
        }
    }

    fn limits(dir: &Path) -> LiveSpotLimits {
        LiveSpotLimits {
            audit_path: dir.join("alloc.jsonl"),
            emergency_stop_path: dir.join("live-spot.stop"),
            ..LiveSpotLimits::default()
        }
    }

    #[tokio::test]
    async fn plan_only_values_portfolio_and_audits() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_spot(&server, "1", "600", "600.1").await;
        let alerter = RecordingAlerter::default();
        let result = run_live_spot_allocator(
            &settings(false, &["BNB/USDT"]),
            &mock_client(&server),
            &alerter,
            LiveSpotConfirmations::default(),
            &intent("BNB", "USDT"),
            &limits(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(result.status, "planned");
        let plan = &result.plan;
        assert_eq!(plan.side, "sell");
        assert_eq!(plan.order_amount, dec!(0.01));
        // 1 BNB * 600 + 10 USDT + 0.0001 BTC * 50000 = 615
        assert_eq!(plan.portfolio_value_usdt_before, dec!(615));
        assert_eq!(plan.route_quote.slippage_bps, plan.spread_bps + dec!(25));
        assert!(plan.benchmark_values_after.contains_key("BTC"));
        assert!(
            !request_paths(&server)
                .await
                .iter()
                .any(|p| p.contains("/api/v3/order"))
        );
        let row: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(dir.path().join("alloc.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(row["plan"]["intent"]["source_asset"], "BNB");
        assert_eq!(row["plan"]["risk"]["max_daily_orders"], 2);
    }

    #[tokio::test]
    async fn buy_side_reserve_and_risk_guards() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_spot(&server, "0.01", "600", "600.1").await;
        let client = mock_client(&server);
        let alerter = RecordingAlerter::default();
        let result = run_live_spot_allocator(
            &settings(false, &["BNB/USDT"]),
            &client,
            &alerter,
            LiveSpotConfirmations::default(),
            &intent("USDT", "BNB"),
            &limits(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(result.plan.side, "buy");
        assert_eq!(result.plan.symbol, "BNB/USDT");
        assert_eq!(result.plan.order_amount, dec!(0.009));

        let err = run_live_spot_allocator(
            &settings(false, &["BNB/USDT"]),
            &client,
            &alerter,
            LiveSpotConfirmations::default(),
            &intent("BNB", "USDT"),
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "BNB reserve would be depleted");
        let tight = LiveSpotLimits {
            max_turnover_percent: dec!(0.5),
            ..limits(dir.path())
        };
        let err = run_live_spot_allocator(
            &settings(false, &["BNB/USDT"]),
            &client,
            &alerter,
            LiveSpotConfirmations::default(),
            &intent("USDT", "BNB"),
            &tight,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "turnover exceeds configured limit");
        let err = run_live_spot_allocator(
            &settings(false, &[]),
            &client,
            &alerter,
            LiveSpotConfirmations::default(),
            &intent("USDT", "BNB"),
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("API scope audit failed"));
        let partial = LiveSpotConfirmations {
            submit_order: true,
            confirm_live_allocator: true,
            ..LiveSpotConfirmations::default()
        };
        let err = run_live_spot_allocator(
            &settings(true, &["BNB/USDT"]),
            &client,
            &alerter,
            partial,
            &intent("USDT", "BNB"),
            &limits(dir.path()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "live spot allocator requires --confirm-canary-reviewed"
        );
        assert!(
            !request_paths(&server)
                .await
                .iter()
                .any(|p| p == "POST /api/v3/order")
        );
        let audit_rows = std::fs::read_to_string(dir.path().join("alloc.jsonl")).unwrap();
        assert_eq!(audit_rows.lines().count(), 1);
    }
}

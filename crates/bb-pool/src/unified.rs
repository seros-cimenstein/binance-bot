//! Unified dry-run asset-pool valuation across Binance account types.
//!
//! Valuation and constraint layer only: it never submits spot orders, margin actions,
//! futures actions, or internal transfers. Inputs are plain holdings plus USDT prices, so
//! account-discovery code (spot/funding/margin/futures) only needs to produce
//! [`UnifiedHolding`] rows.

use std::collections::BTreeMap;
use std::path::Path;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{PoolError, Result};
use crate::pyfmt::{num, py_isoformat};

pub const ACCOUNT_SPOT: &str = "spot";
pub const ACCOUNT_FUNDING: &str = "funding";
pub const ACCOUNT_MARGIN: &str = "margin";
pub const ACCOUNT_FUTURES: &str = "futures";

pub const WALLET_SPOT: &str = "spot";
pub const WALLET_FUNDING: &str = "funding";
pub const WALLET_CROSS_MARGIN: &str = "cross_margin";
pub const WALLET_ISOLATED_MARGIN: &str = "isolated_margin";
pub const WALLET_USD_M_FUTURES: &str = "usd_m_futures";

/// `(source wallet, destination wallet, universal transfer type)`; mirrors `src/transfers.py`.
pub const TRANSFER_ROUTES: [(&str, &str, &str); 15] = [
    (WALLET_SPOT, WALLET_USD_M_FUTURES, "MAIN_UMFUTURE"),
    (WALLET_USD_M_FUTURES, WALLET_SPOT, "UMFUTURE_MAIN"),
    (WALLET_SPOT, WALLET_CROSS_MARGIN, "MAIN_MARGIN"),
    (WALLET_CROSS_MARGIN, WALLET_SPOT, "MARGIN_MAIN"),
    (WALLET_CROSS_MARGIN, WALLET_USD_M_FUTURES, "MARGIN_UMFUTURE"),
    (WALLET_USD_M_FUTURES, WALLET_CROSS_MARGIN, "UMFUTURE_MARGIN"),
    (WALLET_SPOT, WALLET_FUNDING, "MAIN_FUNDING"),
    (WALLET_FUNDING, WALLET_SPOT, "FUNDING_MAIN"),
    (WALLET_FUNDING, WALLET_USD_M_FUTURES, "FUNDING_UMFUTURE"),
    (WALLET_USD_M_FUTURES, WALLET_FUNDING, "UMFUTURE_FUNDING"),
    (WALLET_CROSS_MARGIN, WALLET_FUNDING, "MARGIN_FUNDING"),
    (WALLET_FUNDING, WALLET_CROSS_MARGIN, "FUNDING_MARGIN"),
    (
        WALLET_ISOLATED_MARGIN,
        WALLET_CROSS_MARGIN,
        "ISOLATEDMARGIN_MARGIN",
    ),
    (
        WALLET_CROSS_MARGIN,
        WALLET_ISOLATED_MARGIN,
        "MARGIN_ISOLATEDMARGIN",
    ),
    (
        WALLET_ISOLATED_MARGIN,
        WALLET_ISOLATED_MARGIN,
        "ISOLATEDMARGIN_ISOLATEDMARGIN",
    ),
];

fn unified_error(message: impl Into<String>) -> PoolError {
    PoolError::Unified(message.into())
}

/// One asset exposure inside one account type (JSON field names match Python).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnifiedHolding {
    pub account_type: String,
    pub wallet: String,
    pub asset: String,
    #[serde(serialize_with = "num::decimal")]
    pub total: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub free: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub locked: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub borrowed: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub interest: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub unrealized_pnl: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub available: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub initial_margin: Decimal,
    #[serde(default, serialize_with = "num::decimal")]
    pub maintenance_margin: Decimal,
    #[serde(default, serialize_with = "num::opt_decimal")]
    pub leverage: Option<Decimal>,
    #[serde(default)]
    pub margin_type: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub transfer_enabled: Option<bool>,
    #[serde(default, serialize_with = "num::opt_decimal")]
    pub liquidity_score: Option<Decimal>,
    #[serde(default, serialize_with = "num::opt_decimal")]
    pub volatility_percent: Option<Decimal>,
    #[serde(default)]
    pub source: String,
}

impl UnifiedHolding {
    /// Holding with only identity and total set.
    pub fn new(account_type: &str, wallet: &str, asset: &str, total: Decimal) -> Self {
        Self {
            account_type: account_type.into(),
            wallet: wallet.into(),
            asset: asset.into(),
            total,
            free: Decimal::ZERO,
            locked: Decimal::ZERO,
            borrowed: Decimal::ZERO,
            interest: Decimal::ZERO,
            unrealized_pnl: Decimal::ZERO,
            available: Decimal::ZERO,
            initial_margin: Decimal::ZERO,
            maintenance_margin: Decimal::ZERO,
            leverage: None,
            margin_type: String::new(),
            symbol: String::new(),
            transfer_enabled: None,
            liquidity_score: None,
            volatility_percent: None,
            source: String::new(),
        }
    }
}

/// USDT valuation for one unified holding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoldingValuation {
    pub account_type: String,
    pub wallet: String,
    pub asset: String,
    pub symbol: String,
    #[serde(serialize_with = "num::decimal")]
    pub total: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub free: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub locked: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub available: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub price_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub gross_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub debt_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub interest_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub open_pnl_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub initial_margin_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub maintenance_margin_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub net_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub transferable_usdt: Decimal,
    #[serde(serialize_with = "num::opt_decimal")]
    pub leverage: Option<Decimal>,
    pub margin_type: String,
    pub transfer_routes: Vec<String>,
    #[serde(serialize_with = "num::opt_decimal")]
    pub liquidity_score: Option<Decimal>,
    #[serde(serialize_with = "num::opt_decimal")]
    pub volatility_percent: Option<Decimal>,
}

/// Allocation and risk limits for unified dry-run valuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnifiedPoolLimits {
    pub max_asset_allocation_percent: Decimal,
    pub max_account_type_allocation_percent: Decimal,
    pub max_leverage: Decimal,
    pub max_debt_percent: Decimal,
    pub min_transferable_usdt: Decimal,
    pub min_liquidity_score: Decimal,
    pub max_volatility_percent: Decimal,
    /// `asset -> minimum free amount` (default `BNB: 0.00005`).
    pub asset_reserves: BTreeMap<String, Decimal>,
}

impl Default for UnifiedPoolLimits {
    fn default() -> Self {
        Self {
            max_asset_allocation_percent: Decimal::from(60),
            max_account_type_allocation_percent: Decimal::from(80),
            max_leverage: Decimal::from(3),
            max_debt_percent: Decimal::from(25),
            min_transferable_usdt: Decimal::ZERO,
            min_liquidity_score: Decimal::ZERO,
            max_volatility_percent: Decimal::ONE_HUNDRED,
            asset_reserves: BTreeMap::from([("BNB".to_owned(), Decimal::new(5, 5))]),
        }
    }
}

/// Constraint outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstraintStatus {
    Ok,
    Blocked,
}

/// One unified-pool allocation constraint result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllocationConstraint {
    pub name: String,
    pub status: ConstraintStatus,
    pub detail: String,
    pub account_type: String,
    pub wallet: String,
    pub asset: String,
    #[serde(serialize_with = "num::opt_decimal")]
    pub value: Option<Decimal>,
    #[serde(serialize_with = "num::opt_decimal")]
    pub limit: Option<Decimal>,
}

impl AllocationConstraint {
    fn new(name: &str, ok: bool, detail: String, value: Decimal, limit: Decimal) -> Self {
        Self {
            name: name.into(),
            status: if ok {
                ConstraintStatus::Ok
            } else {
                ConstraintStatus::Blocked
            },
            detail,
            account_type: String::new(),
            wallet: String::new(),
            asset: String::new(),
            value: Some(value),
            limit: Some(limit),
        }
    }

    fn on(mut self, holding: &HoldingValuation) -> Self {
        self.account_type.clone_from(&holding.account_type);
        self.wallet.clone_from(&holding.wallet);
        self.asset.clone_from(&holding.asset);
        self
    }
}

/// Transferable value of one wallet/asset pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferAvailability {
    pub wallet: String,
    pub asset: String,
    #[serde(serialize_with = "num::decimal")]
    pub transferable_usdt: Decimal,
    pub routes: Vec<String>,
}

/// Unified pool valuation and dry-run constraint report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnifiedPoolSnapshot {
    pub timestamp: String,
    pub holdings: Vec<HoldingValuation>,
    #[serde(serialize_with = "num::decimal")]
    pub total_gross_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub total_net_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub total_debt_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub total_interest_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub total_open_pnl_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub total_initial_margin_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub total_maintenance_margin_usdt: Decimal,
    #[serde(serialize_with = "num::decimal")]
    pub total_transferable_usdt: Decimal,
    #[serde(serialize_with = "num::pairs")]
    pub asset_values_usdt: Vec<(String, Decimal)>,
    #[serde(serialize_with = "num::pairs")]
    pub account_type_values_usdt: Vec<(String, Decimal)>,
    #[serde(serialize_with = "num::pairs")]
    pub wallet_values_usdt: Vec<(String, Decimal)>,
    #[serde(serialize_with = "num::pairs")]
    pub benchmark_values: Vec<(String, Decimal)>,
    #[serde(serialize_with = "num::pairs")]
    pub basket_components: Vec<(String, Decimal)>,
    pub transfer_availability: BTreeMap<String, TransferAvailability>,
    pub constraints: Vec<AllocationConstraint>,
    pub allowed: bool,
    pub live_trading_enabled: bool,
    pub live_transfer_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

impl UnifiedPoolSnapshot {
    /// Lookup helper over the ordered value maps.
    pub fn value_of(values: &[(String, Decimal)], key: &str) -> Option<Decimal> {
        values.iter().find(|(name, _)| name == key).map(|(_, v)| *v)
    }
}

/// Value all supplied holdings in USDT and run dry constraints.
pub fn build_unified_pool_snapshot(
    holdings: &[UnifiedHolding],
    prices_usdt: &BTreeMap<String, Decimal>,
    benchmarks: &[String],
    basket_weights: Option<&BTreeMap<String, Decimal>>,
    limits: &UnifiedPoolLimits,
) -> Result<UnifiedPoolSnapshot> {
    validate_limits(limits)?;
    let mut prices = BTreeMap::new();
    for (asset, price) in prices_usdt {
        prices.insert(
            normalize_asset(asset)?,
            positive(*price, &format!("price for {asset}"))?,
        );
    }
    prices.insert("USDT".to_owned(), Decimal::ONE);
    let benchmark_assets = benchmarks
        .iter()
        .map(|b| normalize_asset(b))
        .collect::<Result<Vec<_>>>()?;
    let weights = basket_weights_for(&benchmark_assets, basket_weights)?;
    let valuations = holdings
        .iter()
        .filter(|h| {
            [
                h.total,
                h.free,
                h.available,
                h.borrowed,
                h.interest,
                h.unrealized_pnl,
            ]
            .iter()
            .any(|v| !v.is_zero())
        })
        .map(|h| value_holding(h, &prices))
        .collect::<Result<Vec<_>>>()?;
    let sum = |f: fn(&HoldingValuation) -> Decimal| valuations.iter().map(f).sum::<Decimal>();
    let total_gross = sum(|h| h.gross_usdt);
    let total_debt = sum(|h| h.debt_usdt);
    let total_interest = sum(|h| h.interest_usdt);
    let total_pnl = sum(|h| h.open_pnl_usdt);
    let total_initial_margin = sum(|h| h.initial_margin_usdt);
    let total_maintenance_margin = sum(|h| h.maintenance_margin_usdt);
    let total_transferable = sum(|h| h.transferable_usdt);
    let total_net = sum(|h| h.net_usdt);
    let asset_values = sum_by(&valuations, |h| &h.asset);
    let account_values = sum_by(&valuations, |h| &h.account_type);
    let wallet_values = sum_by(&valuations, |h| &h.wallet);
    let benchmark_values = benchmark_assets
        .iter()
        .map(|b| Ok((b.clone(), total_net / price(b, &prices)?)))
        .collect::<Result<Vec<_>>>()?;
    let basket_components = benchmark_values
        .iter()
        .zip(&weights)
        .map(|((b, value), (_, weight))| (b.clone(), *value * *weight))
        .collect();
    let transfer_availability = valuations
        .iter()
        .map(|h| {
            (
                format!("{}:{}", h.wallet, h.asset),
                TransferAvailability {
                    wallet: h.wallet.clone(),
                    asset: h.asset.clone(),
                    transferable_usdt: h.transferable_usdt,
                    routes: h.transfer_routes.clone(),
                },
            )
        })
        .collect();
    let constraints = constraints(
        &valuations,
        limits,
        total_net,
        total_debt + total_interest,
        &asset_values,
        &account_values,
    )?;
    let allowed = constraints.iter().all(|c| c.status == ConstraintStatus::Ok);
    Ok(UnifiedPoolSnapshot {
        timestamp: py_isoformat(bb_core::time::now()),
        holdings: valuations,
        total_gross_usdt: total_gross,
        total_net_usdt: total_net,
        total_debt_usdt: total_debt,
        total_interest_usdt: total_interest,
        total_open_pnl_usdt: total_pnl,
        total_initial_margin_usdt: total_initial_margin,
        total_maintenance_margin_usdt: total_maintenance_margin,
        total_transferable_usdt: total_transferable,
        asset_values_usdt: asset_values,
        account_type_values_usdt: account_values,
        wallet_values_usdt: wallet_values,
        benchmark_values,
        basket_components,
        transfer_availability,
        constraints,
        allowed,
        live_trading_enabled: false,
        live_transfer_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

fn fixed(value: Decimal, places: u32) -> String {
    let width = places as usize;
    format!("{:.width$}", value.round_dp(places))
}

/// Concise human-readable unified pool report.
pub fn render_unified_snapshot(snapshot: &UnifiedPoolSnapshot) -> String {
    let mut lines = vec![
        "Unified asset pool: dry-run".to_owned(),
        format!("Total net USDT: {}", fixed(snapshot.total_net_usdt, 8)),
        format!("Debt USDT: {}", fixed(snapshot.total_debt_usdt, 8)),
        format!("Interest USDT: {}", fixed(snapshot.total_interest_usdt, 8)),
        format!("Open PnL USDT: {}", fixed(snapshot.total_open_pnl_usdt, 8)),
        format!(
            "Transferable USDT: {}",
            fixed(snapshot.total_transferable_usdt, 8)
        ),
        format!("Allowed: {}", if snapshot.allowed { "yes" } else { "no" }),
        "Benchmarks:".to_owned(),
    ];
    for (benchmark, value) in &snapshot.benchmark_values {
        lines.push(format!("  - {benchmark}: {}", fixed(*value, 8)));
    }
    let failed: Vec<&AllocationConstraint> = snapshot
        .constraints
        .iter()
        .filter(|c| c.status != ConstraintStatus::Ok)
        .collect();
    if !failed.is_empty() {
        lines.push("Constraints:".into());
        for item in failed {
            lines.push(format!("  - {}: {}", item.name, item.detail));
        }
    }
    lines.join("\n")
}

/// Holdings from a JSON list of [`UnifiedHolding`] objects (non-objects are skipped).
pub fn load_holdings(path: &Path) -> Result<Vec<UnifiedHolding>> {
    let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let serde_json::Value::Array(items) = raw else {
        return Err(unified_error("holdings JSON must be a list"));
    };
    items
        .into_iter()
        .filter(serde_json::Value::is_object)
        .map(|item| serde_json::from_value(item).map_err(PoolError::from))
        .collect()
}

/// `asset -> USDT price` from a JSON object.
pub fn load_prices(path: &Path) -> Result<BTreeMap<String, Decimal>> {
    let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let serde_json::Value::Object(map) = raw else {
        return Err(unified_error("prices JSON must be an object"));
    };
    map.into_iter()
        .map(|(asset, value)| {
            let label = format!("price for {asset}");
            let price: Decimal = serde_json::from_value(value)
                .map_err(|_| unified_error(format!("{label} must be a number")))?;
            Ok((normalize_asset(&asset)?, positive(price, &label)?))
        })
        .collect()
}

/// Account type for a transfer wallet name.
pub fn account_type_from_wallet(wallet: &str) -> Result<&'static str> {
    match wallet {
        WALLET_SPOT => Ok(ACCOUNT_SPOT),
        WALLET_FUNDING => Ok(ACCOUNT_FUNDING),
        WALLET_CROSS_MARGIN | WALLET_ISOLATED_MARGIN => Ok(ACCOUNT_MARGIN),
        WALLET_USD_M_FUTURES => Ok(ACCOUNT_FUTURES),
        other => Err(unified_error(format!("unsupported wallet: {other}"))),
    }
}

fn value_holding(
    holding: &UnifiedHolding,
    prices: &BTreeMap<String, Decimal>,
) -> Result<HoldingValuation> {
    let asset = normalize_asset(&holding.asset)?;
    let price = price(&asset, prices)?;
    let gross = holding.total * price;
    let debt = holding.borrowed * price;
    let interest = holding.interest * price;
    let pnl = holding.unrealized_pnl * price;
    let mut transferable = if holding.available > Decimal::ZERO {
        holding.available
    } else {
        holding.free
    };
    if holding.transfer_enabled == Some(false) {
        transferable = Decimal::ZERO;
    }
    let routes = if transferable > Decimal::ZERO {
        outbound_transfer_routes(&holding.wallet)
    } else {
        Vec::new()
    };
    Ok(HoldingValuation {
        account_type: normalize_account_type(&holding.account_type)?,
        wallet: holding.wallet.clone(),
        asset,
        symbol: holding.symbol.clone(),
        total: holding.total,
        free: holding.free,
        locked: holding.locked,
        available: holding.available,
        price_usdt: price,
        gross_usdt: gross,
        debt_usdt: debt,
        interest_usdt: interest,
        open_pnl_usdt: pnl,
        initial_margin_usdt: holding.initial_margin * price,
        maintenance_margin_usdt: holding.maintenance_margin * price,
        net_usdt: gross - debt - interest + pnl,
        transferable_usdt: (transferable * price).max(Decimal::ZERO),
        leverage: holding.leverage,
        margin_type: holding.margin_type.clone(),
        transfer_routes: routes,
        liquidity_score: holding.liquidity_score,
        volatility_percent: holding.volatility_percent,
    })
}

fn constraints(
    holdings: &[HoldingValuation],
    limits: &UnifiedPoolLimits,
    total_net: Decimal,
    total_debt: Decimal,
    asset_values: &[(String, Decimal)],
    account_values: &[(String, Decimal)],
) -> Result<Vec<AllocationConstraint>> {
    let mut out = Vec::new();
    let denominator = if total_net > Decimal::ZERO {
        total_net
    } else {
        Decimal::ONE
    };
    let percent = |value: Decimal| value / denominator * Decimal::ONE_HUNDRED;
    for (asset, value) in asset_values {
        let pct = percent(*value);
        let limit = limits.max_asset_allocation_percent;
        let mut c = AllocationConstraint::new(
            "asset_allocation",
            pct <= limit,
            format!(
                "{asset} allocation {}% vs limit {}%",
                fixed(pct, 4),
                fixed(limit, 4)
            ),
            pct,
            limit,
        );
        c.asset.clone_from(asset);
        out.push(c);
    }
    for (account_type, value) in account_values {
        let pct = percent(*value);
        let limit = limits.max_account_type_allocation_percent;
        let mut c = AllocationConstraint::new(
            "account_type_allocation",
            pct <= limit,
            format!(
                "{account_type} allocation {}% vs limit {}%",
                fixed(pct, 4),
                fixed(limit, 4)
            ),
            pct,
            limit,
        );
        c.account_type.clone_from(account_type);
        out.push(c);
    }
    let debt_pct = percent(total_debt);
    out.push(AllocationConstraint::new(
        "debt_ratio",
        debt_pct <= limits.max_debt_percent,
        format!(
            "debt ratio {}% vs limit {}%",
            fixed(debt_pct, 4),
            fixed(limits.max_debt_percent, 4)
        ),
        debt_pct,
        limits.max_debt_percent,
    ));
    for h in holdings {
        if let Some(leverage) = h.leverage {
            let label = if h.symbol.is_empty() {
                &h.asset
            } else {
                &h.symbol
            };
            out.push(
                AllocationConstraint::new(
                    "leverage",
                    leverage <= limits.max_leverage,
                    format!(
                        "{label} leverage {} vs limit {}",
                        fixed(leverage, 4),
                        fixed(limits.max_leverage, 4)
                    ),
                    leverage,
                    limits.max_leverage,
                )
                .on(h),
            );
        }
        if h.transferable_usdt < limits.min_transferable_usdt {
            out.push(
                AllocationConstraint::new(
                    "transferability",
                    false,
                    format!(
                        "{} {} transferable {} USDT below minimum {}",
                        h.wallet,
                        h.asset,
                        fixed(h.transferable_usdt, 8),
                        fixed(limits.min_transferable_usdt, 8)
                    ),
                    h.transferable_usdt,
                    limits.min_transferable_usdt,
                )
                .on(h),
            );
        }
        if let Some(score) = h
            .liquidity_score
            .filter(|s| *s < limits.min_liquidity_score)
        {
            out.push(
                AllocationConstraint::new(
                    "liquidity",
                    false,
                    format!(
                        "{} liquidity {} below minimum {}",
                        h.asset,
                        fixed(score, 4),
                        fixed(limits.min_liquidity_score, 4)
                    ),
                    score,
                    limits.min_liquidity_score,
                )
                .on(h),
            );
        }
        if let Some(vol) = h
            .volatility_percent
            .filter(|v| *v > limits.max_volatility_percent)
        {
            out.push(
                AllocationConstraint::new(
                    "volatility",
                    false,
                    format!(
                        "{} volatility {}% above maximum {}%",
                        h.asset,
                        fixed(vol, 4),
                        fixed(limits.max_volatility_percent, 4)
                    ),
                    vol,
                    limits.max_volatility_percent,
                )
                .on(h),
            );
        }
    }
    for (asset, reserve) in &limits.asset_reserves {
        let asset = normalize_asset(asset)?;
        let free_amount: Decimal = holdings
            .iter()
            .filter(|h| h.asset == asset)
            .map(|h| h.transferable_usdt / h.price_usdt)
            .sum();
        let mut c = AllocationConstraint::new(
            "reserve",
            free_amount >= *reserve,
            format!(
                "{asset} reserve {} vs required {}",
                fixed(free_amount, 8),
                fixed(*reserve, 8)
            ),
            free_amount,
            *reserve,
        );
        c.asset = asset;
        out.push(c);
    }
    Ok(out)
}

fn sum_by(
    holdings: &[HoldingValuation],
    key: fn(&HoldingValuation) -> &String,
) -> Vec<(String, Decimal)> {
    let mut totals: Vec<(String, Decimal)> = Vec::new();
    for h in holdings {
        match totals.iter_mut().find(|(name, _)| name == key(h)) {
            Some((_, total)) => *total += h.net_usdt,
            None => totals.push((key(h).clone(), h.net_usdt)),
        }
    }
    totals
}

fn outbound_transfer_routes(wallet: &str) -> Vec<String> {
    let mut routes: Vec<String> = TRANSFER_ROUTES
        .iter()
        .filter(|(source, _, _)| *source == wallet)
        .map(|(_, _, kind)| (*kind).to_owned())
        .collect();
    routes.sort();
    routes
}

fn basket_weights_for(
    benchmarks: &[String],
    basket_weights: Option<&BTreeMap<String, Decimal>>,
) -> Result<Vec<(String, Decimal)>> {
    if benchmarks.is_empty() {
        return Err(unified_error("benchmarks must not be empty"));
    }
    let Some(raw) = basket_weights else {
        let equal = Decimal::ONE / Decimal::from(benchmarks.len());
        return Ok(benchmarks.iter().map(|b| (b.clone(), equal)).collect());
    };
    let mut weights = BTreeMap::new();
    for (asset, weight) in raw {
        weights.insert(
            normalize_asset(asset)?,
            positive(*weight, &format!("basket weight for {asset}"))?,
        );
    }
    let missing: Vec<&str> = benchmarks
        .iter()
        .filter(|b| !weights.contains_key(*b))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(unified_error(format!(
            "basket weights missing benchmarks: {}",
            missing.join(", ")
        )));
    }
    let total: Decimal = benchmarks.iter().map(|b| weights[b]).sum();
    if (total - Decimal::ONE).abs() > Decimal::new(1, 8) {
        return Err(unified_error("basket weights must sum to 1"));
    }
    Ok(benchmarks.iter().map(|b| (b.clone(), weights[b])).collect())
}

fn normalize_account_type(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    if [
        ACCOUNT_SPOT,
        ACCOUNT_FUNDING,
        ACCOUNT_MARGIN,
        ACCOUNT_FUTURES,
    ]
    .contains(&normalized.as_str())
    {
        Ok(normalized)
    } else {
        Err(unified_error(format!("unsupported account_type: {value}")))
    }
}

fn validate_limits(limits: &UnifiedPoolLimits) -> Result<()> {
    positive(
        limits.max_asset_allocation_percent,
        "max_asset_allocation_percent",
    )?;
    positive(
        limits.max_account_type_allocation_percent,
        "max_account_type_allocation_percent",
    )?;
    positive(limits.max_leverage, "max_leverage")?;
    positive(limits.max_debt_percent, "max_debt_percent")?;
    positive(limits.max_volatility_percent, "max_volatility_percent")?;
    limits
        .asset_reserves
        .keys()
        .try_for_each(|asset| normalize_asset(asset).map(drop))
}

fn price(asset: &str, prices: &BTreeMap<String, Decimal>) -> Result<Decimal> {
    let normalized = normalize_asset(asset)?;
    if normalized == "USDT" {
        return Ok(Decimal::ONE);
    }
    let price = prices
        .get(&normalized)
        .ok_or_else(|| unified_error(format!("missing USDT price for {normalized}")))?;
    positive(*price, &format!("price for {normalized}"))
}

fn normalize_asset(value: &str) -> Result<String> {
    let text = value.trim().to_uppercase();
    if text.is_empty() {
        return Err(unified_error("asset must not be blank"));
    }
    Ok(text)
}

fn positive(value: Decimal, field: &str) -> Result<Decimal> {
    if value <= Decimal::ZERO {
        return Err(unified_error(format!("{field} must be greater than zero")));
    }
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn prices() -> BTreeMap<String, Decimal> {
        BTreeMap::from([
            ("USDT".to_owned(), dec!(1)),
            ("BNB".to_owned(), dec!(600)),
            ("BTC".to_owned(), dec!(50000)),
            ("PAXG".to_owned(), dec!(2300)),
            ("XAUT".to_owned(), dec!(2300)),
        ])
    }

    fn benchmarks() -> Vec<String> {
        ["USDT", "BTC", "PAXG", "XAUT"].map(String::from).to_vec()
    }

    fn get(values: &[(String, Decimal)], key: &str) -> Decimal {
        UnifiedPoolSnapshot::value_of(values, key).unwrap()
    }

    #[test]
    fn values_spot_margin_futures_and_benchmarks() {
        let mut bnb = UnifiedHolding::new(ACCOUNT_SPOT, WALLET_SPOT, "BNB", dec!(0.02));
        bnb.free = dec!(0.017);
        bnb.locked = dec!(0.003);
        bnb.liquidity_score = Some(dec!(0.9));
        bnb.volatility_percent = Some(dec!(20));
        let mut funding = UnifiedHolding::new(ACCOUNT_FUNDING, WALLET_FUNDING, "USDT", dec!(5));
        funding.free = dec!(5);
        let mut margin = UnifiedHolding::new(ACCOUNT_MARGIN, WALLET_CROSS_MARGIN, "USDT", dec!(10));
        margin.free = dec!(8);
        margin.borrowed = dec!(2);
        margin.interest = dec!(0.1);
        let mut futures =
            UnifiedHolding::new(ACCOUNT_FUTURES, WALLET_USD_M_FUTURES, "USDT", dec!(20));
        futures.free = dec!(16);
        futures.available = dec!(16);
        futures.unrealized_pnl = dec!(-1);
        futures.initial_margin = dec!(3);
        futures.maintenance_margin = dec!(0.5);
        futures.leverage = Some(dec!(2));
        futures.symbol = "BTCUSDT".into();
        let limits = UnifiedPoolLimits {
            max_asset_allocation_percent: dec!(100),
            max_account_type_allocation_percent: dec!(100),
            max_debt_percent: dec!(100),
            min_liquidity_score: dec!(0.5),
            max_volatility_percent: dec!(50),
            ..UnifiedPoolLimits::default()
        };
        let snapshot = build_unified_pool_snapshot(
            &[bnb, funding, margin, futures],
            &prices(),
            &benchmarks(),
            None,
            &limits,
        )
        .unwrap();
        assert!(snapshot.allowed);
        assert_eq!(snapshot.total_gross_usdt, dec!(47));
        assert_eq!(snapshot.total_net_usdt, dec!(43.9));
        assert_eq!(snapshot.total_debt_usdt, dec!(2));
        assert_eq!(snapshot.total_interest_usdt, dec!(0.1));
        assert_eq!(snapshot.total_open_pnl_usdt, dec!(-1));
        assert_eq!(snapshot.total_initial_margin_usdt, dec!(3));
        assert_eq!(snapshot.total_maintenance_margin_usdt, dec!(0.5));
        assert_eq!(get(&snapshot.asset_values_usdt, "BNB"), dec!(12));
        assert_eq!(
            get(&snapshot.account_type_values_usdt, ACCOUNT_MARGIN),
            dec!(7.9)
        );
        assert_eq!(get(&snapshot.benchmark_values, "USDT"), dec!(43.9));
        assert_eq!(
            get(&snapshot.benchmark_values, "BTC"),
            dec!(43.9) / dec!(50000)
        );
        assert_eq!(
            get(&snapshot.benchmark_values, "PAXG"),
            dec!(43.9) / dec!(2300)
        );
        assert_eq!(
            get(&snapshot.basket_components, "USDT"),
            dec!(43.9) / dec!(4)
        );
        assert!(!snapshot.live_trading_enabled && !snapshot.live_transfer_enabled);
        assert!(snapshot.write_endpoints_used.is_empty());
        assert_eq!(
            snapshot.transfer_availability["spot:BNB"].routes,
            ["MAIN_FUNDING", "MAIN_MARGIN", "MAIN_UMFUTURE"]
        );
    }

    #[test]
    fn rejects_allocation_leverage_liquidity_volatility_and_reserve() {
        let mut holding =
            UnifiedHolding::new(ACCOUNT_FUTURES, WALLET_USD_M_FUTURES, "BNB", dec!(1));
        holding.leverage = Some(dec!(5));
        holding.symbol = "BNBUSDT".into();
        holding.liquidity_score = Some(dec!(0.1));
        holding.volatility_percent = Some(dec!(120));
        holding.transfer_enabled = Some(false);
        let limits = UnifiedPoolLimits {
            max_asset_allocation_percent: dec!(50),
            max_account_type_allocation_percent: dec!(80),
            max_leverage: dec!(3),
            max_debt_percent: dec!(10),
            min_transferable_usdt: dec!(10),
            min_liquidity_score: dec!(0.5),
            max_volatility_percent: dec!(50),
            asset_reserves: BTreeMap::from([("BNB".to_owned(), dec!(0.00005))]),
        };
        let snapshot =
            build_unified_pool_snapshot(&[holding], &prices(), &benchmarks(), None, &limits)
                .unwrap();
        let blocked: Vec<&str> = snapshot
            .constraints
            .iter()
            .filter(|c| c.status == ConstraintStatus::Blocked)
            .map(|c| c.name.as_str())
            .collect();
        assert!(!snapshot.allowed);
        for name in [
            "asset_allocation",
            "account_type_allocation",
            "leverage",
            "transferability",
            "liquidity",
            "volatility",
            "reserve",
        ] {
            assert!(blocked.contains(&name), "{name} not blocked");
        }
        let rendered = render_unified_snapshot(&snapshot);
        assert!(rendered.contains("Allowed: no"));
        assert!(rendered.contains("BNBUSDT leverage 5.0000 vs limit 3.0000"));
    }

    #[test]
    fn validates_prices_weights_and_wallets() {
        let holding = UnifiedHolding::new(ACCOUNT_SPOT, WALLET_SPOT, "DOGE", dec!(1));
        let err = build_unified_pool_snapshot(
            &[holding],
            &prices(),
            &benchmarks(),
            None,
            &UnifiedPoolLimits::default(),
        );
        assert_eq!(err.unwrap_err().to_string(), "missing USDT price for DOGE");
        let weights = BTreeMap::from([("USDT".to_owned(), dec!(0.5))]);
        let err = build_unified_pool_snapshot(
            &[],
            &prices(),
            &benchmarks(),
            Some(&weights),
            &UnifiedPoolLimits::default(),
        );
        assert!(
            err.unwrap_err()
                .to_string()
                .contains("basket weights missing benchmarks")
        );
        assert_eq!(
            account_type_from_wallet(WALLET_ISOLATED_MARGIN).unwrap(),
            ACCOUNT_MARGIN
        );
        assert!(account_type_from_wallet("mars").is_err());
    }
}

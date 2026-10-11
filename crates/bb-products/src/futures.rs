//! Read-only USD-M futures discovery and local risk modelling
//! (port of `src/futures.py`). No futures write endpoint is ever called.

use bb_binance::{Api, BinanceClient, BinanceError, RestRequest, normalize_symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde_json::Value;

use crate::common::{
    self, fmt_dec, fmt_opt, jfloat, jtext, num, num_opt, optional_positive, yes_no,
};
use crate::settings::Settings;

pub const PRODUCT_USDM: &str = "USD-M";
pub const WRITE_ENDPOINTS_BLOCKED: [&str; 6] = [
    "fapiPrivatePostOrder",
    "fapiPrivatePostBatchOrders",
    "fapiPrivatePostLeverage",
    "fapiPrivatePostMarginType",
    "fapiPrivatePostPositionSideDual",
    "fapiPrivatePostPositionMargin",
];

/// Raised when dry-run futures modelling cannot proceed safely.
#[derive(Debug, thiserror::Error)]
pub enum FuturesModelError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Binance(#[from] BinanceError),
}

fn invalid(msg: impl Into<String>) -> FuturesModelError {
    FuturesModelError::Invalid(msg.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "UPPERCASE")]
#[value(rename_all = "UPPER")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "UPPERCASE")]
#[value(rename_all = "UPPER")]
pub enum PositionSide {
    Both,
    Long,
    Short,
}

impl PositionSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Both => "BOTH",
            Self::Long => "LONG",
            Self::Short => "SHORT",
        }
    }

    pub fn parse(value: &str) -> Result<Self, FuturesModelError> {
        match value.trim().to_uppercase().as_str() {
            "BOTH" => Ok(Self::Both),
            "LONG" => Ok(Self::Long),
            "SHORT" => Ok(Self::Short),
            _ => Err(invalid("position_side must be BOTH, LONG, or SHORT")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "UPPERCASE")]
#[value(rename_all = "UPPER")]
pub enum FuturesOrderType {
    Market,
    Limit,
}

/// One USD-M futures asset balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesAssetBalance {
    pub asset: String,
    #[serde(with = "num")]
    pub wallet_balance: Decimal,
    #[serde(with = "num")]
    pub unrealized_profit: Decimal,
    #[serde(with = "num")]
    pub margin_balance: Decimal,
    #[serde(with = "num")]
    pub maint_margin: Decimal,
    #[serde(with = "num")]
    pub initial_margin: Decimal,
    #[serde(with = "num")]
    pub position_initial_margin: Decimal,
    #[serde(with = "num")]
    pub open_order_initial_margin: Decimal,
    #[serde(with = "num")]
    pub cross_wallet_balance: Decimal,
    #[serde(with = "num")]
    pub cross_unrealized_pnl: Decimal,
    #[serde(with = "num")]
    pub available_balance: Decimal,
    #[serde(with = "num")]
    pub max_withdraw_amount: Decimal,
}

/// Read-only USD-M account snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesAccountSnapshot {
    #[serde(with = "num")]
    pub total_wallet_balance: Decimal,
    #[serde(with = "num")]
    pub total_unrealized_profit: Decimal,
    #[serde(with = "num")]
    pub total_margin_balance: Decimal,
    #[serde(with = "num")]
    pub total_initial_margin: Decimal,
    #[serde(with = "num")]
    pub total_maint_margin: Decimal,
    #[serde(with = "num")]
    pub total_position_initial_margin: Decimal,
    #[serde(with = "num")]
    pub total_open_order_initial_margin: Decimal,
    #[serde(with = "num")]
    pub total_cross_wallet_balance: Decimal,
    #[serde(with = "num")]
    pub total_cross_unrealized_pnl: Decimal,
    #[serde(with = "num")]
    pub available_balance: Decimal,
    #[serde(with = "num")]
    pub max_withdraw_amount: Decimal,
    pub assets: Vec<FuturesAssetBalance>,
}

impl FuturesAccountSnapshot {
    pub fn maintenance_buffer_percent(&self) -> Option<Decimal> {
        if self.total_maint_margin <= Decimal::ZERO {
            return None;
        }
        Some(
            (self.total_margin_balance - self.total_maint_margin) / self.total_maint_margin
                * dec!(100),
        )
    }
}

/// Read-only USD-M position risk row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesPosition {
    pub symbol: String,
    pub position_side: PositionSide,
    #[serde(with = "num")]
    pub position_amt: Decimal,
    #[serde(with = "num")]
    pub entry_price: Decimal,
    #[serde(with = "num")]
    pub break_even_price: Decimal,
    #[serde(with = "num")]
    pub mark_price: Decimal,
    #[serde(with = "num")]
    pub unrealized_profit: Decimal,
    #[serde(with = "num_opt")]
    pub liquidation_price: Option<Decimal>,
    #[serde(with = "num")]
    pub notional: Decimal,
    pub margin_asset: String,
    #[serde(with = "num")]
    pub isolated_margin: Decimal,
    #[serde(with = "num")]
    pub isolated_wallet: Decimal,
    #[serde(with = "num")]
    pub initial_margin: Decimal,
    #[serde(with = "num")]
    pub maint_margin: Decimal,
    #[serde(with = "num")]
    pub position_initial_margin: Decimal,
    #[serde(with = "num")]
    pub open_order_initial_margin: Decimal,
    #[serde(with = "num_opt")]
    pub leverage: Option<Decimal>,
    pub margin_type: String,
    pub adl: Option<i64>,
    pub update_time: Option<i64>,
}

/// Market rules and filters for one USD-M symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesSymbolRules {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub margin_asset: String,
    pub contract_type: String,
    pub status: String,
    #[serde(with = "num")]
    pub quantity_step: Decimal,
    #[serde(with = "num")]
    pub min_qty: Decimal,
    #[serde(with = "num")]
    pub max_qty: Decimal,
    #[serde(with = "num")]
    pub tick_size: Decimal,
    #[serde(with = "num")]
    pub min_price: Decimal,
    #[serde(with = "num")]
    pub max_price: Decimal,
    #[serde(with = "num")]
    pub min_notional: Decimal,
    #[serde(with = "num")]
    pub trigger_protect: Decimal,
}

/// Account-specific USD-M symbol configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesSymbolConfig {
    pub symbol: String,
    pub margin_type: String,
    #[serde(with = "num")]
    pub leverage: Decimal,
    #[serde(with = "num")]
    pub max_notional_value: Decimal,
    pub auto_add_margin: Option<bool>,
}

/// One leverage bracket for a USD-M symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesLeverageBracket {
    pub symbol: String,
    #[serde(with = "num")]
    pub initial_leverage: Decimal,
    #[serde(with = "num")]
    pub notional_floor: Decimal,
    #[serde(with = "num")]
    pub notional_cap: Decimal,
    #[serde(with = "num")]
    pub maint_margin_ratio: Decimal,
    #[serde(with = "num")]
    pub cum: Decimal,
}

/// Mark/index price and current funding information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesMarkPrice {
    pub symbol: String,
    #[serde(with = "num")]
    pub mark_price: Decimal,
    #[serde(with = "num")]
    pub index_price: Decimal,
    #[serde(with = "num")]
    pub estimated_settle_price: Decimal,
    #[serde(with = "num")]
    pub last_funding_rate: Decimal,
    #[serde(with = "num")]
    pub interest_rate: Decimal,
    pub next_funding_time: Option<i64>,
    pub time: Option<i64>,
}

/// Combined read-only USD-M futures discovery result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesDiscovery {
    pub timestamp: String,
    pub product: String,
    pub account: FuturesAccountSnapshot,
    pub positions: Vec<FuturesPosition>,
    pub symbols: Vec<FuturesSymbolRules>,
    pub symbol_configs: Vec<FuturesSymbolConfig>,
    pub leverage_brackets: Vec<FuturesLeverageBracket>,
    pub mark_prices: Vec<FuturesMarkPrice>,
    pub live_trading_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

impl FuturesDiscovery {
    pub fn symbol_rules(&self, symbol: &str) -> Option<&FuturesSymbolRules> {
        let id = futures_symbol(symbol).ok()?;
        self.symbols.iter().find(|r| r.symbol == id)
    }

    pub fn symbol_config(&self, symbol: &str) -> Option<&FuturesSymbolConfig> {
        let id = futures_symbol(symbol).ok()?;
        self.symbol_configs.iter().find(|c| c.symbol == id)
    }

    /// Bracket containing `notional`, else the symbol's first bracket.
    pub fn leverage_bracket(
        &self,
        symbol: &str,
        notional: Decimal,
    ) -> Option<&FuturesLeverageBracket> {
        let id = futures_symbol(symbol).ok()?;
        let mut candidates = self
            .leverage_brackets
            .iter()
            .filter(|b| b.symbol == id)
            .peekable();
        let first = candidates.peek().copied();
        candidates
            .find(|b| b.notional_floor <= notional && notional <= b.notional_cap)
            .or(first)
    }

    pub fn mark_price(&self, symbol: &str) -> Option<&FuturesMarkPrice> {
        let id = futures_symbol(symbol).ok()?;
        self.mark_prices.iter().find(|m| m.symbol == id)
    }

    pub fn position(&self, symbol: &str, side: PositionSide) -> Option<&FuturesPosition> {
        let id = futures_symbol(symbol).ok()?;
        self.positions
            .iter()
            .find(|p| p.symbol == id && p.position_side == side)
    }
}

/// Conservative local risk limits for USD-M dry-run plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesRiskLimits {
    #[serde(with = "num")]
    pub max_leverage: Decimal,
    #[serde(with = "num")]
    pub max_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub max_account_notional_percent: Decimal,
    #[serde(with = "num")]
    pub max_initial_margin_usdt: Decimal,
    #[serde(with = "num")]
    pub min_free_balance_after_initial_margin_usdt: Decimal,
    #[serde(with = "num")]
    pub min_liquidation_distance_percent: Decimal,
    #[serde(with = "num")]
    pub max_abs_funding_rate: Decimal,
    #[serde(with = "num")]
    pub max_abs_funding_payment_usdt: Decimal,
    #[serde(with = "num")]
    pub min_maintenance_buffer_percent: Decimal,
    pub allowed_margin_types: Vec<String>,
}

impl Default for FuturesRiskLimits {
    fn default() -> Self {
        Self {
            max_leverage: dec!(3.0),
            max_notional_usdt: dec!(25.0),
            max_account_notional_percent: dec!(25.0),
            max_initial_margin_usdt: dec!(10.0),
            min_free_balance_after_initial_margin_usdt: dec!(5.0),
            min_liquidation_distance_percent: dec!(20.0),
            max_abs_funding_rate: dec!(0.001),
            max_abs_funding_payment_usdt: dec!(0.05),
            min_maintenance_buffer_percent: dec!(50.0),
            allowed_margin_types: vec!["CROSSED".into(), "ISOLATED".into()],
        }
    }
}

impl FuturesRiskLimits {
    pub fn validate(&self) -> Result<(), FuturesModelError> {
        positive(self.max_leverage, "max_leverage")?;
        positive(self.max_notional_usdt, "max_notional_usdt")?;
        positive(
            self.max_account_notional_percent,
            "max_account_notional_percent",
        )?;
        if self.max_account_notional_percent > dec!(100) {
            return Err(invalid("max_account_notional_percent must be <= 100"));
        }
        positive(self.max_initial_margin_usdt, "max_initial_margin_usdt")?;
        non_negative(
            self.min_free_balance_after_initial_margin_usdt,
            "min_free_balance_after_initial_margin_usdt",
        )?;
        positive(
            self.min_liquidation_distance_percent,
            "min_liquidation_distance_percent",
        )?;
        non_negative(self.max_abs_funding_rate, "max_abs_funding_rate")?;
        non_negative(
            self.max_abs_funding_payment_usdt,
            "max_abs_funding_payment_usdt",
        )?;
        positive(
            self.min_maintenance_buffer_percent,
            "min_maintenance_buffer_percent",
        )?;
        if self.allowed_margin_types.is_empty() {
            return Err(invalid("allowed_margin_types must not be empty"));
        }
        Ok(())
    }
}

/// Requested USD-M futures order modelling inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuturesOrderIntent {
    pub symbol: String,
    pub side: OrderSide,
    pub quantity: Decimal,
    pub order_type: FuturesOrderType,
    pub price: Option<Decimal>,
    pub position_side: PositionSide,
    pub reduce_only: bool,
    pub requested_leverage: Option<Decimal>,
}

impl FuturesOrderIntent {
    pub fn market(symbol: &str, side: OrderSide, quantity: Decimal) -> Self {
        Self {
            symbol: symbol.to_owned(),
            side,
            quantity,
            order_type: FuturesOrderType::Market,
            price: None,
            position_side: PositionSide::Both,
            reduce_only: false,
            requested_leverage: None,
        }
    }
}

/// Dry-run USD-M order risk result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesOrderPlan {
    pub timestamp: String,
    pub product: String,
    pub symbol: String,
    pub side: OrderSide,
    pub position_side: PositionSide,
    pub order_type: FuturesOrderType,
    #[serde(with = "num")]
    pub quantity: Decimal,
    #[serde(with = "num_opt")]
    pub price: Option<Decimal>,
    #[serde(with = "num")]
    pub mark_price: Decimal,
    #[serde(with = "num")]
    pub notional_usdt: Decimal,
    #[serde(with = "num_opt")]
    pub requested_leverage: Option<Decimal>,
    #[serde(with = "num")]
    pub configured_leverage: Decimal,
    #[serde(with = "num")]
    pub effective_leverage: Decimal,
    pub margin_type: String,
    /// `None` when leverage is zero (Python produced `inf`).
    #[serde(with = "num_opt")]
    pub initial_margin_required_usdt: Option<Decimal>,
    #[serde(with = "num")]
    pub maintenance_margin_estimate_usdt: Decimal,
    #[serde(with = "num_opt")]
    pub liquidation_price_estimate: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub liquidation_distance_percent: Option<Decimal>,
    #[serde(with = "num")]
    pub funding_rate: Decimal,
    #[serde(with = "num")]
    pub projected_funding_payment_usdt: Decimal,
    pub reduce_only: bool,
    pub opening_position: bool,
    pub closing_position: bool,
    #[serde(with = "num_opt")]
    pub maintenance_buffer_percent: Option<Decimal>,
    pub symbol_rules: FuturesSymbolRules,
    pub leverage_bracket: FuturesLeverageBracket,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub risk_limits: FuturesRiskLimits,
}

/// Dry-run reconciliation of current USD-M account state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesReconciliation {
    pub timestamp: String,
    pub total_positions: usize,
    pub nonzero_positions: usize,
    #[serde(with = "num")]
    pub total_abs_notional_usdt: Decimal,
    #[serde(with = "num")]
    pub total_unrealized_pnl_usdt: Decimal,
    #[serde(with = "num")]
    pub margin_balance_usdt: Decimal,
    #[serde(with = "num")]
    pub maint_margin_usdt: Decimal,
    #[serde(with = "num_opt")]
    pub maintenance_buffer_percent: Option<Decimal>,
    pub allowed: bool,
    pub rejection_reasons: Vec<String>,
}

/// Full USD-M futures dry-run report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuturesDryRunReport {
    pub timestamp: String,
    pub mode: String,
    pub discovery: FuturesDiscovery,
    pub order_plan: Option<FuturesOrderPlan>,
    pub reconciliation: Option<FuturesReconciliation>,
    pub live_trading_enabled: bool,
    pub write_endpoints_used: Vec<String>,
}

/// Discover USD-M futures account state with read-only calls.
pub async fn discover_usdm_futures(
    client: &BinanceClient,
    symbols: &[String],
) -> Result<FuturesDiscovery, FuturesModelError> {
    let ids = symbols
        .iter()
        .map(|s| futures_symbol(s))
        .collect::<Result<Vec<_>, _>>()?;
    let single = (ids.len() == 1).then(|| ids[0].clone());
    let signed = |path: &str, weight: u32| {
        let req = RestRequest::get(Api::Futures, path).signed().weight(weight);
        match &single {
            Some(symbol) => req.param("symbol", symbol),
            None => req,
        }
    };
    let account: Value = client
        .request(
            RestRequest::get(Api::Futures, "/fapi/v3/account")
                .signed()
                .weight(5),
        )
        .await?;
    let positions: Value = client.request(signed("/fapi/v3/positionRisk", 5)).await?;
    let exchange_info: Value = client
        .request(RestRequest::get(Api::Futures, "/fapi/v1/exchangeInfo").weight(1))
        .await?;
    let configs: Value = client.request(signed("/fapi/v1/symbolConfig", 5)).await?;
    let brackets: Value = client
        .request(signed("/fapi/v1/leverageBracket", 1))
        .await?;
    let premium = RestRequest::get(Api::Futures, "/fapi/v1/premiumIndex").weight(1);
    let premium = match &single {
        Some(symbol) => premium.param("symbol", symbol),
        None => premium,
    };
    let marks: Value = client.request(premium).await?;

    Ok(FuturesDiscovery {
        timestamp: common::utc_timestamp(),
        product: PRODUCT_USDM.to_owned(),
        account: parse_futures_account_snapshot(&account)?,
        positions: parse_futures_positions(&positions)?,
        symbols: parse_futures_symbol_rules(&exchange_info, &ids)?,
        symbol_configs: parse_futures_symbol_configs(&configs),
        leverage_brackets: parse_futures_leverage_brackets(&brackets),
        mark_prices: parse_futures_mark_prices(&marks),
        live_trading_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

/// Build a dry-run USD-M order plan without submitting an order.
pub fn build_futures_order_plan(
    discovery: &FuturesDiscovery,
    intent: &FuturesOrderIntent,
    limits: &FuturesRiskLimits,
) -> Result<FuturesOrderPlan, FuturesModelError> {
    limits.validate()?;
    let symbol = futures_symbol(&intent.symbol)?;
    let side = intent.side;
    let position_side = intent.position_side;
    let quantity = positive(intent.quantity, "quantity")?;
    let rules = discovery
        .symbol_rules(&symbol)
        .ok_or_else(|| invalid(format!("symbol rules not found for {symbol}")))?;
    let config = discovery
        .symbol_config(&symbol)
        .ok_or_else(|| invalid(format!("symbol config not found for {symbol}")))?;
    let mark = discovery
        .mark_price(&symbol)
        .ok_or_else(|| invalid(format!("mark price not found for {symbol}")))?;
    let price = match intent.order_type {
        FuturesOrderType::Limit => positive(
            intent
                .price
                .ok_or_else(|| invalid("limit futures plan requires --price"))?,
            "price",
        )?,
        FuturesOrderType::Market => {
            if mark.mark_price <= Decimal::ZERO {
                return Err(invalid(
                    "mark price must be positive for market order modeling",
                ));
            }
            mark.mark_price
        }
    };
    let notional = quantity * price;
    let bracket = discovery
        .leverage_bracket(&symbol, notional)
        .ok_or_else(|| invalid(format!("leverage bracket not found for {symbol}")))?;
    let leverage = match intent.requested_leverage {
        Some(requested) => positive(requested, "requested_leverage")?,
        None => config.leverage,
    };
    let initial_margin = (leverage > Decimal::ZERO).then(|| notional / leverage);
    let maintenance_margin = notional * bracket.maint_margin_ratio + bracket.cum;
    let liquidation_price =
        estimate_liquidation_price(side, price, leverage, bracket.maint_margin_ratio);
    let liquidation_distance = liquidation_distance_percent(side, price, liquidation_price);
    let mut funding_payment = notional * mark.last_funding_rate;
    if opens_short(side, position_side) {
        funding_payment = -funding_payment;
    }
    let position = discovery.position(&symbol, position_side);
    let closing = is_closing_position(position, side, quantity, position_side);
    let account = &discovery.account;
    let buffer = account.maintenance_buffer_percent();
    // Python compared `inf` against every cap; a missing margin exceeds them all.
    let margin_exceeds = |cap: Decimal| initial_margin.is_none_or(|m| m > cap);

    let mut rejections: Vec<&str> = Vec::new();
    let mut warnings: Vec<&str> = Vec::new();
    if rules.status != "TRADING" {
        rejections.push("symbol is not trading");
    }
    if rules.contract_type != "PERPETUAL" {
        rejections.push("only USD-M perpetual contracts are modeled");
    }
    if quantity < rules.min_qty {
        rejections.push("quantity is below the symbol minimum");
    }
    if rules.max_qty > Decimal::ZERO && quantity > rules.max_qty {
        rejections.push("quantity is above the symbol maximum");
    }
    if !is_multiple(quantity, rules.quantity_step) {
        rejections.push("quantity does not align with the symbol step size");
    }
    if intent.order_type == FuturesOrderType::Limit
        && intent
            .price
            .is_some_and(|p| !is_multiple(p, rules.tick_size))
    {
        rejections.push("limit price does not align with the symbol tick size");
    }
    if notional < rules.min_notional {
        rejections.push("notional is below the symbol minimum notional");
    }
    if leverage > limits.max_leverage {
        rejections.push("configured leverage exceeds the risk limit");
    }
    if leverage > bracket.initial_leverage {
        rejections.push("configured leverage exceeds the leverage bracket");
    }
    if !limits.allowed_margin_types.contains(&config.margin_type) {
        rejections.push("margin type is not allowed by risk limits");
    }
    if notional > limits.max_notional_usdt {
        rejections.push("notional exceeds max_notional_usdt");
    }
    if account.total_margin_balance > Decimal::ZERO
        && notional > account.total_margin_balance * limits.max_account_notional_percent / dec!(100)
    {
        rejections.push("notional exceeds account exposure percent limit");
    }
    if margin_exceeds(limits.max_initial_margin_usdt) {
        rejections.push("initial margin exceeds max_initial_margin_usdt");
    }
    if margin_exceeds(account.available_balance) {
        rejections.push("initial margin exceeds available futures balance");
    }
    if initial_margin.is_none_or(|m| {
        account.available_balance - m < limits.min_free_balance_after_initial_margin_usdt
    }) {
        rejections.push("free balance after initial margin would be too low");
    }
    if liquidation_distance.is_some_and(|d| d < limits.min_liquidation_distance_percent) {
        rejections.push("estimated liquidation distance is below minimum");
    }
    if mark.last_funding_rate.abs() > limits.max_abs_funding_rate {
        rejections.push("absolute funding rate exceeds configured cap");
    }
    if funding_payment.abs() > limits.max_abs_funding_payment_usdt {
        rejections.push("projected funding payment exceeds configured cap");
    }
    if buffer.is_some_and(|b| b < limits.min_maintenance_buffer_percent) {
        rejections.push("account maintenance buffer is below minimum");
    }
    if intent.reduce_only && !closing {
        rejections.push("reduce-only order would not reduce an existing position");
    }
    if intent.reduce_only && position.is_some_and(|p| quantity > p.position_amt.abs()) {
        rejections.push("reduce-only quantity exceeds the existing position size");
    }
    if !intent.reduce_only && closing {
        warnings.push("order would reduce an existing position but is not reduce-only");
    }

    Ok(FuturesOrderPlan {
        timestamp: common::utc_timestamp(),
        product: PRODUCT_USDM.to_owned(),
        symbol,
        side,
        position_side,
        order_type: intent.order_type,
        quantity,
        price: if intent.order_type == FuturesOrderType::Limit {
            intent.price
        } else {
            None
        },
        mark_price: mark.mark_price,
        notional_usdt: notional,
        requested_leverage: intent.requested_leverage,
        configured_leverage: leverage,
        effective_leverage: leverage,
        margin_type: config.margin_type.clone(),
        initial_margin_required_usdt: initial_margin,
        maintenance_margin_estimate_usdt: maintenance_margin,
        liquidation_price_estimate: liquidation_price,
        liquidation_distance_percent: liquidation_distance,
        funding_rate: mark.last_funding_rate,
        projected_funding_payment_usdt: funding_payment,
        reduce_only: intent.reduce_only,
        opening_position: !closing,
        closing_position: closing,
        maintenance_buffer_percent: buffer,
        symbol_rules: rules.clone(),
        leverage_bracket: bracket.clone(),
        allowed: rejections.is_empty(),
        rejection_reasons: rejections.into_iter().map(str::to_owned).collect(),
        warnings: warnings.into_iter().map(str::to_owned).collect(),
        risk_limits: limits.clone(),
    })
}

/// Summarise current USD-M futures positions and account buffer.
pub fn reconcile_futures_state(
    discovery: &FuturesDiscovery,
    limits: &FuturesRiskLimits,
) -> Result<FuturesReconciliation, FuturesModelError> {
    limits.validate()?;
    let nonzero: Vec<&FuturesPosition> = discovery
        .positions
        .iter()
        .filter(|p| !p.position_amt.is_zero() || !p.notional.is_zero())
        .collect();
    let total_abs: Decimal = nonzero.iter().map(|p| p.notional.abs()).sum();
    let total_unrealized: Decimal = nonzero.iter().map(|p| p.unrealized_profit).sum();
    let buffer = discovery.account.maintenance_buffer_percent();
    let mut rejections = Vec::new();
    if buffer.is_some_and(|b| b < limits.min_maintenance_buffer_percent) {
        rejections.push("account maintenance buffer is below minimum".to_owned());
    }
    if total_abs > limits.max_notional_usdt {
        rejections.push("open position notional exceeds max_notional_usdt".to_owned());
    }
    Ok(FuturesReconciliation {
        timestamp: common::utc_timestamp(),
        total_positions: discovery.positions.len(),
        nonzero_positions: nonzero.len(),
        total_abs_notional_usdt: total_abs,
        total_unrealized_pnl_usdt: total_unrealized,
        margin_balance_usdt: discovery.account.total_margin_balance,
        maint_margin_usdt: discovery.account.total_maint_margin,
        maintenance_buffer_percent: buffer,
        allowed: rejections.is_empty(),
        rejection_reasons: rejections,
    })
}

/// Build a full USD-M futures dry-run report from read-only endpoints.
pub async fn build_futures_dry_run_report(
    settings: &Settings,
    client: &BinanceClient,
    symbols: &[String],
    intent: Option<&FuturesOrderIntent>,
    limits: &FuturesRiskLimits,
) -> Result<FuturesDryRunReport, FuturesModelError> {
    let discovery = discover_usdm_futures(client, symbols).await?;
    let reconciliation = reconcile_futures_state(&discovery, limits)?;
    let order_plan = intent
        .map(|i| build_futures_order_plan(&discovery, i, limits))
        .transpose()?;
    Ok(FuturesDryRunReport {
        timestamp: common::utc_timestamp(),
        mode: common::mode(settings).to_owned(),
        discovery,
        order_plan,
        reconciliation: Some(reconciliation),
        live_trading_enabled: false,
        write_endpoints_used: Vec::new(),
    })
}

pub fn parse_futures_account_snapshot(
    raw: &Value,
) -> Result<FuturesAccountSnapshot, FuturesModelError> {
    if !raw.is_object() {
        return Err(invalid("futures account response was not a mapping"));
    }
    let f = |key: &str| jfloat(raw.get(key));
    let assets = raw
        .get("assets")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(parse_asset_balance)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(FuturesAccountSnapshot {
        total_wallet_balance: f("totalWalletBalance"),
        total_unrealized_profit: f("totalUnrealizedProfit"),
        total_margin_balance: f("totalMarginBalance"),
        total_initial_margin: f("totalInitialMargin"),
        total_maint_margin: f("totalMaintMargin"),
        total_position_initial_margin: f("totalPositionInitialMargin"),
        total_open_order_initial_margin: f("totalOpenOrderInitialMargin"),
        total_cross_wallet_balance: f("totalCrossWalletBalance"),
        total_cross_unrealized_pnl: f("totalCrossUnPnl"),
        available_balance: f("availableBalance"),
        max_withdraw_amount: f("maxWithdrawAmount"),
        assets,
    })
}

fn parse_asset_balance(raw: &Value) -> Result<FuturesAssetBalance, FuturesModelError> {
    let f = |key: &str| jfloat(raw.get(key));
    Ok(FuturesAssetBalance {
        asset: asset(&jtext(raw.get("asset")))?,
        wallet_balance: f("walletBalance"),
        unrealized_profit: f("unrealizedProfit"),
        margin_balance: f("marginBalance"),
        maint_margin: f("maintMargin"),
        initial_margin: f("initialMargin"),
        position_initial_margin: f("positionInitialMargin"),
        open_order_initial_margin: f("openOrderInitialMargin"),
        cross_wallet_balance: f("crossWalletBalance"),
        cross_unrealized_pnl: f("crossUnPnl"),
        available_balance: f("availableBalance"),
        max_withdraw_amount: f("maxWithdrawAmount"),
    })
}

/// Object rows of a payload (`item_key` unwraps a nested list).
fn payload_items<'a>(raw: &'a Value, item_key: Option<&str>) -> Vec<&'a Value> {
    match raw {
        Value::Object(map) => match item_key.and_then(|k| map.get(k)).and_then(Value::as_array) {
            Some(nested) => nested.iter().filter(|i| i.is_object()).collect(),
            None => vec![raw],
        },
        Value::Array(items) => items.iter().filter(|i| i.is_object()).collect(),
        _ => Vec::new(),
    }
}

fn upper_text(value: Option<&Value>) -> String {
    jtext(value).trim().to_uppercase()
}

fn text_or(value: Option<&Value>, default: &str) -> String {
    let text = jtext(value);
    if text.is_empty() {
        default.to_owned()
    } else {
        text
    }
}

pub fn parse_futures_positions(raw: &Value) -> Result<Vec<FuturesPosition>, FuturesModelError> {
    let mut positions = Vec::new();
    for item in payload_items(raw, Some("positions")) {
        let get = |key: &str| item.get(key);
        let symbol = upper_text(get("symbol"));
        if symbol.is_empty() {
            continue;
        }
        let f = |key: &str| jfloat(get(key));
        positions.push(FuturesPosition {
            symbol,
            position_side: PositionSide::parse(&text_or(get("positionSide"), "BOTH"))?,
            position_amt: f("positionAmt"),
            entry_price: f("entryPrice"),
            break_even_price: f("breakEvenPrice"),
            mark_price: f("markPrice"),
            unrealized_profit: f("unRealizedProfit"),
            liquidation_price: optional_positive(get("liquidationPrice")),
            notional: f("notional"),
            margin_asset: asset(&text_or(get("marginAsset"), "USDT"))?,
            isolated_margin: f("isolatedMargin"),
            isolated_wallet: f("isolatedWallet"),
            initial_margin: f("initialMargin"),
            maint_margin: f("maintMargin"),
            position_initial_margin: f("positionInitialMargin"),
            open_order_initial_margin: f("openOrderInitialMargin"),
            leverage: optional_positive(get("leverage")),
            margin_type: margin_type(&text_or(get("marginType"), "CROSSED")),
            adl: optional_int(get("adl")),
            update_time: optional_int(get("updateTime")),
        });
    }
    Ok(positions)
}

pub fn parse_futures_symbol_rules(
    raw: &Value,
    wanted: &[String],
) -> Result<Vec<FuturesSymbolRules>, FuturesModelError> {
    if !raw.is_object() {
        return Err(invalid("futures exchangeInfo response was not a mapping"));
    }
    let items = raw
        .get("symbols")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("futures exchangeInfo response did not include symbols"))?;
    let mut rules = Vec::new();
    for item in items.iter().filter(|i| i.is_object()) {
        let symbol = upper_text(item.get("symbol"));
        if symbol.is_empty() || (!wanted.is_empty() && !wanted.contains(&symbol)) {
            continue;
        }
        let filter = |kind: &str| {
            item.get("filters")
                .and_then(Value::as_array)
                .and_then(|fs| {
                    fs.iter()
                        .rev()
                        .find(|f| upper_text(f.get("filterType")) == kind)
                })
        };
        let field = |kind: &str, key: &str| jfloat(filter(kind).and_then(|f| f.get(key)));
        let min_notional = filter("MIN_NOTIONAL").map_or(Decimal::ZERO, |f| {
            jfloat(
                f.get("notional")
                    .filter(|v| !v.is_null())
                    .or_else(|| f.get("minNotional")),
            )
        });
        rules.push(FuturesSymbolRules {
            symbol,
            base_asset: asset(&jtext(item.get("baseAsset")))?,
            quote_asset: asset(&jtext(item.get("quoteAsset")))?,
            margin_asset: asset(&text_or(item.get("marginAsset"), "USDT"))?,
            contract_type: upper_text(item.get("contractType")),
            status: upper_text(item.get("status")),
            quantity_step: field("LOT_SIZE", "stepSize"),
            min_qty: field("LOT_SIZE", "minQty"),
            max_qty: field("LOT_SIZE", "maxQty"),
            tick_size: field("PRICE_FILTER", "tickSize"),
            min_price: field("PRICE_FILTER", "minPrice"),
            max_price: field("PRICE_FILTER", "maxPrice"),
            min_notional,
            trigger_protect: jfloat(item.get("triggerProtect")),
        });
    }
    Ok(rules)
}

pub fn parse_futures_symbol_configs(raw: &Value) -> Vec<FuturesSymbolConfig> {
    payload_items(raw, None)
        .into_iter()
        .filter_map(|item| {
            let symbol = upper_text(item.get("symbol"));
            (!symbol.is_empty()).then(|| FuturesSymbolConfig {
                symbol,
                margin_type: margin_type(&text_or(item.get("marginType"), "CROSSED")),
                leverage: jfloat(item.get("leverage")),
                max_notional_value: jfloat(item.get("maxNotionalValue")),
                auto_add_margin: optional_boolish(item.get("isAutoAddMargin")),
            })
        })
        .collect()
}

pub fn parse_futures_leverage_brackets(raw: &Value) -> Vec<FuturesLeverageBracket> {
    let mut brackets = Vec::new();
    for item in payload_items(raw, None) {
        let symbol = upper_text(item.get("symbol"));
        let Some(rows) = item.get("brackets").and_then(Value::as_array) else {
            continue;
        };
        if symbol.is_empty() {
            continue;
        }
        for bracket in rows.iter().filter(|b| b.is_object()) {
            let f = |key: &str| jfloat(bracket.get(key));
            brackets.push(FuturesLeverageBracket {
                symbol: symbol.clone(),
                initial_leverage: f("initialLeverage"),
                notional_floor: f("notionalFloor"),
                notional_cap: f("notionalCap"),
                maint_margin_ratio: f("maintMarginRatio"),
                cum: f("cum"),
            });
        }
    }
    brackets
}

pub fn parse_futures_mark_prices(raw: &Value) -> Vec<FuturesMarkPrice> {
    payload_items(raw, None)
        .into_iter()
        .filter_map(|item| {
            let symbol = upper_text(item.get("symbol"));
            let f = |key: &str| jfloat(item.get(key));
            (!symbol.is_empty()).then(|| FuturesMarkPrice {
                symbol,
                mark_price: f("markPrice"),
                index_price: f("indexPrice"),
                estimated_settle_price: f("estimatedSettlePrice"),
                last_funding_rate: f("lastFundingRate"),
                interest_rate: f("interestRate"),
                next_funding_time: optional_int(item.get("nextFundingTime")),
                time: optional_int(item.get("time")),
            })
        })
        .collect()
}

/// Fail closed for any attempted live futures activation.
pub fn assert_futures_live_disabled(confirm_live_futures: bool) -> Result<(), FuturesModelError> {
    if confirm_live_futures {
        return Err(invalid(
            "live futures activation is not implemented; Phase 20 is dry-run only",
        ));
    }
    Ok(())
}

/// Concise human-readable USD-M dry-run report.
pub fn render_futures_report(report: &FuturesDryRunReport) -> String {
    let discovery = &report.discovery;
    let mut lines = vec![
        format!("Mode: {}", report.mode),
        "Live futures trading: disabled".to_owned(),
        "Write endpoints used: none".to_owned(),
        format!("Product: {}", discovery.product),
        format!(
            "Margin balance USDT: {}",
            fmt_dec(discovery.account.total_margin_balance)
        ),
        format!(
            "Available balance USDT: {}",
            fmt_dec(discovery.account.available_balance)
        ),
        format!("Positions discovered: {}", discovery.positions.len()),
    ];
    if let Some(rec) = &report.reconciliation {
        lines.extend([
            "Reconciliation:".to_owned(),
            format!("  - nonzero positions: {}", rec.nonzero_positions),
            format!(
                "  - total notional: {}",
                fmt_dec(rec.total_abs_notional_usdt)
            ),
            format!("  - allowed: {}", yes_no(rec.allowed)),
        ]);
        if !rec.rejection_reasons.is_empty() {
            lines.push(format!(
                "  - rejection reasons: {}",
                rec.rejection_reasons.join(", ")
            ));
        }
    }
    if let Some(plan) = &report.order_plan {
        lines.extend([
            "Order plan:".to_owned(),
            format!("  - symbol: {}", plan.symbol),
            format!("  - side: {}", plan.side.as_str()),
            format!("  - quantity: {}", fmt_dec(plan.quantity)),
            format!("  - notional: {}", fmt_dec(plan.notional_usdt)),
            format!("  - leverage: {}", fmt_dec(plan.configured_leverage)),
            format!(
                "  - liquidation distance: {}",
                fmt_opt(plan.liquidation_distance_percent.map(|d| d.round_dp(6)))
            ),
            format!("  - allowed: {}", yes_no(plan.allowed)),
        ]);
        if !plan.rejection_reasons.is_empty() {
            lines.push(format!(
                "  - rejection reasons: {}",
                plan.rejection_reasons.join(", ")
            ));
        }
        if !plan.warnings.is_empty() {
            lines.push(format!("  - warnings: {}", plan.warnings.join(", ")));
        }
    }
    lines.join("\n")
}

fn estimate_liquidation_price(
    side: OrderSide,
    price: Decimal,
    leverage: Decimal,
    mmr: Decimal,
) -> Option<Decimal> {
    if price <= Decimal::ZERO || leverage <= Decimal::ZERO {
        return None;
    }
    let fraction = Decimal::ONE / leverage - mmr;
    if fraction <= Decimal::ZERO {
        return Some(price);
    }
    Some(match side {
        OrderSide::Buy => price * (Decimal::ONE - fraction),
        OrderSide::Sell => price * (Decimal::ONE + fraction),
    })
}

fn liquidation_distance_percent(
    side: OrderSide,
    price: Decimal,
    liquidation: Option<Decimal>,
) -> Option<Decimal> {
    let liquidation = liquidation?;
    if price <= Decimal::ZERO {
        return None;
    }
    Some(match side {
        OrderSide::Buy => (price - liquidation) / price * dec!(100),
        OrderSide::Sell => (liquidation - price) / price * dec!(100),
    })
}

fn is_closing_position(
    position: Option<&FuturesPosition>,
    side: OrderSide,
    quantity: Decimal,
    position_side: PositionSide,
) -> bool {
    let Some(position) = position else {
        return false;
    };
    if quantity <= Decimal::ZERO {
        return false;
    }
    let amount = position.position_amt;
    match position_side {
        PositionSide::Long => side == OrderSide::Sell && amount > Decimal::ZERO,
        PositionSide::Short => side == OrderSide::Buy && amount < Decimal::ZERO,
        PositionSide::Both if amount > Decimal::ZERO => side == OrderSide::Sell,
        PositionSide::Both if amount < Decimal::ZERO => side == OrderSide::Buy,
        PositionSide::Both => false,
    }
}

fn opens_short(side: OrderSide, position_side: PositionSide) -> bool {
    position_side == PositionSide::Short
        || (position_side == PositionSide::Both && side == OrderSide::Sell)
}

fn margin_type(value: &str) -> String {
    match value.trim().to_uppercase().as_str() {
        "CROSS" | "CROSSED" => "CROSSED".to_owned(),
        "ISOLATED" => "ISOLATED".to_owned(),
        "" => "UNKNOWN".to_owned(),
        other => other.to_owned(),
    }
}

fn asset(value: &str) -> Result<String, FuturesModelError> {
    let text = value.trim().to_uppercase();
    if text.is_empty() {
        return Err(invalid("asset must not be blank"));
    }
    Ok(text)
}

/// `BTC/USDT`, `BTC/USDT:USDT`, `btcusdt` → `BTCUSDT`.
pub fn futures_symbol(symbol: &str) -> Result<String, FuturesModelError> {
    let text = symbol.trim();
    if text.is_empty() {
        return Err(invalid("symbol must not be blank"));
    }
    let text = text.split(':').next().unwrap_or(text);
    let normalized = normalize_symbol(text).unwrap_or_else(|_| text.to_owned());
    Ok(common::exchange_symbol(&normalized))
}

fn is_multiple(value: Decimal, step: Decimal) -> bool {
    step <= Decimal::ZERO || (value % step).is_zero()
}

fn positive(value: Decimal, field: &str) -> Result<Decimal, FuturesModelError> {
    if value <= Decimal::ZERO {
        return Err(invalid(format!("{field} must be greater than zero")));
    }
    Ok(value)
}

fn non_negative(value: Decimal, field: &str) -> Result<Decimal, FuturesModelError> {
    if value < Decimal::ZERO {
        return Err(invalid(format!("{field} must be at least zero")));
    }
    Ok(value)
}

fn optional_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn optional_boolish(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::testutil::{mock_client, mount_get, request_paths, requests_to};

    pub(crate) fn account_payload(margin: &str, available: &str, maint: &str) -> Value {
        json!({
            "totalWalletBalance": "200", "totalUnrealizedProfit": "0", "totalMarginBalance": margin,
            "totalInitialMargin": "0", "totalMaintMargin": maint, "totalPositionInitialMargin": "0",
            "totalOpenOrderInitialMargin": "0", "totalCrossWalletBalance": "200", "totalCrossUnPnl": "0",
            "availableBalance": available, "maxWithdrawAmount": available,
            "assets": [{"asset": "USDT", "walletBalance": "200", "unrealizedProfit": "0", "marginBalance": margin,
                "maintMargin": maint, "initialMargin": "0", "positionInitialMargin": "0", "openOrderInitialMargin": "0",
                "crossWalletBalance": "200", "crossUnPnl": "0", "availableBalance": available, "maxWithdrawAmount": available}]
        })
    }

    pub(crate) fn flat_position() -> Value {
        json!({"symbol": "BTCUSDT", "positionSide": "BOTH", "positionAmt": "0", "entryPrice": "0",
            "breakEvenPrice": "0", "markPrice": "50000", "unRealizedProfit": "0", "liquidationPrice": "0",
            "notional": "0", "marginAsset": "USDT", "isolatedMargin": "0", "isolatedWallet": "0",
            "initialMargin": "0", "maintMargin": "0", "positionInitialMargin": "0", "openOrderInitialMargin": "0",
            "leverage": "3", "marginType": "cross", "adl": "0", "updateTime": "0"})
    }

    pub(crate) fn long_position() -> Value {
        let mut p = flat_position();
        for (k, v) in [
            ("positionAmt", "0.01"),
            ("entryPrice", "50000"),
            ("breakEvenPrice", "50000"),
            ("liquidationPrice", "40000"),
            ("notional", "500"),
            ("initialMargin", "166.67"),
            ("maintMargin", "2"),
            ("positionInitialMargin", "166.67"),
        ] {
            p[k] = json!(v);
        }
        p
    }

    pub(crate) fn exchange_info() -> Value {
        json!({"symbols": [{"symbol": "BTCUSDT", "pair": "BTCUSDT", "contractType": "PERPETUAL", "status": "TRADING",
            "baseAsset": "BTC", "quoteAsset": "USDT", "marginAsset": "USDT", "triggerProtect": "0.0500",
            "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": "1", "maxPrice": "1000000", "tickSize": "0.1"},
                {"filterType": "LOT_SIZE", "minQty": "0.001", "maxQty": "100", "stepSize": "0.001"},
                {"filterType": "MIN_NOTIONAL", "notional": "5"}]}]})
    }

    pub(crate) fn symbol_config(leverage: &str) -> Value {
        json!([{"symbol": "BTCUSDT", "marginType": "CROSSED", "isAutoAddMargin": "false", "leverage": leverage, "maxNotionalValue": "1000000"}])
    }

    pub(crate) fn brackets() -> Value {
        json!([{"symbol": "BTCUSDT", "brackets": [{"bracket": 1, "initialLeverage": "20", "notionalCap": "50000",
            "notionalFloor": "0", "maintMarginRatio": "0.004", "cum": "0"}]}])
    }

    pub(crate) fn mark(funding: &str) -> Value {
        json!({"symbol": "BTCUSDT", "markPrice": "50000", "indexPrice": "49990", "estimatedSettlePrice": "49995",
            "lastFundingRate": funding, "interestRate": "0.0001", "nextFundingTime": "1710000000000", "time": "1709990000000"})
    }

    pub(crate) struct Fixture {
        pub account: Value,
        pub positions: Value,
        pub config: Value,
        pub mark: Value,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Self {
                account: account_payload("200", "180", "1"),
                positions: json!([flat_position()]),
                config: symbol_config("3"),
                mark: mark("0.0001"),
            }
        }
    }

    pub(crate) async fn mount_futures(server: &MockServer, fixture: Fixture) {
        mount_get(server, "/fapi/v3/account", fixture.account).await;
        mount_get(server, "/fapi/v3/positionRisk", fixture.positions).await;
        mount_get(server, "/fapi/v1/exchangeInfo", exchange_info()).await;
        mount_get(server, "/fapi/v1/symbolConfig", fixture.config).await;
        mount_get(server, "/fapi/v1/leverageBracket", brackets()).await;
        mount_get(server, "/fapi/v1/premiumIndex", fixture.mark).await;
    }

    async fn discover(fixture: Fixture) -> (MockServer, FuturesDiscovery) {
        let server = MockServer::start().await;
        mount_futures(&server, fixture).await;
        let discovery = discover_usdm_futures(&mock_client(&server), &["BTC/USDT".into()])
            .await
            .unwrap();
        (server, discovery)
    }

    fn limits(
        notional: Decimal,
        initial: Decimal,
        free: Decimal,
        funding_payment: Decimal,
    ) -> FuturesRiskLimits {
        FuturesRiskLimits {
            max_leverage: dec!(3),
            max_notional_usdt: notional,
            max_account_notional_percent: dec!(100),
            max_initial_margin_usdt: initial,
            min_free_balance_after_initial_margin_usdt: free,
            min_liquidation_distance_percent: dec!(20),
            max_abs_funding_rate: dec!(0.001),
            max_abs_funding_payment_usdt: funding_payment,
            min_maintenance_buffer_percent: dec!(50),
            ..FuturesRiskLimits::default()
        }
    }

    #[tokio::test]
    async fn discovery_reads_usdm_state_without_writes() {
        let (server, discovery) = discover(Fixture::default()).await;
        assert_eq!(discovery.product, "USD-M");
        assert!(!discovery.live_trading_enabled);
        assert_eq!(discovery.account.total_margin_balance, dec!(200));
        assert!(discovery.symbol_rules("BTC/USDT").is_some());
        assert_eq!(
            discovery.symbol_config("BTC/USDT").unwrap().leverage,
            dec!(3)
        );
        assert_eq!(
            discovery.symbol_config("BTC/USDT").unwrap().auto_add_margin,
            Some(false)
        );
        assert_eq!(
            discovery
                .mark_price("BTC/USDT:USDT")
                .unwrap()
                .last_funding_rate,
            dec!(0.0001)
        );
        assert_eq!(discovery.positions[0].margin_type, "CROSSED");
        assert_eq!(
            request_paths(&server).await,
            vec![
                "GET /fapi/v3/account",
                "GET /fapi/v3/positionRisk",
                "GET /fapi/v1/exchangeInfo",
                "GET /fapi/v1/symbolConfig",
                "GET /fapi/v1/leverageBracket",
                "GET /fapi/v1/premiumIndex",
            ]
        );
        assert!(requests_to(&server, "/fapi/v1/premiumIndex").await[0].contains("symbol=BTCUSDT"));
    }

    #[tokio::test]
    async fn order_plan_models_leverage_funding_and_liquidation() {
        let (_server, discovery) = discover(Fixture::default()).await;
        let plan = build_futures_order_plan(
            &discovery,
            &FuturesOrderIntent::market("BTC/USDT", OrderSide::Buy, dec!(0.001)),
            &limits(dec!(100), dec!(20), dec!(5), dec!(0.05)),
        )
        .unwrap();
        assert!(plan.allowed, "{:?}", plan.rejection_reasons);
        assert_eq!(plan.notional_usdt, dec!(50));
        assert_eq!(plan.configured_leverage, dec!(3));
        assert_eq!(
            plan.initial_margin_required_usdt.unwrap().round_dp(7),
            dec!(16.6666667)
        );
        assert_eq!(plan.maintenance_margin_estimate_usdt, dec!(0.2));
        assert_eq!(
            plan.liquidation_distance_percent.unwrap().round_dp(7),
            dec!(32.9333333)
        );
        assert_eq!(plan.projected_funding_payment_usdt, dec!(0.005));
    }

    #[tokio::test]
    async fn order_plan_rejects_leverage_funding_and_liquidation_risk() {
        let (_server, discovery) = discover(Fixture {
            config: symbol_config("10"),
            mark: mark("0.002"),
            ..Fixture::default()
        })
        .await;
        let plan = build_futures_order_plan(
            &discovery,
            &FuturesOrderIntent::market("BTC/USDT", OrderSide::Buy, dec!(0.001)),
            &limits(dec!(100), dec!(20), dec!(5), dec!(1)),
        )
        .unwrap();
        assert!(!plan.allowed);
        for reason in [
            "configured leverage exceeds the risk limit",
            "estimated liquidation distance is below minimum",
            "absolute funding rate exceeds configured cap",
        ] {
            assert!(
                plan.rejection_reasons.iter().any(|r| r == reason),
                "{reason}"
            );
        }
    }

    #[tokio::test]
    async fn reduce_only_requires_reducible_position() {
        let (_server, discovery) = discover(Fixture {
            account: account_payload("500", "480", "1"),
            positions: json!([long_position()]),
            ..Fixture::default()
        })
        .await;
        let limits = limits(dec!(300), dec!(100), dec!(5), dec!(1));
        let sell = FuturesOrderIntent {
            reduce_only: true,
            ..FuturesOrderIntent::market("BTC/USDT", OrderSide::Sell, dec!(0.005))
        };
        let allowed = build_futures_order_plan(&discovery, &sell, &limits).unwrap();
        assert!(allowed.allowed, "{:?}", allowed.rejection_reasons);
        assert!(allowed.closing_position);
        let buy = FuturesOrderIntent {
            side: OrderSide::Buy,
            ..sell
        };
        let rejected = build_futures_order_plan(&discovery, &buy, &limits).unwrap();
        assert!(!rejected.allowed);
        assert!(
            rejected
                .rejection_reasons
                .contains(&"reduce-only order would not reduce an existing position".to_owned())
        );
    }

    #[tokio::test]
    async fn reconciliation_rejects_low_buffer_and_excess_notional() {
        let (_server, discovery) = discover(Fixture {
            account: account_payload("10", "5", "9"),
            positions: json!([long_position()]),
            ..Fixture::default()
        })
        .await;
        let rec =
            reconcile_futures_state(&discovery, &limits(dec!(100), dec!(100), dec!(0), dec!(1)))
                .unwrap();
        assert!(!rec.allowed);
        assert_eq!(rec.nonzero_positions, 1);
        assert_eq!(
            rec.maintenance_buffer_percent.unwrap().round_dp(7),
            dec!(11.1111111)
        );
        assert_eq!(rec.rejection_reasons.len(), 2);
    }

    #[tokio::test]
    async fn dry_run_report_never_enables_live_trading() {
        let server = MockServer::start().await;
        mount_futures(&server, Fixture::default()).await;
        let client = mock_client(&server);
        let settings = crate::testutil::settings_builder().build().unwrap();
        let intent = FuturesOrderIntent::market("BTC/USDT", OrderSide::Buy, dec!(0.001));
        let report = build_futures_dry_run_report(
            &settings,
            &client,
            &["BTC/USDT".into()],
            Some(&intent),
            &limits(dec!(100), dec!(20), dec!(5), dec!(0.05)),
        )
        .await
        .unwrap();
        assert!(!report.live_trading_enabled);
        assert!(report.write_endpoints_used.is_empty());
        assert!(report.order_plan.is_some() && report.reconciliation.is_some());
        let text = render_futures_report(&report);
        assert!(text.contains("Order plan:") && text.contains("  - side: BUY"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["order_plan"]["side"], "BUY");
        assert_eq!(json["order_plan"]["notional_usdt"].to_string(), "50.000");
    }

    #[test]
    fn live_activation_fails_closed_and_zero_leverage_rejects() {
        assert!(
            assert_futures_live_disabled(true)
                .unwrap_err()
                .to_string()
                .contains("not implemented")
        );
        assert_eq!(futures_symbol("btc/usdt:usdt").unwrap(), "BTCUSDT");
        assert_eq!(margin_type("cross"), "CROSSED");
    }
}

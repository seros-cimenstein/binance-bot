//! Port of `StrategyOrderClient` from `src/main.py`: routes strategy orders
//! through sizing, dry-run, risk validation and the order manager.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bb_core::Decimal;
use parking_lot::Mutex;

use crate::alerts::TelegramAlerter;
use crate::capital::{BuyLease, CapitalGate, CapitalGateError, module_usage};
use crate::orders::models::{Order, OrderSide};
use crate::orders::sizing::{
    OrderSizer, OrderSizingConfig, OrderSizingError, OrderSizingRequest, SizedOrder,
};
use crate::orders::{OrderManager, OrderManagerError, OrderStatus, PlaceOrder};
use crate::portfolio::{PortfolioError, PortfolioSource};
use crate::risk::{MarketData, OrderCheck, OrderRequest, RiskBreachError, RiskGuard};
use crate::strategies::{OrderClient, OrderResult, StrategyError, StrategyOrder};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Sizing(#[from] OrderSizingError),
    #[error(transparent)]
    Risk(#[from] RiskBreachError),
    #[error(transparent)]
    Portfolio(#[from] PortfolioError),
    #[error(transparent)]
    Order(#[from] OrderManagerError),
    #[error(transparent)]
    Capital(#[from] CapitalGateError),
}

/// Capital pool / auto-convert attachment for one client.
struct CapitalHook {
    gate: Arc<CapitalGate>,
    module_id: String,
    module_ids: Vec<String>,
}

pub struct StrategyOrderClient {
    order_manager: Arc<OrderManager>,
    risk_guard: Option<Arc<RiskGuard>>,
    alerter: Option<Arc<TelegramAlerter>>,
    dry_run: bool,
    sizing_config: OrderSizingConfig,
    portfolio: Option<Arc<dyn PortfolioSource>>,
    sizer: OrderSizer,
    last_price: Mutex<Option<Decimal>>,
    capital: Option<CapitalHook>,
}

impl std::fmt::Debug for StrategyOrderClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrategyOrderClient")
            .field("dry_run", &self.dry_run)
            .field("sizing_config", &self.sizing_config)
            .finish_non_exhaustive()
    }
}

impl StrategyOrderClient {
    pub fn new(order_manager: Arc<OrderManager>) -> Self {
        Self {
            order_manager,
            risk_guard: None,
            alerter: None,
            dry_run: false,
            sizing_config: OrderSizingConfig::default(),
            portfolio: None,
            sizer: OrderSizer,
            last_price: Mutex::new(None),
            capital: None,
        }
    }

    pub fn with_risk_guard(mut self, guard: Arc<RiskGuard>) -> Self {
        self.risk_guard = Some(guard);
        self
    }

    pub fn with_alerter(mut self, alerter: Option<Arc<TelegramAlerter>>) -> Self {
        self.alerter = alerter;
        self
    }

    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn with_sizing_config(mut self, config: OrderSizingConfig) -> Self {
        self.sizing_config = config;
        self
    }

    pub fn with_portfolio(mut self, portfolio: Arc<dyn PortfolioSource>) -> Self {
        self.portfolio = Some(portfolio);
        self
    }

    /// Request capital leases (module `module_id`, competing with
    /// `module_ids`) and auto-convert quote before buys.
    pub fn with_capital(
        mut self,
        gate: Arc<CapitalGate>,
        module_id: &str,
        module_ids: Vec<String>,
    ) -> Self {
        self.capital = Some(CapitalHook {
            gate,
            module_id: module_id.to_owned(),
            module_ids,
        });
        self
    }

    /// Lease-adjusted amount after any quote auto-conversion (buys only).
    async fn apply_capital(
        &self,
        symbol: &str,
        side: OrderSide,
        amount: Decimal,
        price: Decimal,
        sized: Option<&SizedOrder>,
    ) -> Result<Decimal, ClientError> {
        let Some(hook) = &self.capital else {
            return Ok(amount);
        };
        if side != OrderSide::Buy {
            return Ok(amount);
        }
        let mut amount = amount;
        if hook.gate.pool_enabled() {
            let total_equity = self
                .capital_equity(symbol, price, amount * price, sized)
                .await;
            let usage = module_usage(&self.order_manager.list_orders(None)?, &hook.module_id);
            amount = hook
                .gate
                .adjust_buy(&BuyLease {
                    module_id: &hook.module_id,
                    symbol,
                    side,
                    amount,
                    price,
                    total_equity_usdt: total_equity,
                    module_ids: &hook.module_ids,
                    module_usage_usdt: &usage,
                })
                .await?;
        }
        let min_quote = self
            .risk_guard
            .as_ref()
            .map(|g| g.config().min_balance_usdt)
            .unwrap_or_default();
        hook.gate
            .ensure_quote(symbol, side, amount, price, min_quote)
            .await?;
        Ok(amount)
    }

    /// Pool equity: the portfolio's total equity, else the requested notional.
    async fn capital_equity(
        &self,
        symbol: &str,
        price: Decimal,
        requested: Decimal,
        sized: Option<&SizedOrder>,
    ) -> Decimal {
        if let Some(portfolio) = sized.and_then(|s| s.portfolio.as_ref()) {
            return Some(portfolio.total_equity_usdt)
                .filter(|e| *e > Decimal::ZERO)
                .unwrap_or(requested);
        }
        if let Some(service) = &self.portfolio {
            match service
                .snapshot(&HashMap::from([(symbol.to_owned(), price)]))
                .await
            {
                Ok(snapshot) if snapshot.total_equity_usdt > Decimal::ZERO => {
                    return snapshot.total_equity_usdt;
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(symbol, error = %err, "Capital pool equity snapshot failed; using fallback")
                }
            }
        }
        requested
    }

    pub fn last_price(&self) -> Option<Decimal> {
        *self.last_price.lock()
    }

    /// Validate, optionally simulate, and submit an order.
    pub async fn submit(&self, request: StrategyOrder) -> Result<OrderResult, ClientError> {
        let StrategyOrder {
            symbol,
            order_type,
            side,
            mut amount,
            price,
            size_order,
        } = request;
        let last_price = self.last_price();
        let validation_price = price.or(last_price);
        let market_price = last_price.or(validation_price);

        let mut sized = None;
        if size_order {
            let sizing_request = OrderSizingRequest {
                symbol: symbol.clone(),
                side,
                requested_amount: amount,
                price: validation_price,
            };
            let exchange = self.order_manager.exchange().as_ref();
            match self
                .sizer
                .size_order(
                    &sizing_request,
                    &self.sizing_config,
                    self.portfolio.as_deref(),
                    Some(exchange),
                )
                .await
            {
                Ok(order) => {
                    amount = order.amount;
                    sized = Some(order);
                }
                Err(err) => {
                    if let Some(alerter) = &self.alerter {
                        alerter.alert_risk_breach(&err.to_string()).await;
                    }
                    return Err(err.into());
                }
            }
        }

        // Unsized orders are position closes (rollback): never gated by capital.
        if size_order && let Some(price) = validation_price {
            match self
                .apply_capital(&symbol, side, amount, price, sized.as_ref())
                .await
            {
                Ok(adjusted) => amount = adjusted,
                Err(err) => {
                    if let (
                        ClientError::Capital(CapitalGateError::Blocked(breach)),
                        Some(alerter),
                    ) = (&err, &self.alerter)
                    {
                        alerter.alert_risk_breach(&breach.to_string()).await;
                    }
                    return Err(err);
                }
            }
        }

        if self.dry_run {
            tracing::info!(%symbol, order_type = %order_type, side = %side, %amount, price = ?validation_price, "DRY RUN: order not submitted");
            if let Some(alerter) = &self.alerter {
                alerter
                    .alert_trade(side, &symbol, amount, validation_price, true)
                    .await;
            }
            let id = uuid::Uuid::new_v4().simple().to_string();
            return Ok(OrderResult {
                id: format!("dry-{}", &id[..8]),
                symbol,
                order_type,
                side,
                amount,
                price: validation_price,
                status: OrderStatus::Closed,
                filled: amount,
                remaining: Decimal::ZERO,
                average: validation_price,
            });
        }

        if let Some(guard) = &self.risk_guard {
            match validation_price {
                Some(order_price) => {
                    let check = RiskInputs {
                        symbol: &symbol,
                        side: side.as_str(),
                        order_type: order_type.as_str(),
                        amount,
                        order_price,
                        market_price: market_price.unwrap_or(order_price),
                    };
                    if let Err(err) = self
                        .validate_risk(guard, &check, sized.as_ref(), size_order)
                        .await
                    {
                        if let (ClientError::Risk(breach), Some(alerter)) = (&err, &self.alerter) {
                            alerter.alert_risk_breach(&breach.to_string()).await;
                        }
                        return Err(err);
                    }
                }
                None => tracing::warn!("RiskGuard skipped: no market price available yet"),
            }
        }

        let order = self
            .order_manager
            .place_order(PlaceOrder {
                symbol: symbol.clone(),
                order_type,
                side,
                amount,
                price,
                client_order_id: None,
            })
            .await?;
        if let Some(alerter) = &self.alerter {
            let filled = if order.filled_amount.is_zero() {
                amount
            } else {
                order.filled_amount
            };
            alerter
                .alert_trade(side, &symbol, filled, order.avg_fill_price.or(price), false)
                .await;
        }
        Ok(order_result(order))
    }

    async fn validate_risk(
        &self,
        guard: &RiskGuard,
        inputs: &RiskInputs<'_>,
        sized: Option<&SizedOrder>,
        allow_structured: bool,
    ) -> Result<(), ClientError> {
        let sized_portfolio = sized.and_then(|s| s.portfolio.as_ref());
        if allow_structured
            && (sized_portfolio.is_some() || guard.config().uses_structured_checks())
        {
            let portfolio = match (sized_portfolio, &self.portfolio) {
                (Some(portfolio), _) => Some(portfolio.clone()),
                (None, Some(service)) => {
                    let marks = HashMap::from([(inputs.symbol.to_owned(), inputs.market_price)]);
                    Some(service.snapshot(&marks).await?)
                }
                (None, None) => None,
            };
            if let Some(portfolio) = portfolio {
                let order = OrderRequest {
                    order_type: inputs.order_type.to_owned(),
                    ..OrderRequest::new(
                        inputs.symbol,
                        inputs.side,
                        inputs.amount,
                        inputs.order_price,
                    )
                };
                let market = MarketData {
                    symbol: inputs.symbol.to_owned(),
                    mark_price: inputs.market_price,
                };
                guard.validate_structured(&order, &portfolio.to_risk_state(), &market)?;
                return Ok(());
            }
        }

        let current_position = self
            .order_manager
            .get_position(inputs.symbol)?
            .map(|p| p.amount)
            .unwrap_or_default();
        guard.validate_order(&OrderCheck {
            symbol: inputs.symbol.to_owned(),
            side: inputs.side.to_owned(),
            amount: inputs.amount,
            price: inputs.order_price,
            current_position,
        })?;
        Ok(())
    }
}

struct RiskInputs<'a> {
    symbol: &'a str,
    side: &'a str,
    order_type: &'a str,
    amount: Decimal,
    order_price: Decimal,
    market_price: Decimal,
}

#[async_trait]
impl OrderClient for StrategyOrderClient {
    async fn create_order(&self, order: StrategyOrder) -> Result<OrderResult, StrategyError> {
        self.submit(order)
            .await
            .map_err(|e| StrategyError::Order(e.to_string()))
    }

    fn update_last_price(&self, price: Decimal) {
        *self.last_price.lock() = Some(price);
    }
}

/// ccxt-like order mapping from an order manager snapshot.
pub fn order_result(order: Order) -> OrderResult {
    OrderResult {
        id: order.id,
        symbol: order.symbol,
        order_type: order.order_type,
        side: order.side,
        amount: order.amount,
        price: order.price,
        status: order.status,
        filled: order.filled_amount,
        remaining: order.remaining_amount,
        average: order.avg_fill_price,
    }
}

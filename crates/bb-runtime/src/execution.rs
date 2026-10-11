//! Port of `src/execution/router.py`: apply execution reports to local order
//! state and dispatch newly observed fills to the matching strategy.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bb_binance::normalize_symbol;
use bb_binance::ws::events::ExecutionReport;
use bb_core::Decimal;
use parking_lot::Mutex;

use crate::orders::models::{Fill, Order};
use crate::orders::payload::{FillPayload, OrderPayload, datetime_from_ms};
use crate::orders::{OrderManager, OrderManagerError};
use crate::strategies::{FillData, Strategy};

pub struct ExecutionUpdateRouter {
    order_manager: Arc<OrderManager>,
    strategies: HashMap<String, Arc<dyn Strategy>>,
    notified: Mutex<HashSet<(String, String)>>,
}

impl std::fmt::Debug for ExecutionUpdateRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionUpdateRouter")
            .field("symbols", &self.strategies.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ExecutionUpdateRouter {
    pub fn new(
        order_manager: Arc<OrderManager>,
        strategies: HashMap<String, Arc<dyn Strategy>>,
    ) -> Self {
        Self {
            order_manager,
            strategies,
            notified: Mutex::new(HashSet::new()),
        }
    }

    /// Apply an execution report and notify the strategy of new fills.
    pub async fn route(&self, update: &ExecutionReport) -> Result<Order, OrderManagerError> {
        let order = self
            .order_manager
            .apply_order_update(&execution_report_to_order_payload(update))?;
        let Some(strategy) = self.strategies.get(&order.symbol) else {
            return Ok(order);
        };
        let fresh: Vec<&Fill> = {
            let mut notified = self.notified.lock();
            order
                .fills
                .iter()
                .filter(|fill| notified.insert((fill.order_id.clone(), fill.id.clone())))
                .collect()
        };
        for fill in fresh {
            let data = fill_to_strategy_fill(fill);
            match data.validate() {
                Ok(()) => strategy.on_fill(data).await,
                Err(err) => {
                    tracing::warn!(order_id = %fill.order_id, fill_id = %fill.id, error = %err, "Skipping invalid fill notification")
                }
            }
        }
        Ok(order)
    }
}

/// Convert a Binance execution report into an [`OrderManager`] payload.
///
/// The report's raw symbol (`BTCUSDT`) is normalised to the unified form
/// (`BTC/USDT`) the runtime uses for orders, positions and strategies.
pub fn execution_report_to_order_payload(update: &ExecutionReport) -> OrderPayload {
    let positive = |v: Option<Decimal>| v.filter(|d| !d.is_zero());
    let amount = positive(update.order_quantity)
        .or(positive(update.cumulative_filled_quantity))
        .unwrap_or_default();
    let filled = positive(update.cumulative_filled_quantity).unwrap_or_default();
    let average = match update.cumulative_quote_transacted {
        Some(quote) if filled > Decimal::ZERO => Some(quote / filled),
        _ => None,
    };
    let symbol = unified(&update.symbol);
    let timestamp_ms = update
        .order_creation_time
        .filter(|t| *t != 0)
        .unwrap_or(update.event_time);
    OrderPayload {
        id: Some(update.order_id.to_string()),
        symbol: Some(symbol.clone()),
        order_type: Some(update.order_type.to_lowercase()),
        side: Some(update.side.to_lowercase()),
        amount: Some(amount),
        price: positive(update.order_price).or(update.last_executed_price),
        filled: Some(filled),
        remaining: Some((amount - filled).max(Decimal::ZERO)),
        average,
        cost: None,
        status: Some(update.order_status.to_lowercase()).filter(|s| !s.is_empty()),
        timestamp: datetime_from_ms(timestamp_ms),
        fills: last_fill_payload(update, &symbol).into_iter().collect(),
    }
}

fn last_fill_payload(update: &ExecutionReport, symbol: &str) -> Option<FillPayload> {
    let quantity = update
        .last_executed_quantity
        .filter(|q| *q > Decimal::ZERO)?;
    let price = update.last_executed_price.filter(|p| *p > Decimal::ZERO)?;
    let id = match update.trade_id {
        Some(id) if id >= 0 => id.to_string(),
        _ => format!("{}:{}", update.order_id, update.event_time),
    };
    let trade_time = update
        .trade_time
        .filter(|t| *t != 0)
        .unwrap_or(update.event_time);
    Some(FillPayload {
        id: Some(id),
        symbol: Some(symbol.to_owned()),
        side: Some(update.side.to_lowercase()),
        price: Some(price),
        amount: Some(quantity),
        fee: update.commission_amount.unwrap_or_default(),
        fee_currency: update.commission_asset.clone().unwrap_or_default(),
        timestamp: datetime_from_ms(trade_time),
    })
}

fn unified(symbol: &str) -> String {
    normalize_symbol(symbol).unwrap_or_else(|_| symbol.trim().to_owned())
}

fn fill_to_strategy_fill(fill: &Fill) -> FillData {
    FillData {
        order_id: fill.order_id.clone(),
        symbol: fill.symbol.clone(),
        side: fill.side,
        price: fill.price,
        quantity: fill.amount,
        commission: fill.fee,
        timestamp: fill.timestamp.and_utc().timestamp_millis(),
    }
}

//! Port of `src/portfolio/snapshot.py`: account + local state snapshots for
//! dynamic order sizing and structured risk validation.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use bb_core::Decimal;
use rust_decimal_macros::dec;

use crate::exchange::{Exchange, ExchangeError};
use crate::orders::OrderManager;
use crate::risk::{PortfolioState, RiskGuard};

pub const DEFAULT_QUOTE_ASSET: &str = "USDT";

/// Combined account balance, position, order and PnL state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortfolioSnapshot {
    pub balances_free: BTreeMap<String, Decimal>,
    pub balances_used: BTreeMap<String, Decimal>,
    pub balances_total: BTreeMap<String, Decimal>,
    pub available_balance_usdt: Decimal,
    pub total_balance_usdt: Decimal,
    pub total_equity_usdt: Decimal,
    pub realized_pnl_today_usdt: Decimal,
    pub unrealized_pnl_today_usdt: Decimal,
    /// Signed USDT exposure per symbol.
    pub positions: BTreeMap<String, Decimal>,
    pub total_exposure_usdt: Decimal,
    pub open_orders_count: usize,
}

impl PortfolioSnapshot {
    /// The snapshot in the structured shape accepted by [`RiskGuard`].
    pub fn to_risk_state(&self) -> PortfolioState {
        PortfolioState {
            total_equity_usdt: self.total_equity_usdt,
            available_balance_usdt: self.available_balance_usdt,
            realized_pnl_today_usdt: self.realized_pnl_today_usdt,
            unrealized_pnl_today_usdt: self.unrealized_pnl_today_usdt,
            positions: self.positions.clone(),
            total_exposure_usdt: self.total_exposure_usdt,
            open_orders_count: self.open_orders_count,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PortfolioError {
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
    #[error("{0}")]
    Other(String),
}

/// Anything that can produce a portfolio snapshot (the sizer's dependency).
#[async_trait]
pub trait PortfolioSource: Send + Sync {
    async fn snapshot(
        &self,
        mark_prices: &HashMap<String, Decimal>,
    ) -> Result<PortfolioSnapshot, PortfolioError>;
}

/// Builds snapshots from exchange balances and local order/risk state.
#[derive(Debug, Clone)]
pub struct PortfolioSnapshotService {
    order_manager: Arc<OrderManager>,
    risk_guard: Option<Arc<RiskGuard>>,
    quote_asset: String,
}

impl PortfolioSnapshotService {
    pub fn new(order_manager: Arc<OrderManager>, risk_guard: Option<Arc<RiskGuard>>) -> Self {
        Self {
            order_manager,
            risk_guard,
            quote_asset: DEFAULT_QUOTE_ASSET.to_owned(),
        }
    }

    pub fn with_quote_asset(mut self, quote_asset: &str) -> Self {
        let asset = quote_asset.trim().to_uppercase();
        self.quote_asset = if asset.is_empty() {
            DEFAULT_QUOTE_ASSET.to_owned()
        } else {
            asset
        };
        self
    }

    fn exchange(&self) -> &Arc<dyn Exchange> {
        self.order_manager.exchange()
    }
}

#[async_trait]
impl PortfolioSource for PortfolioSnapshotService {
    async fn snapshot(
        &self,
        mark_prices: &HashMap<String, Decimal>,
    ) -> Result<PortfolioSnapshot, PortfolioError> {
        let balances = self.exchange().balances().await?;
        let available = balances
            .free
            .get(&self.quote_asset)
            .copied()
            .unwrap_or_default();
        let total_quote = balances
            .total
            .get(&self.quote_asset)
            .copied()
            .unwrap_or(available);

        let mut positions = BTreeMap::new();
        let mut unrealized = Decimal::ZERO;
        for position in self.order_manager.list_positions() {
            let symbol = position.symbol.trim().to_owned();
            if symbol.is_empty() {
                continue;
            }
            let mark = mark_prices
                .get(&symbol)
                .copied()
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(position.entry_price);
            let exposure = position.amount * mark;
            if exposure.abs() > dec!(0.000000000001) {
                positions.insert(symbol, exposure);
            }
            unrealized += position.unrealized_pnl;
        }
        let total_exposure: Decimal = positions.values().map(|v| v.abs()).sum();
        let realized = -self
            .risk_guard
            .as_ref()
            .map(|g| g.current_daily_loss())
            .unwrap_or_default();
        let total_equity = total_quote + total_exposure + unrealized;
        let open_orders_count = self
            .order_manager
            .list_open_orders(None)
            .map(|orders| orders.len())
            .unwrap_or_default();

        Ok(PortfolioSnapshot {
            balances_free: balances.free,
            balances_used: balances.used,
            balances_total: balances.total,
            available_balance_usdt: available,
            total_balance_usdt: total_quote,
            total_equity_usdt: total_equity.max(Decimal::ZERO),
            realized_pnl_today_usdt: realized,
            unrealized_pnl_today_usdt: unrealized,
            positions,
            total_exposure_usdt: total_exposure,
            open_orders_count,
        })
    }
}

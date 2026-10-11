//! Live execution layer for the funding-rate harvest runtime (port of
//! `src/funding_arb_executor.py`).
//!
//! - [`FundingArbExecutor::open_position`]: top up futures collateral, spot BUY, then
//!   perp SHORT; a failed perp leg immediately sells the spot back ("no orphan leg").
//! - [`FundingArbExecutor::close_position`]: perp BUY (reduce-only) first, then spot SELL,
//!   then sweep collateral back (best effort).
//! - [`FundingArbExecutor::reconcile`]: compare exchange balances/positions with local
//!   state; the runtime halts on any mismatch.
//!
//! Exchange access goes through the [`SpotVenue`], [`FuturesVenue`] and
//! [`CollateralTransfer`] traits; [`BinanceVenue`] implements all three.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use bb_binance::fills::FillSummary;
use bb_binance::futures::FuturesNewOrder;
use bb_binance::spot::NewOrder;
use bb_binance::wallet::UniversalTransfer;
use bb_binance::{BinanceClient, Side, exchange_id};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::error::{Result, StrategyError};
use crate::funding_arb::FundingArbPosition;

fn fa_err(message: impl Into<String>) -> StrategyError {
    StrategyError::FundingArb(message.into())
}

/// Normalised fill of one market order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LegFill {
    /// Base quantity filled (for spot BUYs: net of any base-asset commission).
    pub filled_qty: Decimal,
    pub avg_price: Decimal,
    /// Quote value of the fill.
    pub cost_usdt: Decimal,
    pub order_id: String,
}

#[async_trait]
pub trait SpotVenue: Send + Sync {
    /// Last traded price.
    async fn last_price(&self, symbol: &str) -> Result<Decimal>;
    /// MARKET order for `quantity` base units (rounded down to the lot step).
    async fn market_order(&self, symbol: &str, side: Side, quantity: Decimal) -> Result<LegFill>;
    /// `asset → free` balances.
    async fn free_balances(&self) -> Result<BTreeMap<String, Decimal>>;
}

#[async_trait]
pub trait FuturesVenue: Send + Sync {
    /// USD-M MARKET order (one-way mode) for `quantity` contracts.
    async fn market_order(
        &self,
        symbol: &str,
        side: Side,
        quantity: Decimal,
        reduce_only: bool,
    ) -> Result<LegFill>;
    /// Available USDT in the USD-M wallet.
    async fn free_usdt(&self) -> Result<Decimal>;
    /// Signed position size keyed by exchange id (`BTCUSDT`); negative = short.
    async fn positions(&self) -> Result<HashMap<String, Decimal>>;
}

#[async_trait]
pub trait CollateralTransfer: Send + Sync {
    async fn transfer_spot_to_futures(&self, amount_usdt: Decimal) -> Result<()>;
    async fn transfer_futures_to_spot(&self, amount_usdt: Decimal) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionLeg {
    /// `spot` | `futures`.
    pub venue: &'static str,
    /// `buy` | `sell`.
    pub side: &'static str,
    pub symbol: String,
    pub requested_qty: Decimal,
    pub filled_qty: Decimal,
    pub avg_price: Decimal,
    pub cost_usdt: Decimal,
    pub order_id: String,
}

impl ExecutionLeg {
    fn new(
        venue: &'static str,
        side: &'static str,
        symbol: &str,
        requested_qty: Decimal,
        fill: LegFill,
    ) -> Self {
        Self {
            venue,
            side,
            symbol: symbol.to_owned(),
            requested_qty,
            filled_qty: fill.filled_qty,
            avg_price: fill.avg_price,
            cost_usdt: fill.cost_usdt,
            order_id: fill.order_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenResult {
    pub spot_leg: ExecutionLeg,
    pub perp_leg: ExecutionLeg,
    pub position: FundingArbPosition,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CloseResult {
    pub perp_leg: ExecutionLeg,
    pub spot_leg: ExecutionLeg,
    pub realized_pnl_usdt: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationMismatch {
    pub symbol: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationReport {
    pub spot_balances: BTreeMap<String, Decimal>,
    pub futures_positions: HashMap<String, Decimal>,
    pub mismatches: Vec<ReconciliationMismatch>,
}

impl ReconciliationReport {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Dual-leg open/close with rollback + reconciliation.
pub struct FundingArbExecutor {
    pub spot: Box<dyn SpotVenue>,
    pub futures: Box<dyn FuturesVenue>,
    pub collateral: Box<dyn CollateralTransfer>,
    pub leverage: u32,
}

impl FundingArbExecutor {
    pub fn new(
        spot: Box<dyn SpotVenue>,
        futures: Box<dyn FuturesVenue>,
        collateral: Box<dyn CollateralTransfer>,
    ) -> Self {
        Self {
            spot,
            futures,
            collateral,
            leverage: 1,
        }
    }

    /// Executor wired to real Binance Spot + USD-M futures through one client.
    pub fn binance(client: BinanceClient) -> Self {
        let venue = BinanceVenue::new(client);
        Self::new(
            Box::new(venue.clone()),
            Box::new(venue.clone()),
            Box::new(venue),
        )
    }

    fn collateral_for(&self, notional_usdt: Decimal) -> Decimal {
        notional_usdt / Decimal::from(self.leverage.max(1))
    }

    /// Open a delta-neutral position: spot BUY then perp SHORT, with rollback.
    pub async fn open_position(
        &self,
        symbol: &str,
        notional_usdt: Decimal,
        entry_funding_bps: f64,
        open_time_ms: i64,
    ) -> Result<OpenResult> {
        if notional_usdt <= Decimal::ZERO {
            return Err(fa_err("notional_usdt must be positive"));
        }
        let spot_price = self.spot.last_price(symbol).await?;
        if spot_price <= Decimal::ZERO {
            return Err(fa_err(format!("spot price unavailable for {symbol}")));
        }
        let requested_qty = notional_usdt / spot_price;

        // Top up futures collateral (notional / leverage) before placing the perp.
        self.collateral
            .transfer_spot_to_futures(self.collateral_for(notional_usdt))
            .await?;

        // Leg 1: spot BUY.
        let spot_fill = self
            .spot
            .market_order(symbol, Side::Buy, requested_qty)
            .await?;
        if spot_fill.filled_qty <= Decimal::ZERO {
            return Err(fa_err(format!("spot leg returned zero fill on {symbol}")));
        }
        let mut spot_leg = ExecutionLeg::new("spot", "buy", symbol, requested_qty, spot_fill);
        if spot_leg.avg_price.is_zero() {
            spot_leg.avg_price = spot_price;
        }
        let spot_filled = spot_leg.filled_qty;

        // Leg 2: perp SHORT for the filled spot quantity.
        let perp_fill = match self
            .futures
            .market_order(symbol, Side::Sell, spot_filled, false)
            .await
        {
            Ok(fill) => fill,
            Err(err) => {
                self.rollback_open_after_perp_failure(symbol, spot_filled)
                    .await?;
                return Err(fa_err(format!(
                    "perp short failed for {symbol} ({err}); spot leg rolled back"
                )));
            }
        };
        if perp_fill.filled_qty <= Decimal::ZERO {
            self.rollback_open_after_perp_failure(symbol, spot_filled)
                .await?;
            return Err(fa_err(format!(
                "perp short returned zero fill on {symbol}; spot leg rolled back"
            )));
        }
        let mut perp_leg = ExecutionLeg::new("futures", "sell", symbol, spot_filled, perp_fill);
        if perp_leg.avg_price.is_zero() {
            perp_leg.avg_price = spot_price;
        }
        let position = FundingArbPosition {
            symbol: symbol.to_owned(),
            open_time_ms,
            entry_funding_bps,
            notional_usdt,
            spot_qty: spot_filled,
            perp_qty: -perp_leg.filled_qty,
            funding_received_bps: 0.0,
            funding_received_usdt: Decimal::ZERO,
            events_held: 0,
        };
        Ok(OpenResult {
            spot_leg,
            perp_leg,
            position,
        })
    }

    async fn rollback_open_after_perp_failure(
        &self,
        symbol: &str,
        quantity: Decimal,
    ) -> Result<()> {
        match self.spot.market_order(symbol, Side::Sell, quantity).await {
            Ok(_) => Ok(()),
            Err(err) => {
                tracing::error!(%symbol, error = %err, "funding-arb rollback sell failed");
                Err(fa_err(format!(
                    "perp leg failed AND rollback sell failed on {symbol}: manual reconciliation required"
                )))
            }
        }
    }

    /// Close a delta-neutral position: perp BUY (reduce-only) then spot SELL.
    pub async fn close_position(&self, position: &FundingArbPosition) -> Result<CloseResult> {
        if position.spot_qty <= Decimal::ZERO {
            return Err(fa_err(format!(
                "position has no spot quantity to close: {position:?}"
            )));
        }
        let perp_close_qty = position.perp_qty.abs();
        if perp_close_qty <= Decimal::ZERO {
            return Err(fa_err(format!(
                "position has no perp quantity to close: {position:?}"
            )));
        }
        let symbol = position.symbol.as_str();

        // Leg 1: close the perp short.
        let perp_fill = self
            .futures
            .market_order(symbol, Side::Buy, perp_close_qty, true)
            .await?;
        if perp_fill.filled_qty <= Decimal::ZERO {
            return Err(fa_err(format!(
                "perp close returned zero fill on {symbol}; position still open"
            )));
        }
        let perp_leg = ExecutionLeg::new("futures", "buy", symbol, perp_close_qty, perp_fill);

        // Leg 2: sell the spot.
        let spot_fill = self
            .spot
            .market_order(symbol, Side::Sell, position.spot_qty)
            .await?;
        if spot_fill.filled_qty <= Decimal::ZERO {
            return Err(fa_err(format!(
                "spot sell returned zero fill on {symbol}; perp already closed, manual reconciliation required"
            )));
        }
        let spot_leg = ExecutionLeg::new("spot", "sell", symbol, position.spot_qty, spot_fill);

        // Sweep collateral back to spot; non-fatal (can be done manually).
        if let Err(err) = self
            .collateral
            .transfer_futures_to_spot(self.collateral_for(position.notional_usdt))
            .await
        {
            tracing::warn!(%symbol, error = %err, "funding-arb collateral sweep failed");
        }

        // spot proceeds − perp buy-back cost + funding received.
        let realized_pnl_usdt =
            spot_leg.cost_usdt - perp_leg.cost_usdt + position.funding_received_usdt;
        Ok(CloseResult {
            perp_leg,
            spot_leg,
            realized_pnl_usdt,
        })
    }

    /// Verify each held symbol: spot free base ≥ `spot_qty` and the perp size within 1%
    /// of `perp_qty`.
    pub async fn reconcile(
        &self,
        local_positions: &[FundingArbPosition],
    ) -> Result<ReconciliationReport> {
        let spot_balances = self.spot.free_balances().await?;
        let futures_positions = self.futures.positions().await?;
        let tol = Decimal::new(1, 6);
        let mut mismatches = Vec::new();
        for position in local_positions {
            let symbol = position.symbol.as_str();
            let base = symbol.split_once('/').map_or(symbol, |(base, _)| base);
            let spot_actual = spot_balances.get(base).copied().unwrap_or_default();
            if spot_actual + tol < position.spot_qty {
                mismatches.push(ReconciliationMismatch {
                    symbol: symbol.to_owned(),
                    detail: format!(
                        "spot {base} balance {spot_actual} is below required {}",
                        position.spot_qty
                    ),
                });
            }
            let key = exchange_id(symbol).unwrap_or_else(|_| symbol.to_owned());
            let perp_actual = futures_positions.get(&key).copied().unwrap_or_default();
            let allowed = (position.perp_qty.abs() * Decimal::new(1, 2)).max(tol);
            if (perp_actual - position.perp_qty).abs() > allowed {
                mismatches.push(ReconciliationMismatch {
                    symbol: symbol.to_owned(),
                    detail: format!(
                        "futures position {perp_actual} differs from expected {} by more than 1% tolerance",
                        position.perp_qty
                    ),
                });
            }
        }
        Ok(ReconciliationReport {
            spot_balances,
            futures_positions,
            mismatches,
        })
    }

    /// Total free USDT across spot + USD-M futures (dynamic balance sizing).
    pub async fn free_usdt(&self) -> Result<Decimal> {
        let spot = self
            .spot
            .free_balances()
            .await?
            .get("USDT")
            .copied()
            .unwrap_or_default();
        let futures = self.futures.free_usdt().await?;
        Ok(spot + futures)
    }
}

/// Binance implementation of every venue trait.
#[derive(Clone)]
pub struct BinanceVenue {
    client: BinanceClient,
}

/// How long to wait for a USD-M MARKET order that was acknowledged before it filled.
const FUTURES_FILL_POLLS: u32 = 10;
const FUTURES_FILL_POLL_INTERVAL: Duration = Duration::from_millis(300);

impl BinanceVenue {
    pub fn new(client: BinanceClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl SpotVenue for BinanceVenue {
    async fn last_price(&self, symbol: &str) -> Result<Decimal> {
        Ok(self.client.price(symbol).await?.price)
    }

    async fn market_order(&self, symbol: &str, side: Side, quantity: Decimal) -> Result<LegFill> {
        let rules = self.client.symbol_rules(symbol).await?;
        let qty = rules.round_market_qty(quantity);
        if qty <= Decimal::ZERO {
            return Err(fa_err(format!(
                "spot {symbol} quantity {quantity} rounds to zero at step {}",
                rules.market_step_size()
            )));
        }
        let order = self
            .client
            .create_order(&NewOrder::market(symbol, side, qty))
            .await?;
        let summary = FillSummary::from_order(&order);
        // A BUY may pay commission in the base asset; only the net amount is sellable/hedgeable.
        let filled_qty = match side {
            Side::Buy => rules.round_market_qty(summary.net_base_received(&rules.base_asset)),
            Side::Sell => summary.executed_qty,
        };
        Ok(LegFill {
            filled_qty,
            avg_price: summary.avg_price.unwrap_or_default(),
            cost_usdt: summary.quote_qty,
            order_id: order.order_id.to_string(),
        })
    }

    async fn free_balances(&self) -> Result<BTreeMap<String, Decimal>> {
        Ok(self.client.account().await?.free_balances())
    }
}

#[async_trait]
impl FuturesVenue for BinanceVenue {
    async fn market_order(
        &self,
        symbol: &str,
        side: Side,
        quantity: Decimal,
        reduce_only: bool,
    ) -> Result<LegFill> {
        let id = exchange_id(symbol)?;
        let rules = self.client.futures_symbol_rules(&id).await?;
        let qty = rules.round_market_qty(quantity);
        if qty <= Decimal::ZERO {
            return Err(fa_err(format!(
                "futures {id} quantity {quantity} rounds to zero at step {}",
                rules.market_step_size()
            )));
        }
        let mut request = FuturesNewOrder::market(&id, side, qty);
        if reduce_only {
            request = request.reduce_only();
        }
        let mut order = self.client.create_futures_order(&request).await?;
        // USD-M MARKET orders can be acknowledged as NEW before matching completes.
        let mut polls = 0;
        while order.executed_qty.is_zero()
            && !order.status.is_terminal()
            && polls < FUTURES_FILL_POLLS
        {
            tokio::time::sleep(FUTURES_FILL_POLL_INTERVAL).await;
            order = self
                .client
                .query_futures_order(&id, &bb_binance::spot::OrderRef::Id(order.order_id))
                .await?;
            polls += 1;
        }
        Ok(LegFill {
            filled_qty: order.executed_qty,
            avg_price: order.avg_price,
            cost_usdt: order.cum_quote,
            order_id: order.order_id.to_string(),
        })
    }

    async fn free_usdt(&self) -> Result<Decimal> {
        Ok(self
            .client
            .futures_balances()
            .await?
            .into_iter()
            .find(|a| a.asset == "USDT")
            .map(|a| a.available_balance)
            .unwrap_or_default())
    }

    async fn positions(&self) -> Result<HashMap<String, Decimal>> {
        let mut out = HashMap::new();
        for row in self.client.position_risk(None).await? {
            *out.entry(row.symbol).or_default() += row.position_amt;
        }
        Ok(out)
    }
}

/// Universal transfers carry at most 8 decimals.
fn transfer_amount(amount: Decimal) -> Decimal {
    amount.round_dp_with_strategy(8, rust_decimal::RoundingStrategy::ToZero)
}

#[async_trait]
impl CollateralTransfer for BinanceVenue {
    async fn transfer_spot_to_futures(&self, amount_usdt: Decimal) -> Result<()> {
        let transfer = UniversalTransfer {
            transfer_type: "MAIN_UMFUTURE".into(),
            asset: "USDT".into(),
            amount: transfer_amount(amount_usdt),
            from_symbol: None,
            to_symbol: None,
        };
        self.client.universal_transfer(&transfer).await?;
        Ok(())
    }

    async fn transfer_futures_to_spot(&self, amount_usdt: Decimal) -> Result<()> {
        let transfer = UniversalTransfer {
            transfer_type: "UMFUTURE_MAIN".into(),
            asset: "USDT".into(),
            amount: transfer_amount(amount_usdt),
            from_symbol: None,
            to_symbol: None,
        };
        self.client.universal_transfer(&transfer).await?;
        Ok(())
    }
}

/// In-memory venues for executor and runtime tests.
#[cfg(test)]
pub(crate) mod fakes {
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    pub struct Call {
        pub venue: &'static str,
        pub side: Side,
        pub amount: Decimal,
        pub reduce_only: bool,
    }

    pub struct FakeSpot {
        pub last: Decimal,
        pub balance: BTreeMap<String, Decimal>,
        pub calls: Mutex<Vec<Call>>,
        /// 1-based call index whose order fails after being recorded.
        pub fail_on_call: Option<usize>,
        /// Fail the first N balance fetches.
        pub balance_failures: AtomicUsize,
    }

    impl FakeSpot {
        pub fn new(last: Decimal) -> Self {
            Self {
                last,
                balance: [
                    ("BTC".to_owned(), Decimal::ZERO),
                    ("USDT".to_owned(), Decimal::from(10_000)),
                ]
                .into(),
                calls: Mutex::new(Vec::new()),
                fail_on_call: None,
                balance_failures: AtomicUsize::new(0),
            }
        }

        pub fn with_balance(mut self, entries: &[(&str, Decimal)]) -> Self {
            self.balance = entries.iter().map(|(a, v)| ((*a).to_owned(), *v)).collect();
            self
        }

        pub fn sides(&self) -> Vec<Side> {
            self.calls.lock().iter().map(|c| c.side).collect()
        }
    }

    #[async_trait]
    impl SpotVenue for FakeSpot {
        async fn last_price(&self, _symbol: &str) -> Result<Decimal> {
            Ok(self.last)
        }

        async fn market_order(
            &self,
            _symbol: &str,
            side: Side,
            quantity: Decimal,
        ) -> Result<LegFill> {
            let mut calls = self.calls.lock();
            calls.push(Call {
                venue: "spot",
                side,
                amount: quantity,
                reduce_only: false,
            });
            if self.fail_on_call == Some(calls.len()) {
                return Err(StrategyError::Http(
                    "simulated spot sell rollback failure".into(),
                ));
            }
            Ok(LegFill {
                filled_qty: quantity,
                avg_price: self.last,
                cost_usdt: quantity * self.last,
                order_id: format!("spot-{}", calls.len()),
            })
        }

        async fn free_balances(&self) -> Result<BTreeMap<String, Decimal>> {
            let remaining = self.balance_failures.load(Ordering::SeqCst);
            if remaining > 0 {
                self.balance_failures.store(remaining - 1, Ordering::SeqCst);
                return Err(StrategyError::Http("network blip".into()));
            }
            Ok(self.balance.clone())
        }
    }

    pub struct FakeFutures {
        pub last: Decimal,
        pub fail_on_open: bool,
        pub calls: Mutex<Vec<Call>>,
        pub positions: HashMap<String, Decimal>,
        pub free: Decimal,
    }

    impl FakeFutures {
        pub fn new(last: Decimal) -> Self {
            Self {
                last,
                fail_on_open: false,
                calls: Mutex::new(Vec::new()),
                positions: HashMap::new(),
                free: Decimal::from(5_000),
            }
        }

        pub fn with_position(mut self, id: &str, qty: Decimal) -> Self {
            self.positions.insert(id.to_owned(), qty);
            self
        }
    }

    #[async_trait]
    impl FuturesVenue for FakeFutures {
        async fn market_order(
            &self,
            _symbol: &str,
            side: Side,
            quantity: Decimal,
            reduce_only: bool,
        ) -> Result<LegFill> {
            let mut calls = self.calls.lock();
            calls.push(Call {
                venue: "futures",
                side,
                amount: quantity,
                reduce_only,
            });
            if self.fail_on_open && side == Side::Sell {
                return Err(StrategyError::Http("simulated perp short failure".into()));
            }
            Ok(LegFill {
                filled_qty: quantity,
                avg_price: self.last,
                cost_usdt: quantity * self.last,
                order_id: format!("futures-{}", calls.len()),
            })
        }

        async fn free_usdt(&self) -> Result<Decimal> {
            Ok(self.free)
        }

        async fn positions(&self) -> Result<HashMap<String, Decimal>> {
            Ok(self.positions.clone())
        }
    }

    #[derive(Default)]
    pub struct FakeCollateral {
        pub calls: Mutex<Vec<(&'static str, Decimal)>>,
    }

    #[async_trait]
    impl CollateralTransfer for FakeCollateral {
        async fn transfer_spot_to_futures(&self, amount_usdt: Decimal) -> Result<()> {
            self.calls.lock().push(("spot->futures", amount_usdt));
            Ok(())
        }

        async fn transfer_futures_to_spot(&self, amount_usdt: Decimal) -> Result<()> {
            self.calls.lock().push(("futures->spot", amount_usdt));
            Ok(())
        }
    }

    /// Shared handles so tests can inspect calls after moving the fakes into an executor.
    pub struct Shared<T>(pub std::sync::Arc<T>);

    #[async_trait]
    impl<T: SpotVenue> SpotVenue for Shared<T> {
        async fn last_price(&self, symbol: &str) -> Result<Decimal> {
            self.0.last_price(symbol).await
        }
        async fn market_order(
            &self,
            symbol: &str,
            side: Side,
            quantity: Decimal,
        ) -> Result<LegFill> {
            self.0.market_order(symbol, side, quantity).await
        }
        async fn free_balances(&self) -> Result<BTreeMap<String, Decimal>> {
            self.0.free_balances().await
        }
    }

    #[async_trait]
    impl<T: FuturesVenue> FuturesVenue for Shared<T> {
        async fn market_order(
            &self,
            symbol: &str,
            side: Side,
            quantity: Decimal,
            reduce_only: bool,
        ) -> Result<LegFill> {
            self.0
                .market_order(symbol, side, quantity, reduce_only)
                .await
        }
        async fn free_usdt(&self) -> Result<Decimal> {
            self.0.free_usdt().await
        }
        async fn positions(&self) -> Result<HashMap<String, Decimal>> {
            self.0.positions().await
        }
    }

    #[async_trait]
    impl<T: CollateralTransfer> CollateralTransfer for Shared<T> {
        async fn transfer_spot_to_futures(&self, amount_usdt: Decimal) -> Result<()> {
            self.0.transfer_spot_to_futures(amount_usdt).await
        }
        async fn transfer_futures_to_spot(&self, amount_usdt: Decimal) -> Result<()> {
            self.0.transfer_futures_to_spot(amount_usdt).await
        }
    }

    pub fn executor(
        spot: &std::sync::Arc<FakeSpot>,
        futures: &std::sync::Arc<FakeFutures>,
        collateral: &std::sync::Arc<FakeCollateral>,
    ) -> FundingArbExecutor {
        FundingArbExecutor::new(
            Box::new(Shared(spot.clone())),
            Box::new(Shared(futures.clone())),
            Box::new(Shared(collateral.clone())),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rust_decimal_macros::dec;

    use super::fakes::*;
    use super::*;

    fn position(spot_qty: Decimal, perp_qty: Decimal, funding_usdt: Decimal) -> FundingArbPosition {
        FundingArbPosition {
            symbol: "BTC/USDT".into(),
            open_time_ms: 0,
            entry_funding_bps: 5.0,
            notional_usdt: dec!(2000),
            spot_qty,
            perp_qty,
            funding_received_bps: 0.0,
            funding_received_usdt: funding_usdt,
            events_held: 0,
        }
    }

    fn setup(
        spot: FakeSpot,
        futures: FakeFutures,
    ) -> (
        Arc<FakeSpot>,
        Arc<FakeFutures>,
        Arc<FakeCollateral>,
        FundingArbExecutor,
    ) {
        let (s, f, c) = (
            Arc::new(spot),
            Arc::new(futures),
            Arc::new(FakeCollateral::default()),
        );
        let exec = executor(&s, &f, &c);
        (s, f, c, exec)
    }

    #[tokio::test]
    async fn open_places_both_legs_and_returns_position() {
        let (_, _, collateral, exec) = setup(FakeSpot::new(dec!(200)), FakeFutures::new(dec!(200)));
        let result = exec
            .open_position("BTC/USDT", dec!(2000), 5.0, 123)
            .await
            .unwrap();
        assert_eq!(
            (result.spot_leg.side, result.spot_leg.filled_qty),
            ("buy", dec!(10))
        );
        assert_eq!(
            (result.perp_leg.side, result.perp_leg.filled_qty),
            ("sell", dec!(10))
        );
        assert_eq!(result.position.spot_qty, dec!(10));
        assert_eq!(result.position.perp_qty, dec!(-10));
        assert_eq!(
            *collateral.calls.lock(),
            vec![("spot->futures", dec!(2000))]
        );
    }

    #[tokio::test]
    async fn open_rolls_back_spot_on_perp_failure() {
        let mut futures = FakeFutures::new(dec!(200));
        futures.fail_on_open = true;
        let (spot, _, _, exec) = setup(FakeSpot::new(dec!(200)), futures);
        let err = exec
            .open_position("BTC/USDT", dec!(2000), 5.0, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("perp short failed"));
        assert_eq!(spot.sides(), vec![Side::Buy, Side::Sell]);
    }

    #[tokio::test]
    async fn open_raises_clearly_when_rollback_also_fails() {
        let mut spot = FakeSpot::new(dec!(200));
        spot.fail_on_call = Some(2);
        let mut futures = FakeFutures::new(dec!(200));
        futures.fail_on_open = true;
        let (_, _, _, exec) = setup(spot, futures);
        let err = exec
            .open_position("BTC/USDT", dec!(2000), 5.0, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("manual reconciliation required"));
    }

    #[tokio::test]
    async fn open_rejects_non_positive_notional_and_missing_price() {
        let (_, _, _, exec) = setup(FakeSpot::new(dec!(200)), FakeFutures::new(dec!(200)));
        let err = exec
            .open_position("BTC/USDT", Decimal::ZERO, 5.0, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("notional_usdt"));
        let (_, _, _, exec) = setup(FakeSpot::new(Decimal::ZERO), FakeFutures::new(dec!(200)));
        let err = exec
            .open_position("BTC/USDT", dec!(2000), 5.0, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("spot price unavailable"));
    }

    #[tokio::test]
    async fn close_runs_perp_then_spot_reduce_only_and_returns_pnl() {
        let (spot, futures, collateral, exec) =
            setup(FakeSpot::new(dec!(210)), FakeFutures::new(dec!(210)));
        let result = exec
            .close_position(&position(dec!(10), dec!(-10), dec!(8)))
            .await
            .unwrap();
        assert_eq!(
            (result.perp_leg.side, result.perp_leg.filled_qty),
            ("buy", dec!(10))
        );
        assert_eq!(
            (result.spot_leg.side, result.spot_leg.filled_qty),
            ("sell", dec!(10))
        );
        assert_eq!(result.realized_pnl_usdt, dec!(8));
        assert_eq!(
            *collateral.calls.lock(),
            vec![("futures->spot", dec!(2000))]
        );
        assert!(futures.calls.lock().last().unwrap().reduce_only);
        assert_eq!(spot.sides(), vec![Side::Sell]);
    }

    #[tokio::test]
    async fn close_rejects_position_with_no_qty() {
        let (_, _, _, exec) = setup(FakeSpot::new(dec!(200)), FakeFutures::new(dec!(200)));
        let err = exec
            .close_position(&position(Decimal::ZERO, Decimal::ZERO, Decimal::ZERO))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no spot quantity"));
    }

    #[tokio::test]
    async fn reconcile_matches_flags_spot_shortfall_and_perp_drift() {
        let held = [position(dec!(10), dec!(-10), Decimal::ZERO)];
        let spot =
            || FakeSpot::new(dec!(200)).with_balance(&[("BTC", dec!(10.5)), ("USDT", dec!(1000))]);

        let (_, _, _, exec) = setup(
            spot(),
            FakeFutures::new(dec!(200)).with_position("BTCUSDT", dec!(-10)),
        );
        let report = exec.reconcile(&held).await.unwrap();
        assert!(report.ok());

        let short =
            FakeSpot::new(dec!(200)).with_balance(&[("BTC", dec!(5)), ("USDT", dec!(1000))]);
        let (_, _, _, exec) = setup(
            short,
            FakeFutures::new(dec!(200)).with_position("BTCUSDT", dec!(-10)),
        );
        let report = exec.reconcile(&held).await.unwrap();
        assert!(!report.ok());
        assert!(
            report
                .mismatches
                .iter()
                .any(|m| m.detail.contains("spot BTC balance"))
        );

        let (_, _, _, exec) = setup(
            spot(),
            FakeFutures::new(dec!(200)).with_position("BTCUSDT", dec!(-7)),
        );
        let report = exec.reconcile(&held).await.unwrap();
        assert!(
            report
                .mismatches
                .iter()
                .any(|m| m.detail.contains("futures position"))
        );
    }

    #[tokio::test]
    async fn binance_futures_order_polls_until_filled() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let info = serde_json::json!({"symbols": [{
            "symbol": "BTCUSDT", "pair": "BTCUSDT", "contractType": "PERPETUAL", "status": "TRADING",
            "baseAsset": "BTC", "quoteAsset": "USDT", "marginAsset": "USDT",
            "filters": [{"filterType": "LOT_SIZE", "minQty": "0.001", "maxQty": "1000", "stepSize": "0.001"},
                        {"filterType": "MARKET_LOT_SIZE", "minQty": "0.001", "maxQty": "100", "stepSize": "0.001"}]
        }]});
        Mock::given(method("GET"))
            .and(path("/fapi/v1/exchangeInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(info))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fapi/v1/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"serverTime": bb_core::time::now_ms()})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/fapi/v1/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 7, "symbol": "BTCUSDT", "status": "NEW", "executedQty": "0", "avgPrice": "0", "cumQuote": "0"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fapi/v1/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 7, "symbol": "BTCUSDT", "status": "FILLED", "executedQty": "0.012", "avgPrice": "100", "cumQuote": "1.2"
            })))
            .mount(&server)
            .await;
        let config = bb_binance::BinanceConfig::new(false)
            .with_futures_rest_url(server.uri())
            .with_credentials(bb_binance::Credentials::new("k", "s").unwrap());
        let venue = BinanceVenue::new(BinanceClient::new(config).unwrap());
        let fill = FuturesVenue::market_order(&venue, "BTC/USDT", Side::Sell, dec!(0.0129), false)
            .await
            .unwrap();
        assert_eq!(fill.filled_qty, dec!(0.012));
        assert_eq!(fill.cost_usdt, dec!(1.2));
        let requests = server.received_requests().await.unwrap();
        let order = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .unwrap();
        let query = order.url.query().unwrap_or_default().to_owned()
            + &String::from_utf8_lossy(&order.body);
        assert!(query.contains("quantity=0.012"), "{query}");
        assert!(!query.contains("reduceOnly"), "{query}");
    }
}

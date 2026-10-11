//! Port of `src/strategies/demo_sma.py`: the reference 20/50 SMA crossover.
//!
//! A buy signal fires when the fast SMA crosses from below/equal to above the
//! slow SMA; a sell signal on the opposite cross. Signals submit a market
//! order of `order_size` (zero disables execution).

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use bb_binance::normalize_symbol;
use bb_core::Decimal;
use parking_lot::Mutex;

use super::base::{
    Candle, FillData, OrderClient, Strategy, StrategyError, StrategyOrder, TickData,
};
use crate::orders::models::OrderSide;

pub const PLUGIN_NAME: &str = "demo-sma";

/// Validated configuration for [`DemoSmaStrategy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmaSettings {
    pub symbol: String,
    pub fast_period: usize,
    pub slow_period: usize,
    pub order_size: Decimal,
    pub timeframe: String,
}

impl SmaSettings {
    /// Python defaults: 20/50, order size 0, timeframe `1h`.
    pub fn new(symbol: &str) -> Result<Self, StrategyError> {
        Self {
            symbol: symbol.to_owned(),
            fast_period: 20,
            slow_period: 50,
            order_size: Decimal::ZERO,
            timeframe: "1h".to_owned(),
        }
        .validate()
    }

    /// Normalise the symbol/timeframe and check field constraints.
    pub fn validate(mut self) -> Result<Self, StrategyError> {
        self.symbol =
            normalize_symbol(&self.symbol).map_err(|e| StrategyError::Invalid(e.to_string()))?;
        self.timeframe = self.timeframe.trim().to_owned();
        if self.timeframe.is_empty() {
            return Err(StrategyError::Invalid("timeframe must not be blank".into()));
        }
        if self.fast_period == 0 || self.slow_period == 0 {
            return Err(StrategyError::Invalid(
                "SMA periods must be greater than 0".into(),
            ));
        }
        if self.order_size < Decimal::ZERO {
            return Err(StrategyError::Invalid(
                "order_size must be at least 0".into(),
            ));
        }
        if self.fast_period >= self.slow_period {
            return Err(StrategyError::Invalid(
                "fast_period must be less than slow_period".into(),
            ));
        }
        Ok(self)
    }
}

/// SMA state calculated from the latest accepted market update.
#[derive(Debug, Clone, PartialEq)]
pub struct SmaSnapshot {
    pub symbol: String,
    pub timestamp: i64,
    pub price: Decimal,
    pub fast_sma: f64,
    pub slow_sma: f64,
    pub relation: i8,
    pub signal: Option<OrderSide>,
}

#[derive(Default)]
struct SmaState {
    running: bool,
    prices: VecDeque<f64>,
    previous_relation: Option<i8>,
    last_snapshot: Option<SmaSnapshot>,
    last_signal: Option<OrderSide>,
}

pub struct DemoSmaStrategy {
    client: Arc<dyn OrderClient>,
    settings: SmaSettings,
    state: Mutex<SmaState>,
}

impl std::fmt::Debug for DemoSmaStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DemoSmaStrategy")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl DemoSmaStrategy {
    pub fn new(client: Arc<dyn OrderClient>, settings: SmaSettings) -> Self {
        Self {
            client,
            settings,
            state: Mutex::new(SmaState::default()),
        }
    }

    pub fn settings(&self) -> &SmaSettings {
        &self.settings
    }

    pub fn sample_count(&self) -> usize {
        self.state.lock().prices.len()
    }

    pub fn required_samples(&self) -> usize {
        self.settings.slow_period
    }

    pub fn last_snapshot(&self) -> Option<SmaSnapshot> {
        self.state.lock().last_snapshot.clone()
    }

    pub fn last_signal(&self) -> Option<OrderSide> {
        self.state.lock().last_signal
    }

    /// Update indicators with one price; returns the crossover signal, if any.
    fn ingest(&self, tick: &TickData) -> Option<OrderSide> {
        let mut st = self.state.lock();
        st.prices.push_back(bb_core::money::to_f64(tick.last_price));
        while st.prices.len() > self.settings.slow_period {
            st.prices.pop_front();
        }
        if st.prices.len() < self.settings.slow_period {
            tracing::debug!(strategy = PLUGIN_NAME, symbol = %self.settings.symbol, samples = st.prices.len(), required_samples = self.settings.slow_period, "SMA window warming");
            return None;
        }
        let fast = sma(&st.prices, self.settings.fast_period);
        let slow = sma(&st.prices, self.settings.slow_period);
        let relation = relation(fast, slow);
        let signal = signal_from_cross(st.previous_relation, relation);
        st.previous_relation = Some(relation);
        st.last_snapshot = Some(SmaSnapshot {
            symbol: self.settings.symbol.clone(),
            timestamp: tick.timestamp,
            price: tick.last_price,
            fast_sma: fast,
            slow_sma: slow,
            relation,
            signal,
        });
        tracing::info!(
            strategy = PLUGIN_NAME,
            symbol = %self.settings.symbol,
            tick_timestamp = tick.timestamp,
            price = %tick.last_price,
            fast_sma = fast,
            slow_sma = slow,
            relation,
            signal = signal.map(OrderSide::as_str),
            "SMA indicators updated"
        );
        if signal.is_some() {
            st.last_signal = signal;
        }
        signal
    }

    async fn execute_signal(&self, side: OrderSide) {
        if self.settings.order_size.is_zero() {
            tracing::info!(
                strategy = PLUGIN_NAME,
                signal = side.as_str(),
                "SMA signal generated; order_size is zero"
            );
            return;
        }
        tracing::info!(
            strategy = PLUGIN_NAME,
            signal = side.as_str(),
            order_size = %self.settings.order_size,
            "SMA crossover signal generated; submitting market order"
        );
        let order =
            StrategyOrder::market(self.settings.symbol.clone(), side, self.settings.order_size);
        match self.client.create_order(order).await {
            Ok(result) => tracing::info!(
                strategy = PLUGIN_NAME,
                signal = side.as_str(),
                order_id = %result.id,
                status = %result.status,
                filled = %result.filled,
                average = ?result.average,
                "Market order placed"
            ),
            Err(error) => self.on_error(&error).await,
        }
    }
}

#[async_trait]
impl Strategy for DemoSmaStrategy {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    fn client(&self) -> &Arc<dyn OrderClient> {
        &self.client
    }

    fn is_running(&self) -> bool {
        self.state.lock().running
    }

    async fn start(&self) {
        let mut st = self.state.lock();
        if st.running {
            tracing::debug!(
                strategy = PLUGIN_NAME,
                "Strategy start skipped; already running"
            );
            return;
        }
        *st = SmaState {
            running: true,
            ..SmaState::default()
        };
        tracing::info!(strategy = PLUGIN_NAME, "Strategy started");
    }

    async fn stop(&self) {
        let mut st = self.state.lock();
        if !st.running {
            tracing::debug!(
                strategy = PLUGIN_NAME,
                "Strategy stop skipped; already stopped"
            );
            return;
        }
        st.running = false;
        tracing::info!(strategy = PLUGIN_NAME, "Strategy stopped");
    }

    async fn on_tick(&self, tick: TickData) {
        if !self.is_running() {
            tracing::debug!(
                strategy = PLUGIN_NAME,
                tick_timestamp = tick.timestamp,
                "Ignoring tick while strategy is stopped"
            );
            return;
        }
        if compact_symbol(&tick.symbol) != compact_symbol(&self.settings.symbol) {
            tracing::debug!(strategy = PLUGIN_NAME, tick_symbol = %tick.symbol, "Ignoring tick for different symbol");
            return;
        }
        if let Some(signal) = self.ingest(&tick) {
            self.execute_signal(signal).await;
        }
    }

    async fn on_candle(&self, candle: &Candle) -> Result<(), StrategyError> {
        if !candle.closed {
            tracing::debug!(strategy = PLUGIN_NAME, "Ignoring open candle");
            return Ok(());
        }
        if candle.close <= Decimal::ZERO {
            return Err(StrategyError::Invalid(
                "close/c must be a positive finite number".into(),
            ));
        }
        if candle.timestamp < 0 {
            return Err(StrategyError::Invalid(
                "timestamp/time/t must be non-negative".into(),
            ));
        }
        if candle.volume < Decimal::ZERO {
            return Err(StrategyError::Invalid(
                "volume must be a non-negative finite number".into(),
            ));
        }
        let symbol = if candle.symbol.is_empty() {
            self.settings.symbol.clone()
        } else {
            candle.symbol.clone()
        };
        self.on_tick(TickData {
            timestamp: candle.timestamp,
            symbol,
            bid: candle.close,
            ask: candle.close,
            last_price: candle.close,
            volume: candle.volume,
        })
        .await;
        Ok(())
    }

    async fn on_fill(&self, fill: FillData) {
        tracing::info!(
            strategy = PLUGIN_NAME,
            order_id = %fill.order_id,
            side = fill.side.as_str(),
            price = %fill.price,
            quantity = %fill.quantity,
            timestamp = fill.timestamp,
            "SMA strategy fill received"
        );
    }

    async fn on_error(&self, error: &StrategyError) {
        tracing::error!(strategy = PLUGIN_NAME, error_message = %error, "SMA strategy error");
    }
}

fn sma(values: &VecDeque<f64>, period: usize) -> f64 {
    let start = values.len().saturating_sub(period);
    values.iter().skip(start).sum::<f64>() / period as f64
}

/// Sign of `left - right` with `math.isclose(rel_tol=1e-12, abs_tol=1e-12)`.
fn relation(left: f64, right: f64) -> i8 {
    let tolerance = (1e-12 * left.abs().max(right.abs())).max(1e-12);
    if (left - right).abs() <= tolerance {
        0
    } else if left > right {
        1
    } else {
        -1
    }
}

fn signal_from_cross(previous: Option<i8>, current: i8) -> Option<OrderSide> {
    let previous = previous?;
    if previous <= 0 && current > 0 {
        Some(OrderSide::Buy)
    } else if previous >= 0 && current < 0 {
        Some(OrderSide::Sell)
    } else {
        None
    }
}

fn compact_symbol(symbol: &str) -> String {
    symbol
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orders::models::{OrderStatus, OrderType};
    use crate::strategies::base::OrderResult;
    use rust_decimal_macros::dec;

    #[derive(Default)]
    struct RecordingClient {
        orders: Mutex<Vec<StrategyOrder>>,
    }

    #[async_trait]
    impl OrderClient for RecordingClient {
        async fn create_order(&self, order: StrategyOrder) -> Result<OrderResult, StrategyError> {
            self.orders.lock().push(order.clone());
            Ok(OrderResult {
                id: "1".into(),
                symbol: order.symbol,
                order_type: order.order_type,
                side: order.side,
                amount: order.amount,
                price: None,
                status: OrderStatus::Closed,
                filled: order.amount,
                remaining: Decimal::ZERO,
                average: None,
            })
        }

        fn update_last_price(&self, _: Decimal) {}
    }

    fn tick(price: Decimal, ts: i64) -> TickData {
        TickData {
            timestamp: ts,
            symbol: "BTCUSDT".into(),
            bid: price,
            ask: price,
            last_price: price,
            volume: dec!(1),
        }
    }

    #[test]
    fn defaults_to_20_50() {
        let settings = SmaSettings::new("btcusdt").unwrap();
        assert_eq!((settings.fast_period, settings.slow_period), (20, 50));
        assert_eq!(settings.symbol, "BTC/USDT");
        let bad = SmaSettings {
            fast_period: 5,
            slow_period: 5,
            ..settings
        };
        assert!(bad.validate().is_err());
    }

    #[tokio::test]
    async fn generates_buy_and_sell_orders_on_tick_crossovers() {
        let client = Arc::new(RecordingClient::default());
        let settings = SmaSettings {
            fast_period: 2,
            slow_period: 3,
            order_size: dec!(0.25),
            ..SmaSettings::new("BTC/USDT").unwrap()
        };
        let strategy = DemoSmaStrategy::new(client.clone(), settings);
        strategy.start().await;
        for (i, price) in [
            dec!(10),
            dec!(10),
            dec!(10),
            dec!(12),
            dec!(12),
            dec!(8),
            dec!(8),
        ]
        .into_iter()
        .enumerate()
        {
            strategy.on_tick(tick(price, i as i64)).await;
        }
        let orders = client.orders.lock().clone();
        let sides: Vec<_> = orders.iter().map(|o| o.side).collect();
        assert_eq!(sides, vec![OrderSide::Buy, OrderSide::Sell]);
        assert!(orders.iter().all(|o| o.order_type == OrderType::Market
            && o.amount == dec!(0.25)
            && o.symbol == "BTC/USDT"));
        assert_eq!(strategy.last_signal(), Some(OrderSide::Sell));
    }

    #[tokio::test]
    async fn candle_adapter_ignores_open_candles_and_stopped_state() {
        let client = Arc::new(RecordingClient::default());
        let strategy = DemoSmaStrategy::new(client, SmaSettings::new("BTC/USDT").unwrap());
        let candle = Candle {
            symbol: "BTC/USDT".into(),
            timestamp: 1,
            open: dec!(1),
            high: dec!(1),
            low: dec!(1),
            close: dec!(1),
            volume: dec!(1),
            closed: true,
        };
        strategy.on_candle(&candle).await.unwrap();
        assert_eq!(strategy.sample_count(), 0, "stopped strategy ignores ticks");
        strategy.start().await;
        strategy
            .on_candle(&Candle {
                closed: false,
                ..candle.clone()
            })
            .await
            .unwrap();
        assert_eq!(strategy.sample_count(), 0);
        strategy.on_candle(&candle).await.unwrap();
        assert_eq!(strategy.sample_count(), 1);
        assert!(
            strategy
                .on_candle(&Candle {
                    close: dec!(0),
                    ..candle
                })
                .await
                .is_err()
        );
    }
}

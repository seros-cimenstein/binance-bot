//! Port of `src/main.py` (and `main.py`): the bot runtime lifecycle.
//!
//! One [`BotRuntime`] owns the component graph for one attempt; [`supervise`]
//! restarts crashed attempts with exponential backoff and Telegram crash /
//! terminal-failure alerts; [`main`] wires settings, signals and alerts.

mod client;
mod sources;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bb_binance::BinanceError;
use bb_binance::ws::{EventType, WsEvent, WsManager, WsPayload};
use bb_core::Decimal;
use futures::StreamExt;
use parking_lot::Mutex;
use rust_decimal_macros::dec;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

pub use client::{ClientError, StrategyOrderClient, order_result};
pub use sources::{
    BinanceComponentFactory, CandleSource, CandleStream, ComponentFactory, Components,
    ExecutionSource, ExecutionStream, WsExecutionSource,
};

use crate::alerts::TelegramAlerter;
use crate::config::{MarketDataMode, OrderSizeMode, OrderUpdateMode, Settings, SettingsError};
use crate::db::{OrderDb, OrderDbError};
use crate::exchange::{Exchange, ExchangeError};
use crate::execution::ExecutionUpdateRouter;
use crate::orders::models::OrderSide;
use crate::orders::sizing::OrderSizingConfig;
use crate::orders::{OrderManager, OrderManagerError, OrderType};
use crate::portfolio::{PortfolioSnapshotService, PortfolioSource};
use crate::risk::{RiskConfig, RiskError, RiskGuard};
use crate::strategies::{Candle, Strategy, StrategyError, StrategyOrder, StrategyRegistry};

pub const MAX_STARTUP_ATTEMPTS: u32 = 5;
pub const BASE_RESTART_DELAY: Duration = Duration::from_secs(5);
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(60);
/// Candles whose close jumps more than this from the previous close are skipped.
pub const MAX_CLOSE_DEVIATION_PCT: Decimal = dec!(10.0);
/// Positions smaller than this are ignored by rollback.
const ROLLBACK_DUST: Decimal = dec!(0.00000001);

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Python `sys.exit(1)`: never retried.
    #[error("{0}")]
    LiveTradingRefused(&'static str),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Db(#[from] OrderDbError),
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
    #[error(transparent)]
    Binance(#[from] BinanceError),
    #[error(transparent)]
    Strategy(#[from] StrategyError),
    #[error(transparent)]
    Orders(#[from] OrderManagerError),
    #[error(transparent)]
    Risk(#[from] RiskError),
    #[error("runtime task failed: {0}")]
    Task(String),
    #[error("capital pool configuration failed: {0}")]
    Capital(String),
}

impl RuntimeError {
    /// Errors that end the process instead of triggering a restart.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::LiveTradingRefused(_))
    }
}

/// A set-once flag (`asyncio.Event`).
#[derive(Debug, Clone)]
pub struct Signal(Arc<watch::Sender<bool>>);

impl Default for Signal {
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl Signal {
    pub fn set(&self) {
        self.0.send_replace(true);
    }

    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        // The sender lives in `self`, so `wait_for` cannot observe a close.
        let _ = rx.wait_for(|set| *set).await;
    }
}

/// WebSocket failure alerts with a per-stream cooldown.
struct StreamAlerts {
    alerter: Arc<TelegramAlerter>,
    cooldown: Duration,
    last_sent: Mutex<HashMap<String, Instant>>,
}

impl StreamAlerts {
    async fn handle(&self, category: &str, event: &WsEvent) {
        let (stream, reason) = match &event.payload {
            WsPayload::StreamError(err) => {
                let reason = match &err.error_type {
                    Some(kind) if !kind.is_empty() => format!("{} ({kind})", err.message),
                    _ => err.message.clone(),
                };
                (format!("{category}/{}", err.stream), reason)
            }
            WsPayload::UserStreamExpired(_) => (
                format!("{category}/user"),
                "Binance user-data stream expired".to_owned(),
            ),
            _ => return,
        };
        let now = Instant::now();
        {
            let mut last_sent = self.last_sent.lock();
            if let Some(previous) = last_sent.get(&stream)
                && now.duration_since(*previous) < self.cooldown
            {
                tracing::debug!(%stream, cooldown_seconds = self.cooldown.as_secs_f64(), "Suppressing repeated WebSocket Telegram alert during cooldown");
                return;
            }
            last_sent.insert(stream.clone(), now);
        }
        self.alerter.alert_stream_failure(&stream, &reason).await;
    }

    fn subscribe(
        self: &Arc<Self>,
        manager: &WsManager,
        category: &'static str,
        include_user_expiration: bool,
    ) {
        let mut kinds = vec![EventType::StreamError];
        if include_user_expiration {
            kinds.push(EventType::UserStreamExpired);
        }
        for kind in kinds {
            let alerts = Arc::clone(self);
            manager.subscribe(kind, move |event| {
                let alerts = Arc::clone(&alerts);
                async move { alerts.handle(category, &event).await }
            });
        }
    }
}

/// Everything built for one runtime attempt.
struct Built {
    exchange: Arc<dyn Exchange>,
    order_manager: Arc<OrderManager>,
    risk_guard: Arc<RiskGuard>,
    /// Per-symbol (client, strategy) in `TRADING_SYMBOLS` order.
    symbols: Vec<SymbolRuntime>,
    feed: Arc<dyn CandleSource>,
    execution: Option<Arc<dyn ExecutionSource>>,
    router: Arc<ExecutionUpdateRouter>,
    _db: Arc<OrderDb>,
}

struct SymbolRuntime {
    settings: Settings,
    client: Arc<StrategyOrderClient>,
    strategy: Arc<dyn Strategy>,
}

enum TaskOutcome {
    Feed(String, Result<(), RuntimeError>),
    Execution(Result<(), RuntimeError>),
}

/// Owns the bot component graph and lifecycle for one runtime attempt.
pub struct BotRuntime {
    settings: Settings,
    stop: Signal,
    rollback: Signal,
    alerter: Option<Arc<TelegramAlerter>>,
    factory: Arc<dyn ComponentFactory>,
    registry: StrategyRegistry,
}

impl BotRuntime {
    pub fn new(
        settings: Settings,
        stop: Signal,
        rollback: Signal,
        factory: Arc<dyn ComponentFactory>,
    ) -> Self {
        Self {
            settings,
            stop,
            rollback,
            alerter: None,
            factory,
            registry: StrategyRegistry::with_builtin(),
        }
    }

    pub fn with_alerter(mut self, alerter: Option<Arc<TelegramAlerter>>) -> Self {
        self.alerter = alerter;
        self
    }

    pub fn with_registry(mut self, registry: StrategyRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// `DRY_RUN` / `LIVE` / `testnet`.
    pub fn mode(&self) -> &'static str {
        self.settings.mode()
    }

    /// Build components, run until a stop condition, then clean up.
    pub async fn run(&self) -> Result<(), RuntimeError> {
        self.ensure_live_trading_confirmed()?;
        let built = self.build().await?;
        let result = self.run_built(&built).await;
        self.close(&built).await;
        result
    }

    async fn run_built(&self, built: &Built) -> Result<(), RuntimeError> {
        self.recover_state(built).await?;
        self.log_started(built);
        if let Some(alerter) = &self.alerter {
            alerter.alert_started(self.mode()).await;
        }
        self.run_until_stopped(built).await
    }

    fn ensure_live_trading_confirmed(&self) -> Result<(), RuntimeError> {
        if self.settings.use_testnet {
            return Ok(());
        }
        if !self.settings.dry_run && self.settings.order_size_mode == OrderSizeMode::Fixed {
            let message = "Fixed-size live execution is disabled; use DRY_RUN=true or a reviewed balance-aware runtime";
            tracing::error!(level = "CRITICAL", "{message}");
            return Err(RuntimeError::LiveTradingRefused(message));
        }
        if self.settings.confirm_live_trading != "yes" {
            let message = "Live trading requires CONFIRM_LIVE_TRADING=yes in environment";
            tracing::error!(level = "CRITICAL", "{message}");
            return Err(RuntimeError::LiveTradingRefused(message));
        }
        tracing::warn!("LIVE TRADING MODE ACTIVE — real money is at risk");
        Ok(())
    }

    async fn build(&self) -> Result<Built, RuntimeError> {
        let db = Arc::new(OrderDb::open(&self.settings.db_path)?);
        let components = self.factory.build(&self.settings)?;
        let exchange = components.exchange.clone();
        match self.build_rest(db, components) {
            Ok(built) => Ok(built),
            Err(err) => {
                exchange.close().await;
                tracing::info!("Binance trading bot stopped");
                Err(err)
            }
        }
    }

    fn build_rest(&self, db: Arc<OrderDb>, components: Components) -> Result<Built, RuntimeError> {
        let s = &self.settings;
        let order_manager = Arc::new(OrderManager::new(
            components.exchange.clone(),
            Some(db.clone()),
        ));
        order_manager.restore_from_db()?;
        let risk_guard = Arc::new(RiskGuard::new(
            RiskConfig {
                max_total_exposure_usd: s.max_total_exposure_usd,
                daily_drawdown_limit_pct: s.daily_drawdown_limit_pct,
                min_balance_usdt: s.min_balance_usdt,
                allowed_leverage: s.allowed_leverage,
                max_open_orders: s.max_open_orders,
                max_price_deviation_pct: s.max_price_deviation_pct,
                ..RiskConfig::new(s.max_position_size_usd, s.daily_loss_limit_usd)
            },
            Some(db.clone()),
        )?);
        risk_guard.restore_daily_loss_from_db()?;
        let portfolio: Arc<dyn PortfolioSource> = Arc::new(PortfolioSnapshotService::new(
            order_manager.clone(),
            Some(risk_guard.clone()),
        ));

        let mut symbols = Vec::with_capacity(s.trading_symbols.len());
        for symbol in &s.trading_symbols {
            let symbol_settings = s.for_symbol(symbol)?;
            let mut client = StrategyOrderClient::new(order_manager.clone())
                .with_risk_guard(risk_guard.clone())
                .with_alerter(self.alerter.clone())
                .with_dry_run(s.dry_run)
                .with_sizing_config(OrderSizingConfig::from_settings(&symbol_settings))
                .with_portfolio(portfolio.clone());
            if let Some(gate) = &components.capital {
                // Module id = strategy name (one module across symbols).
                client = client.with_capital(
                    gate.clone(),
                    &s.strategy_name,
                    vec![s.strategy_name.clone()],
                );
            }
            let client = Arc::new(client);
            let strategy =
                self.registry
                    .create(&s.strategy_name, client.clone(), &symbol_settings)?;
            symbols.push(SymbolRuntime {
                settings: symbol_settings,
                client,
                strategy,
            });
        }
        let strategies: HashMap<String, Arc<dyn Strategy>> = symbols
            .iter()
            .map(|sym| (sym.settings.trading_symbol.clone(), sym.strategy.clone()))
            .collect();
        let router = Arc::new(ExecutionUpdateRouter::new(
            order_manager.clone(),
            strategies,
        ));
        let execution = match s.order_update_mode {
            OrderUpdateMode::Websocket => components.execution,
            OrderUpdateMode::Disabled => None,
        };
        let built = Built {
            exchange: components.exchange,
            order_manager,
            risk_guard,
            symbols,
            feed: components.feed,
            execution,
            router,
            _db: db,
        };
        self.register_websocket_alerts(&built);
        Ok(built)
    }

    fn register_websocket_alerts(&self, built: &Built) {
        let Some(alerter) = &self.alerter else {
            return;
        };
        let alerts = Arc::new(StreamAlerts {
            alerter: alerter.clone(),
            cooldown: Duration::from_secs_f64(self.settings.ws_alert_cooldown_seconds),
            last_sent: Mutex::new(HashMap::new()),
        });
        if self.settings.market_data_mode == MarketDataMode::Websocket
            && let Some(manager) = built.feed.ws_manager()
        {
            alerts.subscribe(&manager, "market-data", false);
        }
        if let Some(manager) = built.execution.as_ref().and_then(|e| e.ws_manager()) {
            alerts.subscribe(&manager, "execution", true);
        }
    }

    async fn recover_state(&self, built: &Built) -> Result<(), RuntimeError> {
        let restored = built.order_manager.list_orders(None)?;
        let restored_active = built.order_manager.list_active_orders(None)?;
        let reconciled = if restored_active.is_empty() {
            Vec::new()
        } else {
            built.order_manager.reconcile_active_orders().await?
        };
        let active = built.order_manager.list_active_orders(None)?;
        let positions = built.order_manager.list_positions();
        tracing::info!(
            restored_order_count = restored.len(),
            restored_active_order_count = restored_active.len(),
            reconciled_order_count = reconciled.len(),
            active_order_count = active.len(),
            active_order_ids = ?active.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            position_count = positions.len(),
            position_symbols = ?positions.iter().map(|p| p.symbol.as_str()).collect::<Vec<_>>(),
            current_daily_loss = %built.risk_guard.current_daily_loss(),
            "Startup recovery audit"
        );
        Ok(())
    }

    fn log_started(&self, built: &Built) {
        let names: BTreeSet<&str> = built.symbols.iter().map(|s| s.strategy.name()).collect();
        tracing::info!(
            symbols = ?self.settings.trading_symbols,
            timeframe = %self.settings.timeframe,
            testnet = self.settings.use_testnet,
            strategies = ?names,
            dry_run = self.settings.dry_run,
            "Starting Binance trading bot"
        );
    }

    async fn run_until_stopped(&self, built: &Built) -> Result<(), RuntimeError> {
        for sym in &built.symbols {
            sym.strategy.start().await;
        }
        let mut tasks = JoinSet::new();
        for sym in &built.symbols {
            let feed = built.feed.clone();
            let client = sym.client.clone();
            let strategy = sym.strategy.clone();
            let settings = sym.settings.clone();
            let symbol = settings.trading_symbol.clone();
            tasks.spawn(async move {
                TaskOutcome::Feed(
                    symbol,
                    stream_candles(feed.as_ref(), &client, strategy.as_ref(), &settings).await,
                )
            });
        }
        if let Some(execution) = &built.execution {
            let execution = execution.clone();
            let router = built.router.clone();
            tasks.spawn(async move {
                TaskOutcome::Execution(stream_order_updates(execution.as_ref(), &router).await)
            });
        }

        let result = tokio::select! {
            biased;
            () = self.rollback.wait() => {
                tracing::warn!("ROLLBACK SIGNAL RECEIVED: Closing all positions and shutting down");
                if let Some(first) = built.symbols.first() {
                    rollback_all_positions(&built.order_manager, &first.client, self.alerter.as_deref()).await;
                }
                self.stop.set();
                Ok(())
            }
            Some(joined) = tasks.join_next() => match joined {
                Ok(TaskOutcome::Feed(symbol, result)) => {
                    if let Err(err) = &result {
                        tracing::error!(%symbol, error = %err, "Market data flow failed");
                    }
                    result
                }
                Ok(TaskOutcome::Execution(result)) => result,
                Err(join_error) => Err(RuntimeError::Task(join_error.to_string())),
            },
            () = self.stop.wait() => {
                tracing::info!("Shutdown signal received");
                Ok(())
            }
        };
        tasks.shutdown().await;
        result
    }

    async fn close(&self, built: &Built) {
        for sym in &built.symbols {
            sym.strategy.stop().await;
        }
        built.exchange.close().await;
        tracing::info!("Binance trading bot stopped");
    }
}

/// Continuously move closed candles from the feed into the strategy.
async fn stream_candles(
    feed: &dyn CandleSource,
    client: &StrategyOrderClient,
    strategy: &dyn Strategy,
    settings: &Settings,
) -> Result<(), RuntimeError> {
    use crate::strategies::OrderClient as _;
    let mut stream = feed
        .subscribe_ohlcv(&settings.trading_symbol, &settings.timeframe)
        .await?;
    let mut prev_close: Option<Decimal> = None;
    while let Some(candle) = stream.next().await {
        let candle = candle?;
        if let Some(prev) = prev_close.filter(|p| *p > Decimal::ZERO) {
            let deviation = (candle.close - prev).abs() / prev * dec!(100);
            if deviation > MAX_CLOSE_DEVIATION_PCT {
                tracing::warn!(
                    "Candle: {} close {} deviates {:.1}% from previous {}; skipping",
                    candle.symbol,
                    candle.close,
                    deviation,
                    prev
                );
                continue;
            }
        }
        prev_close = Some(candle.close);
        client.update_last_price(candle.close);
        tracing::info!(
            "Candle: {} O={} H={} L={} C={} V={}",
            candle.symbol,
            candle.open,
            candle.high,
            candle.low,
            candle.close,
            candle.volume
        );
        strategy.on_candle(&Candle::from(candle)).await?;
    }
    Ok(())
}

/// Continuously route private execution updates into local order state.
async fn stream_order_updates(
    source: &dyn ExecutionSource,
    router: &ExecutionUpdateRouter,
) -> Result<(), RuntimeError> {
    let mut stream = source.open().await?;
    while let Some(report) = stream.next().await {
        router.route(&report).await?;
    }
    Ok(())
}

/// Close every tracked position with an unsized market order.
async fn rollback_all_positions(
    order_manager: &OrderManager,
    client: &StrategyOrderClient,
    alerter: Option<&TelegramAlerter>,
) {
    let positions = order_manager.list_positions();
    if positions.is_empty() {
        tracing::info!("No open positions to rollback");
        return;
    }
    for position in positions {
        if position.amount.abs() < ROLLBACK_DUST {
            continue;
        }
        let side = if position.amount > Decimal::ZERO {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        };
        tracing::warn!(
            "ROLLBACK: Closing {} position of {} with {} order",
            position.symbol,
            position.amount,
            side
        );
        let order = StrategyOrder {
            symbol: position.symbol.clone(),
            order_type: OrderType::Market,
            side,
            amount: position.amount.abs(),
            price: None,
            size_order: false,
        };
        if let Err(err) = client.submit(order).await {
            tracing::error!("Failed to close position for {}: {err}", position.symbol);
            if let Some(alerter) = alerter {
                alerter
                    .alert_risk_breach(&format!("ROLLBACK FAILED for {}: {err}", position.symbol))
                    .await;
            }
        }
    }
}

/// Run attempts until one returns cleanly, restarting crashes with
/// exponential backoff (5 s doubling, capped at 60 s, 5 attempts).
pub async fn supervise<F, Fut>(
    alerter: Option<&TelegramAlerter>,
    stop: &Signal,
    mut attempt: F,
) -> Result<(), RuntimeError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), RuntimeError>>,
{
    for number in 1..=MAX_STARTUP_ATTEMPTS {
        let error = match attempt().await {
            Ok(()) => return Ok(()),
            Err(err) if err.is_fatal() => return Err(err),
            Err(err) => err,
        };
        if stop.is_set() {
            return Ok(());
        }
        if number >= MAX_STARTUP_ATTEMPTS {
            tracing::error!(error = %error, "Bot failed after {MAX_STARTUP_ATTEMPTS} attempts; giving up");
            if let Some(alerter) = alerter {
                alerter.alert_terminal_failure(&error.to_string()).await;
            }
            return Err(error);
        }
        let delay = BASE_RESTART_DELAY
            .saturating_mul(1 << (number - 1))
            .min(MAX_RESTART_DELAY);
        tracing::warn!(
            error = %error,
            "Bot crashed (attempt {number}/{MAX_STARTUP_ATTEMPTS}); restarting in {:.0}s",
            delay.as_secs_f64()
        );
        if let Some(alerter) = alerter {
            alerter
                .alert_crash(
                    number,
                    MAX_STARTUP_ATTEMPTS,
                    delay.as_secs_f64(),
                    &error.to_string(),
                )
                .await;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = stop.wait() => return Ok(()),
        }
    }
    Ok(())
}

/// Run the bot until a shutdown signal (SIGINT/SIGTERM) arrives; SIGUSR1
/// closes all positions and shuts down.
pub async fn main(
    settings: Settings,
    factory: Arc<dyn ComponentFactory>,
) -> Result<(), RuntimeError> {
    let alerter = Arc::new(TelegramAlerter::new(
        &settings.telegram_bot_token,
        &settings.telegram_chat_id,
    ));
    let stop = Signal::default();
    let rollback = Signal::default();
    install_signal_handlers(&stop, &rollback);
    supervise(Some(&alerter), &stop, || {
        let runtime = BotRuntime::new(
            settings.clone(),
            stop.clone(),
            rollback.clone(),
            factory.clone(),
        )
        .with_alerter(Some(alerter.clone()));
        async move { runtime.run().await }
    })
    .await
}

fn install_signal_handlers(stop: &Signal, rollback: &Signal) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        for (kind, name) in [
            (SignalKind::interrupt(), "SIGINT"),
            (SignalKind::terminate(), "SIGTERM"),
        ] {
            let Ok(mut stream) = signal(kind) else {
                continue;
            };
            let stop = stop.clone();
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    if !stop.is_set() {
                        tracing::info!(signal = name, "Shutdown requested");
                        stop.set();
                    }
                }
            });
        }
        if let Ok(mut stream) = signal(SignalKind::user_defined1()) {
            let rollback = rollback.clone();
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    if !rollback.is_set() {
                        tracing::warn!("Rollback (Panic) requested via SIGUSR1");
                        rollback.set();
                    }
                }
            });
        }
    }
    #[cfg(not(unix))]
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!(signal = "SIGINT", "Shutdown requested");
                stop.set();
            }
        });
        let _ = rollback;
    }
}

#[cfg(test)]
mod tests;

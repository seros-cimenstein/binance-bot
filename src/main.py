"""Runtime wiring for the asynchronous Binance trading bot."""

from __future__ import annotations

import asyncio
import inspect
import signal
import sys
import time
import uuid
from collections.abc import Callable
from dataclasses import asdict
from types import FrameType
from typing import Any

from loguru import logger

from src.config import Settings
from src.logger import setup_logger
from src.data.feeds import BinanceDataFeed, CandleData
from src.data.websocket_feed import BinanceWebSocketCandleFeed
from src.db import OrderDB
from src.execution.router import ExecutionUpdateRouter
from src.exchange.client import BinanceClient
from src.exchange.websocket import (
    BinanceWebSocketManager,
    EventType,
    ExecutionReportUpdate,
    StreamError,
    UserStreamExpired,
    WebSocketEvent,
)
from src.orders.manager import OrderManager
from src.orders.models import Order
from src.orders.sizing import (
    OrderSizer,
    OrderSizingConfig,
    OrderSizingError,
    OrderSizingRequest,
)
from src.portfolio import PortfolioSnapshotService
from src.risk.guard import MarketData, OrderRequest, RiskBreachError, RiskGuard
from src.strategies.demo_sma import DemoSMAStrategy
from src.strategies.loader import create_strategy
from src.telegram_alerts import TelegramAlerter


class StrategyOrderClient:
    """Client facade that routes strategy orders through OrderManager."""

    def __init__(
        self,
        rest_client: BinanceClient,
        order_manager: OrderManager,
        *,
        risk_guard: RiskGuard | None = None,
        alerter: TelegramAlerter | None = None,
        dry_run: bool = False,
        sizing_config: OrderSizingConfig | None = None,
        portfolio_service: Any | None = None,
        order_sizer: OrderSizer | None = None,
    ) -> None:
        self._rest_client = rest_client
        self._order_manager = order_manager
        self._risk_guard = risk_guard
        self._alerter = alerter
        self._dry_run = dry_run
        self._sizing_config = sizing_config or OrderSizingConfig()
        self._portfolio_service = portfolio_service
        self._order_sizer = order_sizer or OrderSizer()
        self._last_price: float | None = None
        self.exchange = rest_client.exchange

    def update_last_price(self, price: float) -> None:
        """Record the most recent market price for risk validation."""
        self._last_price = price

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str = "1m",
        limit: int | None = None,
    ) -> list[Any]:
        """Fetch historical candles for strategy warm-up."""
        kwargs: dict[str, Any] = {"timeframe": timeframe}
        if limit is not None:
            kwargs["limit"] = limit
        return await self._rest_client.request("fetch_ohlcv", symbol, **kwargs)

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
        *,
        size_order: bool = True,
    ) -> dict[str, Any]:
        """Validate, optionally simulate, and submit an order."""
        validation_price = price if price is not None else self._last_price
        market_price = self._last_price if self._last_price is not None else validation_price
        sized_order = None
        if size_order:
            try:
                sized_order = await self._order_sizer.size_order(
                    OrderSizingRequest(
                        symbol=symbol,
                        order_type=order_type,
                        side=side,
                        requested_amount=amount,
                        price=validation_price,
                    ),
                    self._sizing_config,
                    portfolio_service=self._portfolio_service,
                    exchange=self.exchange,
                )
            except OrderSizingError as exc:
                if self._alerter is not None:
                    await self._alerter.alert_risk_breach(str(exc))
                raise
            amount = sized_order.amount

        if self._dry_run:
            logger.bind(
                symbol=symbol,
                order_type=order_type,
                side=side,
                amount=amount,
                price=validation_price,
            ).info("DRY RUN: order not submitted")
            if self._alerter is not None:
                await self._alerter.alert_trade(
                    side=side,
                    symbol=symbol,
                    amount=amount,
                    price=validation_price,
                    dry_run=True,
                )
            return {
                "id": f"dry-{uuid.uuid4().hex[:8]}",
                "symbol": symbol,
                "type": order_type,
                "side": side,
                "amount": amount,
                "price": validation_price,
                "status": "closed",
                "filled": amount,
                "remaining": 0.0,
                "average": validation_price,
            }

        if self._risk_guard is not None:
            if validation_price is not None:
                try:
                    await self._validate_risk(
                        symbol=symbol,
                        order_type=order_type,
                        side=side,
                        amount=amount,
                        order_price=validation_price,
                        market_price=market_price or validation_price,
                        sized_order=sized_order,
                        allow_structured=size_order,
                    )
                except RiskBreachError as exc:
                    if self._alerter is not None:
                        await self._alerter.alert_risk_breach(str(exc))
                    raise
            else:
                logger.warning("RiskGuard skipped: no market price available yet")

        order = await self._order_manager.place_order(
            symbol=symbol,
            order_type=order_type,
            side=side,
            amount=amount,
            price=price,
        )
        if self._alerter is not None:
            await self._alerter.alert_trade(
                side=side,
                symbol=symbol,
                amount=order.filled_amount or amount,
                price=order.avg_fill_price or price,
            )
        return _order_to_dict(order)

    async def _validate_risk(
        self,
        *,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        order_price: float,
        market_price: float,
        sized_order: Any | None,
        allow_structured: bool,
    ) -> None:
        if self._risk_guard is None:
            return
        if allow_structured and self._should_use_structured_risk(sized_order):
            portfolio = getattr(sized_order, "portfolio", None)
            if portfolio is None and self._portfolio_service is not None:
                portfolio = await self._portfolio_service.snapshot(
                    mark_prices={symbol: market_price}
                )
            if portfolio is not None:
                await self._risk_guard.validate_order(
                    order=OrderRequest(
                        symbol=symbol,
                        side=side,
                        amount=amount,
                        price=order_price,
                        leverage=1.0,
                        order_type=order_type,
                    ),
                    portfolio=portfolio.to_risk_state(),
                    market_data=MarketData(symbol=symbol, mark_price=market_price),
                )
                return

        position = self._order_manager.get_position(symbol)
        current_position = position.amount if position is not None else 0.0
        await self._risk_guard.validate_order(
            symbol=symbol,
            side=side,
            amount=amount,
            price=order_price,
            current_position=current_position,
        )

    def _should_use_structured_risk(self, sized_order: Any | None) -> bool:
        if getattr(sized_order, "portfolio", None) is not None:
            return True
        config = getattr(self._risk_guard, "config", None)
        if config is None:
            return False
        return (
            getattr(config, "max_total_exposure_usd", None) is not None
            or getattr(config, "daily_drawdown_limit_pct", None) is not None
            or getattr(config, "allowed_leverage", None) is not None
            or getattr(config, "max_open_orders", None) is not None
            or getattr(config, "max_price_deviation_pct", None) is not None
            or getattr(config, "min_balance_usdt", 0.0) > 0
        )


class BotRuntime:
    """Own the bot component graph and lifecycle for one runtime attempt."""

    def __init__(
        self,
        settings: Settings,
        stop_event: asyncio.Event,
        rollback_event: asyncio.Event,
        *,
        alerter: TelegramAlerter | None = None,
        db_factory: Callable[[str], Any] | None = None,
        rest_client_factory: Callable[[Settings], Any] | None = None,
        order_manager_factory: Callable[[Any, Any], Any] | None = None,
        risk_guard_factory: Callable[[Settings, Any], Any] | None = None,
        strategy_client_factory: Callable[..., Any] | None = None,
        strategy_factory: Callable[[Any, Settings], Any] | None = None,
        data_feed_factory: Callable[[Any, Settings], Any] | None = None,
        execution_manager_factory: Callable[[Any], Any] | None = None,
        portfolio_service_factory: Callable[[Any, Any, Any], Any] | None = None,
        order_sizer_factory: Callable[[], Any] | None = None,
    ) -> None:
        self.settings = settings
        self.stop_event = stop_event
        self.rollback_event = rollback_event
        self.alerter = alerter

        self._db_factory = db_factory or OrderDB
        self._rest_client_factory = rest_client_factory or _default_rest_client
        self._order_manager_factory = order_manager_factory or _default_order_manager
        self._risk_guard_factory = risk_guard_factory or _default_risk_guard
        self._strategy_client_factory = strategy_client_factory or StrategyOrderClient
        self._strategy_factory = strategy_factory or _default_strategy
        self._data_feed_factory = data_feed_factory or _default_data_feed
        self._execution_manager_factory = (
            execution_manager_factory or _default_execution_manager
        )
        self._portfolio_service_factory = (
            portfolio_service_factory or _default_portfolio_service
        )
        self._order_sizer_factory = order_sizer_factory or _default_order_sizer

        self.db: Any | None = None
        self.rest_client: Any | None = None
        self.order_manager: Any | None = None
        self.risk_guard: Any | None = None
        self.strategy_client: Any | None = None
        self.strategy: Any | None = None
        self.strategy_clients: dict[str, Any] = {}
        self.strategies: dict[str, Any] = {}
        self.feeds: Any | None = None
        self.execution_manager: Any | None = None
        self.execution_router: ExecutionUpdateRouter | None = None
        self.portfolio_service: Any | None = None
        self.order_sizer: Any | None = None

        self.feed_task: asyncio.Task[None] | None = None
        self.feed_tasks: list[asyncio.Task[None]] = []
        self.execution_task: asyncio.Task[None] | None = None
        self.shutdown_task: asyncio.Task[None] | None = None
        self.rollback_task: asyncio.Task[None] | None = None
        self._stream_alert_last_sent: dict[str, float] = {}
        self._clock: Callable[[], float] = time.monotonic
        self._closed = False

    async def run(self) -> None:
        """Build components, run until a stop condition, then clean up."""
        self._ensure_live_trading_confirmed()

        try:
            self._build_components()
            await self._recover_state()
            self._log_started()
            if self.alerter is not None:
                await self.alerter.alert_started(self.mode)

            await self._run_until_stopped()
        finally:
            await self.close()

    @property
    def mode(self) -> str:
        """Return the active trading mode label used in logs and alerts."""
        if self.settings.dry_run:
            return "DRY_RUN"
        if not self.settings.use_testnet:
            return "LIVE"
        return "testnet"

    async def close(self) -> None:
        """Cancel runtime tasks and close owned components."""
        if self._closed:
            return
        self._closed = True

        tasks = [
            task
            for task in (
                *self.feed_tasks,
                self.execution_task,
                self.shutdown_task,
                self.rollback_task,
            )
            if task is not None
        ]
        for task in tasks:
            if not task.done():
                task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

        for strategy in self.strategies.values():
            await strategy.stop()
        if self.rest_client is not None:
            await self.rest_client.close()
        if self.db is not None:
            self.db.close()
        logger.info("Binance trading bot stopped")

    def _ensure_live_trading_confirmed(self) -> None:
        if self.settings.use_testnet:
            return
        if not self.settings.dry_run and self.settings.order_size_mode == "fixed":
            logger.critical(
                "Fixed-size live execution is disabled; use DRY_RUN=true or "
                "a reviewed balance-aware runtime"
            )
            sys.exit(1)
        if self.settings.confirm_live_trading != "yes":
            logger.critical(
                "Live trading requires CONFIRM_LIVE_TRADING=yes in environment"
            )
            sys.exit(1)
        logger.warning("LIVE TRADING MODE ACTIVE — real money is at risk")

    def _build_components(self) -> None:
        self.db = self._db_factory(self.settings.db_path)
        self.rest_client = self._rest_client_factory(self.settings)
        self.order_manager = self._order_manager_factory(self.rest_client, self.db)
        self.order_manager.restore_from_db()
        self.risk_guard = self._risk_guard_factory(self.settings, self.db)
        self.risk_guard.restore_daily_loss_from_db()
        self.portfolio_service = self._portfolio_service_factory(
            self.rest_client,
            self.order_manager,
            self.risk_guard,
        )
        self.order_sizer = self._order_sizer_factory()
        for symbol in self.settings.trading_symbols:
            symbol_settings = self.settings.for_symbol(symbol)
            strategy_client = self._strategy_client_factory(
                self.rest_client,
                self.order_manager,
                risk_guard=self.risk_guard,
                alerter=self.alerter,
                dry_run=self.settings.dry_run,
                sizing_config=OrderSizingConfig.from_settings(symbol_settings),
                portfolio_service=self.portfolio_service,
                order_sizer=self.order_sizer,
            )
            strategy = self._strategy_factory(strategy_client, symbol_settings)
            self.strategy_clients[symbol] = strategy_client
            self.strategies[symbol] = strategy

        first_symbol = self.settings.trading_symbols[0]
        self.strategy_client = self.strategy_clients[first_symbol]
        self.strategy = self.strategies[first_symbol]
        self.feeds = self._data_feed_factory(self.rest_client, self.settings)
        self.execution_router = ExecutionUpdateRouter(
            self.order_manager,
            self.strategies,
        )
        if self.settings.order_update_mode == "websocket":
            self.execution_manager = self._execution_manager_factory(self.rest_client)
        self._register_websocket_alerts()

    def _register_websocket_alerts(self) -> None:
        if self.alerter is None:
            return
        if self.settings.market_data_mode == "websocket":
            self._subscribe_stream_alerts(
                getattr(self.feeds, "manager", None),
                "market-data",
            )
        if self.settings.order_update_mode == "websocket":
            self._subscribe_stream_alerts(
                self.execution_manager,
                "execution",
                include_user_expiration=True,
            )

    def _subscribe_stream_alerts(
        self,
        manager: Any,
        category: str,
        *,
        include_user_expiration: bool = False,
    ) -> None:
        subscribe = getattr(manager, "subscribe", None)
        if subscribe is None or not callable(subscribe):
            return

        async def on_stream_error(event: WebSocketEvent) -> None:
            await self._alert_stream_event(category, event)

        subscribe(EventType.STREAM_ERROR, on_stream_error)
        if include_user_expiration:
            subscribe(EventType.USER_STREAM_EXPIRED, on_stream_error)

    async def _alert_stream_event(
        self,
        category: str,
        event: WebSocketEvent,
    ) -> None:
        if self.alerter is None:
            return
        payload = event.payload
        if isinstance(payload, StreamError):
            stream = f"{category}/{payload.stream}"
            reason = payload.message
            if payload.error_type:
                reason = f"{reason} ({payload.error_type})"
        elif isinstance(payload, UserStreamExpired):
            stream = f"{category}/user"
            reason = "Binance user-data stream expired"
        else:
            return

        now = self._clock()
        last_sent = self._stream_alert_last_sent.get(stream)
        cooldown = self.settings.ws_alert_cooldown_seconds
        if last_sent is not None and now - last_sent < cooldown:
            logger.bind(stream=stream, cooldown_seconds=cooldown).debug(
                "Suppressing repeated WebSocket Telegram alert during cooldown"
            )
            return
        self._stream_alert_last_sent[stream] = now
        await self.alerter.alert_stream_failure(stream, reason)

    async def _recover_state(self) -> None:
        assert self.order_manager is not None
        assert self.risk_guard is not None

        restored_orders = _list_component_items(self.order_manager, "list_orders")
        restored_active_orders = _list_component_items(
            self.order_manager,
            "list_active_orders",
        )
        reconciled_orders = []
        reconcile = getattr(self.order_manager, "reconcile_active_orders", None)
        if callable(reconcile) and restored_active_orders:
            reconciled_orders = await reconcile()

        active_orders = _list_component_items(
            self.order_manager,
            "list_active_orders",
        )
        positions = _list_positions(self.order_manager)
        logger.bind(
            restored_order_count=len(restored_orders),
            restored_active_order_count=len(restored_active_orders),
            reconciled_order_count=len(reconciled_orders),
            active_order_count=len(active_orders),
            active_order_ids=[getattr(order, "id", None) for order in active_orders],
            position_count=len(positions),
            position_symbols=[getattr(position, "symbol", None) for position in positions],
            current_daily_loss=getattr(self.risk_guard, "current_daily_loss", None),
        ).info("Startup recovery audit")

    def _log_started(self) -> None:
        strategy_names = sorted(
            {
                getattr(strategy, "name", strategy.__class__.__name__)
                for strategy in self.strategies.values()
            }
        )
        logger.bind(
            symbols=self.settings.trading_symbols,
            timeframe=self.settings.timeframe,
            testnet=self.settings.use_testnet,
            strategies=strategy_names,
            dry_run=self.settings.dry_run,
        ).info("Starting Binance trading bot")

    async def _run_until_stopped(self) -> None:
        assert self.feeds is not None
        assert self.order_manager is not None

        try:
            for strategy in self.strategies.values():
                await strategy.start()
            self.feed_tasks = [
                asyncio.create_task(
                    _stream_candles(
                        self.feeds,
                        strategy,
                        self.settings.for_symbol(symbol),
                    ),
                    name=f"binance-market-data-flow-{symbol.replace('/', '')}",
                )
                for symbol, strategy in self.strategies.items()
            ]
            self.feed_task = self.feed_tasks[0] if self.feed_tasks else None
            if self.execution_manager is not None:
                assert self.execution_router is not None
                self.execution_task = asyncio.create_task(
                    _stream_order_updates(
                        self.execution_manager,
                        self.execution_router,
                    ),
                    name="binance-order-update-flow",
                )
            self.shutdown_task = asyncio.create_task(
                self.stop_event.wait(),
                name="binance-shutdown-signal",
            )
            self.rollback_task = asyncio.create_task(
                self.rollback_event.wait(),
                name="binance-rollback-signal",
            )

            done, pending = await asyncio.wait(
                {
                    *self.feed_tasks,
                    *([self.execution_task] if self.execution_task is not None else []),
                    self.shutdown_task,
                    self.rollback_task,
                },
                return_when=asyncio.FIRST_COMPLETED,
            )

            if self.rollback_task in done:
                logger.warning(
                    "ROLLBACK SIGNAL RECEIVED: Closing all positions and shutting down"
                )
                await _rollback_all_positions(
                    self.order_manager,
                    self.strategy_client,
                    self.alerter,
                )
                self.stop_event.set()
            elif any(task in done for task in self.feed_tasks):
                for task in done:
                    if task in self.feed_tasks:
                        task.result()
            elif self.execution_task in done:
                self.execution_task.result()
            else:
                logger.info("Shutdown signal received")

            for task in pending:
                task.cancel()
            if pending:
                await asyncio.gather(*pending, return_exceptions=True)
        except asyncio.CancelledError:
            logger.info("Bot task cancelled")
            raise


def _default_rest_client(settings: Settings) -> BinanceClient:
    return BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
    )


def _default_order_manager(rest_client: Any, db: Any) -> OrderManager:
    return OrderManager(rest_client, db=db)


def _default_risk_guard(settings: Settings, db: Any) -> RiskGuard:
    return RiskGuard(
        max_position_size_usd=settings.max_position_size_usd,
        daily_loss_limit_usd=settings.daily_loss_limit_usd,
        max_total_exposure_usd=settings.max_total_exposure_usd,
        daily_drawdown_limit_pct=settings.daily_drawdown_limit_pct,
        min_balance_usdt=settings.min_balance_usdt,
        allowed_leverage=settings.allowed_leverage,
        max_open_orders=settings.max_open_orders,
        max_price_deviation_pct=settings.max_price_deviation_pct,
        db=db,
    )


def _default_portfolio_service(
    rest_client: Any,
    order_manager: Any,
    risk_guard: Any,
) -> PortfolioSnapshotService:
    return PortfolioSnapshotService(rest_client, order_manager, risk_guard)


def _default_order_sizer() -> OrderSizer:
    return OrderSizer()


def _default_strategy(
    strategy_client: StrategyOrderClient,
    settings: Settings,
) -> Any:
    return create_strategy(settings.strategy_name, strategy_client, settings)


def _default_data_feed(
    rest_client: Any,
    settings: Settings,
) -> BinanceDataFeed | BinanceWebSocketCandleFeed:
    if settings.market_data_mode == "websocket":
        return BinanceWebSocketCandleFeed(
            BinanceWebSocketManager(rest_client=rest_client),
            queue_maxsize=settings.feed_queue_maxsize,
        )
    return BinanceDataFeed(
        rest_client.exchange,
        queue_maxsize=settings.feed_queue_maxsize,
    )


def _default_execution_manager(rest_client: Any) -> BinanceWebSocketManager:
    return BinanceWebSocketManager(rest_client=rest_client)


def _list_component_items(component: Any, method_name: str) -> list[Any]:
    method = getattr(component, method_name, None)
    if method is None or not callable(method):
        return []
    return list(method())


def _list_positions(order_manager: Any) -> list[Any]:
    positions = _list_component_items(order_manager, "list_positions")
    if positions:
        return positions
    return _list_component_items(order_manager, "get_positions")


async def main(settings: Settings | None = None) -> None:
    """Run the bot until a shutdown signal is received."""
    settings = settings or Settings.from_env()
    setup_logger(settings.log_level, "logs/bot.log")

    alerter = TelegramAlerter(settings.telegram_bot_token, settings.telegram_chat_id)
    stop_event = asyncio.Event()
    rollback_event = asyncio.Event()
    _install_signal_handlers(stop_event, rollback_event)

    _MAX_STARTUP_ATTEMPTS = 5
    _BASE_RESTART_DELAY = 5.0

    for attempt in range(1, _MAX_STARTUP_ATTEMPTS + 1):
        try:
            await run_bot(settings, stop_event, rollback_event, alerter=alerter)
            return
        except (asyncio.CancelledError, KeyboardInterrupt):
            raise
        except Exception as exc:
            if stop_event.is_set():
                return
            if attempt >= _MAX_STARTUP_ATTEMPTS:
                logger.opt(exception=exc).error(
                    f"Bot failed after {_MAX_STARTUP_ATTEMPTS} attempts; giving up"
                )
                if alerter:
                    await alerter.alert_terminal_failure(str(exc))
                raise
            delay = min(_BASE_RESTART_DELAY * 2 ** (attempt - 1), 60.0)
            logger.opt(exception=exc).warning(
                f"Bot crashed (attempt {attempt}/{_MAX_STARTUP_ATTEMPTS}); restarting in {delay:.0f}s"
            )
            if alerter:
                await alerter.alert_crash(
                    attempt=attempt,
                    max_attempts=_MAX_STARTUP_ATTEMPTS,
                    delay=delay,
                    error=str(exc),
                )
            await asyncio.sleep(delay)


async def run_bot(
    settings: Settings,
    stop_event: asyncio.Event,
    rollback_event: asyncio.Event,
    *,
    alerter: TelegramAlerter | None = None,
    runtime_factory: Callable[..., BotRuntime] = BotRuntime,
    **runtime_kwargs: Any,
) -> None:
    """Initialize feeds, strategy, and order manager, then run the data loop."""
    runtime = runtime_factory(
        settings,
        stop_event,
        rollback_event,
        alerter=alerter,
        **runtime_kwargs,
    )
    await runtime.run()


async def _rollback_all_positions(
    order_manager: OrderManager,
    client: StrategyOrderClient,
    alerter: TelegramAlerter | None = None
) -> None:
    """Close all open positions tracked by the order manager."""
    positions = order_manager.get_positions()
    if not positions:
        logger.info("No open positions to rollback")
        return

    for pos in positions:
        if abs(pos.amount) < 1e-8:
            continue

        side = "sell" if pos.amount > 0 else "buy"
        amount = abs(pos.amount)
        logger.warning(f"ROLLBACK: Closing {pos.symbol} position of {pos.amount} with {side} order")

        try:
            await client.create_order(
                symbol=pos.symbol,
                order_type="market",
                side=side,
                amount=amount,
                **_create_order_kwargs(client, size_order=False),
            )
        except Exception as e:
            logger.error(f"Failed to close position for {pos.symbol}: {e}")
            if alerter:
                await alerter.alert_risk_breach(f"ROLLBACK FAILED for {pos.symbol}: {e}")


_MAX_CLOSE_DEVIATION_PCT = 10.0


async def _stream_candles(
    feeds: BinanceDataFeed,
    strategy: DemoSMAStrategy,
    settings: Settings,
) -> None:
    """Continuously move OHLCV updates from feeds into the strategy."""
    prev_close: float | None = None
    async for candle in feeds.subscribe_ohlcv(
        settings.trading_symbol,
        timeframe=settings.timeframe,
    ):
        if prev_close is not None and prev_close > 0:
            deviation_pct = abs(candle.close - prev_close) / prev_close * 100.0
            if deviation_pct > _MAX_CLOSE_DEVIATION_PCT:
                logger.warning(
                    f"Candle: {candle.symbol} close {candle.close} deviates "
                    f"{deviation_pct:.1f}% from previous {prev_close}; skipping"
                )
                continue
        prev_close = candle.close
        strategy.client.update_last_price(candle.close)
        logger.info(f"Candle: {candle.symbol} O={candle.open} H={candle.high} L={candle.low} C={candle.close} V={candle.volume}")
        await strategy.on_candle(_candle_to_strategy_payload(candle))


async def _stream_order_updates(
    manager: BinanceWebSocketManager,
    router: ExecutionUpdateRouter,
) -> None:
    """Continuously route private execution updates into local order state."""
    queue = manager.subscribe_queue(EventType.ORDER_UPDATE)
    await manager.start(user_stream=True)
    try:
        while True:
            event = await queue.get()
            try:
                if (
                    event.event_type is EventType.ORDER_UPDATE
                    and isinstance(event.payload, ExecutionReportUpdate)
                ):
                    await router.route(event.payload)
            finally:
                queue.task_done()
    finally:
        manager.unsubscribe_queue(EventType.ORDER_UPDATE, queue)
        await manager.stop()


def _create_order_kwargs(client: Any, **kwargs: Any) -> dict[str, Any]:
    create_order = getattr(client, "create_order", None)
    if create_order is None:
        return {}
    return {
        name: value
        for name, value in kwargs.items()
        if _call_accepts_parameter(create_order, name)
    }


def _call_accepts_parameter(callable_object: Any, parameter_name: str) -> bool:
    try:
        signature = inspect.signature(callable_object)
    except (TypeError, ValueError):
        return True
    return parameter_name in signature.parameters


def _install_signal_handlers(stop_event: asyncio.Event, rollback_event: asyncio.Event) -> None:
    """Install SIGINT/SIGTERM handlers for shutdown and SIGUSR1 for rollback."""
    loop = asyncio.get_running_loop()

    def request_shutdown(signum: int, _: FrameType | None = None) -> None:
        signal_name = signal.Signals(signum).name
        if stop_event.is_set():
            return
        logger.bind(signal=signal_name).info("Shutdown requested")
        stop_event.set()

    def request_rollback(signum: int, _: FrameType | None = None) -> None:
        if rollback_event.is_set():
            return
        logger.warning("Rollback (Panic) requested via SIGUSR1")
        rollback_event.set()

    for signal_name in ("SIGINT", "SIGTERM"):
        signum = getattr(signal, signal_name, None)
        if signum is not None:
            try:
                loop.add_signal_handler(signum, request_shutdown, signum)
            except (NotImplementedError, RuntimeError):
                pass

    sigusr1 = getattr(signal, "SIGUSR1", None)
    if sigusr1 is not None:
        try:
            loop.add_signal_handler(sigusr1, request_rollback, sigusr1)
        except (NotImplementedError, RuntimeError):
            pass


def _candle_to_strategy_payload(candle: CandleData) -> dict[str, Any]:
    """Convert feed candles into the mapping shape accepted by the strategy."""
    payload = asdict(candle)
    payload["closed"] = True
    return payload


def _order_to_dict(order: Order) -> dict[str, Any]:
    """Return a ccxt-like order mapping from an OrderManager snapshot."""
    return {
        "id": order.id,
        "symbol": order.symbol,
        "type": order.type.value,
        "side": order.side.value,
        "amount": order.amount,
        "price": order.price,
        "status": order.status.value,
        "filled": order.filled_amount,
        "remaining": order.remaining_amount,
        "average": order.avg_fill_price,
    }


def run() -> int:
    """Synchronous console entry point."""
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

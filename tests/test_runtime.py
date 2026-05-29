from __future__ import annotations

import asyncio
from datetime import datetime, timezone
from types import SimpleNamespace
from typing import Any

import pytest

import main as entrypoint
import src.main as main_module
from src.config import Settings
from src.data.feeds import BinanceDataFeed, CandleData
from src.data.websocket_feed import BinanceWebSocketCandleFeed
from src.exchange.websocket import EventType, StreamError, UserStreamExpired, WebSocketEvent
from src.main import BotRuntime, StrategyOrderClient, _default_data_feed, run_bot
from src.orders.models import Position
from src.orders.sizing import OrderSizingConfig
from src.portfolio import PortfolioSnapshot
from src.risk.guard import RiskBreachError, RiskGuard
from src.strategies.demo_sma import DemoSMAStrategy


def _settings(**overrides: Any) -> Settings:
    values: dict[str, Any] = {
        "api_key": "test-key",
        "api_secret": "test-secret",
    }
    values.update(overrides)
    return Settings(**values)


class FakeDB:
    def __init__(self) -> None:
        self.closed = False

    def close(self) -> None:
        self.closed = True


class FakeRestClient:
    def __init__(self) -> None:
        self.exchange = object()
        self.closed = False

    async def close(self) -> None:
        self.closed = True


class FakeOrderManager:
    def __init__(
        self,
        positions: list[Position] | None = None,
        orders: list[Any] | None = None,
        active_orders: list[Any] | None = None,
    ) -> None:
        self.positions = positions or []
        self.orders = orders or []
        self.active_orders = active_orders or []
        self.reconcile_called = False
        self.restore_called = False

    def restore_from_db(self) -> None:
        self.restore_called = True

    def get_positions(self) -> list[Position]:
        return list(self.positions)

    def list_positions(self) -> list[Position]:
        return list(self.positions)

    def list_orders(self) -> list[Any]:
        return list(self.orders)

    def list_active_orders(self) -> list[Any]:
        return list(self.active_orders)

    async def reconcile_active_orders(self) -> list[Any]:
        self.reconcile_called = True
        return list(self.active_orders)


class FakeRiskGuard:
    def __init__(self) -> None:
        self.restore_called = False
        self.current_daily_loss = 0.0

    def restore_daily_loss_from_db(self) -> None:
        self.restore_called = True


class FakeStrategyClient:
    def __init__(self) -> None:
        self.orders: list[dict[str, Any]] = []
        self.last_price: float | None = None

    def update_last_price(self, price: float) -> None:
        self.last_price = price

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
        price: float | None = None,
    ) -> dict[str, Any]:
        order = {
            "symbol": symbol,
            "type": order_type,
            "side": side,
            "amount": amount,
            "price": price,
        }
        self.orders.append(order)
        return order


class FakeStrategy:
    name = "fake-strategy"

    def __init__(self, client: FakeStrategyClient) -> None:
        self.client = client
        self.started = False
        self.stopped = False

    async def start(self) -> None:
        self.started = True

    async def stop(self) -> None:
        self.stopped = True


class WaitingFeed:
    def __init__(self) -> None:
        self.subscriptions: list[tuple[str, str]] = []
        self.cancelled = False

    async def subscribe_ohlcv(self, symbol: str, timeframe: str = "1m") -> Any:
        self.subscriptions.append((symbol, timeframe))
        try:
            while True:
                await asyncio.sleep(60)
                if False:
                    yield None
        except asyncio.CancelledError:
            self.cancelled = True
            raise


class OneCandleFeed:
    def __init__(self) -> None:
        self.subscriptions: list[tuple[str, str]] = []

    async def subscribe_ohlcv(self, symbol: str, timeframe: str = "1m") -> Any:
        self.subscriptions.append((symbol, timeframe))
        yield CandleData(
            symbol=symbol,
            timestamp=1,
            open=100.0,
            high=101.0,
            low=99.0,
            close=100.0,
            volume=1.0,
        )
        while True:
            await asyncio.sleep(60)


class WaitingExecutionManager:
    def __init__(self) -> None:
        self.queue: asyncio.Queue[Any] = asyncio.Queue()
        self.started = asyncio.Event()
        self.start_user_stream_values: list[bool] = []
        self.unsubscribed = False
        self.stopped = False

    def subscribe_queue(self, _: Any) -> asyncio.Queue[Any]:
        return self.queue

    def unsubscribe_queue(self, _: Any, queue: asyncio.Queue[Any]) -> None:
        assert queue is self.queue
        self.unsubscribed = True

    async def start(self, *, user_stream: bool = True) -> None:
        self.start_user_stream_values.append(user_stream)
        self.started.set()

    async def stop(self) -> None:
        self.stopped = True


class AlertingManager:
    def __init__(self) -> None:
        self.callbacks: dict[EventType, list[Any]] = {}

    def subscribe(self, event_type: EventType, callback: Any) -> None:
        self.callbacks.setdefault(event_type, []).append(callback)

    async def emit(self, event: WebSocketEvent) -> None:
        for callback in self.callbacks.get(event.event_type, []):
            await callback(event)


class AlertingFeed(WaitingFeed):
    def __init__(self, manager: AlertingManager) -> None:
        super().__init__()
        self.manager = manager


class StaticPortfolioService:
    def __init__(self, snapshot: PortfolioSnapshot) -> None:
        self._snapshot = snapshot
        self.mark_prices: list[dict[str, float]] = []

    async def snapshot(self, *, mark_prices: dict[str, float]) -> PortfolioSnapshot:
        self.mark_prices.append(mark_prices)
        return self._snapshot


class FakeAlerter:
    def __init__(self) -> None:
        self.started_modes: list[str] = []
        self.risk_breaches: list[str] = []
        self.crashes: list[dict[str, Any]] = []
        self.trades: list[dict[str, Any]] = []
        self.stream_failures: list[tuple[str, str]] = []
        self.terminal_failures: list[str] = []

    async def alert_started(self, mode: str) -> None:
        self.started_modes.append(mode)

    async def alert_risk_breach(self, reason: str) -> None:
        self.risk_breaches.append(reason)

    async def alert_trade(
        self,
        *,
        side: str,
        symbol: str,
        amount: float,
        price: float | None,
        dry_run: bool = False,
    ) -> None:
        self.trades.append(
            {
                "side": side,
                "symbol": symbol,
                "amount": amount,
                "price": price,
                "dry_run": dry_run,
            }
        )

    async def alert_crash(
        self,
        attempt: int,
        max_attempts: int,
        delay: float,
        error: str,
    ) -> None:
        self.crashes.append(
            {
                "attempt": attempt,
                "max_attempts": max_attempts,
                "delay": delay,
                "error": error,
            }
        )

    async def alert_stream_failure(self, stream: str, reason: str) -> None:
        self.stream_failures.append((stream, reason))

    async def alert_terminal_failure(self, error: str) -> None:
        self.terminal_failures.append(error)


def _runtime(
    *,
    settings: Settings | None = None,
    stop_event: asyncio.Event | None = None,
    rollback_event: asyncio.Event | None = None,
    db: FakeDB | None = None,
    rest_client: FakeRestClient | None = None,
    order_manager: FakeOrderManager | None = None,
    risk_guard: FakeRiskGuard | None = None,
    strategy_client: FakeStrategyClient | None = None,
    strategy: FakeStrategy | None = None,
    feed: WaitingFeed | None = None,
    data_feed_factory: Any | None = None,
    execution_manager_factory: Any | None = None,
    alerter: FakeAlerter | None = None,
) -> tuple[BotRuntime, dict[str, Any]]:
    db = db or FakeDB()
    rest_client = rest_client or FakeRestClient()
    order_manager = order_manager or FakeOrderManager()
    risk_guard = risk_guard or FakeRiskGuard()
    strategy_client = strategy_client or FakeStrategyClient()
    strategy = strategy or FakeStrategy(strategy_client)
    feed = feed or WaitingFeed()
    components = {
        "db": db,
        "rest_client": rest_client,
        "order_manager": order_manager,
        "risk_guard": risk_guard,
        "strategy_client": strategy_client,
        "strategy": strategy,
        "feed": feed,
        "alerter": alerter,
    }
    runtime = BotRuntime(
        settings or _settings(),
        stop_event or asyncio.Event(),
        rollback_event or asyncio.Event(),
        alerter=alerter,
        db_factory=lambda _: db,
        rest_client_factory=lambda _: rest_client,
        order_manager_factory=lambda *_: order_manager,
        risk_guard_factory=lambda *_: risk_guard,
        strategy_client_factory=lambda *_, **__: strategy_client,
        strategy_factory=lambda *_: strategy,
        data_feed_factory=data_feed_factory or (lambda *_: feed),
        execution_manager_factory=execution_manager_factory,
    )
    return runtime, components


def test_repository_entrypoint_exports_runtime_and_legacy_settings_alias() -> None:
    assert entrypoint.BotRuntime is BotRuntime
    assert entrypoint.BotSettings is Settings
    assert entrypoint.StrategyOrderClient is StrategyOrderClient


def test_simple_exchange_import_does_not_load_environment() -> None:
    import src.simple_exchange as simple_exchange

    assert simple_exchange.BinanceClient is not None


def test_default_data_feed_respects_market_data_mode() -> None:
    rest_client = FakeRestClient()

    polling_feed = _default_data_feed(
        rest_client,
        _settings(market_data_mode="polling"),
    )
    websocket_feed = _default_data_feed(
        rest_client,
        _settings(market_data_mode="websocket"),
    )

    assert isinstance(polling_feed, BinanceDataFeed)
    assert polling_feed.exchange is rest_client.exchange
    assert isinstance(websocket_feed, BinanceWebSocketCandleFeed)


def test_default_risk_guard_uses_expanded_risk_settings() -> None:
    guard = main_module._default_risk_guard(
        _settings(
            max_total_exposure_usd=2_000.0,
            daily_drawdown_limit_pct=5.0,
            min_balance_usdt=100.0,
            allowed_leverage=1.0,
            max_open_orders=3,
            max_price_deviation_pct=2.0,
        ),
        FakeDB(),
    )

    assert guard.config.max_total_exposure_usd == 2_000.0
    assert guard.config.daily_drawdown_limit_pct == 5.0
    assert guard.config.min_balance_usdt == 100.0
    assert guard.config.allowed_leverage == 1.0
    assert guard.config.max_open_orders == 3
    assert guard.config.max_price_deviation_pct == 2.0


def test_default_strategy_uses_configured_strategy_loader() -> None:
    strategy = main_module._default_strategy(
        FakeStrategyClient(),
        _settings(strategy_name="demo-sma", fast_period=3, slow_period=8),
    )

    assert isinstance(strategy, DemoSMAStrategy)
    assert strategy.settings.fast_period == 3
    assert strategy.settings.slow_period == 8


async def test_runtime_rejects_unconfirmed_live_trading_before_setup() -> None:
    runtime, components = _runtime(
        settings=_settings(use_testnet=False, confirm_live_trading="no")
    )

    with pytest.raises(SystemExit) as exc_info:
        await runtime.run()

    assert exc_info.value.code == 1
    assert components["order_manager"].restore_called is False
    assert components["rest_client"].closed is False
    assert components["db"].closed is False


async def test_runtime_rejects_confirmed_fixed_size_live_trading() -> None:
    runtime, components = _runtime(
        settings=_settings(
            use_testnet=False,
            dry_run=False,
            confirm_live_trading="yes",
            order_size_mode="fixed",
        )
    )

    with pytest.raises(SystemExit) as exc_info:
        await runtime.run()

    assert exc_info.value.code == 1
    assert components["order_manager"].restore_called is False


async def test_runtime_cleans_up_components_when_setup_fails() -> None:
    def fail_feed_factory(*_: Any) -> WaitingFeed:
        raise RuntimeError("feed setup failed")

    runtime, components = _runtime(data_feed_factory=fail_feed_factory)

    with pytest.raises(RuntimeError, match="feed setup failed"):
        await runtime.run()

    assert components["order_manager"].restore_called is True
    assert components["risk_guard"].restore_called is True
    assert components["strategy"].stopped is True
    assert components["rest_client"].closed is True
    assert components["db"].closed is True


async def test_runtime_reconciles_active_orders_during_recovery() -> None:
    stop_event = asyncio.Event()
    stop_event.set()
    active_order = SimpleNamespace(id="restored-open")
    order_manager = FakeOrderManager(
        orders=[SimpleNamespace(id="restored-open"), SimpleNamespace(id="closed")],
        active_orders=[active_order],
        positions=[
            Position(
                symbol="BTC/USDT",
                amount=0.1,
                entry_price=100.0,
                timestamp=datetime.now(timezone.utc),
            )
        ],
    )
    runtime, components = _runtime(
        stop_event=stop_event,
        order_manager=order_manager,
    )

    await runtime.run()

    assert components["order_manager"].restore_called is True
    assert components["risk_guard"].restore_called is True
    assert components["order_manager"].reconcile_called is True


async def test_runtime_reports_dry_run_mode_on_shutdown() -> None:
    stop_event = asyncio.Event()
    stop_event.set()
    alerter = FakeAlerter()
    runtime, components = _runtime(
        settings=_settings(dry_run=True),
        stop_event=stop_event,
        alerter=alerter,
    )

    await runtime.run()

    assert components["strategy"].started is True
    assert components["strategy"].stopped is True
    assert components["feed"].cancelled is True
    assert components["strategy_client"].orders == []
    assert alerter.started_modes == ["DRY_RUN"]


async def test_runtime_starts_one_strategy_per_configured_symbol() -> None:
    stop_event = asyncio.Event()
    feed = WaitingFeed()
    clients: list[FakeStrategyClient] = []
    strategies: list[FakeStrategy] = []

    def build_strategy_client(*_: Any, **__: Any) -> FakeStrategyClient:
        client = FakeStrategyClient()
        clients.append(client)
        return client

    def build_strategy(
        client: FakeStrategyClient,
        settings: Settings,
    ) -> FakeStrategy:
        strategy = FakeStrategy(client)
        strategy.symbol = settings.trading_symbol
        strategies.append(strategy)
        return strategy

    runtime = BotRuntime(
        _settings(trading_symbols=("BTC/USDT", "ETH/USDT")),
        stop_event,
        asyncio.Event(),
        db_factory=lambda _: FakeDB(),
        rest_client_factory=lambda _: FakeRestClient(),
        order_manager_factory=lambda *_: FakeOrderManager(),
        risk_guard_factory=lambda *_: FakeRiskGuard(),
        strategy_client_factory=build_strategy_client,
        strategy_factory=build_strategy,
        data_feed_factory=lambda *_: feed,
    )

    task = asyncio.create_task(runtime.run())
    for _ in range(20):
        if len(feed.subscriptions) == 2:
            break
        await asyncio.sleep(0.01)

    stop_event.set()
    await asyncio.wait_for(task, 1.0)

    assert [strategy.symbol for strategy in strategies] == ["BTC/USDT", "ETH/USDT"]
    assert len(clients) == 2
    assert all(strategy.started for strategy in strategies)
    assert all(strategy.stopped for strategy in strategies)
    assert set(feed.subscriptions) == {("BTC/USDT", "1m"), ("ETH/USDT", "1m")}


async def test_runtime_keeps_multi_symbol_order_clients_isolated() -> None:
    stop_event = asyncio.Event()
    feed = OneCandleFeed()
    clients: list[FakeStrategyClient] = []

    class OrderingStrategy(FakeStrategy):
        def __init__(self, client: FakeStrategyClient, symbol: str) -> None:
            super().__init__(client)
            self.symbol = symbol
            self.ordered = False

        async def on_candle(self, _: dict[str, Any]) -> None:
            if self.ordered:
                return
            self.ordered = True
            await self.client.create_order(
                symbol=self.symbol,
                order_type="market",
                side="buy",
                amount=1.0,
            )

    def build_strategy_client(*_: Any, **__: Any) -> FakeStrategyClient:
        client = FakeStrategyClient()
        clients.append(client)
        return client

    runtime = BotRuntime(
        _settings(trading_symbols=("BTC/USDT", "ETH/USDT")),
        stop_event,
        asyncio.Event(),
        db_factory=lambda _: FakeDB(),
        rest_client_factory=lambda _: FakeRestClient(),
        order_manager_factory=lambda *_: FakeOrderManager(),
        risk_guard_factory=lambda *_: FakeRiskGuard(),
        strategy_client_factory=build_strategy_client,
        strategy_factory=lambda client, settings: OrderingStrategy(
            client,
            settings.trading_symbol,
        ),
        data_feed_factory=lambda *_: feed,
    )

    task = asyncio.create_task(runtime.run())
    for _ in range(20):
        if len(clients) == 2 and all(client.orders for client in clients):
            break
        await asyncio.sleep(0.01)

    stop_event.set()
    await asyncio.wait_for(task, 1.0)

    assert clients[0] is not clients[1]
    assert clients[0].orders == [
        {
            "symbol": "BTC/USDT",
            "type": "market",
            "side": "buy",
            "amount": 1.0,
            "price": None,
        }
    ]
    assert clients[1].orders == [
        {
            "symbol": "ETH/USDT",
            "type": "market",
            "side": "buy",
            "amount": 1.0,
            "price": None,
        }
    ]


async def test_runtime_starts_execution_update_stream_when_enabled() -> None:
    stop_event = asyncio.Event()
    execution_manager = WaitingExecutionManager()
    runtime, _ = _runtime(
        settings=_settings(order_update_mode="websocket"),
        stop_event=stop_event,
        execution_manager_factory=lambda _: execution_manager,
    )

    task = asyncio.create_task(runtime.run())
    await asyncio.wait_for(execution_manager.started.wait(), 1.0)

    stop_event.set()
    await asyncio.wait_for(task, 1.0)

    assert execution_manager.start_user_stream_values == [True]
    assert execution_manager.unsubscribed is True
    assert execution_manager.stopped is True


async def test_runtime_alerts_for_enabled_websocket_streams_with_cooldown() -> None:
    times = iter([10.0, 20.0, 30.0, 50.0])
    market_manager = AlertingManager()
    execution_manager = AlertingManager()
    alerter = FakeAlerter()
    runtime, _ = _runtime(
        settings=_settings(
            market_data_mode="websocket",
            order_update_mode="websocket",
            ws_alert_cooldown_seconds=30.0,
        ),
        data_feed_factory=lambda *_: AlertingFeed(market_manager),
        execution_manager_factory=lambda _: execution_manager,
        alerter=alerter,
    )
    runtime._clock = lambda: next(times)
    runtime._build_components()

    public_error = WebSocketEvent(
        event_type=EventType.STREAM_ERROR,
        payload=StreamError(stream="public", message="disconnected"),
    )
    await market_manager.emit(public_error)
    await market_manager.emit(public_error)
    await execution_manager.emit(
        WebSocketEvent(
            event_type=EventType.USER_STREAM_EXPIRED,
            payload=UserStreamExpired(),
        )
    )
    await market_manager.emit(public_error)

    assert alerter.stream_failures == [
        ("market-data/public", "disconnected"),
        ("execution/user", "Binance user-data stream expired"),
        ("market-data/public", "disconnected"),
    ]


async def test_strategy_order_client_applies_dynamic_sizing_in_dry_run() -> None:
    alerter = FakeAlerter()
    portfolio_service = StaticPortfolioService(
        PortfolioSnapshot(
            available_balance_usdt=1_000.0,
            total_balance_usdt=1_000.0,
            total_equity_usdt=1_000.0,
        )
    )
    client = StrategyOrderClient(
        FakeRestClient(),
        FakeOrderManager(),
        alerter=alerter,
        dry_run=True,
        sizing_config=OrderSizingConfig(
            mode="quote_percent",
            order_size_percent=25.0,
        ),
        portfolio_service=portfolio_service,
    )
    client.update_last_price(100.0)

    order = await client.create_order("BTC/USDT", "market", "buy", 0.001)

    assert order["status"] == "closed"
    assert order["amount"] == 2.5
    assert order["filled"] == 2.5
    assert portfolio_service.mark_prices == [{"BTC/USDT": 100.0}]
    assert alerter.trades == [
        {
            "side": "buy",
            "symbol": "BTC/USDT",
            "amount": 2.5,
            "price": 100.0,
            "dry_run": True,
        }
    ]


async def test_strategy_order_client_can_bypass_sizing_for_position_closes() -> None:
    portfolio_service = StaticPortfolioService(
        PortfolioSnapshot(
            available_balance_usdt=1_000.0,
            total_balance_usdt=1_000.0,
            total_equity_usdt=1_000.0,
        )
    )
    client = StrategyOrderClient(
        FakeRestClient(),
        FakeOrderManager(),
        dry_run=True,
        sizing_config=OrderSizingConfig(
            mode="quote_percent",
            order_size_percent=50.0,
        ),
        portfolio_service=portfolio_service,
    )
    client.update_last_price(100.0)

    order = await client.create_order(
        "BTC/USDT",
        "market",
        "sell",
        0.25,
        size_order=False,
    )

    assert order["amount"] == 0.25
    assert portfolio_service.mark_prices == []


@pytest.mark.parametrize(
    (
        "risk_kwargs",
        "snapshot",
        "order_kwargs",
        "last_price",
        "expected_message",
    ),
    [
        pytest.param(
            {"max_total_exposure_usd": 600.0},
            PortfolioSnapshot(
                available_balance_usdt=1_000.0,
                total_balance_usdt=1_000.0,
                total_equity_usdt=2_000.0,
                positions={"ETH/USDT": 590.0},
                total_exposure_usdt=590.0,
            ),
            {
                "symbol": "BTC/USDT",
                "order_type": "market",
                "side": "buy",
                "amount": 0.2,
            },
            100.0,
            "maximum total exposure breached",
            id="total_exposure",
        ),
        pytest.param(
            {"max_open_orders": 1},
            PortfolioSnapshot(
                available_balance_usdt=1_000.0,
                total_balance_usdt=1_000.0,
                total_equity_usdt=1_000.0,
                open_orders_count=1,
            ),
            {
                "symbol": "BTC/USDT",
                "order_type": "market",
                "side": "buy",
                "amount": 0.1,
            },
            100.0,
            "maximum open orders breached",
            id="open_orders",
        ),
        pytest.param(
            {"min_balance_usdt": 950.0},
            PortfolioSnapshot(
                available_balance_usdt=1_000.0,
                total_balance_usdt=1_000.0,
                total_equity_usdt=1_000.0,
            ),
            {
                "symbol": "BTC/USDT",
                "order_type": "market",
                "side": "buy",
                "amount": 1.0,
            },
            100.0,
            "minimum balance breached",
            id="minimum_balance",
        ),
        pytest.param(
            {"allowed_leverage": 2.0},
            PortfolioSnapshot(
                available_balance_usdt=1_000.0,
                total_balance_usdt=1_000.0,
                total_equity_usdt=1_000.0,
            ),
            {
                "symbol": "BTC/USDT",
                "order_type": "market",
                "side": "buy",
                "amount": 0.1,
            },
            100.0,
            "leverage check breached",
            id="leverage",
        ),
        pytest.param(
            {"daily_drawdown_limit_pct": 5.0},
            PortfolioSnapshot(
                available_balance_usdt=1_000.0,
                total_balance_usdt=1_000.0,
                total_equity_usdt=1_000.0,
                realized_pnl_today_usdt=-60.0,
            ),
            {
                "symbol": "BTC/USDT",
                "order_type": "market",
                "side": "buy",
                "amount": 0.1,
            },
            100.0,
            "daily drawdown breached",
            id="daily_drawdown",
        ),
        pytest.param(
            {"max_price_deviation_pct": 5.0},
            PortfolioSnapshot(
                available_balance_usdt=1_000.0,
                total_balance_usdt=1_000.0,
                total_equity_usdt=1_000.0,
            ),
            {
                "symbol": "BTC/USDT",
                "order_type": "limit",
                "side": "buy",
                "amount": 0.1,
                "price": 120.0,
            },
            100.0,
            "price deviation breached",
            id="price_deviation",
        ),
    ],
)
async def test_strategy_order_client_applies_expanded_risk_rules_and_alerts(
    risk_kwargs: dict[str, Any],
    snapshot: PortfolioSnapshot,
    order_kwargs: dict[str, Any],
    last_price: float,
    expected_message: str,
) -> None:
    alerter = FakeAlerter()
    client = StrategyOrderClient(
        FakeRestClient(),
        FakeOrderManager(),
        risk_guard=RiskGuard(
            max_position_size_usd=10_000.0,
            daily_loss_limit_usd=10_000.0,
            **risk_kwargs,
        ),
        alerter=alerter,
        portfolio_service=StaticPortfolioService(snapshot),
        sizing_config=OrderSizingConfig(),
    )
    client.update_last_price(last_price)

    with pytest.raises(RiskBreachError, match=expected_message):
        await client.create_order(**order_kwargs)

    assert len(alerter.risk_breaches) == 1
    assert expected_message in alerter.risk_breaches[0]


async def test_runtime_rolls_back_positions_and_stops() -> None:
    rollback_event = asyncio.Event()
    rollback_event.set()
    alerter = FakeAlerter()
    order_manager = FakeOrderManager(
        positions=[
            Position(
                symbol="BTC/USDT",
                amount=0.25,
                entry_price=100.0,
                timestamp=datetime.now(timezone.utc),
            ),
            Position(
                symbol="ETH/USDT",
                amount=-1.5,
                entry_price=10.0,
                timestamp=datetime.now(timezone.utc),
            ),
            Position(
                symbol="BNB/USDT",
                amount=0.0,
                entry_price=1.0,
                timestamp=datetime.now(timezone.utc),
            ),
        ]
    )
    runtime, components = _runtime(
        rollback_event=rollback_event,
        order_manager=order_manager,
        alerter=alerter,
    )

    await runtime.run()

    assert components["strategy"].started is True
    assert components["strategy"].stopped is True
    assert components["feed"].cancelled is True
    assert components["rest_client"].closed is True
    assert components["db"].closed is True
    assert alerter.started_modes == ["testnet"]
    assert components["strategy_client"].orders == [
        {
            "symbol": "BTC/USDT",
            "type": "market",
            "side": "sell",
            "amount": 0.25,
            "price": None,
        },
        {
            "symbol": "ETH/USDT",
            "type": "market",
            "side": "buy",
            "amount": 1.5,
            "price": None,
        },
    ]


async def test_run_bot_uses_runtime_factory() -> None:
    calls: list[tuple[Settings, asyncio.Event, asyncio.Event, Any]] = []

    class RuntimeStub:
        def __init__(
            self,
            settings: Settings,
            stop_event: asyncio.Event,
            rollback_event: asyncio.Event,
            *,
            alerter: Any = None,
        ) -> None:
            calls.append((settings, stop_event, rollback_event, alerter))
            self.ran = False

        async def run(self) -> None:
            self.ran = True

    settings = _settings()
    stop_event = asyncio.Event()
    rollback_event = asyncio.Event()
    alerter = object()

    await run_bot(
        settings,
        stop_event,
        rollback_event,
        alerter=alerter,
        runtime_factory=RuntimeStub,
    )

    assert calls == [(settings, stop_event, rollback_event, alerter)]


async def test_main_restarts_after_runtime_crash(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    alerter = FakeAlerter()
    attempts: list[tuple[Settings, asyncio.Event, asyncio.Event, Any]] = []
    sleeps: list[float] = []

    async def fake_run_bot(
        settings: Settings,
        stop_event: asyncio.Event,
        rollback_event: asyncio.Event,
        *,
        alerter: Any,
    ) -> None:
        attempts.append((settings, stop_event, rollback_event, alerter))
        if len(attempts) == 1:
            raise RuntimeError("boom")

    async def fake_sleep(delay: float) -> None:
        sleeps.append(delay)

    monkeypatch.setattr(main_module, "setup_logger", lambda *_: None)
    monkeypatch.setattr(main_module, "TelegramAlerter", lambda *_: alerter)
    monkeypatch.setattr(main_module, "_install_signal_handlers", lambda *_: None)
    monkeypatch.setattr(main_module, "run_bot", fake_run_bot)
    monkeypatch.setattr(main_module.asyncio, "sleep", fake_sleep)

    settings = _settings()

    await main_module.main(settings)

    assert [attempt[0] for attempt in attempts] == [settings, settings]
    assert all(attempt[3] is alerter for attempt in attempts)
    assert sleeps == [5.0]
    assert alerter.crashes == [
        {
            "attempt": 1,
            "max_attempts": 5,
            "delay": 5.0,
            "error": "boom",
        }
    ]


async def test_main_alerts_after_terminal_runtime_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    alerter = FakeAlerter()

    async def fail_runtime(*_: Any, **__: Any) -> None:
        raise RuntimeError("still broken")

    async def fake_sleep(_: float) -> None:
        return None

    monkeypatch.setattr(main_module, "setup_logger", lambda *_: None)
    monkeypatch.setattr(main_module, "TelegramAlerter", lambda *_: alerter)
    monkeypatch.setattr(main_module, "_install_signal_handlers", lambda *_: None)
    monkeypatch.setattr(main_module, "run_bot", fail_runtime)
    monkeypatch.setattr(main_module.asyncio, "sleep", fake_sleep)

    with pytest.raises(RuntimeError, match="still broken"):
        await main_module.main(_settings())

    assert len(alerter.crashes) == 4
    assert alerter.terminal_failures == ["still broken"]

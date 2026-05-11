"""Runtime wiring for the asynchronous Binance trading bot."""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import threading
import uuid
from dataclasses import asdict, dataclass
from types import FrameType
from typing import Any

from dotenv import load_dotenv
from loguru import logger

from src.data.feeds import BinanceDataFeed, CandleData, normalize_symbol
from src.db import OrderDB
from src.exchange.client import BinanceClient
from src.orders.manager import OrderManager
from src.orders.models import Order
from src.risk.guard import RiskBreachError, RiskConfig, RiskGuard
from src.strategies.demo_sma import DemoSMAStrategy, SMASettings
from src.telegram_alerts import TelegramAlerter


DEFAULT_TRADING_SYMBOL = "BTC/USDT"
DEFAULT_ORDER_SIZE = 0.001
DEFAULT_TIMEFRAME = "1m"
DEFAULT_FAST_PERIOD = 20
DEFAULT_SLOW_PERIOD = 50
DEFAULT_USE_TESTNET = True
DEFAULT_LOG_LEVEL = "INFO"
DEFAULT_FEED_QUEUE_MAXSIZE = 1_000
DEFAULT_DRY_RUN = False
DEFAULT_MAX_POSITION_SIZE_USD = 500.0
DEFAULT_DAILY_LOSS_LIMIT_USD = 100.0
DEFAULT_DB_PATH = "data/bot.db"


@dataclass(frozen=True, slots=True)
class BotSettings:
    """Environment-backed runtime settings for the trading bot."""

    api_key: str
    api_secret: str
    trading_symbol: str = DEFAULT_TRADING_SYMBOL
    order_size: float = DEFAULT_ORDER_SIZE
    timeframe: str = DEFAULT_TIMEFRAME
    fast_period: int = DEFAULT_FAST_PERIOD
    slow_period: int = DEFAULT_SLOW_PERIOD
    use_testnet: bool = DEFAULT_USE_TESTNET
    log_level: str = DEFAULT_LOG_LEVEL
    feed_queue_maxsize: int = DEFAULT_FEED_QUEUE_MAXSIZE
    dry_run: bool = DEFAULT_DRY_RUN
    max_position_size_usd: float = DEFAULT_MAX_POSITION_SIZE_USD
    daily_loss_limit_usd: float = DEFAULT_DAILY_LOSS_LIMIT_USD
    telegram_bot_token: str = ""
    telegram_chat_id: str = ""
    db_path: str = DEFAULT_DB_PATH
    confirm_live_trading: str = "no"

    @classmethod
    def from_env(cls) -> "BotSettings":
        """Build settings from ``.env`` and process environment variables."""
        load_dotenv()

        api_key = _required_env("BINANCE_API_KEY")
        api_secret = _required_env("BINANCE_API_SECRET", "BINANCE_SECRET_KEY")

        return cls(
            api_key=api_key,
            api_secret=api_secret,
            trading_symbol=normalize_symbol(
                _env("TRADING_SYMBOL", default=DEFAULT_TRADING_SYMBOL)
            ),
            order_size=_float_env(
                "ORDER_SIZE",
                "TRADE_QUANTITY",
                default=DEFAULT_ORDER_SIZE,
            ),
            timeframe=_env("TRADING_TIMEFRAME", "TIMEFRAME", default=DEFAULT_TIMEFRAME),
            fast_period=_int_env("SMA_FAST_PERIOD", default=DEFAULT_FAST_PERIOD),
            slow_period=_int_env("SMA_SLOW_PERIOD", default=DEFAULT_SLOW_PERIOD),
            use_testnet=_bool_env("USE_TESTNET", default=DEFAULT_USE_TESTNET),
            log_level=_env("LOG_LEVEL", default=DEFAULT_LOG_LEVEL).upper(),
            feed_queue_maxsize=_int_env(
                "FEED_QUEUE_MAXSIZE",
                default=DEFAULT_FEED_QUEUE_MAXSIZE,
            ),
            dry_run=_bool_env("DRY_RUN", default=DEFAULT_DRY_RUN),
            max_position_size_usd=_float_env(
                "MAX_POSITION_SIZE_USD",
                default=DEFAULT_MAX_POSITION_SIZE_USD,
            ),
            daily_loss_limit_usd=_float_env(
                "DAILY_LOSS_LIMIT_USD",
                default=DEFAULT_DAILY_LOSS_LIMIT_USD,
            ),
            telegram_bot_token=_env("TELEGRAM_BOT_TOKEN", default=""),
            telegram_chat_id=_env("TELEGRAM_CHAT_ID", default=""),
            db_path=_env("DB_PATH", default=DEFAULT_DB_PATH),
            confirm_live_trading=_env("CONFIRM_LIVE_TRADING", default="no").strip().lower(),
        )


class StrategyOrderClient:
    """Client facade that routes strategy orders through ``OrderManager``."""

    def __init__(
        self,
        rest_client: BinanceClient,
        order_manager: OrderManager,
        *,
        risk_guard: RiskGuard | None = None,
        alerter: TelegramAlerter | None = None,
        dry_run: bool = False,
    ) -> None:
        self._rest_client = rest_client
        self._order_manager = order_manager
        self._risk_guard = risk_guard
        self._alerter = alerter
        self._dry_run = dry_run
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
    ) -> dict[str, Any]:
        """Validate, optionally simulate, and submit an order."""
        validation_price = price if price is not None else self._last_price

        if self._dry_run:
            logger.bind(
                symbol=symbol,
                order_type=order_type,
                side=side,
                amount=amount,
                price=validation_price,
            ).info("DRY RUN: order not submitted")
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
                position = self._order_manager.get_position(symbol)
                current_position = position.amount if position is not None else 0.0
                try:
                    await self._risk_guard.validate_order(
                        symbol=symbol,
                        side=side,
                        amount=amount,
                        price=validation_price,
                        current_position=current_position,
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
        return _order_to_dict(order)


async def main(settings: BotSettings | None = None) -> None:
    """Run the bot until a shutdown signal is received."""
    settings = settings or BotSettings.from_env()
    logger.remove()
    logger.add(
        sink=sys.stderr,
        level=settings.log_level,
        format=(
            "<green>{time:YYYY-MM-DD HH:mm:ss}</green> | "
            "<level>{level: <8}</level> | "
            "<cyan>{name}</cyan>:<cyan>{function}</cyan>:<cyan>{line}</cyan> - "
            "<level>{message}</level>"
        ),
    )

    alerter = TelegramAlerter(settings.telegram_bot_token, settings.telegram_chat_id)
    stop_event = asyncio.Event()
    _install_signal_handlers(stop_event)

    _MAX_STARTUP_ATTEMPTS = 5
    _BASE_RESTART_DELAY = 5.0

    for attempt in range(1, _MAX_STARTUP_ATTEMPTS + 1):
        try:
            await run_bot(settings, stop_event, alerter=alerter)
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
                raise
            delay = min(_BASE_RESTART_DELAY * 2 ** (attempt - 1), 60.0)
            logger.opt(exception=exc).warning(
                f"Bot crashed (attempt {attempt}/{_MAX_STARTUP_ATTEMPTS}); restarting in {delay:.0f}s"
            )
            await alerter.alert_crash(
                attempt=attempt,
                max_attempts=_MAX_STARTUP_ATTEMPTS,
                delay=delay,
                error=str(exc),
            )
            await asyncio.sleep(delay)


async def run_bot(
    settings: BotSettings,
    stop_event: asyncio.Event,
    *,
    alerter: TelegramAlerter | None = None,
) -> None:
    """Initialize feeds, strategy, and order manager, then run the data loop."""
    if not settings.use_testnet:
        if settings.confirm_live_trading != "yes":
            logger.critical(
                "Live trading requires CONFIRM_LIVE_TRADING=yes in environment"
            )
            sys.exit(1)
        logger.warning("LIVE TRADING MODE ACTIVE — real money is at risk")

    db = OrderDB(settings.db_path)
    rest_client = BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
    )
    order_manager = OrderManager(rest_client, db=db)
    order_manager.restore_from_db()
    risk_guard = RiskGuard(
        max_position_size_usd=settings.max_position_size_usd,
        daily_loss_limit_usd=settings.daily_loss_limit_usd,
        db=db,
    )
    risk_guard.restore_daily_loss_from_db()
    strategy_client = StrategyOrderClient(
        rest_client,
        order_manager,
        risk_guard=risk_guard,
        alerter=alerter,
        dry_run=settings.dry_run,
    )
    strategy = DemoSMAStrategy(
        strategy_client,
        SMASettings(
            symbol=settings.trading_symbol,
            fast_period=settings.fast_period,
            slow_period=settings.slow_period,
            timeframe=settings.timeframe,
            order_size=settings.order_size,
        ),
    )
    feeds = BinanceDataFeed(
        rest_client.exchange,
        queue_maxsize=settings.feed_queue_maxsize,
    )

    feed_task: asyncio.Task[None] | None = None
    shutdown_task: asyncio.Task[None] | None = None

    mode = "DRY_RUN" if settings.dry_run else ("LIVE" if not settings.use_testnet else "testnet")
    logger.bind(
        symbol=settings.trading_symbol,
        timeframe=settings.timeframe,
        testnet=settings.use_testnet,
        strategy=strategy.name,
        dry_run=settings.dry_run,
    ).info("Starting Binance trading bot")
    if settings.dry_run:
        logger.warning("DRY RUN mode enabled: signals will be logged but no orders submitted")

    if alerter is not None:
        await alerter.alert_started(mode)

    try:
        await strategy.start()
        feed_task = asyncio.create_task(
            _stream_candles(feeds, strategy, settings),
            name="binance-market-data-flow",
        )
        shutdown_task = asyncio.create_task(
            stop_event.wait(),
            name="binance-shutdown-signal",
        )

        done, pending = await asyncio.wait(
            {feed_task, shutdown_task},
            return_when=asyncio.FIRST_COMPLETED,
        )

        if feed_task in done:
            feed_task.result()
        else:
            logger.info("Shutdown signal received")

        for task in pending:
            task.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
    except asyncio.CancelledError:
        logger.info("Bot task cancelled")
        raise
    finally:
        if feed_task is not None and not feed_task.done():
            feed_task.cancel()
            await asyncio.gather(feed_task, return_exceptions=True)
        if shutdown_task is not None and not shutdown_task.done():
            shutdown_task.cancel()
            await asyncio.gather(shutdown_task, return_exceptions=True)

        await strategy.stop()
        await rest_client.close()
        db.close()
        logger.info("Binance trading bot stopped")


_MAX_CLOSE_DEVIATION_PCT = 10.0


async def _stream_candles(
    feeds: BinanceDataFeed,
    strategy: DemoSMAStrategy,
    settings: BotSettings,
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


def _install_signal_handlers(stop_event: asyncio.Event) -> None:
    """Install SIGINT/SIGTERM handlers that request graceful shutdown."""
    loop = asyncio.get_running_loop()

    def request_shutdown(signum: int, _: FrameType | None = None) -> None:
        signal_name = signal.Signals(signum).name
        if stop_event.is_set():
            return
        logger.bind(signal=signal_name).info("Shutdown requested")
        stop_event.set()

    for signal_name in ("SIGINT", "SIGTERM"):
        signum = getattr(signal, signal_name, None)
        if signum is None:
            continue
        try:
            loop.add_signal_handler(signum, request_shutdown, signum)
        except NotImplementedError:  # pragma: no cover - Windows fallback
            if threading.current_thread() is threading.main_thread():
                signal.signal(signum, request_shutdown)
        except RuntimeError:
            logger.bind(signal=signal_name).debug(
                "Signal handler installation skipped"
            )


def _candle_to_strategy_payload(candle: CandleData) -> dict[str, Any]:
    """Convert feed candles into the mapping shape accepted by the strategy."""
    payload = asdict(candle)
    payload["closed"] = True
    return payload


def _order_to_dict(order: Order) -> dict[str, Any]:
    """Return a ccxt-like order mapping from an ``OrderManager`` snapshot."""
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


def _required_env(*names: str) -> str:
    value = _env(*names, default="")
    if value:
        return value
    joined_names = " or ".join(names)
    raise RuntimeError(f"missing required environment variable: {joined_names}")


def _env(*names: str, default: str) -> str:
    for name in names:
        value = os.getenv(name)
        if value is not None and value.strip():
            return value.strip()
    return default


def _float_env(*names: str, default: float) -> float:
    value = _env(*names, default=str(default))
    try:
        return float(value)
    except ValueError as exc:
        joined_names = " or ".join(names)
        raise RuntimeError(f"{joined_names} must be a number") from exc


def _int_env(*names: str, default: int) -> int:
    value = _env(*names, default=str(default))
    try:
        return int(value)
    except ValueError as exc:
        joined_names = " or ".join(names)
        raise RuntimeError(f"{joined_names} must be an integer") from exc


def _bool_env(*names: str, default: bool) -> bool:
    value = _env(*names, default=str(default)).strip().lower()
    if value in {"1", "true", "t", "yes", "y", "on"}:
        return True
    if value in {"0", "false", "f", "no", "n", "off"}:
        return False
    joined_names = " or ".join(names)
    raise RuntimeError(f"{joined_names} must be a boolean")


def run() -> int:
    """Synchronous console entry point."""
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

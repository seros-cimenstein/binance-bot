"""Centralized configuration for the Binance trading bot."""

import math
import os
from dataclasses import dataclass, replace

from dotenv import load_dotenv

from src.data.feeds import normalize_symbol

DEFAULT_TRADING_SYMBOL = "BTC/USDT"
DEFAULT_ORDER_SIZE = 0.001
DEFAULT_ORDER_SIZE_MODE = "fixed"
DEFAULT_ORDER_SIZE_PERCENT = 1.0
DEFAULT_MIN_ORDER_NOTIONAL_USD = 0.0
DEFAULT_MAX_ORDER_NOTIONAL_USD = 0.0
DEFAULT_MIN_BALANCE_USDT = 0.0
DEFAULT_MAX_TOTAL_EXPOSURE_USD: float | None = None
DEFAULT_DAILY_DRAWDOWN_LIMIT_PCT: float | None = None
DEFAULT_ALLOWED_LEVERAGE: float | None = None
DEFAULT_MAX_OPEN_ORDERS: int | None = None
DEFAULT_MAX_PRICE_DEVIATION_PCT: float | None = None
DEFAULT_TIMEFRAME = "1m"
DEFAULT_FAST_PERIOD = 20
DEFAULT_SLOW_PERIOD = 50
DEFAULT_USE_TESTNET = True
DEFAULT_LOG_LEVEL = "INFO"
DEFAULT_FEED_QUEUE_MAXSIZE = 1_000
DEFAULT_DRY_RUN = False
DEFAULT_MARKET_DATA_MODE = "polling"
DEFAULT_STRATEGY = "demo-sma"
DEFAULT_ORDER_UPDATE_MODE = "disabled"
DEFAULT_MAX_POSITION_SIZE_USD = 500.0
DEFAULT_DAILY_LOSS_LIMIT_USD = 100.0
DEFAULT_DB_PATH = "data/bot.db"


@dataclass(frozen=True, slots=True)
class Settings:
    """Environment-backed runtime settings for the trading bot."""

    api_key: str
    api_secret: str
    trading_symbol: str = DEFAULT_TRADING_SYMBOL
    trading_symbols: tuple[str, ...] = ()
    order_size: float = DEFAULT_ORDER_SIZE
    order_size_mode: str = DEFAULT_ORDER_SIZE_MODE
    order_size_percent: float = DEFAULT_ORDER_SIZE_PERCENT
    min_order_notional_usd: float = DEFAULT_MIN_ORDER_NOTIONAL_USD
    max_order_notional_usd: float = DEFAULT_MAX_ORDER_NOTIONAL_USD
    min_balance_usdt: float = DEFAULT_MIN_BALANCE_USDT
    max_total_exposure_usd: float | None = DEFAULT_MAX_TOTAL_EXPOSURE_USD
    daily_drawdown_limit_pct: float | None = DEFAULT_DAILY_DRAWDOWN_LIMIT_PCT
    allowed_leverage: float | None = DEFAULT_ALLOWED_LEVERAGE
    max_open_orders: int | None = DEFAULT_MAX_OPEN_ORDERS
    max_price_deviation_pct: float | None = DEFAULT_MAX_PRICE_DEVIATION_PCT
    timeframe: str = DEFAULT_TIMEFRAME
    fast_period: int = DEFAULT_FAST_PERIOD
    slow_period: int = DEFAULT_SLOW_PERIOD
    use_testnet: bool = DEFAULT_USE_TESTNET
    log_level: str = DEFAULT_LOG_LEVEL
    feed_queue_maxsize: int = DEFAULT_FEED_QUEUE_MAXSIZE
    dry_run: bool = DEFAULT_DRY_RUN
    market_data_mode: str = DEFAULT_MARKET_DATA_MODE
    strategy_name: str = DEFAULT_STRATEGY
    order_update_mode: str = DEFAULT_ORDER_UPDATE_MODE
    max_position_size_usd: float = DEFAULT_MAX_POSITION_SIZE_USD
    daily_loss_limit_usd: float = DEFAULT_DAILY_LOSS_LIMIT_USD
    telegram_bot_token: str = ""
    telegram_chat_id: str = ""
    db_path: str = DEFAULT_DB_PATH
    confirm_live_trading: str = "no"

    def __post_init__(self) -> None:
        object.__setattr__(self, "api_key", _non_blank(self.api_key, "api_key"))
        object.__setattr__(
            self,
            "api_secret",
            _non_blank(self.api_secret, "api_secret"),
        )
        object.__setattr__(
            self,
            "trading_symbol",
            normalize_symbol(_non_blank(self.trading_symbol, "trading_symbol")),
        )
        symbols = self.trading_symbols or (self.trading_symbol,)
        normalized_symbols = tuple(
            normalize_symbol(_non_blank(symbol, "trading_symbols"))
            for symbol in symbols
        )
        if not normalized_symbols:
            raise RuntimeError("trading_symbols must not be empty")
        object.__setattr__(self, "trading_symbols", normalized_symbols)
        object.__setattr__(self, "trading_symbol", normalized_symbols[0])
        object.__setattr__(
            self,
            "timeframe",
            _non_blank(self.timeframe, "timeframe"),
        )
        object.__setattr__(
            self,
            "order_size_mode",
            _non_blank(self.order_size_mode, "order_size_mode").lower(),
        )
        object.__setattr__(
            self,
            "log_level",
            _non_blank(self.log_level, "log_level").upper(),
        )
        object.__setattr__(self, "db_path", _non_blank(self.db_path, "db_path"))
        object.__setattr__(
            self,
            "market_data_mode",
            self.market_data_mode.strip().lower(),
        )
        object.__setattr__(
            self,
            "strategy_name",
            _non_blank(self.strategy_name, "strategy_name"),
        )
        object.__setattr__(
            self,
            "order_update_mode",
            self.order_update_mode.strip().lower(),
        )
        object.__setattr__(
            self,
            "telegram_bot_token",
            self.telegram_bot_token.strip(),
        )
        object.__setattr__(
            self,
            "telegram_chat_id",
            self.telegram_chat_id.strip(),
        )
        object.__setattr__(
            self,
            "confirm_live_trading",
            self.confirm_live_trading.strip().lower(),
        )

        _finite_float(self.order_size, "order_size", minimum=0.0)
        order_size_percent = _finite_float(
            self.order_size_percent,
            "order_size_percent",
            minimum=0.0,
        )
        object.__setattr__(self, "order_size_percent", order_size_percent)
        if self.order_size_mode not in {
            "fixed",
            "quote_percent",
            "equity_percent",
            "risk_percent",
        }:
            raise RuntimeError(
                "ORDER_SIZE_MODE must be one of: fixed, quote_percent, "
                "equity_percent, risk_percent"
            )
        if self.order_size_mode != "fixed" and order_size_percent <= 0:
            raise RuntimeError("ORDER_SIZE_PERCENT must be greater than 0")
        min_order_notional_usd = _finite_float(
            self.min_order_notional_usd,
            "min_order_notional_usd",
            minimum=0.0,
        )
        object.__setattr__(
            self,
            "min_order_notional_usd",
            min_order_notional_usd,
        )
        max_order_notional_usd = _finite_float(
            self.max_order_notional_usd,
            "max_order_notional_usd",
            minimum=0.0,
        )
        object.__setattr__(
            self,
            "max_order_notional_usd",
            max_order_notional_usd,
        )
        min_balance_usdt = _finite_float(
            self.min_balance_usdt,
            "min_balance_usdt",
            minimum=0.0,
        )
        object.__setattr__(self, "min_balance_usdt", min_balance_usdt)
        object.__setattr__(
            self,
            "max_total_exposure_usd",
            _optional_finite_float(
                self.max_total_exposure_usd,
                "max_total_exposure_usd",
                minimum=0.0,
                exclusive_minimum=True,
            ),
        )
        object.__setattr__(
            self,
            "daily_drawdown_limit_pct",
            _optional_finite_float(
                self.daily_drawdown_limit_pct,
                "daily_drawdown_limit_pct",
                minimum=0.0,
                exclusive_minimum=True,
            ),
        )
        object.__setattr__(
            self,
            "allowed_leverage",
            _optional_finite_float(
                self.allowed_leverage,
                "allowed_leverage",
                minimum=0.0,
                exclusive_minimum=True,
            ),
        )
        object.__setattr__(
            self,
            "max_open_orders",
            _optional_positive_int(self.max_open_orders, "max_open_orders"),
        )
        object.__setattr__(
            self,
            "max_price_deviation_pct",
            _optional_finite_float(
                self.max_price_deviation_pct,
                "max_price_deviation_pct",
                minimum=0.0,
            ),
        )
        if (
            max_order_notional_usd > 0
            and max_order_notional_usd < min_order_notional_usd
        ):
            raise RuntimeError(
                "MAX_ORDER_NOTIONAL_USD must be greater than or equal to "
                "MIN_ORDER_NOTIONAL_USD"
            )
        _positive_int(self.fast_period, "fast_period")
        _positive_int(self.slow_period, "slow_period")
        if self.fast_period >= self.slow_period:
            raise RuntimeError("SMA_FAST_PERIOD must be less than SMA_SLOW_PERIOD")
        if self.market_data_mode not in {"polling", "websocket"}:
            raise RuntimeError(
                "MARKET_DATA_MODE must be either 'polling' or 'websocket'"
            )
        if self.order_update_mode not in {"disabled", "websocket"}:
            raise RuntimeError(
                "ORDER_UPDATE_MODE must be either 'disabled' or 'websocket'"
            )
        _non_negative_int(self.feed_queue_maxsize, "feed_queue_maxsize")
        _finite_float(
            self.max_position_size_usd,
            "max_position_size_usd",
            minimum=0.0,
            exclusive_minimum=True,
        )
        _finite_float(
            self.daily_loss_limit_usd,
            "daily_loss_limit_usd",
            minimum=0.0,
            exclusive_minimum=True,
        )

    @classmethod
    def from_env(cls) -> "Settings":
        """Build settings from .env and process environment variables."""
        load_dotenv()

        api_key = _required_env("BINANCE_API_KEY")
        api_secret = _env("BINANCE_API_SECRET", "BINANCE_SECRET_KEY", default="")
        if not api_secret:
            raise RuntimeError(
                "missing required environment variable: "
                "BINANCE_API_SECRET or BINANCE_SECRET_KEY"
            )

        return cls(
            api_key=api_key,
            api_secret=api_secret,
            trading_symbol=_env("TRADING_SYMBOL", default=DEFAULT_TRADING_SYMBOL),
            trading_symbols=_symbols_env("TRADING_SYMBOLS"),
            order_size=_float_env(
                "ORDER_SIZE",
                "TRADE_QUANTITY",
                default=DEFAULT_ORDER_SIZE,
            ),
            order_size_mode=_env(
                "ORDER_SIZE_MODE",
                default=DEFAULT_ORDER_SIZE_MODE,
            ),
            order_size_percent=_float_env(
                "ORDER_SIZE_PERCENT",
                default=DEFAULT_ORDER_SIZE_PERCENT,
            ),
            min_order_notional_usd=_float_env(
                "MIN_ORDER_NOTIONAL_USD",
                default=DEFAULT_MIN_ORDER_NOTIONAL_USD,
            ),
            max_order_notional_usd=_float_env(
                "MAX_ORDER_NOTIONAL_USD",
                default=DEFAULT_MAX_ORDER_NOTIONAL_USD,
            ),
            min_balance_usdt=_float_env(
                "MIN_BALANCE_USDT",
                default=DEFAULT_MIN_BALANCE_USDT,
            ),
            max_total_exposure_usd=_optional_float_env("MAX_TOTAL_EXPOSURE_USD"),
            daily_drawdown_limit_pct=_optional_float_env(
                "DAILY_DRAWDOWN_LIMIT_PCT"
            ),
            allowed_leverage=_optional_float_env("ALLOWED_LEVERAGE"),
            max_open_orders=_optional_int_env("MAX_OPEN_ORDERS"),
            max_price_deviation_pct=_optional_float_env(
                "MAX_PRICE_DEVIATION_PCT"
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
            market_data_mode=_env(
                "MARKET_DATA_MODE",
                default=DEFAULT_MARKET_DATA_MODE,
            ),
            strategy_name=_env("STRATEGY", default=DEFAULT_STRATEGY),
            order_update_mode=_env(
                "ORDER_UPDATE_MODE",
                default=DEFAULT_ORDER_UPDATE_MODE,
            ),
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
            confirm_live_trading=_env(
                "CONFIRM_LIVE_TRADING",
                default="no",
            ).strip().lower(),
        )

    def for_symbol(self, symbol: str) -> "Settings":
        """Return a settings copy scoped to one trading symbol."""
        normalized_symbol = normalize_symbol(symbol)
        return replace(
            self,
            trading_symbol=normalized_symbol,
            trading_symbols=(normalized_symbol,),
        )


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


def _optional_float_env(name: str) -> float | None:
    value = os.getenv(name)
    if value is None or not value.strip():
        return None
    try:
        return float(value)
    except ValueError as exc:
        raise RuntimeError(f"{name} must be a number") from exc


def _optional_int_env(name: str) -> int | None:
    value = os.getenv(name)
    if value is None or not value.strip():
        return None
    try:
        return int(value)
    except ValueError as exc:
        raise RuntimeError(f"{name} must be an integer") from exc


def _bool_env(*names: str, default: bool) -> bool:
    value = _env(*names, default=str(default)).strip().lower()
    if value in {"1", "true", "t", "yes", "y", "on"}:
        return True
    if value in {"0", "false", "f", "no", "n", "off"}:
        return False
    joined_names = " or ".join(names)
    raise RuntimeError(f"{joined_names} must be a boolean")


def _symbols_env(name: str) -> tuple[str, ...]:
    raw_symbols = os.getenv(name)
    if raw_symbols is None or not raw_symbols.strip():
        return ()
    symbols = tuple(part.strip() for part in raw_symbols.split(",") if part.strip())
    if not symbols:
        raise RuntimeError(f"{name} must contain at least one symbol")
    return symbols


def _non_blank(value: str, field_name: str) -> str:
    normalized = value.strip()
    if not normalized:
        raise RuntimeError(f"{field_name} must not be blank")
    return normalized


def _positive_int(value: int, field_name: str) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise RuntimeError(f"{field_name} must be a positive integer")


def _non_negative_int(value: int, field_name: str) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise RuntimeError(f"{field_name} must be a non-negative integer")


def _optional_positive_int(value: int | None, field_name: str) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise RuntimeError(f"{field_name} must be a positive integer")
    return value


def _finite_float(
    value: float,
    field_name: str,
    *,
    minimum: float,
    exclusive_minimum: bool = False,
) -> float:
    if isinstance(value, bool) or not math.isfinite(value):
        raise RuntimeError(f"{field_name} must be a finite number")
    if exclusive_minimum and value <= minimum:
        raise RuntimeError(f"{field_name} must be greater than {minimum:g}")
    if not exclusive_minimum and value < minimum:
        raise RuntimeError(f"{field_name} must be at least {minimum:g}")
    return float(value)


def _optional_finite_float(
    value: float | None,
    field_name: str,
    *,
    minimum: float,
    exclusive_minimum: bool = False,
) -> float | None:
    if value is None:
        return None
    if isinstance(value, bool) or not math.isfinite(value):
        raise RuntimeError(f"{field_name} must be a finite number")
    if exclusive_minimum and value <= minimum:
        raise RuntimeError(f"{field_name} must be greater than {minimum:g}")
    if not exclusive_minimum and value < minimum:
        raise RuntimeError(f"{field_name} must be at least {minimum:g}")
    return float(value)

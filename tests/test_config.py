from __future__ import annotations

from collections.abc import Iterable

import pytest

import src.config as config_module
from src.config import Settings


ENV_KEYS = (
    "BINANCE_API_KEY",
    "BINANCE_API_SECRET",
    "BINANCE_SECRET_KEY",
    "TRADING_SYMBOL",
    "TRADING_SYMBOLS",
    "ORDER_SIZE",
    "TRADE_QUANTITY",
    "ORDER_SIZE_MODE",
    "ORDER_SIZE_PERCENT",
    "MIN_ORDER_NOTIONAL_USD",
    "MAX_ORDER_NOTIONAL_USD",
    "MIN_BALANCE_USDT",
    "MAX_TOTAL_EXPOSURE_USD",
    "DAILY_DRAWDOWN_LIMIT_PCT",
    "ALLOWED_LEVERAGE",
    "MAX_OPEN_ORDERS",
    "MAX_PRICE_DEVIATION_PCT",
    "TRADING_TIMEFRAME",
    "TIMEFRAME",
    "SMA_FAST_PERIOD",
    "SMA_SLOW_PERIOD",
    "USE_TESTNET",
    "LOG_LEVEL",
    "FEED_QUEUE_MAXSIZE",
    "DRY_RUN",
    "MARKET_DATA_MODE",
    "STRATEGY",
    "ORDER_UPDATE_MODE",
    "MAX_POSITION_SIZE_USD",
    "DAILY_LOSS_LIMIT_USD",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_CHAT_ID",
    "DB_PATH",
    "CONFIRM_LIVE_TRADING",
)


@pytest.fixture
def clean_env(monkeypatch: pytest.MonkeyPatch) -> pytest.MonkeyPatch:
    monkeypatch.setattr(config_module, "load_dotenv", lambda: None)
    for key in ENV_KEYS:
        monkeypatch.delenv(key, raising=False)
    return monkeypatch


def _set_required_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("BINANCE_API_KEY", "test-key")
    monkeypatch.setenv("BINANCE_API_SECRET", "test-secret")


def test_from_env_parses_aliases_and_normalizes_values(
    clean_env: pytest.MonkeyPatch,
) -> None:
    clean_env.setenv("BINANCE_API_KEY", " test-key ")
    clean_env.setenv("BINANCE_SECRET_KEY", " test-secret ")
    clean_env.setenv("TRADING_SYMBOLS", "btcusdt, eth-usdt")
    clean_env.setenv("TRADE_QUANTITY", "0.25")
    clean_env.setenv("ORDER_SIZE_MODE", " quote_percent ")
    clean_env.setenv("ORDER_SIZE_PERCENT", "2.5")
    clean_env.setenv("MIN_ORDER_NOTIONAL_USD", "10")
    clean_env.setenv("MAX_ORDER_NOTIONAL_USD", "250")
    clean_env.setenv("MIN_BALANCE_USDT", "50")
    clean_env.setenv("MAX_TOTAL_EXPOSURE_USD", "2000")
    clean_env.setenv("DAILY_DRAWDOWN_LIMIT_PCT", "5")
    clean_env.setenv("ALLOWED_LEVERAGE", "1")
    clean_env.setenv("MAX_OPEN_ORDERS", "3")
    clean_env.setenv("MAX_PRICE_DEVIATION_PCT", "1.5")
    clean_env.setenv("TIMEFRAME", " 5m ")
    clean_env.setenv("SMA_FAST_PERIOD", "3")
    clean_env.setenv("SMA_SLOW_PERIOD", "9")
    clean_env.setenv("USE_TESTNET", "false")
    clean_env.setenv("DRY_RUN", "yes")
    clean_env.setenv("LOG_LEVEL", " debug ")
    clean_env.setenv("FEED_QUEUE_MAXSIZE", "50")
    clean_env.setenv("MARKET_DATA_MODE", " websocket ")
    clean_env.setenv("STRATEGY", "demo-sma")
    clean_env.setenv("ORDER_UPDATE_MODE", " websocket ")
    clean_env.setenv("MAX_POSITION_SIZE_USD", "1000")
    clean_env.setenv("DAILY_LOSS_LIMIT_USD", "125")
    clean_env.setenv("TELEGRAM_BOT_TOKEN", " token ")
    clean_env.setenv("TELEGRAM_CHAT_ID", " chat ")
    clean_env.setenv("DB_PATH", " tmp/orders.db ")
    clean_env.setenv("CONFIRM_LIVE_TRADING", " YES ")

    settings = Settings.from_env()

    assert settings.api_key == "test-key"
    assert settings.api_secret == "test-secret"
    assert settings.trading_symbol == "BTC/USDT"
    assert settings.trading_symbols == ("BTC/USDT", "ETH/USDT")
    assert settings.order_size == 0.25
    assert settings.order_size_mode == "quote_percent"
    assert settings.order_size_percent == 2.5
    assert settings.min_order_notional_usd == 10
    assert settings.max_order_notional_usd == 250
    assert settings.min_balance_usdt == 50
    assert settings.max_total_exposure_usd == 2000
    assert settings.daily_drawdown_limit_pct == 5
    assert settings.allowed_leverage == 1
    assert settings.max_open_orders == 3
    assert settings.max_price_deviation_pct == 1.5
    assert settings.timeframe == "5m"
    assert settings.fast_period == 3
    assert settings.slow_period == 9
    assert settings.use_testnet is False
    assert settings.dry_run is True
    assert settings.log_level == "DEBUG"
    assert settings.feed_queue_maxsize == 50
    assert settings.market_data_mode == "websocket"
    assert settings.strategy_name == "demo-sma"
    assert settings.order_update_mode == "websocket"
    assert settings.max_position_size_usd == 1000
    assert settings.daily_loss_limit_usd == 125
    assert settings.telegram_bot_token == "token"
    assert settings.telegram_chat_id == "chat"
    assert settings.db_path == "tmp/orders.db"
    assert settings.confirm_live_trading == "yes"


@pytest.mark.parametrize(
    ("env_values", "message"),
    [
        ((), "missing required environment variable: BINANCE_API_KEY"),
        (
            (("BINANCE_API_KEY", "key"),),
            "missing required environment variable: BINANCE_API_SECRET or BINANCE_SECRET_KEY",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("USE_TESTNET", "maybe")),
            "USE_TESTNET must be a boolean",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("ORDER_SIZE", "nan")),
            "order_size must be a finite number",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("ORDER_SIZE_MODE", "all-in")),
            "ORDER_SIZE_MODE must be one of",
        ),
        (
            (
                ("BINANCE_API_KEY", "key"),
                ("BINANCE_API_SECRET", "secret"),
                ("ORDER_SIZE_MODE", "quote_percent"),
                ("ORDER_SIZE_PERCENT", "0"),
            ),
            "ORDER_SIZE_PERCENT must be greater",
        ),
        (
            (
                ("BINANCE_API_KEY", "key"),
                ("BINANCE_API_SECRET", "secret"),
                ("MIN_ORDER_NOTIONAL_USD", "20"),
                ("MAX_ORDER_NOTIONAL_USD", "10"),
            ),
            "MAX_ORDER_NOTIONAL_USD must be greater",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("MAX_TOTAL_EXPOSURE_USD", "nan")),
            "max_total_exposure_usd must be a finite number",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("DAILY_DRAWDOWN_LIMIT_PCT", "0")),
            "daily_drawdown_limit_pct must be greater",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("ALLOWED_LEVERAGE", "0")),
            "allowed_leverage must be greater",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("MAX_OPEN_ORDERS", "0")),
            "max_open_orders must be a positive integer",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("MAX_PRICE_DEVIATION_PCT", "-1")),
            "max_price_deviation_pct must be at least",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("SMA_FAST_PERIOD", "50")),
            "SMA_FAST_PERIOD must be less than SMA_SLOW_PERIOD",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("FEED_QUEUE_MAXSIZE", "-1")),
            "feed_queue_maxsize must be a non-negative integer",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("MARKET_DATA_MODE", "stream")),
            "MARKET_DATA_MODE must be either",
        ),
        (
            (("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret"), ("ORDER_UPDATE_MODE", "polling")),
            "ORDER_UPDATE_MODE must be either",
        ),
    ],
)
def test_from_env_rejects_invalid_values(
    clean_env: pytest.MonkeyPatch,
    env_values: Iterable[tuple[str, str]],
    message: str,
) -> None:
    for key, value in env_values:
        clean_env.setenv(key, value)

    with pytest.raises(RuntimeError, match=message):
        Settings.from_env()


def test_settings_constructor_validates_values(clean_env: pytest.MonkeyPatch) -> None:
    _set_required_env(clean_env)

    settings = Settings(
        api_key=" key ",
        api_secret=" secret ",
        trading_symbol="eth-usdt",
        log_level=" warning ",
        confirm_live_trading=" YES ",
    )

    assert settings.api_key == "key"
    assert settings.api_secret == "secret"
    assert settings.trading_symbol == "ETH/USDT"
    assert settings.trading_symbols == ("ETH/USDT",)
    assert settings.log_level == "WARNING"
    assert settings.confirm_live_trading == "yes"
    assert settings.strategy_name == "demo-sma"

    with pytest.raises(RuntimeError, match="trading_symbol must not be blank"):
        Settings(api_key="key", api_secret="secret", trading_symbol=" ")

    with pytest.raises(RuntimeError, match="daily_loss_limit_usd must be greater"):
        Settings(api_key="key", api_secret="secret", daily_loss_limit_usd=0)

    with pytest.raises(RuntimeError, match="strategy_name must not be blank"):
        Settings(api_key="key", api_secret="secret", strategy_name=" ")


def test_settings_can_be_scoped_to_one_symbol() -> None:
    settings = Settings(
        api_key="key",
        api_secret="secret",
        trading_symbols=("BTC/USDT", "ETH/USDT"),
    )

    scoped = settings.for_symbol("ethusdt")

    assert settings.trading_symbols == ("BTC/USDT", "ETH/USDT")
    assert scoped.trading_symbol == "ETH/USDT"
    assert scoped.trading_symbols == ("ETH/USDT",)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Port of `tests/test_config.py` (env parsing via an injected lookup).

use std::collections::HashMap;

use bb_runtime::config::{MarketDataMode, OrderSizeMode, OrderUpdateMode, Settings};
use rust_decimal_macros::dec;

fn from(pairs: &[(&str, &str)]) -> Result<Settings, String> {
    let env: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Settings::from_source(&|name| env.get(name).cloned()).map_err(|e| e.to_string())
}

#[test]
fn from_env_parses_aliases_and_normalizes_values() {
    let s = from(&[
        ("BINANCE_API_KEY", " test-key "),
        ("BINANCE_SECRET_KEY", " test-secret "),
        ("TRADING_SYMBOLS", "btcusdt, eth-usdt"),
        ("TRADE_QUANTITY", "0.25"),
        ("ORDER_SIZE_MODE", " quote_percent "),
        ("ORDER_SIZE_PERCENT", "2.5"),
        ("MIN_ORDER_NOTIONAL_USD", "10"),
        ("MAX_ORDER_NOTIONAL_USD", "250"),
        ("MIN_BALANCE_USDT", "50"),
        ("MAX_TOTAL_EXPOSURE_USD", "2000"),
        ("ALLOWED_LEVERAGE", "1"),
        ("MAX_OPEN_ORDERS", "3"),
        ("MAX_PRICE_DEVIATION_PCT", "1.5"),
        ("TIMEFRAME", " 5m "),
        ("SMA_FAST_PERIOD", "3"),
        ("SMA_SLOW_PERIOD", "9"),
        ("USE_TESTNET", "false"),
        ("DRY_RUN", "yes"),
        ("LOG_LEVEL", " debug "),
        ("FEED_QUEUE_MAXSIZE", "50"),
        ("MARKET_DATA_MODE", " websocket "),
        ("STRATEGY", "demo-sma"),
        ("ORDER_UPDATE_MODE", " websocket "),
        ("WS_ALERT_COOLDOWN_SECONDS", "45"),
        ("MAX_POSITION_SIZE_USD", "1000"),
        ("TELEGRAM_BOT_TOKEN", " token "),
        ("TELEGRAM_CHAT_ID", " chat "),
        ("DB_PATH", " tmp/orders.db "),
        ("CONFIRM_LIVE_TRADING", " YES "),
        ("API_SYMBOL_WHITELIST", "btcusdt, bnb-usdt"),
        ("API_ASSET_WHITELIST", "bnb, usdt"),
    ])
    .unwrap();
    assert_eq!(
        (s.api_key.as_str(), s.api_secret.as_str()),
        ("test-key", "test-secret")
    );
    assert_eq!(s.trading_symbol, "BTC/USDT");
    assert_eq!(s.trading_symbols, vec!["BTC/USDT", "ETH/USDT"]);
    assert_eq!(s.order_size, dec!(0.25));
    assert_eq!(s.order_size_mode, OrderSizeMode::QuotePercent);
    assert_eq!(s.order_size_percent, dec!(2.5));
    assert_eq!(
        (s.min_order_notional_usd, s.max_order_notional_usd),
        (dec!(10), dec!(250))
    );
    assert_eq!(s.min_balance_usdt, dec!(50));
    assert_eq!(s.max_total_exposure_usd, Some(dec!(2000)));
    assert_eq!(s.allowed_leverage, Some(dec!(1)));
    assert_eq!(s.max_open_orders, Some(3));
    assert_eq!(s.max_price_deviation_pct, Some(dec!(1.5)));
    assert_eq!(s.timeframe, "5m");
    assert_eq!((s.fast_period, s.slow_period), (3, 9));
    assert!(!s.use_testnet && s.dry_run);
    assert_eq!(s.log_level, "DEBUG");
    assert_eq!(s.feed_queue_maxsize, 50);
    assert_eq!(s.market_data_mode, MarketDataMode::Websocket);
    assert_eq!(s.order_update_mode, OrderUpdateMode::Websocket);
    assert_eq!(s.strategy_name, "demo-sma");
    assert!((s.ws_alert_cooldown_seconds - 45.0).abs() < f64::EPSILON);
    assert_eq!(s.max_position_size_usd, dec!(1000));
    assert_eq!(
        (s.telegram_bot_token.as_str(), s.telegram_chat_id.as_str()),
        ("token", "chat")
    );
    assert_eq!(s.db_path, "tmp/orders.db");
    assert_eq!(s.confirm_live_trading, "yes");
    assert_eq!(s.api_symbol_whitelist, vec!["BTC/USDT", "BNB/USDT"]);
    assert_eq!(s.api_asset_whitelist, vec!["BNB", "USDT"]);
    assert_eq!(s.mode(), "DRY_RUN");
}

#[test]
fn from_env_rejects_invalid_values() {
    let base = [("BINANCE_API_KEY", "key"), ("BINANCE_API_SECRET", "secret")];
    let cases: &[(&[(&str, &str)], &str)] = &[
        (&[("USE_TESTNET", "maybe")], "USE_TESTNET must be a boolean"),
        (
            &[("ORDER_SIZE", "nan")],
            "ORDER_SIZE or TRADE_QUANTITY must be a number",
        ),
        (
            &[("ORDER_SIZE_MODE", "all-in")],
            "ORDER_SIZE_MODE must be one of",
        ),
        (
            &[
                ("ORDER_SIZE_MODE", "quote_percent"),
                ("ORDER_SIZE_PERCENT", "0"),
            ],
            "ORDER_SIZE_PERCENT must be greater",
        ),
        (
            &[
                ("MIN_ORDER_NOTIONAL_USD", "20"),
                ("MAX_ORDER_NOTIONAL_USD", "10"),
            ],
            "MAX_ORDER_NOTIONAL_USD must be greater",
        ),
        (
            &[("MAX_TOTAL_EXPOSURE_USD", "nan")],
            "MAX_TOTAL_EXPOSURE_USD must be a number",
        ),
        (
            &[("ALLOWED_LEVERAGE", "0")],
            "allowed_leverage must be greater",
        ),
        (
            &[("MAX_OPEN_ORDERS", "0")],
            "max_open_orders must be a positive integer",
        ),
        (
            &[("MAX_PRICE_DEVIATION_PCT", "-1")],
            "max_price_deviation_pct must be at least",
        ),
        (
            &[("SMA_FAST_PERIOD", "50")],
            "SMA_FAST_PERIOD must be less than SMA_SLOW_PERIOD",
        ),
        (
            &[("FEED_QUEUE_MAXSIZE", "-1")],
            "feed_queue_maxsize must be a non-negative integer",
        ),
        (
            &[("MARKET_DATA_MODE", "stream")],
            "MARKET_DATA_MODE must be either",
        ),
        (
            &[("ORDER_UPDATE_MODE", "polling")],
            "ORDER_UPDATE_MODE must be either",
        ),
        (
            &[("WS_ALERT_COOLDOWN_SECONDS", "-1")],
            "ws_alert_cooldown_seconds must be at least",
        ),
        (
            &[("TRADING_SYMBOLS", " , ")],
            "TRADING_SYMBOLS must contain at least one symbol",
        ),
        (
            &[("API_ASSET_WHITELIST", "bn$b")],
            "asset whitelist values must be asset symbols",
        ),
    ];
    assert_eq!(
        from(&[]).unwrap_err(),
        "missing required environment variable: BINANCE_API_KEY"
    );
    assert_eq!(
        from(&[("BINANCE_API_KEY", "key")]).unwrap_err(),
        "missing required environment variable: BINANCE_API_SECRET or BINANCE_SECRET_KEY"
    );
    for (extra, message) in cases {
        let mut env = base.to_vec();
        env.extend_from_slice(extra);
        let err = from(&env).unwrap_err();
        assert!(err.starts_with(message), "{extra:?}: {err}");
    }
}

#[test]
fn settings_constructor_validates_values() {
    let mut raw = Settings::defaults(" key ", " secret ");
    raw.trading_symbol = "eth-usdt".into();
    raw.log_level = " warning ".into();
    raw.confirm_live_trading = " YES ".into();
    let s = raw.validate().unwrap();
    assert_eq!(
        (s.api_key.as_str(), s.api_secret.as_str()),
        ("key", "secret")
    );
    assert_eq!(s.trading_symbols, vec!["ETH/USDT"]);
    assert_eq!(s.log_level, "WARNING");
    assert_eq!(s.confirm_live_trading, "yes");
    assert_eq!(s.strategy_name, "demo-sma");

    let blank_symbol = Settings {
        trading_symbol: " ".into(),
        ..Settings::defaults("key", "secret")
    };
    assert_eq!(
        blank_symbol.validate().unwrap_err().to_string(),
        "trading_symbol must not be blank"
    );
    let blank_strategy = Settings {
        strategy_name: " ".into(),
        ..Settings::defaults("key", "secret")
    };
    assert_eq!(
        blank_strategy.validate().unwrap_err().to_string(),
        "strategy_name must not be blank"
    );
}

#[test]
fn settings_can_be_scoped_to_one_symbol() {
    let s = Settings {
        trading_symbols: vec!["BTC/USDT".into(), "ETH/USDT".into()],
        ..Settings::defaults("key", "secret")
    }
    .validate()
    .unwrap();
    let scoped = s.for_symbol("ethusdt").unwrap();
    assert_eq!(s.trading_symbols, vec!["BTC/USDT", "ETH/USDT"]);
    assert_eq!(scoped.trading_symbol, "ETH/USDT");
    assert_eq!(scoped.trading_symbols, vec!["ETH/USDT"]);
}

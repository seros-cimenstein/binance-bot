#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Port of `tests/test_risk_guard.py`.

use std::sync::Arc;
use std::time::Duration;

use bb_core::Decimal;
use bb_runtime::db::OrderDb;
use bb_runtime::risk::{
    MarketData, OrderCheck, OrderRequest, PortfolioState, RiskConfig, RiskError, RiskGuard,
};
use rust_decimal_macros::dec;

fn structured_config() -> RiskConfig {
    RiskConfig {
        max_total_exposure_usd: Some(dec!(5000)),
        daily_drawdown_limit_pct: Some(dec!(5)),
        min_balance_usdt: dec!(100),
        allowed_leverage: Some(dec!(5)),
        max_open_orders: Some(5),
        max_price_deviation_pct: Some(dec!(2)),
        ..RiskConfig::new(dec!(1000), dec!(1000))
    }
}

fn order() -> OrderRequest {
    OrderRequest {
        leverage: dec!(5),
        ..OrderRequest::new("BTC/USDT", "buy", dec!(0.01), dec!(50000))
    }
}

fn portfolio() -> PortfolioState {
    PortfolioState {
        total_equity_usdt: dec!(10000),
        available_balance_usdt: dec!(500),
        realized_pnl_today_usdt: dec!(-100),
        unrealized_pnl_today_usdt: dec!(-100),
        positions: [("ETH/USDT".to_owned(), dec!(500))].into(),
        total_exposure_usdt: dec!(500),
        open_orders_count: 1,
    }
}

fn market() -> MarketData {
    MarketData {
        symbol: "BTC/USDT".into(),
        mark_price: dec!(50000),
    }
}

fn positions(symbol: &str, exposure: Decimal) -> std::collections::BTreeMap<String, Decimal> {
    [(symbol.to_owned(), exposure)].into()
}

#[test]
fn structured_risk_guard_conditions_are_table_driven() {
    type Case = (
        &'static str,
        RiskConfig,
        OrderRequest,
        PortfolioState,
        MarketData,
        Option<&'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "happy_path_order_allowed",
            structured_config(),
            order(),
            portfolio(),
            market(),
            None,
        ),
        (
            "max_position_size_exact_limit_allowed",
            structured_config(),
            order(),
            PortfolioState {
                positions: positions("BTC/USDT", dec!(500)),
                total_exposure_usdt: dec!(500),
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "max_position_size_exceeded_rejected",
            structured_config(),
            order(),
            PortfolioState {
                positions: positions("BTC/USDT", dec!(500.01)),
                total_exposure_usdt: dec!(500.01),
                ..portfolio()
            },
            market(),
            Some("maximum position size breached"),
        ),
        (
            "sell_reduces_long_position_allowed",
            structured_config(),
            OrderRequest {
                side: "sell".into(),
                amount: dec!(0.005),
                ..order()
            },
            PortfolioState {
                positions: positions("BTC/USDT", dec!(750)),
                total_exposure_usdt: dec!(750),
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "sell_increases_short_exposure_rejected",
            structured_config(),
            OrderRequest {
                side: "sell".into(),
                amount: dec!(0.005),
                ..order()
            },
            PortfolioState {
                positions: positions("BTC/USDT", dec!(-750.01)),
                total_exposure_usdt: dec!(750.01),
                ..portfolio()
            },
            market(),
            Some("maximum position size breached"),
        ),
        (
            "total_exposure_exact_limit_allowed",
            structured_config(),
            order(),
            PortfolioState {
                positions: positions("ETH/USDT", dec!(4500)),
                total_exposure_usdt: dec!(4500),
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "total_exposure_exceeded_rejected",
            structured_config(),
            order(),
            PortfolioState {
                positions: positions("ETH/USDT", dec!(4500.01)),
                total_exposure_usdt: dec!(4500.01),
                ..portfolio()
            },
            market(),
            Some("maximum total exposure breached"),
        ),
        (
            "daily_drawdown_exact_limit_allowed",
            structured_config(),
            order(),
            PortfolioState {
                realized_pnl_today_usdt: dec!(-250),
                unrealized_pnl_today_usdt: dec!(-250),
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "daily_drawdown_guard_disabled_allows_large_loss",
            RiskConfig {
                daily_drawdown_limit_pct: None,
                ..structured_config()
            },
            order(),
            PortfolioState {
                realized_pnl_today_usdt: dec!(-1000),
                unrealized_pnl_today_usdt: dec!(-1000),
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "positive_daily_pnl_has_no_drawdown",
            structured_config(),
            order(),
            PortfolioState {
                realized_pnl_today_usdt: dec!(600),
                unrealized_pnl_today_usdt: dec!(-200),
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "daily_drawdown_exceeded_rejected",
            structured_config(),
            order(),
            PortfolioState {
                realized_pnl_today_usdt: dec!(-250.01),
                unrealized_pnl_today_usdt: dec!(-250),
                ..portfolio()
            },
            market(),
            Some("daily drawdown breached"),
        ),
        (
            "minimum_balance_exact_buffer_allowed",
            structured_config(),
            order(),
            PortfolioState {
                available_balance_usdt: dec!(200),
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "minimum_balance_buffer_breached_rejected",
            structured_config(),
            order(),
            PortfolioState {
                available_balance_usdt: dec!(199.99),
                ..portfolio()
            },
            market(),
            Some("minimum balance breached"),
        ),
        (
            "leverage_exact_match_allowed",
            structured_config(),
            order(),
            portfolio(),
            market(),
            None,
        ),
        (
            "leverage_mismatch_rejected",
            structured_config(),
            OrderRequest {
                leverage: dec!(10),
                ..order()
            },
            portfolio(),
            market(),
            Some("leverage check breached"),
        ),
        (
            "max_open_orders_exact_result_allowed",
            structured_config(),
            order(),
            PortfolioState {
                open_orders_count: 4,
                ..portfolio()
            },
            market(),
            None,
        ),
        (
            "max_open_orders_exceeded_rejected",
            structured_config(),
            order(),
            PortfolioState {
                open_orders_count: 5,
                ..portfolio()
            },
            market(),
            Some("maximum open orders breached"),
        ),
        (
            "price_deviation_exact_limit_allowed",
            structured_config(),
            OrderRequest {
                price: dec!(51000),
                amount: dec!(0.0098),
                ..order()
            },
            portfolio(),
            market(),
            None,
        ),
        (
            "price_deviation_exceeded_rejected",
            structured_config(),
            OrderRequest {
                price: dec!(51001),
                amount: dec!(0.0098),
                ..order()
            },
            portfolio(),
            market(),
            Some("price deviation breached"),
        ),
        (
            "market_symbol_mismatch_rejected",
            structured_config(),
            order(),
            portfolio(),
            MarketData {
                symbol: "ETH/USDT".into(),
                ..market()
            },
            Some("market data symbol mismatch"),
        ),
    ];
    for (id, config, order, portfolio, market, expected) in cases {
        let guard = RiskGuard::new(config, None).unwrap();
        let result = guard.validate_structured(&order, &portfolio, &market);
        match expected {
            None => assert!(result.is_ok(), "{id}: {result:?}"),
            Some(message) => {
                let err = result.expect_err(id);
                assert!(err.message.starts_with(message), "{id}: {}", err.message);
                assert!(err.context.is_object(), "{id}");
            }
        }
    }
}

#[test]
fn legacy_duplicate_order_window_is_enforced() {
    let config = RiskConfig {
        duplicate_window_seconds: 10.0,
        ..RiskConfig::new(dec!(1000), dec!(1000))
    };
    let check = OrderCheck {
        symbol: "BTC/USDT".into(),
        side: "buy".into(),
        amount: dec!(0.01),
        price: dec!(50000),
        current_position: Decimal::ZERO,
    };

    let guard = RiskGuard::new(config.clone(), None).unwrap();
    guard.validate_order(&check).unwrap();
    let err = guard.validate_order(&check).unwrap_err();
    assert!(
        err.message
            .starts_with("duplicate order breached: identical order submitted"),
        "{}",
        err.message
    );
    assert!(
        err.message.ends_with("within 10.000s window"),
        "{}",
        err.message
    );

    // Same key with an equal-but-differently-scaled amount is still a duplicate.
    let err = guard
        .validate_order(&OrderCheck {
            amount: dec!(0.0100),
            ..check.clone()
        })
        .unwrap_err();
    assert!(err.message.starts_with("duplicate order breached"));

    let guard = RiskGuard::new(config, None).unwrap();
    guard.backdate_duplicate("BTC/USDT", "buy", dec!(0.01), Duration::from_secs(11));
    guard.validate_order(&check).unwrap();
}

fn check(side: &str, amount: Decimal, price: Decimal, current_position: Decimal) -> OrderCheck {
    OrderCheck {
        symbol: "BTC/USDT".into(),
        side: side.into(),
        amount,
        price,
        current_position,
    }
}

#[test]
fn legacy_position_and_daily_loss_guards_are_preserved() {
    let guard = RiskGuard::new(RiskConfig::new(dec!(1000), dec!(100)), None).unwrap();
    // Resulting 0.02 BTC * 50,000 = exactly the limit: allowed.
    guard
        .validate_order(&check("buy", dec!(0.01), dec!(50000), dec!(0.01)))
        .unwrap();
    let err = guard
        .validate_order(&check("buy", dec!(0.0101), dec!(50000), dec!(0.01)))
        .unwrap_err();
    assert_eq!(
        err.message,
        "maximum position size breached: 1005.00000000 USD exceeds 1000.00000000 USD"
    );
    // Selling down a long position is allowed.
    guard
        .validate_order(&check("sell", dec!(0.015), dec!(50000), dec!(0.02)))
        .unwrap();

    guard.update_realized_pnl(dec!(-60)).unwrap();
    guard.update_realized_pnl(dec!(500)).unwrap();
    guard
        .validate_order(&check("buy", dec!(0.001), dec!(50000), dec!(0)))
        .unwrap();
    guard.update_realized_pnl(dec!(-40)).unwrap();
    assert_eq!(guard.current_daily_loss(), dec!(100));
    let err = guard
        .validate_order(&check("buy", dec!(0.002), dec!(50000), dec!(0)))
        .unwrap_err();
    assert_eq!(
        err.message,
        "daily loss limit breached: 100.00000000 USD has reached or exceeded 100.00000000 USD"
    );
}

#[test]
fn legacy_order_input_validation_rejects_invalid_requests() {
    let guard = RiskGuard::new(RiskConfig::new(dec!(1000), dec!(100)), None).unwrap();
    let cases = [
        (
            OrderCheck {
                symbol: "  ".into(),
                ..check("buy", dec!(1), dec!(1), dec!(0))
            },
            "symbol must not be blank",
        ),
        (
            check("hold", dec!(1), dec!(1), dec!(0)),
            "side must be 'buy' or 'sell'",
        ),
        (
            check("buy", dec!(0), dec!(1), dec!(0)),
            "amount must be a finite value greater than zero",
        ),
        (
            check("buy", dec!(1), dec!(-1), dec!(0)),
            "price must be a finite value greater than zero",
        ),
    ];
    for (input, message) in cases {
        assert_eq!(guard.validate_order(&input).unwrap_err().message, message);
    }
}

#[test]
fn realized_pnl_persists_and_restores_daily_loss() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(OrderDb::open(dir.path().join("bot.db")).unwrap());
    let guard = RiskGuard::new(RiskConfig::new(dec!(1000), dec!(100)), Some(db.clone())).unwrap();
    guard.update_realized_pnl(dec!(-12.5)).unwrap();
    guard.update_realized_pnl(dec!(3)).unwrap();
    assert_eq!(guard.current_daily_loss(), dec!(12.5));

    let restored = RiskGuard::new(RiskConfig::new(dec!(1000), dec!(100)), Some(db)).unwrap();
    assert_eq!(restored.current_daily_loss(), dec!(0));
    restored.restore_daily_loss_from_db().unwrap();
    assert_eq!(restored.current_daily_loss(), dec!(12.5));
}

#[test]
fn risk_config_rejects_invalid_thresholds() {
    for config in [
        RiskConfig::new(dec!(0), dec!(1)),
        RiskConfig::new(dec!(1), dec!(-1)),
        RiskConfig {
            duplicate_window_seconds: f64::NAN,
            ..RiskConfig::new(dec!(1), dec!(1))
        },
        RiskConfig {
            max_open_orders: Some(0),
            ..RiskConfig::new(dec!(1), dec!(1))
        },
        RiskConfig {
            max_price_deviation_pct: Some(dec!(-1)),
            ..RiskConfig::new(dec!(1), dec!(1))
        },
    ] {
        assert!(matches!(
            RiskGuard::new(config, None),
            Err(RiskError::Config(_))
        ));
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Port of `tests/test_risk_guard.py`.

use std::time::Duration;

use bb_core::Decimal;
use bb_runtime::risk::{
    MarketData, OrderCheck, OrderRequest, PortfolioState, RiskConfig, RiskError, RiskGuard,
};
use rust_decimal_macros::dec;

fn structured_config() -> RiskConfig {
    RiskConfig {
        max_total_exposure_usd: Some(dec!(5000)),
        min_balance_usdt: dec!(100),
        allowed_leverage: Some(dec!(5)),
        max_open_orders: Some(5),
        max_price_deviation_pct: Some(dec!(2)),
        ..RiskConfig::new(dec!(1000))
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
        let guard = RiskGuard::new(config).unwrap();
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
        ..RiskConfig::new(dec!(1000))
    };
    let check = OrderCheck {
        symbol: "BTC/USDT".into(),
        side: "buy".into(),
        amount: dec!(0.01),
        price: dec!(50000),
        current_position: Decimal::ZERO,
    };

    let guard = RiskGuard::new(config.clone()).unwrap();
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

    let guard = RiskGuard::new(config).unwrap();
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
fn legacy_position_size_guard_is_preserved() {
    let guard = RiskGuard::new(RiskConfig::new(dec!(1000))).unwrap();
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
}

#[test]
fn legacy_order_input_validation_rejects_invalid_requests() {
    let guard = RiskGuard::new(RiskConfig::new(dec!(1000))).unwrap();
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
fn risk_config_rejects_invalid_thresholds() {
    for config in [
        RiskConfig::new(dec!(0)),
        RiskConfig {
            duplicate_window_seconds: f64::NAN,
            ..RiskConfig::new(dec!(1))
        },
        RiskConfig {
            max_open_orders: Some(0),
            ..RiskConfig::new(dec!(1))
        },
        RiskConfig {
            max_price_deviation_pct: Some(dec!(-1)),
            ..RiskConfig::new(dec!(1))
        },
    ] {
        assert!(matches!(RiskGuard::new(config), Err(RiskError::Config(_))));
    }
}

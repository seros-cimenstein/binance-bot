#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Capital pool leases and quote auto-convert on the StrategyOrderClient path.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bb_binance::SymbolRules;
use bb_binance::spot::{NewOrder, Order as BinanceOrder, SpotSymbolInfo};
use bb_capital::{AutoConvertConfig, CapitalPool, CapitalPoolConfig, ConvertExchange, ConvertMode};
use bb_core::Decimal;
use bb_runtime::capital::{CapitalGate, CapitalGateError};
use bb_runtime::orders::OrderManager;
use bb_runtime::orders::models::OrderSide;
use bb_runtime::runtime::{ClientError, StrategyOrderClient};
use bb_runtime::strategies::{OrderClient, StrategyOrder};
use parking_lot::Mutex;
use rust_decimal_macros::dec;
use serde_json::json;
use support::{CapturingTransport, FakeExchange};

fn pool(reserve_pct: Decimal) -> CapitalPool {
    CapitalPool::new(CapitalPoolConfig {
        enabled: true,
        reserve_pct,
        min_module_allocation_usdt: Decimal::ZERO,
        max_module_allocation_usdt: Some(dec!(25)),
    })
}

fn client(exchange: &Arc<FakeExchange>, gate: CapitalGate) -> StrategyOrderClient {
    StrategyOrderClient::new(Arc::new(OrderManager::new(exchange.clone(), None))).with_capital(
        Arc::new(gate),
        "demo-sma",
        vec!["demo-sma".into()],
    )
}

#[tokio::test]
async fn lease_reduces_buy_amount_and_persists_pool_state() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("capital-pool-state.json");
    let exchange = FakeExchange::new();
    let client = client(
        &exchange,
        CapitalGate::default().with_pool(pool(dec!(40)), Some(state.clone())),
    );
    client.update_last_price(dec!(82000));

    client
        .submit(StrategyOrder::market(
            "BTC/USDT",
            OrderSide::Buy,
            dec!(0.01),
        ))
        .await
        .unwrap();
    let call = exchange.create_calls.lock()[0].clone();
    assert_eq!((call.amount * dec!(82000)).round_dp(8), dec!(25));
    assert!(state.exists(), "lease state is saved");

    // Sells bypass the pool.
    client
        .submit(StrategyOrder::market(
            "BTC/USDT",
            OrderSide::Sell,
            dec!(0.01),
        ))
        .await
        .unwrap();
    assert_eq!(exchange.create_calls.lock()[1].amount, dec!(0.01));
}

#[tokio::test]
async fn paused_lease_blocks_the_buy_and_alerts() {
    let exchange = FakeExchange::new();
    let (transport, alerter) = CapturingTransport::alerter();
    // A 100% reserve leaves nothing to allocate: the module is paused.
    let client = client(
        &exchange,
        CapitalGate::default().with_pool(pool(dec!(100)), None),
    )
    .with_alerter(Some(alerter));
    client.update_last_price(dec!(82000));

    let err = client
        .submit(StrategyOrder::market(
            "BTC/USDT",
            OrderSide::Buy,
            dec!(0.01),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::Capital(CapitalGateError::Blocked(b)) if b.message == "capital pool allocation blocked buy order"),
        "{err}"
    );
    assert!(exchange.create_calls.lock().is_empty());
    assert!(transport.messages()[0].contains("capital pool allocation blocked buy order"));
}

struct FakeConvert {
    balances: Vec<(String, Decimal, Decimal)>,
    tested: Mutex<Vec<NewOrder>>,
    placed: Mutex<Vec<NewOrder>>,
}

#[async_trait]
impl ConvertExchange for FakeConvert {
    async fn spot_balances(&self) -> bb_capital::Result<Vec<(String, Decimal, Decimal)>> {
        Ok(self.balances.clone())
    }

    async fn last_prices(&self) -> bb_capital::Result<BTreeMap<String, Decimal>> {
        Ok(BTreeMap::from([("BNBUSDT".to_owned(), dec!(300))]))
    }

    async fn rules(&self, symbol: &str) -> bb_capital::Result<SymbolRules> {
        assert_eq!(symbol, "BNB/USDT");
        let info: SpotSymbolInfo = serde_json::from_value(json!({
            "symbol": "BNBUSDT", "status": "TRADING", "baseAsset": "BNB", "quoteAsset": "USDT",
            "orderTypes": ["MARKET", "LIMIT"], "isSpotTradingAllowed": true,
            "filters": [
                {"filterType": "LOT_SIZE", "minQty": "0.001", "maxQty": "9000", "stepSize": "0.001"},
                {"filterType": "NOTIONAL", "minNotional": "5", "applyMinToMarket": true, "maxNotional": "9000000",
                 "applyMaxToMarket": false, "avgPriceMins": 5}
            ]
        }))
        .unwrap();
        Ok(SymbolRules::from_spot(&info))
    }

    async fn test_order(&self, order: &NewOrder) -> bb_capital::Result<()> {
        self.tested.lock().push(order.clone());
        Ok(())
    }

    async fn create_order(&self, order: &NewOrder) -> bb_capital::Result<BinanceOrder> {
        self.placed.lock().push(order.clone());
        Ok(BinanceOrder::default())
    }
}

#[tokio::test]
async fn auto_convert_runs_on_quote_shortfall_in_plan_mode_and_skips_sells() {
    let convert = Arc::new(FakeConvert {
        balances: vec![
            ("USDT".into(), dec!(10), dec!(0)),
            ("BNB".into(), dec!(1), dec!(0)),
        ],
        tested: Mutex::new(Vec::new()),
        placed: Mutex::new(Vec::new()),
    });
    let config = AutoConvertConfig {
        enabled: true,
        ..AutoConvertConfig::default()
    };
    let gate = CapitalGate::default().with_auto_convert(
        convert.clone(),
        config,
        ConvertMode::PlanOnly,
        None,
    );
    let exchange = FakeExchange::new();
    let client = client(&exchange, gate).with_dry_run(true);
    client.update_last_price(dec!(1000));

    // 0.05 * 1000 = 50 USDT needed, 10 free: convert BNB (plan only).
    client
        .submit(StrategyOrder::market(
            "BTC/USDT",
            OrderSide::Buy,
            dec!(0.05),
        ))
        .await
        .unwrap();
    let tested = convert.tested.lock().clone();
    assert_eq!(tested.len(), 1);
    assert!(tested[0].symbol.starts_with("BNB"), "{}", tested[0].symbol);
    assert!(convert.placed.lock().is_empty(), "plan mode never submits");

    client
        .submit(StrategyOrder::market(
            "BTC/USDT",
            OrderSide::Sell,
            dec!(0.05),
        ))
        .await
        .unwrap();
    assert_eq!(convert.tested.lock().len(), 1, "sells bypass auto-convert");
    assert!(exchange.create_calls.lock().is_empty(), "dry run");
}

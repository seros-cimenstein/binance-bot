//! Test helpers: wiremock-backed Binance client and fixtures.

use std::time::Duration;

use bb_binance::{BinanceClient, BinanceConfig, Credentials};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::settings::SettingsBuilder;

/// Mainnet-shaped client pointed at `server` for spot and futures, no retries.
pub fn mock_client(server: &MockServer) -> BinanceClient {
    let config = BinanceConfig::new(false)
        .with_credentials(Credentials::new("key", "secret").unwrap())
        .with_spot_rest_url(server.uri())
        .with_futures_rest_url(server.uri())
        .with_retries(0, Duration::ZERO, Duration::ZERO);
    BinanceClient::new(config).unwrap()
}

/// Mount `GET path` returning `body`, plus the server-time endpoints.
pub async fn mount_get(server: &MockServer, route: &str, body: Value) {
    mount_time(server).await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub async fn mount_post(server: &MockServer, route: &str, status: u16, body: Value) {
    mount_time(server).await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_time(server: &MockServer) {
    for route in ["/api/v3/time", "/fapi/v1/time"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"serverTime": bb_core::time::now_ms()})),
            )
            .mount(server)
            .await;
    }
}

/// Query string + body of every request to `route`.
pub async fn requests_to(server: &MockServer, route: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == route)
        .map(|r| {
            format!(
                "{}&{}",
                r.url.query().unwrap_or(""),
                String::from_utf8_lossy(&r.body)
            )
        })
        .collect()
}

/// Paths of every non-time request, in order.
pub async fn request_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .filter(|p| !p.ends_with("/time"))
        .collect()
}

pub fn settings_builder() -> SettingsBuilder {
    SettingsBuilder::new("key", "secret")
}

/// Spot exchange info for BNB/USDT and BTC/USDT with standard filters.
pub fn spot_exchange_info() -> Value {
    let symbol = |id: &str, base: &str, tick: &str, step: &str, min_qty: &str| {
        serde_json::json!({
            "symbol": id, "status": "TRADING", "baseAsset": base, "quoteAsset": "USDT",
            "baseAssetPrecision": 8, "quoteAssetPrecision": 8, "orderTypes": ["LIMIT", "MARKET"],
            "isSpotTradingAllowed": true, "isMarginTradingAllowed": true,
            "quoteOrderQtyMarketAllowed": true, "permissions": ["SPOT"],
            "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": tick, "maxPrice": "1000000", "tickSize": tick},
                {"filterType": "LOT_SIZE", "minQty": min_qty, "maxQty": "9000", "stepSize": step},
                {"filterType": "MARKET_LOT_SIZE", "minQty": "0", "maxQty": "9000", "stepSize": "0"},
                {"filterType": "NOTIONAL", "minNotional": "5", "applyMinToMarket": true, "maxNotional": "9000000", "applyMaxToMarket": false, "avgPriceMins": 5}
            ]
        })
    };
    serde_json::json!({"symbols": [
        symbol("BNBUSDT", "BNB", "0.01", "0.001", "0.001"),
        symbol("BTCUSDT", "BTC", "0.01", "0.00001", "0.00001"),
    ]})
}

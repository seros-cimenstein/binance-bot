//! REST transport, endpoint and history tests against a wiremock Binance.

use std::time::Duration;

use bb_binance::fills::FillSummary;
use bb_binance::history::HistoryFetcher;
use bb_binance::spot::NewOrder;
use bb_binance::{BinanceClient, BinanceConfig, BinanceError, Credentials, ErrorKind, Side};
use chrono::{DateTime, Utc};
use pretty_assertions::assert_eq;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "test-api-key";
const SECRET: &str = "test-api-secret";

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn client(server: &MockServer, signed: bool) -> Result<BinanceClient, BinanceError> {
    let mut config = BinanceConfig::new(false)
        .with_spot_rest_url(server.uri())
        .with_futures_rest_url(server.uri())
        .with_retries(2, Duration::from_millis(1), Duration::from_millis(5));
    config.max_retry_after = Duration::from_secs(2);
    if signed {
        config = config.with_credentials(Credentials::new(KEY, SECRET)?);
    }
    BinanceClient::new(config)
}

async fn mount_time(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v3/time"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "serverTime": Utc::now().timestamp_millis() })),
        )
        .mount(server)
        .await;
}

fn query_string(request: &Request) -> String {
    request.url.query().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn signed_request_sends_key_header_and_valid_hmac_signature() -> TestResult {
    let server = MockServer::start().await;
    mount_time(&server).await;
    let signature_ok = |request: &Request| {
        let query = query_string(request);
        let Some((payload, signature)) = query.rsplit_once("&signature=") else {
            return false;
        };
        payload.contains("recvWindow=10000")
            && payload.contains("timestamp=")
            && bb_binance::auth::hmac_sha256_hex(SECRET, payload) == signature
    };
    Mock::given(method("GET"))
        .and(path("/api/v3/account"))
        .and(header("X-MBX-APIKEY", KEY))
        .and(signature_ok)
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-MBX-USED-WEIGHT-1M", "12")
                .set_body_json(json!({
                    "canTrade": true,
                    "balances": [{"asset": "USDT", "free": "25.5", "locked": "1.0"}],
                    "permissions": ["SPOT"]
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, true)?;
    let balance = client.asset_balance("usdt").await?;
    assert_eq!(balance.free, dec!(25.5));
    assert_eq!(balance.used, dec!(1.0));
    assert_eq!(balance.total, dec!(26.5));
    assert_eq!(client.used_weight_1m(), Some(12));
    Ok(())
}

#[tokio::test]
async fn timestamp_error_resyncs_clock_and_retries_once() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/time"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "serverTime": Utc::now().timestamp_millis() + 5_000 })),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/account"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": -1021, "msg": "Timestamp for this request is outside of the recvWindow."
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "balances": [] })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, true)?;
    client.account().await?;
    let offset = client.time_offset_ms(bb_binance::Api::Spot);
    assert!((4_000..=6_000).contains(&offset), "offset {offset}");
    Ok(())
}

#[tokio::test]
async fn persistent_timestamp_error_is_not_retried_forever() -> TestResult {
    let server = MockServer::start().await;
    mount_time(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v3/account"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({ "code": -1021, "msg": "outside recvWindow" })),
        )
        .expect(2)
        .mount(&server)
        .await;
    let err = client(&server, true)?
        .account()
        .await
        .err()
        .ok_or("expected error")?;
    assert_eq!(err.kind(), ErrorKind::Timestamp);
    assert_eq!(err.code(), Some(-1021));
    Ok(())
}

#[tokio::test]
async fn rate_limit_honours_retry_after_then_succeeds() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/ticker/bookTicker"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "code": -1003, "msg": "Too many requests" })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/ticker/bookTicker"))
        .and(query_param("symbol", "BTCUSDT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "BTCUSDT", "bidPrice": "99.5", "bidQty": "1", "askPrice": "100.5", "askQty": "1"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, false)?;
    let started = std::time::Instant::now();
    assert_eq!(client.mid_price("BTC/USDT").await?, dec!(100));
    assert!(started.elapsed() >= Duration::from_millis(950));
    Ok(())
}

#[tokio::test]
async fn long_ban_fails_fast_and_blocks_following_requests() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/ticker/price"))
        .respond_with(
            ResponseTemplate::new(418)
                .insert_header("Retry-After", "120")
                .set_body_json(json!({ "code": -1003, "msg": "Way too many requests; IP banned" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, false)?;
    let first = client.price("BTCUSDT").await.err().ok_or("expected ban")?;
    match &first {
        BinanceError::RateLimited {
            http_status,
            retry_after,
            ..
        } => {
            assert_eq!(*http_status, 418);
            assert_eq!(*retry_after, Some(Duration::from_secs(120)));
        }
        other => return Err(format!("unexpected {other}").into()),
    }
    // Second call never reaches the server (expect(1) above).
    let second = client
        .price("BTCUSDT")
        .await
        .err()
        .ok_or("expected block")?;
    assert_eq!(second.kind(), ErrorKind::RateLimit);
    Ok(())
}

#[tokio::test]
async fn retryable_5xx_is_retried_and_exhaustion_reports_status() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/ticker/price"))
        .and(query_param("symbol", "BTCUSDT"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({ "msg": "unavailable" })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/ticker/price"))
        .and(query_param("symbol", "BTCUSDT"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "symbol": "BTCUSDT", "price": "100.0" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/ticker/price"))
        .and(query_param("symbol", "ETHUSDT"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({ "msg": "rate limited" })))
        .expect(3)
        .mount(&server)
        .await;

    let client = client(&server, false)?;
    assert_eq!(client.price("BTC/USDT").await?.price, dec!(100));
    let err = client.price("ETH/USDT").await.err().ok_or("expected 429")?;
    assert_eq!(err.http_status(), Some(429));
    assert_eq!(err.endpoint(), Some("GET /api/v3/ticker/price"));
    Ok(())
}

#[tokio::test]
async fn live_order_is_not_retried_on_ambiguous_5xx_and_errors_are_sanitised() -> TestResult {
    let server = MockServer::start().await;
    mount_time(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v3/order"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/order/test"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": -2010, "msg": "Account has insufficient balance for requested action."
        })))
        .mount(&server)
        .await;

    let client = client(&server, true)?;
    let order = NewOrder::market_quote("BTC/USDT", Side::Buy, dec!(6));
    let err = client
        .create_order(&order)
        .await
        .err()
        .ok_or("expected 502")?;
    assert_eq!(err.kind(), ErrorKind::Unavailable);
    let funds = client
        .test_order(&order)
        .await
        .err()
        .ok_or("expected -2010")?;
    assert_eq!(funds.kind(), ErrorKind::InsufficientFunds);
    for rendered in [err.to_string(), format!("{funds:?}"), funds.to_string()] {
        assert!(!rendered.contains(KEY));
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains("signature"));
        assert!(!rendered.contains("timestamp="));
    }
    Ok(())
}

#[tokio::test]
async fn order_test_and_full_order_fills() -> TestResult {
    let server = MockServer::start().await;
    mount_time(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v3/order/test"))
        .and(query_param("symbol", "BTCUSDT"))
        .and(query_param("side", "BUY"))
        .and(query_param("type", "MARKET"))
        .and(query_param("quoteOrderQty", "6"))
        .and(query_param("newOrderRespType", "FULL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/order"))
        .and(query_param("type", "LIMIT"))
        .and(query_param("timeInForce", "GTC"))
        .and(query_param("price", "60000.5"))
        .and(query_param("quantity", "0.0001"))
        .and(query_param("newClientOrderId", "bb-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "BTCUSDT", "orderId": 7, "clientOrderId": "bb-1", "transactTime": 1,
            "price": "60000.5", "origQty": "0.0001", "executedQty": "0.0001", "cummulativeQuoteQty": "6.00005",
            "status": "FILLED", "type": "LIMIT", "side": "BUY",
            "fills": [{"price": "60000.5", "qty": "0.0001", "commission": "0.0000001", "commissionAsset": "BTC", "tradeId": 1}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, true)?;
    client
        .test_order(&NewOrder::market_quote("BTC/USDT", Side::Buy, dec!(6.000)))
        .await?;
    let order = client
        .create_order(
            &NewOrder::limit("BTCUSDT", Side::Buy, dec!(0.00010), dec!(60000.50))
                .client_order_id("bb-1"),
        )
        .await?;
    let fills = FillSummary::from_order(&order);
    assert_eq!(fills.avg_price, Some(dec!(60000.5)));
    assert_eq!(fills.net_base_received("BTC"), dec!(0.0000999));
    Ok(())
}

#[tokio::test]
async fn exchange_info_rules_are_cached_and_validated() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/exchangeInfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "timezone": "UTC", "serverTime": 1, "rateLimits": [],
            "symbols": [{
                "symbol": "BTCUSDT", "status": "TRADING", "baseAsset": "BTC", "baseAssetPrecision": 8,
                "quoteAsset": "USDT", "quotePrecision": 8, "quoteAssetPrecision": 8,
                "orderTypes": ["LIMIT", "LIMIT_MAKER", "MARKET"], "quoteOrderQtyMarketAllowed": true,
                "isSpotTradingAllowed": true, "isMarginTradingAllowed": true,
                "permissions": [], "permissionSets": [["SPOT", "MARGIN"]],
                "filters": [
                    {"filterType": "PRICE_FILTER", "minPrice": "0.01", "maxPrice": "1000000", "tickSize": "0.01"},
                    {"filterType": "LOT_SIZE", "minQty": "0.00001", "maxQty": "9000", "stepSize": "0.00001"},
                    {"filterType": "NOTIONAL", "minNotional": "5", "applyMinToMarket": true, "maxNotional": "9000000", "applyMaxToMarket": false, "avgPriceMins": 5}
                ]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, false)?;
    let rules = client.symbol_rules("BTC/USDT").await?;
    assert!(rules.has_permission("MARGIN"));
    assert_eq!(rules.round_qty(dec!(0.123456)), dec!(0.12345));
    let markets = client.spot_markets(false).await?;
    assert_eq!(markets.unified("btcusdt"), Some("BTC/USDT"));
    assert!(markets.get("btc-usdt").is_some());
    assert!(matches!(
        client.symbol_rules("ETH/USDT").await,
        Err(BinanceError::InvalidSymbol(_))
    ));
    Ok(())
}

fn kline_row(open_time: i64) -> Value {
    json!([
        open_time,
        "1.0",
        "2.0",
        "0.5",
        "1.5",
        "10.0",
        open_time + 59_999,
        "15.0",
        42,
        "4.0",
        "6.0",
        "0"
    ])
}

fn ts(ms: i64) -> Result<DateTime<Utc>, Box<dyn std::error::Error>> {
    DateTime::from_timestamp_millis(ms).ok_or_else(|| "bad timestamp".into())
}

#[tokio::test]
async fn history_paginates_klines() -> TestResult {
    let server = MockServer::start().await;
    let end = 1_499 * 60_000;
    Mock::given(method("GET"))
        .and(path("/api/v3/klines"))
        .and(query_param("symbol", "BTCUSDT"))
        .and(query_param("interval", "1m"))
        .and(query_param("startTime", "0"))
        .and(query_param("endTime", end.to_string()))
        .and(query_param("limit", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(
            (0..1_000).map(|i| kline_row(i * 60_000)).collect(),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/klines"))
        .and(query_param("startTime", (999 * 60_000 + 1).to_string()))
        .and(query_param("endTime", end.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(
            (1_000..1_500).map(|i| kline_row(i * 60_000)).collect(),
        )))
        .expect(1)
        .mount(&server)
        .await;

    let fetcher = HistoryFetcher::spot(client(&server, false)?);
    let rows = fetcher
        .fetch_ohlcv("BTC/USDT", "1m", ts(0)?, Some(ts(end)?))
        .await?;
    assert_eq!(rows.len(), 1_500);
    assert_eq!(rows[0].timestamp, 0);
    assert_eq!(rows[0].close, dec!(1.5));
    assert_eq!(rows[1_499].timestamp, end);
    Ok(())
}

#[tokio::test]
async fn history_sorts_partial_page_normalises_symbol_and_rejects_bad_range() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/klines"))
        .and(query_param("symbol", "ETHUSDT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            kline_row(120_000),
            kline_row(0),
            kline_row(60_000)
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let fetcher = HistoryFetcher::spot(client(&server, false)?);
    let rows = fetcher
        .fetch_klines("eth-usdt", "1m", ts(0)?, Some(ts(180_000)?))
        .await?;
    assert_eq!(
        rows.iter().map(|k| k.open_time).collect::<Vec<_>>(),
        vec![0, 60_000, 120_000]
    );
    let err = fetcher
        .fetch_klines("BTCUSDT", "1m", ts(60_000)?, Some(ts(0)?))
        .await
        .err()
        .ok_or("expected error")?;
    assert!(err.to_string().contains("start_time"));
    Ok(())
}

#[tokio::test]
async fn futures_premium_index_and_leverage_brackets_parse() -> TestResult {
    let server = MockServer::start().await;
    mount_time(&server).await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/time"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "serverTime": Utc::now().timestamp_millis() })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/premiumIndex"))
        .and(query_param("symbol", "BTCUSDT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "BTCUSDT", "markPrice": "11793.63104562", "indexPrice": "11781.80495970",
            "estimatedSettlePrice": "11781.16138815", "lastFundingRate": "0.00038246", "interestRate": "0.00010000",
            "nextFundingTime": 1597392000000_i64, "time": 1597370495002_i64
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/leverageBracket"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "ETHUSDT", "notionalCoef": 1.5,
            "brackets": [{"bracket": 1, "initialLeverage": 75, "notionalCap": 10000, "notionalFloor": 0, "maintMarginRatio": 0.0065, "cum": 0}]
        })))
        .mount(&server)
        .await;
    let client = client(&server, true)?;
    let premium = client.premium_index("BTC/USDT:USDT").await?;
    assert_eq!(premium.last_funding_rate, dec!(0.00038246));
    let brackets = client.leverage_brackets(Some("ETH/USDT:USDT")).await?;
    assert_eq!(brackets[0].brackets[0].maint_margin_ratio, dec!(0.0065));
    assert_eq!(brackets[0].brackets[0].initial_leverage, 75);
    Ok(())
}

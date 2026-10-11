//! Async client for the public Polymarket CLOB REST API (no auth).
//!
//! Endpoints:
//! - `GET /sampling-simplified-markets`: active, accepting-orders markets with their
//!   outcome token IDs and last-traded prices, paginated via `next_cursor`.
//! - `GET /book?token_id=...`: orderbook for one outcome token.
//!
//! Payload parsing is deliberately lenient: malformed markets, tokens and levels are
//! dropped instead of failing a whole scan.

use std::collections::HashMap;
use std::time::Duration;

use futures::{Stream, StreamExt, TryStreamExt, stream};
use rust_decimal::Decimal;
use serde_json::Value;

pub const CLOB_API_BASE: &str = "https://clob.polymarket.com";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// Cursor Polymarket returns on the last page (base64 of `-1`).
pub const END_CURSOR: &str = "LTE=";

#[derive(Debug, thiserror::Error)]
pub enum PolymarketError {
    #[error("{endpoint} HTTP {status}: {preview}")]
    Http {
        endpoint: &'static str,
        status: u16,
        preview: String,
    },
    #[error("polymarket request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("{0}")]
    Malformed(&'static str),
}

/// One outcome token within a market (e.g. "Yes" or "No").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketToken {
    pub token_id: String,
    pub outcome: String,
    pub last_price: Decimal,
}

/// The simplified-market shape returned by `/sampling-simplified-markets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimplifiedMarket {
    pub condition_id: String,
    pub tokens: Vec<MarketToken>,
    pub accepting_orders: bool,
    pub closed: bool,
}

/// One price level on a side of the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderbookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

/// Snapshot of one token's orderbook; both sides are sorted best-first
/// (bids highest price first, asks lowest price first).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Orderbook {
    pub market: String,
    pub asset_id: String,
    pub timestamp_ms: i64,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
}

impl Orderbook {
    pub fn best_bid(&self) -> Option<&OrderbookLevel> {
        self.bids.first()
    }

    pub fn best_ask(&self) -> Option<&OrderbookLevel> {
        self.asks.first()
    }
}

/// One page of markets plus the cursor for the next page (`None` when exhausted).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarketsPage {
    pub markets: Vec<SimplifiedMarket>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PolymarketClient {
    http: reqwest::Client,
    base_url: String,
}

impl PolymarketClient {
    /// Client with its own connection pool and the default request timeout.
    pub fn new(base_url: &str) -> Result<Self, PolymarketError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()?;
        Ok(Self::with_http(http, base_url))
    }

    /// Reuse an existing `reqwest::Client` (its timeouts apply).
    pub fn with_http(http: reqwest::Client, base_url: &str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetch one page of active markets.
    pub async fn fetch_active_markets_page(
        &self,
        cursor: &str,
        limit: usize,
    ) -> Result<MarketsPage, PolymarketError> {
        let mut query = vec![("limit", limit.to_string())];
        if !cursor.is_empty() {
            query.push(("next_cursor", cursor.to_owned()));
        }
        let response = self
            .http
            .get(format!("{}/sampling-simplified-markets", self.base_url))
            .query(&query)
            .send()
            .await?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            let body = response.text().await.unwrap_or_default();
            return Err(PolymarketError::Http {
                endpoint: "sampling-simplified-markets",
                status: status.as_u16(),
                preview: body.chars().take(200).collect(),
            });
        }
        let payload: Value = response.json().await?;
        Ok(parse_markets_page(&payload))
    }

    /// Every active market, paging until the cursor is exhausted or repeats,
    /// fetching at most `max_pages` pages. Pages are fetched lazily, so a consumer
    /// that stops early does not pay for the remaining pages.
    pub fn active_markets(
        &self,
        max_pages: usize,
        limit_per_page: usize,
    ) -> impl Stream<Item = Result<SimplifiedMarket, PolymarketError>> + '_ {
        struct Paging {
            cursor: String,
            pages: usize,
            done: bool,
        }
        let start = Paging {
            cursor: String::new(),
            pages: 0,
            done: false,
        };
        stream::try_unfold(start, move |mut state| async move {
            if state.done || state.pages >= max_pages {
                return Ok::<_, PolymarketError>(None);
            }
            let page = self
                .fetch_active_markets_page(&state.cursor, limit_per_page)
                .await?;
            state.pages += 1;
            match page.next_cursor {
                Some(next) if next != state.cursor && next != END_CURSOR => state.cursor = next,
                _ => state.done = true,
            }
            let markets = stream::iter(page.markets.into_iter().map(Ok::<_, PolymarketError>));
            Ok(Some((markets, state)))
        })
        .try_flatten()
    }

    /// Orderbook for one token; `None` when the token has no book.
    pub async fn fetch_orderbook(
        &self,
        token_id: &str,
    ) -> Result<Option<Orderbook>, PolymarketError> {
        let response = self
            .http
            .get(format!("{}/book", self.base_url))
            .query(&[("token_id", token_id)])
            .send()
            .await?;
        if response.status() != reqwest::StatusCode::OK {
            return Ok(None);
        }
        let payload: Value = response.json().await?;
        if payload
            .as_object()
            .is_some_and(|obj| obj.contains_key("error") && !obj.contains_key("asset_id"))
        {
            return Ok(None);
        }
        parse_orderbook(&payload).map(Some)
    }

    /// Fetch many orderbooks with bounded concurrency, dropping tokens without a book.
    pub async fn fetch_orderbooks(
        &self,
        token_ids: &[String],
        concurrency: usize,
    ) -> Result<HashMap<String, Orderbook>, PolymarketError> {
        stream::iter(token_ids)
            .map(|token_id| async move {
                let book = self.fetch_orderbook(token_id).await?;
                Ok::<_, PolymarketError>(book.map(|book| (token_id.clone(), book)))
            })
            .buffer_unordered(concurrency.max(1))
            .try_filter_map(|entry| async move { Ok(entry) })
            .try_collect()
            .await
    }
}

pub fn parse_markets_page(payload: &Value) -> MarketsPage {
    let Some(data) = payload.get("data").and_then(Value::as_array) else {
        return MarketsPage::default();
    };
    MarketsPage {
        markets: data.iter().filter_map(parse_simplified_market).collect(),
        next_cursor: truthy_string(payload.get("next_cursor")),
    }
}

pub fn parse_simplified_market(payload: &Value) -> Option<SimplifiedMarket> {
    let obj = payload.as_object()?;
    let condition_id = truthy_string(obj.get("condition_id"))?;
    let tokens = obj
        .get("tokens")
        .and_then(Value::as_array)
        .map(|raw| raw.iter().filter_map(parse_market_token).collect())
        .unwrap_or_default();
    Some(SimplifiedMarket {
        condition_id,
        tokens,
        accepting_orders: obj.get("accepting_orders").is_some_and(is_truthy),
        closed: obj.get("closed").is_some_and(is_truthy),
    })
}

fn parse_market_token(payload: &Value) -> Option<MarketToken> {
    let obj = payload.as_object()?;
    let token_id = truthy_string(obj.get("token_id"))?;
    let outcome = truthy_string(obj.get("outcome"))?;
    Some(MarketToken {
        token_id,
        outcome,
        last_price: lenient_decimal(obj.get("price")).unwrap_or(Decimal::ZERO),
    })
}

pub fn parse_orderbook(payload: &Value) -> Result<Orderbook, PolymarketError> {
    let obj = payload.as_object().ok_or(PolymarketError::Malformed(
        "orderbook payload was not an object",
    ))?;
    let mut bids = parse_levels(obj.get("bids"));
    bids.sort_by_key(|level| std::cmp::Reverse(level.price));
    let mut asks = parse_levels(obj.get("asks"));
    asks.sort_by_key(|level| level.price);
    Ok(Orderbook {
        market: truthy_string(obj.get("market")).unwrap_or_default(),
        asset_id: truthy_string(obj.get("asset_id")).unwrap_or_default(),
        timestamp_ms: lenient_int(obj.get("timestamp")),
        bids,
        asks,
    })
}

fn parse_levels(rows: Option<&Value>) -> Vec<OrderbookLevel> {
    rows.and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let row = row.as_object()?;
                    let price = lenient_decimal(row.get("price"))?;
                    let size = lenient_decimal(row.get("size"))?;
                    (price > Decimal::ZERO && size > Decimal::ZERO)
                        .then_some(OrderbookLevel { price, size })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Python truthiness of a JSON value.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => !number_is_zero(n),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn number_is_zero(n: &serde_json::Number) -> bool {
    parse_decimal(&n.to_string()).is_some_and(|d| d.is_zero())
}

/// `str(value)` for truthy values, `None` for missing/falsy ones.
fn truthy_string(value: Option<&Value>) -> Option<String> {
    let value = value.filter(|v| is_truthy(v))?;
    Some(match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn parse_decimal(text: &str) -> Option<Decimal> {
    let text = text.trim();
    Decimal::from_str_exact(text)
        .or_else(|_| Decimal::from_scientific(text))
        .ok()
}

/// `float(value or 0)`: missing/falsy is zero, unparsable is `None`.
fn lenient_decimal(value: Option<&Value>) -> Option<Decimal> {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Some(Decimal::ZERO),
        Some(Value::Number(n)) => parse_decimal(&n.to_string()),
        Some(Value::String(s)) if s.is_empty() => Some(Decimal::ZERO),
        Some(Value::String(s)) => parse_decimal(s),
        Some(_) => None,
    }
}

/// `int(value or 0)` with unparsable input falling back to zero.
fn lenient_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| parse_decimal(&n.to_string()).and_then(|d| d.trunc().try_into().ok()))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rust_decimal_macros::dec;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn prices(levels: &[OrderbookLevel]) -> Vec<Decimal> {
        levels.iter().map(|l| l.price).collect()
    }

    #[test]
    fn parse_simplified_market_extracts_tokens() {
        let market = parse_simplified_market(&json!({
            "condition_id": "0xabc",
            "tokens": [
                {"token_id": "T1", "outcome": "Yes", "price": 0.42},
                {"token_id": "T2", "outcome": "No", "price": 0.58},
            ],
            "accepting_orders": true, "closed": false,
        }))
        .unwrap();
        assert_eq!(market.condition_id, "0xabc");
        assert_eq!(market.tokens.len(), 2);
        assert_eq!(market.tokens[0].outcome, "Yes");
        assert_eq!(market.tokens[0].last_price, dec!(0.42));
        assert!(market.accepting_orders);
        assert!(!market.closed);
    }

    #[test]
    fn parse_simplified_market_skips_payload_missing_id() {
        assert_eq!(parse_simplified_market(&json!({"tokens": []})), None);
    }

    #[test]
    fn parse_simplified_market_drops_malformed_tokens() {
        let market = parse_simplified_market(&json!({
            "condition_id": "0xabc",
            "tokens": [
                {"token_id": "T1", "outcome": "Yes", "price": "not-a-num"},
                {"missing_id": true},
            ],
            "accepting_orders": true, "closed": false,
        }))
        .unwrap();
        assert_eq!(market.tokens.len(), 1);
        assert_eq!(market.tokens[0].outcome, "Yes");
        assert_eq!(market.tokens[0].last_price, Decimal::ZERO);
    }

    #[test]
    fn parse_orderbook_sorts_sides_best_first() {
        let book = parse_orderbook(&json!({
            "market": "0xm", "asset_id": "T1", "timestamp": "123",
            "bids": [
                {"price": "0.10", "size": "100"},
                {"price": "0.05", "size": "200"},
                {"price": "0.12", "size": "50"},
            ],
            "asks": [
                {"price": "0.30", "size": "20"},
                {"price": "0.99", "size": "1"},
                {"price": "0.25", "size": "10"},
            ],
        }))
        .unwrap();
        assert_eq!(prices(&book.bids), vec![dec!(0.12), dec!(0.10), dec!(0.05)]);
        assert_eq!(prices(&book.asks), vec![dec!(0.25), dec!(0.30), dec!(0.99)]);
        assert_eq!(book.best_bid().map(|l| l.price), Some(dec!(0.12)));
        assert_eq!(book.best_ask().map(|l| l.price), Some(dec!(0.25)));
        assert_eq!(book.timestamp_ms, 123);
    }

    #[test]
    fn parse_orderbook_returns_empty_sides_when_missing() {
        let book =
            parse_orderbook(&json!({"market": "x", "asset_id": "y", "timestamp": "0"})).unwrap();
        assert!(book.bids.is_empty() && book.asks.is_empty());
        assert!(book.best_bid().is_none() && book.best_ask().is_none());
    }

    #[test]
    fn parse_orderbook_drops_invalid_levels() {
        let book = parse_orderbook(&json!({
            "market": "x", "asset_id": "y", "timestamp": "0",
            "bids": [
                {"price": "0", "size": "100"},
                {"price": "0.5", "size": "0"},
                {"price": "0.4", "size": "10"},
                {"price": "junk", "size": "5"},
            ],
            "asks": "not-a-list",
        }))
        .unwrap();
        assert_eq!(prices(&book.bids), vec![dec!(0.4)]);
        assert!(book.asks.is_empty());
    }

    #[test]
    fn parse_orderbook_rejects_non_object() {
        let err = parse_orderbook(&json!("not a mapping")).unwrap_err();
        assert!(err.to_string().contains("not an object"));
    }

    fn market_json(id: &str, a: &str, b: &str) -> Value {
        json!({"condition_id": id, "tokens": [
            {"token_id": a, "outcome": "Yes", "price": 0.5},
            {"token_id": b, "outcome": "No", "price": 0.5},
        ], "accepting_orders": true, "closed": false})
    }

    #[tokio::test]
    async fn fetch_active_markets_page_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sampling-simplified-markets"))
            .and(query_param("limit", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [market_json("0x1", "A", "B")],
                "next_cursor": "abc123",
            })))
            .mount(&server)
            .await;
        let client = PolymarketClient::new(&server.uri()).unwrap();
        let page = client.fetch_active_markets_page("", 100).await.unwrap();
        assert_eq!(page.markets.len(), 1);
        assert_eq!(page.markets[0].condition_id, "0x1");
        assert_eq!(page.next_cursor.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn fetch_active_markets_page_errors_on_http_failure() {
        let server = MockServer::start().await;
        Mock::given(path("/sampling-simplified-markets"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = PolymarketClient::new(&server.uri()).unwrap();
        let err = client.fetch_active_markets_page("", 100).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "sampling-simplified-markets HTTP 500: boom"
        );
    }

    #[tokio::test]
    async fn active_markets_paginates_until_no_cursor() {
        let server = MockServer::start().await;
        Mock::given(path("/sampling-simplified-markets"))
            .and(query_param("next_cursor", "cursor_2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [market_json("0x2", "A2", "B2")], "next_cursor": "",
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/sampling-simplified-markets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [market_json("0x1", "A1", "B1")], "next_cursor": "cursor_2",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = PolymarketClient::new(&server.uri()).unwrap();
        let seen: Vec<String> = client
            .active_markets(50, 100)
            .map_ok(|m| m.condition_id)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(seen, vec!["0x1", "0x2"]);
    }

    #[tokio::test]
    async fn active_markets_stops_at_end_cursor() {
        let server = MockServer::start().await;
        Mock::given(path("/sampling-simplified-markets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [market_json("0x1", "A1", "B1")], "next_cursor": END_CURSOR,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = PolymarketClient::new(&server.uri()).unwrap();
        let seen: Vec<SimplifiedMarket> =
            client.active_markets(50, 100).try_collect().await.unwrap();
        assert_eq!(seen.len(), 1);
    }

    #[tokio::test]
    async fn fetch_orderbook_returns_none_on_404() {
        let server = MockServer::start().await;
        let client = PolymarketClient::new(&server.uri()).unwrap();
        assert_eq!(client.fetch_orderbook("T1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn fetch_orderbook_returns_none_when_no_book_error() {
        let server = MockServer::start().await;
        Mock::given(path("/book"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"error": "No orderbook exists for the requested token id"}),
                ),
            )
            .mount(&server)
            .await;
        let client = PolymarketClient::new(&server.uri()).unwrap();
        assert_eq!(client.fetch_orderbook("T1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn fetch_orderbook_returns_book_when_present() {
        let server = MockServer::start().await;
        Mock::given(path("/book"))
            .and(query_param("token_id", "T1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "market": "0xm", "asset_id": "T1", "timestamp": "999",
                "bids": [{"price": "0.4", "size": "10"}],
                "asks": [{"price": "0.5", "size": "5"}],
            })))
            .mount(&server)
            .await;
        let client = PolymarketClient::new(&server.uri()).unwrap();
        let book = client.fetch_orderbook("T1").await.unwrap().unwrap();
        assert_eq!(book.best_bid().map(|l| l.price), Some(dec!(0.4)));
        assert_eq!(book.best_ask().map(|l| l.price), Some(dec!(0.5)));
    }

    #[tokio::test]
    async fn fetch_orderbooks_skips_missing() {
        let server = MockServer::start().await;
        Mock::given(path("/book"))
            .and(query_param("token_id", "T_missing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": "no book"})))
            .mount(&server)
            .await;
        for token in ["T1", "T2"] {
            Mock::given(path("/book"))
                .and(query_param("token_id", token))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "market": "x", "asset_id": token, "timestamp": "1",
                    "bids": [{"price": "0.4", "size": "10"}],
                    "asks": [{"price": "0.5", "size": "5"}],
                })))
                .mount(&server)
                .await;
        }
        let client = PolymarketClient::new(&server.uri()).unwrap();
        let ids = ["T1", "T_missing", "T2"].map(String::from);
        let books = client.fetch_orderbooks(&ids, 8).await.unwrap();
        let mut keys: Vec<&str> = books.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["T1", "T2"]);
        assert_eq!(books["T2"].asset_id, "T2");
    }
}

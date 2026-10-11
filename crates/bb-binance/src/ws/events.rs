//! Typed WebSocket events and payload parsing (port of the pydantic models in
//! `src/exchange/websocket.py`).

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::models::PriceLevel;

/// Event categories subscribers can filter on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    TickerUpdate,
    MiniTickerUpdate,
    BookTickerUpdate,
    TradeUpdate,
    OrderBookUpdate,
    KlineUpdate,
    MarkPriceUpdate,
    AccountPosition,
    BalanceUpdate,
    OrderUpdate,
    UserStreamExpired,
    StreamError,
    RawMessage,
}

/// `@ticker` (24hrTicker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickerUpdate {
    pub symbol: String,
    pub event_time: Option<i64>,
    pub price: Decimal,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
    pub open_price: Option<Decimal>,
    pub high_price: Option<Decimal>,
    pub low_price: Option<Decimal>,
    pub base_volume: Option<Decimal>,
    pub quote_volume: Option<Decimal>,
}

/// `@miniTicker`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiniTickerUpdate {
    pub symbol: String,
    pub event_time: Option<i64>,
    pub close: Decimal,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub base_volume: Decimal,
    pub quote_volume: Decimal,
}

/// `@bookTicker`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookTickerUpdate {
    pub symbol: String,
    pub update_id: Option<i64>,
    pub event_time: Option<i64>,
    pub bid_price: Decimal,
    pub bid_qty: Decimal,
    pub ask_price: Decimal,
    pub ask_qty: Decimal,
}

/// `@trade` / `@aggTrade`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeUpdate {
    pub symbol: String,
    pub event_time: Option<i64>,
    pub trade_id: Option<i64>,
    pub price: Decimal,
    pub quantity: Decimal,
    pub trade_time: Option<i64>,
    pub is_buyer_maker: Option<bool>,
    pub aggregate: bool,
}

/// Partial-depth / diff-depth update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookUpdate {
    pub symbol: String,
    pub event_time: Option<i64>,
    pub last_update_id: Option<i64>,
    pub first_update_id: Option<i64>,
    pub final_update_id: Option<i64>,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

/// `@kline_<interval>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KlineUpdate {
    pub symbol: String,
    pub interval: String,
    pub event_time: Option<i64>,
    pub open_time: i64,
    pub close_time: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub is_closed: bool,
}

/// Futures `@markPrice` (`markPriceUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkPriceUpdate {
    pub symbol: String,
    pub event_time: Option<i64>,
    pub mark_price: Decimal,
    pub index_price: Option<Decimal>,
    pub funding_rate: Option<Decimal>,
    pub next_funding_time: Option<i64>,
}

/// A balance row of `outboundAccountPosition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBalance {
    pub asset: String,
    pub free: Decimal,
    pub locked: Decimal,
}

/// `outboundAccountPosition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountPositionUpdate {
    pub event_time: i64,
    pub last_account_update_time: Option<i64>,
    pub balances: Vec<AccountBalance>,
}

/// `balanceUpdate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceUpdate {
    pub event_time: i64,
    pub asset: String,
    pub delta: Decimal,
    pub clear_time: Option<i64>,
}

/// `executionReport`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub event_time: i64,
    pub symbol: String,
    pub client_order_id: String,
    pub side: String,
    pub order_type: String,
    pub time_in_force: Option<String>,
    pub order_quantity: Option<Decimal>,
    pub order_price: Option<Decimal>,
    pub stop_price: Option<Decimal>,
    pub iceberg_quantity: Option<Decimal>,
    pub order_list_id: Option<i64>,
    pub original_client_order_id: Option<String>,
    pub execution_type: String,
    pub order_status: String,
    pub reject_reason: Option<String>,
    pub order_id: i64,
    pub last_executed_quantity: Option<Decimal>,
    pub cumulative_filled_quantity: Option<Decimal>,
    pub last_executed_price: Option<Decimal>,
    pub commission_amount: Option<Decimal>,
    pub commission_asset: Option<String>,
    pub trade_time: Option<i64>,
    pub trade_id: Option<i64>,
    pub order_creation_time: Option<i64>,
    pub quote_order_quantity: Option<Decimal>,
    pub last_quote_transacted: Option<Decimal>,
    pub cumulative_quote_transacted: Option<Decimal>,
    pub is_maker: Option<bool>,
    pub is_buyer: Option<bool>,
    /// Original event payload.
    pub raw: Value,
}

/// User-data stream terminated (`eventStreamTerminated` / `listenKeyExpired`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStreamExpired {
    pub event_time: Option<i64>,
    pub raw: Value,
}

/// Receiver-side failure surfaced to subscribers (drives failure alerts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamError {
    pub stream: String,
    pub message: String,
    pub error_type: Option<String>,
    pub raw: Value,
}

/// Valid but unsupported message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawMessage {
    pub stream: String,
    pub raw: Value,
}

/// Event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum WsPayload {
    Ticker(TickerUpdate),
    MiniTicker(MiniTickerUpdate),
    BookTicker(BookTickerUpdate),
    Trade(TradeUpdate),
    OrderBook(OrderBookUpdate),
    Kline(KlineUpdate),
    MarkPrice(MarkPriceUpdate),
    AccountPosition(AccountPositionUpdate),
    Balance(BalanceUpdate),
    Order(Box<ExecutionReport>),
    UserStreamExpired(UserStreamExpired),
    StreamError(StreamError),
    Raw(RawMessage),
}

impl WsPayload {
    /// Category of this payload.
    pub fn event_type(&self) -> EventType {
        match self {
            Self::Ticker(_) => EventType::TickerUpdate,
            Self::MiniTicker(_) => EventType::MiniTickerUpdate,
            Self::BookTicker(_) => EventType::BookTickerUpdate,
            Self::Trade(_) => EventType::TradeUpdate,
            Self::OrderBook(_) => EventType::OrderBookUpdate,
            Self::Kline(_) => EventType::KlineUpdate,
            Self::MarkPrice(_) => EventType::MarkPriceUpdate,
            Self::AccountPosition(_) => EventType::AccountPosition,
            Self::Balance(_) => EventType::BalanceUpdate,
            Self::Order(_) => EventType::OrderUpdate,
            Self::UserStreamExpired(_) => EventType::UserStreamExpired,
            Self::StreamError(_) => EventType::StreamError,
            Self::Raw(_) => EventType::RawMessage,
        }
    }
}

/// Envelope delivered to queues and callbacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsEvent {
    pub payload: WsPayload,
    pub received_at: DateTime<Utc>,
}

impl WsEvent {
    /// Wrap a payload stamped with the current time.
    pub fn new(payload: WsPayload) -> Self {
        Self {
            payload,
            received_at: Utc::now(),
        }
    }

    /// Category of the payload.
    pub fn event_type(&self) -> EventType {
        self.payload.event_type()
    }

    pub(crate) fn stream_error(
        stream: &str,
        message: &str,
        error_type: Option<&str>,
        raw: Value,
    ) -> Self {
        Self::new(WsPayload::StreamError(StreamError {
            stream: stream.to_owned(),
            message: message.to_owned(),
            error_type: error_type.map(str::to_owned),
            raw,
        }))
    }

    pub(crate) fn raw(stream: &str, raw: Value) -> Self {
        Self::new(WsPayload::Raw(RawMessage {
            stream: stream.to_owned(),
            raw,
        }))
    }
}

/// Field-level parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError(pub String);

type Obj = Map<String, Value>;
type Parsed<T> = Result<T, ParseError>;

fn opt_dec(obj: &Obj, key: &str) -> Parsed<Option<Decimal>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Decimal::from_str(s.trim())
            .or_else(|_| Decimal::from_scientific(s.trim()))
            .map(Some)
            .map_err(|_| ParseError(format!("{key} is not numeric: {s:?}"))),
        Some(Value::Number(n)) => {
            let text = n.to_string();
            Decimal::from_str(&text)
                .or_else(|_| Decimal::from_scientific(&text))
                .map(Some)
                .map_err(|_| ParseError(format!("{key} is not numeric: {text}")))
        }
        Some(other) => Err(ParseError(format!("{key} is not numeric: {other}"))),
    }
}

fn dec(obj: &Obj, key: &str) -> Parsed<Decimal> {
    opt_dec(obj, key)?.ok_or_else(|| ParseError(format!("numeric value {key} is required")))
}

fn opt_int(obj: &Obj, key: &str) -> Parsed<Option<i64>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| ParseError(format!("{key} is not an integer"))),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| ParseError(format!("{key} is not an integer"))),
        Some(other) => Err(ParseError(format!("{key} is not an integer: {other}"))),
    }
}

fn int(obj: &Obj, key: &str) -> Parsed<i64> {
    opt_int(obj, key)?.ok_or_else(|| ParseError(format!("integer value {key} is required")))
}

fn opt_bool(obj: &Obj, key: &str) -> Option<bool> {
    match obj.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" => Some(true),
            "false" | "0" | "no" | "n" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|v| v != 0),
        _ => None,
    }
}

fn opt_str(obj: &Obj, key: &str) -> Option<String> {
    match obj.get(key)? {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn text(obj: &Obj, key: &str) -> String {
    opt_str(obj, key).unwrap_or_default()
}

fn levels(value: Option<&Value>) -> Vec<PriceLevel> {
    let Some(Value::Array(rows)) = value else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let Value::Array(pair) = row else { return None };
            let num = |v: &Value| match v {
                Value::String(s) => Decimal::from_str(s).ok(),
                Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
                _ => None,
            };
            Some(PriceLevel {
                price: num(pair.first()?)?,
                quantity: num(pair.get(1)?)?,
            })
        })
        .collect()
}

/// Resolves exchange ids back to the symbol spelling the caller subscribed with.
#[derive(Debug, Default, Clone)]
pub(crate) struct SymbolLookup(pub HashMap<String, String>);

impl SymbolLookup {
    /// `payload.s` (or the stream prefix) mapped through the subscription table.
    pub fn resolve(&self, stream: &str, payload: &Obj) -> String {
        let raw = opt_str(payload, "s").or_else(|| {
            stream
                .split_once('@')
                .map(|(prefix, _)| prefix.to_uppercase())
                .filter(|p| !p.is_empty())
        });
        match raw {
            Some(raw) => self.0.get(&raw.to_uppercase()).cloned().unwrap_or(raw),
            None => "UNKNOWN".to_owned(),
        }
    }
}

/// Classify and parse one public market-stream payload (`data` of a combined stream).
pub(crate) fn parse_public(stream: &str, data: &Obj, lookup: &SymbolLookup) -> Parsed<WsPayload> {
    let event = data.get("e").and_then(Value::as_str).unwrap_or_default();
    let symbol = || lookup.resolve(stream, data);
    if event == "24hrTicker" || stream.ends_with("@ticker") {
        let price = match opt_dec(data, "c")? {
            Some(p) => p,
            None => opt_dec(data, "last")?
                .ok_or_else(|| ParseError("ticker payload did not include a price".into()))?,
        };
        return Ok(WsPayload::Ticker(TickerUpdate {
            symbol: symbol(),
            event_time: opt_int(data, "E")?,
            price,
            bid: opt_dec(data, "b")?,
            ask: opt_dec(data, "a")?,
            open_price: opt_dec(data, "o")?,
            high_price: opt_dec(data, "h")?,
            low_price: opt_dec(data, "l")?,
            base_volume: opt_dec(data, "v")?,
            quote_volume: opt_dec(data, "q")?,
        }));
    }
    if event == "24hrMiniTicker" || stream.ends_with("@miniTicker") {
        return Ok(WsPayload::MiniTicker(MiniTickerUpdate {
            symbol: symbol(),
            event_time: opt_int(data, "E")?,
            close: dec(data, "c")?,
            open: dec(data, "o")?,
            high: dec(data, "h")?,
            low: dec(data, "l")?,
            base_volume: dec(data, "v")?,
            quote_volume: dec(data, "q")?,
        }));
    }
    if event == "bookTicker" || stream.ends_with("@bookTicker") {
        return Ok(WsPayload::BookTicker(BookTickerUpdate {
            symbol: symbol(),
            update_id: opt_int(data, "u")?,
            event_time: opt_int(data, "E")?,
            bid_price: dec(data, "b")?,
            bid_qty: dec(data, "B")?,
            ask_price: dec(data, "a")?,
            ask_qty: dec(data, "A")?,
        }));
    }
    if event == "trade"
        || event == "aggTrade"
        || stream.ends_with("@trade")
        || stream.ends_with("@aggTrade")
    {
        let aggregate = event == "aggTrade" || stream.ends_with("@aggTrade");
        return Ok(WsPayload::Trade(TradeUpdate {
            symbol: symbol(),
            event_time: opt_int(data, "E")?,
            trade_id: if aggregate {
                opt_int(data, "a")?
            } else {
                opt_int(data, "t")?
            },
            price: dec(data, "p")?,
            quantity: dec(data, "q")?,
            trade_time: opt_int(data, "T")?,
            is_buyer_maker: opt_bool(data, "m"),
            aggregate,
        }));
    }
    if event == "markPriceUpdate" || stream.contains("@markPrice") {
        return Ok(WsPayload::MarkPrice(MarkPriceUpdate {
            symbol: symbol(),
            event_time: opt_int(data, "E")?,
            mark_price: dec(data, "p")?,
            index_price: opt_dec(data, "i")?,
            funding_rate: opt_dec(data, "r")?,
            next_funding_time: opt_int(data, "T")?,
        }));
    }
    let is_book = stream.contains("depth")
        || event == "depthUpdate"
        || data.contains_key("lastUpdateId")
        || (data.contains_key("b") && data.contains_key("a") && data.contains_key("u"));
    if is_book {
        return Ok(WsPayload::OrderBook(OrderBookUpdate {
            symbol: symbol(),
            event_time: opt_int(data, "E")?,
            last_update_id: opt_int(data, "lastUpdateId")?,
            first_update_id: opt_int(data, "U")?,
            final_update_id: opt_int(data, "u")?,
            bids: levels(data.get("bids").or_else(|| data.get("b"))),
            asks: levels(data.get("asks").or_else(|| data.get("a"))),
        }));
    }
    if event == "kline" || stream.contains("@kline_") || data.get("k").is_some_and(Value::is_object)
    {
        let kline = match data.get("k") {
            Some(Value::Object(k)) => k,
            Some(_) => {
                return Err(ParseError(
                    "kline payload did not include candle data".into(),
                ));
            }
            None => data,
        };
        let mut symbol = symbol();
        if symbol == "UNKNOWN" {
            symbol = lookup.resolve(stream, kline);
        }
        return Ok(WsPayload::Kline(KlineUpdate {
            symbol,
            interval: text(kline, "i"),
            event_time: opt_int(data, "E")?,
            open_time: int(kline, "t")?,
            close_time: int(kline, "T")?,
            open: dec(kline, "o")?,
            high: dec(kline, "h")?,
            low: dec(kline, "l")?,
            close: dec(kline, "c")?,
            volume: dec(kline, "v")?,
            is_closed: opt_bool(kline, "x")
                .ok_or_else(|| ParseError("boolean value x is required".into()))?,
        }));
    }
    Ok(WsPayload::Raw(RawMessage {
        stream: stream.to_owned(),
        raw: Value::Object(data.clone()),
    }))
}

/// Outcome of parsing a user-data message.
pub(crate) enum UserParse {
    Event(WsPayload),
    /// Stream terminated: emit and reconnect.
    Terminated(WsPayload),
}

/// Parse a user-data event (WS-API `{"subscriptionId", "event": {...}}` or legacy flat body).
pub(crate) fn parse_user(message: &Obj, lookup: &SymbolLookup) -> Parsed<UserParse> {
    let payload = match message.get("event") {
        Some(Value::Object(event)) => event,
        _ => message,
    };
    let event = payload.get("e").and_then(Value::as_str).unwrap_or_default();
    Ok(UserParse::Event(match event {
        "outboundAccountPosition" => {
            let balances = match payload.get("B") {
                Some(Value::Array(rows)) => rows
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|b| {
                        Ok(AccountBalance {
                            asset: text(b, "a"),
                            free: dec(b, "f")?,
                            locked: dec(b, "l")?,
                        })
                    })
                    .collect::<Parsed<Vec<_>>>()?,
                _ => Vec::new(),
            };
            WsPayload::AccountPosition(AccountPositionUpdate {
                event_time: int(payload, "E")?,
                last_account_update_time: opt_int(payload, "u")?,
                balances,
            })
        }
        "balanceUpdate" => WsPayload::Balance(BalanceUpdate {
            event_time: int(payload, "E")?,
            asset: text(payload, "a"),
            delta: dec(payload, "d")?,
            clear_time: opt_int(payload, "T")?,
        }),
        "executionReport" => WsPayload::Order(Box::new(ExecutionReport {
            event_time: int(payload, "E")?,
            symbol: lookup.resolve("user", payload),
            client_order_id: text(payload, "c"),
            side: text(payload, "S"),
            order_type: text(payload, "o"),
            time_in_force: opt_str(payload, "f"),
            order_quantity: opt_dec(payload, "q")?,
            order_price: opt_dec(payload, "p")?,
            stop_price: opt_dec(payload, "P")?,
            iceberg_quantity: opt_dec(payload, "F")?,
            order_list_id: opt_int(payload, "g")?,
            original_client_order_id: opt_str(payload, "C"),
            execution_type: text(payload, "x"),
            order_status: text(payload, "X"),
            reject_reason: opt_str(payload, "r"),
            order_id: int(payload, "i")?,
            last_executed_quantity: opt_dec(payload, "l")?,
            cumulative_filled_quantity: opt_dec(payload, "z")?,
            last_executed_price: opt_dec(payload, "L")?,
            commission_amount: opt_dec(payload, "n")?,
            commission_asset: opt_str(payload, "N"),
            trade_time: opt_int(payload, "T")?,
            trade_id: opt_int(payload, "t")?,
            order_creation_time: opt_int(payload, "O")?,
            quote_order_quantity: opt_dec(payload, "Q")?,
            last_quote_transacted: opt_dec(payload, "Y")?,
            cumulative_quote_transacted: opt_dec(payload, "Z")?,
            is_maker: opt_bool(payload, "m"),
            is_buyer: opt_bool(payload, "M"),
            raw: Value::Object(payload.clone()),
        })),
        "eventStreamTerminated" | "listenKeyExpired" => {
            return Ok(UserParse::Terminated(WsPayload::UserStreamExpired(
                UserStreamExpired {
                    event_time: opt_int(payload, "E")?,
                    raw: Value::Object(message.clone()),
                },
            )));
        }
        _ => WsPayload::Raw(RawMessage {
            stream: "user".into(),
            raw: Value::Object(message.clone()),
        }),
    }))
}

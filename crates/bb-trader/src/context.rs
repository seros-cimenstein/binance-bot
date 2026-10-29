//! Decision context snapshot, `context_hash` and `route_id` (same hashing as Python).

use std::fmt::Write as _;

use bb_pool::RouteQuote;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::TraderConfig;
use crate::memory::TradeMemory;
use crate::universe::{Balances, RouteCandidate};

/// `SRC->DST:` + first 12 hex chars of sha256(`src|dst|sym:side:a>b,...`).
pub fn route_id(route: &RouteQuote) -> String {
    let legs = route
        .legs
        .iter()
        .map(|leg| {
            format!(
                "{}:{}:{}>{}",
                leg.symbol, leg.side, leg.source_asset, leg.destination_asset
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let raw = format!("{}|{}|{legs}", route.source_asset, route.destination_asset);
    let digest = hex::encode(Sha256::digest(raw.as_bytes()));
    format!(
        "{}->{}:{}",
        route.source_asset,
        route.destination_asset,
        &digest[..12]
    )
}

/// Exact decimal as a JSON number.
pub fn decimal_json(value: Decimal) -> Value {
    serde_json::from_str(&value.normalize().to_string()).unwrap_or(Value::Null)
}

pub fn opt_decimal_json(value: Option<Decimal>) -> Value {
    value.map_or(Value::Null, decimal_json)
}

/// Python `json.dumps(value, sort_keys=True)` (default separators, `ensure_ascii`).
pub fn python_json(value: &Value) -> String {
    let mut out = String::new();
    write_python_json(value, &mut out);
    out
}

fn write_python_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_python_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_python_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_python_string(key, out);
                out.push_str(": ");
                write_python_json(&map[key], out);
            }
            out.push('}');
        }
    }
}

fn write_python_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// sha256 hex of the Python-style sorted JSON.
pub fn hash_payload(payload: &Value) -> String {
    hex::encode(Sha256::digest(python_json(payload).as_bytes()))
}

/// Invariants the runtime enforces regardless of what the bot asks for.
pub fn execution_invariants(config: &TraderConfig) -> Vec<String> {
    vec![
        "execute_route only accepts route_id values quoted in the current cycle (cycle prompt or list_routes); \
         quotes older than the staleness limit are re-quoted and refused if the path changed"
            .into(),
        format!(
            "Spot MARKET orders only, at most {} serial legs per route; no margin, futures, transfers, \
             withdrawals, convert or prediction markets",
            config.max_route_legs
        ),
        "each leg is sized from the live free balance with a fee/slippage reserve and rounded to Binance \
         LOT_SIZE / min-qty / min-notional filters; legs below exchange minimums are refused"
            .into(),
        "Binance POST /api/v3/order/test must accept every leg before it is submitted".into(),
        "every leg must reach a terminal FILLED status and increase the destination balance; otherwise the \
         route halts and live trading stays halted until the operator reviews it"
            .into(),
        "the operator emergency stop file halts trading before any leg".into(),
        "operator scope applies: allowed/blocked symbols, assets and quote assets; fiat assets are blocked \
         when fiat blocking is enabled"
            .into(),
        "in plan-only mode execute_route records a plan and never places orders".into(),
    ]
}

/// Operator scope and information settings recorded with the context.
pub fn route_scope(config: &TraderConfig) -> Value {
    json!({
        "allowed_symbols": config.scope.allowed_symbols,
        "allowed_assets": config.scope.allowed_assets,
        "blocked_symbols": config.scope.blocked_symbols,
        "blocked_assets": config.scope.blocked_assets,
        "blocked_quotes": config.scope.blocked_quotes,
        "block_fiat_assets": config.scope.block_fiat_assets,
        "max_route_legs": config.max_route_legs,
        "trade_memory_limit": config.trade_memory_limit,
    })
}

/// Everything the cycle knows when the rule decides.
#[derive(Debug, Clone)]
pub struct DecisionContext {
    pub timestamp: String,
    pub rules_version: String,
    pub market_pick_mode: &'static str,
    pub balances: Balances,
    pub portfolio_value_usdt: Decimal,
    pub candidate_routes: Vec<RouteCandidate>,
    pub execution_invariants: Vec<String>,
    pub route_scope: Value,
    pub trade_memory: TradeMemory,
    /// 20-coin liquid USDT crypto market breadth (`None` when disabled or unavailable).
    pub market_mood: Option<crate::market_mood::MarketView>,
    /// The /biz/ feed (`None` in tests; the cycle always reads it); the rule scores on it.
    pub biz: Option<crate::biz::BizNow>,
}

impl DecisionContext {
    /// The recorded context (`--evidence-only` output; hashed into `context_hash`).
    pub fn as_payload(&self) -> Value {
        json!({
            "timestamp": self.timestamp,
            "rules_version": self.rules_version,
            "market_pick_mode": self.market_pick_mode,
            "balances": self.balances,
            "portfolio_value_usdt": decimal_json(self.portfolio_value_usdt),
            "candidate_routes": self.candidate_contexts(),
            "execution_invariants": self.execution_invariants,
            "route_scope": self.route_scope,
            "trade_memory": self.trade_memory,
            "market_mood": self.market_mood,
            "biz": self.biz,
        })
    }

    pub fn candidate_contexts(&self) -> Vec<Value> {
        self.candidate_routes
            .iter()
            .map(RouteCandidate::as_context)
            .collect()
    }

    pub fn context_hash(&self) -> String {
        hash_payload(&self.as_payload())
    }
}

#[cfg(test)]
mod tests {
    use bb_pool::LegSide;

    use super::*;
    use crate::scope::tests::{leg, route};

    #[test]
    fn route_id_matches_python_hash() {
        let quote = route(
            "USDT",
            "SOL",
            vec![
                leg("ETH/USDT", LegSide::Buy, "USDT", "ETH"),
                leg("SOL/ETH", LegSide::Buy, "ETH", "SOL"),
            ],
        );
        // python3: hashlib.sha256(b"USDT|SOL|ETH/USDT:buy:USDT>ETH,SOL/ETH:buy:ETH>SOL").hexdigest()[:12]
        assert_eq!(route_id(&quote), "USDT->SOL:5d6024387aed");
    }

    #[test]
    fn python_json_matches_json_dumps_sort_keys() {
        let value = json!({"b": [1, 2.5, null], "a": {"z": true, "y": "é\"\n"}, "c": "ok"});
        // python3 -c 'import json;print(json.dumps({...}, sort_keys=True))'
        assert_eq!(
            python_json(&value),
            r#"{"a": {"y": "\u00e9\"\n", "z": true}, "b": [1, 2.5, null], "c": "ok"}"#
        );
        assert_eq!(python_json(&json!({})), "{}");
        assert_eq!(python_json(&json!("😀")), r#""\ud83d\ude00""#);
        assert_eq!(
            hash_payload(&json!({"a": 1})),
            hex::encode(Sha256::digest(br#"{"a": 1}"#))
        );
    }

    #[test]
    fn decimal_json_is_an_exact_number() {
        assert_eq!(decimal_json(Decimal::new(12_300, 4)).to_string(), "1.23");
        assert_eq!(decimal_json(Decimal::ZERO).to_string(), "0");
        assert_eq!(opt_decimal_json(None), Value::Null);
    }
}

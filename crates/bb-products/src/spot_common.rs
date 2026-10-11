//! Helpers shared by the spot live paths (canary, allocator, probe): balance
//! snapshots, tickers, market rules, order sanitising, daily audit usage.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bb_binance::fills::FillSummary;
use bb_binance::spot::{AccountInfo, Order, OrderRef};
use bb_binance::{BinanceClient, BinanceError, SymbolRules};
use bb_core::money::{ceil_to_step, floor_to_step};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde_json::Value;

use crate::commission::CommissionSnapshot;
use crate::common::{jdec, num, num_opt};

/// Positive balances by group (`free`/`used`/`total`), as the Python
/// `_sanitize_balance` produced from a ccxt balance. Empty groups are omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BalanceSnapshot {
    #[serde(skip_serializing_if = "BTreeMap::is_empty", serialize_with = "dec_map")]
    pub free: BTreeMap<String, Decimal>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", serialize_with = "dec_map")]
    pub used: BTreeMap<String, Decimal>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", serialize_with = "dec_map")]
    pub total: BTreeMap<String, Decimal>,
}

fn dec_map<S: serde::Serializer>(
    map: &BTreeMap<String, Decimal>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut out = serializer.serialize_map(Some(map.len()))?;
    for (asset, amount) in map {
        out.serialize_entry(asset, &dec_value(*amount))?;
    }
    out.end()
}

/// Decimal as a JSON number.
pub fn dec_value(value: Decimal) -> Value {
    value
        .normalize()
        .to_string()
        .parse::<serde_json::Number>()
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

impl BalanceSnapshot {
    pub fn from_account(account: &AccountInfo) -> Self {
        let mut snapshot = Self::default();
        for balance in &account.balances {
            let asset = balance.asset.to_uppercase();
            let total = balance.free + balance.locked;
            for (group, amount) in [
                (&mut snapshot.free, balance.free),
                (&mut snapshot.used, balance.locked),
                (&mut snapshot.total, total),
            ] {
                if amount > Decimal::ZERO {
                    group.insert(asset.clone(), amount);
                }
            }
        }
        snapshot
    }

    pub fn free(&self, asset: &str) -> Option<Decimal> {
        self.free.get(asset).copied().filter(|v| *v > Decimal::ZERO)
    }

    pub fn total(&self, asset: &str) -> Decimal {
        self.total.get(asset).copied().unwrap_or(Decimal::ZERO)
    }
}

/// Read the spot balance snapshot (`GET /api/v3/account`).
pub async fn fetch_balance(client: &BinanceClient) -> Result<BalanceSnapshot, BinanceError> {
    Ok(BalanceSnapshot::from_account(&client.account().await?))
}

/// ccxt-style ticker fields used for pricing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpotTicker {
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
    pub last: Option<Decimal>,
}

impl SpotTicker {
    /// Sell price: bid, else last.
    pub fn sell_price(&self) -> Option<Decimal> {
        self.bid.or(self.last)
    }

    /// Buy price: ask, else last.
    pub fn buy_price(&self) -> Option<Decimal> {
        self.ask.or(self.last)
    }

    /// `(ask - bid) / mid * 10_000`, `None` without a sane book.
    pub fn spread_bps(&self) -> Option<Decimal> {
        let (bid, ask) = (self.bid?, self.ask?);
        if ask < bid {
            return None;
        }
        ((ask - bid) * dec!(10000)).checked_div((ask + bid) / dec!(2))
    }
}

/// 24h ticker (bid/ask/last), as ccxt `fetch_ticker` provided.
pub async fn fetch_ticker(
    client: &BinanceClient,
    symbol: &str,
) -> Result<SpotTicker, BinanceError> {
    let ticker = client.ticker_24h(symbol).await?;
    let positive = |v: Option<Decimal>| v.filter(|d| *d > Decimal::ZERO);
    Ok(SpotTicker {
        bid: positive(ticker.bid_price),
        ask: positive(ticker.ask_price),
        last: positive(Some(ticker.last_price)),
    })
}

/// Active (trading) spot market rules for a unified symbol.
pub fn active_market(markets: &bb_binance::markets::Markets, symbol: &str) -> Option<SymbolRules> {
    markets
        .get(symbol)
        .filter(|rules| rules.is_trading())
        .cloned()
}

pub fn min_amount(rules: &SymbolRules) -> Decimal {
    rules
        .min_qty()
        .filter(|v| *v > Decimal::ZERO)
        .unwrap_or(Decimal::ZERO)
}

pub fn min_notional(rules: &SymbolRules) -> Decimal {
    rules
        .min_notional()
        .filter(|v| *v > Decimal::ZERO)
        .unwrap_or(Decimal::ZERO)
}

pub fn round_down(rules: &SymbolRules, amount: Decimal) -> Decimal {
    floor_to_step(amount, rules.step_size())
}

pub fn round_up(rules: &SymbolRules, amount: Decimal) -> Decimal {
    ceil_to_step(amount, rules.step_size())
}

/// One fee entry (ccxt `fees`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderFee {
    pub currency: String,
    #[serde(with = "num")]
    pub cost: Decimal,
}

/// Whitelisted order fields kept in audits (Python `_sanitize_order`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SanitizedOrder {
    pub id: String,
    #[serde(rename = "clientOrderId")]
    pub client_order_id: String,
    pub symbol: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub side: String,
    pub status: String,
    #[serde(with = "num")]
    pub amount: Decimal,
    #[serde(with = "num")]
    pub filled: Decimal,
    #[serde(with = "num")]
    pub remaining: Decimal,
    #[serde(with = "num_opt")]
    pub average: Option<Decimal>,
    #[serde(with = "num_opt")]
    pub price: Option<Decimal>,
    #[serde(with = "num")]
    pub cost: Decimal,
    pub fees: Vec<OrderFee>,
    pub timestamp: Option<i64>,
    pub datetime: Option<String>,
}

impl SanitizedOrder {
    pub fn from_order(order: &Order, unified_symbol: &str) -> Self {
        let summary = FillSummary::from_order(order);
        let timestamp = order.transact_time.or(order.time);
        Self {
            id: order.order_id.to_string(),
            client_order_id: order.client_order_id.clone(),
            symbol: unified_symbol.to_owned(),
            order_type: order.order_type.to_lowercase(),
            side: order.side.to_lowercase(),
            status: order.status.ccxt().to_owned(),
            amount: order.orig_qty,
            filled: order.executed_qty,
            remaining: (order.orig_qty - order.executed_qty).max(Decimal::ZERO),
            average: order.average_price(),
            price: order
                .average_price()
                .or((order.price > Decimal::ZERO).then_some(order.price)),
            cost: order.cummulative_quote_qty,
            fees: fee_summary(&summary),
            timestamp,
            datetime: timestamp
                .and_then(bb_core::time::from_ms)
                .map(crate::common::iso),
        }
    }
}

/// Commission totals per asset as ccxt-style fee rows.
pub fn fee_summary(summary: &FillSummary) -> Vec<OrderFee> {
    summary
        .commissions
        .iter()
        .map(|(asset, cost)| OrderFee {
            currency: asset.clone(),
            cost: *cost,
        })
        .collect()
}

/// Poll a non-terminal order up to three times, one second apart.
pub async fn final_order_status(
    client: &BinanceClient,
    symbol: &str,
    order: Order,
) -> Result<Order, BinanceError> {
    if order.status.is_terminal() || order.order_id <= 0 {
        return Ok(order);
    }
    let mut latest = order;
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        latest = client
            .query_order(symbol, &OrderRef::Id(latest.order_id))
            .await?;
        if latest.status.is_terminal() {
            break;
        }
    }
    Ok(latest)
}

/// Commission block recorded in plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommissionSummary {
    pub symbol: String,
    #[serde(with = "num")]
    pub taker_rate: Decimal,
    pub discount_enabled: bool,
    pub discount_asset: String,
    pub retrieved_at: String,
}

impl CommissionSummary {
    pub fn new(commission: &CommissionSnapshot, side: &str) -> Self {
        Self {
            symbol: commission.symbol.clone(),
            taker_rate: commission.taker_rate(side, true),
            discount_enabled: commission.discount_enabled,
            discount_asset: commission.discount_asset.clone(),
            retrieved_at: crate::common::iso(commission.retrieved_at),
        }
    }
}

/// One leg of a direct route quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteLeg {
    pub symbol: String,
    pub side: String,
    pub source_asset: String,
    pub destination_asset: String,
    #[serde(with = "num")]
    pub input_amount: Decimal,
    #[serde(with = "num")]
    pub spent_amount: Decimal,
    #[serde(with = "num")]
    pub gross_output_amount: Decimal,
    #[serde(with = "num")]
    pub fee_rate: Decimal,
    #[serde(with = "num")]
    pub fee_output_amount: Decimal,
    #[serde(with = "num")]
    pub net_output_amount: Decimal,
    pub commission_discount_asset: String,
}

/// Direct route quote recorded in plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteQuote {
    pub source_asset: String,
    pub destination_asset: String,
    #[serde(with = "num")]
    pub input_amount: Decimal,
    #[serde(with = "num")]
    pub output_amount: Decimal,
    #[serde(with = "num")]
    pub slippage_bps: Decimal,
    pub legs: Vec<RouteLeg>,
}

/// Today's usage from an audit JSONL (Python `_daily_usage`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DailyUsage {
    pub orders: u32,
    pub notional: Decimal,
    pub realized_loss_usdt: Decimal,
    pub equity_high_usdt: Decimal,
}

/// Count today's submission attempts and their planned notional.
pub fn daily_usage(path: &Path, today: &str) -> DailyUsage {
    let mut usage = DailyUsage::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return usage;
    };
    let positive = |v: Option<&Value>| {
        jdec(v)
            .filter(|d| *d > Decimal::ZERO)
            .unwrap_or(Decimal::ZERO)
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(payload) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let timestamp = payload
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !timestamp.starts_with(today) {
            continue;
        }
        let plan = payload.get("plan").filter(|p| p.is_object());
        let reconciliation = payload.get("reconciliation").filter(|p| p.is_object());
        for value in [
            positive(plan.and_then(|p| p.get("portfolio_value_usdt_before"))),
            positive(plan.and_then(|p| p.get("portfolio_value_usdt_after"))),
            positive(reconciliation.and_then(|r| r.get("portfolio_value_usdt_after"))),
        ] {
            usage.equity_high_usdt = usage.equity_high_usdt.max(value);
        }
        if !payload
            .get("submission_attempted")
            .is_some_and(crate::common::truthy)
        {
            continue;
        }
        usage.orders += 1;
        usage.notional += positive(plan.and_then(|p| p.get("estimated_notional_usdt")));
        usage.realized_loss_usdt += positive(payload.get("realized_loss_usdt"));
    }
    usage
}

/// UTC date `YYYY-MM-DD` for today.
pub fn today() -> String {
    bb_core::time::now().date_naive().to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn balance_snapshot_keeps_positive_amounts() {
        let account: AccountInfo = serde_json::from_value(json!({
            "balances": [
                {"asset": "BNB", "free": "0.5", "locked": "0"},
                {"asset": "usdt", "free": "0", "locked": "2"},
                {"asset": "ETH", "free": "0", "locked": "0"}
            ]
        }))
        .unwrap();
        let snapshot = BalanceSnapshot::from_account(&account);
        assert_eq!(snapshot.free("BNB"), Some(dec!(0.5)));
        assert_eq!(snapshot.free("USDT"), None);
        assert_eq!(snapshot.total("USDT"), dec!(2));
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["free"]["BNB"].to_string(), "0.5");
        assert!(json["used"].get("BNB").is_none());
        assert!(json["total"].get("ETH").is_none());
    }

    #[test]
    fn spread_and_daily_usage() {
        let ticker = SpotTicker {
            bid: Some(dec!(99)),
            ask: Some(dec!(101)),
            last: Some(dec!(100)),
        };
        assert_eq!(ticker.spread_bps(), Some(dec!(200)));
        assert_eq!(
            SpotTicker {
                ask: Some(dec!(98)),
                ..ticker
            }
            .spread_bps(),
            None
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        std::fs::write(
            &path,
            [
                json!({"timestamp": "2026-10-11T01:00:00+00:00", "submission_attempted": true, "plan": {"estimated_notional_usdt": 5.5, "portfolio_value_usdt_before": 100}}).to_string(),
                json!({"timestamp": "2026-10-11T02:00:00+00:00", "submission_attempted": false, "plan": {"estimated_notional_usdt": 6, "portfolio_value_usdt_after": 120}}).to_string(),
                json!({"timestamp": "2026-10-10T02:00:00+00:00", "submission_attempted": true, "plan": {"estimated_notional_usdt": 6}}).to_string(),
                "not json".to_owned(),
            ]
            .join("\n"),
        )
        .unwrap();
        let usage = daily_usage(&path, "2026-10-11");
        assert_eq!(usage.orders, 1);
        assert_eq!(usage.notional, dec!(5.5));
        assert_eq!(usage.equity_high_usdt, dec!(120));
        assert_eq!(
            daily_usage(&dir.path().join("missing"), "2026-10-11"),
            DailyUsage::default()
        );
    }
}

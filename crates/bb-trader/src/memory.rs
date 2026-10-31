//! Trade memory from the audit JSONL: closed/open outcomes, PnL, current holding and the
//! loss streak. Information for the bot only; nothing here blocks a trade.

use std::collections::BTreeMap;
use std::path::Path;

use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

use bb_core::money::{BPS_PER_UNIT, to_f64};

use crate::trades::{Mark, RouteRecord, Trade, TradeStatus, build_trades, route_record};

/// Python reads the last 500 audit lines.
pub const AUDIT_TAIL_LINES: usize = 500;

/// Audit rows (JSON objects) and the read error, if any.
#[derive(Debug, Clone, Default)]
pub struct AuditRows {
    pub rows: Vec<Value>,
    pub error: Option<String>,
}

/// Last [`AUDIT_TAIL_LINES`] parseable object rows; a missing file is empty history.
pub fn load_audit_rows(path: &Path) -> AuditRows {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return AuditRows::default(),
        Err(error) => {
            return AuditRows {
                rows: Vec::new(),
                error: Some(error.to_string()),
            };
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(AUDIT_TAIL_LINES);
    AuditRows {
        rows: lines[start..]
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(Value::is_object)
            .collect(),
        error: None,
    }
}

/// Lower-case `status` of a row.
pub fn row_status(row: &Value) -> String {
    row.get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase()
}

/// `submission_attempted` truthy or any `submitted_legs`.
pub fn row_attempted_submission(row: &Value) -> bool {
    row.get("submission_attempted").is_some_and(truthy)
        || row.get("submitted_legs").is_some_and(truthy)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// An operator review marker that clears a failed-route halt.
pub fn row_failed_submission_reviewed(row: &Value) -> bool {
    matches!(row_status(row).as_str(), "reviewed" | "operator_reviewed")
        && row.pointer("/operator_review/failed_submission_reviewed") == Some(&Value::Bool(true))
}

/// `decision.route_id`, else `selected_route.route_id`.
pub fn row_route_id(row: &Value) -> String {
    ["/decision/route_id", "/selected_route/route_id"]
        .iter()
        .find_map(|pointer| row.pointer(pointer).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

/// Upper-case `decision.<field>`, else `selected_route.<field>` (`source_asset`, `destination_asset`).
pub fn row_asset(row: &Value, field: &str) -> String {
    ["decision", "selected_route"]
        .iter()
        .find_map(|section| {
            row.get(section)
                .and_then(|s| s.get(field))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default()
        .to_uppercase()
}

/// Finite decimal from a JSON number or numeric string.
pub fn value_decimal(value: Option<&Value>) -> Option<Decimal> {
    match value? {
        Value::Number(number) => {
            let text = number.to_string();
            Decimal::from_str_exact(&text)
                .or_else(|_| Decimal::from_scientific(&text))
                .ok()
        }
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn selected_decimal(row: &Value, field: &str) -> Option<Decimal> {
    value_decimal(row.get("selected_route").and_then(|s| s.get(field)))
}

/// One submitted route and how it has done since.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeOutcome {
    pub timestamp: String,
    /// The sleeve that placed the route (empty before Phase 53 D5).
    pub sleeve: String,
    pub route_id: String,
    pub source_asset: String,
    pub destination_asset: String,
    /// USDT paid for the position ([`crate::trades`]: fills when available, else the quote).
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub entry_value_usdt: Option<Decimal>,
    /// USDT the closing sale fetched for it, before fees.
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub exit_value_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub current_value_usdt: Option<Decimal>,
    /// Result after the buy and sell fees, in bps of the entry (the dashboard's P&L).
    pub pnl_bps: Option<f64>,
    /// `closed`, `open`, `cash` (a sale into cash or fiat: parking, no P&L) or `unknown`.
    pub status: &'static str,
    pub route_friction_bps: Option<f64>,
}

/// The asset bought by a sleeve's last submitted route.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurrentHolding {
    /// `open`, `cash` (parked in cash or fiat), `not_current` or `unknown`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::arbitrary_precision_option"
    )]
    pub amount: Option<Decimal>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::arbitrary_precision_option"
    )]
    pub entry_value_usdt: Option<Decimal>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::arbitrary_precision_option"
    )]
    pub current_value_usdt: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_pnl_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
}

/// Trade memory packet (Python `_trade_memory_context`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeMemory {
    /// `ready`, `empty` or `unavailable`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub recent_trade_count: usize,
    /// Consecutive trailing losing outcomes (information only).
    pub recent_loss_streak: usize,
    /// Per sleeve (Phase 53 D5; `""` for routes from before sleeves), what its last
    /// submitted route bought.
    pub current_holdings: BTreeMap<String, CurrentHolding>,
    pub recent_outcomes: Vec<TradeOutcome>,
}

impl TradeMemory {
    /// Build from audit rows. `value_usdt(asset, amount)` marks open positions to market.
    pub fn from_rows(
        rows: &AuditRows,
        free: &BTreeMap<String, Decimal>,
        value_usdt: impl Fn(&str, Decimal) -> Option<Decimal>,
        limit: usize,
    ) -> Self {
        if let Some(error) = &rows.error {
            return Self::empty("unavailable", error.clone());
        }
        let submitted: Vec<&Value> = rows
            .rows
            .iter()
            .filter(|row| row_status(row) == "submitted" && row_attempted_submission(row))
            .collect();
        if submitted.is_empty() {
            return Self::empty(
                "empty",
                "no submitted Bot Spot routes in audit history".into(),
            );
        }
        let records: Vec<RouteRecord> = submitted.iter().map(|row| route_record(row)).collect();
        let mark = |asset: &str| {
            Some(Mark {
                free: free.get(asset).copied().unwrap_or_default(),
                price_usdt: value_usdt(asset, Decimal::ONE)?,
            })
        };
        let trades = build_trades(&records, mark, bb_core::time::now());
        let outcomes = outcomes(&submitted, &trades);
        let recent = if limit == 0 {
            outcomes
        } else {
            outcomes[outcomes.len().saturating_sub(limit)..].to_vec()
        };
        Self {
            status: "ready",
            reason: None,
            recent_trade_count: submitted.len(),
            recent_loss_streak: loss_streak(&recent),
            current_holdings: current_holdings(&submitted, &records, &trades, free, &value_usdt),
            recent_outcomes: recent,
        }
    }

    fn empty(status: &'static str, reason: String) -> Self {
        Self {
            status,
            reason: Some(reason),
            recent_trade_count: 0,
            recent_loss_streak: 0,
            current_holdings: BTreeMap::new(),
            recent_outcomes: Vec::new(),
        }
    }
}

/// Each sleeve's last submitted route as its [`CurrentHolding`].
fn current_holdings(
    submitted: &[&Value],
    records: &[RouteRecord],
    trades: &[Trade],
    free: &BTreeMap<String, Decimal>,
    value_usdt: &impl Fn(&str, Decimal) -> Option<Decimal>,
) -> BTreeMap<String, CurrentHolding> {
    let mut last: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        last.insert(record.sleeve.as_str(), index);
    }
    last.into_iter()
        .map(|(sleeve, index)| {
            let holding = current_holding(
                submitted[index],
                records[index].bought_qty,
                trades.get(index),
                free,
                value_usdt,
            );
            (sleeve.to_owned(), holding)
        })
        .collect()
}

/// Net result of `trade` in bps of what it cost.
fn trade_pnl_bps(trade: &Trade) -> Option<f64> {
    let (pnl, cost) = trade.pnl_usdt.zip(trade.cost_usdt)?;
    (cost > Decimal::ZERO).then(|| to_f64(pnl / cost * BPS_PER_UNIT))
}

/// One outcome per submitted route; `trades` is [`build_trades`] over the same routes.
fn outcomes(submitted: &[&Value], trades: &[Trade]) -> Vec<TradeOutcome> {
    submitted
        .iter()
        .zip(trades)
        .map(|(row, trade)| {
            let destination = row_asset(row, "destination_asset");
            let mut outcome = TradeOutcome {
                timestamp: row
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                sleeve: trade.sleeve.clone(),
                route_id: row_route_id(row),
                source_asset: row_asset(row, "source_asset"),
                destination_asset: destination.clone(),
                entry_value_usdt: trade.cost_usdt,
                exit_value_usdt: None,
                current_value_usdt: None,
                pnl_bps: None,
                status: "unknown",
                route_friction_bps: selected_decimal(row, "immediate_friction_bps").map(to_f64),
            };
            if crate::assets::is_low_upside(&destination) {
                // Into cash or fiat: parking, not a bet.
                outcome.status = "cash";
                return outcome;
            }
            match trade.status {
                TradeStatus::Win | TradeStatus::Loss => {
                    outcome.status = "closed";
                    outcome.exit_value_usdt = trade.exit_usdt;
                    outcome.pnl_bps = trade_pnl_bps(trade);
                }
                TradeStatus::Open => {
                    outcome.status = "open";
                    outcome.current_value_usdt = trade.exit_usdt;
                    outcome.pnl_bps = trade_pnl_bps(trade);
                }
                TradeStatus::Cash => outcome.status = "cash",
                TradeStatus::Unknown => {}
            }
            outcome
        })
        .collect()
}

/// What a sleeve's last submitted route bought (`bought`: the quantity it received, capped
/// at the free balance two sleeves may share); `trade` is that route in the trade ledger.
fn current_holding(
    last: &Value,
    bought: Option<Decimal>,
    trade: Option<&Trade>,
    free: &BTreeMap<String, Decimal>,
    value_usdt: &impl Fn(&str, Decimal) -> Option<Decimal>,
) -> CurrentHolding {
    let blank = CurrentHolding {
        status: "unknown",
        asset: None,
        reason: Some("last route destination is unknown"),
        amount: None,
        entry_value_usdt: None,
        current_value_usdt: None,
        unrealized_pnl_bps: None,
        entered_at: None,
        route_id: None,
    };
    let destination = row_asset(last, "destination_asset");
    if destination.is_empty() {
        return blank;
    }
    let Some(amount) = free
        .get(&destination)
        .copied()
        .filter(|a| *a > Decimal::ZERO)
        .map(|free| bought.filter(|b| *b > Decimal::ZERO).map_or(free, |b| b.min(free)))
    else {
        return CurrentHolding {
            status: "not_current",
            asset: Some(destination),
            reason: Some("last route destination is not a positive free balance"),
            ..blank
        };
    };
    let current = value_usdt(&destination, amount);
    if crate::assets::is_low_upside(&destination) {
        // Parked in cash: the balance is not a position with a return.
        return CurrentHolding {
            status: "cash",
            reason: None,
            amount: Some(amount),
            current_value_usdt: current,
            asset: Some(destination),
            ..blank
        };
    }
    // Entry: what the position cost (fills); unrealized: marked now, net of the buy fee.
    let entry = trade.and_then(|t| t.cost_usdt);
    CurrentHolding {
        status: "open",
        reason: None,
        amount: Some(amount),
        entry_value_usdt: entry,
        current_value_usdt: current,
        unrealized_pnl_bps: trade.and_then(trade_pnl_bps),
        entered_at: Some(
            last.get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        route_id: Some(row_route_id(last)),
        asset: Some(destination),
    }
}

/// Consecutive trailing losing outcomes, skipping parking in cash (stops at unknown or
/// non-negative).
pub fn loss_streak(outcomes: &[TradeOutcome]) -> usize {
    outcomes
        .iter()
        .rev()
        .filter(|outcome| outcome.status != "cash")
        .take_while(|outcome| outcome.pnl_bps.is_some_and(|pnl| pnl < 0.0))
        .count()
}

#[cfg(test)]
pub(crate) mod tests {
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;

    pub(crate) fn submitted_row(
        source: &str,
        destination: &str,
        entry: f64,
        source_value: f64,
    ) -> Value {
        json!({
            "timestamp": "2026-10-01T00:00:00+00:00",
            "status": "submitted",
            "submission_attempted": true,
            "decision": {"action": "rebalance", "route_id": format!("{source}->{destination}:abc"),
                         "source_asset": source, "destination_asset": destination},
            "selected_route": {"estimated_output_usdt": entry, "source_value_usdt": source_value,
                               "immediate_friction_bps": 12.5},
            "submitted_legs": [{"symbol": "X"}],
        })
    }

    fn value(asset: &str, amount: Decimal) -> Option<Decimal> {
        match asset {
            "USDT" => Some(amount),
            "ETH" => Some(amount * dec!(3000)),
            _ => None,
        }
    }

    /// One filled market order against USDT with its fee in BNB.
    fn filled_row(at: &str, from: &str, to: &str, filled: &str, cost: &str, bnb_fee: f64) -> Value {
        let (coin, side) = if from == "USDT" { (to, "buy") } else { (from, "sell") };
        json!({"timestamp": at, "status": "submitted", "submission_attempted": true, "row_type": "route",
               "decision": {"source_asset": from, "destination_asset": to},
               // The quote reserves slippage and the undiscounted fee: worse than the fills.
               "selected_route": {"source_value_usdt": 8.0, "estimated_output_usdt": 7.94,
                                  "input_amount": 8.0, "output_amount": 0.05},
               "submitted_legs": [{"symbol": format!("{coin}/USDT"), "side": side,
                   "order": {"filled": filled, "cost": cost, "fees": [{"cost": bnb_fee, "currency": "BNB"}]}}]})
    }

    #[test]
    fn outcomes_are_net_of_both_fees_from_the_fills_like_the_dashboard() {
        // AAVE 2026-10-18: bought 0.052 for 7.93728, sold 0.052 for 7.98668, 0.0000083 BNB
        // fee each way. The old memory used the quotes (estimated output vs next quote).
        let rows = vec![
            filled_row("2026-10-18T02:36:56Z", "USDT", "AAVE", "0.052", "7.93728", 0.0000083),
            filled_row("2026-10-18T04:04:53Z", "AAVE", "USDT", "0.052", "7.98668", 0.0000083),
        ];
        let value = |asset: &str, amount: Decimal| match asset {
            "USDT" => Some(amount),
            "BNB" => Some(amount * dec!(700)),
            _ => None,
        };
        let memory = TradeMemory::from_rows(
            &AuditRows { rows: rows.clone(), error: None },
            &BTreeMap::new(),
            value,
            8,
        );
        let records: Vec<_> = rows.iter().map(route_record).collect();
        let mark = |asset: &str| (asset == "BNB").then_some(Mark { free: dec!(0.01), price_usdt: dec!(700) });
        let dashboard = build_trades(&records, mark, bb_core::time::now());

        let aave = &memory.recent_outcomes[0];
        assert_eq!(aave.status, "closed");
        assert_eq!(aave.entry_value_usdt, Some(dec!(7.93728)));
        let (pnl, cost) = (dashboard[0].pnl_usdt.unwrap(), dashboard[0].cost_usdt.unwrap());
        // 7.98668 - 7.93728 - 2 x 0.0000083 x 700 = 0.03778.
        assert_eq!(pnl, dec!(0.03778));
        assert!((aave.pnl_bps.unwrap() - to_f64(pnl / cost * BPS_PER_UNIT)).abs() < 1e-9);
        assert_eq!(memory.recent_outcomes[1].status, "cash");
    }

    #[test]
    fn trade_memory_records_current_holding_pnl() {
        let mut row = submitted_row("USDT", "ETH", 100.0, 100.5);
        row["selected_route"]["input_amount"] = json!(100.5);
        row["selected_route"]["output_amount"] = json!(0.035);
        let rows = AuditRows { rows: vec![row], error: None };
        let free = BTreeMap::from([("ETH".to_owned(), dec!(0.035))]);
        let memory = TradeMemory::from_rows(&rows, &free, value, 8);
        assert_eq!(memory.status, "ready");
        assert_eq!(memory.recent_trade_count, 1);
        let holding = &memory.current_holdings[""];
        assert_eq!(holding.status, "open");
        assert_eq!(holding.entry_value_usdt, Some(dec!(100.5)));
        assert_eq!(holding.current_value_usdt, Some(dec!(105.000)));
        // 0.035 x 3000 = 105 against the 100.5 spent.
        assert!((holding.unrealized_pnl_bps.unwrap() - 4.5 / 100.5 * 10_000.0).abs() < 1e-9);
        assert_eq!(memory.recent_outcomes[0].status, "open");
    }

    #[test]
    fn closed_outcomes_and_loss_streak() {
        let rows = AuditRows {
            rows: vec![
                submitted_row("USDT", "ETH", 100.0, 100.0),
                json!({"status": "held", "decision": {"action": "hold"}}),
                {
                    let mut sell = submitted_row("ETH", "USDT", 98.0, 95.0);
                    sell["cycle_id"] = json!("exit-plan");
                    sell["decision"]["reason"] =
                        json!("exit plan atr=3:1h,stretch=2:1 fired (stretch, live)");
                    sell
                },
                submitted_row("USDT", "BTC", 97.0, 99.0),
            ],
            error: None,
        };
        let memory = TradeMemory::from_rows(&rows, &BTreeMap::new(), value, 8);
        let outcomes = &memory.recent_outcomes;
        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0].status, "closed");
        assert!((outcomes[0].pnl_bps.unwrap() - -500.0).abs() < 1e-9);
        // Selling into USDT is parking: no P&L from the next route's whole balance.
        assert_eq!(outcomes[1].status, "cash");
        assert_eq!(outcomes[1].pnl_bps, None);
        assert_eq!(outcomes[2].status, "unknown");
        // An old exit-plan sell (Phase 45, removed) counts as an ordinary sell.
        assert_eq!(memory.recent_loss_streak, 0);
        assert_eq!(
            memory.current_holdings[""].status,
            "not_current",
            "BTC not held any more"
        );

        // Parking in cash between two losers does not break the streak.
        let losing = vec![
            TradeOutcome {
                pnl_bps: Some(-1.0),
                ..outcomes[0].clone()
            },
            outcomes[1].clone(),
            TradeOutcome {
                pnl_bps: Some(-2.0),
                ..outcomes[0].clone()
            },
        ];
        assert_eq!(loss_streak(&losing), 2);

        // The wallet parked in USDT is cash, not an open position with a return.
        let parked = AuditRows {
            rows: vec![
                submitted_row("USDT", "ETH", 100.0, 100.0),
                submitted_row("ETH", "USDT", 7.0, 7.1),
            ],
            error: None,
        };
        let free = BTreeMap::from([("USDT".to_owned(), dec!(15.25))]);
        let holding = TradeMemory::from_rows(&parked, &free, value, 8)
            .current_holdings
            .remove("")
            .unwrap();
        assert_eq!(
            (holding.status, holding.current_value_usdt, holding.unrealized_pnl_bps),
            ("cash", Some(dec!(15.25)), None)
        );
    }

    #[test]
    fn each_sleeve_has_its_own_current_holding_in_a_shared_asset() {
        let in_sleeve = |sleeve: &str, from: &str, to: &str, spent: f64, got: f64, qty: f64| {
            let mut row = submitted_row(from, to, spent, got);
            row["sleeve"] = json!(sleeve);
            row["selected_route"]["output_amount"] = json!(qty);
            row
        };
        // fast buys 0.02 ETH, core 0.01 ETH, then core sells its own.
        let rows = AuditRows {
            rows: vec![
                in_sleeve("fast", "USDT", "ETH", 60.0, 60.0, 0.02),
                in_sleeve("core", "USDT", "ETH", 30.0, 30.0, 0.01),
                in_sleeve("core", "ETH", "USDT", 33.0, 33.0, 33.0),
            ],
            error: None,
        };
        let free = BTreeMap::from([("ETH".to_owned(), dec!(0.02)), ("USDT".to_owned(), dec!(33))]);
        let memory = TradeMemory::from_rows(&rows, &free, value, 8);
        let fast = &memory.current_holdings["fast"];
        assert_eq!((fast.status, fast.asset.as_deref(), fast.amount), ("open", Some("ETH"), Some(dec!(0.02))));
        assert_eq!(memory.current_holdings["core"].status, "cash");
        let sleeves: Vec<&str> = memory.recent_outcomes.iter().map(|o| o.sleeve.as_str()).collect();
        assert_eq!(sleeves, ["fast", "core", "core"]);
        // core's ETH trade closed at its own sale (+3), fast's is still open.
        assert_eq!((memory.recent_outcomes[1].status, memory.recent_outcomes[0].status), ("closed", "open"));
    }

    #[test]
    fn unreadable_and_empty_history() {
        let unavailable = TradeMemory::from_rows(
            &AuditRows {
                rows: Vec::new(),
                error: Some("denied".into()),
            },
            &BTreeMap::new(),
            value,
            8,
        );
        assert_eq!(unavailable.status, "unavailable");
        let empty = TradeMemory::from_rows(&AuditRows::default(), &BTreeMap::new(), value, 8);
        assert_eq!(empty.status, "empty");
    }

    #[test]
    fn load_audit_rows_skips_garbage_and_keeps_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut text = String::new();
        for index in 0..(AUDIT_TAIL_LINES + 5) {
            text.push_str(&format!("{{\"n\": {index}}}\n"));
        }
        text.push_str("not json\n[1]\n");
        std::fs::write(&path, text).unwrap();
        let rows = load_audit_rows(&path);
        assert!(rows.error.is_none());
        assert_eq!(rows.rows.len(), AUDIT_TAIL_LINES - 2);
        assert_eq!(load_audit_rows(&dir.path().join("missing")).rows.len(), 0);
    }
}

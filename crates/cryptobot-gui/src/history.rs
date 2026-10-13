//! Claude Spot trade history from the whole audit log.
//!
//! Every submitted route opens one trade in its destination asset. The trade closes at the
//! first later submitted route that sells that asset. Values come from the route quotes the
//! trader logged (`selected_route`), compared per unit so partial sells and repeat buys stay
//! consistent:
//!
//! - cost = `source_value_usdt` of the opening route (what was given up, before fees);
//! - exit = bought quantity × the closing route's `source_value_usdt / input_amount`;
//! - P&L = exit − cost, so each route's fees and spread land on the trade it opened and the
//!   trade P&Ls add up to the whole rotation chain.
//!
//! A trade with no closing route is `open` (marked at the current price) while its asset is
//! still in the wallet, else `unknown`. Trades into stablecoins are `cash`, held or not: their
//! P&L (the cost of converting out) counts toward net P&L but not toward wins and losses.
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use bb_trader::memory::{row_asset, row_attempted_submission, row_route_id, row_status, value_decimal};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

/// Assets treated as cash: a trade into one of them is parking, not a bet.
pub const CASH_ASSETS: &[&str] = &["USDT", "USDC", "FDUSD", "TUSD", "DAI", "USDP", "BUSD"];

/// One submitted route as logged by the trader.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRecord {
    pub timestamp: String,
    pub route_id: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub reason: String,
    /// USDT value of what the route spent (before fees).
    pub source_value_usdt: Option<Decimal>,
    /// USDT value the route expected to receive (after fees).
    pub output_value_usdt: Option<Decimal>,
    pub input_amount: Option<Decimal>,
    pub output_amount: Option<Decimal>,
    pub legs: usize,
}

/// A route that failed after reaching the exchange.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailedRoute {
    pub timestamp: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub error: String,
}

/// The audit log reduced to what the history needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedAudit {
    pub routes: Vec<RouteRecord>,
    pub failed: Vec<FailedRoute>,
    pub error: Option<String>,
}

/// Stream the whole audit file (a missing file is an empty history; torn lines are skipped).
pub fn parse_audit(path: &Path) -> ParsedAudit {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ParsedAudit::default(),
        Err(error) => {
            return ParsedAudit {
                error: Some(format!("cannot read {}: {error}", path.display())),
                ..ParsedAudit::default()
            };
        }
    };
    let mut parsed = ParsedAudit::default();
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                parsed.error = Some(format!("cannot read {}: {error}", path.display()));
                break;
            }
        };
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if !row_attempted_submission(&row) {
            continue;
        }
        match row_status(&row).as_str() {
            "submitted" => parsed.routes.push(route_record(&row)),
            "failed" => parsed.failed.push(FailedRoute {
                timestamp: text(&row, "/timestamp"),
                source_asset: row_asset(&row, "source_asset"),
                destination_asset: row_asset(&row, "destination_asset"),
                error: text(&row, "/error"),
            }),
            _ => {}
        }
    }
    parsed
}

fn text(row: &Value, pointer: &str) -> String {
    row.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn route_record(row: &Value) -> RouteRecord {
    let selected = |field: &str| value_decimal(row.pointer(&format!("/selected_route/{field}")));
    RouteRecord {
        timestamp: text(row, "/timestamp"),
        route_id: row_route_id(row),
        source_asset: row_asset(row, "source_asset"),
        destination_asset: row_asset(row, "destination_asset"),
        reason: text(row, "/decision/reason"),
        source_value_usdt: selected("source_value_usdt"),
        output_value_usdt: selected("estimated_output_usdt"),
        input_amount: selected("input_amount"),
        output_amount: selected("output_amount"),
        legs: row
            .get("submitted_legs")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
    }
}

/// How a trade ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeStatus {
    Win,
    Loss,
    /// Into a stablecoin.
    Cash,
    /// Still held; P&L is unrealized.
    Open,
    /// Never sold by Claude and no longer in the wallet (or not valued).
    Unknown,
}

/// One trade (one submitted route) with its result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trade {
    pub opened_at: String,
    pub closed_at: Option<String>,
    pub route_id: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub reason: String,
    pub legs: usize,
    pub status: TradeStatus,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub cost_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub exit_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub pnl_usdt: Option<Decimal>,
    pub pnl_pct: Option<f64>,
    /// Fees, spread and slippage the opening route expected to pay.
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub fees_usdt: Option<Decimal>,
    pub hold_seconds: Option<i64>,
}

/// Current holding used to mark open trades: free quantity and USDT price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mark {
    pub free: Decimal,
    pub price_usdt: Decimal,
}

/// Trades, oldest first. `mark(asset)` returns the asset's current holding, if any.
pub fn build_trades(
    routes: &[RouteRecord],
    mark: impl Fn(&str) -> Option<Mark>,
    now: DateTime<Utc>,
) -> Vec<Trade> {
    routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let destination = route.destination_asset.as_str();
            let cost = route.source_value_usdt;
            let closing = (!destination.is_empty())
                .then(|| {
                    routes[index + 1..]
                        .iter()
                        .find(|later| later.source_asset == destination)
                })
                .flatten();
            let (exit, closed_at, marked) = match closing {
                Some(later) => (exit_value(route, later), Some(later.timestamp.clone()), false),
                None => match mark(destination).filter(|m| m.free > Decimal::ZERO) {
                    Some(m) => (route.output_amount.map(|qty| qty * m.price_usdt), None, true),
                    None => (None, None, false),
                },
            };
            let pnl = exit.zip(cost).map(|(exit, cost)| exit - cost);
            let status = match pnl {
                None => TradeStatus::Unknown,
                Some(_) if CASH_ASSETS.contains(&destination) => TradeStatus::Cash,
                Some(_) if marked => TradeStatus::Open,
                Some(pnl) if pnl > Decimal::ZERO => TradeStatus::Win,
                Some(_) => TradeStatus::Loss,
            };
            let opened = parse_time(&route.timestamp);
            let ended = match &closed_at {
                Some(at) => parse_time(at),
                None if marked => Some(now),
                None => None,
            };
            Trade {
                opened_at: route.timestamp.clone(),
                closed_at,
                route_id: route.route_id.clone(),
                source_asset: route.source_asset.clone(),
                destination_asset: route.destination_asset.clone(),
                reason: route.reason.clone(),
                legs: route.legs,
                status,
                cost_usdt: cost,
                exit_usdt: exit,
                pnl_usdt: pnl,
                pnl_pct: pnl.zip(cost).and_then(|(pnl, cost)| pct(pnl, cost)),
                fees_usdt: cost.zip(route.output_value_usdt).map(|(cost, out)| cost - out),
                hold_seconds: opened
                    .zip(ended)
                    .map(|(opened, ended)| (ended - opened).num_seconds()),
            }
        })
        .collect()
}

/// USDT value of `route`'s bought quantity at the price `later` sold it for. Falls back to the
/// whole sale value when quantities are missing.
fn exit_value(route: &RouteRecord, later: &RouteRecord) -> Option<Decimal> {
    let sold_value = later.source_value_usdt?;
    match (route.output_amount, later.input_amount) {
        (Some(bought), Some(sold)) if sold > Decimal::ZERO => Some(bought * sold_value / sold),
        _ => Some(sold_value),
    }
}

fn pct(pnl: Decimal, cost: Decimal) -> Option<f64> {
    (cost > Decimal::ZERO).then(|| bb_core::money::to_f64(pnl / cost * Decimal::ONE_HUNDRED))
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;

    fn route(at: &str, from: &str, to: &str, spent: Decimal, got: Decimal, input: Decimal, output: Decimal) -> RouteRecord {
        RouteRecord {
            timestamp: at.into(),
            route_id: format!("{from}>{to}"),
            source_asset: from.into(),
            destination_asset: to.into(),
            reason: String::new(),
            source_value_usdt: Some(spent),
            output_value_usdt: Some(got),
            input_amount: Some(input),
            output_amount: Some(output),
            legs: 1,
        }
    }

    fn now() -> DateTime<Utc> {
        parse_time("2026-10-13T12:00:00+00:00").unwrap()
    }

    #[test]
    fn rotation_chain_closes_each_trade_at_the_next_sale_and_sums_to_the_chain() {
        // 100 USDT -> 1 SOL (fees 1), SOL sold for 110 -> 2 ETH, ETH sold for 90 -> USDT.
        let routes = [
            route("2026-10-13T00:00:00Z", "USDT", "SOL", dec!(100), dec!(99), dec!(100), dec!(1)),
            route("2026-10-13T01:00:00Z", "SOL", "ETH", dec!(110), dec!(108), dec!(1), dec!(2)),
            route("2026-10-13T03:00:00Z", "ETH", "USDT", dec!(90), dec!(89), dec!(2), dec!(89)),
        ];
        let trades = build_trades(&routes, |_| None, now());

        assert_eq!(trades[0].status, TradeStatus::Win);
        assert_eq!(trades[0].pnl_usdt, Some(dec!(10)));
        assert_eq!(trades[0].fees_usdt, Some(dec!(1)));
        assert_eq!(trades[0].hold_seconds, Some(3_600));
        assert_eq!(trades[1].status, TradeStatus::Loss);
        assert_eq!(trades[1].pnl_usdt, Some(dec!(-20)));
        // Not held any more and never sold: no result.
        assert_eq!(trades[2].status, TradeStatus::Unknown);
        assert_eq!(trades[2].pnl_usdt, None);
    }

    #[test]
    fn partial_sale_values_the_whole_position_at_the_sale_price() {
        // 2 SOL bought for 200; 0.5 SOL later sold for 60 (120/SOL) -> position worth 240.
        let routes = [
            route("2026-10-13T00:00:00Z", "USDT", "SOL", dec!(200), dec!(198), dec!(200), dec!(2)),
            route("2026-10-13T02:00:00Z", "SOL", "BTC", dec!(60), dec!(59), dec!(0.5), dec!(0.001)),
        ];
        let trades = build_trades(&routes, |_| None, now());
        assert_eq!(trades[0].exit_usdt, Some(dec!(240)));
        assert_eq!(trades[0].pnl_usdt, Some(dec!(40)));
        assert_eq!(trades[0].pnl_pct, Some(20.0));
    }

    #[test]
    fn held_position_is_open_and_marked_while_cash_parking_is_not_a_win_or_loss() {
        let routes = [
            route("2026-10-13T00:00:00Z", "SOL", "USDT", dec!(50), dec!(49.5), dec!(0.5), dec!(49.5)),
            route("2026-10-13T06:00:00Z", "USDT", "ETH", dec!(49.5), dec!(49), dec!(49.5), dec!(0.02)),
        ];
        let mark = |asset: &str| (asset == "ETH").then_some(Mark { free: dec!(0.02), price_usdt: dec!(2600) });
        let trades = build_trades(&routes, mark, now());

        // USDT bought for 50 and spent at 1 USDT each: the 0.5 conversion cost.
        assert_eq!(trades[0].status, TradeStatus::Cash);
        assert_eq!(trades[0].pnl_usdt, Some(dec!(-0.5)));
        assert_eq!(trades[1].status, TradeStatus::Open);
        assert_eq!(trades[1].exit_usdt, Some(dec!(52)));
        assert_eq!(trades[1].pnl_usdt, Some(dec!(2.5)));
        assert_eq!(trades[1].hold_seconds, Some(6 * 3_600));

        // Still parked in USDT: cash, not an open bet.
        let usdt = |asset: &str| (asset == "USDT").then_some(Mark { free: dec!(49.5), price_usdt: Decimal::ONE });
        let parked = build_trades(&routes[..1], usdt, now());
        assert_eq!(parked[0].status, TradeStatus::Cash);
        assert_eq!(parked[0].pnl_usdt, Some(dec!(-0.5)));
    }

    #[test]
    fn parse_keeps_submitted_and_failed_routes_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let rows = [
            json!({"timestamp": "t1", "status": "submitted", "submission_attempted": true,
                   "decision": {"source_asset": "usdt", "destination_asset": "sol", "reason": "why"},
                   "selected_route": {"source_value_usdt": 10, "estimated_output_usdt": "9.9",
                                      "input_amount": 10, "output_amount": 0.05},
                   "submitted_legs": [{}]}),
            json!({"timestamp": "t2", "status": "submitted", "row_type": "cycle_summary"}),
            json!({"timestamp": "t3", "status": "held"}),
            json!({"timestamp": "t4", "status": "failed", "submission_attempted": true, "error": "boom",
                   "decision": {"source_asset": "SOL", "destination_asset": "ETH"}}),
            json!({"timestamp": "t5", "status": "failed", "error": "network, nothing sent"}),
        ];
        let mut text: String = rows.iter().map(|row| format!("{row}\n")).collect();
        text.push_str("{torn");
        std::fs::write(&path, text).unwrap();

        let parsed = parse_audit(&path);

        assert_eq!(parsed.error, None);
        assert_eq!(parsed.routes.len(), 1);
        let first = &parsed.routes[0];
        assert_eq!((first.source_asset.as_str(), first.destination_asset.as_str()), ("USDT", "SOL"));
        assert_eq!(first.output_value_usdt, Some(dec!(9.9)));
        assert_eq!(first.legs, 1);
        assert_eq!(parsed.failed.len(), 1);
        assert_eq!(parsed.failed[0].error, "boom");
        assert_eq!(parse_audit(&dir.path().join("missing")), ParsedAudit::default());
    }
}

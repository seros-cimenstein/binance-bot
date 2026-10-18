//! Bot Spot trade ledger from the audit log, shared by the trader (trade memory, its record)
//! and the dashboard (`cryptobot-gui` History, Sessions).
//!
//! Every submitted route opens one trade in its destination asset. The trade closes at the
//! first later submitted route that sells that asset. Amounts come from the orders the
//! exchange filled (`submitted_legs[].final_order`, else `order`), compared per unit so
//! partial sells and repeat buys stay consistent:
//!
//! - cost = what the fills paid for the bought quantity, in USDT;
//! - exit = bought quantity × the closing route's fill price for that asset;
//! - fees = the exchange fees of the order that bought it and the closing order's share;
//! - P&L = exit − cost − fees.
//!
//! Only USDT-quoted legs carry a USDT value, so a route with another quote (or without fill
//! data) falls back to the trader's quote (`selected_route`): cost = `source_value_usdt`,
//! exit per unit = `source_value_usdt / input_amount`, fees and spread inside both. The
//! quote is what the trader planned to spend; the order is sized below it (fee and rounding
//! reserve), so quotes overstate the cost whenever fills exist.
//!
//! A trade with no closing route is `open` (marked at the current price) while its asset is
//! still in the wallet, else `unknown`. Trades into stablecoins are `cash`, held or not: their
//! P&L counts toward net P&L but not toward wins and losses (with fills it is zero, since every
//! fee belongs to the coin bought or sold). `closed_by_plan` marks a trade whose closing route
//! was a Phase 45 exit-plan sell. Fees in BNB or another coin are valued at that coin's
//! current wallet price (approximate; a coin not in the wallet leaves its fees out).
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

use crate::cycle::EXIT_CYCLE_ID;
use crate::memory::{row_asset, row_route_id, value_decimal};

/// Assets treated as cash: a trade into one of them is parking, not a bet.
pub const CASH_ASSETS: &[&str] = &["USDT", "USDC", "FDUSD", "TUSD", "DAI", "USDP", "BUSD"];
/// The quote every fill is valued in.
const USDT: &str = "USDT";

/// Where a route's amounts come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Valuation {
    /// The exchange's filled orders.
    Fills,
    /// The trader's pre-trade quote (no usable fill data).
    Quotes,
}

/// One exchange fee as logged with its order.
#[derive(Debug, Clone, PartialEq)]
pub struct Fee {
    pub amount: Decimal,
    pub currency: String,
    /// USDT value when the fill itself prices it (a USDT fee, or one in the traded coin).
    pub usdt: Option<Decimal>,
}

/// One submitted route as logged by the trader.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRecord {
    pub timestamp: String,
    /// The cycle that placed it (`exit-plan` for plan sells; empty in older rows).
    pub cycle_id: String,
    pub route_id: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub reason: String,
    pub legs: usize,
    /// Sold by an exit plan between cycles (`cycle_id` [`EXIT_CYCLE_ID`]), not by the bot.
    pub exit_plan: bool,
    pub valuation: Valuation,
    /// Source quantity given up and the USDT it fetched (fills: before fees).
    pub sold_qty: Option<Decimal>,
    pub sold_usdt: Option<Decimal>,
    /// Destination quantity received (after fees taken in it) and its USDT cost (fills: before
    /// the fees in `open_fees`; quotes: fees inside).
    pub bought_qty: Option<Decimal>,
    pub basis_usdt: Option<Decimal>,
    /// Fills: fees of the orders that sold the source (they close its trade).
    pub close_fees: Vec<Fee>,
    /// Fills: fees of the orders that bought the destination (they open its trade).
    pub open_fees: Vec<Fee>,
    /// Quotes: fees, spread and slippage the route expected (already inside the amounts).
    pub quoted_cost_usdt: Option<Decimal>,
}

pub(crate) fn text(row: &Value, pointer: &str) -> String {
    row.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// One submitted route row as a [`RouteRecord`]: fill values when every leg is USDT-quoted
/// and filled, else the trader's quote.
pub fn route_record(row: &Value) -> RouteRecord {
    let source_asset = row_asset(row, "source_asset");
    let destination_asset = row_asset(row, "destination_asset");
    let legs = row
        .get("submitted_legs")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut record = RouteRecord {
        timestamp: text(row, "/timestamp"),
        cycle_id: text(row, "/cycle_id"),
        route_id: row_route_id(row),
        reason: text(row, "/decision/reason"),
        legs,
        exit_plan: row.get("cycle_id").and_then(Value::as_str) == Some(EXIT_CYCLE_ID),
        valuation: Valuation::Quotes,
        sold_qty: None,
        sold_usdt: None,
        bought_qty: None,
        basis_usdt: None,
        close_fees: Vec::new(),
        open_fees: Vec::new(),
        quoted_cost_usdt: None,
        source_asset,
        destination_asset,
    };
    match fill_values(row, &record.source_asset, &record.destination_asset) {
        Some(fills) => {
            record.valuation = Valuation::Fills;
            record.sold_qty = Some(fills.sold_qty);
            record.sold_usdt = Some(fills.sold_usdt);
            record.bought_qty = Some(fills.bought_qty);
            record.basis_usdt = Some(fills.basis_usdt);
            record.close_fees = fills.close_fees;
            record.open_fees = fills.open_fees;
        }
        None => {
            let selected = |field: &str| value_decimal(row.pointer(&format!("/selected_route/{field}")));
            let spent = selected("source_value_usdt");
            record.sold_qty = selected("input_amount");
            record.sold_usdt = spent;
            record.bought_qty = selected("output_amount");
            record.basis_usdt = spent;
            record.quoted_cost_usdt = spent
                .zip(selected("estimated_output_usdt"))
                .map(|(spent, out)| spent - out);
        }
    }
    record
}

/// One filled order of a route.
struct Leg {
    base: String,
    quote: String,
    buy: bool,
    filled: Decimal,
    /// Quote amount of the fill.
    cost: Decimal,
    fees: Vec<(Decimal, String)>,
}

impl Leg {
    /// The leg's fees valued where possible, leaving out those charged in `skip`.
    fn fees(&self, skip: Option<&str>) -> impl Iterator<Item = Fee> + '_ {
        let skip = skip.map(str::to_owned);
        self.fees
            .iter()
            .filter(move |(_, c)| skip.as_deref() != Some(c.as_str()))
            .map(|(amount, c)| Fee {
                amount: *amount,
                currency: c.clone(),
                usdt: if c == USDT {
                    Some(*amount)
                } else if *c == self.base {
                    Some(*amount * self.cost / self.filled)
                } else {
                    None
                },
            })
    }

    fn fees_in(&self, currency: &str) -> Decimal {
        self.fees.iter().filter(|(_, c)| c == currency).map(|(a, _)| *a).sum()
    }
}

fn legs(row: &Value) -> Option<Vec<Leg>> {
    let rows = row.get("submitted_legs")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    rows.iter()
        .map(|leg| {
            let order = leg
                .get("final_order")
                .filter(|o| o.is_object())
                .or_else(|| leg.get("order"))?;
            let (base, quote) = leg.get("symbol")?.as_str()?.split_once('/')?;
            let filled = value_decimal(order.get("filled")).filter(|v| *v > Decimal::ZERO)?;
            let cost = value_decimal(order.get("cost")).filter(|v| *v > Decimal::ZERO)?;
            let fees = order
                .get("fees")
                .and_then(Value::as_array)
                .map(|fees| {
                    fees.iter()
                        .filter_map(|f| {
                            let amount = value_decimal(f.get("cost"))?;
                            let currency = f.get("currency")?.as_str()?.to_uppercase();
                            Some((amount, currency))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Leg {
                base: base.to_uppercase(),
                quote: quote.to_uppercase(),
                buy: leg.get("side").and_then(Value::as_str) == Some("buy"),
                filled,
                cost,
                fees,
            })
        })
        .collect()
}

struct FillValues {
    sold_qty: Decimal,
    sold_usdt: Decimal,
    bought_qty: Decimal,
    basis_usdt: Decimal,
    close_fees: Vec<Fee>,
    open_fees: Vec<Fee>,
}

/// Route amounts from its fills; `None` without fill data or with a non-USDT quote.
fn fill_values(row: &Value, source: &str, destination: &str) -> Option<FillValues> {
    let legs = legs(row)?;
    if legs.iter().any(|leg| leg.quote != USDT) {
        return None;
    }
    let (sold_qty, sold_usdt, close_fees) = if source == USDT {
        let spent: Decimal = legs.iter().filter(|l| l.buy).map(|l| l.cost).sum();
        (spent, spent, Vec::new())
    } else {
        let sells: Vec<&Leg> = legs.iter().filter(|l| !l.buy && l.base == source).collect();
        (
            sells.iter().map(|l| l.filled).sum(),
            sells.iter().map(|l| l.cost).sum(),
            sells.iter().flat_map(|l| l.fees(None)).collect(),
        )
    };
    let (bought_qty, basis_usdt, open_fees) = if destination == USDT {
        // USDT fees come off the proceeds; every fee here closes the coin's trade above.
        let got: Decimal = legs
            .iter()
            .filter(|l| !l.buy)
            .map(|l| l.cost - l.fees_in(USDT))
            .sum();
        (got, got, Vec::new())
    } else {
        let buys: Vec<&Leg> = legs.iter().filter(|l| l.buy && l.base == destination).collect();
        (
            // A fee taken in the bought coin shrinks what arrived instead of adding a cost.
            buys.iter().map(|l| l.filled - l.fees_in(destination)).sum(),
            buys.iter().map(|l| l.cost).sum(),
            buys.iter().flat_map(|l| l.fees(Some(destination))).collect(),
        )
    };
    (sold_qty > Decimal::ZERO && bought_qty > Decimal::ZERO).then_some(FillValues {
        sold_qty,
        sold_usdt,
        bought_qty,
        basis_usdt,
        close_fees,
        open_fees,
    })
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
    /// Never sold by the bot and no longer in the wallet (or not valued).
    Unknown,
}

/// One trade (one submitted route) with its result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trade {
    pub opened_at: String,
    pub closed_at: Option<String>,
    /// What the coin was sold into (the closing route's destination, usually USDT).
    pub closed_into: Option<String>,
    /// The cycle that opened it.
    pub cycle_id: String,
    pub route_id: String,
    pub source_asset: String,
    pub destination_asset: String,
    pub reason: String,
    pub legs: usize,
    /// The closing route was an exit-plan sell.
    pub closed_by_plan: bool,
    /// The opening route's amounts: the exchange's fills or the trader's quote.
    pub valuation: Valuation,
    pub status: TradeStatus,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub cost_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub exit_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub pnl_usdt: Option<Decimal>,
    pub pnl_pct: Option<f64>,
    /// Fills: exchange fees of the buy and the closing sell (subtracted in `pnl_usdt`).
    /// Quotes: fees, spread and slippage the opening quote expected (inside `pnl_usdt`).
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub fees_usdt: Option<Decimal>,
    pub hold_seconds: Option<i64>,
}

/// Current holding used to mark open trades and to value fees: free quantity and USDT price.
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
    let value = |fees: &[Fee]| -> Decimal {
        fees.iter()
            .filter_map(|f| f.usdt.or_else(|| mark(&f.currency).map(|m| f.amount * m.price_usdt)))
            .sum()
    };
    routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let destination = route.destination_asset.as_str();
            let qty = route.bought_qty;
            let closing = (!destination.is_empty())
                .then(|| {
                    routes[index + 1..]
                        .iter()
                        .find(|later| later.source_asset == destination)
                })
                .flatten();
            let (exit, closed_at, marked) = match closing {
                Some(later) => (exit_value(qty, later), Some(later.timestamp.clone()), false),
                None => match mark(destination).filter(|m| m.free > Decimal::ZERO) {
                    Some(m) => (qty.map(|qty| qty * m.price_usdt), None, true),
                    None => (None, None, false),
                },
            };
            let fees = match route.valuation {
                Valuation::Fills => {
                    let close = closing
                        .filter(|later| later.valuation == Valuation::Fills)
                        .and_then(|later| {
                            let share = qty.zip(later.sold_qty).map(|(qty, sold)| (qty / sold).min(Decimal::ONE))?;
                            Some(value(&later.close_fees) * share)
                        })
                        .unwrap_or_default();
                    Some(value(&route.open_fees) + close)
                }
                Valuation::Quotes => route.quoted_cost_usdt,
            };
            let charged = match route.valuation {
                Valuation::Fills => fees.unwrap_or_default(),
                Valuation::Quotes => Decimal::ZERO,
            };
            let cost = route.basis_usdt;
            let pnl = exit.zip(cost).map(|(exit, cost)| exit - cost - charged);
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
                closed_into: closing.map(|later| later.destination_asset.clone()),
                cycle_id: route.cycle_id.clone(),
                route_id: route.route_id.clone(),
                source_asset: route.source_asset.clone(),
                destination_asset: route.destination_asset.clone(),
                reason: route.reason.clone(),
                legs: route.legs,
                closed_by_plan: closing.is_some_and(|later| later.exit_plan),
                valuation: route.valuation,
                status,
                cost_usdt: cost,
                exit_usdt: exit,
                pnl_usdt: pnl,
                pnl_pct: pnl.zip(cost).and_then(|(pnl, cost)| pct(pnl, cost)),
                fees_usdt: fees,
                hold_seconds: opened
                    .zip(ended)
                    .map(|(opened, ended)| (ended - opened).num_seconds()),
            }
        })
        .collect()
}

/// USDT value of `qty` at the price `later` sold the asset for. Falls back to the whole sale
/// value when quantities are missing.
fn exit_value(qty: Option<Decimal>, later: &RouteRecord) -> Option<Decimal> {
    let sold_value = later.sold_usdt?;
    match (qty, later.sold_qty) {
        (Some(bought), Some(sold)) if sold > Decimal::ZERO => Some(bought * sold_value / sold),
        _ => Some(sold_value),
    }
}

pub fn pct(pnl: Decimal, cost: Decimal) -> Option<f64> {
    (cost > Decimal::ZERO).then(|| bb_core::money::to_f64(pnl / cost * Decimal::ONE_HUNDRED))
}

pub fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The bot's record over closed coin trades (wins and losses; cash parking excluded).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RecordWindow {
    pub trips: usize,
    pub wins: usize,
    /// Realized P&L after the buy and sell fees.
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub net_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub fees_usdt: Decimal,
    pub mean_hold_minutes: Option<f64>,
}

/// [`RecordWindow`] of the trades closed at or after `since` (all of them when `None`).
pub fn record_window(trades: &[Trade], since: Option<DateTime<Utc>>) -> RecordWindow {
    let closed: Vec<&Trade> = trades
        .iter()
        .filter(|t| matches!(t.status, TradeStatus::Win | TradeStatus::Loss))
        .filter(|t| {
            since.is_none_or(|since| t.closed_at.as_deref().and_then(parse_time).is_some_and(|at| at >= since))
        })
        .collect();
    let holds: Vec<i64> = closed.iter().filter_map(|t| t.hold_seconds).collect();
    RecordWindow {
        trips: closed.len(),
        wins: closed.iter().filter(|t| t.status == TradeStatus::Win).count(),
        net_usdt: closed.iter().filter_map(|t| t.pnl_usdt).sum(),
        fees_usdt: closed.iter().filter_map(|t| t.fees_usdt).sum(),
        mean_hold_minutes: (!holds.is_empty())
            .then(|| holds.iter().sum::<i64>() as f64 / holds.len() as f64 / 60.0),
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;

    /// A quote-valued route (no fill data), as older or non-USDT routes are.
    fn route(at: &str, from: &str, to: &str, spent: Decimal, got: Decimal, input: Decimal, output: Decimal) -> RouteRecord {
        RouteRecord {
            timestamp: at.into(),
            cycle_id: String::new(),
            route_id: format!("{from}>{to}"),
            source_asset: from.into(),
            destination_asset: to.into(),
            reason: String::new(),
            legs: 1,
            exit_plan: false,
            valuation: Valuation::Quotes,
            sold_qty: Some(input),
            sold_usdt: Some(spent),
            bought_qty: Some(output),
            basis_usdt: Some(spent),
            close_fees: Vec::new(),
            open_fees: Vec::new(),
            quoted_cost_usdt: Some(spent - got),
        }
    }

    /// An audit row with one filled market order against USDT (a buy from USDT, else a sell),
    /// fee in BNB.
    fn filled_row(at: &str, from: &str, to: &str, filled: &str, cost: &str, quote_spend: f64, bnb_fee: f64) -> Value {
        let (coin, side) = if from == USDT { (to, "buy") } else { (from, "sell") };
        json!({"timestamp": at, "status": "submitted", "submission_attempted": true, "row_type": "route",
               "decision": {"source_asset": from, "destination_asset": to, "reason": "r"},
               "selected_route": {"source_value_usdt": quote_spend, "estimated_output_usdt": quote_spend * 0.99,
                                  "input_amount": quote_spend, "output_amount": 1},
               "submitted_legs": [{"symbol": format!("{coin}/USDT"), "side": side,
                   "order": {"filled": filled, "cost": cost, "fees": [{"cost": bnb_fee, "currency": "BNB"}]}}]})
    }

    fn now() -> DateTime<Utc> {
        parse_time("2026-10-13T12:00:00+00:00").unwrap()
    }

    /// BNB (for fees) at `price`, and the parked USDT still in the wallet.
    fn bnb_at(price: Decimal) -> impl Fn(&str) -> Option<Mark> {
        move |asset: &str| match asset {
            "BNB" => Some(Mark { free: dec!(0.01), price_usdt: price }),
            "USDT" => Some(Mark { free: dec!(9.95), price_usdt: Decimal::ONE }),
            _ => None,
        }
    }

    #[test]
    fn filled_round_trip_uses_the_fills_not_the_planned_spend_and_subtracts_both_fees() {
        // The bot planned 10 USDT; the order filled 53.07 ENA for 9.934704 (reserve kept back).
        // The exit plan sold all 53.07 for 9.950625. Each order paid 0.00001 BNB at 700.
        let rows = [
            filled_row("2026-10-15T11:56:06Z", "USDT", "ENA", "53.07", "9.934704", 10.0, 0.00001),
            filled_row("2026-10-15T12:15:54Z", "ENA", "USDT", "53.07", "9.950625", 9.92, 0.00001),
        ];
        let routes: Vec<RouteRecord> = rows.iter().map(route_record).collect();
        assert_eq!(routes[0].valuation, Valuation::Fills);
        let trades = build_trades(&routes, bnb_at(dec!(700)), now());

        let ena = &trades[0];
        assert_eq!(ena.cost_usdt, Some(dec!(9.934704)));
        assert_eq!(ena.exit_usdt, Some(dec!(9.950625)));
        assert_eq!(ena.fees_usdt, Some(dec!(0.014)));
        // +0.015921 before fees, +0.001921 after.
        assert_eq!(ena.pnl_usdt, Some(dec!(0.001921)));
        assert_eq!(ena.status, TradeStatus::Win);
        // Parking the proceeds costs nothing: every fee belongs to the coin trade.
        assert_eq!(trades[1].status, TradeStatus::Cash);
        assert_eq!(trades[1].cost_usdt, Some(dec!(9.950625)));
        assert_eq!(trades[1].pnl_usdt, Some(Decimal::ZERO));

        // The same round trip is a loss once the fees outweigh the move.
        let pricey = build_trades(&routes, bnb_at(dec!(900)), now());
        assert_eq!(pricey[0].pnl_usdt, Some(dec!(-0.002079)));
        assert_eq!(pricey[0].status, TradeStatus::Loss);
    }

    #[test]
    fn partial_filled_sell_charges_only_the_sold_share_of_the_closing_fee() {
        // 2 SOL bought for 200; half sold for 110 (fee 1 USDT): position valued at 220.
        let buy = json!({"timestamp": "2026-10-13T00:00:00Z", "status": "submitted", "submission_attempted": true,
            "decision": {"source_asset": "USDT", "destination_asset": "SOL"},
            "submitted_legs": [{"symbol": "SOL/USDT", "side": "buy",
                "order": {"filled": 2, "cost": 200, "fees": [{"cost": 0.002, "currency": "SOL"}]}}]});
        let sell = json!({"timestamp": "2026-10-13T01:00:00Z", "status": "submitted", "submission_attempted": true,
            "decision": {"source_asset": "SOL", "destination_asset": "USDT"},
            "submitted_legs": [{"symbol": "SOL/USDT", "side": "sell",
                "final_order": {"filled": 1, "cost": 110, "fees": [{"cost": 1, "currency": "USDT"}]}}]});
        let routes = [route_record(&buy), route_record(&sell)];
        // The SOL fee shrank what arrived instead of being charged twice.
        assert_eq!(routes[0].bought_qty, Some(dec!(1.998)));
        assert!(routes[0].open_fees.is_empty());
        // The sell's USDT fee came off the proceeds that were parked.
        assert_eq!(routes[1].bought_qty, Some(dec!(109)));

        let trades = build_trades(&routes, |_| None, now());
        assert_eq!(trades[0].exit_usdt, Some(dec!(219.78)));
        // The whole closing fee: 1.998 SOL held is more than the 1 sold.
        assert_eq!(trades[0].fees_usdt, Some(dec!(1)));
        assert_eq!(trades[0].pnl_usdt, Some(dec!(18.78)));
    }

    #[test]
    fn route_without_usable_fills_falls_back_to_its_quote() {
        let mut row = filled_row("2026-10-13T00:00:00Z", "BTC", "ETH", "1", "0.05", 100.0, 0.0);
        row["submitted_legs"][0]["symbol"] = json!("ETH/BTC");
        row["submitted_legs"][0]["side"] = json!("buy");
        assert_eq!(route_record(&row).valuation, Valuation::Quotes, "BTC-quoted leg has no USDT value");
        row["submitted_legs"] = json!([]);
        let record = route_record(&row);
        assert_eq!(record.valuation, Valuation::Quotes);
        assert_eq!(record.basis_usdt, Some(dec!(100)));
        assert_eq!(record.quoted_cost_usdt, Some(dec!(1)));
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
    fn trade_closed_by_an_exit_plan_sell_is_marked_and_valued_like_any_close() {
        let mut sell = route("2026-10-13T02:00:00Z", "SOL", "USDT", dec!(120), dec!(119), dec!(1), dec!(119));
        sell.exit_plan = true;
        let routes = [
            route("2026-10-13T00:00:00Z", "USDT", "SOL", dec!(100), dec!(99), dec!(100), dec!(1)),
            sell,
        ];
        let trades = build_trades(&routes, |_| None, now());
        assert!(trades[0].closed_by_plan);
        assert_eq!(trades[0].status, TradeStatus::Win);
        assert_eq!(trades[0].pnl_usdt, Some(dec!(20)));
        // The sell itself opens a cash trade that nothing closed yet.
        assert!(!trades[1].closed_by_plan);
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

}

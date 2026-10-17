//! `claude-spot-exit-replay` (Phase 45 step 1): replay the [`crate::exit_plan`] menu on
//! Claude Spot's real round trips.
//!
//! Each round trip from the audit log (a submitted route whose last leg bought a coin on
//! the `--quote` market, closed by the next submitted route that sells that coin) is
//! replayed on public klines from entry to entry + horizon. Every plan's sells are costed
//! with the entry leg's real fee rate plus modelled exit slippage, and compared with
//! Claude's actual exit (its real fill and fee rate) and with the best price seen (an
//! upper bound, never expected return). Research only: public market data, no order path.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bb_binance::history::HistoryFetcher;
use bb_binance::{BinanceClient, BinanceConfig};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

use crate::config::DEFAULT_AUDIT_PATH;
use crate::error::{Result, TraderError};
use crate::exit_plan::{
    ATR_PERIODS, AtrTimeframe, ExitFill, ExitPlan, ExitWatch, PriceBar, average_true_range,
};
use crate::memory::value_decimal;

const MS_PER_MINUTE: i64 = 60_000;
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
/// A sell below this share of the held quantity is flagged as a partial exit.
const FULL_EXIT_SHARE: Decimal = Decimal::from_parts(95, 0, 0, false, 2);
/// Entry fee rate when the audit row does not carry one (Binance VIP0 taker).
const DEFAULT_FEE_RATE: Decimal = Decimal::from_parts(1, 0, 0, false, 3);

/// Plans replayed when no `--plan` is given.
pub const DEFAULT_PLANS: [&str; 24] = [
    "hold",
    "trail=0.5",
    "trail=1",
    "trail=2",
    "trail=3",
    "trail=5",
    "atr=2:1m",
    "atr=3:1m",
    "atr=3:5m",
    "atr=3:15m",
    "atr=3:1h",
    "stretch=2:0.5",
    "stretch=2:1",
    "stretch=1.5:1",
    "slow=0.5:5",
    "slow=1:5",
    "slow=1:3",
    "part=1:0.5,trail=1",
    "trail=0.5,slow=0.5:5",
    "atr=3:15m,stretch=2:0.5",
    "atr=3:1h,slow=0.5:5,stretch=2:0.5",
    "fade=0.5:10",
    "deadline=15",
    "deadline=60",
];

/// Replay the Claude Spot exit-plan menu on real round trips from the audit log.
#[derive(clap::Args, Debug, Clone)]
pub struct ExitReplayArgs {
    /// Claude Spot audit JSONL (default `CLAUDE_SPOT_AUDIT_PATH`).
    #[arg(long)]
    pub audit_path: Option<PathBuf>,
    /// Only round trips entered at or after this ISO time.
    #[arg(long)]
    pub since: Option<String>,
    /// Kline interval for the replay window (`1s`, `1m`, `3m`, `5m`, `15m`).
    #[arg(long, default_value = "1m")]
    pub interval: String,
    /// Minutes after entry each plan may run (cut at now).
    #[arg(long, default_value_t = 1440)]
    pub horizon_minutes: u32,
    /// 1-minute bars before entry that warm up the stretch and slowdown rules.
    #[arg(long, default_value_t = 60)]
    pub history_minutes: u32,
    /// Exit slippage in bps on top of the fee for simulated sells.
    #[arg(long, default_value = "5")]
    pub slippage_bps: Decimal,
    /// Only round trips bought on this quote asset (default: every market).
    #[arg(long)]
    pub quote: Option<String>,
    /// Exit plan spec, repeatable (default: the built-in grid). Example: `part=3:0.5,trail=5`.
    #[arg(long = "plan")]
    pub plans: Vec<String>,
    /// Also write the JSON report here.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Print a Markdown summary table instead of the JSON report.
    #[arg(long)]
    pub table: bool,
}

/// Claude's real exit of a round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualExit {
    pub ts_ms: i64,
    /// Average fill on the entry market; `None` when sold on another market.
    pub price: Option<Decimal>,
    pub partial: bool,
}

/// One real position: bought on `symbol`, closed by Claude (or still open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundTrip {
    pub symbol: String,
    pub asset: String,
    pub entry_ms: i64,
    pub entry_price: Decimal,
    pub quantity: Decimal,
    pub fee_rate: Decimal,
    /// USDT value spent on the position (the route's `source_value_usdt`), for P&L in USDT.
    pub entry_value_usdt: Decimal,
    pub exit: Option<ActualExit>,
}

impl RoundTrip {
    /// Quote spent including the entry fee.
    pub fn entry_cost(&self) -> Decimal {
        self.entry_price * self.quantity * (Decimal::ONE + self.fee_rate)
    }
}

struct Leg {
    symbol: String,
    side: String,
    base: String,
    quote: String,
    price: Decimal,
    filled: Decimal,
    ts_ms: i64,
    fee_rate: Option<Decimal>,
}

fn legs(row: &Value) -> Vec<Leg> {
    let fee_rates: Vec<Option<Decimal>> = row
        .pointer("/selected_route/legs")
        .and_then(Value::as_array)
        .map(|legs| {
            legs.iter()
                .map(|l| value_decimal(l.get("fee_rate")))
                .collect()
        })
        .unwrap_or_default();
    row.get("submitted_legs")
        .and_then(Value::as_array)
        .map(|legs| {
            legs.iter()
                .enumerate()
                .filter_map(|(i, leg)| {
                    let order = leg
                        .get("final_order")
                        .filter(|o| o.is_object())
                        .or_else(|| leg.get("order"))?;
                    let symbol = leg.get("symbol")?.as_str()?.to_uppercase();
                    let (base, quote) = symbol.split_once('/')?;
                    let filled = value_decimal(order.get("filled"))?;
                    let price = value_decimal(order.get("average"))?;
                    (filled > Decimal::ZERO && price > Decimal::ZERO).then(|| Leg {
                        base: base.to_owned(),
                        quote: quote.to_owned(),
                        side: leg
                            .get("side")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_lowercase(),
                        price,
                        filled,
                        ts_ms: order.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                        fee_rate: fee_rates.get(i).copied().flatten(),
                        symbol: symbol.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Round trips (oldest first) from audit rows, plus notes on skipped entries. Route rows
/// are `row_type: "route"` or, in the older schema, rows without a `row_type`. `quote`
/// keeps only entries bought on that quote asset.
pub fn round_trips(
    rows: &[Value],
    quote: Option<&str>,
    since_ms: Option<i64>,
) -> (Vec<RoundTrip>, Vec<String>) {
    let quote = quote.map(str::to_uppercase);
    let mut open: BTreeMap<String, RoundTrip> = BTreeMap::new();
    // Open positions that were partly sold already.
    let mut partly_sold: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut done = Vec::new();
    let mut skipped = Vec::new();
    for row in rows {
        if !matches!(
            row.get("row_type").and_then(Value::as_str),
            None | Some("route")
        ) || row.get("status").and_then(Value::as_str) != Some("submitted")
        {
            continue;
        }
        let legs = legs(row);
        // A sell of most of a held coin closes its round trip; a smaller sell (an exit
        // plan taking part, or Claude scaling out) only shrinks it.
        if let Some(first) = legs.first().filter(|l| l.side == "sell")
            && let Some(mut trip) = open.remove(&first.base)
        {
            if first.filled < trip.quantity * FULL_EXIT_SHARE {
                trip.quantity -= first.filled;
                partly_sold.insert(trip.asset.clone());
                open.insert(trip.asset.clone(), trip);
            } else {
                trip.exit = Some(ActualExit {
                    ts_ms: first.ts_ms,
                    price: (first.symbol == trip.symbol).then_some(first.price),
                    partial: partly_sold.remove(&trip.asset),
                });
                done.push(trip);
            }
        }
        let Some(last) = legs.last().filter(|l| l.side == "buy") else {
            continue;
        };
        if since_ms.is_some_and(|since| last.ts_ms < since) {
            continue;
        }
        if let Some(quote) = quote.as_deref()
            && last.quote != quote
        {
            skipped.push(format!("{} entry on a non-{quote} market", last.symbol));
            continue;
        }
        let value_usdt = match value_decimal(row.pointer("/selected_route/source_value_usdt")) {
            Some(value) if value > Decimal::ZERO => value,
            _ if last.quote == "USDT" => last.price * last.filled,
            _ => {
                skipped.push(format!("{} entry without a USDT value", last.symbol));
                continue;
            }
        };
        let fee_rate = last.fee_rate.unwrap_or(DEFAULT_FEE_RATE);
        match open.get_mut(&last.base) {
            // Scale-in: one position at the weighted average price.
            Some(trip) if trip.symbol == last.symbol => {
                let quantity = trip.quantity + last.filled;
                trip.entry_price =
                    (trip.entry_price * trip.quantity + last.price * last.filled) / quantity;
                trip.quantity = quantity;
                trip.entry_value_usdt += value_usdt;
            }
            _ => {
                partly_sold.remove(&last.base);
                open.insert(
                    last.base.clone(),
                    RoundTrip {
                        symbol: last.symbol.clone(),
                        asset: last.base.clone(),
                        entry_ms: last.ts_ms,
                        entry_price: last.price,
                        quantity: last.filled,
                        fee_rate,
                        entry_value_usdt: value_usdt,
                        exit: None,
                    },
                );
            }
        }
    }
    done.extend(open.into_values());
    done.sort_by_key(|t| t.entry_ms);
    (done, skipped)
}

/// Every parseable JSON object row of the audit file, oldest first.
pub fn read_audit(path: &Path) -> Result<Vec<Value>> {
    let file = std::fs::File::open(path)?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(Value::is_object)
        .collect())
}

/// One way of exiting one round trip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub net_return_pct: Decimal,
    /// Net return times the USDT value spent on the position.
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub pnl_usdt: Decimal,
    pub hold_minutes: i64,
    /// Still (partly) held at the end of the window, marked at the last close.
    pub open_at_end: bool,
    pub exits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripReport {
    pub symbol: String,
    pub entry_at: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub entry_price: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub entry_value_usdt: Decimal,
    /// Average true range before entry, per candle size the plans use.
    pub atr: BTreeMap<String, Decimal>,
    pub window_minutes: i64,
    pub claude_partial_exit: bool,
    pub claude: Outcome,
    pub peak_upper_bound: Outcome,
    pub plans: BTreeMap<String, Outcome>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanSummary {
    pub plan: String,
    pub trips: usize,
    pub wins: usize,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub mean_net_return_pct: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub total_pnl_usdt: Decimal,
    /// `total_pnl_usdt` minus Claude's on the same trips.
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub vs_claude_usdt: Decimal,
    pub mean_hold_minutes: i64,
    pub open_at_end: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReplayReport {
    pub generated_at: String,
    pub audit_path: String,
    /// Quote filter (`None` = every market).
    pub quote: Option<String>,
    pub interval: String,
    pub horizon_minutes: u32,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub slippage_bps: Decimal,
    pub note: &'static str,
    /// Plans first (in the requested order), then `claude_actual` and `peak_upper_bound`.
    pub summary: Vec<PlanSummary>,
    pub trips: Vec<TripReport>,
    pub skipped: Vec<String>,
}

const NOTE: &str = "Research only. Simulated sells use the entry leg's fee rate plus slippage; \
claude_actual uses Claude's real fill (an exit on another market is marked at that market's \
close). Bars are read pessimistically (low before high). The summary only counts round trips \
Claude has closed. peak_upper_bound sells at the best price seen and is never expected return.";

struct Costs {
    fee_rate: Decimal,
    slippage: Decimal,
}

impl Costs {
    fn per_unit_cost(&self, trip: &RoundTrip) -> Decimal {
        trip.entry_price * (Decimal::ONE + self.fee_rate)
    }

    fn simulated(&self, price: Decimal) -> Decimal {
        price * (Decimal::ONE - self.fee_rate - self.slippage)
    }
}

fn outcome(
    trip: &RoundTrip,
    costs: &Costs,
    proceeds_per_unit: Decimal,
    last_ms: i64,
    open: bool,
    exits: Vec<String>,
) -> Outcome {
    let ratio = proceeds_per_unit / costs.per_unit_cost(trip) - Decimal::ONE;
    Outcome {
        net_return_pct: (ratio * Decimal::ONE_HUNDRED).round_dp(4),
        pnl_usdt: (ratio * trip.entry_value_usdt).round_dp(6),
        hold_minutes: (last_ms - trip.entry_ms).max(0) / MS_PER_MINUTE,
        open_at_end: open,
        exits,
    }
}

fn fill_note(fill: &ExitFill) -> String {
    format!(
        "{} {}@{}",
        fill.reason.as_str(),
        fill.fraction.normalize(),
        fill.price.normalize()
    )
}

/// Prices around one round trip.
#[derive(Debug, Clone, Default)]
pub struct TripPrices {
    /// 1-minute bars before entry (history for the stretch and slowdown rules).
    pub history: Vec<PriceBar>,
    /// Bars from entry on, at least up to Claude's exit (to mark an exit on another
    /// market).
    pub bars: Vec<PriceBar>,
    /// Average true range before entry, per candle size the plans use.
    pub atr: BTreeMap<AtrTimeframe, Decimal>,
}

/// Replay `plans` on one round trip. Plans, holding and the peak only use bars that open
/// at or before `horizon_end_ms`. A plan whose ATR is unknown is left out.
pub fn replay_trip(
    trip: &RoundTrip,
    prices: &TripPrices,
    horizon_end_ms: i64,
    plans: &[(String, ExitPlan)],
    slippage_bps: Decimal,
) -> Option<TripReport> {
    let bars: Vec<PriceBar> = prices
        .bars
        .iter()
        .copied()
        .filter(|b| b.open_ms >= trip.entry_ms)
        .collect();
    let window: Vec<PriceBar> = bars
        .iter()
        .copied()
        .filter(|b| b.open_ms <= horizon_end_ms)
        .collect();
    let last = *window.last()?;
    let costs = Costs {
        fee_rate: trip.fee_rate,
        slippage: slippage_bps / BPS,
    };

    let mut outcomes = BTreeMap::new();
    for (name, plan) in plans {
        let atr = plan
            .atr_timeframe()
            .and_then(|tf| prices.atr.get(&tf).copied());
        let Ok(mut watch) = ExitWatch::new(plan, trip.entry_price, trip.entry_ms, atr, trip.fee_rate)
        else {
            continue;
        };
        for bar in &prices.history {
            watch.warm_up_before_entry(bar);
        }
        let mut proceeds = Decimal::ZERO;
        let mut last_ms = trip.entry_ms;
        let mut exits = Vec::new();
        for bar in &window {
            for fill in watch.on_bar(bar) {
                proceeds += fill.fraction * costs.simulated(fill.price);
                last_ms = fill.ts_ms;
                exits.push(fill_note(&fill));
            }
            if watch.remaining() <= Decimal::ZERO {
                break;
            }
        }
        let open = watch.remaining() > Decimal::ZERO;
        if open {
            proceeds += watch.remaining() * costs.simulated(last.close);
            last_ms = last.close_ms;
        }
        outcomes.insert(
            name.clone(),
            outcome(trip, &costs, proceeds, last_ms, open, exits),
        );
    }

    let claude = match &trip.exit {
        Some(ActualExit {
            ts_ms,
            price: Some(price),
            ..
        }) => outcome(
            trip,
            &costs,
            price * (Decimal::ONE - costs.fee_rate),
            *ts_ms,
            false,
            vec![format!("claude 1@{}", price.normalize())],
        ),
        Some(ActualExit {
            ts_ms, price: None, ..
        }) => {
            let mark = bars
                .iter()
                .rev()
                .find(|b| b.open_ms <= *ts_ms)
                .map_or(trip.entry_price, |b| b.close);
            outcome(
                trip,
                &costs,
                costs.simulated(mark),
                *ts_ms,
                false,
                vec![format!(
                    "claude via another market, marked {}",
                    mark.normalize()
                )],
            )
        }
        None => outcome(
            trip,
            &costs,
            costs.simulated(last.close),
            last.close_ms,
            true,
            vec!["claude still holding".into()],
        ),
    };

    let peak_bar = window
        .iter()
        .max_by(|a, b| a.high.cmp(&b.high).then(b.open_ms.cmp(&a.open_ms)))?;
    let peak = outcome(
        trip,
        &costs,
        costs.simulated(peak_bar.high),
        peak_bar.close_ms,
        false,
        vec![format!("peak {}", peak_bar.high.normalize())],
    );

    Some(TripReport {
        symbol: trip.symbol.clone(),
        entry_at: iso(trip.entry_ms),
        entry_price: trip.entry_price,
        entry_value_usdt: trip.entry_value_usdt.round_dp(6),
        atr: prices
            .atr
            .iter()
            .map(|(tf, atr)| (tf.as_str().to_owned(), atr.round_dp(8)))
            .collect(),
        window_minutes: (last.close_ms - trip.entry_ms) / MS_PER_MINUTE,
        claude_partial_exit: trip.exit.as_ref().is_some_and(|e| e.partial),
        claude,
        peak_upper_bound: peak,
        plans: outcomes,
    })
}

fn summarize(
    name: &str,
    trips: &[TripReport],
    pick: impl Fn(&TripReport) -> Option<&Outcome>,
) -> PlanSummary {
    let rows: Vec<(&Outcome, &Outcome)> = trips
        .iter()
        .filter(|t| !t.claude.open_at_end)
        .filter_map(|t| pick(t).map(|o| (o, &t.claude)))
        .collect();
    let n = rows.len();
    let total: Decimal = rows.iter().map(|(o, _)| o.pnl_usdt).sum();
    let claude: Decimal = rows.iter().map(|(_, c)| c.pnl_usdt).sum();
    let count = Decimal::from(n.max(1));
    PlanSummary {
        plan: name.to_owned(),
        trips: n,
        wins: rows
            .iter()
            .filter(|(o, _)| o.pnl_usdt > Decimal::ZERO)
            .count(),
        mean_net_return_pct: (rows.iter().map(|(o, _)| o.net_return_pct).sum::<Decimal>() / count)
            .round_dp(4),
        total_pnl_usdt: total.round_dp(6),
        vs_claude_usdt: (total - claude).round_dp(6),
        mean_hold_minutes: rows.iter().map(|(o, _)| o.hold_minutes).sum::<i64>()
            / (n.max(1) as i64),
        open_at_end: rows.iter().filter(|(o, _)| o.open_at_end).count(),
    }
}

/// Summary rows: each plan in order, then `claude_actual` and `peak_upper_bound`.
pub fn summary(plans: &[(String, ExitPlan)], trips: &[TripReport]) -> Vec<PlanSummary> {
    let mut rows: Vec<PlanSummary> = plans
        .iter()
        .map(|(name, _)| summarize(name, trips, |t| t.plans.get(name)))
        .collect();
    rows.push(summarize("claude_actual", trips, |t| Some(&t.claude)));
    rows.push(summarize("peak_upper_bound", trips, |t| {
        Some(&t.peak_upper_bound)
    }));
    rows
}

/// Markdown table of the summary.
pub fn summary_table(report: &ReplayReport) -> String {
    let mut out = format!(
        "Exit-plan replay: {} round trips ({}), {} bars, horizon {} min, exit slippage {} bps. P&L in USDT.\n\n",
        report.trips.len(),
        report
            .quote
            .as_deref()
            .map_or_else(|| "every market".to_owned(), |q| format!("{q} markets")),
        report.interval,
        report.horizon_minutes,
        report.slippage_bps.normalize(),
    );
    out.push_str("| plan | trips | wins | mean net % | total P&L | vs Claude | mean hold min | open at end |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for row in &report.summary {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.plan,
            row.trips,
            row.wins,
            row.mean_net_return_pct.round_dp(2),
            row.total_pnl_usdt.round_dp(4),
            row.vs_claude_usdt.round_dp(4),
            row.mean_hold_minutes,
            row.open_at_end
        ));
    }
    if !report.skipped.is_empty() {
        out.push_str(&format!("\nSkipped: {}\n", report.skipped.join("; ")));
    }
    out.push_str(&format!("\n{}\n", report.note));
    out
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn interval_ms(interval: &str) -> Result<i64> {
    Ok(match interval {
        "1s" => 1_000,
        "1m" => MS_PER_MINUTE,
        "3m" => 3 * MS_PER_MINUTE,
        "5m" => 5 * MS_PER_MINUTE,
        "15m" => 15 * MS_PER_MINUTE,
        other => {
            return Err(TraderError::config(format!(
                "unsupported replay interval {other:?} (1s, 1m, 3m, 5m, 15m)"
            )));
        }
    })
}

fn time(ms: i64) -> Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .ok_or_else(|| TraderError::config(format!("timestamp out of range: {ms}")))
}

async fn bars(
    fetcher: &HistoryFetcher,
    symbol: &str,
    interval: &str,
    step_ms: i64,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<PriceBar>> {
    let klines = fetcher
        .fetch_klines(symbol, interval, time(start_ms)?, Some(time(end_ms)?))
        .await?;
    Ok(klines
        .into_iter()
        .map(|k| PriceBar {
            open_ms: k.open_time,
            close_ms: k.close_time.unwrap_or(k.open_time + step_ms - 1),
            open: k.open,
            high: k.high,
            low: k.low,
            close: k.close,
        })
        .collect())
}

/// The 1-minute history before entry, the ATR per candle size, and the replay bars from
/// entry to `end_ms` at `interval` (`(name, step_ms)`).
async fn trip_prices(
    fetcher: &HistoryFetcher,
    trip: &RoundTrip,
    timeframes: &std::collections::BTreeSet<AtrTimeframe>,
    history_minutes: i64,
    interval: (&str, i64),
    end_ms: i64,
) -> Result<TripPrices> {
    let before_entry = |bars: Vec<PriceBar>| -> Vec<PriceBar> {
        bars.into_iter()
            .filter(|b| b.close_ms < trip.entry_ms)
            .collect()
    };
    let history = before_entry(
        bars(
            fetcher,
            &trip.symbol,
            "1m",
            MS_PER_MINUTE,
            trip.entry_ms - history_minutes * MS_PER_MINUTE,
            trip.entry_ms - 1,
        )
        .await?,
    );
    let mut atr = BTreeMap::new();
    for tf in timeframes {
        // One spare candle so the first range has a previous close.
        let span = tf.millis() * (ATR_PERIODS as i64 + 1);
        let candles = before_entry(
            bars(
                fetcher,
                &trip.symbol,
                tf.as_str(),
                tf.millis(),
                trip.entry_ms - span,
                trip.entry_ms - 1,
            )
            .await?,
        );
        if let Some(value) = average_true_range(&candles) {
            atr.insert(*tf, value);
        }
    }
    let (name, step_ms) = interval;
    let bars = bars(fetcher, &trip.symbol, name, step_ms, trip.entry_ms, end_ms).await?;
    Ok(TripPrices { history, bars, atr })
}

/// Build the report: read the audit, fetch klines per round trip, replay every plan.
pub async fn build_report(
    args: &ExitReplayArgs,
    client: BinanceClient,
    now_ms: i64,
) -> Result<ReplayReport> {
    let step_ms = interval_ms(args.interval.trim())?;
    if args.horizon_minutes == 0 {
        return Err(TraderError::config("--horizon-minutes must be positive"));
    }
    if args.slippage_bps < Decimal::ZERO {
        return Err(TraderError::config("--slippage-bps must not be negative"));
    }
    let specs: Vec<String> = if args.plans.is_empty() {
        DEFAULT_PLANS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        args.plans.clone()
    };
    let mut plans: Vec<(String, ExitPlan)> = Vec::with_capacity(specs.len());
    for spec in &specs {
        let plan = ExitPlan::parse_spec(spec)
            .map_err(|e| TraderError::config(format!("--plan {spec:?}: {e}")))?;
        let name = plan.to_string();
        if !plans.iter().any(|(n, _)| *n == name) {
            plans.push((name, plan));
        }
    }
    let since_ms = args
        .since
        .as_deref()
        .map(|s| {
            DateTime::parse_from_rfc3339(s.trim())
                .map(|t| t.timestamp_millis())
                .map_err(|e| TraderError::config(format!("--since {s:?}: {e}")))
        })
        .transpose()?;
    let audit_path = args.audit_path.clone().unwrap_or_else(|| {
        PathBuf::from(bb_core::env::string(
            &["CLAUDE_SPOT_AUDIT_PATH"],
            DEFAULT_AUDIT_PATH,
        ))
    });
    let rows = read_audit(&audit_path)?;
    let quote = args
        .quote
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_uppercase);
    let (round_trips, mut skipped) = round_trips(&rows, quote.as_deref(), since_ms);

    let timeframes: std::collections::BTreeSet<AtrTimeframe> = plans
        .iter()
        .filter_map(|(_, plan)| plan.atr_timeframe())
        .collect();
    let fetcher = HistoryFetcher::spot(client);
    let mut trips = Vec::with_capacity(round_trips.len());
    for trip in &round_trips {
        let horizon_end = trip.entry_ms + i64::from(args.horizon_minutes) * MS_PER_MINUTE;
        // Bars up to Claude's exit too, to mark an exit made on another market.
        let claude_exit = trip.exit.as_ref().map_or(0, |e| e.ts_ms + MS_PER_MINUTE);
        let end_ms = horizon_end.max(claude_exit).min(now_ms);
        let fetched = trip_prices(
            &fetcher,
            trip,
            &timeframes,
            i64::from(args.history_minutes),
            (args.interval.trim(), step_ms),
            end_ms,
        )
        .await;
        let prices = match fetched {
            Ok(prices) => prices,
            Err(error) => {
                skipped.push(format!(
                    "{} at {}: klines unavailable ({error})",
                    trip.symbol,
                    iso(trip.entry_ms)
                ));
                continue;
            }
        };
        match replay_trip(trip, &prices, horizon_end, &plans, args.slippage_bps) {
            Some(report) => trips.push(report),
            None => skipped.push(format!(
                "{} at {}: no bars after entry yet",
                trip.symbol,
                iso(trip.entry_ms)
            )),
        }
    }
    Ok(ReplayReport {
        generated_at: iso(now_ms),
        audit_path: audit_path.display().to_string(),
        quote,
        interval: args.interval.trim().to_owned(),
        horizon_minutes: args.horizon_minutes,
        slippage_bps: args.slippage_bps,
        note: NOTE,
        summary: summary(&plans, &trips),
        trips,
        skipped,
    })
}

/// `bb claude-spot-exit-replay`.
pub async fn run_cli(args: ExitReplayArgs) -> Result<ExitCode> {
    let client = BinanceClient::new(BinanceConfig::new(false))?;
    let report = build_report(&args, client, bb_core::time::now_ms()).await?;
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = &args.output {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{json}\n"))?;
    }
    if args.table {
        print!("{}", summary_table(&report));
    } else {
        println!("{json}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    fn leg(symbol: &str, side: &str, average: &str, filled: &str, ts: i64) -> Value {
        json!({
            "symbol": symbol,
            "side": side,
            "status": "closed",
            "final_order": {"average": average, "filled": filled, "timestamp": ts}
        })
    }

    fn route(legs: Vec<Value>, fee_rate: f64) -> Value {
        let fees: Vec<Value> = legs.iter().map(|_| json!({"fee_rate": fee_rate})).collect();
        json!({
            "row_type": "route",
            "status": "submitted",
            "selected_route": {"legs": fees},
            "submitted_legs": legs
        })
    }

    #[test]
    fn pairs_buys_with_the_next_sell_of_the_same_coin() {
        let mut legacy = route(vec![leg("XLM/EUR", "buy", "0.25", "40", 6_000)], 0.001);
        legacy["selected_route"]["source_value_usdt"] = json!(11.5);
        legacy.as_object_mut().unwrap().remove("row_type");
        let rows = vec![
            route(vec![leg("ENA/USDT", "buy", "0.2", "100", 1_000)], 0.001),
            json!({"row_type": "route", "status": "failed", "submitted_legs": [leg("ENA/USDT", "sell", "0.1", "100", 1_500)]}),
            route(vec![leg("ENA/USDT", "buy", "0.4", "100", 1_800)], 0.001),
            route(vec![leg("ENA/USDT", "sell", "0.25", "200", 2_000)], 0.001),
            route(vec![leg("SOL/BTC", "buy", "0.002", "1", 2_500)], 0.001),
            route(vec![leg("OP/USDT", "buy", "1", "10", 3_000)], 0.002),
            route(vec![leg("OP/BNB", "sell", "0.01", "5", 4_000)], 0.001),
            route(vec![leg("OP/BNB", "sell", "0.011", "5", 4_500)], 0.001),
            route(vec![leg("FIL/USDT", "buy", "2", "1", 5_000)], 0.001),
            legacy,
            json!({"row_type": "cycle_summary", "status": "submitted", "submitted_legs": [leg("FIL/USDT", "sell", "3", "1", 7_000)]}),
        ];
        let (trips, skipped) = round_trips(&rows, Some("usdt"), None);
        assert_eq!(
            skipped,
            vec![
                "SOL/BTC entry on a non-USDT market",
                "XLM/EUR entry on a non-USDT market"
            ]
        );
        assert_eq!(trips.len(), 3);

        // Scale-in merged at the weighted price and USDT value, closed by the sell; the
        // failed row is ignored.
        assert_eq!(
            (
                trips[0].entry_price,
                trips[0].quantity,
                trips[0].entry_value_usdt
            ),
            (dec!(0.3), dec!(200), dec!(60))
        );
        assert_eq!(
            trips[0].exit,
            Some(ActualExit {
                ts_ms: 2_000,
                price: Some(dec!(0.25)),
                partial: false
            })
        );
        // Half sold, then the rest on another market: the trip stays open through the
        // partial sell (quantity 10 -> 5) and closes on the second, flagged partial.
        assert_eq!(
            trips[1].exit,
            Some(ActualExit {
                ts_ms: 4_500,
                price: None,
                partial: true
            })
        );
        assert_eq!(trips[1].quantity, dec!(5));
        assert_eq!(trips[1].fee_rate, dec!(0.002));
        // A cycle_summary row never closes a position.
        assert_eq!(trips[2].exit, None);

        // Every market: the SOL/BTC entry has no USDT value; the legacy row (no row_type)
        // carries one.
        let (all, skipped) = round_trips(&rows, None, Some(2_400));
        assert_eq!(skipped, vec!["SOL/BTC entry without a USDT value"]);
        assert_eq!(
            all.iter().map(|t| t.asset.as_str()).collect::<Vec<_>>(),
            ["OP", "FIL", "XLM"]
        );
        assert_eq!(all[2].entry_value_usdt, dec!(11.5));
    }

    fn bar(minute: i64, open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> PriceBar {
        PriceBar {
            open_ms: minute * MS_PER_MINUTE,
            close_ms: (minute + 1) * MS_PER_MINUTE - 1,
            open,
            high,
            low,
            close,
        }
    }

    #[test]
    fn replay_costs_plans_against_claudes_real_exit() {
        let trip = RoundTrip {
            symbol: "X/USDT".into(),
            asset: "X".into(),
            entry_ms: 0,
            entry_price: dec!(100),
            quantity: dec!(1),
            fee_rate: dec!(0.001),
            entry_value_usdt: dec!(50),
            exit: Some(ActualExit {
                ts_ms: 2 * MS_PER_MINUTE,
                price: Some(dec!(99)),
                partial: false,
            }),
        };
        let window = [
            bar(0, dec!(100), dec!(110), dec!(100), dec!(110)),
            bar(1, dec!(110), dec!(110), dec!(105), dec!(106)),
            bar(2, dec!(106), dec!(107), dec!(104), dec!(104)),
        ];
        let plans: Vec<(String, ExitPlan)> = ["hold", "trail=3"]
            .iter()
            .map(|s| (s.to_string(), ExitPlan::parse_spec(s).unwrap()))
            .collect();
        let prices = TripPrices {
            bars: window.to_vec(),
            ..TripPrices::default()
        };
        let report = replay_trip(&trip, &prices, i64::MAX, &plans, dec!(10)).unwrap();

        // Cost per unit 100.1; trail sells at 110 * 0.97 = 106.7 less 20 bps.
        let trail = &report.plans["trail=3"];
        assert_eq!(trail.exits, vec!["follow_peak 1@106.7"]);
        let expected = dec!(106.7) * dec!(0.998) / dec!(100.1) - Decimal::ONE;
        assert_eq!(trail.net_return_pct, (expected * dec!(100)).round_dp(4));
        assert!(!trail.open_at_end);

        // Hold is marked at the last close and flagged open.
        let hold = &report.plans["hold"];
        assert!(hold.open_at_end);
        assert_eq!(hold.hold_minutes, 2);

        // Claude: real fill 99 less the 10 bps fee, no slippage; P&L on the USDT value spent.
        let claude = dec!(99) * dec!(0.999) / dec!(100.1) - Decimal::ONE;
        assert_eq!(report.claude.pnl_usdt, (claude * dec!(50)).round_dp(6));
        assert_eq!(report.peak_upper_bound.exits, vec!["peak 110"]);

        let rows = summary(&plans, std::slice::from_ref(&report));
        let names: Vec<&str> = rows.iter().map(|r| r.plan.as_str()).collect();
        assert_eq!(
            names,
            ["hold", "trail=3", "claude_actual", "peak_upper_bound"]
        );
        assert_eq!(
            rows[1].vs_claude_usdt,
            (trail.pnl_usdt - report.claude.pnl_usdt).round_dp(6)
        );
        assert_eq!(rows[2].vs_claude_usdt, Decimal::ZERO);
    }

    #[test]
    fn atr_plans_are_skipped_without_an_atr_and_use_it_when_known() {
        let trip = RoundTrip {
            symbol: "X/USDT".into(),
            asset: "X".into(),
            entry_ms: 0,
            entry_price: dec!(100),
            quantity: dec!(1),
            fee_rate: dec!(0.001),
            entry_value_usdt: dec!(100),
            exit: None,
        };
        let plans = vec![(
            "atr=2:1h".to_string(),
            ExitPlan::parse_spec("atr=2:1h").unwrap(),
        )];
        let mut prices = TripPrices {
            bars: vec![
                bar(0, dec!(100), dec!(110), dec!(100), dec!(110)),
                bar(1, dec!(110), dec!(110), dec!(104), dec!(104)),
            ],
            ..TripPrices::default()
        };
        let report = replay_trip(&trip, &prices, i64::MAX, &plans, dec!(5)).unwrap();
        assert!(report.plans.is_empty());
        assert!(report.claude.open_at_end);

        prices.atr.insert(AtrTimeframe::H1, dec!(2.5));
        let report = replay_trip(&trip, &prices, i64::MAX, &plans, dec!(5)).unwrap();
        assert_eq!(report.plans["atr=2:1h"].exits, vec!["follow_peak 1@105"]);
        assert_eq!(report.atr["1h"], dec!(2.5));
        assert!(replay_trip(&trip, &TripPrices::default(), i64::MAX, &plans, dec!(5)).is_none());
    }

    #[test]
    fn plans_stop_at_the_horizon_but_claudes_late_exit_is_marked_at_its_time() {
        let mut trip = RoundTrip {
            symbol: "X/BNB".into(),
            asset: "X".into(),
            entry_ms: 0,
            entry_price: dec!(100),
            quantity: dec!(1),
            fee_rate: dec!(0.001),
            entry_value_usdt: dec!(10),
            exit: Some(ActualExit {
                ts_ms: 5 * MS_PER_MINUTE + 30_000,
                price: None,
                partial: false,
            }),
        };
        let bars: Vec<PriceBar> = (0..8)
            .map(|m| {
                let p = Decimal::from(100 + m);
                bar(m, p, p, p, p)
            })
            .collect();
        let plans = vec![("hold".to_string(), ExitPlan::default())];
        let prices = TripPrices {
            bars,
            ..TripPrices::default()
        };
        let report = replay_trip(&trip, &prices, MS_PER_MINUTE, &plans, dec!(0)).unwrap();
        assert_eq!(report.plans["hold"].exits, Vec::<String>::new());
        assert_eq!(report.plans["hold"].hold_minutes, 1); // marked at minute 1's close (101)
        assert_eq!(report.peak_upper_bound.exits, vec!["peak 101"]);
        assert_eq!(
            report.claude.exits,
            vec!["claude via another market, marked 105"]
        );
        assert_eq!(summary(&plans, std::slice::from_ref(&report))[0].trips, 1);

        // A position Claude has not closed is reported but left out of the summary.
        trip.exit = None;
        let open = replay_trip(&trip, &prices, MS_PER_MINUTE, &plans, dec!(0)).unwrap();
        assert!(open.claude.open_at_end);
        let rows = summary(&plans, &[report, open]);
        assert!(rows.iter().all(|r| r.trips == 1));
    }
}

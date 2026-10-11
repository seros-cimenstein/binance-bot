//! Backtest harness for macro-event pre-positioning trades (port of
//! `src/news_event_backtest.py`).
//!
//! Hypothesis: scheduled macro releases (FOMC, CPI, BTC ETF decisions, the halving)
//! produce repeatable volatility expansion. For each built-in event the harness fetches
//! a kline window around the scheduled time and simulates one of:
//!
//! * `long_only`: enter long `pre_minutes` before, exit `post_minutes` after
//! * `short_only`: enter short
//! * `straddle`: hold both legs — captures the absolute move minus 2x costs
//!
//! Read-only: no live runtime exists; a positive verdict here is the precondition.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bb_binance::BinanceClient;
use bb_binance::history::HistoryFetcher;
use chrono::TimeDelta;
use serde::{Deserialize, Serialize};

use crate::common::{self, Bar, parse_iso, pstdev, py_round, py_sum, split_csv};
use crate::error::{Result, StrategyError};
use crate::grid_backtest::{read_cached_bars, write_cached_rows};

/// One scheduled macro event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroEvent {
    /// e.g. `FOMC 2024-01-31`.
    pub label: String,
    /// `fomc` | `cpi` | `etf` | `halving`.
    pub category: String,
    /// ISO timestamp.
    pub event_time_utc: String,
}

impl MacroEvent {
    pub fn new(
        label: impl Into<String>,
        category: impl Into<String>,
        event_time_utc: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            category: category.into(),
            event_time_utc: event_time_utc.into(),
        }
    }
}

/// Representative, well-documented 2024-2025 events: official release times in UTC
/// (FOMC statements 18:00/19:00 UTC, CPI 12:30/13:30 UTC), from the public Federal
/// Reserve / BLS release calendars.
const KNOWN_EVENTS_2024_2025: [(&str, &str, &str); 35] = [
    // 2024 FOMC statements
    ("FOMC 2024-01-31", "fomc", "2024-01-31T19:00:00+00:00"),
    ("FOMC 2024-03-20", "fomc", "2024-03-20T18:00:00+00:00"),
    ("FOMC 2024-05-01", "fomc", "2024-05-01T18:00:00+00:00"),
    ("FOMC 2024-06-12", "fomc", "2024-06-12T18:00:00+00:00"),
    ("FOMC 2024-07-31", "fomc", "2024-07-31T18:00:00+00:00"),
    ("FOMC 2024-09-18", "fomc", "2024-09-18T18:00:00+00:00"),
    ("FOMC 2024-11-07", "fomc", "2024-11-07T19:00:00+00:00"),
    ("FOMC 2024-12-18", "fomc", "2024-12-18T19:00:00+00:00"),
    // 2025 FOMC
    ("FOMC 2025-01-29", "fomc", "2025-01-29T19:00:00+00:00"),
    ("FOMC 2025-03-19", "fomc", "2025-03-19T18:00:00+00:00"),
    ("FOMC 2025-05-07", "fomc", "2025-05-07T18:00:00+00:00"),
    ("FOMC 2025-06-18", "fomc", "2025-06-18T18:00:00+00:00"),
    ("FOMC 2025-07-30", "fomc", "2025-07-30T18:00:00+00:00"),
    ("FOMC 2025-09-17", "fomc", "2025-09-17T18:00:00+00:00"),
    ("FOMC 2025-10-29", "fomc", "2025-10-29T18:00:00+00:00"),
    ("FOMC 2025-12-10", "fomc", "2025-12-10T19:00:00+00:00"),
    // 2024 CPI releases
    ("CPI 2024-01-11", "cpi", "2024-01-11T13:30:00+00:00"),
    ("CPI 2024-02-13", "cpi", "2024-02-13T13:30:00+00:00"),
    ("CPI 2024-03-12", "cpi", "2024-03-12T12:30:00+00:00"),
    ("CPI 2024-04-10", "cpi", "2024-04-10T12:30:00+00:00"),
    ("CPI 2024-05-15", "cpi", "2024-05-15T12:30:00+00:00"),
    ("CPI 2024-06-12", "cpi", "2024-06-12T12:30:00+00:00"),
    ("CPI 2024-07-11", "cpi", "2024-07-11T12:30:00+00:00"),
    ("CPI 2024-08-14", "cpi", "2024-08-14T12:30:00+00:00"),
    ("CPI 2024-09-11", "cpi", "2024-09-11T12:30:00+00:00"),
    ("CPI 2024-10-10", "cpi", "2024-10-10T12:30:00+00:00"),
    ("CPI 2024-11-13", "cpi", "2024-11-13T13:30:00+00:00"),
    ("CPI 2024-12-11", "cpi", "2024-12-11T13:30:00+00:00"),
    // 2025 CPI
    ("CPI 2025-01-15", "cpi", "2025-01-15T13:30:00+00:00"),
    ("CPI 2025-02-12", "cpi", "2025-02-12T13:30:00+00:00"),
    ("CPI 2025-03-12", "cpi", "2025-03-12T12:30:00+00:00"),
    ("CPI 2025-04-10", "cpi", "2025-04-10T12:30:00+00:00"),
    ("CPI 2025-05-13", "cpi", "2025-05-13T12:30:00+00:00"),
    // Special events
    ("Spot BTC ETF approval", "etf", "2024-01-10T21:00:00+00:00"),
    ("BTC Halving", "halving", "2024-04-19T23:00:00+00:00"),
];

/// The built-in event calendar (Python `KNOWN_EVENTS_2024_2025`), in declaration order.
pub fn known_events() -> Vec<MacroEvent> {
    KNOWN_EVENTS_2024_2025
        .iter()
        .map(|(label, category, ts)| MacroEvent::new(*label, *category, *ts))
        .collect()
}

/// Pre-position knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewsEventConfig {
    pub symbol: String,
    pub pre_minutes: i64,
    pub post_minutes: i64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    pub notional_usdt: f64,
    /// `long_only` | `short_only` | `straddle`.
    pub strategy: String,
}

impl Default for NewsEventConfig {
    fn default() -> Self {
        Self {
            symbol: "BTC/USDT".into(),
            pre_minutes: 15,
            post_minutes: 30,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            notional_usdt: 100.0,
            strategy: "straddle".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventTrade {
    pub label: String,
    pub category: String,
    pub event_time_ms: i64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub move_pct: f64,
    pub pnl_usdt: f64,
    pub fee_usdt: f64,
    pub strategy: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewsEventResult {
    pub config: NewsEventConfig,
    pub events_evaluated: usize,
    pub events_skipped_missing_data: usize,
    pub num_trades: usize,
    pub total_pnl_usdt: f64,
    pub avg_pnl_usdt: f64,
    pub win_rate_pct: f64,
    pub avg_abs_move_pct: f64,
    pub sharpe_per_event: f64,
    pub by_category_pnl: BTreeMap<String, f64>,
    pub by_category_count: BTreeMap<String, usize>,
    pub trades: Vec<EventTrade>,
}

impl NewsEventResult {
    /// Edge needs average P&L > 0 *and* per-event Sharpe > 0.5 (a lower bar than the
    /// continuous strategies because trades are discrete events).
    pub fn verdict(&self) -> &'static str {
        if self.events_evaluated == 0 {
            "no_data"
        } else if self.num_trades == 0 {
            "no_trades"
        } else if self.avg_pnl_usdt > 0.0 && self.sharpe_per_event > 0.5 {
            "edge_likely"
        } else if self.avg_pnl_usdt > 0.0 {
            "edge_marginal"
        } else {
            "no_edge"
        }
    }
}

/// Close of the 1-minute bar containing `target_ms`, else of the nearest bar (first on
/// ties) when it is within 5 minutes.
pub fn kline_close(klines: &[Bar], target_ms: i64) -> Option<f64> {
    let mut closest: Option<(&Bar, u64)> = None;
    for bar in klines {
        if bar.open_ms <= target_ms && target_ms < bar.open_ms.saturating_add(60_000) {
            return Some(bar.close);
        }
        let dist = target_ms.abs_diff(bar.open_ms);
        if closest.is_none_or(|(_, best)| dist < best) {
            closest = Some((bar, dist));
        }
    }
    closest
        .filter(|(_, dist)| *dist <= 5 * 60_000)
        .map(|(bar, _)| bar.close)
}

fn event_ms(event: &MacroEvent) -> Result<i64> {
    Ok(parse_iso(&event.event_time_utc)?.timestamp_millis())
}

/// Simulate one pre/post trade for one event using its surrounding klines.
/// `Ok(None)` when either price is missing or the entry price is not positive.
pub fn simulate_event(
    event: &MacroEvent,
    klines: &[Bar],
    cfg: &NewsEventConfig,
) -> Result<Option<EventTrade>> {
    let event_ms = event_ms(event)?;
    let entry_ms = event_ms.saturating_sub(cfg.pre_minutes.saturating_mul(60_000));
    let exit_ms = event_ms.saturating_add(cfg.post_minutes.saturating_mul(60_000));

    let (Some(entry_px), Some(exit_px)) =
        (kline_close(klines, entry_ms), kline_close(klines, exit_ms))
    else {
        return Ok(None);
    };
    if entry_px <= 0.0 {
        return Ok(None);
    }

    let one_side_cost_rate = (cfg.taker_fee_bps + cfg.slippage_bps) / 10_000.0;
    let move_pct = (exit_px / entry_px - 1.0) * 100.0;
    let qty = cfg.notional_usdt / entry_px;

    let (pnl, fee) = match cfg.strategy.as_str() {
        "long_only" => {
            let fee = 2.0 * cfg.notional_usdt * one_side_cost_rate;
            (qty * (exit_px - entry_px) - fee, fee)
        }
        "short_only" => {
            let fee = 2.0 * cfg.notional_usdt * one_side_cost_rate;
            (qty * (entry_px - exit_px) - fee, fee)
        }
        "straddle" => {
            // Equal-notional long + short legs: absolute move minus twice the round trip.
            let fee = 4.0 * cfg.notional_usdt * one_side_cost_rate;
            ((exit_px - entry_px).abs() * qty - fee, fee)
        }
        other => {
            return Err(StrategyError::InvalidConfig(format!(
                "unknown strategy: {other}"
            )));
        }
    };

    Ok(Some(EventTrade {
        label: event.label.clone(),
        category: event.category.clone(),
        event_time_ms: event_ms,
        entry_price: entry_px,
        exit_price: exit_px,
        move_pct,
        pnl_usdt: pnl,
        fee_usdt: fee,
        strategy: cfg.strategy.clone(),
    }))
}

/// Simulate every event (klines looked up by event label) and aggregate.
pub fn replay_events(
    events: &[MacroEvent],
    klines_by_event: &HashMap<String, Vec<Bar>>,
    cfg: &NewsEventConfig,
) -> Result<NewsEventResult> {
    let mut trades: Vec<EventTrade> = Vec::new();
    let mut skipped = 0_usize;
    for event in events {
        let klines = klines_by_event
            .get(&event.label)
            .map_or(&[][..], Vec::as_slice);
        match simulate_event(event, klines, cfg)? {
            Some(trade) => trades.push(trade),
            None => skipped += 1,
        }
    }
    let mut result = NewsEventResult {
        config: cfg.clone(),
        events_evaluated: events.len(),
        events_skipped_missing_data: skipped,
        num_trades: trades.len(),
        total_pnl_usdt: 0.0,
        avg_pnl_usdt: 0.0,
        win_rate_pct: 0.0,
        avg_abs_move_pct: 0.0,
        sharpe_per_event: 0.0,
        by_category_pnl: BTreeMap::new(),
        by_category_count: BTreeMap::new(),
        trades: Vec::new(),
    };
    if trades.is_empty() {
        return Ok(result);
    }

    let num = trades.len() as f64;
    let pnls: Vec<f64> = trades.iter().map(|t| t.pnl_usdt).collect();
    let total = py_sum(pnls.iter().copied());
    let avg = total / num;
    let wins = pnls.iter().filter(|p| **p > 0.0).count();
    let losses = pnls.iter().filter(|p| **p < 0.0).count();
    let decisive = wins + losses;
    result.total_pnl_usdt = total;
    result.avg_pnl_usdt = avg;
    result.win_rate_pct = if decisive > 0 {
        wins as f64 / decisive as f64 * 100.0
    } else {
        0.0
    };
    result.avg_abs_move_pct = py_sum(trades.iter().map(|t| t.move_pct.abs())) / num;
    if trades.len() > 1 {
        let sd = pstdev(&pnls);
        result.sharpe_per_event = if sd > 0.0 { avg / sd } else { 0.0 };
    }
    for trade in &trades {
        *result
            .by_category_pnl
            .entry(trade.category.clone())
            .or_insert(0.0) += trade.pnl_usdt;
        *result
            .by_category_count
            .entry(trade.category.clone())
            .or_insert(0) += 1;
    }
    result.trades = trades;
    Ok(result)
}

/// Compact CLI summary (Python `_summary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub config: NewsEventConfig,
    pub events_evaluated: usize,
    pub events_skipped_missing_data: usize,
    pub num_trades: usize,
    pub total_pnl_usdt: f64,
    pub avg_pnl_usdt: f64,
    pub win_rate_pct: f64,
    pub avg_abs_move_pct: f64,
    pub sharpe_per_event: f64,
    pub by_category_pnl: BTreeMap<String, f64>,
    pub by_category_count: BTreeMap<String, usize>,
}

pub fn result_summary(result: &NewsEventResult) -> ResultSummary {
    ResultSummary {
        verdict: result.verdict(),
        config: result.config.clone(),
        events_evaluated: result.events_evaluated,
        events_skipped_missing_data: result.events_skipped_missing_data,
        num_trades: result.num_trades,
        total_pnl_usdt: py_round(result.total_pnl_usdt, 4),
        avg_pnl_usdt: py_round(result.avg_pnl_usdt, 4),
        win_rate_pct: py_round(result.win_rate_pct, 2),
        avg_abs_move_pct: py_round(result.avg_abs_move_pct, 4),
        sharpe_per_event: py_round(result.sharpe_per_event, 4),
        by_category_pnl: result
            .by_category_pnl
            .iter()
            .map(|(k, v)| (k.clone(), py_round(*v, 4)))
            .collect(),
        by_category_count: result.by_category_count.clone(),
    }
}

/// `{cache_dir}/event_{label with ' ' and '/' → '_'}.json` (keyed by label only, as in Python).
pub fn event_cache_path(cache_dir: &Path, event: &MacroEvent) -> PathBuf {
    cache_dir.join(format!(
        "event_{}.json",
        event.label.replace([' ', '/'], "_")
    ))
}

/// Klines from `pre_minutes + 5` before the event to `post_minutes + 5` after it, with
/// the optional disk cache (unreadable cache → refetch).
pub async fn fetch_event_window(
    client: &BinanceClient,
    event: &MacroEvent,
    symbol: &str,
    interval: &str,
    pre_minutes: i64,
    post_minutes: i64,
    cache_dir: Option<&Path>,
) -> Result<Vec<Bar>> {
    let event_dt = parse_iso(&event.event_time_utc)?;
    let minutes = |m: i64| {
        TimeDelta::try_minutes(m.saturating_add(5)).ok_or_else(|| {
            StrategyError::InvalidConfig(format!("event window offset out of range: {m} minutes"))
        })
    };
    let out_of_range = || StrategyError::InvalidConfig("event window out of range".into());
    let start = event_dt
        .checked_sub_signed(minutes(pre_minutes)?)
        .ok_or_else(out_of_range)?;
    let end = event_dt
        .checked_add_signed(minutes(post_minutes)?)
        .ok_or_else(out_of_range)?;
    let cache_path = cache_dir.map(|dir| event_cache_path(dir, event));
    if let Some(path) = cache_path.as_deref()
        && let Some(bars) = read_cached_bars(path)
    {
        return Ok(bars);
    }
    let klines = HistoryFetcher::spot(client.clone())
        .fetch_klines(symbol, interval, start, Some(end))
        .await?;
    if let Some(path) = cache_path.as_deref() {
        write_cached_rows(path, &klines)?;
    }
    Ok(klines.iter().map(Bar::from_kline).collect())
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, default_value = "BTC/USDT")]
    pub symbol: String,
    #[arg(long, default_value = "1m")]
    pub interval: String,
    #[arg(long, default_value = "straddle", value_parser = clap::builder::PossibleValuesParser::new(["long_only", "short_only", "straddle"]))]
    pub strategy: String,
    #[arg(long, default_value_t = 15, allow_negative_numbers = true)]
    pub pre_minutes: i64,
    #[arg(long, default_value_t = 30, allow_negative_numbers = true)]
    pub post_minutes: i64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub taker_fee_bps: f64,
    #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
    pub slippage_bps: f64,
    #[arg(long, default_value_t = 100.0, allow_negative_numbers = true)]
    pub notional_usdt: f64,
    /// comma-separated category filter (fomc,cpi,etf,halving)
    #[arg(long, default_value = "")]
    pub categories: String,
    /// Directory to cache fetched event windows.
    #[arg(long, default_value = "")]
    pub klines_cache: String,
    /// Optional path to dump the full report JSON.
    #[arg(long, default_value = "")]
    pub output: String,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = NewsEventConfig {
        symbol: args.symbol.clone(),
        pre_minutes: args.pre_minutes,
        post_minutes: args.post_minutes,
        taker_fee_bps: args.taker_fee_bps,
        slippage_bps: args.slippage_bps,
        notional_usdt: args.notional_usdt,
        strategy: args.strategy.clone(),
    };
    let mut events = known_events();
    if !args.categories.is_empty() {
        let wanted = split_csv(&args.categories);
        events.retain(|e| wanted.contains(&e.category));
    }
    let cache_dir = (!args.klines_cache.is_empty()).then(|| PathBuf::from(&args.klines_cache));

    let client = common::public_client()?;
    let mut klines_by_event: HashMap<String, Vec<Bar>> = HashMap::with_capacity(events.len());
    for event in &events {
        let bars = fetch_event_window(
            &client,
            event,
            &args.symbol,
            &args.interval,
            args.pre_minutes,
            args.post_minutes,
            cache_dir.as_deref(),
        )
        .await?;
        klines_by_event.insert(event.label.clone(), bars);
    }

    let result = replay_events(&events, &klines_by_event, &cfg)?;
    common::print_json(&result_summary(&result))?;
    if !args.output.is_empty() {
        common::write_report(Path::new(&args.output), &result)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const E1_MS: i64 = 1_706_727_600_000;
    const E2_MS: i64 = 1_710_957_600_000;

    fn bar(open_ms: i64, close: f64) -> Bar {
        Bar::new(open_ms, close, close, close, close)
    }

    fn build_klines(start_ms: i64, prices: &[f64]) -> Vec<Bar> {
        prices
            .iter()
            .enumerate()
            .map(|(i, p)| bar(start_ms + i as i64 * 60_000, *p))
            .collect()
    }

    /// 60 one-minute bars from 30 min before the event; `post` from the event on.
    fn around(event_ms: i64, post: f64) -> Vec<Bar> {
        (0..60)
            .map(|i| {
                let t = event_ms - 30 * 60_000 + i * 60_000;
                bar(t, if t < event_ms { 100.0 } else { post })
            })
            .collect()
    }

    fn e(label: &str, ts: &str) -> MacroEvent {
        MacroEvent::new(label, "fomc", ts)
    }

    fn ladder(n: usize) -> Vec<f64> {
        (0..n).map(|i| 100.0 + i as f64).collect()
    }

    #[test]
    fn kline_close_finds_bar_containing_target() {
        assert_eq!(
            kline_close(&build_klines(0, &ladder(10)), 200_000),
            Some(103.0)
        );
    }

    #[test]
    fn kline_close_falls_back_to_nearest_within_5min() {
        assert_eq!(
            kline_close(&build_klines(0, &ladder(10)), 9 * 60_000 + 3 * 60_000),
            Some(109.0)
        );
    }

    #[test]
    fn kline_close_returns_none_when_far_outside() {
        assert_eq!(
            kline_close(&build_klines(0, &[100.0; 5]), 60_000 * 60),
            None
        );
    }

    #[test]
    fn kline_close_returns_none_when_no_klines() {
        assert_eq!(kline_close(&[], 0), None);
    }

    #[test]
    fn long_only_captures_upmove() {
        let cfg = NewsEventConfig {
            strategy: "long_only".into(),
            ..NewsEventConfig::default()
        };
        let trade = simulate_event(
            &e("E", "2024-01-31T19:00:00+00:00"),
            &around(E1_MS, 102.0),
            &cfg,
        )
        .unwrap()
        .unwrap();
        assert!((trade.move_pct - 2.0).abs() <= 0.01);
        assert!(trade.pnl_usdt > 1.5);
        assert_eq!(trade.event_time_ms, E1_MS);
    }

    #[test]
    fn short_only_captures_downmove() {
        let cfg = NewsEventConfig {
            strategy: "short_only".into(),
            ..NewsEventConfig::default()
        };
        let trade = simulate_event(
            &e("E", "2024-01-31T19:00:00+00:00"),
            &around(E1_MS, 98.0),
            &cfg,
        )
        .unwrap()
        .unwrap();
        assert!(trade.pnl_usdt > 1.5);
    }

    #[test]
    fn straddle_captures_abs_move_minus_costs() {
        let trade = simulate_event(
            &e("E", "2024-01-31T19:00:00+00:00"),
            &around(E1_MS, 105.0),
            &NewsEventConfig::default(),
        )
        .unwrap()
        .unwrap();
        assert!((trade.pnl_usdt - (5.0 - 0.60)).abs() <= 0.05);
    }

    #[test]
    fn returns_none_when_no_klines_match() {
        let trade = simulate_event(
            &e("E", "2024-01-31T19:00:00+00:00"),
            &[],
            &NewsEventConfig::default(),
        )
        .unwrap();
        assert!(trade.is_none());
    }

    #[test]
    fn rejects_unknown_strategy() {
        let cfg = NewsEventConfig {
            strategy: "bogus".into(),
            ..NewsEventConfig::default()
        };
        let err = simulate_event(
            &e("E", "2024-01-31T19:00:00+00:00"),
            &around(E1_MS, 100.0),
            &cfg,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn replay_aggregates_metrics_correctly() {
        let events = [
            e("E1", "2024-01-31T19:00:00+00:00"),
            e("E2", "2024-03-20T18:00:00+00:00"),
        ];
        let klines: HashMap<String, Vec<Bar>> = [
            ("E1".to_owned(), around(E1_MS, 102.0)),
            ("E2".to_owned(), around(E2_MS, 98.0)),
        ]
        .into();
        let cfg = NewsEventConfig {
            strategy: "long_only".into(),
            ..NewsEventConfig::default()
        };
        let result = replay_events(&events, &klines, &cfg).unwrap();
        assert_eq!(result.num_trades, 2);
        assert_eq!(result.events_evaluated, 2);
        assert_eq!(result.events_skipped_missing_data, 0);
        assert_eq!(
            result.by_category_count,
            BTreeMap::from([("fomc".to_owned(), 2)])
        );
        assert_eq!(result.win_rate_pct, 50.0);
    }

    #[test]
    fn replay_skips_events_with_missing_data() {
        let events = [
            e("E1", "2024-01-31T19:00:00+00:00"),
            e("E2", "2024-03-20T18:00:00+00:00"),
        ];
        let klines: HashMap<String, Vec<Bar>> = [
            (
                "E1".to_owned(),
                build_klines(E1_MS - 30 * 60_000, &[100.0; 60]),
            ),
            ("E2".to_owned(), Vec::new()),
        ]
        .into();
        let result = replay_events(&events, &klines, &NewsEventConfig::default()).unwrap();
        assert_eq!(result.events_evaluated, 2);
        assert_eq!(result.events_skipped_missing_data, 1);
        assert_eq!(result.num_trades, 1);
    }

    #[test]
    fn summary_round_trips_as_json() {
        let result = replay_events(&[], &HashMap::new(), &NewsEventConfig::default()).unwrap();
        let decoded: serde_json::Value =
            serde_json::from_str(&common::to_pretty_sorted(&result_summary(&result)).unwrap())
                .unwrap();
        assert_eq!(decoded["verdict"], result.verdict());
        assert_eq!(decoded["num_trades"], 0);
    }

    #[test]
    fn known_events_parse() {
        let events = known_events();
        assert_eq!(events.len(), 35);
        assert!(events.iter().all(|ev| event_ms(ev).is_ok()));
        assert_eq!(events[0].label, "FOMC 2024-01-31");
        assert_eq!(event_ms(&events[0]).unwrap(), E1_MS);
    }

    #[tokio::test]
    async fn fetch_event_window_requests_padded_range_and_caches() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let start_ms = E1_MS - 20 * 60_000;
        let end_ms = E1_MS + 35 * 60_000;
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .and(query_param("startTime", start_ms.to_string()))
            .and(query_param("endTime", end_ms.to_string()))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([[
                    start_ms,
                    "1",
                    "2",
                    "0.5",
                    "1.5",
                    "10",
                    start_ms + 59_999,
                    "15",
                    3,
                    "5",
                    "7.5",
                    "0"
                ],])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = BinanceClient::new(
            bb_binance::BinanceConfig::new(false)
                .with_spot_rest_url(server.uri())
                .with_futures_rest_url(server.uri()),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let event = MacroEvent::new("FOMC 2024/01/31", "fomc", "2024-01-31T19:00:00+00:00");
        let first = fetch_event_window(&client, &event, "BTC/USDT", "1m", 15, 30, Some(dir.path()))
            .await
            .unwrap();
        assert!(dir.path().join("event_FOMC_2024_01_31.json").exists());
        let second =
            fetch_event_window(&client, &event, "BTC/USDT", "1m", 15, 30, Some(dir.path()))
                .await
                .unwrap();
        assert_eq!(first, second);
        assert_eq!(first, vec![Bar::new(start_ms, 1.0, 2.0, 0.5, 1.5)]);
    }
}

//! Historical replay harness for the long-horizon top-K trend filter (port of
//! `src/trend_filter_backtest.py`).
//!
//! Every `rebalance_every_bars`, rank symbols by trailing `lookback_bars` return, hold
//! the top K equal-weighted (sells first, then buys capped to cash), pay taker fee +
//! slippage per leg. Equity is marked to market every bar.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::common::{
    self, Bar, KlineSet, fetch_klines_for_symbols, load_kline_fixture, parse_iso, py_round,
    sharpe_annualized, split_csv,
};
use crate::error::{Result, StrategyError};

/// Python's trend table adds `1w` and defaults to daily (365).
fn bars_per_year(interval: &str) -> f64 {
    if interval == "1w" {
        52.0
    } else {
        common::bars_per_year(interval, 365.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendFilterConfig {
    pub interval: String,
    pub lookback_bars: i64,
    pub top_k: i64,
    pub rebalance_every_bars: i64,
    /// Require trailing return ≥ this floor (percent).
    pub min_lookback_return_pct: f64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    pub starting_balance: f64,
}

impl Default for TrendFilterConfig {
    fn default() -> Self {
        Self {
            interval: "1d".into(),
            lookback_bars: 90,
            top_k: 5,
            rebalance_every_bars: 30,
            min_lookback_return_pct: 0.0,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            starting_balance: 10_000.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrendTrade {
    pub timestamp_ms: i64,
    pub symbol: String,
    pub side: &'static str,
    pub qty: f64,
    pub price: f64,
    pub fee_quote: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrendFilterResult {
    pub config: TrendFilterConfig,
    pub initial_balance: f64,
    pub final_equity: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub fees_paid_quote: f64,
    pub num_rebalances: usize,
    pub bars_evaluated: usize,
    pub bars_in_position_pct: f64,
    /// Insertion-ordered (first bar held) like the Python dict; serialised as an object.
    #[serde(serialize_with = "ser_held_bars")]
    pub per_symbol_held_bars: Vec<(String, usize)>,
    pub equity_curve: Vec<(i64, f64)>,
    pub trades: Vec<TrendTrade>,
}

fn ser_held_bars<S: serde::Serializer>(
    rows: &[(String, usize)],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let map: BTreeMap<&str, usize> = rows.iter().map(|(s, n)| (s.as_str(), *n)).collect();
    map.serialize(serializer)
}

impl TrendFilterResult {
    fn empty(cfg: &TrendFilterConfig) -> Self {
        Self {
            config: cfg.clone(),
            initial_balance: cfg.starting_balance,
            final_equity: cfg.starting_balance,
            total_return_pct: 0.0,
            annualized_return_pct: 0.0,
            sharpe_annualized: 0.0,
            max_drawdown_pct: 0.0,
            num_trades: 0,
            fees_paid_quote: 0.0,
            num_rebalances: 0,
            bars_evaluated: 0,
            bars_in_position_pct: 0.0,
            per_symbol_held_bars: Vec::new(),
            equity_curve: Vec::new(),
            trades: Vec::new(),
        }
    }

    pub fn verdict(&self) -> &'static str {
        common::verdict(
            self.bars_evaluated,
            self.num_trades,
            self.sharpe_annualized,
            self.total_return_pct,
        )
    }

    pub fn held_bars(&self, symbol: &str) -> usize {
        self.per_symbol_held_bars
            .iter()
            .find(|(s, _)| s == symbol)
            .map_or(0, |(_, n)| *n)
    }
}

/// Insertion-ordered `symbol → f64` map (Python dict semantics: overwrite keeps the
/// slot, delete + reinsert moves to the end). Float sums depend on this order.
#[derive(Default)]
struct Ordered(Vec<(String, f64)>);

impl Ordered {
    fn get(&self, key: &str) -> Option<f64> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    fn set(&mut self, key: &str, value: f64) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }

    fn remove(&mut self, key: &str) {
        self.0.retain(|(k, _)| k != key);
    }
}

/// Replay a top-K trend-filter portfolio against historical klines.
pub fn replay_trend_filter(
    klines: &KlineSet,
    cfg: &TrendFilterConfig,
) -> Result<TrendFilterResult> {
    if cfg.starting_balance <= 0.0 {
        return Err(StrategyError::InvalidConfig(
            "starting_balance must be positive".into(),
        ));
    }
    if cfg.top_k <= 0 {
        return Err(StrategyError::InvalidConfig(
            "top_k must be positive".into(),
        ));
    }
    if cfg.lookback_bars <= 0 {
        return Err(StrategyError::InvalidConfig(
            "lookback_bars must be positive".into(),
        ));
    }
    if cfg.rebalance_every_bars <= 0 {
        return Err(StrategyError::InvalidConfig(
            "rebalance_every_bars must be positive".into(),
        ));
    }
    let lookback = usize::try_from(cfg.lookback_bars).unwrap_or(usize::MAX);
    let rebalance_every = usize::try_from(cfg.rebalance_every_bars).unwrap_or(usize::MAX);
    let top_k = usize::try_from(cfg.top_k).unwrap_or(usize::MAX);

    let by_symbol: BTreeMap<&str, HashMap<i64, &Bar>> = klines
        .iter()
        .map(|(s, bars)| (s.as_str(), bars.iter().map(|b| (b.open_ms, b)).collect()))
        .collect();
    let mut times: Vec<i64> = by_symbol.values().flat_map(|m| m.keys().copied()).collect();
    times.sort_unstable();
    times.dedup();
    if times.len() <= lookback.saturating_add(1) {
        return Ok(TrendFilterResult::empty(cfg));
    }
    let close_at = |symbol: &str, ts: i64| {
        by_symbol
            .get(symbol)
            .and_then(|m| m.get(&ts))
            .map(|b| b.close)
    };

    let one_side_cost_rate = cfg.taker_fee_bps / 10_000.0 + cfg.slippage_bps / 10_000.0;
    let mut cash = cfg.starting_balance;
    let mut inventory = Ordered::default();
    let mut trades = Vec::new();
    let mut fees_paid_quote = 0.0;
    let mut num_rebalances = 0;
    let mut equity_curve = Vec::new();
    let mut returns = Vec::new();
    let mut held_bars: Vec<(String, usize)> = Vec::new();
    let mut last_equity = cash;
    let mut peak_equity = cash;
    let mut max_drawdown_pct = 0.0_f64;
    let mut bars_in_position = 0_usize;

    for i in lookback..times.len() {
        let ts = times[i];
        let ts_lookback = times[i - lookback];

        if (i - lookback) % rebalance_every == 0 {
            let mut mtm = 0.0;
            for (symbol, qty) in &inventory.0 {
                if let Some(px) = close_at(symbol, ts) {
                    mtm += qty * px;
                }
            }
            let equity = cash + mtm;

            let mut scored: Vec<(f64, &str)> = by_symbol
                .iter()
                .filter_map(|(symbol, bars)| {
                    let (here, past) = (bars.get(&ts)?, bars.get(&ts_lookback)?);
                    if past.close <= 0.0 {
                        return None;
                    }
                    let ret_pct = (here.close / past.close - 1.0) * 100.0;
                    (ret_pct >= cfg.min_lookback_return_pct).then_some((ret_pct, *symbol))
                })
                .collect();
            scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(a.1)));
            let targets: Vec<&str> = scored.iter().take(top_k).map(|(_, s)| *s).collect();
            let target_weight = if targets.is_empty() {
                0.0
            } else {
                1.0 / targets.len() as f64
            };
            let mut target_qty = Ordered::default();
            for symbol in &targets {
                if let Some(px) = close_at(symbol, ts).filter(|px| *px > 0.0) {
                    target_qty.set(symbol, (equity * target_weight) / px);
                }
            }

            // Sells first to free cash.
            let held: Vec<(String, f64)> = inventory.0.clone();
            for (symbol, current) in held {
                let desired = target_qty.get(&symbol).unwrap_or(0.0);
                if current <= desired {
                    continue;
                }
                let delta = current - desired;
                let Some(px) = close_at(&symbol, ts).filter(|px| *px > 0.0) else {
                    continue;
                };
                cash += delta * px * (1.0 - one_side_cost_rate);
                let fee = delta * px * one_side_cost_rate;
                fees_paid_quote += fee;
                let remaining = current - delta;
                if remaining < 1e-12 {
                    inventory.remove(&symbol);
                } else {
                    inventory.set(&symbol, remaining);
                }
                trades.push(TrendTrade {
                    timestamp_ms: ts,
                    symbol,
                    side: "sell",
                    qty: delta,
                    price: px,
                    fee_quote: fee,
                });
            }

            // Buys next, capped to available cash.
            for (symbol, desired) in &target_qty.0 {
                let current = inventory.get(symbol).unwrap_or(0.0);
                if *desired <= current {
                    continue;
                }
                let Some(px) = close_at(symbol, ts).filter(|px| *px > 0.0) else {
                    continue;
                };
                let mut delta = desired - current;
                let mut cost = delta * px * (1.0 + one_side_cost_rate);
                let mut fee = delta * px * one_side_cost_rate;
                if cost > cash + 1e-6 {
                    delta = (cash / (px * (1.0 + one_side_cost_rate))).max(0.0);
                    cost = delta * px * (1.0 + one_side_cost_rate);
                    fee = delta * px * one_side_cost_rate;
                }
                if delta <= 0.0 {
                    continue;
                }
                cash -= cost;
                fees_paid_quote += fee;
                inventory.set(symbol, current + delta);
                trades.push(TrendTrade {
                    timestamp_ms: ts,
                    symbol: symbol.clone(),
                    side: "buy",
                    qty: delta,
                    price: px,
                    fee_quote: fee,
                });
            }
            num_rebalances += 1;
        }

        let mut mtm = 0.0;
        for (symbol, qty) in &inventory.0 {
            let Some(px) = close_at(symbol, ts) else {
                continue;
            };
            mtm += qty * px;
            match held_bars.iter_mut().find(|(s, _)| s == symbol) {
                Some(slot) => slot.1 += 1,
                None => held_bars.push((symbol.clone(), 1)),
            }
        }
        if !inventory.0.is_empty() {
            bars_in_position += 1;
        }
        let equity = cash + mtm;
        if last_equity > 0.0 {
            returns.push(equity / last_equity - 1.0);
        }
        last_equity = equity;
        equity_curve.push((ts, equity));
        if equity > peak_equity {
            peak_equity = equity;
        } else if peak_equity > 0.0 {
            let dd = (peak_equity - equity) / peak_equity * 100.0;
            if dd > max_drawdown_pct {
                max_drawdown_pct = dd;
            }
        }
    }

    let bars_evaluated = returns.len();
    let bpy = bars_per_year(&cfg.interval);
    let years = if bpy > 0.0 {
        bars_evaluated as f64 / bpy
    } else {
        0.0
    };
    let annualized_return_pct = if years > 0.0 && last_equity > 0.0 {
        ((last_equity / cfg.starting_balance).powf(1.0 / years) - 1.0) * 100.0
    } else {
        0.0
    };
    Ok(TrendFilterResult {
        config: cfg.clone(),
        initial_balance: cfg.starting_balance,
        final_equity: last_equity,
        total_return_pct: (last_equity / cfg.starting_balance - 1.0) * 100.0,
        annualized_return_pct,
        sharpe_annualized: sharpe_annualized(&returns, bpy),
        max_drawdown_pct,
        num_trades: trades.len(),
        fees_paid_quote,
        num_rebalances,
        bars_evaluated,
        bars_in_position_pct: if bars_evaluated > 0 {
            bars_in_position as f64 / bars_evaluated as f64 * 100.0
        } else {
            0.0
        },
        per_symbol_held_bars: held_bars,
        equity_curve,
        trades,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub config: TrendFilterConfig,
    pub initial_balance: f64,
    pub final_equity: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub fees_paid_quote: f64,
    pub num_rebalances: usize,
    pub bars_evaluated: usize,
    pub bars_in_position_pct: f64,
    /// `[symbol, bars]` pairs, most held first (stable on ties).
    pub top_held_symbols: Vec<(String, usize)>,
}

pub fn result_summary(result: &TrendFilterResult) -> ResultSummary {
    let mut top = result.per_symbol_held_bars.clone();
    top.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    top.truncate(10);
    ResultSummary {
        verdict: result.verdict(),
        config: result.config.clone(),
        initial_balance: result.initial_balance,
        final_equity: py_round(result.final_equity, 4),
        total_return_pct: py_round(result.total_return_pct, 4),
        annualized_return_pct: py_round(result.annualized_return_pct, 4),
        sharpe_annualized: py_round(result.sharpe_annualized, 4),
        max_drawdown_pct: py_round(result.max_drawdown_pct, 4),
        num_trades: result.num_trades,
        fees_paid_quote: py_round(result.fees_paid_quote, 4),
        num_rebalances: result.num_rebalances,
        bars_evaluated: result.bars_evaluated,
        bars_in_position_pct: py_round(result.bars_in_position_pct, 2),
        top_held_symbols: top,
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long,
        default_value = "BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT,XRP/USDT,ADA/USDT,DOGE/USDT,AVAX/USDT,LINK/USDT,DOT/USDT,LTC/USDT,MATIC/USDT"
    )]
    pub symbols: String,
    #[arg(long, default_value = "1d")]
    pub interval: String,
    #[arg(long, default_value = "")]
    pub start: String,
    #[arg(long, default_value = "")]
    pub end: String,
    #[arg(long, default_value = "")]
    pub fixture: String,
    #[arg(long, default_value = "")]
    pub klines_cache: String,
    #[arg(long, default_value = "")]
    pub output: String,
    #[arg(long, default_value_t = 90, allow_negative_numbers = true)]
    pub lookback_bars: i64,
    #[arg(long, default_value_t = 5, allow_negative_numbers = true)]
    pub top_k: i64,
    #[arg(long, default_value_t = 30, allow_negative_numbers = true)]
    pub rebalance_every: i64,
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    pub min_lookback_return_pct: f64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub taker_fee_bps: f64,
    #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
    pub slippage_bps: f64,
    #[arg(long, default_value_t = 10_000.0, allow_negative_numbers = true)]
    pub starting_balance: f64,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = TrendFilterConfig {
        interval: args.interval.clone(),
        lookback_bars: args.lookback_bars,
        top_k: args.top_k,
        rebalance_every_bars: args.rebalance_every,
        min_lookback_return_pct: args.min_lookback_return_pct,
        taker_fee_bps: args.taker_fee_bps,
        slippage_bps: args.slippage_bps,
        starting_balance: args.starting_balance,
    };
    let klines = if !args.fixture.is_empty() {
        load_kline_fixture(Path::new(&args.fixture))?
    } else {
        if args.start.is_empty() {
            return Ok(crate::cli::usage_error(
                "trend-filter-backtest",
                "either --fixture or --start must be supplied",
            ));
        }
        let start = parse_iso(&args.start)?;
        let end = if args.end.is_empty() {
            bb_core::time::now()
        } else {
            parse_iso(&args.end)?
        };
        let cache_dir = (!args.klines_cache.is_empty()).then(|| PathBuf::from(&args.klines_cache));
        let client = common::public_client()?;
        let symbols = split_csv(&args.symbols);
        fetch_klines_for_symbols(
            &client,
            &symbols,
            &args.interval,
            start,
            end,
            cache_dir.as_deref(),
            6,
        )
        .await?
    };
    let result = replay_trend_filter(&klines, &cfg)?;
    common::print_json(&result_summary(&result))?;
    if !args.output.is_empty() {
        common::write_report(Path::new(&args.output), &result)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: i64 = 86_400_000;

    fn bar(i: i64, close: f64) -> Bar {
        Bar::new(i * DAY_MS, close, close, close, close)
    }

    fn trending(n: i64, bar_return: f64) -> Vec<Bar> {
        let mut price = 100.0;
        (0..n)
            .map(|i| {
                let b = bar(i, price);
                price *= 1.0 + bar_return;
                b
            })
            .collect()
    }

    fn flat(n: i64) -> Vec<Bar> {
        (0..n).map(|i| bar(i, 100.0)).collect()
    }

    fn set(entries: Vec<(&str, Vec<Bar>)>) -> KlineSet {
        entries
            .into_iter()
            .map(|(s, b)| (s.to_owned(), b))
            .collect()
    }

    fn cfg(top_k: i64, floor: f64) -> TrendFilterConfig {
        TrendFilterConfig {
            lookback_bars: 30,
            top_k,
            rebalance_every_bars: 30,
            min_lookback_return_pct: floor,
            ..TrendFilterConfig::default()
        }
    }

    #[test]
    fn holds_nothing_when_universe_is_flat() {
        let result = replay_trend_filter(
            &set(vec![("FOO/USDT", flat(200)), ("BAR/USDT", flat(200))]),
            &cfg(2, 5.0),
        )
        .unwrap();
        assert_eq!(result.num_trades, 0);
        assert_eq!(result.verdict(), "no_trades");
    }

    #[test]
    fn buys_top_k_trending_assets() {
        let klines = set(vec![
            ("FLAT/USDT", flat(200)),
            ("SLOW/USDT", trending(200, 0.001)),
            ("FAST/USDT", trending(200, 0.005)),
        ]);
        let result = replay_trend_filter(&klines, &cfg(1, 0.0)).unwrap();
        assert!(result.num_trades >= 1);
        let top = result
            .per_symbol_held_bars
            .iter()
            .max_by_key(|(_, n)| *n)
            .unwrap();
        assert_eq!(top.0, "FAST/USDT");
        assert!(result.held_bars("FAST/USDT") > result.held_bars("SLOW/USDT"));
    }

    #[test]
    fn captures_positive_returns_on_clean_trend() {
        let c = TrendFilterConfig {
            starting_balance: 1_000.0,
            ..cfg(1, 0.0)
        };
        let result =
            replay_trend_filter(&set(vec![("BTC/USDT", trending(200, 0.005))]), &c).unwrap();
        assert!(result.final_equity > result.initial_balance);
        assert!(["edge_likely", "edge_marginal"].contains(&result.verdict()));
    }

    #[test]
    fn handles_short_window_and_invalid_config() {
        let result = replay_trend_filter(&set(vec![("BTC/USDT", flat(10))]), &cfg(1, 0.0)).unwrap();
        assert_eq!(result.bars_evaluated, 0);
        assert_eq!(result.verdict(), "no_data");
        let klines = set(vec![("BTC/USDT", flat(50))]);
        let err = |c: TrendFilterConfig| replay_trend_filter(&klines, &c).unwrap_err().to_string();
        let d = TrendFilterConfig::default;
        assert!(
            err(TrendFilterConfig {
                lookback_bars: 0,
                ..d()
            })
            .contains("lookback_bars")
        );
        assert!(err(TrendFilterConfig { top_k: 0, ..d() }).contains("top_k"));
        assert!(
            err(TrendFilterConfig {
                rebalance_every_bars: 0,
                ..d()
            })
            .contains("rebalance")
        );
        assert!(
            err(TrendFilterConfig {
                starting_balance: 0.0,
                ..d()
            })
            .contains("starting_balance")
        );
    }

    #[test]
    fn summary_round_trips_and_report_shape() {
        let result =
            replay_trend_filter(&set(vec![("BTC/USDT", trending(120, 0.003))]), &cfg(1, 0.0))
                .unwrap();
        let summary: serde_json::Value =
            serde_json::from_str(&common::to_pretty_sorted(&result_summary(&result)).unwrap())
                .unwrap();
        assert_eq!(summary["verdict"], result.verdict());
        assert_eq!(
            summary["num_trades"].as_u64().unwrap() as usize,
            result.num_trades
        );
        assert_eq!(summary["top_held_symbols"][0][0], "BTC/USDT");
        let full = common::sorted_value(&result).unwrap();
        assert!(full["per_symbol_held_bars"]["BTC/USDT"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn cli_runs_against_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let rows = |bars: Vec<Bar>| -> Vec<Vec<f64>> {
            bars.iter()
                .map(|b| vec![b.open_ms as f64, b.open, b.high, b.low, b.close, 1.0])
                .collect()
        };
        let fixture = dir.path().join("klines.json");
        std::fs::write(
            &fixture,
            serde_json::json!({"FLAT/USDT": rows(flat(150)), "TREND/USDT": rows(trending(150, 0.004))}).to_string(),
        )
        .unwrap();
        let output = dir.path().join("report.json");
        let args = Args {
            symbols: String::new(),
            interval: "1d".into(),
            start: String::new(),
            end: String::new(),
            fixture: fixture.display().to_string(),
            klines_cache: String::new(),
            output: output.display().to_string(),
            lookback_bars: 30,
            top_k: 1,
            rebalance_every: 30,
            min_lookback_return_pct: 0.0,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            starting_balance: 10_000.0,
        };
        assert_eq!(run_cli(args).await.unwrap(), ExitCode::SUCCESS);
        let full: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert!(full["num_trades"].as_u64().unwrap() >= 1);
        assert!(full.get("trades").is_some());
    }
}

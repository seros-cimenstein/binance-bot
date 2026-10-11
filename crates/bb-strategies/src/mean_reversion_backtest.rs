//! Historical replay harness for short-horizon dump-reversion trades (port of
//! `src/mean_reversion_backtest.py`).
//!
//! Hypothesis: when a liquid spot asset dumps materially over a short window (e.g. more
//! than 5% in 15 minutes), buyers step in and the price retraces part of the move within
//! the hour. Enter long when the trailing return is at or below `dump_threshold_pct`;
//! exit on the retrace target (take profit), a further `stop_loss_pct` drop from entry
//! (stop loss) or after `max_hold_bars` (time exit). One position per symbol, no
//! pyramiding. A positive verdict here justifies the live runtime
//! ([`crate::mean_reversion`]).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::common::{
    self, Bar, KlineSet, bars_per_year, fetch_klines_for_symbols, parse_iso, py_round, py_sum,
    sharpe_annualized, split_csv,
};
use crate::error::{Result, StrategyError};

/// `[(symbol, klines)]` in Python dict insertion order: the entry scan walks symbols in
/// this order, so it decides which dumps win the free slots.
pub type OrderedKlines = Vec<(String, Vec<Bar>)>;

/// Dump-reversion knobs: enter on 15m drops of at least 5%, take profit once 50% of the
/// drop is recovered, stop out on a further 3% drop from entry, time-exit after 60
/// minutes; 10 bps taker fee + 5 bps slippage per side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeanReversionConfig {
    pub interval: String,
    pub dump_lookback_bars: i64,
    /// Enter when the lookback return is at or below this.
    pub dump_threshold_pct: f64,
    /// Fraction of the drop to recover.
    pub retrace_target_pct: f64,
    /// Additional drop from entry that stops the trade out.
    pub stop_loss_pct: f64,
    pub max_hold_bars: i64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    pub starting_balance: f64,
    pub notional_per_position: f64,
    pub max_concurrent_positions: i64,
}

impl Default for MeanReversionConfig {
    fn default() -> Self {
        Self {
            interval: "1m".into(),
            dump_lookback_bars: 15,
            dump_threshold_pct: -5.0,
            retrace_target_pct: 0.5,
            stop_loss_pct: -3.0,
            max_hold_bars: 60,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            starting_balance: 1_000.0,
            notional_per_position: 100.0,
            max_concurrent_positions: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    StopLoss,
    TakeProfit,
    MaxHold,
    WindowEnd,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StopLoss => "stop_loss",
            Self::TakeProfit => "take_profit",
            Self::MaxHold => "max_hold",
            Self::WindowEnd => "window_end",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReversionTrade {
    pub symbol: String,
    pub entry_time_ms: i64,
    pub exit_time_ms: i64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub notional_usdt: f64,
    pub dump_pct: f64,
    pub bars_held: i64,
    pub pnl_quote: f64,
    pub fee_quote: f64,
    pub exit_reason: ExitReason,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeanReversionResult {
    pub config: MeanReversionConfig,
    pub initial_balance: f64,
    pub final_equity: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub win_rate_pct: f64,
    pub avg_bars_held: f64,
    pub fees_paid_quote: f64,
    pub avg_pnl_quote: f64,
    pub bars_evaluated: usize,
    pub bars_in_position_pct: f64,
    pub by_symbol_trade_count: BTreeMap<String, usize>,
    pub by_exit_reason: BTreeMap<String, usize>,
    pub equity_curve: Vec<(i64, f64)>,
    pub trades: Vec<ReversionTrade>,
}

impl MeanReversionResult {
    fn empty(cfg: &MeanReversionConfig) -> Self {
        Self {
            config: cfg.clone(),
            initial_balance: cfg.starting_balance,
            final_equity: cfg.starting_balance,
            total_return_pct: 0.0,
            annualized_return_pct: 0.0,
            sharpe_annualized: 0.0,
            max_drawdown_pct: 0.0,
            num_trades: 0,
            win_rate_pct: 0.0,
            avg_bars_held: 0.0,
            fees_paid_quote: 0.0,
            avg_pnl_quote: 0.0,
            bars_evaluated: 0,
            bars_in_position_pct: 0.0,
            by_symbol_trade_count: BTreeMap::new(),
            by_exit_reason: BTreeMap::new(),
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
}

struct OpenPosition {
    /// Index into the replay's symbol list.
    symbol: usize,
    entry_time_ms: i64,
    entry_price: f64,
    qty: f64,
    dump_pct: f64,
    notional_usdt: f64,
    entry_fee: f64,
    /// Notional + entry fee, deducted from cash at entry.
    entry_equity_committed: f64,
    bars_held: i64,
}

impl OpenPosition {
    /// Close at `exit_price`: `(trade, proceeds returned to cash, exit-side fee)`.
    fn close(
        &self,
        symbol: &str,
        ts: i64,
        exit_price: f64,
        reason: ExitReason,
        cost_rate: f64,
    ) -> (ReversionTrade, f64, f64) {
        let proceeds = self.qty * exit_price * (1.0 - cost_rate);
        let exit_fee = self.qty * exit_price * cost_rate;
        let trade = ReversionTrade {
            symbol: symbol.to_owned(),
            entry_time_ms: self.entry_time_ms,
            exit_time_ms: ts,
            entry_price: self.entry_price,
            exit_price,
            notional_usdt: self.notional_usdt,
            dump_pct: self.dump_pct,
            bars_held: self.bars_held,
            pnl_quote: proceeds - self.entry_equity_committed,
            fee_quote: exit_fee + self.entry_fee,
            exit_reason: reason,
        };
        (trade, proceeds, exit_fee)
    }
}

fn invalid(message: &str) -> StrategyError {
    StrategyError::InvalidConfig(message.to_owned())
}

/// Replay dump-reversion trades over every symbol on the union-of-open-times grid.
pub fn replay_mean_reversion(
    klines: &[(String, Vec<Bar>)],
    cfg: &MeanReversionConfig,
) -> Result<MeanReversionResult> {
    if cfg.starting_balance <= 0.0 {
        return Err(invalid("starting_balance must be positive"));
    }
    if cfg.notional_per_position <= 0.0 {
        return Err(invalid("notional_per_position must be positive"));
    }
    if cfg.dump_lookback_bars <= 0 {
        return Err(invalid("dump_lookback_bars must be positive"));
    }
    if cfg.dump_threshold_pct >= 0.0 {
        return Err(invalid("dump_threshold_pct must be negative"));
    }
    if cfg.max_hold_bars <= 0 {
        return Err(invalid("max_hold_bars must be positive"));
    }
    if cfg.max_concurrent_positions <= 0 {
        return Err(invalid("max_concurrent_positions must be positive"));
    }

    // Per-symbol `open_ms → bar` (later duplicates win) + the sorted union of open times.
    let indexed: Vec<HashMap<i64, Bar>> = klines
        .iter()
        .map(|(_, bars)| bars.iter().map(|b| (b.open_ms, *b)).collect())
        .collect();
    let mut times: Vec<i64> = indexed.iter().flat_map(|m| m.keys().copied()).collect();
    times.sort_unstable();
    times.dedup();

    let lookback = usize::try_from(cfg.dump_lookback_bars).unwrap_or(usize::MAX);
    let max_concurrent = usize::try_from(cfg.max_concurrent_positions).unwrap_or(usize::MAX);
    let Some(&final_ts) = times.last() else {
        return Ok(MeanReversionResult::empty(cfg));
    };
    if times.len() <= lookback.saturating_add(1) {
        return Ok(MeanReversionResult::empty(cfg));
    }

    let cost_rate = cfg.taker_fee_bps / 10_000.0 + cfg.slippage_bps / 10_000.0;
    let mut equity = cfg.starting_balance;
    let mut open: Vec<OpenPosition> = Vec::new();
    let mut trades: Vec<ReversionTrade> = Vec::new();
    let mut by_symbol_count: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_exit: BTreeMap<String, usize> = BTreeMap::new();
    let mut equity_curve: Vec<(i64, f64)> = Vec::with_capacity(times.len());
    let mut returns: Vec<f64> = Vec::with_capacity(times.len());
    let mut peak_equity = equity;
    let mut max_drawdown_pct = 0.0_f64;
    let mut fees_paid_quote = 0.0;
    let mut last_equity = equity;
    let mut bars_in_position = 0_usize;

    let mut record = |trade: ReversionTrade, trades: &mut Vec<ReversionTrade>| {
        *by_symbol_count.entry(trade.symbol.clone()).or_insert(0) += 1;
        *by_exit
            .entry(trade.exit_reason.as_str().to_owned())
            .or_insert(0) += 1;
        trades.push(trade);
    };

    for (i, &ts) in times.iter().enumerate().skip(lookback) {
        let ts_back = times[i - lookback];

        // 1. Exits on open positions (insertion order).
        let mut kept = Vec::with_capacity(open.len());
        for mut pos in open.drain(..) {
            let Some(bar) = indexed[pos.symbol].get(&ts) else {
                kept.push(pos);
                continue;
            };
            let target_price =
                pos.entry_price * (1.0 + pos.dump_pct.abs() * cfg.retrace_target_pct / 100.0);
            let stop_price = pos.entry_price * (1.0 + cfg.stop_loss_pct / 100.0);
            let exit = if bar.low <= stop_price {
                Some((ExitReason::StopLoss, stop_price))
            } else if bar.high >= target_price {
                Some((ExitReason::TakeProfit, target_price))
            } else if pos.bars_held >= cfg.max_hold_bars {
                Some((ExitReason::MaxHold, bar.close))
            } else {
                None
            };
            match exit {
                Some((reason, exit_price)) => {
                    let (trade, proceeds, exit_fee) =
                        pos.close(&klines[pos.symbol].0, ts, exit_price, reason, cost_rate);
                    // Cost basis left cash at entry; only the proceeds return.
                    equity += proceeds;
                    fees_paid_quote += exit_fee;
                    record(trade, &mut trades);
                }
                None => {
                    pos.bars_held += 1;
                    kept.push(pos);
                }
            }
        }
        open = kept;

        // 2. New entries while capacity remains (symbol order decides who gets a slot).
        if open.len() < max_concurrent {
            for (symbol, per_symbol) in indexed.iter().enumerate() {
                if open.iter().any(|p| p.symbol == symbol) {
                    continue;
                }
                let (Some(here), Some(past)) = (per_symbol.get(&ts), per_symbol.get(&ts_back))
                else {
                    continue;
                };
                if past.close <= 0.0 {
                    continue;
                }
                let ret_pct = (here.close / past.close - 1.0) * 100.0;
                if ret_pct > cfg.dump_threshold_pct {
                    continue;
                }
                // Enter long at this bar's close (a zero close would divide by zero in Python).
                let entry_price = here.close;
                if entry_price <= 0.0 {
                    continue;
                }
                let qty = cfg.notional_per_position / entry_price;
                let entry_fee = cfg.notional_per_position * cost_rate;
                // Commit the notional (capital is held until exit).
                let cost_basis = cfg.notional_per_position + entry_fee;
                if cost_basis > equity {
                    continue;
                }
                equity -= cost_basis;
                fees_paid_quote += entry_fee;
                open.push(OpenPosition {
                    symbol,
                    entry_time_ms: ts,
                    entry_price,
                    qty,
                    dump_pct: ret_pct,
                    notional_usdt: cfg.notional_per_position,
                    entry_fee,
                    entry_equity_committed: cost_basis,
                    bars_held: 0,
                });
                if open.len() >= max_concurrent {
                    break;
                }
            }
        }

        // 3. Mark equity (cash + open position MTM; committed capital when a bar is missing).
        let mut mtm = 0.0;
        for pos in &open {
            match indexed[pos.symbol].get(&ts) {
                Some(bar) => mtm += pos.qty * bar.close,
                None => mtm += pos.entry_equity_committed,
            }
        }
        if !open.is_empty() {
            bars_in_position += 1;
        }
        let total_equity = equity + mtm;
        returns.push(if last_equity > 0.0 {
            total_equity / last_equity - 1.0
        } else {
            0.0
        });
        last_equity = total_equity;
        equity_curve.push((ts, total_equity));
        if total_equity > peak_equity {
            peak_equity = total_equity;
        } else if peak_equity > 0.0 {
            let drawdown_pct = (peak_equity - total_equity) / peak_equity * 100.0;
            if drawdown_pct > max_drawdown_pct {
                max_drawdown_pct = drawdown_pct;
            }
        }
    }

    // Force-close anything still open at the final bar (entry price when it has no bar).
    for pos in open.drain(..) {
        let exit_price = indexed[pos.symbol]
            .get(&final_ts)
            .map_or(pos.entry_price, |b| b.close);
        let (trade, proceeds, exit_fee) = pos.close(
            &klines[pos.symbol].0,
            final_ts,
            exit_price,
            ExitReason::WindowEnd,
            cost_rate,
        );
        equity += proceeds;
        fees_paid_quote += exit_fee;
        record(trade, &mut trades);
    }

    let bars_evaluated = returns.len();
    let total_return_pct = (equity / cfg.starting_balance - 1.0) * 100.0;
    let bpy = bars_per_year(&cfg.interval, 525_600.0);
    let years = if bpy > 0.0 {
        bars_evaluated as f64 / bpy
    } else {
        0.0
    };
    let annualized_return_pct = if years > 0.0 && equity > 0.0 {
        ((equity / cfg.starting_balance).powf(1.0 / years) - 1.0) * 100.0
    } else {
        0.0
    };
    let wins = trades.iter().filter(|t| t.pnl_quote > 0.0).count();
    let losses = trades.iter().filter(|t| t.pnl_quote < 0.0).count();
    let decisive = wins + losses;
    let win_rate_pct = if decisive > 0 {
        wins as f64 / decisive as f64 * 100.0
    } else {
        0.0
    };
    let (avg_bars_held, avg_pnl_quote) = if trades.is_empty() {
        (0.0, 0.0)
    } else {
        let n = trades.len() as f64;
        (
            trades.iter().map(|t| t.bars_held).sum::<i64>() as f64 / n,
            py_sum(trades.iter().map(|t| t.pnl_quote)) / n,
        )
    };
    let bars_in_position_pct = if bars_evaluated > 0 {
        bars_in_position as f64 / bars_evaluated as f64 * 100.0
    } else {
        0.0
    };

    Ok(MeanReversionResult {
        config: cfg.clone(),
        initial_balance: cfg.starting_balance,
        final_equity: equity,
        total_return_pct,
        annualized_return_pct,
        sharpe_annualized: sharpe_annualized(&returns, bpy),
        max_drawdown_pct,
        num_trades: trades.len(),
        win_rate_pct,
        avg_bars_held,
        fees_paid_quote,
        avg_pnl_quote,
        bars_evaluated,
        bars_in_position_pct,
        by_symbol_trade_count: by_symbol_count,
        by_exit_reason: by_exit,
        equity_curve,
        trades,
    })
}

/// Compact CLI summary (Python `_result_summary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub config: MeanReversionConfig,
    pub initial_balance: f64,
    pub final_equity: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub win_rate_pct: f64,
    pub avg_bars_held: f64,
    pub fees_paid_quote: f64,
    pub avg_pnl_quote: f64,
    pub bars_evaluated: usize,
    pub bars_in_position_pct: f64,
    pub by_exit_reason: BTreeMap<String, usize>,
    pub by_symbol_trade_count: BTreeMap<String, usize>,
}

pub fn result_summary(result: &MeanReversionResult) -> ResultSummary {
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
        win_rate_pct: py_round(result.win_rate_pct, 2),
        avg_bars_held: py_round(result.avg_bars_held, 2),
        fees_paid_quote: py_round(result.fees_paid_quote, 4),
        avg_pnl_quote: py_round(result.avg_pnl_quote, 6),
        bars_evaluated: result.bars_evaluated,
        bars_in_position_pct: py_round(result.bars_in_position_pct, 2),
        by_exit_reason: result.by_exit_reason.clone(),
        by_symbol_trade_count: result.by_symbol_trade_count.clone(),
    }
}

/// `{symbol: [kline, ...]}` kept in document order (a repeated key keeps its first
/// position and its last value, like a Python dict).
struct OrderedFixture(OrderedKlines);

impl<'de> Deserialize<'de> for OrderedFixture {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct FixtureVisitor;
        impl<'de> Visitor<'de> for FixtureVisitor {
            type Value = OrderedFixture;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a mapping of symbol -> klines")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<OrderedFixture, A::Error> {
                let mut out: OrderedKlines = Vec::new();
                while let Some((symbol, bars)) = map.next_entry::<String, Vec<Bar>>()? {
                    match out.iter_mut().find(|(s, _)| *s == symbol) {
                        Some(slot) => slot.1 = bars,
                        None => out.push((symbol, bars)),
                    }
                }
                Ok(OrderedFixture(out))
            }
        }
        deserializer.deserialize_map(FixtureVisitor)
    }
}

/// Python `_load_fixture`: `{symbol: [kline, ...]}` in file order.
pub fn load_fixture(path: &Path) -> Result<OrderedKlines> {
    let text = std::fs::read_to_string(path).map_err(|e| StrategyError::io(path, e))?;
    let fixture: OrderedFixture =
        serde_json::from_str(&text).map_err(|e| common::json_error(path, &e))?;
    Ok(fixture.0)
}

/// Fetched klines in `--symbols` order (first occurrence wins for repeats).
fn in_symbol_order(symbols: &[String], mut fetched: KlineSet) -> OrderedKlines {
    symbols
        .iter()
        .filter_map(|symbol| fetched.remove(symbol).map(|bars| (symbol.clone(), bars)))
        .collect()
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long,
        default_value = "BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT,XRP/USDT,ADA/USDT,DOGE/USDT,AVAX/USDT"
    )]
    pub symbols: String,
    #[arg(long, default_value = "1m")]
    pub interval: String,
    /// ISO start timestamp; required unless --fixture is supplied.
    #[arg(long, default_value = "")]
    pub start: String,
    /// ISO end timestamp; defaults to now.
    #[arg(long, default_value = "")]
    pub end: String,
    /// JSON `{symbol: [kline, ...]}` file for offline runs.
    #[arg(long, default_value = "")]
    pub fixture: String,
    /// Directory to cache fetched klines.
    #[arg(long, default_value = "")]
    pub klines_cache: String,
    /// Optional path to dump the full report JSON.
    #[arg(long, default_value = "")]
    pub output: String,
    #[arg(long, default_value_t = 15, allow_negative_numbers = true)]
    pub dump_lookback_bars: i64,
    #[arg(long, default_value_t = -5.0, allow_negative_numbers = true)]
    pub dump_threshold_pct: f64,
    #[arg(long, default_value_t = 0.5, allow_negative_numbers = true)]
    pub retrace_target_pct: f64,
    #[arg(long, default_value_t = -3.0, allow_negative_numbers = true)]
    pub stop_loss_pct: f64,
    #[arg(long, default_value_t = 60, allow_negative_numbers = true)]
    pub max_hold_bars: i64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub taker_fee_bps: f64,
    #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
    pub slippage_bps: f64,
    #[arg(long, default_value_t = 1_000.0, allow_negative_numbers = true)]
    pub starting_balance: f64,
    #[arg(long, default_value_t = 100.0, allow_negative_numbers = true)]
    pub notional_per_position: f64,
    #[arg(long, default_value_t = 5, allow_negative_numbers = true)]
    pub max_concurrent_positions: i64,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = MeanReversionConfig {
        interval: args.interval.clone(),
        dump_lookback_bars: args.dump_lookback_bars,
        dump_threshold_pct: args.dump_threshold_pct,
        retrace_target_pct: args.retrace_target_pct,
        stop_loss_pct: args.stop_loss_pct,
        max_hold_bars: args.max_hold_bars,
        taker_fee_bps: args.taker_fee_bps,
        slippage_bps: args.slippage_bps,
        starting_balance: args.starting_balance,
        notional_per_position: args.notional_per_position,
        max_concurrent_positions: args.max_concurrent_positions,
    };
    let klines = if !args.fixture.is_empty() {
        load_fixture(Path::new(&args.fixture))?
    } else {
        if args.start.is_empty() {
            return Ok(crate::cli::usage_error(
                "mean-reversion-backtest",
                "either --fixture or --start must be supplied",
            ));
        }
        let start = parse_iso(&args.start)?;
        let end = if args.end.is_empty() {
            bb_core::time::now()
        } else {
            parse_iso(&args.end)?
        };
        let symbols = split_csv(&args.symbols);
        let cache_dir = (!args.klines_cache.is_empty()).then(|| PathBuf::from(&args.klines_cache));
        let client = common::public_client()?;
        let fetched = fetch_klines_for_symbols(
            &client,
            &symbols,
            &args.interval,
            start,
            end,
            cache_dir.as_deref(),
            6,
        )
        .await?;
        in_symbol_order(&symbols, fetched)
    };
    let result = replay_mean_reversion(&klines, &cfg)?;
    common::print_json(&result_summary(&result))?;
    if !args.output.is_empty() {
        common::write_report(Path::new(&args.output), &result)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE_MS: i64 = 60_000;

    fn bar(i: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar::new(i * MINUTE_MS, o, h, l, c)
    }

    fn flat(n: i64, level: f64) -> Vec<Bar> {
        (0..n).map(|i| bar(i, level, level, level, level)).collect()
    }

    fn one(symbol: &str, bars: Vec<Bar>) -> OrderedKlines {
        vec![(symbol.to_owned(), bars)]
    }

    fn cfg(stop_loss_pct: f64, max_concurrent_positions: i64) -> MeanReversionConfig {
        MeanReversionConfig {
            stop_loss_pct,
            max_concurrent_positions,
            ..MeanReversionConfig::default()
        }
    }

    /// Flat at 100 for `flat_bars`, then a 15-bar slide to 94, then `tail` bars at 94.
    fn slide_series(flat_bars: i64, tail: i64, low_offset: f64, high_offset: f64) -> Vec<Bar> {
        let mut bars: Vec<Bar> = (0..flat_bars)
            .map(|i| bar(i, 100.0, 100.0, 100.0, 100.0))
            .collect();
        for i in 0..15 {
            let price = 100.0 - (6.0 * (i + 1) as f64 / 15.0);
            bars.push(bar(
                flat_bars + i,
                price + high_offset,
                price + high_offset,
                price - low_offset,
                price,
            ));
        }
        bars.extend((0..tail).map(|i| bar(flat_bars + 15 + i, 94.0, 94.0, 94.0, 94.0)));
        bars
    }

    #[test]
    fn no_trades_on_flat_market() {
        let result = replay_mean_reversion(
            &one("BTC/USDT", flat(120, 100.0)),
            &MeanReversionConfig::default(),
        )
        .unwrap();
        assert_eq!(result.num_trades, 0);
    }

    #[test]
    fn enters_on_dump_then_take_profit_on_retrace() {
        let mut bars: Vec<Bar> = (0..30)
            .map(|i| bar(i, 100.0, 100.0, 100.0, 100.0))
            .collect();
        bars.push(bar(30, 100.0, 100.0, 94.0, 94.0));
        bars.push(bar(31, 94.0, 97.0, 94.0, 97.0));
        bars.extend((32..50).map(|i| bar(i, 97.0, 97.0, 97.0, 97.0)));
        let result = replay_mean_reversion(&one("BTC/USDT", bars), &cfg(-10.0, 5)).unwrap();
        assert_eq!(result.num_trades, 1);
        assert_eq!(result.trades[0].exit_reason, ExitReason::TakeProfit);
        assert!(result.trades[0].pnl_quote > 0.0);
        assert_eq!(result.by_exit_reason.get("take_profit"), Some(&1));
    }

    #[test]
    fn stops_out_on_further_dump() {
        let mut bars: Vec<Bar> = (0..20)
            .map(|i| bar(i, 100.0, 100.0, 100.0, 100.0))
            .collect();
        bars.push(bar(20, 100.0, 100.0, 94.0, 94.0));
        bars.push(bar(21, 94.0, 94.0, 89.0, 89.0));
        bars.extend((22..40).map(|i| bar(i, 89.0, 89.0, 89.0, 89.0)));
        let result = replay_mean_reversion(&one("BTC/USDT", bars), &cfg(-3.0, 1)).unwrap();
        let first = &result.trades[0];
        assert_eq!(first.exit_reason, ExitReason::StopLoss);
        assert!(first.pnl_quote < 0.0);
    }

    #[test]
    fn time_exit_when_neither_target_nor_stop_hit() {
        let result = replay_mean_reversion(
            &one("BTC/USDT", slide_series(20, 70, 0.1, 0.1)),
            &MeanReversionConfig::default(),
        )
        .unwrap();
        assert_eq!(result.num_trades, 1);
        assert_eq!(result.trades[0].exit_reason, ExitReason::MaxHold);
    }

    #[test]
    fn max_concurrent_positions_capped() {
        let klines = vec![
            ("A/USDT".to_owned(), slide_series(20, 60, 0.1, 0.0)),
            ("B/USDT".to_owned(), slide_series(20, 60, 0.1, 0.0)),
        ];
        let result = replay_mean_reversion(&klines, &cfg(-3.0, 1)).unwrap();
        assert!(
            result
                .trades
                .iter()
                .all(|t| t.symbol == "A/USDT" || t.symbol == "B/USDT")
        );
        assert!(result.by_symbol_trade_count.values().all(|n| *n <= 1));
    }

    #[test]
    fn entry_scan_follows_insertion_order() {
        // Both dump identically and capacity is one slot: the first-listed symbol wins.
        let klines = vec![
            ("Z/USDT".to_owned(), slide_series(20, 60, 0.1, 0.0)),
            ("A/USDT".to_owned(), slide_series(20, 60, 0.1, 0.0)),
        ];
        let result = replay_mean_reversion(&klines, &cfg(-3.0, 1)).unwrap();
        assert_eq!(result.trades[0].symbol, "Z/USDT");
    }

    #[test]
    fn rejects_invalid_config() {
        let klines = one("A/USDT", flat(50, 100.0));
        let err =
            |c: MeanReversionConfig| replay_mean_reversion(&klines, &c).unwrap_err().to_string();
        let d = MeanReversionConfig::default;
        assert!(
            err(MeanReversionConfig {
                starting_balance: 0.0,
                ..d()
            })
            .contains("starting_balance")
        );
        assert!(
            err(MeanReversionConfig {
                dump_threshold_pct: 5.0,
                ..d()
            })
            .contains("dump_threshold_pct")
        );
        assert!(
            err(MeanReversionConfig {
                dump_lookback_bars: 0,
                ..d()
            })
            .contains("dump_lookback_bars")
        );
        assert!(
            err(MeanReversionConfig {
                max_hold_bars: 0,
                ..d()
            })
            .contains("max_hold_bars")
        );
    }

    #[test]
    fn summary_round_trips_as_json() {
        let result = replay_mean_reversion(
            &one("BTC/USDT", flat(100, 100.0)),
            &MeanReversionConfig::default(),
        )
        .unwrap();
        let decoded: serde_json::Value =
            serde_json::from_str(&common::to_pretty_sorted(&result_summary(&result)).unwrap())
                .unwrap();
        assert_eq!(decoded["verdict"], result.verdict());
        assert_eq!(
            decoded["num_trades"].as_u64().unwrap() as usize,
            result.num_trades
        );
    }

    #[test]
    fn short_window_returns_no_data_safely() {
        let result = replay_mean_reversion(
            &one("BTC/USDT", flat(5, 100.0)),
            &MeanReversionConfig::default(),
        )
        .unwrap();
        assert_eq!(result.verdict(), "no_data");
        assert_eq!(result.final_equity, result.initial_balance);
    }

    #[test]
    fn zero_close_dump_is_skipped() {
        // Python divides by the zero entry price here (ZeroDivisionError).
        let mut bars = flat(30, 100.0);
        bars.push(bar(30, 100.0, 100.0, 0.0, 0.0));
        let result =
            replay_mean_reversion(&one("BTC/USDT", bars), &MeanReversionConfig::default()).unwrap();
        assert_eq!(result.num_trades, 0);
    }

    #[test]
    fn fixture_keeps_document_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("klines.json");
        std::fs::write(&path, r#"{"Z/USDT": [[0, 1, 1, 1, 1]], "A/USDT": [[0, 2, 2, 2, 2]], "Z/USDT": [[0, 3, 3, 3, 3]]}"#)
            .unwrap();
        let klines = load_fixture(&path).unwrap();
        assert_eq!(klines.len(), 2);
        assert_eq!(klines[0].0, "Z/USDT");
        assert_eq!(klines[0].1[0].close, 3.0);
        assert_eq!(klines[1].0, "A/USDT");
        let fetched: KlineSet = [
            ("A/USDT".to_owned(), flat(1, 1.0)),
            ("Z/USDT".to_owned(), flat(1, 2.0)),
        ]
        .into();
        let symbols = vec![
            "Z/USDT".to_owned(),
            "A/USDT".to_owned(),
            "Z/USDT".to_owned(),
        ];
        let ordered = in_symbol_order(&symbols, fetched);
        assert_eq!(
            ordered.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            ["Z/USDT", "A/USDT"]
        );
    }

    #[tokio::test]
    async fn cli_runs_against_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("klines.json");
        let rows: Vec<Vec<f64>> = slide_series(20, 70, 0.1, 0.1)
            .iter()
            .map(|b| vec![b.open_ms as f64, b.open, b.high, b.low, b.close, 1.0])
            .collect();
        std::fs::write(&fixture, serde_json::json!({"BTC/USDT": rows}).to_string()).unwrap();
        let output = dir.path().join("report.json");
        let args = Args {
            symbols: String::new(),
            interval: "1m".into(),
            start: String::new(),
            end: String::new(),
            fixture: fixture.display().to_string(),
            klines_cache: String::new(),
            output: output.display().to_string(),
            dump_lookback_bars: 15,
            dump_threshold_pct: -5.0,
            retrace_target_pct: 0.5,
            stop_loss_pct: -3.0,
            max_hold_bars: 60,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            starting_balance: 1_000.0,
            notional_per_position: 100.0,
            max_concurrent_positions: 5,
        };
        assert_eq!(run_cli(args).await.unwrap(), ExitCode::SUCCESS);
        let full: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(full["trades"][0]["exit_reason"], "max_hold");
        assert_eq!(full["by_exit_reason"]["max_hold"], 1);
        assert!(full["equity_curve"][0].is_array());
    }
}

//! Historical replay harness for spread-reversion pairs trading (port of
//! `src/stat_arb_backtest.py`).
//!
//! Rolling z-score of the log price ratio `z = (log(A/B) - mean) / pstdev` over a lookback
//! window. `z > +entry`: short A, long B; `z < -entry`: long A, short B. Close when `|z|`
//! reverts inside the exit band, blows past the stop, or the max hold elapses. Both legs
//! are USD-notional matched (delta-neutral at entry); short borrow/funding is a flat
//! per-bar drag in bps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::common::{
    self, KlineSet, bars_per_year, fetch_klines_for_symbols, fmean, load_kline_fixture, parse_iso,
    pstdev, py_float_repr, py_round, sharpe_annualized,
};
use crate::error::{Result, StrategyError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatArbConfig {
    pub interval: String,
    /// ~1 week of hourly data.
    pub lookback_bars: i64,
    pub entry_z: f64,
    pub exit_z: f64,
    pub stop_z: f64,
    pub max_hold_bars: i64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    /// Spot-only mode = 0; perp-short mode = funding cost.
    pub short_borrow_drag_bps_per_bar: f64,
    /// USDT per leg.
    pub notional_per_position: f64,
    pub starting_balance: f64,
}

impl Default for StatArbConfig {
    fn default() -> Self {
        Self {
            interval: "1h".into(),
            lookback_bars: 168,
            entry_z: 2.0,
            exit_z: 0.3,
            stop_z: 4.0,
            max_hold_bars: 168,
            taker_fee_bps: 10.0,
            slippage_bps: 3.0,
            short_borrow_drag_bps_per_bar: 0.0,
            notional_per_position: 1_000.0,
            starting_balance: 5_000.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatArbTrade {
    pub symbol_long: String,
    pub symbol_short: String,
    pub open_time_ms: i64,
    pub close_time_ms: i64,
    pub entry_z: f64,
    pub exit_z: f64,
    pub bars_held: i64,
    pub pnl_quote: f64,
    pub fees_quote: f64,
    pub exit_reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatArbResult {
    pub config: StatArbConfig,
    pub pair: (String, String),
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
    pub bars_evaluated: usize,
    pub bars_in_position_pct: f64,
    pub equity_curve: Vec<(i64, f64)>,
    pub trades: Vec<StatArbTrade>,
}

impl StatArbResult {
    fn empty(cfg: &StatArbConfig, pair: (String, String)) -> Self {
        Self {
            config: cfg.clone(),
            pair,
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
            bars_evaluated: 0,
            bars_in_position_pct: 0.0,
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

/// Common open times (ascending) with `(close_a, close_b)`; later duplicates win.
fn align_two_series(a: &[common::Bar], b: &[common::Bar]) -> Vec<(i64, f64, f64)> {
    let indexed_a: std::collections::HashMap<i64, f64> =
        a.iter().map(|k| (k.open_ms, k.close)).collect();
    let indexed_b: std::collections::HashMap<i64, f64> =
        b.iter().map(|k| (k.open_ms, k.close)).collect();
    let mut aligned: Vec<(i64, f64, f64)> = indexed_a
        .iter()
        .filter_map(|(ts, pa)| indexed_b.get(ts).map(|pb| (*ts, *pa, *pb)))
        .collect();
    aligned.sort_unstable_by_key(|(ts, _, _)| *ts);
    aligned
}

/// Python `math.log(pa / pb)`, which raises on a zero divisor or a non-positive ratio.
fn log_ratio(ts: i64, pa: f64, pb: f64) -> Result<f64> {
    if pb == 0.0 {
        return Err(StrategyError::InvalidConfig(format!(
            "float division by zero (price ratio at {ts})"
        )));
    }
    let ratio = pa / pb;
    if ratio <= 0.0 {
        return Err(StrategyError::InvalidConfig(format!(
            "math domain error (price ratio {ratio} at {ts})"
        )));
    }
    Ok(ratio.ln())
}

struct OpenPosition {
    open_time_ms: i64,
    entry_z: f64,
    /// Long leg is symbol A (else B); the short leg is the other one.
    long_is_a: bool,
    long_qty: f64,
    short_qty: f64,
    entry_prices: (f64, f64),
    open_fees: f64,
    accrued_drag: f64,
    bars_held: i64,
    entry_equity: f64,
}

impl OpenPosition {
    fn mtm_pnl(&self, pa_now: f64, pb_now: f64) -> f64 {
        let (pa_open, pb_open) = self.entry_prices;
        let long_pnl = if self.long_is_a {
            self.long_qty * (pa_now - pa_open)
        } else {
            self.long_qty * (pb_now - pb_open)
        };
        let short_pnl = if self.long_is_a {
            -self.short_qty * (pb_now - pb_open)
        } else {
            -self.short_qty * (pa_now - pa_open)
        };
        long_pnl + short_pnl
    }

    /// Close (one side of fees on each leg); returns the trade and the post-close equity.
    fn close(
        &self,
        pair: (&str, &str),
        ts: i64,
        mtm_pnl: f64,
        exit_z: f64,
        close_fees: f64,
        exit_reason: String,
    ) -> (StatArbTrade, f64) {
        let final_pnl = mtm_pnl - self.accrued_drag - self.open_fees - close_fees;
        let (long, short) = if self.long_is_a {
            pair
        } else {
            (pair.1, pair.0)
        };
        let trade = StatArbTrade {
            symbol_long: long.to_owned(),
            symbol_short: short.to_owned(),
            open_time_ms: self.open_time_ms,
            close_time_ms: ts,
            entry_z: self.entry_z,
            exit_z,
            bars_held: self.bars_held,
            pnl_quote: final_pnl,
            fees_quote: self.open_fees + close_fees + self.accrued_drag,
            exit_reason,
        };
        (trade, self.entry_equity + final_pnl)
    }
}

fn close_reason(z: f64, bars_held: i64, cfg: &StatArbConfig) -> Option<String> {
    if z.abs() <= cfg.exit_z {
        Some(format!(
            "z={z:+.2} reverted within exit band ({})",
            py_float_repr(cfg.exit_z)
        ))
    } else if z.abs() >= cfg.stop_z {
        Some(format!(
            "z={z:+.2} hit stop ({})",
            py_float_repr(cfg.stop_z)
        ))
    } else if bars_held >= cfg.max_hold_bars {
        Some(format!("max hold {} bars reached", cfg.max_hold_bars))
    } else {
        None
    }
}

/// Replay one pair from a two-symbol kline set (legs ordered by symbol: A < B).
pub fn replay_pair(klines: &KlineSet, cfg: &StatArbConfig) -> Result<StatArbResult> {
    if cfg.starting_balance <= 0.0 {
        return Err(StrategyError::InvalidConfig(
            "starting_balance must be positive".into(),
        ));
    }
    if cfg.notional_per_position <= 0.0 {
        return Err(StrategyError::InvalidConfig(
            "notional_per_position must be positive".into(),
        ));
    }
    if cfg.lookback_bars <= 1 {
        return Err(StrategyError::InvalidConfig(
            "lookback_bars must be > 1".into(),
        ));
    }
    if cfg.entry_z <= cfg.exit_z {
        return Err(StrategyError::InvalidConfig(
            "entry_z must be strictly greater than exit_z".into(),
        ));
    }
    // KlineSet is a BTreeMap: iteration order is Python's `sorted(keys)`.
    let mut entries = klines.iter();
    let ((sa, bars_a), (sb, bars_b)) = match (entries.next(), entries.next(), entries.next()) {
        (Some(a), Some(b), None) => (a, b),
        _ => {
            return Err(StrategyError::InvalidConfig(
                "replay_pair requires exactly two symbols".into(),
            ));
        }
    };
    let pair = (sa.as_str(), sb.as_str());
    let pair_owned = || (sa.clone(), sb.clone());

    let lookback = usize::try_from(cfg.lookback_bars).unwrap_or(usize::MAX);
    let aligned = align_two_series(bars_a, bars_b);
    if aligned.len() <= lookback.saturating_add(1) {
        return Ok(StatArbResult::empty(cfg, pair_owned()));
    }

    let one_side_cost_rate = cfg.taker_fee_bps / 10_000.0 + cfg.slippage_bps / 10_000.0;
    let leg_fees = 2.0 * cfg.notional_per_position * one_side_cost_rate;

    let mut equity = cfg.starting_balance;
    let mut trades: Vec<StatArbTrade> = Vec::new();
    let mut fees_paid_quote = 0.0;
    let mut open_position: Option<OpenPosition> = None;
    let mut equity_curve: Vec<(i64, f64)> = Vec::with_capacity(aligned.len());
    let mut returns: Vec<f64> = Vec::with_capacity(aligned.len());
    let mut peak_equity = equity;
    let mut max_drawdown_pct = 0.0_f64;
    let mut bars_in_position = 0_usize;
    let mut last_equity = equity;

    let log_ratios: Vec<f64> = aligned
        .iter()
        .map(|&(ts, pa, pb)| log_ratio(ts, pa, pb))
        .collect::<Result<_>>()?;

    for i in lookback..aligned.len() {
        let (ts, pa_now, pb_now) = aligned[i];
        let window = &log_ratios[i - lookback..i];
        let mean = fmean(window);
        let sd = pstdev(window);
        if sd <= 0.0 {
            equity_curve.push((ts, equity));
            continue;
        }
        let z = (log_ratios[i] - mean) / sd;

        if let Some(pos) = open_position.as_mut() {
            let mtm_pnl = pos.mtm_pnl(pa_now, pb_now);
            // Per-bar short borrow drag accrues before the close decision.
            pos.accrued_drag +=
                cfg.notional_per_position * (cfg.short_borrow_drag_bps_per_bar / 10_000.0);
            pos.bars_held += 1;
            if let Some(reason) = close_reason(z, pos.bars_held, cfg) {
                let (trade, new_equity) = pos.close(pair, ts, mtm_pnl, z, leg_fees, reason);
                equity = new_equity;
                fees_paid_quote += leg_fees;
                trades.push(trade);
                open_position = None;
            }
        }

        if open_position.is_none() && z.abs() >= cfg.entry_z {
            // z > 0: A is too high relative to B → short A, long B.
            let long_is_a = z <= 0.0;
            let (long_qty, short_qty) = if long_is_a {
                (
                    cfg.notional_per_position / pa_now,
                    cfg.notional_per_position / pb_now,
                )
            } else {
                (
                    cfg.notional_per_position / pb_now,
                    cfg.notional_per_position / pa_now,
                )
            };
            fees_paid_quote += leg_fees;
            open_position = Some(OpenPosition {
                open_time_ms: ts,
                entry_z: z,
                long_is_a,
                long_qty,
                short_qty,
                entry_prices: (pa_now, pb_now),
                open_fees: leg_fees,
                accrued_drag: 0.0,
                bars_held: 0,
                entry_equity: equity,
            });
        }

        if open_position.is_some() {
            bars_in_position += 1;
        }
        returns.push(if last_equity > 0.0 {
            equity / last_equity - 1.0
        } else {
            0.0
        });
        last_equity = equity;
        equity_curve.push((ts, equity));
        if equity > peak_equity {
            peak_equity = equity;
        } else if peak_equity > 0.0 {
            let drawdown_pct = (peak_equity - equity) / peak_equity * 100.0;
            if drawdown_pct > max_drawdown_pct {
                max_drawdown_pct = drawdown_pct;
            }
        }
    }

    // Force-close any still-open trade at the final bar.
    if let (Some(pos), Some(&(ts, pa_now, pb_now))) = (open_position.as_ref(), aligned.last()) {
        let mtm_pnl = pos.mtm_pnl(pa_now, pb_now);
        let (trade, new_equity) = pos.close(pair, ts, mtm_pnl, 0.0, leg_fees, "window end".into());
        equity = new_equity;
        fees_paid_quote += leg_fees;
        trades.push(trade);
    }

    let bars_evaluated = returns.len();
    let total_return_pct = (equity / cfg.starting_balance - 1.0) * 100.0;
    let bpy = bars_per_year(&cfg.interval, 8_760.0);
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
    let avg_bars_held = if trades.is_empty() {
        0.0
    } else {
        trades.iter().map(|t| t.bars_held).sum::<i64>() as f64 / trades.len() as f64
    };
    let bars_in_position_pct = if bars_evaluated > 0 {
        bars_in_position as f64 / bars_evaluated as f64 * 100.0
    } else {
        0.0
    };

    Ok(StatArbResult {
        config: cfg.clone(),
        pair: pair_owned(),
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
        bars_evaluated,
        bars_in_position_pct,
        equity_curve,
        trades,
    })
}

/// Compact CLI summary (Python `_result_summary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub pair: (String, String),
    pub config: StatArbConfig,
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
    pub bars_evaluated: usize,
    pub bars_in_position_pct: f64,
}

pub fn result_summary(result: &StatArbResult) -> ResultSummary {
    ResultSummary {
        verdict: result.verdict(),
        pair: result.pair.clone(),
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
        bars_evaluated: result.bars_evaluated,
        bars_in_position_pct: py_round(result.bars_in_position_pct, 2),
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, default_value = "ETH/USDT")]
    pub symbol_a: String,
    #[arg(long, default_value = "BTC/USDT")]
    pub symbol_b: String,
    #[arg(long, default_value = "1h")]
    pub interval: String,
    /// ISO start timestamp; required unless --fixture is supplied.
    #[arg(long, default_value = "")]
    pub start: String,
    /// ISO end timestamp; defaults to now.
    #[arg(long, default_value = "")]
    pub end: String,
    /// JSON `{symbol: [kline, ...]}` file with exactly two symbols.
    #[arg(long, default_value = "")]
    pub fixture: String,
    /// Directory to cache fetched klines.
    #[arg(long, default_value = "")]
    pub klines_cache: String,
    /// Optional path to dump the full report JSON.
    #[arg(long, default_value = "")]
    pub output: String,
    #[arg(long, default_value_t = 168, allow_negative_numbers = true)]
    pub lookback_bars: i64,
    #[arg(long, default_value_t = 2.0, allow_negative_numbers = true)]
    pub entry_z: f64,
    #[arg(long, default_value_t = 0.3, allow_negative_numbers = true)]
    pub exit_z: f64,
    #[arg(long, default_value_t = 4.0, allow_negative_numbers = true)]
    pub stop_z: f64,
    #[arg(long, default_value_t = 168, allow_negative_numbers = true)]
    pub max_hold_bars: i64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub taker_fee_bps: f64,
    #[arg(long, default_value_t = 3.0, allow_negative_numbers = true)]
    pub slippage_bps: f64,
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    pub short_borrow_drag_bps_per_bar: f64,
    #[arg(long, default_value_t = 1_000.0, allow_negative_numbers = true)]
    pub notional_per_position: f64,
    #[arg(long, default_value_t = 5_000.0, allow_negative_numbers = true)]
    pub starting_balance: f64,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = StatArbConfig {
        interval: args.interval.clone(),
        lookback_bars: args.lookback_bars,
        entry_z: args.entry_z,
        exit_z: args.exit_z,
        stop_z: args.stop_z,
        max_hold_bars: args.max_hold_bars,
        taker_fee_bps: args.taker_fee_bps,
        slippage_bps: args.slippage_bps,
        short_borrow_drag_bps_per_bar: args.short_borrow_drag_bps_per_bar,
        notional_per_position: args.notional_per_position,
        starting_balance: args.starting_balance,
    };
    let klines = if !args.fixture.is_empty() {
        load_kline_fixture(Path::new(&args.fixture))?
    } else {
        if args.start.is_empty() {
            return Ok(crate::cli::usage_error(
                "stat-arb-backtest",
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
        let symbols = [args.symbol_a.clone(), args.symbol_b.clone()];
        fetch_klines_for_symbols(
            &client,
            &symbols,
            &args.interval,
            start,
            end,
            cache_dir.as_deref(),
            2,
        )
        .await?
    };
    let result = replay_pair(&klines, &cfg)?;
    common::print_json(&result_summary(&result))?;
    if !args.output.is_empty() {
        common::write_report(Path::new(&args.output), &result)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Bar;

    const HOUR_MS: i64 = 3_600_000;

    fn kline(open_ms: i64, close: f64) -> Bar {
        Bar::new(open_ms, close, close, close, close)
    }

    /// A and B share a mild common drift; their ratio oscillates as a sine wave.
    fn mean_reverting(n: i64, amplitude: f64, period: f64) -> (Vec<Bar>, Vec<Bar>) {
        (0..n)
            .map(|i| {
                let common = 100.0 * (1.0 + 0.0005 * i as f64);
                let delta = amplitude * (2.0 * std::f64::consts::PI * i as f64 / period).sin();
                (
                    kline(i * HOUR_MS, common * (1.0 + delta)),
                    kline(i * HOUR_MS, common * (1.0 - delta)),
                )
            })
            .unzip()
    }

    fn set(entries: Vec<(&str, Vec<Bar>)>) -> KlineSet {
        entries
            .into_iter()
            .map(|(s, b)| (s.to_owned(), b))
            .collect()
    }

    fn pair_set() -> KlineSet {
        let (a, b) = mean_reverting(2000, 0.10, 200.0);
        set(vec![("A/USDT", a), ("B/USDT", b)])
    }

    #[test]
    fn no_trades_on_pair_that_drifts_in_lockstep() {
        let series: Vec<Bar> = (0..300)
            .map(|i| kline(i * HOUR_MS, 100.0 + i as f64 * 0.01))
            .collect();
        let klines = set(vec![("ETH/USDT", series.clone()), ("BTC/USDT", series)]);
        let cfg = StatArbConfig {
            lookback_bars: 50,
            ..StatArbConfig::default()
        };
        assert_eq!(replay_pair(&klines, &cfg).unwrap().num_trades, 0);
    }

    #[test]
    fn trades_mean_reverting_pair() {
        let cfg = StatArbConfig {
            lookback_bars: 100,
            entry_z: 1.0,
            exit_z: 0.2,
            stop_z: 5.0,
            max_hold_bars: 400,
            taker_fee_bps: 10.0,
            slippage_bps: 2.0,
            ..StatArbConfig::default()
        };
        let result = replay_pair(&pair_set(), &cfg).unwrap();
        assert!(result.num_trades >= 1);
        for trade in &result.trades {
            assert_ne!(trade.symbol_long, trade.symbol_short);
            assert!(trade.bars_held > 0);
        }
        assert_eq!(result.pair, ("A/USDT".to_owned(), "B/USDT".to_owned()));
    }

    #[test]
    fn rejects_invalid_config() {
        let klines = set(vec![
            ("A/USDT", vec![kline(0, 100.0)]),
            ("B/USDT", vec![kline(0, 100.0)]),
        ]);
        let err = |cfg: StatArbConfig| replay_pair(&klines, &cfg).unwrap_err().to_string();
        let d = StatArbConfig::default;
        assert!(
            err(StatArbConfig {
                starting_balance: 0.0,
                ..d()
            })
            .contains("starting_balance")
        );
        assert!(
            err(StatArbConfig {
                notional_per_position: 0.0,
                ..d()
            })
            .contains("notional_per_position")
        );
        assert!(
            err(StatArbConfig {
                lookback_bars: 1,
                ..d()
            })
            .contains("lookback_bars")
        );
        assert!(
            err(StatArbConfig {
                entry_z: 0.5,
                exit_z: 1.0,
                ..d()
            })
            .contains("entry_z")
        );
    }

    #[test]
    fn requires_exactly_two_symbols() {
        let klines = set(vec![
            ("A/USDT", vec![kline(0, 100.0)]),
            ("B/USDT", vec![kline(0, 100.0)]),
            ("C/USDT", vec![kline(0, 100.0)]),
        ]);
        let err = replay_pair(&klines, &StatArbConfig::default()).unwrap_err();
        assert!(err.to_string().contains("two symbols"));
    }

    #[test]
    fn handles_short_window_safely() {
        let short: Vec<Bar> = (0..5).map(|i| kline(i * HOUR_MS, 100.0)).collect();
        let klines = set(vec![("A/USDT", short.clone()), ("B/USDT", short)]);
        let cfg = StatArbConfig {
            lookback_bars: 50,
            ..StatArbConfig::default()
        };
        let result = replay_pair(&klines, &cfg).unwrap();
        assert_eq!(result.bars_evaluated, 0);
        assert_eq!(result.verdict(), "no_data");
    }

    #[test]
    fn summary_round_trips_as_json() {
        let cfg = StatArbConfig {
            lookback_bars: 100,
            entry_z: 1.0,
            exit_z: 0.2,
            ..StatArbConfig::default()
        };
        let result = replay_pair(&pair_set(), &cfg).unwrap();
        let decoded: serde_json::Value =
            serde_json::from_str(&common::to_pretty_sorted(&result_summary(&result)).unwrap())
                .unwrap();
        assert_eq!(decoded["verdict"], result.verdict());
        assert_eq!(
            decoded["num_trades"].as_u64().unwrap() as usize,
            result.num_trades
        );
        assert_eq!(decoded["pair"], serde_json::json!(["A/USDT", "B/USDT"]));
    }

    #[test]
    fn close_reasons_use_python_formatting() {
        let cfg = StatArbConfig::default();
        assert_eq!(
            close_reason(0.1234, 1, &cfg).unwrap(),
            "z=+0.12 reverted within exit band (0.3)"
        );
        assert_eq!(
            close_reason(-4.5, 1, &cfg).unwrap(),
            "z=-4.50 hit stop (4.0)"
        );
        assert_eq!(
            close_reason(1.0, 168, &cfg).unwrap(),
            "max hold 168 bars reached"
        );
        assert!(close_reason(1.0, 1, &cfg).is_none());
    }

    #[test]
    fn rejects_non_positive_prices() {
        let mut bars: Vec<Bar> = (0..10).map(|i| kline(i * HOUR_MS, 100.0)).collect();
        let klines_b = bars.clone();
        bars[3] = kline(3 * HOUR_MS, 0.0);
        let klines = set(vec![("A/USDT", bars), ("B/USDT", klines_b)]);
        let cfg = StatArbConfig {
            lookback_bars: 2,
            ..StatArbConfig::default()
        };
        assert!(
            replay_pair(&klines, &cfg)
                .unwrap_err()
                .to_string()
                .contains("math domain error")
        );
    }

    #[tokio::test]
    async fn cli_runs_against_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("klines.json");
        let rows = |bars: &[Bar]| -> Vec<Vec<f64>> {
            bars.iter()
                .map(|b| vec![b.open_ms as f64, b.open, b.high, b.low, b.close, 1.0])
                .collect()
        };
        let (a, b) = mean_reverting(2000, 0.10, 200.0);
        std::fs::write(
            &fixture,
            serde_json::json!({"A/USDT": rows(&a), "B/USDT": rows(&b)}).to_string(),
        )
        .unwrap();
        let output = dir.path().join("report.json");
        let args = Args {
            symbol_a: "ETH/USDT".into(),
            symbol_b: "BTC/USDT".into(),
            interval: "1h".into(),
            start: String::new(),
            end: String::new(),
            fixture: fixture.display().to_string(),
            klines_cache: String::new(),
            output: output.display().to_string(),
            lookback_bars: 100,
            entry_z: 1.0,
            exit_z: 0.2,
            stop_z: 4.0,
            max_hold_bars: 168,
            taker_fee_bps: 10.0,
            slippage_bps: 3.0,
            short_borrow_drag_bps_per_bar: 0.0,
            notional_per_position: 1_000.0,
            starting_balance: 5_000.0,
        };
        assert_eq!(run_cli(args).await.unwrap(), ExitCode::SUCCESS);
        let full: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert!(full["bars_evaluated"].as_u64().unwrap() > 0);
        assert!(full.get("trades").is_some());
    }
}

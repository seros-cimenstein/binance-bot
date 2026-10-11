//! Historical replay harness for spot/perp basis-convergence arbitrage (port of
//! `src/basis_arb_backtest.py`).
//!
//! When the perp price diverges from spot beyond an entry threshold, take a
//! delta-neutral position betting on convergence:
//!
//! * `basis_bps = (mark - spot) / spot * 10000`
//! * `basis_bps > +entry`: perp expensive → short perp, long spot (`short_basis`)
//! * `basis_bps < -entry`: perp cheap → long perp, short spot (`long_basis`; only in
//!   `both_sides` mode since shorting spot needs margin)
//! * Close when `|basis_bps|` drops below exit, max-hold elapses, or basis blows past stop.

use std::path::Path;
use std::process::ExitCode;

use bb_binance::BinanceClient;
use bb_binance::futures::RangeQuery;
use bb_binance::history::HistoryFetcher;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common::{
    self, Bar, bars_per_year, fmean, parse_iso, py_float_repr, py_round, sharpe_annualized,
};
use crate::error::{Result, StrategyError};

/// `limit` per `/fapi/v1/klines` page in the Python perp fetcher.
pub const PERP_PAGE_LIMIT: u32 = 1_500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BasisArbConfig {
    pub interval: String,
    pub entry_basis_bps: f64,
    pub exit_basis_bps: f64,
    pub stop_basis_bps: f64,
    pub max_hold_bars: i64,
    pub taker_fee_spot_bps: f64,
    pub taker_fee_futures_bps: f64,
    pub slippage_per_leg_bps: f64,
    pub notional_per_position: f64,
    pub starting_balance: f64,
    /// `spot_only` | `both_sides`.
    pub mode: String,
}

impl Default for BasisArbConfig {
    fn default() -> Self {
        Self {
            interval: "1h".into(),
            entry_basis_bps: 30.0,
            exit_basis_bps: 5.0,
            stop_basis_bps: 200.0,
            max_hold_bars: 24,
            taker_fee_spot_bps: 10.0,
            taker_fee_futures_bps: 5.0,
            slippage_per_leg_bps: 2.0,
            notional_per_position: 1_000.0,
            starting_balance: 5_000.0,
            mode: "spot_only".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BasisSide {
    /// Long perp, short spot (perp at a discount).
    LongBasis,
    /// Short perp, long spot (perp at a premium).
    ShortBasis,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BasisTrade {
    pub symbol: String,
    pub side: BasisSide,
    pub open_time_ms: i64,
    pub close_time_ms: i64,
    pub entry_basis_bps: f64,
    pub exit_basis_bps: f64,
    pub bars_held: i64,
    pub realized_basis_bps: f64,
    pub pnl_quote: f64,
    pub fees_quote: f64,
    pub exit_reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BasisArbResult {
    pub config: BasisArbConfig,
    pub symbol: String,
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
    pub avg_basis_bps: f64,
    pub median_basis_bps: f64,
    pub equity_curve: Vec<(i64, f64)>,
    pub trades: Vec<BasisTrade>,
}

impl BasisArbResult {
    fn empty(symbol: &str, cfg: &BasisArbConfig) -> Self {
        Self {
            config: cfg.clone(),
            symbol: symbol.to_owned(),
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
            avg_basis_bps: 0.0,
            median_basis_bps: 0.0,
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

/// Common open times (ascending) with `(spot_close, perp_close)`; later duplicates win.
fn align_spot_perp(spot: &[Bar], perp: &[Bar]) -> Vec<(i64, f64, f64)> {
    let spot_idx: std::collections::HashMap<i64, f64> =
        spot.iter().map(|b| (b.open_ms, b.close)).collect();
    let perp_idx: std::collections::HashMap<i64, f64> =
        perp.iter().map(|b| (b.open_ms, b.close)).collect();
    let mut common: Vec<(i64, f64, f64)> = spot_idx
        .iter()
        .filter_map(|(ts, s)| perp_idx.get(ts).map(|p| (*ts, *s, *p)))
        .collect();
    common.sort_unstable_by_key(|(ts, _, _)| *ts);
    common
}

/// `statistics.median`.
fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

struct OpenPosition {
    side: BasisSide,
    open_time_ms: i64,
    entry_basis_bps: f64,
    bars_held: i64,
    open_fees: f64,
    entry_equity: f64,
}

impl OpenPosition {
    /// Close at `basis_bps`; returns the trade, the post-close equity and the close fees.
    fn close(
        &self,
        symbol: &str,
        ts: i64,
        basis_bps: f64,
        cfg: &BasisArbConfig,
        one_side_cost_bps: f64,
        exit_reason: String,
    ) -> (BasisTrade, f64, f64) {
        let mut realized_basis_bps = self.entry_basis_bps - basis_bps;
        if self.side == BasisSide::LongBasis {
            realized_basis_bps = -realized_basis_bps;
        }
        let gross_pnl = cfg.notional_per_position * (realized_basis_bps / 10_000.0);
        let close_fees = cfg.notional_per_position * (one_side_cost_bps / 10_000.0);
        let final_pnl = gross_pnl - self.open_fees - close_fees;
        let trade = BasisTrade {
            symbol: symbol.to_owned(),
            side: self.side,
            open_time_ms: self.open_time_ms,
            close_time_ms: ts,
            entry_basis_bps: self.entry_basis_bps,
            exit_basis_bps: basis_bps,
            bars_held: self.bars_held,
            realized_basis_bps,
            pnl_quote: final_pnl,
            fees_quote: self.open_fees + close_fees,
            exit_reason,
        };
        (trade, self.entry_equity + final_pnl, close_fees)
    }
}

fn close_reason(
    side: BasisSide,
    basis_bps: f64,
    bars_held: i64,
    cfg: &BasisArbConfig,
) -> Option<String> {
    let exit = || {
        format!(
            "basis {basis_bps:.1}bps within exit ({})",
            py_float_repr(cfg.exit_basis_bps)
        )
    };
    let stop = || {
        format!(
            "basis {basis_bps:.1}bps blew past stop ({})",
            py_float_repr(cfg.stop_basis_bps)
        )
    };
    let reason = match side {
        // Converging basis means basis_bps drops toward 0.
        BasisSide::ShortBasis if basis_bps <= cfg.exit_basis_bps => Some(exit()),
        BasisSide::ShortBasis if basis_bps >= cfg.stop_basis_bps => Some(stop()),
        BasisSide::LongBasis if basis_bps >= -cfg.exit_basis_bps => Some(exit()),
        BasisSide::LongBasis if basis_bps <= -cfg.stop_basis_bps => Some(stop()),
        _ => None,
    };
    reason.or_else(|| {
        (bars_held >= cfg.max_hold_bars)
            .then(|| format!("max hold ({} bars) reached", cfg.max_hold_bars))
    })
}

/// Replay basis-convergence arbitrage for one symbol.
pub fn replay_basis_arb(
    symbol: &str,
    spot: &[Bar],
    perp: &[Bar],
    cfg: &BasisArbConfig,
) -> Result<BasisArbResult> {
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
    if cfg.entry_basis_bps <= cfg.exit_basis_bps {
        return Err(StrategyError::InvalidConfig(
            "entry_basis_bps must be > exit_basis_bps".into(),
        ));
    }
    let both_sides = match cfg.mode.as_str() {
        "spot_only" => false,
        "both_sides" => true,
        _ => {
            return Err(StrategyError::InvalidConfig(
                "mode must be spot_only or both_sides".into(),
            ));
        }
    };

    let aligned = align_spot_perp(spot, perp);
    if aligned.is_empty() {
        return Ok(BasisArbResult::empty(symbol, cfg));
    }

    let one_side_cost_bps =
        cfg.taker_fee_spot_bps + cfg.taker_fee_futures_bps + 2.0 * cfg.slippage_per_leg_bps;

    let mut equity = cfg.starting_balance;
    let mut fees_paid_quote = 0.0;
    let mut trades: Vec<BasisTrade> = Vec::new();
    let mut open_position: Option<OpenPosition> = None;
    let mut equity_curve: Vec<(i64, f64)> = Vec::with_capacity(aligned.len());
    let mut returns: Vec<f64> = Vec::with_capacity(aligned.len());
    let mut basis_history: Vec<f64> = Vec::with_capacity(aligned.len());
    let mut peak_equity = equity;
    let mut max_drawdown_pct = 0.0_f64;
    let mut bars_in_position = 0_usize;
    let mut last_equity = equity;
    // Last bar actually evaluated (spot > 0): the force-close mark.
    let mut last_evaluated: Option<(i64, f64)> = None;

    for &(ts, spot_price, mark_price) in &aligned {
        if spot_price <= 0.0 {
            continue;
        }
        let basis_bps = (mark_price - spot_price) / spot_price * 10_000.0;
        basis_history.push(basis_bps);
        last_evaluated = Some((ts, basis_bps));

        if let Some(pos) = open_position.as_mut() {
            pos.bars_held += 1;
            if let Some(reason) = close_reason(pos.side, basis_bps, pos.bars_held, cfg) {
                let (trade, new_equity, close_fees) =
                    pos.close(symbol, ts, basis_bps, cfg, one_side_cost_bps, reason);
                equity = new_equity;
                fees_paid_quote += close_fees;
                trades.push(trade);
                open_position = None;
            }
        }

        if open_position.is_none() {
            let side = if basis_bps >= cfg.entry_basis_bps {
                Some(BasisSide::ShortBasis)
            } else if both_sides && basis_bps <= -cfg.entry_basis_bps {
                Some(BasisSide::LongBasis)
            } else {
                None
            };
            if let Some(side) = side {
                let open_fees = cfg.notional_per_position * (one_side_cost_bps / 10_000.0);
                fees_paid_quote += open_fees;
                open_position = Some(OpenPosition {
                    side,
                    open_time_ms: ts,
                    entry_basis_bps: basis_bps,
                    bars_held: 0,
                    open_fees,
                    entry_equity: equity,
                });
            }
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

    // Force-close at the end of the window (positions only open on evaluated bars).
    if let (Some(pos), Some((ts, basis_bps))) = (open_position.as_ref(), last_evaluated) {
        let (trade, new_equity, close_fees) = pos.close(
            symbol,
            ts,
            basis_bps,
            cfg,
            one_side_cost_bps,
            "window end".into(),
        );
        equity = new_equity;
        fees_paid_quote += close_fees;
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

    Ok(BasisArbResult {
        config: cfg.clone(),
        symbol: symbol.to_owned(),
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
        avg_basis_bps: fmean(&basis_history),
        median_basis_bps: median(&basis_history),
        equity_curve,
        trades,
    })
}

/// Compact CLI summary (Python `_result_summary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub symbol: String,
    pub config: BasisArbConfig,
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
    pub avg_basis_bps: f64,
    pub median_basis_bps: f64,
}

pub fn result_summary(result: &BasisArbResult) -> ResultSummary {
    ResultSummary {
        verdict: result.verdict(),
        symbol: result.symbol.clone(),
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
        avg_basis_bps: py_round(result.avg_basis_bps, 4),
        median_basis_bps: py_round(result.median_basis_bps, 4),
    }
}

/// Spot klines (`/api/v3/klines`) via the paged history fetcher.
pub async fn fetch_spot_klines(
    client: &BinanceClient,
    symbol: &str,
    interval: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<Bar>> {
    let klines = HistoryFetcher::spot(client.clone())
        .fetch_klines(symbol, interval, start, Some(end))
        .await?;
    Ok(klines.iter().map(Bar::from_kline).collect())
}

/// USD-M perp klines (`/fapi/v1/klines`), paged exactly like the Python fetcher:
/// 1500-row pages from `start` to `end`, next cursor = last open + 1, stopping on an
/// empty page, a non-advancing page or a short page.
pub async fn fetch_perp_klines(
    client: &BinanceClient,
    symbol: &str,
    interval: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<Bar>> {
    let mut out: Vec<Bar> = Vec::new();
    let mut cursor = start.timestamp_millis();
    let end_ms = end.timestamp_millis();
    while cursor <= end_ms {
        let range = RangeQuery {
            start_time: Some(cursor),
            end_time: Some(end_ms),
            limit: Some(PERP_PAGE_LIMIT),
        };
        let page = client.futures_klines(symbol, interval, &range).await?;
        let Some(last_open) = page.last().map(|k| k.open_time) else {
            break;
        };
        out.extend(page.iter().map(Bar::from_kline));
        if last_open <= cursor {
            break;
        }
        cursor = last_open + 1;
        if page.len() < PERP_PAGE_LIMIT as usize {
            break;
        }
    }
    Ok(out)
}

/// `{"spot": [kline, ...], "perp": [kline, ...]}` (other keys ignored).
#[derive(Debug, Deserialize)]
struct BasisFixture {
    spot: Vec<Bar>,
    perp: Vec<Bar>,
}

/// Python `_load_fixture` + `payload["spot"]` / `payload["perp"]`.
pub fn load_fixture(path: &Path) -> Result<(Vec<Bar>, Vec<Bar>)> {
    let text = std::fs::read_to_string(path).map_err(|e| StrategyError::io(path, e))?;
    let fixture: BasisFixture =
        serde_json::from_str(&text).map_err(|e| common::json_error(path, &e))?;
    Ok((fixture.spot, fixture.perp))
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, default_value = "BTC/USDT")]
    pub symbol: String,
    #[arg(long, default_value = "1h")]
    pub interval: String,
    /// ISO start timestamp; required unless --fixture is supplied.
    #[arg(long, default_value = "")]
    pub start: String,
    /// ISO end timestamp; defaults to now.
    #[arg(long, default_value = "")]
    pub end: String,
    /// JSON `{"spot": [kline, ...], "perp": [kline, ...]}` file for offline runs.
    #[arg(long, default_value = "")]
    pub fixture: String,
    /// Optional path to dump the full report JSON.
    #[arg(long, default_value = "")]
    pub output: String,
    #[arg(long, default_value_t = 30.0, allow_negative_numbers = true)]
    pub entry_basis_bps: f64,
    #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
    pub exit_basis_bps: f64,
    #[arg(long, default_value_t = 200.0, allow_negative_numbers = true)]
    pub stop_basis_bps: f64,
    #[arg(long, default_value_t = 24, allow_negative_numbers = true)]
    pub max_hold_bars: i64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub taker_fee_spot_bps: f64,
    #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
    pub taker_fee_futures_bps: f64,
    #[arg(long, default_value_t = 2.0, allow_negative_numbers = true)]
    pub slippage_per_leg_bps: f64,
    #[arg(long, default_value_t = 1_000.0, allow_negative_numbers = true)]
    pub notional_per_position: f64,
    #[arg(long, default_value_t = 5_000.0, allow_negative_numbers = true)]
    pub starting_balance: f64,
    #[arg(long, default_value = "spot_only", value_parser = clap::builder::PossibleValuesParser::new(["spot_only", "both_sides"]))]
    pub mode: String,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = BasisArbConfig {
        interval: args.interval.clone(),
        entry_basis_bps: args.entry_basis_bps,
        exit_basis_bps: args.exit_basis_bps,
        stop_basis_bps: args.stop_basis_bps,
        max_hold_bars: args.max_hold_bars,
        taker_fee_spot_bps: args.taker_fee_spot_bps,
        taker_fee_futures_bps: args.taker_fee_futures_bps,
        slippage_per_leg_bps: args.slippage_per_leg_bps,
        notional_per_position: args.notional_per_position,
        starting_balance: args.starting_balance,
        mode: args.mode.clone(),
    };
    let (spot, perp) = if !args.fixture.is_empty() {
        load_fixture(Path::new(&args.fixture))?
    } else {
        if args.start.is_empty() {
            return Ok(crate::cli::usage_error(
                "basis-arb-backtest",
                "either --fixture or --start must be supplied",
            ));
        }
        let start = parse_iso(&args.start)?;
        let end = if args.end.is_empty() {
            bb_core::time::now()
        } else {
            parse_iso(&args.end)?
        };
        let client = common::public_client()?;
        tokio::try_join!(
            fetch_spot_klines(&client, &args.symbol, &args.interval, start, end),
            fetch_perp_klines(&client, &args.symbol, &args.interval, start, end),
        )?
    };
    let result = replay_basis_arb(&args.symbol, &spot, &perp, &cfg)?;
    common::print_json(&result_summary(&result))?;
    if !args.output.is_empty() {
        common::write_report(Path::new(&args.output), &result)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 3_600_000;

    fn kline(open_ms: i64, close: f64) -> Bar {
        Bar::new(open_ms, close, close, close, close)
    }

    fn spot_perp_with_basis_spike(
        n: i64,
        spike_at: i64,
        spike_bps: f64,
        decay_bars: i64,
    ) -> (Vec<Bar>, Vec<Bar>) {
        let spot_level = 100.0;
        let spot = (0..n).map(|i| kline(i * HOUR_MS, spot_level)).collect();
        let perp = (0..n)
            .map(|i| {
                let premium = if i < spike_at {
                    0.0
                } else {
                    spike_bps * (1.0 - (i - spike_at) as f64 / decay_bars as f64).max(0.0)
                };
                kline(i * HOUR_MS, spot_level * (1.0 + premium / 10_000.0))
            })
            .collect();
        (spot, perp)
    }

    fn flat(n: i64, price: f64) -> Vec<Bar> {
        (0..n).map(|i| kline(i * HOUR_MS, price)).collect()
    }

    #[test]
    fn no_trades_when_basis_within_band() {
        let cfg = BasisArbConfig {
            entry_basis_bps: 30.0,
            exit_basis_bps: 5.0,
            ..BasisArbConfig::default()
        };
        let result =
            replay_basis_arb("BTC/USDT", &flat(100, 100.0), &flat(100, 100.0), &cfg).unwrap();
        assert_eq!(result.num_trades, 0);
    }

    #[test]
    fn enters_on_premium_spike_and_exits_on_convergence() {
        let (spot, perp) = spot_perp_with_basis_spike(80, 20, 100.0, 10);
        let cfg = BasisArbConfig {
            entry_basis_bps: 50.0,
            exit_basis_bps: 10.0,
            stop_basis_bps: 300.0,
            max_hold_bars: 40,
            ..BasisArbConfig::default()
        };
        let result = replay_basis_arb("BTC/USDT", &spot, &perp, &cfg).unwrap();
        assert!(result.num_trades >= 1);
        assert!(
            result
                .trades
                .iter()
                .any(|t| t.side == BasisSide::ShortBasis)
        );
        // Realized ~90 bps vs 38 bps round-trip cost.
        assert!(result.final_equity > result.initial_balance);
        assert!(result.trades[0].exit_reason.ends_with("within exit (10.0)"));
    }

    #[test]
    fn skips_negative_basis_in_spot_only_mode() {
        let perp = flat(80, 100.0 * (1.0 - 50.0 / 10_000.0));
        let cfg = BasisArbConfig {
            entry_basis_bps: 30.0,
            ..BasisArbConfig::default()
        };
        let result = replay_basis_arb("BTC/USDT", &flat(80, 100.0), &perp, &cfg).unwrap();
        assert_eq!(result.num_trades, 0);
    }

    #[test]
    fn both_sides_takes_negative_basis() {
        let perp: Vec<Bar> = (0..80)
            .map(|i| {
                let premium = if i < 30 {
                    -50.0
                } else {
                    -50.0 * (1.0 - (i - 30) as f64 / 10.0).max(0.0)
                };
                kline(i * HOUR_MS, 100.0 * (1.0 + premium / 10_000.0))
            })
            .collect();
        let cfg = BasisArbConfig {
            entry_basis_bps: 30.0,
            exit_basis_bps: 5.0,
            mode: "both_sides".into(),
            max_hold_bars: 60,
            ..BasisArbConfig::default()
        };
        let result = replay_basis_arb("BTC/USDT", &flat(80, 100.0), &perp, &cfg).unwrap();
        assert!(result.num_trades >= 1);
        assert!(result.trades.iter().any(|t| t.side == BasisSide::LongBasis));
    }

    #[test]
    fn rejects_invalid_config() {
        let (spot, perp) = (flat(1, 100.0), flat(1, 100.0));
        let err = |cfg: BasisArbConfig| {
            replay_basis_arb("BTC/USDT", &spot, &perp, &cfg)
                .unwrap_err()
                .to_string()
        };
        let d = BasisArbConfig::default;
        assert!(
            err(BasisArbConfig {
                starting_balance: 0.0,
                ..d()
            })
            .contains("starting_balance")
        );
        assert!(
            err(BasisArbConfig {
                notional_per_position: 0.0,
                ..d()
            })
            .contains("notional_per_position")
        );
        assert!(
            err(BasisArbConfig {
                entry_basis_bps: 5.0,
                exit_basis_bps: 10.0,
                ..d()
            })
            .contains("entry_basis_bps")
        );
        assert!(
            err(BasisArbConfig {
                mode: "bogus".into(),
                ..d()
            })
            .contains("mode")
        );
    }

    #[test]
    fn summary_round_trips_as_json() {
        let (spot, perp) = spot_perp_with_basis_spike(60, 15, 80.0, 10);
        let result =
            replay_basis_arb("BTC/USDT", &spot, &perp, &BasisArbConfig::default()).unwrap();
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
    fn force_close_ignores_trailing_bar_with_zero_spot() {
        // Python divides by the trailing zero spot price here (ZeroDivisionError).
        let (mut spot, perp) = spot_perp_with_basis_spike(30, 20, 100.0, 1_000);
        spot[29] = kline(29 * HOUR_MS, 0.0);
        let result =
            replay_basis_arb("BTC/USDT", &spot, &perp, &BasisArbConfig::default()).unwrap();
        let last = result.trades.last().unwrap();
        assert_eq!(last.exit_reason, "window end");
        assert_eq!(last.close_time_ms, 28 * HOUR_MS);
        assert!(last.exit_basis_bps.is_finite());
        assert_eq!(result.bars_evaluated, 29);
    }

    #[test]
    fn median_matches_statistics_median() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }

    #[tokio::test]
    async fn perp_fetch_pages_like_python() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let row = |t: i64| {
            serde_json::json!([
                t,
                "1",
                "1",
                "1",
                "1",
                "1",
                t + 59_999,
                "1",
                1,
                "1",
                "1",
                "0"
            ])
        };
        let full_page: Vec<serde_json::Value> = (0..1_500).map(|i| row(i * 60_000)).collect();
        Mock::given(method("GET"))
            .and(path("/fapi/v1/klines"))
            .and(query_param("startTime", "0"))
            .and(query_param("limit", "1500"))
            .and(query_param("symbol", "BTCUSDT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(full_page))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fapi/v1/klines"))
            .and(query_param("startTime", (1_499 * 60_000 + 1).to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![row(1_500 * 60_000)]))
            .expect(1)
            .mount(&server)
            .await;
        let client = BinanceClient::new(
            bb_binance::BinanceConfig::new(false)
                .with_spot_rest_url(server.uri())
                .with_futures_rest_url(server.uri()),
        )
        .unwrap();
        let start = parse_iso("1970-01-01").unwrap();
        let end = parse_iso("1970-01-03").unwrap();
        let bars = fetch_perp_klines(&client, "BTC/USDT", "1m", start, end)
            .await
            .unwrap();
        assert_eq!(bars.len(), 1_501);
        assert_eq!(bars[1_500].open_ms, 1_500 * 60_000);
    }

    #[tokio::test]
    async fn cli_runs_against_fixture() {
        let (spot, perp) = spot_perp_with_basis_spike(80, 20, 100.0, 10);
        let rows = |bars: &[Bar]| -> Vec<Vec<f64>> {
            bars.iter()
                .map(|b| vec![b.open_ms as f64, b.open, b.high, b.low, b.close, 1.0])
                .collect()
        };
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture.json");
        std::fs::write(
            &fixture,
            serde_json::json!({"spot": rows(&spot), "perp": rows(&perp)}).to_string(),
        )
        .unwrap();
        let output = dir.path().join("report.json");
        let args = Args {
            symbol: "BTC/USDT".into(),
            interval: "1h".into(),
            start: String::new(),
            end: String::new(),
            fixture: fixture.display().to_string(),
            output: output.display().to_string(),
            entry_basis_bps: 50.0,
            exit_basis_bps: 10.0,
            stop_basis_bps: 200.0,
            max_hold_bars: 24,
            taker_fee_spot_bps: 10.0,
            taker_fee_futures_bps: 5.0,
            slippage_per_leg_bps: 2.0,
            notional_per_position: 1_000.0,
            starting_balance: 5_000.0,
            mode: "spot_only".into(),
        };
        assert_eq!(run_cli(args).await.unwrap(), ExitCode::SUCCESS);
        let full: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert!(full["bars_evaluated"].as_u64().unwrap() > 0);
        assert!(full.get("trades").is_some());
        assert_eq!(full["trades"][0]["side"], "short_basis");
    }
}

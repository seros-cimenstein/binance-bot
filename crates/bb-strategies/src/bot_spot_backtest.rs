//! Historical replay harness for Bot Spot's decision regime (port of
//! `src/claude_spot_backtest.py`).
//!
//! Deterministic momentum rotation: each bar, score every candidate by its lookback
//! return minus a modelled round-trip cost (taker fee + half-spread proxy + slippage),
//! rotate into the best one when its edge clears `min_edge_after_friction_bps`, otherwise
//! sit in the base asset. A positive verdict is what justifies `BOT_SPOT_PROVEN_EDGE=yes`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bb_binance::BinanceClient;
use serde::{Deserialize, Serialize};

use crate::common::{
    self, Bar, KlineSet, bars_per_year, fetch_klines_for_symbols, load_kline_fixture, parse_iso,
    py_round, sharpe_annualized, split_csv,
};
use crate::error::{Result, StrategyError};
use bb_binance::fiat;

/// Pseudo-symbol for "holding the base asset".
pub const BASE_CASH: &str = "__BASE__";

/// Cost-aware replay knobs (Binance VIP0 spot taker fees + slippage + half-spread proxy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub base_asset: String,
    pub interval: String,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    pub min_spread_bps: f64,
    pub max_spread_bps: f64,
    pub lookback_bars: i64,
    pub min_edge_after_friction_bps: f64,
    pub rebalance_cooldown_bars: i64,
    pub starting_balance: f64,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            base_asset: "USDT".into(),
            interval: "1h".into(),
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            min_spread_bps: 2.0,
            max_spread_bps: 100.0,
            lookback_bars: 24,
            min_edge_after_friction_bps: 50.0,
            rebalance_cooldown_bars: 1,
            starting_balance: 10_000.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeRecord {
    pub timestamp_ms: i64,
    pub from_asset: String,
    pub to_asset: String,
    pub cost_bps: f64,
    pub equity_before: f64,
    pub equity_after: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BacktestResult {
    pub config: BacktestConfig,
    pub initial_balance: f64,
    pub final_balance: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub win_rate_pct: f64,
    pub fee_drag_bps: f64,
    pub bars_in_position_pct: f64,
    pub bars_evaluated: usize,
    pub equity_curve: Vec<(i64, f64)>,
    pub per_asset_bars_held: BTreeMap<String, usize>,
    pub trades: Vec<TradeRecord>,
}

impl BacktestResult {
    fn empty(cfg: &BacktestConfig) -> Self {
        Self {
            config: cfg.clone(),
            initial_balance: cfg.starting_balance,
            final_balance: cfg.starting_balance,
            total_return_pct: 0.0,
            annualized_return_pct: 0.0,
            sharpe_annualized: 0.0,
            max_drawdown_pct: 0.0,
            num_trades: 0,
            win_rate_pct: 0.0,
            fee_drag_bps: 0.0,
            bars_in_position_pct: 0.0,
            bars_evaluated: 0,
            equity_curve: Vec::new(),
            per_asset_bars_held: BTreeMap::new(),
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

/// Bar range as a half-spread proxy, clipped to `[min_spread_bps, max_spread_bps]`.
pub fn half_spread_bps(bar: &Bar, cfg: &BacktestConfig) -> f64 {
    if bar.close <= 0.0 {
        return cfg.max_spread_bps;
    }
    let raw = (bar.high - bar.low) / bar.close * 10_000.0 / 2.0;
    cfg.min_spread_bps.max(cfg.max_spread_bps.min(raw))
}

fn round_trip_cost_bps(spread_bps: f64, cfg: &BacktestConfig) -> f64 {
    2.0 * cfg.taker_fee_bps + spread_bps + cfg.slippage_bps
}

type TimeIndex<'a> = HashMap<&'a str, HashMap<i64, &'a Bar>>;

/// Union-of-times grid plus per-symbol `open_ms → bar` index (later duplicates win).
fn align_by_open_time(klines: &KlineSet) -> (Vec<i64>, TimeIndex<'_>) {
    let mut by_ts: TimeIndex<'_> = HashMap::with_capacity(klines.len());
    let mut times: Vec<i64> = Vec::new();
    for (symbol, bars) in klines {
        let indexed: HashMap<i64, &Bar> = bars.iter().map(|b| (b.open_ms, b)).collect();
        times.extend(indexed.keys().copied());
        by_ts.insert(symbol.as_str(), indexed);
    }
    times.sort_unstable();
    times.dedup();
    (times, by_ts)
}

/// Replay deterministic decisions against historical klines.
pub fn replay(klines: &KlineSet, cfg: &BacktestConfig) -> Result<BacktestResult> {
    if cfg.lookback_bars <= 0 {
        return Err(StrategyError::InvalidConfig(
            "lookback_bars must be positive".into(),
        ));
    }
    if cfg.starting_balance <= 0.0 {
        return Err(StrategyError::InvalidConfig(
            "starting_balance must be positive".into(),
        ));
    }
    let lookback = usize::try_from(cfg.lookback_bars).unwrap_or(usize::MAX);
    let (times, by_ts) = align_by_open_time(klines);
    if times.len() <= lookback.saturating_add(1) {
        return Ok(BacktestResult::empty(cfg));
    }

    let mut equity = cfg.starting_balance;
    let mut held: &str = BASE_CASH;
    let mut last_switch_idx: i64 = -1_000_000_000;
    let mut fee_paid_bps_total = 0.0;
    let mut equity_curve = vec![(times[lookback], equity)];
    let mut per_asset_bars: BTreeMap<String, usize> = BTreeMap::new();
    let mut trades = Vec::new();
    let mut returns = Vec::with_capacity(times.len());
    let (mut wins, mut losses) = (0_usize, 0_usize);
    let mut peak_equity = equity;
    let mut max_drawdown_pct = 0.0_f64;
    // KlineSet is a BTreeMap: keys are already sorted like Python's `sorted(...)`.
    let symbols: Vec<&str> = klines.keys().map(String::as_str).collect();
    let bar_at = |symbol: &str, ts: i64| by_ts.get(symbol).and_then(|m| m.get(&ts)).copied();

    for i in lookback..times.len() - 1 {
        let ts = times[i];
        let next_ts = times[i + 1];
        let ts_lookback = times[i - lookback];

        let mut best_symbol: Option<&str> = None;
        let mut best_edge_bps = f64::NEG_INFINITY;
        for &symbol in &symbols {
            let (Some(here), Some(past)) = (bar_at(symbol, ts), bar_at(symbol, ts_lookback)) else {
                continue;
            };
            if past.close <= 0.0 {
                continue;
            }
            let lookback_return_bps = (here.close / past.close - 1.0) * 10_000.0;
            let cost_bps = round_trip_cost_bps(half_spread_bps(here, cfg), cfg);
            let expected_edge_bps = lookback_return_bps - cost_bps;
            if expected_edge_bps > best_edge_bps {
                best_edge_bps = expected_edge_bps;
                best_symbol = Some(symbol);
            }
        }

        // Force-exit if the held asset has no data this bar.
        if held != BASE_CASH && bar_at(held, ts).is_none() {
            held = BASE_CASH;
        }

        let idx = i64::try_from(i).unwrap_or(i64::MAX);
        if let Some(best) = best_symbol
            && best_edge_bps >= cfg.min_edge_after_friction_bps
            && best != held
            && idx - last_switch_idx >= cfg.rebalance_cooldown_bars
            && let Some(best_bar) = bar_at(best, ts)
        {
            let spread_bps = half_spread_bps(best_bar, cfg);
            let one_leg = 2.0 * cfg.taker_fee_bps + spread_bps + cfg.slippage_bps;
            let switch_cost_bps = if held == BASE_CASH || best == BASE_CASH {
                one_leg
            } else {
                2.0 * one_leg
            };
            let equity_before = equity;
            equity *= 1.0 - switch_cost_bps / 10_000.0;
            fee_paid_bps_total += switch_cost_bps;
            trades.push(TradeRecord {
                timestamp_ms: ts,
                from_asset: held.to_owned(),
                to_asset: best.to_owned(),
                cost_bps: switch_cost_bps,
                equity_before,
                equity_after: equity,
            });
            held = best;
            last_switch_idx = idx;
        }

        let step_return = if held == BASE_CASH {
            0.0
        } else {
            match (bar_at(held, ts), bar_at(held, next_ts)) {
                (Some(here), Some(next)) => {
                    if here.close <= 0.0 {
                        0.0
                    } else {
                        next.close / here.close - 1.0
                    }
                }
                (_, next) => {
                    if next.is_none() {
                        held = BASE_CASH;
                    }
                    0.0
                }
            }
        };
        equity *= 1.0 + step_return;
        returns.push(step_return);
        if step_return > 0.0 {
            wins += 1;
        } else if step_return < 0.0 {
            losses += 1;
        }
        *per_asset_bars.entry(held.to_owned()).or_default() += 1;
        equity_curve.push((next_ts, equity));
        if equity > peak_equity {
            peak_equity = equity;
        } else {
            let drawdown_pct = (peak_equity - equity) / peak_equity * 100.0;
            if drawdown_pct > max_drawdown_pct {
                max_drawdown_pct = drawdown_pct;
            }
        }
    }

    let bars_evaluated = returns.len();
    let mut result = BacktestResult {
        final_balance: equity,
        max_drawdown_pct,
        num_trades: trades.len(),
        fee_drag_bps: fee_paid_bps_total,
        bars_evaluated,
        equity_curve,
        trades,
        ..BacktestResult::empty(cfg)
    };
    if bars_evaluated > 0 {
        let bpy = bars_per_year(&cfg.interval, 8_760.0);
        let years = if bpy > 0.0 {
            bars_evaluated as f64 / bpy
        } else {
            0.0
        };
        result.total_return_pct = (equity / cfg.starting_balance - 1.0) * 100.0;
        result.annualized_return_pct = if years > 0.0 && equity > 0.0 {
            ((equity / cfg.starting_balance).powf(1.0 / years) - 1.0) * 100.0
        } else {
            0.0
        };
        result.sharpe_annualized = sharpe_annualized(&returns, bpy);
        let decisive = wins + losses;
        result.win_rate_pct = if decisive > 0 {
            wins as f64 / decisive as f64 * 100.0
        } else {
            0.0
        };
        let in_cash = per_asset_bars.get(BASE_CASH).copied().unwrap_or(0);
        result.bars_in_position_pct =
            (bars_evaluated - in_cash) as f64 / bars_evaluated as f64 * 100.0;
    }
    result.per_asset_bars_held = per_asset_bars;
    Ok(result)
}

/// Compact CLI summary (Python `_result_summary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub config: BacktestConfig,
    pub initial_balance: f64,
    pub final_balance: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub win_rate_pct: f64,
    pub fee_drag_bps: f64,
    pub bars_in_position_pct: f64,
    pub bars_evaluated: usize,
    pub per_asset_bars_held: BTreeMap<String, usize>,
}

pub fn result_summary(result: &BacktestResult) -> ResultSummary {
    ResultSummary {
        verdict: result.verdict(),
        config: result.config.clone(),
        initial_balance: result.initial_balance,
        final_balance: py_round(result.final_balance, 4),
        total_return_pct: py_round(result.total_return_pct, 4),
        annualized_return_pct: py_round(result.annualized_return_pct, 4),
        sharpe_annualized: py_round(result.sharpe_annualized, 4),
        max_drawdown_pct: py_round(result.max_drawdown_pct, 4),
        num_trades: result.num_trades,
        win_rate_pct: py_round(result.win_rate_pct, 2),
        fee_drag_bps: py_round(result.fee_drag_bps, 2),
        bars_in_position_pct: py_round(result.bars_in_position_pct, 2),
        bars_evaluated: result.bars_evaluated,
        per_asset_bars_held: result.per_asset_bars_held.clone(),
    }
}

/// Whether a base asset is a leveraged token (`*UP`, `*DOWN`, `*BULL`, `*BEAR`).
fn is_leveraged_token(base: &str) -> bool {
    ["UP", "DOWN", "BULL", "BEAR"]
        .iter()
        .any(|suffix| base.ends_with(suffix))
}

/// Top-N USDT spot symbols by 24h quote volume, excluding fiat bases and leveraged tokens.
pub async fn fetch_spot_usdt_universe(
    client: &BinanceClient,
    max_symbols: usize,
    min_quote_volume_usdt: f64,
) -> Result<Vec<String>> {
    let info = client.exchange_info(None).await?;
    let tickers = client.tickers_24h().await?;
    let volume_by_symbol: HashMap<&str, f64> = tickers
        .iter()
        .map(|t| (t.symbol.as_str(), common::decimal_f64(t.quote_volume)))
        .collect();

    let mut candidates: Vec<(f64, String)> = info
        .symbols
        .iter()
        .filter(|s| s.status == "TRADING" && s.is_spot_trading_allowed && s.quote_asset == "USDT")
        .filter(|s| !fiat::is_fiat(&s.base_asset) && !is_leveraged_token(&s.base_asset))
        .filter_map(|s| {
            let volume = volume_by_symbol
                .get(s.symbol.as_str())
                .copied()
                .unwrap_or(0.0);
            (volume >= min_quote_volume_usdt).then(|| (volume, format!("{}/USDT", s.base_asset)))
        })
        .collect();
    sort_desc_by_volume(&mut candidates);
    Ok(candidates
        .into_iter()
        .take(max_symbols)
        .map(|(_, s)| s)
        .collect())
}

/// Python `candidates.sort(reverse=True)` on `(volume, symbol)` tuples.
pub(crate) fn sort_desc_by_volume(candidates: &mut [(f64, String)]) {
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
}

/// Printed before fetching when a dynamic universe is requested (insertion-ordered keys).
#[derive(Debug, Serialize)]
pub(crate) struct UniverseBanner<'a> {
    pub universe: &'a str,
    pub symbol_count: usize,
    pub first_10: &'a [String],
}

impl<'a> UniverseBanner<'a> {
    pub(crate) fn print(universe: &'a str, symbols: &'a [String]) -> Result<()> {
        let banner = Self {
            universe,
            symbol_count: symbols.len(),
            first_10: &symbols[..symbols.len().min(10)],
        };
        println!("{}", serde_json::to_string_pretty(&banner)?);
        Ok(())
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Comma-separated CCXT-style symbols (default: top liquid USDT pairs).
    #[arg(long, default_value = "BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT")]
    pub symbols: String,
    /// If set to spot_usdt_top, ignore --symbols and pull all Binance spot USDT pairs
    /// (ex fiats / leveraged tokens) ranked by 24h quote volume.
    #[arg(long, default_value = "", value_parser = clap::builder::PossibleValuesParser::new(["", "spot_usdt_top"]))]
    pub universe: String,
    /// Max symbols when --universe is set.
    #[arg(long, default_value_t = 200)]
    pub universe_size: usize,
    /// Minimum 24h quote volume in USDT for --universe filtering.
    #[arg(long, default_value_t = 1_000_000.0)]
    pub min_quote_volume_usdt: f64,
    /// Directory to cache fetched klines so re-runs are fast.
    #[arg(long, default_value = "")]
    pub klines_cache: String,
    /// Parallel kline fetches (respect Binance rate limits).
    #[arg(long, default_value_t = 6)]
    pub fetch_concurrency: i64,
    #[arg(long, default_value = "1h")]
    pub interval: String,
    /// ISO start timestamp; required unless --fixture is supplied.
    #[arg(long, default_value = "")]
    pub start: String,
    /// ISO end timestamp; defaults to now if --start is set.
    #[arg(long, default_value = "")]
    pub end: String,
    /// Optional path to a JSON {symbol: [kline,...]} file for offline runs.
    #[arg(long, default_value = "")]
    pub fixture: String,
    /// Optional path to dump full report JSON.
    #[arg(long, default_value = "")]
    pub output: String,
    #[arg(long, default_value_t = 24, allow_negative_numbers = true)]
    pub lookback_bars: i64,
    #[arg(long, default_value_t = 50.0, allow_negative_numbers = true)]
    pub min_edge_bps: f64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub taker_fee_bps: f64,
    #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
    pub slippage_bps: f64,
    #[arg(long, default_value_t = 10_000.0, allow_negative_numbers = true)]
    pub starting_balance: f64,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = BacktestConfig {
        interval: args.interval.clone(),
        taker_fee_bps: args.taker_fee_bps,
        slippage_bps: args.slippage_bps,
        lookback_bars: args.lookback_bars,
        min_edge_after_friction_bps: args.min_edge_bps,
        starting_balance: args.starting_balance,
        ..BacktestConfig::default()
    };

    let klines = if !args.fixture.is_empty() {
        load_kline_fixture(Path::new(&args.fixture))?
    } else {
        if args.start.is_empty() {
            return Ok(crate::cli::usage_error(
                "bot-spot-backtest",
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
        let symbols = if args.universe == "spot_usdt_top" {
            let symbols =
                fetch_spot_usdt_universe(&client, args.universe_size, args.min_quote_volume_usdt)
                    .await?;
            UniverseBanner::print("spot_usdt_top", &symbols)?;
            symbols
        } else {
            split_csv(&args.symbols)
        };
        let cache_dir = (!args.klines_cache.is_empty()).then(|| PathBuf::from(&args.klines_cache));
        fetch_klines_for_symbols(
            &client,
            &symbols,
            &args.interval,
            start,
            end,
            cache_dir.as_deref(),
            usize::try_from(args.fetch_concurrency).unwrap_or(1),
        )
        .await?
    };

    let result = replay(&klines, &cfg)?;
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

    fn kline(open_ms: i64, close: f64, spread_pct: f64) -> Bar {
        Bar::new(
            open_ms,
            close,
            close * (1.0 + spread_pct),
            close * (1.0 - spread_pct),
            close,
        )
    }

    fn flat_series(n: i64) -> Vec<Bar> {
        (0..n).map(|i| kline(i * HOUR_MS, 100.0, 0.001)).collect()
    }

    fn trending_series(n: i64, bar_return: f64) -> Vec<Bar> {
        let mut price = 100.0;
        (0..n)
            .map(|i| {
                let bar = kline(i * HOUR_MS, price, 0.001);
                price *= 1.0 + bar_return;
                bar
            })
            .collect()
    }

    fn set(entries: Vec<(&str, Vec<Bar>)>) -> KlineSet {
        entries
            .into_iter()
            .map(|(s, b)| (s.to_owned(), b))
            .collect()
    }

    fn cfg(lookback: i64) -> BacktestConfig {
        BacktestConfig {
            lookback_bars: lookback,
            ..BacktestConfig::default()
        }
    }

    #[test]
    fn half_spread_clipped_to_bounds() {
        let cfg = BacktestConfig::default();
        assert!((half_spread_bps(&kline(0, 100.0, 0.0), &cfg) - cfg.min_spread_bps).abs() < 1e-9);
        assert!((half_spread_bps(&kline(0, 100.0, 0.5), &cfg) - cfg.max_spread_bps).abs() < 1e-9);
    }

    #[test]
    fn holds_when_no_signal_clears_cost_bar() {
        let result = replay(&set(vec![("BTC/USDT", flat_series(200))]), &cfg(24)).unwrap();
        assert_eq!(result.num_trades, 0);
        assert!((result.final_balance - result.initial_balance).abs() < 1e-9);
        assert_eq!(result.verdict(), "no_trades");
        assert_eq!(result.bars_in_position_pct, 0.0);
    }

    #[test]
    fn captures_trend_when_edge_clears_cost_bar() {
        let result = replay(
            &set(vec![("BTC/USDT", trending_series(200, 0.005))]),
            &cfg(12),
        )
        .unwrap();
        assert!(result.num_trades >= 1);
        assert!(result.final_balance > result.initial_balance);
        assert!(result.total_return_pct > 0.0);
        assert!(result.bars_in_position_pct > 50.0);
        assert!(["edge_likely", "edge_marginal"].contains(&result.verdict()));
    }

    #[test]
    fn prefers_trending_over_flat_candidate() {
        let klines = set(vec![
            ("FLAT/USDT", flat_series(120)),
            ("TREND/USDT", trending_series(120, 0.004)),
        ]);
        let result = replay(&klines, &cfg(12)).unwrap();
        assert!(result.num_trades >= 1);
        let held = &result.per_asset_bars_held;
        let get = |k: &str| held.get(k).copied().unwrap_or(0);
        assert!(get("TREND/USDT") > get("FLAT/USDT"));
        assert!(get("TREND/USDT") > get(BASE_CASH));
    }

    #[test]
    fn handles_short_window_safely() {
        let result = replay(&set(vec![("BTC/USDT", flat_series(5))]), &cfg(24)).unwrap();
        assert_eq!(result.bars_evaluated, 0);
        assert_eq!(result.verdict(), "no_data");
        assert_eq!(result.final_balance, result.initial_balance);
    }

    #[test]
    fn rejects_invalid_config() {
        let klines = set(vec![("BTC/USDT", flat_series(50))]);
        let err = replay(&klines, &cfg(0)).unwrap_err();
        assert!(err.to_string().contains("lookback_bars"));
        let bad = BacktestConfig {
            starting_balance: 0.0,
            ..BacktestConfig::default()
        };
        assert!(
            replay(&klines, &bad)
                .unwrap_err()
                .to_string()
                .contains("starting_balance")
        );
    }

    #[test]
    fn summary_round_trips_as_json() {
        let result = replay(
            &set(vec![("BTC/USDT", trending_series(80, 0.003))]),
            &cfg(12),
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
        assert_eq!(
            decoded["final_balance"].as_f64().unwrap(),
            py_round(result.final_balance, 4)
        );
    }

    #[test]
    fn universe_filters_fiat_leveraged_and_volume() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            let sym = |s: &str, base: &str, quote: &str, status: &str| {
                serde_json::json!({"symbol": s, "status": status, "baseAsset": base, "quoteAsset": quote, "isSpotTradingAllowed": true})
            };
            Mock::given(method("GET"))
                .and(path("/api/v3/exchangeInfo"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"symbols": [
                    sym("BTCUSDT", "BTC", "USDT", "TRADING"),
                    sym("ETHUSDT", "ETH", "USDT", "TRADING"),
                    sym("EURUSDT", "EUR", "USDT", "TRADING"),
                    sym("BTCUPUSDT", "BTCUP", "USDT", "TRADING"),
                    sym("LUNAUSDT", "LUNA", "USDT", "BREAK"),
                    sym("ETHBTC", "ETH", "BTC", "TRADING"),
                    sym("DOGEUSDT", "DOGE", "USDT", "TRADING"),
                ]})))
                .mount(&server)
                .await;
            let t = |s: &str, v: &str| serde_json::json!({"symbol": s, "quoteVolume": v});
            Mock::given(method("GET"))
                .and(path("/api/v3/ticker/24hr"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    t("BTCUSDT", "5000000"), t("ETHUSDT", "9000000"), t("EURUSDT", "99000000"),
                    t("BTCUPUSDT", "99000000"), t("DOGEUSDT", "10"),
                ])))
                .mount(&server)
                .await;
            let client = BinanceClient::new(bb_binance::BinanceConfig::new(false).with_spot_rest_url(server.uri())).unwrap();
            let universe = fetch_spot_usdt_universe(&client, 10, 1_000_000.0).await.unwrap();
            assert_eq!(universe, vec!["ETH/USDT".to_owned(), "BTC/USDT".to_owned()]);
        });
    }

    #[test]
    fn klines_cache_round_trip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/v3/klines"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    [
                        0, "1.0", "2.0", "0.5", "1.5", "10", 3_599_999, "15", 3, "5", "7.5", "0"
                    ],
                    [
                        3_600_000, "1.5", "2.5", "1.0", "2.0", "10", 7_199_999, "20", 3, "5", "10",
                        "0"
                    ],
                ])))
                .expect(1)
                .mount(&server)
                .await;
            let client = BinanceClient::new(
                bb_binance::BinanceConfig::new(false).with_spot_rest_url(server.uri()),
            )
            .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let start = parse_iso("1970-01-01").unwrap();
            let end = parse_iso("1970-01-01T02:00:00").unwrap();
            let symbols = vec!["BTC/USDT".to_owned()];
            let first =
                fetch_klines_for_symbols(&client, &symbols, "1h", start, end, Some(dir.path()), 6)
                    .await
                    .unwrap();
            let cache = dir.path().join("1h_BTC_USDT_0_7200.json");
            let raw: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&cache).unwrap()).unwrap();
            assert_eq!(raw[0][1], "1.0");
            assert_eq!(raw[1][0], 3_600_000);
            // Second call is served from the cache (mock expects exactly one hit).
            let second =
                fetch_klines_for_symbols(&client, &symbols, "1h", start, end, Some(dir.path()), 6)
                    .await
                    .unwrap();
            assert_eq!(first, second);
            assert_eq!(
                first["BTC/USDT"][1],
                Bar::new(3_600_000, 1.5, 2.5, 1.0, 2.0)
            );
        });
    }

    #[tokio::test]
    async fn cli_runs_against_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("klines.json");
        let rows: Vec<Vec<f64>> = trending_series(80, 0.004)
            .iter()
            .map(|b| vec![b.open_ms as f64, b.open, b.high, b.low, b.close, 1.0])
            .collect();
        std::fs::write(
            &fixture,
            serde_json::to_string(&serde_json::json!({"BTC/USDT": rows})).unwrap(),
        )
        .unwrap();
        let output = dir.path().join("report.json");
        let args = Args {
            symbols: String::new(),
            universe: String::new(),
            universe_size: 200,
            min_quote_volume_usdt: 1e6,
            klines_cache: String::new(),
            fetch_concurrency: 6,
            interval: "1h".into(),
            start: String::new(),
            end: String::new(),
            fixture: fixture.display().to_string(),
            output: output.display().to_string(),
            lookback_bars: 12,
            min_edge_bps: 50.0,
            taker_fee_bps: 10.0,
            slippage_bps: 5.0,
            starting_balance: 10_000.0,
        };
        assert_eq!(run_cli(args).await.unwrap(), ExitCode::SUCCESS);
        let full: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert!(full.get("trades").is_some());
        assert!(full.get("config").is_some());
        assert!(full["bars_evaluated"].as_u64().unwrap() > 0);
    }
}

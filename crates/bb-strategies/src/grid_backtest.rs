//! Historical replay harness for symmetric grid trading on a single pair (port of
//! `src/grid_backtest.py`).
//!
//! Grid trading places limit buys below the current price and limit sells above, evenly
//! spaced across a configured range. When a bar's `[low, high]` range touches a level with
//! an open order the order fills and the opposite-side order is queued at the adjacent
//! level. Only direct fills are modelled (no intra-bar oscillation chains), so the trade
//! count is a conservative lower bound on what a real venue would produce.
//!
//! The metric that matters is total return *after fees*: the edge is mechanical (capture
//! spread) but the risk is regime-dependent (trends strand inventory or cash).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bb_binance::BinanceClient;
use bb_binance::history::HistoryFetcher;
use chrono::{DateTime, Utc};
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::common::{
    self, Bar, bars_per_year, kline_row, parse_iso, py_float_repr, py_round, py_sum,
    sharpe_annualized,
};
use crate::error::{Result, StrategyError};

/// Symmetric grid parameters. `maker_fee_bps` defaults to Binance VIP0 maker (10 bps);
/// `qty_per_level` is fixed in base units so the per-trade quote notional varies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridConfig {
    pub symbol: String,
    pub grid_low: f64,
    pub grid_high: f64,
    pub num_levels: i64,
    pub qty_per_level: f64,
    pub maker_fee_bps: f64,
    /// Passive fills, minimal slippage.
    pub slippage_bps: f64,
    pub starting_quote: f64,
    pub interval: String,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            symbol: "BTC/USDT".into(),
            grid_low: 70_000.0,
            grid_high: 90_000.0,
            num_levels: 20,
            qty_per_level: 0.001,
            maker_fee_bps: 10.0,
            slippage_bps: 1.0,
            starting_quote: 1_000.0,
            interval: "1h".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FillSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GridFill {
    pub timestamp_ms: i64,
    pub side: FillSide,
    pub price: f64,
    pub qty: f64,
    pub fee_quote: f64,
}

/// Fill counts per grid level (rounded to 8 dp), in first-fill order. Serialises as the
/// Python `dict[float, int]` did: keys are the float `repr` strings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FillsByLevel(pub Vec<(f64, usize)>);

impl FillsByLevel {
    /// Fill count recorded at `level` (keyed by `round(level, 8)`).
    pub fn get(&self, level: f64) -> usize {
        let bits = level_key(level).1;
        self.0
            .iter()
            .find(|(k, _)| k.to_bits() == bits)
            .map_or(0, |(_, n)| *n)
    }
}

impl Serialize for FillsByLevel {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_map(
            self.0
                .iter()
                .map(|(level, count)| (py_float_repr(*level), count)),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GridResult {
    pub config: GridConfig,
    pub initial_cash: f64,
    pub initial_inventory: f64,
    pub final_cash: f64,
    pub final_inventory: f64,
    pub final_mark_to_market: f64,
    pub final_equity: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub realized_pnl_quote: f64,
    pub fees_paid_quote: f64,
    pub num_buys: usize,
    pub num_sells: usize,
    pub num_round_trips: usize,
    pub bars_evaluated: usize,
    pub bars_with_fills: usize,
    pub bars_in_range_pct: f64,
    pub max_drawdown_pct: f64,
    pub sharpe_annualized: f64,
    pub fills_by_level: FillsByLevel,
    pub equity_curve: Vec<(i64, f64)>,
    pub fills: Vec<GridFill>,
}

impl GridResult {
    fn empty(cfg: &GridConfig) -> Self {
        Self {
            config: cfg.clone(),
            initial_cash: cfg.starting_quote,
            initial_inventory: 0.0,
            final_cash: cfg.starting_quote,
            final_inventory: 0.0,
            final_mark_to_market: 0.0,
            final_equity: cfg.starting_quote,
            total_return_pct: 0.0,
            annualized_return_pct: 0.0,
            realized_pnl_quote: 0.0,
            fees_paid_quote: 0.0,
            num_buys: 0,
            num_sells: 0,
            num_round_trips: 0,
            bars_evaluated: 0,
            bars_with_fills: 0,
            bars_in_range_pct: 0.0,
            max_drawdown_pct: 0.0,
            sharpe_annualized: 0.0,
            fills_by_level: FillsByLevel::default(),
            equity_curve: Vec::new(),
            fills: Vec::new(),
        }
    }

    pub fn verdict(&self) -> &'static str {
        common::verdict(
            self.bars_evaluated,
            self.num_round_trips,
            self.sharpe_annualized,
            self.total_return_pct,
        )
    }
}

/// Evenly spaced grid levels from `grid_low` to `grid_high` inclusive.
pub fn build_levels(cfg: &GridConfig) -> Result<Vec<f64>> {
    if cfg.num_levels < 2 {
        return Err(StrategyError::InvalidConfig(
            "num_levels must be at least 2".into(),
        ));
    }
    if cfg.grid_high <= cfg.grid_low {
        return Err(StrategyError::InvalidConfig(
            "grid_high must be greater than grid_low".into(),
        ));
    }
    let step = (cfg.grid_high - cfg.grid_low) / (cfg.num_levels - 1) as f64;
    Ok((0..cfg.num_levels)
        .map(|i| cfg.grid_low + i as f64 * step)
        .collect())
}

/// `round(level, 8)` as a set key (`-0.0` and `0.0` compare equal in Python sets).
fn level_key(level: f64) -> (f64, u64) {
    let rounded = py_round(level, 8);
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    (rounded, rounded.to_bits())
}

#[derive(Default)]
struct LevelCounter {
    index: HashMap<u64, usize>,
    counts: FillsByLevel,
}

impl LevelCounter {
    fn record(&mut self, (level, bits): (f64, u64)) {
        let counts = &mut self.counts.0;
        let slot = *self.index.entry(bits).or_insert_with(|| {
            counts.push((level, 0));
            counts.len() - 1
        });
        counts[slot].1 += 1;
    }
}

/// Replay grid trading against historical klines.
///
/// The initial state distributes the starting quote so inventory covers sells at every
/// level above the opening price and cash covers buys at every level at or below it.
pub fn replay_grid(klines: &[Bar], cfg: &GridConfig) -> Result<GridResult> {
    if cfg.starting_quote <= 0.0 {
        return Err(StrategyError::InvalidConfig(
            "starting_quote must be positive".into(),
        ));
    }
    if cfg.qty_per_level <= 0.0 {
        return Err(StrategyError::InvalidConfig(
            "qty_per_level must be positive".into(),
        ));
    }
    let (Some(first), Some(last)) = (klines.first(), klines.last()) else {
        return Ok(GridResult::empty(cfg));
    };

    let levels = build_levels(cfg)?;
    let keys: Vec<(f64, u64)> = levels.iter().map(|&l| level_key(l)).collect();
    let grid_spacing = levels[1] - levels[0];
    let grid_bottom = levels[0];
    let grid_top = levels[levels.len() - 1];
    let fee_rate = cfg.maker_fee_bps / 10_000.0;
    let slip_rate = cfg.slippage_bps / 10_000.0;

    let opening_price = first.open;
    let levels_above: Vec<usize> = (0..levels.len())
        .filter(|&i| levels[i] > opening_price)
        .collect();
    let levels_below: Vec<usize> = (0..levels.len())
        .filter(|&i| levels[i] <= opening_price)
        .collect();
    let inventory_needed = cfg.qty_per_level * levels_above.len() as f64;
    let cash_for_buys = py_sum(levels_below.iter().map(|&i| cfg.qty_per_level * levels[i]));
    let cost_to_acquire_inventory = inventory_needed * opening_price;
    let total_required = cost_to_acquire_inventory + cash_for_buys;

    // Not enough capital to populate the configured grid at this price: pro-rate
    // qty_per_level down so the capital fits exactly.
    let adjusted_qty = if total_required > cfg.starting_quote * 1.0001 {
        cfg.qty_per_level * (cfg.starting_quote / total_required)
    } else {
        cfg.qty_per_level
    };

    let mut inventory = adjusted_qty * levels_above.len() as f64;
    let mut cash = cfg.starting_quote - inventory * opening_price;
    let initial_cash = cash;
    let initial_inventory = inventory;

    let mut open_buys: HashSet<u64> = levels_below.iter().map(|&i| keys[i].1).collect();
    let mut open_sells: HashSet<u64> = levels_above.iter().map(|&i| keys[i].1).collect();

    let mut fills: Vec<GridFill> = Vec::new();
    let mut fills_by_level = LevelCounter::default();
    let mut realized_pnl_quote = 0.0;
    let mut fees_paid_quote = 0.0;
    let mut equity_curve: Vec<(i64, f64)> = Vec::with_capacity(klines.len());
    let mut returns: Vec<f64> = Vec::with_capacity(klines.len());
    let mut bars_with_fills = 0_usize;
    let mut bars_in_range = 0_usize;
    let mut last_equity = initial_cash + initial_inventory * opening_price;
    let mut peak_equity = last_equity;
    let mut max_drawdown_pct = 0.0_f64;

    for bar in klines {
        let ts = bar.open_ms;
        if bar.high >= grid_bottom && bar.low <= grid_top {
            bars_in_range += 1;
        }

        let mut bar_fills = 0_usize;
        // Walk levels in ascending order; one pass per bar (no intra-bar chains beyond it).
        for (&level, &key) in levels.iter().zip(&keys) {
            if level < bar.low || level > bar.high {
                continue;
            }
            if open_buys.contains(&key.1) {
                let qty = adjusted_qty;
                let cost = qty * level * (1.0 + fee_rate + slip_rate);
                if cost <= cash {
                    cash -= cost;
                    inventory += qty;
                    let fee_quote = qty * level * (fee_rate + slip_rate);
                    fees_paid_quote += fee_quote;
                    open_buys.remove(&key.1);
                    let sell_level = level + grid_spacing;
                    if sell_level <= cfg.grid_high + 1e-9 {
                        open_sells.insert(level_key(sell_level).1);
                    }
                    fills.push(GridFill {
                        timestamp_ms: ts,
                        side: FillSide::Buy,
                        price: level,
                        qty,
                        fee_quote,
                    });
                    fills_by_level.record(key);
                    bar_fills += 1;
                }
            }
            if open_sells.contains(&key.1) && inventory >= adjusted_qty {
                let qty = adjusted_qty;
                let proceeds = qty * level * (1.0 - fee_rate - slip_rate);
                cash += proceeds;
                inventory -= qty;
                let fee_quote = qty * level * (fee_rate + slip_rate);
                fees_paid_quote += fee_quote;
                // Realized P&L approximation: one spacing above the implicit cost basis.
                realized_pnl_quote += grid_spacing * qty - 2.0 * fee_quote;
                open_sells.remove(&key.1);
                let buy_level = level - grid_spacing;
                if buy_level >= cfg.grid_low - 1e-9 {
                    open_buys.insert(level_key(buy_level).1);
                }
                fills.push(GridFill {
                    timestamp_ms: ts,
                    side: FillSide::Sell,
                    price: level,
                    qty,
                    fee_quote,
                });
                fills_by_level.record(key);
                bar_fills += 1;
            }
        }
        if bar_fills > 0 {
            bars_with_fills += 1;
        }

        let equity = cash + inventory * bar.close;
        if last_equity > 0.0 {
            returns.push(equity / last_equity - 1.0);
        }
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

    let mtm = inventory * last.close;
    let equity = cash + mtm;
    let total_return_pct = (equity / cfg.starting_quote - 1.0) * 100.0;
    let bars_evaluated = klines.len();
    let bpy = bars_per_year(&cfg.interval, 8_760.0);
    let years = if bpy > 0.0 {
        bars_evaluated as f64 / bpy
    } else {
        0.0
    };
    let annualized_return_pct = if years > 0.0 && equity > 0.0 {
        ((equity / cfg.starting_quote).powf(1.0 / years) - 1.0) * 100.0
    } else {
        0.0
    };
    let num_buys = fills.iter().filter(|f| f.side == FillSide::Buy).count();
    let num_sells = fills.len() - num_buys;

    Ok(GridResult {
        config: cfg.clone(),
        initial_cash,
        initial_inventory,
        final_cash: cash,
        final_inventory: inventory,
        final_mark_to_market: mtm,
        final_equity: equity,
        total_return_pct,
        annualized_return_pct,
        realized_pnl_quote,
        fees_paid_quote,
        num_buys,
        num_sells,
        num_round_trips: num_buys.min(num_sells),
        bars_evaluated,
        bars_with_fills,
        bars_in_range_pct: bars_in_range as f64 / bars_evaluated as f64 * 100.0,
        max_drawdown_pct,
        sharpe_annualized: sharpe_annualized(&returns, bpy),
        fills_by_level: fills_by_level.counts,
        equity_curve,
        fills,
    })
}

/// Compact CLI summary (Python `_result_summary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub config: GridConfig,
    pub initial_cash: f64,
    pub initial_inventory: f64,
    pub final_cash: f64,
    pub final_inventory: f64,
    pub final_mark_to_market: f64,
    pub final_equity: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub realized_pnl_quote: f64,
    pub fees_paid_quote: f64,
    pub num_buys: usize,
    pub num_sells: usize,
    pub num_round_trips: usize,
    pub bars_evaluated: usize,
    pub bars_with_fills: usize,
    pub bars_in_range_pct: f64,
    pub max_drawdown_pct: f64,
    pub sharpe_annualized: f64,
}

pub fn result_summary(result: &GridResult) -> ResultSummary {
    ResultSummary {
        verdict: result.verdict(),
        config: result.config.clone(),
        initial_cash: py_round(result.initial_cash, 4),
        initial_inventory: py_round(result.initial_inventory, 8),
        final_cash: py_round(result.final_cash, 4),
        final_inventory: py_round(result.final_inventory, 8),
        final_mark_to_market: py_round(result.final_mark_to_market, 4),
        final_equity: py_round(result.final_equity, 4),
        total_return_pct: py_round(result.total_return_pct, 4),
        annualized_return_pct: py_round(result.annualized_return_pct, 4),
        realized_pnl_quote: py_round(result.realized_pnl_quote, 4),
        fees_paid_quote: py_round(result.fees_paid_quote, 4),
        num_buys: result.num_buys,
        num_sells: result.num_sells,
        num_round_trips: result.num_round_trips,
        bars_evaluated: result.bars_evaluated,
        bars_with_fills: result.bars_with_fills,
        bars_in_range_pct: py_round(result.bars_in_range_pct, 2),
        max_drawdown_pct: py_round(result.max_drawdown_pct, 4),
        sharpe_annualized: py_round(result.sharpe_annualized, 4),
    }
}

/// `{cache_dir}/grid_{interval}_{SYM}_{startTs}_{endTs}.json`.
pub fn grid_cache_path(
    cache_dir: &Path,
    interval: &str,
    symbol: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> PathBuf {
    cache_dir.join(format!(
        "grid_{interval}_{}_{}_{}.json",
        symbol.replace('/', "_"),
        start.timestamp(),
        end.timestamp()
    ))
}

/// Spot klines for one symbol with the optional disk cache (unreadable cache → refetch).
pub async fn fetch_klines(
    client: &BinanceClient,
    symbol: &str,
    interval: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    cache_dir: Option<&Path>,
) -> Result<Vec<Bar>> {
    let cache_path = cache_dir.map(|dir| grid_cache_path(dir, interval, symbol, start, end));
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

pub(crate) fn read_cached_bars(path: &Path) -> Option<Vec<Bar>> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn write_cached_rows(path: &Path, klines: &[bb_binance::models::Kline]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| StrategyError::io(parent, e))?;
    }
    let rows: Vec<serde_json::Value> = klines.iter().map(kline_row).collect();
    std::fs::write(path, serde_json::to_vec(&rows)?).map_err(|e| StrategyError::io(path, e))
}

const FIXTURE_SHAPE_ERROR: &str = "klines fixture must be a list or a {symbol: klines} mapping";

/// A `[kline, ...]` list, or a `{symbol: klines}` mapping whose *first* entry (document
/// order, like Python's insertion-ordered dict) is used.
struct GridFixture(Vec<Bar>);

impl<'de> Deserialize<'de> for GridFixture {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct FixtureVisitor;
        impl<'de> Visitor<'de> for FixtureVisitor {
            type Value = GridFixture;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(FIXTURE_SHAPE_ERROR)
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<GridFixture, A::Error> {
                let mut bars = Vec::new();
                while let Some(bar) = seq.next_element::<Bar>()? {
                    bars.push(bar);
                }
                Ok(GridFixture(bars))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<GridFixture, A::Error> {
                let Some((_, bars)) = map.next_entry::<IgnoredAny, Vec<Bar>>()? else {
                    return Err(de::Error::custom(FIXTURE_SHAPE_ERROR));
                };
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(GridFixture(bars))
            }
        }
        deserializer.deserialize_any(FixtureVisitor)
    }
}

/// Python `_load_fixture`.
pub fn load_fixture(path: &Path) -> Result<Vec<Bar>> {
    let text = std::fs::read_to_string(path).map_err(|e| StrategyError::io(path, e))?;
    let fixture: GridFixture =
        serde_json::from_str(&text).map_err(|e| common::json_error(path, &e))?;
    Ok(fixture.0)
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
    /// JSON `[kline, ...]` or `{symbol: [kline, ...]}` file for offline runs.
    #[arg(long, default_value = "")]
    pub fixture: String,
    /// Directory to cache fetched klines.
    #[arg(long, default_value = "")]
    pub klines_cache: String,
    /// Optional path to dump the full report JSON.
    #[arg(long, default_value = "")]
    pub output: String,
    #[arg(long, default_value_t = 70_000.0, allow_negative_numbers = true)]
    pub grid_low: f64,
    #[arg(long, default_value_t = 90_000.0, allow_negative_numbers = true)]
    pub grid_high: f64,
    #[arg(long, default_value_t = 20, allow_negative_numbers = true)]
    pub num_levels: i64,
    #[arg(long, default_value_t = 0.001, allow_negative_numbers = true)]
    pub qty_per_level: f64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub maker_fee_bps: f64,
    #[arg(long, default_value_t = 1.0, allow_negative_numbers = true)]
    pub slippage_bps: f64,
    #[arg(long, default_value_t = 1_000.0, allow_negative_numbers = true)]
    pub starting_quote: f64,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = GridConfig {
        symbol: args.symbol.clone(),
        interval: args.interval.clone(),
        grid_low: args.grid_low,
        grid_high: args.grid_high,
        num_levels: args.num_levels,
        qty_per_level: args.qty_per_level,
        maker_fee_bps: args.maker_fee_bps,
        slippage_bps: args.slippage_bps,
        starting_quote: args.starting_quote,
    };

    let klines = if !args.fixture.is_empty() {
        load_fixture(Path::new(&args.fixture))?
    } else {
        if args.start.is_empty() {
            return Ok(crate::cli::usage_error(
                "grid-backtest",
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
        fetch_klines(
            &client,
            &args.symbol,
            &args.interval,
            start,
            end,
            cache_dir.as_deref(),
        )
        .await?
    };

    let result = replay_grid(&klines, &cfg)?;
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

    fn bar(open_ms: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar::new(open_ms, o, h, l, c)
    }

    fn grid(low: f64, high: f64, levels: i64, qty: f64, starting_quote: f64) -> GridConfig {
        GridConfig {
            grid_low: low,
            grid_high: high,
            num_levels: levels,
            qty_per_level: qty,
            starting_quote,
            ..GridConfig::default()
        }
    }

    fn approx(a: f64, b: f64, rel: f64) -> bool {
        (a - b).abs() <= rel * b.abs().max(1e-12)
    }

    #[test]
    fn build_levels_returns_evenly_spaced_grid() {
        let levels = build_levels(&grid(100.0, 200.0, 11, 0.001, 1_000.0)).unwrap();
        assert!(approx(levels[0], 100.0, 1e-6));
        assert!(approx(levels[10], 200.0, 1e-6));
        assert_eq!(levels.len(), 11);
        assert!(
            levels
                .windows(2)
                .all(|w| ((w[1] - w[0]) - 10.0).abs() < 1e-9)
        );
    }

    #[test]
    fn oscillating_price_captures_round_trip_spread() {
        let klines = vec![
            bar(0, 100.0, 110.0, 100.0, 110.0),
            bar(HOUR_MS, 110.0, 110.0, 100.0, 100.0),
            bar(2 * HOUR_MS, 100.0, 110.0, 100.0, 110.0),
            bar(3 * HOUR_MS, 110.0, 110.0, 100.0, 100.0),
        ];
        let cfg = GridConfig {
            maker_fee_bps: 10.0,
            slippage_bps: 0.0,
            ..grid(100.0, 110.0, 2, 1.0, 300.0)
        };
        let result = replay_grid(&klines, &cfg).unwrap();
        assert!(result.num_buys >= 1);
        assert!(result.num_sells >= 1);
        assert!(result.final_equity > 0.0);
        assert!(result.bars_with_fills > 0);
    }

    #[test]
    fn trending_market_strands_inventory() {
        let mut price = 100.0;
        let klines: Vec<Bar> = (0..20)
            .map(|i| {
                let b = bar(i * HOUR_MS, price, price + 5.0, price, price + 5.0);
                price += 5.0;
                b
            })
            .collect();
        let cfg = GridConfig {
            slippage_bps: 0.0,
            ..grid(100.0, 200.0, 11, 0.05, 200.0)
        };
        let result = replay_grid(&klines, &cfg).unwrap();
        assert!(result.num_sells >= 1);
        assert!(result.final_inventory < cfg.qty_per_level * 2.0);
    }

    #[test]
    fn flat_market_makes_no_trades() {
        let klines: Vec<Bar> = (0..20)
            .map(|i| bar(i * HOUR_MS, 155.0, 155.0, 155.0, 155.0))
            .collect();
        let result = replay_grid(&klines, &grid(100.0, 200.0, 11, 0.01, 200.0)).unwrap();
        assert_eq!(result.num_buys, 0);
        assert_eq!(result.num_sells, 0);
        assert_eq!(result.verdict(), "no_trades");
    }

    #[test]
    fn zero_klines_returns_no_data() {
        let result = replay_grid(&[], &GridConfig::default()).unwrap();
        assert_eq!(result.verdict(), "no_data");
        assert!(approx(
            result.final_equity,
            result.config.starting_quote,
            1e-9
        ));
    }

    #[test]
    fn rejects_invalid_grid() {
        let klines = [bar(0, 100.0, 100.0, 100.0, 100.0)];
        let one_level = GridConfig {
            num_levels: 1,
            ..GridConfig::default()
        };
        assert!(
            replay_grid(&klines, &one_level)
                .unwrap_err()
                .to_string()
                .contains("num_levels")
        );
        let inverted = grid(200.0, 100.0, 2, 0.001, 1_000.0);
        assert!(
            replay_grid(&klines, &inverted)
                .unwrap_err()
                .to_string()
                .contains("grid_high")
        );
    }

    #[test]
    fn rejects_non_positive_qty() {
        let klines = [bar(0, 100.0, 100.0, 100.0, 100.0)];
        let cfg = GridConfig {
            qty_per_level: 0.0,
            ..GridConfig::default()
        };
        assert!(
            replay_grid(&klines, &cfg)
                .unwrap_err()
                .to_string()
                .contains("qty_per_level")
        );
    }

    #[test]
    fn rescales_qty_when_capital_insufficient() {
        let klines = [bar(0, 150.0, 150.0, 150.0, 150.0)];
        let cfg = grid(100.0, 200.0, 100, 1.0, 200.0);
        let result = replay_grid(&klines, &cfg).unwrap();
        assert!(approx(result.final_equity, cfg.starting_quote, 0.01));
    }

    #[test]
    fn summary_round_trips_as_json() {
        let klines: Vec<Bar> = (0..4)
            .map(|i| bar(i * HOUR_MS, 100.0, 110.0, 100.0, 110.0))
            .collect();
        let result = replay_grid(&klines, &grid(100.0, 110.0, 2, 0.1, 200.0)).unwrap();
        let decoded: serde_json::Value =
            serde_json::from_str(&common::to_pretty_sorted(&result_summary(&result)).unwrap())
                .unwrap();
        assert_eq!(decoded["verdict"], result.verdict());
        assert_eq!(
            decoded["num_round_trips"].as_u64().unwrap() as usize,
            result.num_round_trips
        );
    }

    #[test]
    fn fills_by_level_serialises_python_float_keys() {
        let klines: Vec<Bar> = (0..4)
            .map(|i| {
                if i % 2 == 0 {
                    bar(i * HOUR_MS, 100.0, 110.0, 100.0, 110.0)
                } else {
                    bar(i * HOUR_MS, 110.0, 110.0, 100.0, 100.0)
                }
            })
            .collect();
        let result = replay_grid(&klines, &grid(100.0, 110.0, 2, 0.1, 200.0)).unwrap();
        assert!(result.fills_by_level.get(110.0) >= 1);
        let full = common::sorted_value(&result).unwrap();
        assert!(full["fills_by_level"].get("110.0").is_some());
        assert_eq!(full["fills"][0]["side"], "buy");
        assert!(full["equity_curve"][0].is_array());
    }

    #[test]
    fn python_helpers_match_cpython() {
        assert_eq!(py_float_repr(5.0), "5.0");
        assert_eq!(py_float_repr(-0.0), "-0.0");
        assert_eq!(py_float_repr(0.3), "0.3");
        assert_eq!(py_float_repr(0.0001), "0.0001");
        assert_eq!(py_float_repr(1e-5), "1e-05");
        assert_eq!(py_float_repr(1.5e16), "1.5e+16");
        assert_eq!(py_float_repr(105.55555556), "105.55555556");
        // sum([0.1] * 10) == 1.0 on CPython 3.12+ (compensated); naive summation gives 0.9999999999999999.
        assert_eq!(py_sum([0.1; 10]), 1.0);
        assert_eq!(py_sum([1e100, 1.0, -1e100, 1.0]), 2.0);
        assert_eq!(py_sum(std::iter::empty()), 0.0);
    }

    #[test]
    fn fixture_accepts_list_or_first_mapping_entry() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("list.json");
        std::fs::write(&list, "[[0, 1, 2, 0.5, 1.5, 9]]").unwrap();
        assert_eq!(
            load_fixture(&list).unwrap(),
            vec![bar(0, 1.0, 2.0, 0.5, 1.5)]
        );
        // Document order wins (Python dict insertion order), not key order.
        let map = dir.path().join("map.json");
        std::fs::write(
            &map,
            r#"{"ZZZ/USDT": [[1, 1, 1, 1, 1]], "AAA/USDT": [[2, 2, 2, 2, 2]]}"#,
        )
        .unwrap();
        assert_eq!(load_fixture(&map).unwrap()[0].open_ms, 1);
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "{}").unwrap();
        assert!(
            load_fixture(&empty)
                .unwrap_err()
                .to_string()
                .contains(FIXTURE_SHAPE_ERROR)
        );
    }

    #[tokio::test]
    async fn fetch_klines_uses_cache() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .and(query_param("symbol", "BTCUSDT"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([[
                    0, "1.0", "2.0", "0.5", "1.5", "10", 3_599_999, "15", 3, "5", "7.5", "0"
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
        let start = parse_iso("1970-01-01").unwrap();
        let end = parse_iso("1970-01-01T02:00:00").unwrap();
        let first = fetch_klines(&client, "BTC/USDT", "1h", start, end, Some(dir.path()))
            .await
            .unwrap();
        assert!(dir.path().join("grid_1h_BTC_USDT_0_7200.json").exists());
        let second = fetch_klines(&client, "BTC/USDT", "1h", start, end, Some(dir.path()))
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first, vec![bar(0, 1.0, 2.0, 0.5, 1.5)]);
    }

    #[tokio::test]
    async fn cli_runs_against_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("klines.json");
        let rows: Vec<Vec<f64>> = (0..8)
            .map(|i| {
                let t = (i * HOUR_MS) as f64;
                if i % 2 == 0 {
                    vec![t, 100.0, 110.0, 100.0, 110.0, 1.0]
                } else {
                    vec![t, 110.0, 110.0, 100.0, 100.0, 1.0]
                }
            })
            .collect();
        std::fs::write(&fixture, serde_json::to_string(&rows).unwrap()).unwrap();
        let output = dir.path().join("report.json");
        let args = Args {
            symbol: "BTC/USDT".into(),
            interval: "1h".into(),
            start: String::new(),
            end: String::new(),
            fixture: fixture.display().to_string(),
            klines_cache: String::new(),
            output: output.display().to_string(),
            grid_low: 100.0,
            grid_high: 110.0,
            num_levels: 2,
            qty_per_level: 0.1,
            maker_fee_bps: 10.0,
            slippage_bps: 1.0,
            starting_quote: 200.0,
        };
        assert_eq!(run_cli(args).await.unwrap(), ExitCode::SUCCESS);
        let full: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(full["bars_evaluated"], 8);
        assert!(full.get("fills").is_some());
    }
}

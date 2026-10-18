//! Historical replay harness for delta-neutral funding-rate harvesting (port of
//! `src/funding_arb_backtest.py`).
//!
//! Long spot + short the same USD-M perp 1:1; every funding event pays
//! `rate × notional` to the short. Price moves wash out at delta-neutral, leaving
//! funding income minus four taker fees and four slippage legs. A positive verdict
//! justifies the live execution path (`FUNDING_ARB_PROVEN_EDGE=yes`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bb_binance::BinanceClient;
use bb_binance::futures::RangeQuery;
use chrono::{DateTime, Utc};
use futures::{StreamExt as _, TryStreamExt as _};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::bot_spot_backtest::{UniverseBanner, sort_desc_by_volume};
use crate::common::{
    self, json_error, parse_iso, py_round, sharpe_annualized, split_csv, value_f64, value_i64,
};
use crate::error::{Result, StrategyError};

/// One funding event row: `{"symbol", "fundingTime", "fundingRate", "markPrice"}`.
/// Rate and mark price stay raw JSON scalars (string or number) so cache files
/// round-trip byte-compatible with the Python harness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundingRow {
    #[serde(default)]
    pub symbol: String,
    #[serde(rename = "fundingTime", deserialize_with = "de_time")]
    pub funding_time: i64,
    #[serde(rename = "fundingRate")]
    pub funding_rate: Value,
    #[serde(rename = "markPrice", default)]
    pub mark_price: Value,
}

fn de_time<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<i64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value_i64(&value)
        .ok_or_else(|| serde::de::Error::custom(format!("invalid fundingTime: {value}")))
}

impl FundingRow {
    /// `float(fundingRate) * 10_000`.
    pub fn rate_bps(&self) -> Result<f64> {
        value_f64(&self.funding_rate)
            .map(|rate| rate * 10_000.0)
            .ok_or_else(|| {
                StrategyError::InvalidConfig(format!("invalid fundingRate: {}", self.funding_rate))
            })
    }
}

pub type FundingSet = BTreeMap<String, Vec<FundingRow>>;

/// Cost-aware funding-harvest knobs (Binance VIP0: 10 bps spot, 5 bps futures taker).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundingArbConfig {
    pub entry_funding_bps: f64,
    pub exit_funding_bps: f64,
    pub max_hold_funding_events: i64,
    pub taker_fee_spot_bps: f64,
    pub taker_fee_futures_bps: f64,
    pub slippage_per_leg_bps: f64,
    pub max_concurrent_positions: i64,
    pub per_position_notional_pct: f64,
    pub starting_balance: f64,
}

impl Default for FundingArbConfig {
    fn default() -> Self {
        Self {
            entry_funding_bps: 1.0,
            exit_funding_bps: -0.5,
            max_hold_funding_events: 90,
            taker_fee_spot_bps: 10.0,
            taker_fee_futures_bps: 5.0,
            slippage_per_leg_bps: 2.0,
            max_concurrent_positions: 5,
            per_position_notional_pct: 20.0,
            starting_balance: 10_000.0,
        }
    }
}

impl FundingArbConfig {
    /// Open (or close) side: spot fee + futures fee + two slippage legs.
    pub fn one_side_cost_bps(&self) -> f64 {
        self.taker_fee_spot_bps + self.taker_fee_futures_bps + 2.0 * self.slippage_per_leg_bps
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FundingTrade {
    pub symbol: String,
    pub open_time_ms: i64,
    pub close_time_ms: i64,
    pub entry_funding_bps: f64,
    pub funding_received_bps: f64,
    pub funding_events_held: i64,
    pub round_trip_cost_bps: f64,
    pub net_pnl_bps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FundingBacktestResult {
    pub config: FundingArbConfig,
    pub initial_balance: f64,
    pub final_balance: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub win_rate_pct: f64,
    pub avg_holding_events: f64,
    pub funding_collected_bps: f64,
    pub fees_paid_bps: f64,
    pub bars_in_position_pct: f64,
    pub bars_evaluated: usize,
    pub equity_curve: Vec<(i64, f64)>,
    pub trades: Vec<FundingTrade>,
}

impl FundingBacktestResult {
    fn empty(cfg: &FundingArbConfig) -> Self {
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
            avg_holding_events: 0.0,
            funding_collected_bps: 0.0,
            fees_paid_bps: 0.0,
            bars_in_position_pct: 0.0,
            bars_evaluated: 0,
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
    symbol: String,
    open_time_ms: i64,
    entry_funding_bps: f64,
    funding_received_bps: f64,
    events_held: i64,
    notional: f64,
}

impl OpenPosition {
    fn trade(&self, close_time_ms: i64, round_trip_cost_bps: f64) -> FundingTrade {
        FundingTrade {
            symbol: self.symbol.clone(),
            open_time_ms: self.open_time_ms,
            close_time_ms,
            entry_funding_bps: self.entry_funding_bps,
            funding_received_bps: self.funding_received_bps,
            funding_events_held: self.events_held,
            round_trip_cost_bps,
            net_pnl_bps: self.funding_received_bps - round_trip_cost_bps,
        }
    }
}

/// Sorted union of funding times plus `symbol → time → rate_bps` (later duplicates win).
type RateIndex<'a> = BTreeMap<&'a str, HashMap<i64, f64>>;

fn build_grid(funding: &FundingSet) -> Result<(Vec<i64>, RateIndex<'_>)> {
    let mut indexed = BTreeMap::new();
    let mut times = Vec::new();
    for (symbol, rows) in funding {
        let mut per_symbol = HashMap::with_capacity(rows.len());
        for row in rows {
            per_symbol.insert(row.funding_time, row.rate_bps()?);
            times.push(row.funding_time);
        }
        indexed.insert(symbol.as_str(), per_symbol);
    }
    times.sort_unstable();
    times.dedup();
    Ok((times, indexed))
}

/// Replay delta-neutral funding harvest over historical funding events.
pub fn replay_funding(
    funding: &FundingSet,
    cfg: &FundingArbConfig,
) -> Result<FundingBacktestResult> {
    if cfg.starting_balance <= 0.0 {
        return Err(StrategyError::InvalidConfig(
            "starting_balance must be positive".into(),
        ));
    }
    if cfg.per_position_notional_pct <= 0.0 || cfg.per_position_notional_pct > 100.0 {
        return Err(StrategyError::InvalidConfig(
            "per_position_notional_pct must be in (0, 100]".into(),
        ));
    }
    if cfg.max_concurrent_positions <= 0 {
        return Err(StrategyError::InvalidConfig(
            "max_concurrent_positions must be positive".into(),
        ));
    }
    let (times, by_symbol) = build_grid(funding)?;
    let Some(&last_ts) = times.last() else {
        return Ok(FundingBacktestResult::empty(cfg));
    };
    let max_positions = usize::try_from(cfg.max_concurrent_positions).unwrap_or(usize::MAX);
    let one_side_cost_bps = cfg.one_side_cost_bps();
    let round_trip_cost_bps = 2.0 * one_side_cost_bps;
    let notional_per_position = cfg.starting_balance * (cfg.per_position_notional_pct / 100.0);

    let mut equity = cfg.starting_balance;
    // Insertion-ordered like the Python dict: float accumulation order matters.
    let mut open: Vec<OpenPosition> = Vec::new();
    let mut trades = Vec::new();
    let mut equity_curve = Vec::with_capacity(times.len());
    let mut returns = Vec::with_capacity(times.len());
    let (mut wins, mut losses) = (0_usize, 0_usize);
    let mut peak_equity = equity;
    let mut max_drawdown_pct = 0.0_f64;
    let mut funding_collected_bps_total = 0.0;
    let mut fees_paid_bps_total = 0.0;
    let mut bars_in_position = 0_usize;

    for &ts in &times {
        let equity_before_step = equity;

        // 1. Collect funding on open positions with a row at ts; flag exits.
        let mut to_close: Vec<String> = Vec::new();
        for pos in &mut open {
            let Some(&funding_bps) = by_symbol.get(pos.symbol.as_str()).and_then(|m| m.get(&ts))
            else {
                continue;
            };
            equity += pos.notional * (funding_bps / 10_000.0);
            pos.funding_received_bps += funding_bps;
            pos.events_held += 1;
            funding_collected_bps_total += funding_bps;
            if funding_bps <= cfg.exit_funding_bps || pos.events_held >= cfg.max_hold_funding_events
            {
                to_close.push(pos.symbol.clone());
            }
        }

        // 2. Close flagged positions; only the close-side cost is charged here.
        for symbol in &to_close {
            let Some(idx) = open.iter().position(|p| &p.symbol == symbol) else {
                continue;
            };
            let pos = open.remove(idx);
            equity -= pos.notional * (one_side_cost_bps / 10_000.0);
            fees_paid_bps_total += one_side_cost_bps;
            let trade = pos.trade(ts, round_trip_cost_bps);
            if trade.net_pnl_bps > 0.0 {
                wins += 1;
            } else if trade.net_pnl_bps < 0.0 {
                losses += 1;
            }
            trades.push(trade);
        }

        // 3. Open new positions, highest funding first, if capacity remains.
        if open.len() < max_positions {
            let mut scored: Vec<(f64, &str)> = by_symbol
                .iter()
                .filter(|(symbol, _)| !open.iter().any(|p| p.symbol == **symbol))
                .filter_map(|(symbol, rows)| rows.get(&ts).map(|&bps| (bps, *symbol)))
                .filter(|(bps, _)| *bps >= cfg.entry_funding_bps)
                .collect();
            scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(a.1)));
            let free_slots = max_positions - open.len();
            for (funding_bps, symbol) in scored.into_iter().take(free_slots) {
                equity -= notional_per_position * (one_side_cost_bps / 10_000.0);
                fees_paid_bps_total += one_side_cost_bps;
                open.push(OpenPosition {
                    symbol: symbol.to_owned(),
                    open_time_ms: ts,
                    entry_funding_bps: funding_bps,
                    funding_received_bps: 0.0,
                    events_held: 0,
                    notional: notional_per_position,
                });
            }
        }

        if !open.is_empty() {
            bars_in_position += 1;
        }
        returns.push(if equity_before_step > 0.0 {
            equity / equity_before_step - 1.0
        } else {
            0.0
        });
        equity_curve.push((ts, equity));
        if equity > peak_equity {
            peak_equity = equity;
        } else {
            let drawdown_pct = (peak_equity - equity) / peak_equity * 100.0;
            if drawdown_pct > max_drawdown_pct {
                max_drawdown_pct = drawdown_pct;
            }
        }
    }

    // Force-close anything still open at the final timestamp (close-side cost only).
    for pos in open.drain(..) {
        equity -= pos.notional * (one_side_cost_bps / 10_000.0);
        fees_paid_bps_total += one_side_cost_bps;
        trades.push(pos.trade(last_ts, round_trip_cost_bps));
    }

    let bars_evaluated = returns.len();
    let bars_per_year = 365.0 * 3.0;
    let years = bars_evaluated as f64 / bars_per_year;
    let annualized_return_pct = if years > 0.0 && equity > 0.0 {
        ((equity / cfg.starting_balance).powf(1.0 / years) - 1.0) * 100.0
    } else {
        0.0
    };
    let decisive = wins + losses;
    let avg_holding_events = if trades.is_empty() {
        0.0
    } else {
        trades.iter().map(|t| t.funding_events_held).sum::<i64>() as f64 / trades.len() as f64
    };
    Ok(FundingBacktestResult {
        config: cfg.clone(),
        initial_balance: cfg.starting_balance,
        final_balance: equity,
        total_return_pct: (equity / cfg.starting_balance - 1.0) * 100.0,
        annualized_return_pct,
        sharpe_annualized: sharpe_annualized(&returns, bars_per_year),
        max_drawdown_pct,
        num_trades: trades.len(),
        win_rate_pct: if decisive > 0 {
            wins as f64 / decisive as f64 * 100.0
        } else {
            0.0
        },
        avg_holding_events,
        funding_collected_bps: funding_collected_bps_total,
        fees_paid_bps: fees_paid_bps_total,
        bars_in_position_pct: bars_in_position as f64 / bars_evaluated as f64 * 100.0,
        bars_evaluated,
        equity_curve,
        trades,
    })
}

/// Compact CLI summary (Python `_result_summary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultSummary {
    pub verdict: &'static str,
    pub config: FundingArbConfig,
    pub initial_balance: f64,
    pub final_balance: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_annualized: f64,
    pub max_drawdown_pct: f64,
    pub num_trades: usize,
    pub win_rate_pct: f64,
    pub avg_holding_events: f64,
    pub funding_collected_bps: f64,
    pub fees_paid_bps: f64,
    pub bars_in_position_pct: f64,
    pub bars_evaluated: usize,
}

pub fn result_summary(result: &FundingBacktestResult) -> ResultSummary {
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
        avg_holding_events: py_round(result.avg_holding_events, 2),
        funding_collected_bps: py_round(result.funding_collected_bps, 2),
        fees_paid_bps: py_round(result.fees_paid_bps, 2),
        bars_in_position_pct: py_round(result.bars_in_position_pct, 2),
        bars_evaluated: result.bars_evaluated,
    }
}

/// `BTC/USDT`, `BTC/USDT:USDT` → `BTCUSDT` (Python `.replace("/", "").replace(":USDT", "")`).
fn rest_symbol(symbol: &str) -> String {
    symbol.replace('/', "").replace(":USDT", "").to_uppercase()
}

/// Page through USD-M funding-rate history for one symbol (limit 1000, cursor = last + 1).
pub async fn fetch_funding_history(
    client: &BinanceClient,
    symbol: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<FundingRow>> {
    let rest = rest_symbol(symbol);
    let end_ms = end.timestamp_millis();
    let mut cursor = start.timestamp_millis();
    let mut out = Vec::new();
    while cursor <= end_ms {
        let range = RangeQuery {
            start_time: Some(cursor),
            end_time: Some(end_ms),
            limit: Some(1000),
        };
        let page = client.funding_rate_history(Some(&rest), &range).await?;
        let Some(last) = page.last() else {
            break;
        };
        let last_time = last.funding_time;
        let page_len = page.len();
        out.extend(page.into_iter().map(|row| FundingRow {
            symbol: row.symbol,
            funding_time: row.funding_time,
            funding_rate: Value::String(row.funding_rate.to_string()),
            mark_price: Value::String(row.mark_price.map(|m| m.to_string()).unwrap_or_default()),
        }));
        if last_time <= cursor {
            break;
        }
        cursor = last_time + 1;
        if page_len < 1000 {
            break;
        }
    }
    Ok(out)
}

fn funding_cache_path(
    dir: &Path,
    symbol: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> PathBuf {
    dir.join(format!(
        "funding_{}_{}_{}.json",
        symbol.replace(['/', ':'], "_"),
        start.timestamp(),
        end.timestamp()
    ))
}

/// Fetch funding history for many symbols concurrently with an optional disk cache.
pub async fn fetch_funding_for_symbols(
    client: &BinanceClient,
    symbols: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    cache_dir: Option<&Path>,
    concurrency: usize,
) -> Result<FundingSet> {
    futures::stream::iter(symbols.iter().cloned())
        .map(|symbol| async move {
            let path = cache_dir.map(|dir| funding_cache_path(dir, &symbol, start, end));
            if let Some(path) = path.as_deref()
                && let Some(rows) = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Vec<FundingRow>>(&text).ok())
            {
                return Ok((symbol, rows));
            }
            let rows = fetch_funding_history(client, &symbol, start, end).await?;
            if let Some(path) = path.as_deref() {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| StrategyError::io(parent, e))?;
                }
                std::fs::write(path, serde_json::to_vec(&rows)?)
                    .map_err(|e| StrategyError::io(path, e))?;
            }
            Ok::<_, StrategyError>((symbol, rows))
        })
        .buffer_unordered(concurrency.max(1))
        .try_collect()
        .await
}

/// Top-N TRADING USDT-quoted perpetuals by 24h quote volume, as `BASE/USDT`.
pub async fn fetch_perp_universe(
    client: &BinanceClient,
    max_symbols: usize,
    min_quote_volume_usdt: f64,
) -> Result<Vec<String>> {
    let info = client.futures_exchange_info().await?;
    let tickers = client.futures_tickers_24h().await?;
    let volume_by_symbol: HashMap<&str, f64> = tickers
        .iter()
        .map(|t| (t.symbol.as_str(), common::decimal_f64(t.quote_volume)))
        .collect();
    let mut candidates: Vec<(f64, String)> = info
        .symbols
        .iter()
        .filter(|s| {
            s.status == "TRADING" && s.contract_type == "PERPETUAL" && s.quote_asset == "USDT"
        })
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

/// `{symbol: [row, ...]}` fixture.
pub fn load_fixture(path: &Path) -> Result<FundingSet> {
    let text = std::fs::read_to_string(path).map_err(|e| StrategyError::io(path, e))?;
    let raw: Value = serde_json::from_str(&text).map_err(|e| json_error(path, &e))?;
    let Value::Object(map) = raw else {
        return Err(StrategyError::InvalidConfig(
            "funding fixture must be a mapping of symbol -> rows".into(),
        ));
    };
    map.into_iter()
        .map(|(symbol, rows)| {
            Ok((
                symbol,
                serde_json::from_value(rows).map_err(|e| json_error(path, &e))?,
            ))
        })
        .collect()
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Comma-separated CCXT-style symbols.
    #[arg(
        long,
        default_value = "BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT,XRP/USDT,ADA/USDT"
    )]
    pub symbols: String,
    /// If set, ignore --symbols and pull all USD-M perp USDT pairs by 24h volume.
    #[arg(long, default_value = "", value_parser = clap::builder::PossibleValuesParser::new(["", "perp_usdt_top"]))]
    pub universe: String,
    #[arg(long, default_value_t = 50)]
    pub universe_size: usize,
    #[arg(long, default_value_t = 10_000_000.0, allow_negative_numbers = true)]
    pub min_quote_volume_usdt: f64,
    /// ISO start; required unless --fixture.
    #[arg(long, default_value = "")]
    pub start: String,
    /// ISO end; defaults to now.
    #[arg(long, default_value = "")]
    pub end: String,
    #[arg(long, default_value = "")]
    pub fixture: String,
    #[arg(long, default_value = "")]
    pub output: String,
    /// Directory to cache fetched funding rows.
    #[arg(long, default_value = "")]
    pub funding_cache: String,
    #[arg(long, default_value_t = 4, allow_negative_numbers = true)]
    pub fetch_concurrency: i64,
    #[arg(long, default_value_t = 1.0, allow_negative_numbers = true)]
    pub entry_funding_bps: f64,
    #[arg(long, default_value_t = -0.5, allow_negative_numbers = true)]
    pub exit_funding_bps: f64,
    #[arg(long, default_value_t = 90, allow_negative_numbers = true)]
    pub max_hold_events: i64,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    pub taker_fee_spot_bps: f64,
    #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
    pub taker_fee_futures_bps: f64,
    #[arg(long, default_value_t = 2.0, allow_negative_numbers = true)]
    pub slippage_per_leg_bps: f64,
    #[arg(long, default_value_t = 5, allow_negative_numbers = true)]
    pub max_concurrent_positions: i64,
    #[arg(long, default_value_t = 20.0, allow_negative_numbers = true)]
    pub per_position_notional_pct: f64,
    #[arg(long, default_value_t = 10_000.0, allow_negative_numbers = true)]
    pub starting_balance: f64,
}

pub async fn run_cli(args: Args) -> anyhow::Result<ExitCode> {
    let cfg = FundingArbConfig {
        entry_funding_bps: args.entry_funding_bps,
        exit_funding_bps: args.exit_funding_bps,
        max_hold_funding_events: args.max_hold_events,
        taker_fee_spot_bps: args.taker_fee_spot_bps,
        taker_fee_futures_bps: args.taker_fee_futures_bps,
        slippage_per_leg_bps: args.slippage_per_leg_bps,
        max_concurrent_positions: args.max_concurrent_positions,
        per_position_notional_pct: args.per_position_notional_pct,
        starting_balance: args.starting_balance,
    };
    let funding = if !args.fixture.is_empty() {
        load_fixture(Path::new(&args.fixture))?
    } else {
        if args.start.is_empty() {
            return Ok(crate::cli::usage_error(
                "funding-arb-backtest",
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
        let symbols = if args.universe == "perp_usdt_top" {
            let symbols =
                fetch_perp_universe(&client, args.universe_size, args.min_quote_volume_usdt)
                    .await?;
            UniverseBanner::print("perp_usdt_top", &symbols)?;
            symbols
        } else {
            split_csv(&args.symbols)
        };
        let cache_dir =
            (!args.funding_cache.is_empty()).then(|| PathBuf::from(&args.funding_cache));
        fetch_funding_for_symbols(
            &client,
            &symbols,
            start,
            end,
            cache_dir.as_deref(),
            usize::try_from(args.fetch_concurrency).unwrap_or(1),
        )
        .await?
    };

    let result = replay_funding(&funding, &cfg)?;
    common::print_json(&result_summary(&result))?;
    if !args.output.is_empty() {
        common::write_report(Path::new(&args.output), &result)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EIGHT_HOURS_MS: i64 = 8 * 60 * 60 * 1000;

    fn series(rates_bps: &[f64], symbol: &str) -> Vec<FundingRow> {
        rates_bps
            .iter()
            .enumerate()
            .map(|(i, rate)| FundingRow {
                symbol: symbol.to_owned(),
                funding_time: i as i64 * EIGHT_HOURS_MS,
                funding_rate: serde_json::json!(rate / 10_000.0),
                mark_price: Value::String("100.0".into()),
            })
            .collect()
    }

    fn one(rates: &[f64]) -> FundingSet {
        [("BTCUSDT".to_owned(), series(rates, "BTCUSDT"))]
            .into_iter()
            .collect()
    }

    fn single_slot(overrides: FundingArbConfig) -> FundingArbConfig {
        FundingArbConfig {
            max_concurrent_positions: 1,
            per_position_notional_pct: 100.0,
            ..overrides
        }
    }

    #[test]
    fn holds_when_funding_never_clears_entry() {
        let result = replay_funding(&one(&[0.1; 50]), &FundingArbConfig::default()).unwrap();
        assert_eq!(result.num_trades, 0);
        assert!((result.final_balance - result.initial_balance).abs() < 1e-9);
        assert_eq!(result.verdict(), "no_trades");
    }

    #[test]
    fn collects_funding_when_persistently_positive() {
        let cfg = single_slot(FundingArbConfig {
            max_hold_funding_events: 30,
            ..FundingArbConfig::default()
        });
        let result = replay_funding(&one(&[5.0; 100]), &cfg).unwrap();
        assert!(result.num_trades >= 2);
        assert!(result.final_balance > result.initial_balance);
        assert!(["edge_likely", "edge_marginal"].contains(&result.verdict()));
        assert!(result.trades.iter().all(|t| t.net_pnl_bps > 0.0));
    }

    #[test]
    fn exits_when_funding_drops_below_exit_threshold() {
        let mut rates = vec![5.0; 10];
        rates.extend([-2.0; 10]);
        let cfg = single_slot(FundingArbConfig {
            max_hold_funding_events: 100,
            ..FundingArbConfig::default()
        });
        let result = replay_funding(&one(&rates), &cfg).unwrap();
        assert!(result.num_trades >= 1);
        assert!(result.trades.iter().any(|t| t.funding_events_held < 100));
    }

    #[test]
    fn concurrent_positions_capped() {
        let funding: FundingSet = [
            ("BTCUSDT".to_owned(), series(&[5.0; 20], "BTCUSDT")),
            ("ETHUSDT".to_owned(), series(&[5.0; 20], "ETHUSDT")),
        ]
        .into_iter()
        .collect();
        let cfg = single_slot(FundingArbConfig {
            max_hold_funding_events: 5,
            ..FundingArbConfig::default()
        });
        let result = replay_funding(&funding, &cfg).unwrap();
        assert!(!result.trades.is_empty());
        let mut intervals: Vec<(i64, i64)> = result
            .trades
            .iter()
            .map(|t| (t.open_time_ms, t.close_time_ms))
            .collect();
        intervals.sort_unstable();
        assert!(intervals.windows(2).all(|w| w[1].0 >= w[0].1));
    }

    #[test]
    fn force_closes_open_positions_at_end() {
        let cfg = single_slot(FundingArbConfig {
            max_hold_funding_events: 1000,
            ..FundingArbConfig::default()
        });
        assert_eq!(
            replay_funding(&one(&[5.0; 10]), &cfg).unwrap().num_trades,
            1
        );
    }

    #[test]
    fn fees_accounting_is_single_round_trip_per_trade() {
        let result = replay_funding(
            &one(&[5.0, -2.0]),
            &single_slot(FundingArbConfig::default()),
        )
        .unwrap();
        assert_eq!(result.num_trades, 1);
        assert!((result.fees_paid_bps - 38.0).abs() < 0.5);
    }

    #[test]
    fn rejects_invalid_config() {
        let empty = FundingSet::new();
        let err = |cfg: FundingArbConfig| replay_funding(&empty, &cfg).unwrap_err().to_string();
        assert!(
            err(FundingArbConfig {
                starting_balance: 0.0,
                ..FundingArbConfig::default()
            })
            .contains("starting_balance")
        );
        assert!(
            err(FundingArbConfig {
                per_position_notional_pct: 0.0,
                ..FundingArbConfig::default()
            })
            .contains("per_position_notional_pct")
        );
        assert!(
            err(FundingArbConfig {
                max_concurrent_positions: 0,
                ..FundingArbConfig::default()
            })
            .contains("max_concurrent_positions")
        );
    }

    #[test]
    fn summary_round_trips_as_json() {
        let cfg = FundingArbConfig {
            max_hold_funding_events: 5,
            ..FundingArbConfig::default()
        };
        let result = replay_funding(&one(&[5.0; 20]), &cfg).unwrap();
        let decoded: Value =
            serde_json::from_str(&common::to_pretty_sorted(&result_summary(&result)).unwrap())
                .unwrap();
        assert_eq!(decoded["verdict"], result.verdict());
        assert_eq!(
            decoded["num_trades"].as_u64().unwrap() as usize,
            result.num_trades
        );
    }

    #[tokio::test]
    async fn funding_history_pages_and_caches() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let row = |t: i64| serde_json::json!({"symbol": "BTCUSDT", "fundingTime": t, "fundingRate": "0.00010000", "markPrice": "100.5"});
        Mock::given(method("GET"))
            .and(path("/fapi/v1/fundingRate"))
            .and(query_param("symbol", "BTCUSDT"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([row(0), row(EIGHT_HOURS_MS)])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = BinanceClient::new(
            bb_binance::BinanceConfig::new(false).with_futures_rest_url(server.uri()),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let start = parse_iso("1970-01-01").unwrap();
        let end = parse_iso("1970-01-02").unwrap();
        let symbols = vec!["BTC/USDT".to_owned()];
        let first = fetch_funding_for_symbols(&client, &symbols, start, end, Some(dir.path()), 4)
            .await
            .unwrap();
        let cached = dir.path().join("funding_BTC_USDT_0_86400.json");
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(cached).unwrap()).unwrap();
        assert_eq!(raw[0]["fundingRate"], "0.00010000");
        assert_eq!(raw[1]["fundingTime"], EIGHT_HOURS_MS);
        let second = fetch_funding_for_symbols(&client, &symbols, start, end, Some(dir.path()), 4)
            .await
            .unwrap();
        assert_eq!(first, second);
        assert!((first["BTC/USDT"][0].rate_bps().unwrap() - 1.0).abs() < 1e-12);
    }

    #[tokio::test]
    async fn cli_runs_against_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("funding.json");
        std::fs::write(&fixture, serde_json::to_string(&one(&[5.0; 30])).unwrap()).unwrap();
        let output = dir.path().join("report.json");
        let args = Args {
            symbols: String::new(),
            universe: String::new(),
            universe_size: 50,
            min_quote_volume_usdt: 1e7,
            start: String::new(),
            end: String::new(),
            fixture: fixture.display().to_string(),
            output: output.display().to_string(),
            funding_cache: String::new(),
            fetch_concurrency: 4,
            entry_funding_bps: 1.0,
            exit_funding_bps: -0.5,
            max_hold_events: 5,
            taker_fee_spot_bps: 10.0,
            taker_fee_futures_bps: 5.0,
            slippage_per_leg_bps: 2.0,
            max_concurrent_positions: 1,
            per_position_notional_pct: 100.0,
            starting_balance: 10_000.0,
        };
        assert_eq!(run_cli(args).await.unwrap(), ExitCode::SUCCESS);
        let full: Value = serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert!(full.get("trades").is_some());
        assert!(full["bars_evaluated"].as_u64().unwrap() > 0);
    }
}

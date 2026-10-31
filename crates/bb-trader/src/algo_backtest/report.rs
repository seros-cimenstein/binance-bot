//! `bb algo-backtest`: tune the trader's algorithm on the older half of a window of the
//! offline cache, judge it on the newer half, and write the report (`.md`), the summary
//! (`.csv`), the trade list (`_trades.csv`) and the per-step traces.
//!
//! The output is deterministic: the same cache and flags give byte-identical files (no run
//! time or wall clock in them).

use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};

use super::data::{MINUTE_MS, Market, TagsHistory};
use super::engine::{Config, LIQUID, RunResult, run};
use super::rule::BizAlgo;
use crate::error::{Result, TraderError};
use crate::rules::{Horizon, Rule, Sleeve, Style};
use crate::signals::{Name, Span};

/// `biz_algo` grid rows listed in the report.
const GRID_ROWS_SHOWN: usize = 5;
const DAY_MS: i64 = 86_400_000;
/// History needed before the window start: the 7d return of the first step.
const WARM_UP_MS: i64 = 7 * DAY_MS;
/// Binance minimum order value the engine enforces on buys.
const MIN_NOTIONAL_USDT: f64 = 5.0;
/// Measured live cost per round trip (40 trips, `docs/archive/loops/LOOP_MONEY.md`).
const MEASURED_ROUND_TRIP_BPS: f64 = 14.8;
/// Backtest the trader's algorithm on cached 1m candles (offline, research only, no orders).
#[derive(clap::Args, Debug, Clone)]
pub struct AlgoBacktestArgs {
    /// Directory of `1m_<BASE>_USDT_<start>_<end>.json` files (the `bb entry-study` cache).
    #[arg(long, default_value = "data/entry-study-cache")]
    pub cache_dir: PathBuf,
    /// Window end (ISO 8601 / RFC 3339, whole minute), e.g. `2026-10-18T10:43:00Z`.
    #[arg(long)]
    pub end: String,
    /// Window length in days.
    #[arg(long, default_value_t = 60)]
    pub days: i64,
    /// Starting USDT balance of each half.
    #[arg(long, default_value_t = 1000.0)]
    pub starting_balance: f64,
    /// Fee per side in bps.
    #[arg(long, default_value_t = 7.5)]
    pub fee_bps: f64,
    /// Slippage per side in bps.
    #[arg(long, default_value_t = 5.0)]
    pub slippage_bps: f64,
    /// Minutes between decision steps (the live cycle is 20).
    #[arg(long, default_value_t = 20)]
    pub step_minutes: i64,
    /// Where the three report files go.
    #[arg(long, default_value = "reports")]
    pub output_dir: PathBuf,
    /// Hourly `tags-feed-v1` history (`biz-insights feed --history`); without it the
    /// /biz/ terms are 0.
    #[arg(long)]
    pub tags_history: Option<PathBuf>,
    /// A feed older than this at a step is not visible (hourly history needs more than 60).
    #[arg(long, default_value_t = 90)]
    pub tags_max_age_minutes: i64,
    /// Fixed weights `talk,stance,momentum,buy_above,sell_below` (e.g. read off the
    /// `bb tag-scoreboard` file) instead of tuning them on the older half.
    #[arg(long)]
    pub biz_weights: Option<String>,
    /// Distinct posters a 6h /biz/ window needs before its stance counts.
    #[arg(long, default_value_t = 2)]
    pub biz_min_posters: u64,
}

/// What the algorithm needs beyond the market: the tags feed history.
#[derive(Debug, Clone, Default)]
pub struct AlgoInputs {
    pub tags: Option<Arc<TagsHistory>>,
}

/// Fail unless the cache holds every liquid coin, reaches the window end, and has the warm-up
/// history before the window start for at least one liquid coin (late listings such as HYPE
/// simply join when their bars start).
pub fn check_coverage(market: &Market, start_ms: i64, end_ms: i64) -> Result<()> {
    let missing: Vec<&str> = LIQUID
        .iter()
        .copied()
        .filter(|a| market.get(a).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(TraderError::config(format!(
            "missing cache for {} (needs every liquid coin: {})",
            missing.join(", "),
            LIQUID.join(", ")
        )));
    }
    let short: Vec<&str> = LIQUID
        .iter()
        .copied()
        .filter(|a| {
            market
                .get(a)
                .and_then(|s| s.last_open_ms())
                .is_none_or(|last| last < end_ms - MINUTE_MS)
        })
        .collect();
    if !short.is_empty() {
        return Err(TraderError::config(format!(
            "missing cache: {} end before the window end {}",
            short.join(", "),
            iso(end_ms)
        )));
    }
    let earliest = LIQUID
        .iter()
        .filter_map(|a| market.get(a)?.first_open_ms())
        .min();
    if earliest.is_none_or(|first| first > start_ms - WARM_UP_MS) {
        return Err(TraderError::config(format!(
            "missing cache: no liquid coin has bars from {} (7 days before the window start)",
            iso(start_ms - WARM_UP_MS)
        )));
    }
    Ok(())
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |d| d.to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

fn parse_end(end: &str) -> Result<i64> {
    let ms = DateTime::parse_from_rfc3339(end)
        .map_err(|e| TraderError::config(format!("--end {end:?}: {e}")))?
        .timestamp_millis();
    if ms % MINUTE_MS != 0 {
        return Err(TraderError::config(format!(
            "--end {end:?} must be a whole minute"
        )));
    }
    Ok(ms)
}

/// Settings of one report.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportContext {
    pub cfg: Config,
    pub fee_bps: f64,
    pub slippage_bps: f64,
    pub cache_dir: String,
    pub coins: usize,
    /// The report's file stem, for naming the per-variant trace files.
    pub stem: String,
    /// The older/newer split and the rule.
    pub split: SplitReport,
}

/// The backtest's weight set over the legacy names: `talk.6h`, `stance.6h`,
/// `momentum.1h` and the levels (`--biz-weights` or the tuning grid; the trader reads a
/// rule file, [`crate::rules`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridWeights {
    pub talk: f64,
    pub stance: f64,
    pub momentum: f64,
    /// A candidate is bought when its score is at least this.
    pub buy_above: f64,
    /// A held coin is sold when its score is below this.
    pub sell_below: f64,
}

impl GridWeights {
    /// `talk,stance,momentum,buy_above,sell_below`, e.g. `1,0.5,0.5,0.5,0`.
    pub fn parse(spec: &str) -> std::result::Result<Self, String> {
        let values: Vec<f64> = spec
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| format!("weights {spec:?}: {part:?} is not a number"))
            })
            .collect::<std::result::Result<_, _>>()?;
        let [talk, stance, momentum, buy_above, sell_below] = values[..] else {
            return Err(format!(
                "weights {spec:?}: need 5 numbers: talk,stance,momentum,buy_above,sell_below"
            ));
        };
        if sell_below > buy_above {
            return Err(format!(
                "weights {spec:?}: sell_below must not exceed buy_above"
            ));
        }
        Ok(Self {
            talk,
            stance,
            momentum,
            buy_above,
            sell_below,
        })
    }

    /// The one-sleeve fixed rule these weights stand for.
    pub fn rule(&self, min_posters: u64) -> Rule {
        Rule {
            rules_version: "algo-backtest".into(),
            style: Style::Fixed,
            sleeves: vec![Sleeve {
                name: "fast".into(),
                share: 1.0,
                horizon: Horizon::Any,
                defensive: vec!["USDT".into()],
                risk_off_below: None,
                buy_above: self.buy_above,
                sell_below: self.sell_below,
                min_posters,
                weights: [
                    (Name::Talk, self.talk),
                    (Name::Stance, self.stance),
                    (Name::Momentum(Span::H1), self.momentum),
                ]
                .into(),
            }],
        }
    }
}

impl fmt::Display for GridWeights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "talk.6h {:+}, stance.6h {:+}, momentum.1h {:+}; buy at score ≥ {:+}, sell below {:+}",
            self.talk, self.stance, self.momentum, self.buy_above, self.sell_below
        )
    }
}

/// One `biz_algo` weight set tried on the older half.
#[derive(Debug, Clone, PartialEq)]
pub struct GridRow {
    pub weights: GridWeights,
    pub net_pnl_pct: f64,
    pub round_trips: usize,
    pub max_drawdown_pct: f64,
}

/// The older/newer split of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitReport {
    /// First step of the newer half.
    pub mid_ms: i64,
    pub weights: GridWeights,
    /// Distinct posters a 6h window needs before its stance counts.
    pub min_posters: u64,
    /// Every weight set tried on the older half, in grid order; `None` when the weights
    /// came from `--biz-weights`.
    pub grid: Option<Vec<GridRow>>,
    /// One line describing the tags feed history.
    pub tags: String,
}

/// First step of the newer half: the window's middle, rounded down to the step grid.
pub fn split_point(cfg: &Config) -> Result<i64> {
    let step_ms = cfg.step_minutes * MINUTE_MS;
    let half = (cfg.end_ms - cfg.start_ms) / 2;
    let mid = cfg.start_ms + half - half.rem_euclid(step_ms);
    if mid <= cfg.start_ms || mid >= cfg.end_ms {
        return Err(TraderError::config(
            "the window is too short to split into an older and a newer half",
        ));
    }
    Ok(mid)
}

/// The weight sets `biz_algo` is tuned over: talk and stance −1/0/+1 (0 only without
/// feeds), momentum 0/1, buy at 0.5/1, sell below −0.5/0.
pub fn weight_grid(with_biz: bool) -> Vec<GridWeights> {
    let signed: &[f64] = if with_biz { &[-1.0, 0.0, 1.0] } else { &[0.0] };
    let mut out = Vec::new();
    for &talk in signed {
        for &stance in signed {
            for momentum in [0.0, 1.0] {
                for buy_above in [0.5, 1.0] {
                    for sell_below in [-0.5, 0.0] {
                        out.push(GridWeights {
                            talk,
                            stance,
                            momentum,
                            buy_above,
                            sell_below,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Run `biz_algo` with every grid weight set on `older`: the highest net % wins, the
/// first in grid order on ties.
pub fn tune(
    market: &Market,
    older: &Config,
    inputs: &AlgoInputs,
    min_posters: u64,
) -> Result<(GridWeights, Vec<GridRow>)> {
    let mut rows = Vec::new();
    for weights in weight_grid(inputs.tags.is_some()) {
        let rule = BizAlgo::new(weights.rule(min_posters), inputs.tags.clone());
        let r = run(market, older, Box::new(rule))?;
        rows.push(GridRow {
            weights,
            net_pnl_pct: r.net_pnl_pct,
            round_trips: r.round_trips,
            max_drawdown_pct: r.max_drawdown_pct,
        });
    }
    let mut best: Option<&GridRow> = None;
    for row in &rows {
        if best.is_none_or(|b| row.net_pnl_pct > b.net_pnl_pct) {
            best = Some(row);
        }
    }
    let weights = best
        .map(|row| row.weights)
        .ok_or_else(|| TraderError::config("empty biz_algo weight grid"))?;
    Ok((weights, rows))
}

/// Tune (or take) the rule on the older half, then run it on the older and the newer half
/// (`biz_algo@older`, `biz_algo@newer`).
fn run_split(
    market: &Market,
    cfg: &Config,
    inputs: &AlgoInputs,
    args: &AlgoBacktestArgs,
) -> Result<(Vec<RunResult>, SplitReport)> {
    let mid_ms = split_point(cfg)?;
    let older = Config {
        end_ms: mid_ms,
        ..*cfg
    };
    let newer = Config {
        start_ms: mid_ms,
        ..*cfg
    };
    let (weights, grid) = match &args.biz_weights {
        Some(spec) => (GridWeights::parse(spec).map_err(TraderError::config)?, None),
        None => {
            let (weights, rows) = tune(market, &older, inputs, args.biz_min_posters)?;
            (weights, Some(rows))
        }
    };
    let rule = weights.rule(args.biz_min_posters);
    let mut results = Vec::new();
    for (label, half) in [("older", older), ("newer", newer)] {
        let mut r = run(market, &half, Box::new(BizAlgo::new(rule.clone(), inputs.tags.clone())))?;
        r.name = format!("{}@{label}", r.name);
        results.push(r);
    }
    let split = SplitReport {
        mid_ms,
        weights,
        min_posters: args.biz_min_posters,
        grid,
        tags: tags_summary(inputs.tags.as_deref(), args.tags_history.as_deref()),
    };
    Ok((results, split))
}

fn tags_summary(history: Option<&TagsHistory>, dir: Option<&Path>) -> String {
    let (Some(history), Some(dir)) = (history, dir) else {
        return "not loaded (no `--tags-history`): the talk and stance terms are 0, so the rule trades on momentum alone".to_string();
    };
    let span = |ms: Option<i64>| ms.map_or_else(|| "none".to_string(), iso);
    let readers: Vec<&str> = history.readers().iter().map(String::as_str).collect();
    format!(
        "`{}`, {} feeds from {} to {}; AI readers {}{}",
        dir.display(),
        history.len(),
        span(history.first_ms()),
        span(history.last_ms()),
        readers.join(", "),
        if readers.len() > 1 {
            " (mixed: stances from different models or instructions)"
        } else {
            ""
        }
    )
}

/// What the backtested rule does, in words.
const RULE_TEXT: &str = "the trader's algorithm, no LLM: scores each liquid-top coin as w_talk·talk + w_stance·stance + w_momentum·momentum (talk = the /biz/ 6h mention rate over its 24h rate − 1; stance = (bullish − bearish) / tagged of the 6h AI tags with enough posters; momentum = 1h return / 5%, capped); a held coin scoring below the sell level is sold, and with nothing held the best coin is bought with all cash at or above the buy level; no exit plan";

/// The summary CSV: one row per variant.
pub fn summary_csv(results: &[RunResult]) -> String {
    let mut out = String::from(
        "variant,net_pnl_usdt,net_pnl_pct,max_drawdown_pct,round_trips,win_rate_pct,sharpe,fees_usdt\n",
    );
    for r in results {
        let _ = writeln!(
            out,
            "{},{:.10},{:.6},{:.6},{},{:.4},{:.6},{:.10}",
            r.name,
            r.net_pnl_usdt,
            r.net_pnl_pct,
            r.max_drawdown_pct,
            r.round_trips,
            r.win_rate_pct,
            r.sharpe,
            r.fees_usdt
        );
    }
    out
}

/// The trade CSV: one row per round trip or still-open position, per variant.
pub fn trades_csv(results: &[RunResult]) -> String {
    let mut out = String::from(
        "variant,asset,entry_time,exit_time,entry_price,exit_price,qty,cost_usdt,proceeds_usdt,fees_usdt,net_pnl_usdt,reason,open_at_end\n",
    );
    for r in results {
        for t in &r.trades {
            let _ = writeln!(
                out,
                "{},{},{},{},{:.10},{:.10},{:.10},{:.10},{:.10},{:.10},{:.10},{},{}",
                r.name,
                t.asset,
                iso(t.entry_ms),
                iso(t.exit_ms),
                t.entry_price,
                t.exit_price,
                t.qty,
                t.cost_usdt,
                t.proceeds_usdt,
                t.fees_usdt,
                t.net_pnl_usdt,
                t.reason,
                t.open_at_end,
            );
        }
    }
    out
}

/// The Markdown report.
pub fn markdown(results: &[RunResult], ctx: &ReportContext) -> String {
    let cfg = &ctx.cfg;
    let round_trip_bps = 2.0 * (ctx.fee_bps + ctx.slippage_bps);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Algorithm backtest ({} → {})\n",
        iso(cfg.start_ms),
        iso(cfg.end_ms)
    );
    let _ = writeln!(
        out,
        "Research only: `bb algo-backtest`, offline on `{}` ({} coins), no network, no orders.\n",
        ctx.cache_dir, ctx.coins
    );
    let _ = writeln!(out, "## Settings\n");
    let _ = writeln!(
        out,
        "- Window: {} → {} ({} days), a decision step every {} minutes ({} steps).",
        iso(cfg.start_ms),
        iso(cfg.end_ms),
        (cfg.end_ms - cfg.start_ms) / DAY_MS,
        cfg.step_minutes,
        results.first().map_or(0, |r| r.steps)
    );
    let _ = writeln!(
        out,
        "- Each half starts with {:.2} USDT. Costs per side: fee {} bps + slippage {} bps ({} bps per round trip).",
        cfg.starting_balance, ctx.fee_bps, ctx.slippage_bps, round_trip_bps
    );
    let _ = writeln!(
        out,
        "- Liquid universe ({} coins): {}.",
        LIQUID.len(),
        LIQUID.join(", ")
    );
    let split = &ctx.split;
    let _ = writeln!(
        out,
        "- Split: the older half {} → {} tunes the rule, the newer half {} → {} judges it (`biz_algo@older`, `biz_algo@newer`); the step count above is the older half's.",
        iso(cfg.start_ms),
        iso(split.mid_ms),
        iso(split.mid_ms),
        iso(cfg.end_ms)
    );
    let _ = writeln!(out, "- Tags feeds: {}.", split.tags);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Results\n");
    let _ = writeln!(
        out,
        "| Variant | Net USDT | Net % | Max drawdown % | Round trips | Win rate % | Sharpe | Fees USDT | Skipped buys/sells |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for r in results {
        let _ = writeln!(
            out,
            "| {} | {:.2} | {:.2} | {:.2} | {} | {:.1} | {:.3} | {:.2} | {} |",
            r.name,
            r.net_pnl_usdt,
            r.net_pnl_pct,
            r.max_drawdown_pct,
            r.round_trips,
            r.win_rate_pct,
            r.sharpe,
            r.fees_usdt,
            r.skipped_orders
        );
    }
    let _ = writeln!(
        out,
        "\nNet = ending equity − starting balance after every fee and slippage; positions still open at the end are marked at their last close. Win rate counts closed round trips with a positive net. Sharpe = mean / population std of step-equity returns × √(525600 / step minutes). Full numbers: the `.csv`; every trade: `_trades.csv`.\n"
    );
    let _ = writeln!(out, "## Rule\n");
    let _ = writeln!(out, "- `biz_algo`: {RULE_TEXT}.");
    let _ = writeln!(out, "\n## Per-variant details\n");
    for r in results {
        let skips = if r.skip_reasons.is_empty() {
            "none".to_string()
        } else {
            r.skip_reasons
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut reasons: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for t in &r.trades {
            *reasons.entry(t.reason.as_str()).or_default() += 1;
        }
        let exits = reasons
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let open = r.trades.iter().filter(|t| t.open_at_end).count();
        let _ = writeln!(out, "### `{}`\n", r.name);
        let _ = writeln!(
            out,
            "- Ending equity {:.2} USDT; {} closed round trips ({} wins), {} still open at the end.",
            r.final_equity, r.round_trips, r.wins, open
        );
        let _ = writeln!(
            out,
            "- Exit reasons: {}.",
            if exits.is_empty() { "none" } else { &exits }
        );
        let _ = writeln!(out, "- Skipped orders: {skips}.");
        if r.round_trips == 0 {
            let _ = writeln!(
                out,
                "- No round trip closed in {} steps: its entry rule never fired, or its only position stayed open.",
                r.steps
            );
        }
        for note in &r.notes {
            let _ = writeln!(out, "- {note}");
        }
        if let Some(trace) = &r.trace {
            let _ = writeln!(
                out,
                "- Per-step trace: `{}` ({} rows, one per step: `{}`).",
                trace_file_name(&ctx.stem, &r.name),
                trace.rows.len(),
                trace.header
            );
        }
        let _ = writeln!(out);
    }
    {
        let _ = writeln!(out, "## `biz_algo` rule\n");
        let _ = writeln!(
            out,
            "- Rule: {}; stances need {} distinct posters in the 6h window.",
            split.weights, split.min_posters
        );
        match &split.grid {
            None => {
                let _ = writeln!(
                    out,
                    "- Given with `--biz-weights`, not tuned on this window: both halves are out of sample."
                );
            }
            Some(grid) => {
                let _ = writeln!(
                    out,
                    "- Picked on the older half from {} weight sets by net %; judge it by `biz_algo@newer` (the `@older` row is in-sample). Best {} on the older half:\n",
                    grid.len(),
                    GRID_ROWS_SHOWN.min(grid.len())
                );
                let mut ranked: Vec<&GridRow> = grid.iter().collect();
                ranked.sort_by(|a, b| b.net_pnl_pct.total_cmp(&a.net_pnl_pct));
                let _ = writeln!(out, "| Weights | Net % | Round trips | Max drawdown % |");
                let _ = writeln!(out, "|---|---:|---:|---:|");
                for row in ranked.into_iter().take(GRID_ROWS_SHOWN) {
                    let _ = writeln!(
                        out,
                        "| {} | {:.2} | {} | {:.2} |",
                        row.weights, row.net_pnl_pct, row.round_trips, row.max_drawdown_pct
                    );
                }
            }
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "## Assumptions\n");
    for line in [
        "Decisions see only bars closed by the step time; intents fill at the open of the 1m bar that opens at the step, sells before buys.".to_string(),
        format!("The {MIN_NOTIONAL_USDT} USDT minimum applies to buys only; a held position can always be sold."),
        "A buy with no cash left is counted as `no_cash`.".to_string(),
        "Equity, drawdown and positions still open at the end are marked at the last close without exit costs.".to_string(),
    ] {
        let _ = writeln!(out, "- {line}");
    }
    let _ = writeln!(out, "\n## Caveats\n");
    for line in [
        "Survivorship: the liquid universe is the top 20 by volume as of 2026-10-18, applied to the whole window.".to_string(),
        format!(
            "Costs: {round_trip_bps} bps per round trip, against the measured {MEASURED_ROUND_TRIP_BPS} bps of the 40 live trips (deliberately pessimistic)."
        ),
        "Out of sample: when the weights were picked on the older half, the `@newer` row is the honest result. Each half is about half the window: few trades, a small sample.".to_string(),
    ] {
        let _ = writeln!(out, "- {line}");
    }
    out
}

/// File stem for a window ending at `end_ms`: `algo_backtest_<YYYY-MM-DD>`.
pub fn file_stem(end_ms: i64) -> String {
    let date = DateTime::<Utc>::from_timestamp_millis(end_ms)
        .map_or_else(|| end_ms.to_string(), |d| d.format("%Y-%m-%d").to_string());
    format!("algo_backtest_{date}")
}

/// File name of a variant's per-step trace: `<stem>_<variant>_trace.csv`.
pub fn trace_file_name(stem: &str, variant: &str) -> String {
    format!("{stem}_{variant}_trace.csv")
}

/// A trace as CSV text.
pub fn trace_csv(trace: &super::engine::Trace) -> String {
    let mut out = String::with_capacity(trace.rows.len() * 96);
    out.push_str(&trace.header);
    out.push('\n');
    for row in &trace.rows {
        out.push_str(row);
        out.push('\n');
    }
    out
}

/// Load, run and write; returns the paths written (`.md`, `.csv`, `_trades.csv`, then one
/// `_<variant>_trace.csv` per variant that keeps a trace).
pub fn run_backtest(args: &AlgoBacktestArgs) -> Result<(Vec<RunResult>, Vec<PathBuf>)> {
    if args.days <= 0 {
        return Err(TraderError::config("--days must be positive"));
    }
    let end_ms = parse_end(&args.end)?;
    let start_ms = end_ms - args.days * DAY_MS;
    let cfg = Config {
        start_ms,
        end_ms,
        step_minutes: args.step_minutes,
        fee_rate: args.fee_bps / 10_000.0,
        slippage: args.slippage_bps / 10_000.0,
        starting_balance: args.starting_balance,
        min_notional: MIN_NOTIONAL_USDT,
    };
    cfg.validate()?;
    let tags = match &args.tags_history {
        Some(dir) => Some(Arc::new(TagsHistory::load_dir(dir, args.tags_max_age_minutes)?)),
        None => None,
    };
    let inputs = AlgoInputs { tags };
    let market = Market::load_dir(&args.cache_dir)?;
    check_coverage(&market, start_ms, end_ms)?;
    let (results, split) = run_split(&market, &cfg, &inputs, args)?;
    let stem = file_stem(end_ms);
    let ctx = ReportContext {
        cfg,
        fee_bps: args.fee_bps,
        slippage_bps: args.slippage_bps,
        cache_dir: args.cache_dir.display().to_string(),
        coins: market.len(),
        stem: stem.clone(),
        split,
    };
    std::fs::create_dir_all(&args.output_dir)?;
    let mut paths = vec![
        args.output_dir.join(format!("{stem}.md")),
        args.output_dir.join(format!("{stem}.csv")),
        args.output_dir.join(format!("{stem}_trades.csv")),
    ];
    write(&paths[0], &markdown(&results, &ctx))?;
    write(&paths[1], &summary_csv(&results))?;
    write(&paths[2], &trades_csv(&results))?;
    for r in &results {
        if let Some(trace) = &r.trace {
            let path = args.output_dir.join(trace_file_name(&stem, &r.name));
            write(&path, &trace_csv(trace))?;
            paths.push(path);
        }
    }
    Ok((results, paths))
}

fn write(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body)?;
    Ok(())
}

/// `bb algo-backtest`.
pub fn run_cli(args: &AlgoBacktestArgs) -> Result<ExitCode> {
    let (results, paths) = run_backtest(args)?;
    print!("{}", summary_csv(&results));
    for path in paths {
        println!("wrote {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_END: i64 = 1_784_448_000_000 + 4 * DAY_MS; // 2026-07-23T08:00Z

    /// Writes 9 days of 1m bars for every liquid coin (plus INJ), each with its own drift
    /// and wiggle, ending at `T_END` (a 1-day window needs 7 days of warm-up).
    fn cache_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let start = T_END - 9 * DAY_MS;
        let mut coins: Vec<&str> = LIQUID.to_vec();
        coins.push("INJ");
        for (i, coin) in coins.iter().enumerate() {
            let k = i as f64;
            let rows: Vec<String> = (0..9 * 1440)
                .map(|m| {
                    let x = m as f64;
                    let p = 10.0
                        + k
                        + (x / (37.0 + k)).sin() * (0.4 + 0.02 * k)
                        + x * (k - 10.0) * 2e-5;
                    format!(
                        r#"{{"t":{},"o":{p},"h":{},"l":{},"c":{p},"v":{}}}"#,
                        start + m * MINUTE_MS,
                        p * 1.001,
                        p * 0.999,
                        100.0 + k
                    )
                })
                .collect();
            std::fs::write(
                dir.path().join(format!("1m_{coin}_USDT_1_2.json")),
                format!("[{}]", rows.join(",")),
            )
            .unwrap();
        }
        dir
    }

    fn args(cache: &Path, out: &Path) -> AlgoBacktestArgs {
        AlgoBacktestArgs {
            cache_dir: cache.to_path_buf(),
            end: "2026-07-23T08:00:00Z".into(),
            days: 1,
            starting_balance: 1000.0,
            fee_bps: 7.5,
            slippage_bps: 5.0,
            step_minutes: 20,
            output_dir: out.to_path_buf(),
            tags_history: None,
            tags_max_age_minutes: 90,
            biz_weights: None,
            biz_min_posters: 2,
        }
    }

    /// Hourly tags feeds over the window and the day before it, all about SOL.
    fn tags_history() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for hour in 0..48_i64 {
            let as_of = T_END - 2 * DAY_MS + hour * 3_600_000;
            let m6 = 2 + hour % 7;
            let body = format!(
                r#"{{"schema":"tags-feed-v1","as_of":"{}","sources":[{{"id":"biz","model":"m","prompt_version":"p"}}],"tags":{{"asset:SOL":{{"kind":"asset","name":"SOL","windows":{{"6h":{{"mentions":{m6},"posters":3,"up":2.0,"down":{}}},"24h":{{"mentions":12,"posters":5,"up":3.0,"down":1.0}}}}}}}}}}"#,
                iso(as_of),
                hour % 3
            );
            std::fs::write(dir.path().join(format!("{hour:02}.json")), body).unwrap();
        }
        dir
    }

    #[test]
    fn biz_algo_is_tuned_on_the_older_half_and_judged_on_the_newer() {
        let cache = cache_dir();
        let feeds = tags_history();
        let out = tempfile::tempdir().unwrap();
        let mut a = args(cache.path(), out.path());
        a.tags_history = Some(feeds.path().to_path_buf());
        let (results, paths) = run_backtest(&a).unwrap();
        assert_eq!(
            results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["biz_algo@older", "biz_algo@newer"]
        );
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        for needle in ["## `biz_algo` rule", "from 72 weight sets", "48 feeds", "biz_algo@newer"] {
            assert!(md.contains(needle), "missing {needle:?}: {md}");
        }
        // Every newer step sees a feed, and never one from after the step.
        let newer = &results[1];
        let trace = newer.trace.as_ref().unwrap();
        assert_eq!(trace.rows.len(), newer.steps);
        for row in &trace.rows {
            let cols: Vec<&str> = row.split(',').collect();
            assert!(!cols[1].is_empty() && cols[1] <= cols[0], "{row}");
        }
        assert!(paths.contains(&out.path().join("algo_backtest_2026-07-23_biz_algo@newer_trace.csv")));

        // Fixed weights skip the tuning.
        a.biz_weights = Some("1,1,1,0.5,0".into());
        let (_, paths) = run_backtest(&a).unwrap();
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        assert!(md.contains("Given with `--biz-weights`"), "{md}");
        a.biz_weights = Some("1,1".into());
        assert!(run_backtest(&a).unwrap_err().to_string().contains("need 5 numbers"));
    }

    #[test]
    fn writes_both_halves_deterministically() {
        let cache = cache_dir();
        let out = tempfile::tempdir().unwrap();
        let (results, paths) = run_backtest(&args(cache.path(), out.path())).unwrap();
        assert_eq!(
            results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["biz_algo@older", "biz_algo@newer"]
        );
        assert!(paths[0].ends_with("algo_backtest_2026-07-23.md"));
        let csv = std::fs::read_to_string(&paths[1]).unwrap();
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| r.split(',').count() == 8 && !r.contains(",,"))
        );
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        for needle in ["Survivorship", "25 bps per round trip", "Out of sample", "not loaded (no `--tags-history`)"] {
            assert!(md.contains(needle), "missing {needle:?}");
        }
        let trades = std::fs::read_to_string(&paths[2]).unwrap();
        let total: usize = results.iter().map(|r| r.trades.len()).sum();
        assert_eq!(trades.lines().count(), total + 1);

        let again = tempfile::tempdir().unwrap();
        run_backtest(&args(cache.path(), again.path())).unwrap();
        for path in &paths {
            let name = path.file_name().unwrap();
            assert_eq!(
                std::fs::read(path).unwrap(),
                std::fs::read(again.path().join(name)).unwrap(),
                "{name:?} differs between runs"
            );
        }
    }

    #[test]
    fn a_missing_or_short_cache_fails_without_fetching() {
        let empty = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let err = run_backtest(&args(empty.path(), out.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing cache"), "{err}");

        let cache = cache_dir();
        std::fs::remove_file(cache.path().join("1m_SOL_USDT_1_2.json")).unwrap();
        let err = run_backtest(&args(cache.path(), out.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing cache for SOL"), "{err}");

        let cache = cache_dir();
        let mut long = args(cache.path(), out.path());
        long.days = 3; // needs bars from 2026-07-13T08:00Z, the cache starts 07-14
        let err = run_backtest(&long).unwrap_err().to_string();
        assert!(
            err.contains("missing cache: no liquid coin has bars from"),
            "{err}"
        );

        let mut late = args(cache.path(), out.path());
        late.end = "2026-07-24T08:00:00Z".into();
        let err = run_backtest(&late).unwrap_err().to_string();
        assert!(err.contains("end before the window end"), "{err}");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    }
}

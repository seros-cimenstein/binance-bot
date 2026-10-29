//! `bb bot-spot-persona-backtest`: run the personas over the offline cache and write the
//! report (`.md`), the summary (`.csv`) and the trade list (`_trades.csv`).
//!
//! The output is deterministic: the same cache and flags give byte-identical files (no run
//! time or wall clock in them).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};

use super::data::{BizHistory, MINUTE_MS, Market};
use super::engine::{Config, LIQUID, Persona, RunResult, run};
use super::personas::{
    BizAlgo, CONTRARIAN_EXIT_PLAN, Contrarian, Detached, Ensemble, HIGH_VARIANCE_EXIT_PLAN,
    HighVariance, Rotation, Schizo, SchizoMdp, V13, V13_EXIT_PLAN,
};
use crate::decider::{Rule, Weights};
use crate::error::{Result, TraderError};
use crate::gcp2::Gcp2Feed;
use crate::mdp::Model;

/// The Phase 48 variants `all` expands to, in report order.
pub const ALL_PERSONAS: [&str; 6] = [
    "v13",
    "contrarian",
    "detached",
    "high_variance",
    "rotation",
    "ensemble",
];
/// Every persona `--persona` accepts by name (`schizo`, Phase 49, `schizo_mdp`, Phase 50, and
/// `biz_algo`, Phase 51, are not part of `all`).
pub const KNOWN_PERSONAS: [&str; 9] = [
    "v13",
    "contrarian",
    "detached",
    "high_variance",
    "rotation",
    "ensemble",
    "schizo",
    "schizo_mdp",
    "biz_algo",
];
/// `biz_algo` grid rows listed in the report.
const GRID_ROWS_SHOWN: usize = 5;
const DAY_MS: i64 = 86_400_000;
/// History needed before the window start: 24h breadth lookback plus the 22 closed 1h bars
/// of an hourly ATR (rounded up).
const WARM_UP_MS: i64 = 48 * 3_600_000;
/// Binance minimum order value the engine enforces on buys.
const MIN_NOTIONAL_USDT: f64 = 5.0;
/// Measured live cost per round trip (40 trips, `docs/archive/loops/LOOP_MONEY.md`).
const MEASURED_ROUND_TRIP_BPS: f64 = 14.8;
/// Phase 47 (`bot-spot-v13-agentic`) went live at 2026-10-18T14:02:04Z.
const PHASE_47_DEPLOY_MS: i64 = 1_792_332_124_000;

/// Run the Phase 48 personas on cached 1m candles (offline, research only, no orders).
#[derive(clap::Args, Debug, Clone)]
pub struct PersonaBacktestArgs {
    /// Directory of `1m_<BASE>_USDT_<start>_<end>.json` files (the `bb entry-study` cache).
    #[arg(long, default_value = "data/entry-study-cache")]
    pub cache_dir: PathBuf,
    /// Window end (ISO 8601 / RFC 3339, whole minute), e.g. `2026-10-18T10:43:00Z`.
    #[arg(long)]
    pub end: String,
    /// Window length in days.
    #[arg(long, default_value_t = 60)]
    pub days: i64,
    /// Persona to run, repeatable: v13, contrarian, detached, high_variance, rotation,
    /// ensemble, schizo, schizo_mdp, biz_algo, or `all`. With `biz_algo` the window is split
    /// in an older (tuning) and a newer (judging) half and every persona runs on both.
    #[arg(long = "persona", default_value = "all")]
    pub personas: Vec<String>,
    /// Starting USDT balance of every variant.
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
    /// `bb gcp2-history` cache (monthly CSVs and `colorzones.json`); a missing directory
    /// runs without GCP2.
    #[arg(long, default_value = "data/gcp2-cache")]
    pub gcp2_cache_dir: PathBuf,
    /// `bb bot-spot-mdp-fit` model for `schizo_mdp`; it must be fitted on data that ends
    /// before the window starts.
    #[arg(long)]
    pub mdp_model: Option<PathBuf>,
    /// `biz-feed-v1` history (`biz-insights feed --history`) for `biz_algo`; without it the
    /// /biz/ terms are 0.
    #[arg(long)]
    pub biz_feeds: Option<PathBuf>,
    /// A feed older than this at a step is not visible (hourly history needs more than 60).
    #[arg(long, default_value_t = 90)]
    pub biz_max_age_minutes: i64,
    /// Fixed `biz_algo` weights `talk,stance,momentum,value,buy_above,sell_below` (e.g. read
    /// off the `bb biz-lead-study` report) instead of tuning them on the older half.
    #[arg(long)]
    pub biz_weights: Option<String>,
    /// Distinct posters a 6h /biz/ window needs before its stance counts.
    #[arg(long, default_value_t = 2)]
    pub biz_min_posters: u64,
}

/// Expand `all`, drop repeats (first mention wins) and reject unknown names.
pub fn resolve_personas(names: &[String]) -> Result<Vec<&'static str>> {
    let mut out: Vec<&'static str> = Vec::new();
    for name in names {
        let expanded: Vec<&'static str> = if name == "all" {
            ALL_PERSONAS.to_vec()
        } else {
            let known = KNOWN_PERSONAS.iter().find(|p| **p == name).ok_or_else(|| {
                TraderError::config(format!(
                    "unknown persona {name:?}; use one of {} or all",
                    KNOWN_PERSONAS.join(", ")
                ))
            })?;
            vec![*known]
        };
        for p in expanded {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    if out.is_empty() {
        return Err(TraderError::config("no persona selected"));
    }
    Ok(out)
}

/// What some personas need beyond the market: the long-term value model (`schizo_mdp`,
/// `biz_algo`), the /biz/ feed history and the decider rule (`biz_algo`).
#[derive(Debug, Clone, Default)]
pub struct PersonaInputs {
    pub mdp: Option<Arc<Model>>,
    pub biz: Option<Arc<BizHistory>>,
    pub rule: Option<Rule>,
}

fn build_persona(name: &str, cfg: Config, inputs: &PersonaInputs) -> Result<Box<dyn Persona>> {
    Ok(match name {
        "v13" => Box::new(V13::new()?),
        "contrarian" => Box::new(Contrarian::new()?),
        "detached" => Box::new(Detached),
        "high_variance" => Box::new(HighVariance::new()?),
        "rotation" => Box::new(Rotation::new(cfg)?),
        "ensemble" => Box::new(Ensemble::new()?),
        "schizo" => Box::new(Schizo::new()?),
        "schizo_mdp" => Box::new(SchizoMdp::new(Arc::clone(inputs.mdp.as_ref().ok_or_else(
            || TraderError::config("schizo_mdp needs --mdp-model"),
        )?))?),
        "biz_algo" => Box::new(BizAlgo::new(
            inputs
                .rule
                .ok_or_else(|| TraderError::config("biz_algo needs its rule (tuned or --biz-weights)"))?,
            inputs.biz.clone(),
            inputs.mdp.clone(),
        )),
        other => return Err(TraderError::config(format!("unknown persona {other:?}"))),
    })
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
            "missing cache: no liquid coin has bars from {} (48h before the window start)",
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
    /// One line describing the GCP2 history the run used (or that it had none).
    pub gcp2: String,
    /// The report's file stem, for naming the per-variant trace files.
    pub stem: String,
    /// Minutes of the window per GCP2 zone (in zone order) and minutes without a reading;
    /// `None` without GCP2 history.
    pub gcp2_zone_minutes: Option<(Vec<(String, usize)>, usize)>,
    /// The older/newer split and the `biz_algo` rule; `None` unless `biz_algo` ran.
    pub split: Option<SplitReport>,
}

/// One `biz_algo` weight set tried on the older half.
#[derive(Debug, Clone, PartialEq)]
pub struct GridRow {
    pub weights: Weights,
    pub net_pnl_pct: f64,
    pub round_trips: usize,
    pub max_drawdown_pct: f64,
}

/// The split of a run with `biz_algo`.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitReport {
    /// First step of the newer half.
    pub mid_ms: i64,
    pub rule: Rule,
    /// Every weight set tried on the older half, in grid order; `None` when the weights
    /// came from `--biz-weights`.
    pub grid: Option<Vec<GridRow>>,
    /// One line describing the /biz/ feed history.
    pub biz: String,
}

/// Minutes of `[start_ms, end_ms)` per GCP2 zone (zone order) and without a reading.
pub fn gcp2_zone_minutes(
    feed: &Gcp2Feed,
    start_ms: i64,
    end_ms: i64,
) -> (Vec<(String, usize)>, usize) {
    let mut counts = vec![0_usize; feed.zones().zones.len()];
    let mut unavailable = 0;
    let mut m = start_ms;
    while m < end_ms {
        match feed.view_at(m) {
            Some(view) => counts[view.zone] += 1,
            None => unavailable += 1,
        }
        m += MINUTE_MS;
    }
    let zones = feed
        .zones()
        .zones
        .iter()
        .zip(counts)
        .map(|(z, n)| (z.label.clone(), n))
        .collect();
    (zones, unavailable)
}

/// Run the selected personas; one result per persona, in the order given.
pub fn run_personas(
    market: &Market,
    cfg: &Config,
    names: &[&str],
    inputs: &PersonaInputs,
) -> Result<Vec<RunResult>> {
    names
        .iter()
        .map(|name| run(market, cfg, build_persona(name, *cfg, inputs)?))
        .collect()
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
/// feeds), momentum 0/1, value 0/1 (0 only without a model), buy at 0.5/1, sell below
/// −0.5/0.
pub fn weight_grid(with_biz: bool, with_mdp: bool) -> Vec<Weights> {
    let signed: &[f64] = if with_biz { &[-1.0, 0.0, 1.0] } else { &[0.0] };
    let values: &[f64] = if with_mdp { &[0.0, 1.0] } else { &[0.0] };
    let mut out = Vec::new();
    for &talk in signed {
        for &stance in signed {
            for momentum in [0.0, 1.0] {
                for &value in values {
                    for buy_above in [0.5, 1.0] {
                        for sell_below in [-0.5, 0.0] {
                            out.push(Weights {
                                talk,
                                stance,
                                momentum,
                                value,
                                buy_above,
                                sell_below,
                            });
                        }
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
    inputs: &PersonaInputs,
    min_posters: u64,
) -> Result<(Weights, Vec<GridRow>)> {
    let mut rows = Vec::new();
    for weights in weight_grid(inputs.biz.is_some(), inputs.mdp.is_some()) {
        let persona = BizAlgo::new(
            Rule {
                weights,
                min_posters,
            },
            inputs.biz.clone(),
            inputs.mdp.clone(),
        );
        let r = run(market, older, Box::new(persona))?;
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

/// Tune (or take) the `biz_algo` rule on the older half, then run every persona on the
/// older and the newer half (`<name>@older`, `<name>@newer`).
fn run_split(
    market: &Market,
    cfg: &Config,
    names: &[&str],
    inputs: &mut PersonaInputs,
    args: &PersonaBacktestArgs,
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
        Some(spec) => (Weights::parse(spec).map_err(TraderError::config)?, None),
        None => {
            let (weights, rows) = tune(market, &older, inputs, args.biz_min_posters)?;
            (weights, Some(rows))
        }
    };
    let rule = Rule {
        weights,
        min_posters: args.biz_min_posters,
    };
    inputs.rule = Some(rule);
    let mut results = Vec::new();
    for (label, half) in [("older", older), ("newer", newer)] {
        for mut r in run_personas(market, &half, names, inputs)? {
            r.name = format!("{}@{label}", r.name);
            results.push(r);
        }
    }
    let split = SplitReport {
        mid_ms,
        rule,
        grid,
        biz: biz_summary(inputs.biz.as_deref(), args.biz_feeds.as_deref()),
    };
    Ok((results, split))
}

fn biz_summary(history: Option<&BizHistory>, dir: Option<&Path>) -> String {
    let (Some(history), Some(dir)) = (history, dir) else {
        return "not loaded (no `--biz-feeds`): the talk and stance terms are 0, so `biz_algo` trades on momentum and the value model alone".to_string();
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

fn rules(name: &str) -> String {
    match name.split('@').next().unwrap_or(name) {
        "v13" => format!(
            "rule proxy of the live v13/v14 guidance (not the LLM): liquid top 10; buy when flat, breadth ≥ +15 and 4h market median ≥ +1%: the best 4h coin with positive 1h and 4h, all cash; exit `{V13_EXIT_PLAN}` or sell at breadth ≤ −15"
        ),
        "contrarian" => format!(
            "fades breadth: liquid top 10; buy when flat and breadth ≤ −15: the worst 4h coin, all cash; exit `{CONTRARIAN_EXIT_PLAN}`, or sell at a step on a +1.5% 1h bounce or breadth ≥ +15"
        ),
        "detached" => "raw price action only (no breadth, volume, liquid filter or exit plan): all cached coins; buy when flat: the best 1h coin above +0.25% with a rising 15m, all cash; sell when 15m and 1h are both negative; rotate when another coin leads on 1h by more than 0.5 points".to_string(),
        "high_variance" => format!(
            "liquid top 10; every step buys each coin not held that is up ≥ +0.5% over 1h, strongest first, up to 3 positions, sized clamp(|1h| / 2%, 0.25, 1.0) of equity capped by cash; exit `{HIGH_VARIANCE_EXIT_PLAN}` only"
        ),
        "rotation" => "shadow-runs the four base personas; at the first step of each UTC day follows the one with the best trailing-72h shadow return (v13 on ties and before 72h exist)".to_string(),
        "ensemble" => format!(
            "the four base personas vote on its own book; buys the coin with ≥ 2 buy votes (most votes, then 24h volume), all cash; sells on ≥ 2 sell votes or when `{V13_EXIT_PLAN}` fires"
        ),
        "schizo" => "Phase 49, ruled by GCP2: the mood (one of the four base personas) is picked by the GCP2 dice and switches only when the GCP2 zone changes between steps (the mood follows GCP2: an unchanged zone keeps it); the four base personas vote with the mood's vote counting twice (buy or sell at ≥ 2), one coin with all cash; a position keeps the exit plan of the mood that bought it; no stop-loss".to_string(),
        "schizo_mdp" => format!("Phase 50: `schizo` with the long-term value voice (`bb bot-spot-mdp-fit`, a Markov model of 20-minute steps, fitted before the window) as the strongest voice: a vote sell goes through only when the model values selling above holding, the model sells on its own when it does, and a buy goes through only when its long-term value after the entry cost is above 0 (cash); a buy whose mood has no exit plan (`detached`) runs the live default plan `{V13_EXIT_PLAN}`"),
        "biz_algo" => "Phase 51, no LLM: the rule-based decider scores each liquid-top coin as w_talk·talk + w_stance·stance + w_momentum·momentum + w_value·value (talk = the /biz/ 6h mention rate over its 24h rate − 1; stance = (bullish − bearish) / tagged of the 6h AI tags with enough posters; momentum = 1h return / 5%, capped; value = the long-term value model's verdict); a held coin scoring below the sell level is sold, and with nothing held the best coin is bought with all cash at or above the buy level; no exit plan".to_string(),
        other => other.to_string(),
    }
}

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
        "# Schizoid persona backtest ({} → {})\n",
        iso(cfg.start_ms),
        iso(cfg.end_ms)
    );
    let _ = writeln!(
        out,
        "Research only: `bb bot-spot-persona-backtest`, offline on `{}` ({} coins), no network, no orders. Phase 48, `docs/archive/implementation-plan-phases-1-50.md`.\n",
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
        "- Each variant starts with {:.2} USDT. Costs per side: fee {} bps + slippage {} bps ({} bps per round trip).",
        cfg.starting_balance, ctx.fee_bps, ctx.slippage_bps, round_trip_bps
    );
    let _ = writeln!(
        out,
        "- Liquid universe ({} coins): {}.",
        LIQUID.len(),
        LIQUID.join(", ")
    );
    if let Some(split) = &ctx.split {
        let _ = writeln!(
            out,
            "- Split (`biz_algo` selected): the older half {} → {} tunes `biz_algo`, the newer half {} → {} judges it; every persona runs on both halves (`<persona>@older`, `<persona>@newer`), so the step count above is the older half's.",
            iso(cfg.start_ms),
            iso(split.mid_ms),
            iso(split.mid_ms),
            iso(cfg.end_ms)
        );
        let _ = writeln!(out, "- /biz/ feeds: {}.", split.biz);
    }
    let _ = writeln!(out, "- GCP2: {}.\n", ctx.gcp2);
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
    let _ = writeln!(out, "## Personas\n");
    for r in results {
        let _ = writeln!(out, "- `{}`: {}.", r.name, rules(&r.name));
    }
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
    if let Some(split) = &ctx.split {
        let _ = writeln!(out, "## `biz_algo` rule\n");
        let _ = writeln!(
            out,
            "- Rule: {}; stances need {} distinct posters in the 6h window.",
            split.rule.weights, split.rule.min_posters
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
    if let Some((zones, unavailable)) = &ctx.gcp2_zone_minutes {
        let total = zones.iter().map(|(_, n)| n).sum::<usize>() + unavailable;
        let pct = |n: usize| {
            if total == 0 {
                0.0
            } else {
                n as f64 / total as f64 * 100.0
            }
        };
        let _ = writeln!(out, "## GCP2\n");
        let _ = writeln!(
            out,
            "- Zone minutes over the window (each minute's reading is the hour ending at the second before it): {}; unavailable {} ({:.1}%).",
            zones
                .iter()
                .map(|(label, n)| format!("{label} {n} ({:.1}%)", pct(*n)))
                .collect::<Vec<_>>()
                .join(", "),
            unavailable,
            pct(*unavailable)
        );
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "## Assumptions\n");
    for line in [
        "Decisions see only bars closed by the step time; intents fill at the open of the 1m bar that opens at the step, sells before buys.".to_string(),
        format!("The {MIN_NOTIONAL_USDT} USDT minimum applies to buys only; a held position can always be sold."),
        "A buy whose exit plan cannot start (e.g. no hourly ATR yet) is skipped and counted (`exit_plan_unavailable`); a buy with no cash left is counted as `no_cash`.".to_string(),
        "Equity, drawdown and positions still open at the end are marked at the last close without exit costs.".to_string(),
        "At an exact 15-minute boundary, breadth uses the bucket that holds the last closed minute.".to_string(),
        "The `stretch` exit counts break-even with fees only (as the live rule does), not slippage.".to_string(),
        "Exit plans read each 1m bar low before high (the pessimistic order `bb bot-spot-exit-replay` uses); ATR comes from the closed 1h bars before entry and stretch/slowdown get 60 minutes of 1m warm-up.".to_string(),
        "`rotation` scores its shadows at the first scheduled step of each new UTC day (00:03 in the default window, keeping the step cadence). A switch step only sells the old book; the new persona trades from the next step on a flat book.".to_string(),
    ] {
        let _ = writeln!(out, "- {line}");
    }
    if ctx.gcp2_zone_minutes.is_some() {
        let _ = writeln!(
            out,
            "- GCP2 at a step is `|Σ network_coherence|` over the hour ending at the last second before the step's minute (the live `getNetVarAggregate24H` value); an hour with under 90% of its seconds is unavailable, and zones are GCP2's own published bounds."
        );
    }
    let _ = writeln!(out, "\n## Caveats\n");
    let window_note = if cfg.end_ms <= PHASE_47_DEPLOY_MS {
        "The window ends before the Phase 47 (v13) deploy at 2026-10-18T14:02:04Z, so the `v13` row cannot be compared with live v13 trades on the same dates."
    } else {
        "The window reaches past the Phase 47 (v13) deploy at 2026-10-18T14:02:04Z; live v13 trades exist only after that time."
    };
    for line in [
        "The `v13` row is a deterministic rule proxy of the v13/v14 guidance, not the LLM trader, which cannot be replayed (cost, quota, live-only tools).".to_string(),
        "Survivorship: the liquid universe is the top 20 by volume as of 2026-10-18, applied to the whole window.".to_string(),
        format!(
            "Costs: {round_trip_bps} bps per round trip, against the measured {MEASURED_ROUND_TRIP_BPS} bps of the 40 live trips (deliberately pessimistic)."
        ),
        window_note.to_string(),
        if ctx.split.is_some() {
            "Out of sample: `biz_algo`'s weights were picked on the older half only, so its `@newer` row is the honest result; the other personas' rules were fixed before any run. Each half is about half the window: few trades, a small sample.".to_string()
        } else {
            "No out-of-sample split: the rules were fixed in `docs/archive/loops/LOOP_SCHIZOID.md` before any real-cache run and not tuned on it, but they are tested on this one period only.".to_string()
        },
    ] {
        let _ = writeln!(out, "- {line}");
    }
    if ctx.gcp2_zone_minutes.is_some() {
        let _ = writeln!(
            out,
            "- GCP2 (gcp2.net, a network of random-number generators) has no shown link to crypto prices; `schizo` reads it only because the operator chose it (`docs/decisions.md`)."
        );
    }
    out
}

/// File stem for a window ending at `end_ms`: `schizoid_backtest_<YYYY-MM-DD>`.
pub fn file_stem(end_ms: i64) -> String {
    let date = DateTime::<Utc>::from_timestamp_millis(end_ms)
        .map_or_else(|| end_ms.to_string(), |d| d.format("%Y-%m-%d").to_string());
    format!("schizoid_backtest_{date}")
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
pub fn run_backtest(args: &PersonaBacktestArgs) -> Result<(Vec<RunResult>, Vec<PathBuf>)> {
    if args.days <= 0 {
        return Err(TraderError::config("--days must be positive"));
    }
    let end_ms = parse_end(&args.end)?;
    let start_ms = end_ms - args.days * DAY_MS;
    let names = resolve_personas(&args.personas)?;
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
    let mdp = match &args.mdp_model {
        Some(path) => {
            let model = Model::load(path)?;
            let fitted_to = DateTime::parse_from_rfc3339(&model.data_to)
                .map_err(|e| TraderError::config(format!("MDP model data_to {:?}: {e}", model.data_to)))?
                .timestamp_millis();
            if fitted_to >= start_ms {
                return Err(TraderError::config(format!(
                    "the MDP model saw data up to {}, inside the window that starts {} (look-ahead); refit with --end before the window",
                    model.data_to,
                    iso(start_ms)
                )));
            }
            Some(Arc::new(model))
        }
        None => None,
    };
    let biz = match &args.biz_feeds {
        Some(dir) => Some(Arc::new(BizHistory::load_dir(dir, args.biz_max_age_minutes)?)),
        None => None,
    };
    let mut inputs = PersonaInputs {
        mdp,
        biz,
        rule: None,
    };
    let gcp2 = load_gcp2(&args.gcp2_cache_dir)?;
    let gcp2_line = gcp2_summary(gcp2.as_ref(), &args.gcp2_cache_dir);
    let market = Market::load_dir(&args.cache_dir)?.with_gcp2(gcp2);
    check_coverage(&market, start_ms, end_ms)?;
    let (results, split) = if names.contains(&"biz_algo") {
        let (results, split) = run_split(&market, &cfg, &names, &mut inputs, args)?;
        (results, Some(split))
    } else {
        (run_personas(&market, &cfg, &names, &inputs)?, None)
    };
    let zone_minutes = market
        .gcp2()
        .map(|feed| gcp2_zone_minutes(feed, start_ms, end_ms));
    let stem = file_stem(end_ms);
    let ctx = ReportContext {
        cfg,
        fee_bps: args.fee_bps,
        slippage_bps: args.slippage_bps,
        cache_dir: args.cache_dir.display().to_string(),
        coins: market.len(),
        gcp2: gcp2_line,
        stem: stem.clone(),
        gcp2_zone_minutes: zone_minutes,
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

/// GCP2 history from `dir`; `None` when the directory does not exist (a present but broken
/// cache is an error).
pub fn load_gcp2(dir: &Path) -> Result<Option<Gcp2Feed>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    Gcp2Feed::load_dir(dir).map(Some)
}

fn gcp2_summary(feed: Option<&Gcp2Feed>, dir: &Path) -> String {
    let Some(feed) = feed else {
        return format!("not loaded (`{}` does not exist)", dir.display());
    };
    let last = feed
        .series()
        .last_second()
        .map_or_else(|| "none".to_string(), |s| iso(s * 1000));
    let zones = feed
        .zones()
        .zones
        .iter()
        .map(|z| format!("{} from {}", z.label, z.start))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "`{}`, per-second network coherence up to {last}; colour zones as published by GCP2 at {} ({zones})",
        dir.display(),
        feed.zones().fetched_at
    )
}

fn write(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body)?;
    Ok(())
}

/// `bb bot-spot-persona-backtest`.
pub fn run_cli(args: &PersonaBacktestArgs) -> Result<ExitCode> {
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

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn persona_names_expand_dedupe_and_reject_unknowns() {
        assert_eq!(
            resolve_personas(&names(&["all"])).unwrap(),
            ALL_PERSONAS.to_vec()
        );
        assert_eq!(
            resolve_personas(&names(&["detached", "v13", "detached"])).unwrap(),
            vec!["detached", "v13"]
        );
        assert_eq!(
            resolve_personas(&names(&["ensemble", "all"])).unwrap()[..2],
            ["ensemble", "v13"]
        );
        let err = resolve_personas(&names(&["chaos"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown persona \"chaos\""), "{err}");
    }

    const T_END: i64 = 1_784_448_000_000 + 4 * DAY_MS; // 2026-07-23T08:00Z

    /// Writes 4 days of 1m bars for every liquid coin (plus INJ), each with its own drift
    /// and wiggle, ending at `T_END`.
    fn cache_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let start = T_END - 4 * DAY_MS;
        let mut coins: Vec<&str> = LIQUID.to_vec();
        coins.push("INJ");
        for (i, coin) in coins.iter().enumerate() {
            let k = i as f64;
            let rows: Vec<String> = (0..4 * 1440)
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

    fn args(cache: &Path, out: &Path) -> PersonaBacktestArgs {
        PersonaBacktestArgs {
            cache_dir: cache.to_path_buf(),
            end: "2026-07-23T08:00:00Z".into(),
            days: 1,
            personas: names(&["all"]),
            starting_balance: 1000.0,
            fee_bps: 7.5,
            slippage_bps: 5.0,
            step_minutes: 20,
            output_dir: out.to_path_buf(),
            gcp2_cache_dir: cache.join("no-gcp2"),
            mdp_model: None,
            biz_feeds: None,
            biz_max_age_minutes: 90,
            biz_weights: None,
            biz_min_posters: 2,
        }
    }

    /// Hourly /biz/ feeds over the window and the day before it, all about SOL.
    fn biz_feeds() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for hour in 0..48_i64 {
            let as_of = T_END - 2 * DAY_MS + hour * 3_600_000;
            let m6 = 2 + hour % 7;
            let body = format!(
                r#"{{"schema":"biz-feed-v1","as_of":"{}","model":"m","prompt_version":"p","coins":{{"SOL":{{"6h":{{"mentions":{m6},"posters":3,"bullish":2.0,"bearish":{}}},"24h":{{"mentions":12,"posters":5,"bullish":3.0,"bearish":1.0}}}}}}}}"#,
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
        let feeds = biz_feeds();
        let out = tempfile::tempdir().unwrap();
        let mut a = args(cache.path(), out.path());
        a.personas = names(&["v13", "biz_algo"]);
        a.biz_feeds = Some(feeds.path().to_path_buf());
        let (results, paths) = run_backtest(&a).unwrap();
        assert_eq!(
            results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["v13@older", "biz_algo@older", "v13@newer", "biz_algo@newer"]
        );
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        for needle in ["## `biz_algo` rule", "from 72 weight sets", "48 feeds", "biz_algo@newer"] {
            assert!(md.contains(needle), "missing {needle:?}: {md}");
        }
        // Every newer step sees a feed, and never one from after the step.
        let newer = &results[3];
        let trace = newer.trace.as_ref().unwrap();
        assert_eq!(trace.rows.len(), newer.steps);
        for row in &trace.rows {
            let cols: Vec<&str> = row.split(',').collect();
            assert!(!cols[1].is_empty() && cols[1] <= cols[0], "{row}");
        }
        assert!(paths.contains(&out.path().join("schizoid_backtest_2026-07-23_biz_algo@newer_trace.csv")));

        // Fixed weights skip the tuning.
        a.biz_weights = Some("1,1,1,0,0.5,0".into());
        let (_, paths) = run_backtest(&a).unwrap();
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        assert!(md.contains("Given with `--biz-weights`"), "{md}");
        a.biz_weights = Some("1,1".into());
        assert!(run_backtest(&a).unwrap_err().to_string().contains("need 6 numbers"));
    }

    #[test]
    fn writes_all_six_variants_deterministically() {
        let cache = cache_dir();
        let out = tempfile::tempdir().unwrap();
        let (results, paths) = run_backtest(&args(cache.path(), out.path())).unwrap();
        assert_eq!(
            results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ALL_PERSONAS.to_vec()
        );
        assert!(paths[0].ends_with("schizoid_backtest_2026-07-23.md"));
        let csv = std::fs::read_to_string(&paths[1]).unwrap();
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert_eq!(rows.len(), 6);
        assert!(
            rows.iter()
                .all(|r| r.split(',').count() == 8 && !r.contains(",,"))
        );
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        for needle in [
            "rule proxy",
            "Survivorship",
            "25 bps per round trip",
            "out-of-sample",
            "first scheduled step of each new UTC day",
        ] {
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
        long.days = 3; // needs bars from 2026-07-18T08:00Z, the cache starts 07-19
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

    #[test]
    fn the_gcp2_cache_is_optional_but_must_be_valid_when_present() {
        let cache = cache_dir();
        let out = tempfile::tempdir().unwrap();
        // A missing directory runs without GCP2 and says so.
        run_backtest(&args(cache.path(), out.path())).unwrap();
        let md =
            std::fs::read_to_string(out.path().join("schizoid_backtest_2026-07-23.md")).unwrap();
        assert!(md.contains("- GCP2: not loaded"), "{md}");
        assert!(
            !md.contains("## GCP2") && !md.contains("random-number generators"),
            "{md}"
        );
        // An existing but empty directory is an error, not a silent skip.
        let gcp2 = tempfile::tempdir().unwrap();
        let mut with = args(cache.path(), out.path());
        with.gcp2_cache_dir = gcp2.path().to_path_buf();
        let err = run_backtest(&with).unwrap_err().to_string();
        assert!(err.contains("missing GCP2 cache"), "{err}");
        // A valid cache is attached and described.
        let zones = crate::persona_backtest::engine::tests::gcp2_zones();
        std::fs::write(
            gcp2.path().join("colorzones.json"),
            serde_json::to_string(&zones).unwrap(),
        )
        .unwrap();
        let start_s = (T_END - 4 * DAY_MS) / 1000;
        let rows: String = (0..4 * 86_400)
            .map(|i| {
                format!(
                    "{},{:.4},400\n",
                    start_s + i,
                    ((i as f64) / 5000.0).sin() * 0.11
                )
            })
            .collect();
        std::fs::write(
            gcp2.path()
                .join("GCP2_Network_Coherence_Global_Network_2026_07.csv"),
            format!("epoch_time_utc,network_coherence,active_devices\n{rows}"),
        )
        .unwrap();
        let (results, _) = run_backtest(&with).unwrap();
        assert_eq!(results.len(), 6);
        let md =
            std::fs::read_to_string(out.path().join("schizoid_backtest_2026-07-23.md")).unwrap();
        assert!(
            md.contains("per-second network coherence up to 2026-07-23T07:59:59Z"),
            "{md}"
        );
        assert!(md.contains("Normal from 0, Elevated from 140"), "{md}");
    }

    #[test]
    fn the_schizo_trace_records_every_simulated_step() {
        use crate::persona_backtest::engine::tests::{gcp2_wave, gcp2_zones};
        let cache = cache_dir();
        let out = tempfile::tempdir().unwrap();
        let gcp2 = tempfile::tempdir().unwrap();
        std::fs::write(
            gcp2.path().join("colorzones.json"),
            serde_json::to_string(&gcp2_zones()).unwrap(),
        )
        .unwrap();
        // GCP2 starts at the window start (so the first steps have no full hour) and has a
        // 90-minute gap mid-window (so some later steps have no reading either).
        let window_start_s = (T_END - DAY_MS) / 1000;
        let gap = (window_start_s + 5 * 3_600)..(window_start_s + 5 * 3_600 + 5_400);
        let rows: String = (window_start_s..T_END / 1000)
            .filter(|s| !gap.contains(s))
            .map(|s| {
                let units = gcp2_wave(s - window_start_s);
                format!("{s},{:.4},400\n", units as f64 / 10_000.0)
            })
            .collect();
        std::fs::write(
            gcp2.path()
                .join("GCP2_Network_Coherence_Global_Network_2026_07.csv"),
            format!("epoch_time_utc,network_coherence,active_devices\n{rows}"),
        )
        .unwrap();
        let mut a = args(cache.path(), out.path());
        a.gcp2_cache_dir = gcp2.path().to_path_buf();
        a.personas = names(&["schizo"]);
        let (results, paths) = run_backtest(&a).unwrap();
        let schizo = &results[0];
        assert_eq!(schizo.name, "schizo");
        let trace_path = out
            .path()
            .join("schizoid_backtest_2026-07-23_schizo_trace.csv");
        assert_eq!(paths.last(), Some(&trace_path));
        let text = std::fs::read_to_string(&trace_path).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some(crate::persona_backtest::personas::MOOD_TRACE_HEADER)
        );
        let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
        assert_eq!(rows.len(), schizo.steps);

        let feed = load_gcp2(gcp2.path()).unwrap().unwrap();
        let (mut prev_to, mut prev_mood, mut seen_reading) = ("", "", false);
        for (k, row) in rows.iter().enumerate() {
            let t = T_END - DAY_MS + k as i64 * 20 * MINUTE_MS;
            assert_eq!(row[0], iso(t), "row {k}");
            let view = feed.view_at(t);
            assert_eq!(
                row[3],
                view.as_ref().map_or("", |v| v.zone_label.as_str()),
                "row {k}"
            );
            let value = view
                .as_ref()
                .map(|v| format!("{:.4}", v.value()))
                .unwrap_or_default();
            assert_eq!(row[1], value, "row {k}");
            let dice = view
                .as_ref()
                .map(|v| v.dice.to_string())
                .unwrap_or_default();
            assert_eq!(row[4], dice, "row {k}");
            assert_eq!(
                row[2], prev_to,
                "row {k}: from zone is the previous step's zone"
            );
            let expected = if view.is_none() {
                "no reading"
            } else if !seen_reading {
                "initial"
            } else if prev_to.is_empty() {
                "no reading"
            } else if row[2] != row[3] {
                "switched"
            } else {
                "held"
            };
            assert_eq!(row[5], expected, "row {k}");
            assert_eq!(
                row[6], prev_mood,
                "row {k}: mood before is the last mood after"
            );
            let changed = row[6] != row[7];
            assert_eq!(
                changed,
                expected == "initial" || expected == "switched",
                "row {k}"
            );
            seen_reading |= view.is_some();
            prev_to = row[3];
            prev_mood = row[7];
        }
        let count = |r: &str| rows.iter().filter(|row| row[5] == r).count();
        assert_eq!(count("initial"), 1);
        assert!(
            rows[0][5] == "no reading" && count("no reading") >= 6,
            "{text}"
        );
        assert!(count("switched") > 0 && count("held") > 0, "{text}");
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        assert!(
            md.contains(&format!(
                "Per-step trace: `schizoid_backtest_2026-07-23_schizo_trace.csv` ({} rows",
                rows.len()
            )),
            "{md}"
        );
        // The GCP2 section: zone minutes cover the window.
        let (zones, unavailable) = gcp2_zone_minutes(&feed, T_END - DAY_MS, T_END);
        assert_eq!(
            zones.iter().map(|(_, n)| n).sum::<usize>() + unavailable,
            1440
        );
        assert!(unavailable >= 59 + 90, "{unavailable}");
        assert!(md.contains("## GCP2"), "{md}");
        assert!(md.contains(&format!("unavailable {unavailable} (")), "{md}");
        assert!(
            md.contains("GCP2 (gcp2.net, a network of random-number generators) has no shown link")
        );
    }
}

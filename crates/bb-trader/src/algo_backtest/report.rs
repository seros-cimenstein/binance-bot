//! `bb algo-backtest`: run the operator's rule file ([`crate::rules`]) over a window of the
//! offline candle cache and the tags feed history, every sleeve as the live trader runs it,
//! and write the report (`.md`), the summary (`.csv`), the trade list (`_trades.csv`) and
//! one per-step trace per sleeve (`_<sleeve>_trace.csv`).
//!
//! No tuning: the rule file is taken as it is (both styles; an edge rule reads its
//! point-in-time scoreboards walk-forward, [`super::rule`]).
//!
//! The output is deterministic: the same cache, history, rule and flags give byte-identical
//! files (no run time or wall clock in them).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};

use super::data::{MINUTE_MS, Market, TagsHistory};
use super::engine::{COVERAGE, Config, LIQUID, RunResult, run};
use super::rule::{CandidateSet, RuleFile, TRACE_HEADER};
use crate::error::{Result, TraderError};
use crate::reach::Proxies;
use crate::rules::Rule;
use crate::scoreboard::Scoreboard;

const DAY_MS: i64 = 86_400_000;
/// History needed before the window start: the 7d return of the first step.
const WARM_UP_MS: i64 = 7 * DAY_MS;
/// Binance minimum order value the engine enforces on buys.
const MIN_NOTIONAL_USDT: f64 = 5.0;
/// Measured live cost per round trip (40 trips, `docs/archive/loops/LOOP_MONEY.md`).
const MEASURED_ROUND_TRIP_BPS: f64 = 14.8;

/// Backtest the operator's rule file on cached 1m candles and the tags feed history
/// (offline, research only, no orders).
#[derive(clap::Args, Debug, Clone)]
pub struct AlgoBacktestArgs {
    /// The operator's rule file (as `BOT_SPOT_ALGO_RULE`), e.g.
    /// `config/rules/algo-2026-10-29.json`.
    #[arg(long)]
    pub rule: PathBuf,
    /// Hourly `tags-feed-v1` history (`biz-insights feed --history`).
    #[arg(long)]
    pub tags_history: PathBuf,
    /// The operator's proxy file (as `BOT_SPOT_PROXIES`).
    #[arg(long, default_value = "config/proxies.json")]
    pub proxies: PathBuf,
    /// Edge rules only: a directory of point-in-time scoreboards (`bb tag-scoreboard
    /// --as-of`); each step uses the newest whose outcomes all closed by then.
    #[arg(long)]
    pub scoreboards: Option<PathBuf>,
    /// Which coins a step may buy: `tags` as live, or `liquid` (the liquid 20) to measure
    /// what the tag pick adds.
    #[arg(long, value_enum, default_value_t = CandidateSet::Tags)]
    pub candidates: CandidateSet,
    /// Directory of `1m_<BASE>_USDT_<start>_<end>.json` files (the `bb entry-study` cache).
    #[arg(long, default_value = "data/entry-study-cache")]
    pub cache_dir: PathBuf,
    /// Window end (ISO 8601 / RFC 3339, whole minute), e.g. `2026-10-25T14:00:00Z`.
    #[arg(long)]
    pub end: String,
    /// Window length in days.
    #[arg(long, default_value_t = 24)]
    pub days: i64,
    /// Starting USDT balance.
    #[arg(long, default_value_t = 1000.0)]
    pub starting_balance: f64,
    /// Fee per side in bps.
    #[arg(long, default_value_t = 7.5)]
    pub fee_bps: f64,
    /// Slippage per side in bps.
    #[arg(long, default_value_t = 5.0)]
    pub slippage_bps: f64,
    /// Minutes between decision steps.
    #[arg(long, default_value_t = 20)]
    pub step_minutes: i64,
    /// A feed older than this at a step is not visible (hourly history needs more than 60).
    #[arg(long, default_value_t = 90)]
    pub tags_max_age_minutes: i64,
    /// Where the report files go.
    #[arg(long, default_value = "reports")]
    pub output_dir: PathBuf,
}

/// Fail unless the cache holds every liquid coin and the gold proxies ([`COVERAGE`]),
/// they reach the window end, and at least one has the warm-up history before the window
/// start (late listings such as HYPE simply join when their bars start).
pub fn check_coverage(market: &Market, start_ms: i64, end_ms: i64) -> Result<()> {
    let required: Vec<&str> = LIQUID.iter().chain(&COVERAGE).copied().collect();
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|a| market.get(a).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(TraderError::config(format!(
            "missing cache for {} (needs every liquid coin and the gold proxies: {})",
            missing.join(", "),
            required.join(", ")
        )));
    }
    let short: Vec<&str> = required
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
    let earliest = required
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
    /// The report's file stem, for naming the per-sleeve trace files.
    pub stem: String,
    pub rule_path: String,
    pub rule: Rule,
    /// One line describing the tags feed history.
    pub tags: String,
    /// Edge rules: one line describing the scoreboards.
    pub scoreboards: Option<String>,
    pub candidates: CandidateSet,
}

fn tags_summary(history: &TagsHistory, dir: &Path, proxies: &Path) -> String {
    let span = |ms: Option<i64>| ms.map_or_else(|| "none".to_string(), iso);
    let readers: Vec<&str> = history.readers().iter().map(String::as_str).collect();
    format!(
        "`{}`, {} feeds from {} to {}; AI readers {}{}; proxies `{}`",
        dir.display(),
        history.len(),
        span(history.first_ms()),
        span(history.last_ms()),
        readers.join(", "),
        if readers.len() > 1 {
            " (mixed: stances from different models or instructions)"
        } else {
            ""
        },
        proxies.display()
    )
}

/// Every `*.json` in `dir` as a scoreboard (sorted by name); fails on a bad file or none.
fn load_scoreboards(dir: &Path) -> Result<Vec<Scoreboard>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| TraderError::config(format!("--scoreboards {}: {e}", dir.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(TraderError::config(format!(
            "--scoreboards {}: no scoreboard (*.json)",
            dir.display()
        )));
    }
    paths
        .iter()
        .map(|p| Scoreboard::read(p).map_err(TraderError::config))
        .collect()
}

fn scoreboards_summary(boards: &[Scoreboard], dir: &Path) -> String {
    let mut available: Vec<&str> = boards
        .iter()
        .filter_map(|b| b.available_at.as_deref())
        .collect();
    available.sort_unstable();
    format!(
        "`{}`, {} point-in-time scoreboards, available from {}",
        dir.display(),
        boards.len(),
        available.join(", ")
    )
}

/// The summary CSV: the portfolio, then each sleeve (its trades' net; no equity path of its
/// own, the cash is shared).
pub fn summary_csv(r: &RunResult) -> String {
    let mut out = String::from(
        "scope,net_pnl_usdt,net_pnl_pct,max_drawdown_pct,round_trips,win_rate_pct,sharpe,fees_usdt,open_at_end,skipped_orders\n",
    );
    let open = r.trades.iter().filter(|t| t.open_at_end).count();
    let _ = writeln!(
        out,
        "portfolio,{:.10},{:.6},{:.6},{},{:.4},{:.6},{:.10},{},{}",
        r.net_pnl_usdt,
        r.net_pnl_pct,
        r.max_drawdown_pct,
        r.round_trips,
        r.win_rate_pct,
        r.sharpe,
        r.fees_usdt,
        open,
        r.skipped_orders
    );
    for s in &r.sleeves {
        let _ = writeln!(
            out,
            "sleeve:{},{:.10},{:.6},,{},{:.4},,{:.10},{},{}",
            s.name,
            s.net_pnl_usdt,
            s.net_pnl_usdt / r.starting_balance * 100.0,
            s.round_trips,
            s.win_rate_pct,
            s.fees_usdt,
            s.open,
            s.skipped_orders
        );
    }
    out
}

/// The trade CSV: one row per round trip or still-open position, by entry time.
pub fn trades_csv(r: &RunResult) -> String {
    let mut out = String::from(
        "sleeve,asset,entry_time,exit_time,entry_price,exit_price,qty,cost_usdt,proceeds_usdt,fees_usdt,net_pnl_usdt,reason,open_at_end\n",
    );
    for t in &r.trades {
        let _ = writeln!(
            out,
            "{},{},{},{},{:.10},{:.10},{:.10},{:.10},{:.10},{:.10},{:.10},{},{}",
            t.sleeve,
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
    out
}

/// The Markdown report.
pub fn markdown(r: &RunResult, ctx: &ReportContext) -> String {
    let cfg = &ctx.cfg;
    let round_trip_bps = 2.0 * (ctx.fee_bps + ctx.slippage_bps);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Algorithm backtest: `{}` ({} → {})\n",
        ctx.rule.rules_version,
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
        r.steps
    );
    let _ = writeln!(
        out,
        "- Starts with {:.2} USDT. Costs per side: fee {} bps + slippage {} bps ({} bps per round trip).",
        cfg.starting_balance, ctx.fee_bps, ctx.slippage_bps, round_trip_bps
    );
    let _ = writeln!(
        out,
        "- Rule file `{}`: `rules_version` {}, style {}, sha256 `{}`.",
        ctx.rule_path,
        ctx.rule.rules_version,
        serde_json::to_value(ctx.rule.style)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        ctx.rule.sha256
    );
    for sleeve in &ctx.rule.sleeves {
        let _ = writeln!(
            out,
            "  - `{}`: share {}, horizon {}, defensive {}, risk-off {}; {sleeve}.",
            sleeve.name,
            sleeve.share,
            serde_json::to_value(sleeve.horizon)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            sleeve.defensive.join("/"),
            sleeve
                .risk_off_below
                .map_or_else(|| "none".to_owned(), |l| format!("below {l:+}")),
        );
    }
    let _ = writeln!(out, "- Tags feeds: {}.", ctx.tags);
    if let Some(boards) = &ctx.scoreboards {
        let _ = writeln!(out, "- Scoreboards (walk-forward): {boards}.");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Results\n");
    let _ = writeln!(
        out,
        "| Scope | Net USDT | Net % | Max drawdown % | Round trips | Win rate % | Sharpe | Fees USDT | Open at end | Skipped orders |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let open = r.trades.iter().filter(|t| t.open_at_end).count();
    let _ = writeln!(
        out,
        "| portfolio | {:.2} | {:.2} | {:.2} | {} | {:.1} | {:.3} | {:.2} | {} | {} |",
        r.net_pnl_usdt,
        r.net_pnl_pct,
        r.max_drawdown_pct,
        r.round_trips,
        r.win_rate_pct,
        r.sharpe,
        r.fees_usdt,
        open,
        r.skipped_orders
    );
    for s in &r.sleeves {
        let _ = writeln!(
            out,
            "| sleeve `{}` | {:.2} | {:.2} | — | {} | {:.1} | — | {:.2} | {} | {} |",
            s.name,
            s.net_pnl_usdt,
            s.net_pnl_usdt / r.starting_balance * 100.0,
            s.round_trips,
            s.win_rate_pct,
            s.fees_usdt,
            s.open,
            s.skipped_orders
        );
    }
    let _ = writeln!(
        out,
        "\nNet = ending equity − starting balance after every fee and slippage; positions still open at the end are marked at their last close. A sleeve's net is the sum of its trades (its % is of the starting balance); the sleeves share the USDT, so drawdown and Sharpe are the portfolio's only. Win rate counts closed round trips with a positive net. Sharpe = mean / population std of step-equity returns × √(525600 / step minutes). Full numbers: the `.csv`; every trade: `_trades.csv`.\n"
    );
    let _ = writeln!(out, "## Details\n");
    let skips = if r.skip_reasons.is_empty() {
        "none".to_string()
    } else {
        r.skip_reasons
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(
        out,
        "- Ending equity {:.2} USDT; {} closed round trips ({} wins), {} still open at the end.",
        r.final_equity, r.round_trips, r.wins, open
    );
    let _ = writeln!(out, "- Skipped orders: {skips}.");
    for note in &r.notes {
        let _ = writeln!(out, "- {note}");
    }
    for (sleeve, trace) in &r.traces {
        let _ = writeln!(
            out,
            "- Sleeve `{sleeve}` per-step trace: `{}` ({} rows, one per step it decided: `{TRACE_HEADER}`).",
            trace_file_name(&ctx.stem, sleeve),
            trace.rows.len(),
        );
    }
    let _ = writeln!(out, "\n## Assumptions\n");
    for line in [
        "Decisions see only bars closed by the step time and the newest tags feed with `as_of` at or before it (none when older than the age limit); intents fill at the open of the 1m bar that opens at the step, sells before buys.".to_string(),
        match ctx.candidates {
            CandidateSet::Tags => "Candidates, as live: the cached coins the visible feed's live asset tags reach, directly or through the proxy file; held coins are always scored. A coin without a cached market is scored-only and never bought.",
            CandidateSet::Liquid => "Candidates (`--candidates liquid`, not as live): the liquid 20 priced at the step, whatever the tags reach; none without a visible feed, as live; held coins are always scored.",
        }
        .to_string(),
        "Sleeves: each sleeve with a share or a holding decides on its own holdings; a sell sells exactly its quantity, a buy spends its share of the portfolio value at the step (cash plus every sleeve's holdings), capped by the cash left.".to_string(),
        format!("The {MIN_NOTIONAL_USDT} USDT minimum applies to buys only; a held position can always be sold."),
        "A buy with no cash left is counted as `no_cash`.".to_string(),
        "Equity, drawdown and positions still open at the end are marked at the last close without exit costs.".to_string(),
    ] {
        let _ = writeln!(out, "- {line}");
    }
    let _ = writeln!(out, "\n## Caveats\n");
    for line in [
        "Survivorship: the candidates are limited to the coins in the candle cache; the scored-only note above lists the live asset tags that had none.".to_string(),
        format!(
            "Costs: {round_trip_bps} bps per round trip, against the measured {MEASURED_ROUND_TRIP_BPS} bps of the 40 live trips (deliberately pessimistic)."
        ),
        "No tuning: the rule file is run as it is; a rule picked after looking at this window is in-sample here.".to_string(),
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

/// File name of a sleeve's per-step trace: `<stem>_<sleeve>_trace.csv`.
pub fn trace_file_name(stem: &str, sleeve: &str) -> String {
    format!("{stem}_{sleeve}_trace.csv")
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
/// `_<sleeve>_trace.csv` per sleeve that decided).
pub fn run_backtest(args: &AlgoBacktestArgs) -> Result<(RunResult, Vec<PathBuf>)> {
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
    let rule = Rule::load(&args.rule).map_err(TraderError::config)?;
    let boards = match (&rule.edge, &args.scoreboards) {
        (Some(_), Some(dir)) => load_scoreboards(dir)?,
        (Some(_), None) => {
            return Err(TraderError::config(
                "an edge rule needs --scoreboards (point-in-time scoreboards, `bb tag-scoreboard --as-of`)",
            ));
        }
        (None, Some(_)) => {
            return Err(TraderError::config(
                "--scoreboards is read only for an edge rule; this rule's style is fixed",
            ));
        }
        (None, None) => Vec::new(),
    };
    let proxies = Proxies::load(&args.proxies).map_err(TraderError::config)?;
    let market = Market::load_dir(&args.cache_dir)?;
    check_coverage(&market, start_ms, end_ms)?;
    let max_age_ms = args.tags_max_age_minutes * MINUTE_MS;
    let tags = TagsHistory::load_dir(
        &args.tags_history,
        args.tags_max_age_minutes,
        (start_ms - max_age_ms, end_ms),
        &proxies,
        |a| market.get(a).is_some(),
    )?;
    let ctx = ReportContext {
        cfg,
        fee_bps: args.fee_bps,
        slippage_bps: args.slippage_bps,
        cache_dir: args.cache_dir.display().to_string(),
        coins: market.len(),
        stem: file_stem(end_ms),
        rule_path: args.rule.display().to_string(),
        rule: rule.clone(),
        tags: tags_summary(&tags, &args.tags_history, &args.proxies),
        scoreboards: args
            .scoreboards
            .as_deref()
            .map(|dir| scoreboards_summary(&boards, dir)),
        candidates: args.candidates,
    };
    let step = RuleFile::new(rule, Arc::new(tags), Arc::new(boards), args.candidates);
    let result = run(&market, &cfg, Box::new(step))?;
    std::fs::create_dir_all(&args.output_dir)?;
    let stem = &ctx.stem;
    let mut paths = vec![
        args.output_dir.join(format!("{stem}.md")),
        args.output_dir.join(format!("{stem}.csv")),
        args.output_dir.join(format!("{stem}_trades.csv")),
    ];
    write(&paths[0], &markdown(&result, &ctx))?;
    write(&paths[1], &summary_csv(&result))?;
    write(&paths[2], &trades_csv(&result))?;
    for (sleeve, trace) in &result.traces {
        let path = args.output_dir.join(trace_file_name(stem, sleeve));
        write(&path, &trace_csv(trace))?;
        paths.push(path);
    }
    Ok((result, paths))
}

fn write(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body)?;
    Ok(())
}

/// `bb algo-backtest`.
pub fn run_cli(args: &AlgoBacktestArgs) -> Result<ExitCode> {
    let (result, paths) = run_backtest(args)?;
    print!("{}", summary_csv(&result));
    for path in paths {
        println!("wrote {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_END: i64 = 1_784_448_000_000 + 4 * DAY_MS; // 2026-07-23T08:00Z
    const H: i64 = 3_600_000;

    /// Writes 9 days of 1m bars for every liquid coin, the gold proxies and INJ, each with
    /// its own drift and wiggle, ending at `T_END` (a 1-day window needs 7 days of warm-up).
    fn cache_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let start = T_END - 9 * DAY_MS;
        let mut coins: Vec<&str> = LIQUID.iter().chain(&COVERAGE).copied().collect();
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

    /// Hourly tags feeds over the window and the day before it: SOL and ETH (bullish 6h
    /// stances), NVDA (no market) and gold.
    fn tags_history() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for hour in 0..48_i64 {
            let as_of = T_END - 2 * DAY_MS + hour * H;
            let m6 = 2 + hour % 7;
            let asset = |name: &str| {
                format!(
                    r#""asset:{name}":{{"kind":"asset","name":"{name}","windows":{{"6h":{{"mentions":{m6},"posters":3,"up":2.0,"down":{}}},"24h":{{"mentions":12,"posters":5,"up":3.0,"down":1.0}}}}}}"#,
                    hour % 3
                )
            };
            let body = format!(
                r#"{{"schema":"tags-feed-v1","as_of":"{}","sources":[{{"id":"biz","model":"m","prompt_version":"p"}}],"tags":{{{},{},{},{}}}}}"#,
                iso(as_of),
                asset("SOL"),
                asset("ETH"),
                asset("NVDA"),
                asset("GOLD"),
            );
            std::fs::write(dir.path().join(format!("{hour:02}.json")), body).unwrap();
        }
        dir
    }

    struct Fixture {
        cache: tempfile::TempDir,
        feeds: tempfile::TempDir,
        files: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let files = tempfile::tempdir().unwrap();
        std::fs::write(
            files.path().join("proxies.json"),
            r#"{"GOLD": ["PAXG", "XAUT"]}"#,
        )
        .unwrap();
        Fixture {
            cache: cache_dir(),
            feeds: tags_history(),
            files,
        }
    }

    fn args(f: &Fixture, rule: &Path, out: &Path) -> AlgoBacktestArgs {
        AlgoBacktestArgs {
            rule: rule.to_path_buf(),
            tags_history: f.feeds.path().to_path_buf(),
            proxies: f.files.path().join("proxies.json"),
            scoreboards: None,
            cache_dir: f.cache.path().to_path_buf(),
            end: "2026-07-23T08:00:00Z".into(),
            days: 1,
            starting_balance: 1000.0,
            fee_bps: 7.5,
            slippage_bps: 5.0,
            step_minutes: 20,
            tags_max_age_minutes: 90,
            output_dir: out.to_path_buf(),
            candidates: CandidateSet::Tags,
        }
    }

    fn shipped() -> PathBuf {
        PathBuf::from(crate::rules::tests::SHIPPED)
    }

    /// A rule file with `sleeves` (JSON values) written into `dir`.
    fn rule_file(dir: &Path, name: &str, sleeves: serde_json::Value) -> PathBuf {
        let path = dir.join(format!("{name}.json"));
        let text = serde_json::json!({"rules_version": name, "style": "fixed", "sleeves": sleeves});
        std::fs::write(&path, text.to_string()).unwrap();
        path
    }

    fn sleeve(
        name: &str,
        share: f64,
        buy: f64,
        sell: f64,
        weights: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({"name": name, "share": share, "horizon": "any", "defensive": ["USDT"],
            "risk_off_below": null, "buy_above": buy, "sell_below": sell, "min_posters": 2,
            "weights": weights})
    }

    fn trace_rows(path: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .skip(1)
            .map(|l| l.split(',').map(str::to_owned).collect())
            .collect()
    }

    #[test]
    fn the_shipped_rule_runs_its_funded_sleeve_on_the_tags_history() {
        let f = fixture();
        let out = tempfile::tempdir().unwrap();
        let (result, paths) = run_backtest(&args(&f, &shipped(), out.path())).unwrap();
        assert_eq!(result.name, "algo-2026-10-29");
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        for needle in [
            "`rules_version` algo-2026-10-29, style fixed, sha256",
            "25 feeds from 2026-07-22T07:00:00Z",
            "sleeve `fast` (fast:",
            "sleeve `core` (core:",
            "never active",
        ] {
            assert!(md.contains(needle), "missing {needle:?}: {md}");
        }
        // Only fast decides (core has no share and holds nothing): one trace.
        assert_eq!(
            result
                .traces
                .iter()
                .map(|(s, _)| s.as_str())
                .collect::<Vec<_>>(),
            ["fast"]
        );
        let trace = out.path().join("algo_backtest_2026-07-23_fast_trace.csv");
        assert!(paths.contains(&trace));
        let rows = trace_rows(&trace);
        assert_eq!(rows.len(), result.steps);
        for row in &rows {
            // Every step sees a feed, never one from after the step; the candidates are the
            // fed coins with a market (SOL, ETH) and gold's two proxies.
            assert!(!row[1].is_empty() && row[1] <= row[0], "{row:?}");
            assert_eq!(row[2], "4", "{row:?}");
        }
        // Only the fed coins are ever bought.
        assert!(!result.trades.is_empty());
        assert!(
            result.trades.iter().all(|t| t.sleeve == "fast"
                && ["SOL", "ETH", "PAXG", "XAUT"].contains(&t.asset.as_str()))
        );
        assert!(
            md.contains("scored-only asset tags") && md.contains("NVDA"),
            "{md}"
        );
    }

    #[test]
    fn two_sleeves_trade_side_by_side_with_their_own_shares() {
        let f = fixture();
        let out = tempfile::tempdir().unwrap();
        // fast trades on 1h momentum, core buys and holds the best 24h stance.
        let rule = rule_file(
            f.files.path(),
            "two-sleeves",
            serde_json::json!([
                sleeve("fast", 0.6, 0.2, 0.0, serde_json::json!({"momentum.1h": 1})),
                sleeve(
                    "core",
                    0.4,
                    -10.0,
                    -20.0,
                    serde_json::json!({"asset.direction.24h": 1})
                ),
            ]),
        );
        let (result, paths) = run_backtest(&args(&f, &rule, out.path())).unwrap();
        let names: Vec<&str> = result.sleeves.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["core", "fast"]);
        let core: Vec<_> = result
            .trades
            .iter()
            .filter(|t| t.sleeve == "core")
            .collect();
        assert_eq!(core.len(), 1, "core buys once and holds: {core:?}");
        assert!(core[0].open_at_end);
        // core's buy spent 40% of the portfolio at its step (the first step: 400 USDT).
        assert_eq!(core[0].entry_ms, result.trades[0].entry_ms);
        assert!(
            (core[0].cost_usdt - 400.0).abs() < 1e-6,
            "{}",
            core[0].cost_usdt
        );
        let fast = result.trades.iter().filter(|t| t.sleeve == "fast").count();
        assert!(fast >= 2, "{fast}");
        // Per-sleeve rows and traces.
        let csv = std::fs::read_to_string(&paths[1]).unwrap();
        let scopes: Vec<&str> = csv
            .lines()
            .skip(1)
            .map(|l| l.split(',').next().unwrap())
            .collect();
        assert_eq!(scopes, ["portfolio", "sleeve:core", "sleeve:fast"]);
        let sum: f64 = result.sleeves.iter().map(|s| s.net_pnl_usdt).sum();
        assert!(
            (sum - result.net_pnl_usdt).abs() < 1e-6,
            "{sum} vs {}",
            result.net_pnl_usdt
        );
        for sleeve in ["fast", "core"] {
            let rows = trace_rows(
                &out.path()
                    .join(format!("algo_backtest_2026-07-23_{sleeve}_trace.csv")),
            );
            assert_eq!(rows.len(), result.steps, "{sleeve}");
        }
        let trades = std::fs::read_to_string(&paths[2]).unwrap();
        assert!(trades.lines().skip(1).any(|l| l.starts_with("core,")));
    }

    #[test]
    fn the_liquid_candidate_set_replaces_the_tag_pick() {
        let f = fixture();
        let rule = rule_file(
            f.files.path(),
            "momentum",
            serde_json::json!([sleeve(
                "fast",
                1.0,
                0.2,
                0.0,
                serde_json::json!({"momentum.1h": 1})
            )]),
        );
        let run_with = |candidates: CandidateSet| {
            let out = tempfile::tempdir().unwrap();
            let mut a = args(&f, &rule, out.path());
            a.candidates = candidates;
            let (result, paths) = run_backtest(&a).unwrap();
            let rows = trace_rows(&out.path().join("algo_backtest_2026-07-23_fast_trace.csv"));
            let md = std::fs::read_to_string(&paths[0]).unwrap();
            (result, rows, md)
        };
        // As live: the fed coins with a market (SOL, ETH) and gold's proxies.
        let (tags, rows, md) = run_with(CandidateSet::Tags);
        assert!(rows.iter().all(|r| r[2] == "4"), "{rows:?}");
        assert!(md.contains("Candidates, as live"), "{md}");
        assert!(
            tags.trades
                .iter()
                .all(|t| ["SOL", "ETH", "PAXG", "XAUT"].contains(&t.asset.as_str()))
        );
        // Liquid: the 20 liquid coins whatever the feed names; never the gold proxies.
        let (liquid, rows, md) = run_with(CandidateSet::Liquid);
        assert!(rows.iter().all(|r| r[2] == "20"), "{rows:?}");
        assert!(md.contains("`--candidates liquid`, not as live"), "{md}");
        assert!(!liquid.trades.is_empty());
        assert!(
            liquid
                .trades
                .iter()
                .all(|t| LIQUID.contains(&t.asset.as_str()))
        );
        assert!(
            liquid
                .trades
                .iter()
                .any(|t| !["SOL", "ETH"].contains(&t.asset.as_str()))
        );
    }

    #[test]
    fn an_edge_rule_walks_forward_on_the_scoreboards_available_at_each_step() {
        let f = fixture();
        let out = tempfile::tempdir().unwrap();
        let boards = tempfile::tempdir().unwrap();
        let start = T_END - DAY_MS;
        // Two snapshots: outcomes available 6h and 12h into the window.
        let (early, late) = (start + 6 * H, start + 12 * H);
        for (name, at) in [("a.json", early), ("b.json", late)] {
            std::fs::write(
                boards.path().join(name),
                crate::edge::tests::fixture_available_at(at),
            )
            .unwrap();
        }
        let rule = f.files.path().join("edge.json");
        std::fs::write(
            &rule,
            crate::edge::tests::edge_rule_text("unused-in-backtests", "24h"),
        )
        .unwrap();
        let mut a = args(&f, &rule, out.path());
        let err = run_backtest(&a).unwrap_err().to_string();
        assert!(err.contains("an edge rule needs --scoreboards"), "{err}");
        a.scoreboards = Some(boards.path().to_path_buf());
        let (result, paths) = run_backtest(&a).unwrap();
        let rows = trace_rows(&out.path().join("algo_backtest_2026-07-23_fast_trace.csv"));
        assert_eq!(rows.len(), result.steps);
        for row in &rows {
            let (t, board, action) = (&row[0], &row[3], &row[10]);
            if *t < iso(early) {
                assert!(
                    board.is_empty() && action == "hold (no evidence)",
                    "{row:?}"
                );
            } else {
                let want = if *t < iso(late) {
                    iso(early)
                } else {
                    iso(late)
                };
                assert_eq!(board, &want, "{row:?}");
                assert!(board <= t && action != "hold (no evidence)", "{row:?}");
            }
        }
        assert!(result.trades.iter().all(|t| t.entry_ms >= early));
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        assert!(
            md.contains("2 point-in-time scoreboards")
                && md.contains("held without a usable scoreboard at 18"),
            "{md}"
        );
        // A fixed rule refuses scoreboards.
        let mut fixed = args(&f, &shipped(), out.path());
        fixed.scoreboards = Some(boards.path().to_path_buf());
        assert!(
            run_backtest(&fixed)
                .unwrap_err()
                .to_string()
                .contains("only for an edge rule")
        );
    }

    #[test]
    fn writes_every_file_deterministically() {
        let f = fixture();
        let out = tempfile::tempdir().unwrap();
        let (result, paths) = run_backtest(&args(&f, &shipped(), out.path())).unwrap();
        assert!(paths[0].ends_with("algo_backtest_2026-07-23.md"));
        let csv = std::fs::read_to_string(&paths[1]).unwrap();
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.split(',').count() == 10));
        let md = std::fs::read_to_string(&paths[0]).unwrap();
        for needle in ["Survivorship", "25 bps per round trip", "No tuning"] {
            assert!(md.contains(needle), "missing {needle:?}");
        }
        let trades = std::fs::read_to_string(&paths[2]).unwrap();
        assert_eq!(trades.lines().count(), result.trades.len() + 1);

        let again = tempfile::tempdir().unwrap();
        run_backtest(&args(&f, &shipped(), again.path())).unwrap();
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
        let f = fixture();
        let out = tempfile::tempdir().unwrap();
        let empty = Fixture {
            cache: tempfile::tempdir().unwrap(),
            feeds: tags_history(),
            files: tempfile::tempdir().unwrap(),
        };
        std::fs::write(empty.files.path().join("proxies.json"), "{}").unwrap();
        let err = run_backtest(&args(&empty, &shipped(), out.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing cache"), "{err}");

        for coin in ["SOL", "XAUT"] {
            let f = fixture();
            std::fs::remove_file(f.cache.path().join(format!("1m_{coin}_USDT_1_2.json"))).unwrap();
            let err = run_backtest(&args(&f, &shipped(), out.path()))
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("missing cache for {coin}")), "{err}");
        }

        let mut long = args(&f, &shipped(), out.path());
        long.days = 3; // needs bars from 2026-07-13T08:00Z, the cache starts 07-14
        let err = run_backtest(&long).unwrap_err().to_string();
        assert!(
            err.contains("missing cache: no liquid coin has bars from"),
            "{err}"
        );

        let mut late = args(&f, &shipped(), out.path());
        late.end = "2026-07-24T08:00:00Z".into();
        let err = run_backtest(&late).unwrap_err().to_string();
        assert!(err.contains("end before the window end"), "{err}");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    }
}

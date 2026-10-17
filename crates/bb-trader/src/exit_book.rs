//! Exit-plan book (Phase 45): the plans Claude sets with the `set_exit_plan` host tool,
//! the default plan, the run settings, and the watcher's view of them.
//!
//! Files (each has one writer): the plan book (`CLAUDE_SPOT_EXIT_PLANS_PATH`) is written
//! only by the trader's host tool; the watch state (`CLAUDE_SPOT_EXIT_WATCH_STATE_PATH`)
//! and the watch log (`CLAUDE_SPOT_EXIT_WATCH_LOG_PATH`) by the paper watcher
//! `claude-spot-exit-watch`, or by the trader's exit guard when exit plans run live
//! (never both). Nothing here places orders.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::exit_plan::{AtrTimeframe, ExitPlan, Fade, Slowdown, Stretch, TakePart, Trail};
use crate::memory::value_decimal;

pub const DEFAULT_EXIT_PLANS_PATH: &str = "data/claude-spot-exit-plans.json";
pub const DEFAULT_EXIT_WATCH_STATE_PATH: &str = "data/claude-spot-exit-watch-state.json";
pub const DEFAULT_EXIT_WATCH_LOG_PATH: &str = "data/claude-spot-exit-watch.jsonl";

/// The three exit-plan files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitPaths {
    pub plans: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
}

impl ExitPaths {
    /// `CLAUDE_SPOT_EXIT_{PLANS,WATCH_STATE,WATCH_LOG}_PATH` from `lookup` (blank = default).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let path = |name: &str, default: &str| {
            PathBuf::from(
                lookup(name)
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| default.to_owned()),
            )
        };
        Self {
            plans: path("CLAUDE_SPOT_EXIT_PLANS_PATH", DEFAULT_EXIT_PLANS_PATH),
            state: path(
                "CLAUDE_SPOT_EXIT_WATCH_STATE_PATH",
                DEFAULT_EXIT_WATCH_STATE_PATH,
            ),
            log: path(
                "CLAUDE_SPOT_EXIT_WATCH_LOG_PATH",
                DEFAULT_EXIT_WATCH_LOG_PATH,
            ),
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
}

/// Plan applied to every position Claude opens at or after `from_ms` unless Claude set one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultPlan {
    pub plan: ExitPlan,
    pub from_ms: i64,
}

/// How exit plans run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitSettings {
    /// `CLAUDE_SPOT_EXIT_PLANS_LIVE`: the trader sells when a plan fires (else paper).
    pub live: bool,
    /// `CLAUDE_SPOT_DEFAULT_EXIT_PLAN` (spec, blank = none) from
    /// `CLAUDE_SPOT_DEFAULT_EXIT_PLAN_FROM` (RFC 3339, required with a plan).
    pub default_plan: Option<DefaultPlan>,
    /// `CLAUDE_SPOT_EXIT_POLL_SECONDS`: bid checks between cycles (live mode).
    pub poll_seconds: u64,
}

impl ExitSettings {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let live = match get("CLAUDE_SPOT_EXIT_PLANS_LIVE").map(|v| v.to_lowercase()) {
            None => false,
            Some(v) if ["1", "true", "yes", "on"].contains(&v.as_str()) => true,
            Some(v) if ["0", "false", "no", "off"].contains(&v.as_str()) => false,
            Some(v) => {
                return Err(format!(
                    "CLAUDE_SPOT_EXIT_PLANS_LIVE must be true or false, got {v:?}"
                ));
            }
        };
        let default_plan = match get("CLAUDE_SPOT_DEFAULT_EXIT_PLAN") {
            None => None,
            Some(spec) => {
                let plan = ExitPlan::parse_spec(&spec)
                    .map_err(|e| format!("CLAUDE_SPOT_DEFAULT_EXIT_PLAN {spec:?}: {e}"))?;
                let from = get("CLAUDE_SPOT_DEFAULT_EXIT_PLAN_FROM").ok_or(
                    "CLAUDE_SPOT_DEFAULT_EXIT_PLAN_FROM (RFC 3339) is required with a default exit plan",
                )?;
                let from_ms = DateTime::parse_from_rfc3339(&from)
                    .map_err(|e| format!("CLAUDE_SPOT_DEFAULT_EXIT_PLAN_FROM {from:?}: {e}"))?
                    .timestamp_millis();
                (plan != ExitPlan::default()).then_some(DefaultPlan { plan, from_ms })
            }
        };
        let poll_seconds = match get("CLAUDE_SPOT_EXIT_POLL_SECONDS") {
            None => 5,
            Some(v) => v.parse::<u64>().ok().filter(|s| *s >= 1).ok_or_else(|| {
                format!("CLAUDE_SPOT_EXIT_POLL_SECONDS must be a whole number >= 1, got {v:?}")
            })?,
        };
        Ok(Self {
            live,
            default_plan,
            poll_seconds,
        })
    }
}

/// One plan Claude set for one open position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub asset: String,
    /// Entry market, e.g. `ENA/USDT`.
    pub symbol: String,
    pub plan: ExitPlan,
    pub spec: String,
    pub entry_ms: i64,
    pub entry_price: Decimal,
    pub set_at_ms: i64,
    pub cycle_id: String,
}

impl PlanEntry {
    /// Identity of one trial: asset, position and the moment the plan was set.
    pub fn key(&self) -> String {
        format!("{}@{}@{}", self.asset, self.entry_ms, self.set_at_ms)
    }
}

/// Plans by asset (one per asset; setting again replaces).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanBook {
    pub plans: BTreeMap<String, PlanEntry>,
}

impl PlanBook {
    /// A missing file is an empty book.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        bb_core::fsutil::write_json_atomic(path, self)
    }
}

/// `set_exit_plan` arguments. Numbers arrive as JSON values and are read as decimals.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetExitPlanArgs {
    pub asset: String,
    #[serde(default)]
    pub clear: bool,
    pub trail_pct: Option<Value>,
    pub atr_multiple: Option<Value>,
    pub atr_timeframe: Option<String>,
    pub take_part_gain_pct: Option<Value>,
    pub take_part_fraction: Option<Value>,
    pub stretch_z: Option<Value>,
    pub stretch_fraction: Option<Value>,
    pub slowdown_fraction: Option<Value>,
    pub slowdown_minutes: Option<u32>,
    pub fade_min_gain_pct: Option<Value>,
    pub fade_minutes: Option<u32>,
    pub deadline_minutes: Option<u32>,
}

fn number(value: Option<&Value>, name: &str) -> Result<Option<Decimal>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(v) => value_decimal(Some(v))
            .map(Some)
            .ok_or_else(|| format!("{name} must be a number")),
    }
}

/// Both halves of a two-field rule, or neither.
fn both<A, B>(a: Option<A>, b: Option<B>, names: &str) -> Result<Option<(A, B)>, String> {
    match (a, b) {
        (Some(a), Some(b)) => Ok(Some((a, b))),
        (None, None) => Ok(None),
        _ => Err(format!("{names} go together")),
    }
}

impl SetExitPlanArgs {
    /// The menu plan these arguments describe.
    pub fn plan(&self) -> Result<ExitPlan, String> {
        let trail = number(self.trail_pct.as_ref(), "trail_pct")?;
        let atr = both(
            number(self.atr_multiple.as_ref(), "atr_multiple")?,
            self.atr_timeframe.as_deref(),
            "atr_multiple and atr_timeframe",
        )?;
        let plan = ExitPlan {
            trail: match (trail, atr) {
                (Some(_), Some(_)) => {
                    return Err("set trail_pct or atr_multiple, not both".into());
                }
                (Some(pct), None) => Some(Trail::Pct(pct)),
                (None, Some((multiple, timeframe))) => Some(Trail::Atr {
                    multiple,
                    timeframe: AtrTimeframe::parse(timeframe)?,
                }),
                (None, None) => None,
            },
            take_part: both(
                number(self.take_part_gain_pct.as_ref(), "take_part_gain_pct")?,
                number(self.take_part_fraction.as_ref(), "take_part_fraction")?,
                "take_part_gain_pct and take_part_fraction",
            )?
            .map(|(gain_pct, fraction)| TakePart { gain_pct, fraction }),
            stretch: both(
                number(self.stretch_z.as_ref(), "stretch_z")?,
                number(self.stretch_fraction.as_ref(), "stretch_fraction")?,
                "stretch_z and stretch_fraction",
            )?
            .map(|(z, fraction)| Stretch { z, fraction }),
            slowdown: both(
                number(self.slowdown_fraction.as_ref(), "slowdown_fraction")?,
                self.slowdown_minutes,
                "slowdown_fraction and slowdown_minutes",
            )?
            .map(|(fraction, minutes)| Slowdown { fraction, minutes }),
            fade: both(
                number(self.fade_min_gain_pct.as_ref(), "fade_min_gain_pct")?,
                self.fade_minutes,
                "fade_min_gain_pct and fade_minutes",
            )?
            .map(|(min_gain_pct, minutes)| Fade {
                min_gain_pct,
                minutes,
            }),
            deadline_minutes: self.deadline_minutes,
        };
        if plan == ExitPlan::default() {
            return Err("set at least one rule, or clear: true to remove the plan".into());
        }
        plan.validate()?;
        Ok(plan)
    }
}

/// One simulated sell, as stored in the watch state and log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillView {
    pub ts_ms: i64,
    pub price: Decimal,
    pub fraction: Decimal,
    pub reason: String,
}

/// The watcher's view of one running trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchView {
    pub key: String,
    pub spec: String,
    pub symbol: String,
    pub entry_price: Decimal,
    pub trail_pct: Option<Decimal>,
    pub peak: Decimal,
    pub sell_line: Option<Decimal>,
    pub remaining: Decimal,
    pub last_bid: Option<Decimal>,
    pub last_ms: i64,
    pub fills: Vec<FillView>,
}

/// Closed positions with a plan kept in the watch state for the cycle prompt.
pub const RECENT_CLOSED: usize = 5;

/// One position with a plan, after it closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClosedView {
    pub asset: String,
    pub spec: String,
    pub entry_ms: i64,
    pub entry_price: Decimal,
    pub exit_ms: i64,
    /// Price of the sale that closed the position.
    pub exit_price: Decimal,
    /// The plan's sells before the close, with the rule that fired (live: real sells at
    /// the trigger price; paper: would-be sells).
    pub plan_fills: Vec<FillView>,
    /// Fee per side used for the returns.
    pub fee_rate: Decimal,
    /// Net return after fees with the plan's sells, in percent.
    pub plan_net_pct: Decimal,
    /// Net return after fees of the closing sale alone, in percent.
    pub exit_net_pct: Decimal,
}

/// Watch state written by `claude-spot-exit-watch` every loop.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchState {
    pub updated_at_ms: i64,
    /// Running trials by asset.
    pub active: BTreeMap<String, WatchView>,
    /// Keys of finished trials (closed by Claude's sell, cleared, replaced or orphaned).
    pub finalized: BTreeSet<String>,
    /// Trials closed by Claude's real sell.
    pub closed_trials: usize,
    /// Sum over closed trials of (with plan - Claude alone), in USDT.
    pub plan_vs_claude_usdt: Decimal,
    /// The newest closed trials (at most [`RECENT_CLOSED`]), oldest first.
    #[serde(default)]
    pub recent_closed: Vec<ClosedView>,
}

impl WatchState {
    /// A missing or unreadable file is an empty state.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        bb_core::fsutil::write_json_atomic(path, self)
    }
}

fn hhmm(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn price(value: Decimal) -> String {
    value.round_dp(8).normalize().to_string()
}

fn plan_line(view: &WatchView, live: bool) -> String {
    let mut parts = vec![format!("peak {}", price(view.peak))];
    if let Some(line) = view.sell_line {
        parts.push(format!("sell line {}", price(line)));
    }
    if let Some(bid) = view.last_bid {
        parts.push(format!("last bid {}", price(bid)));
    }
    for fill in &view.fills {
        parts.push(format!(
            "{} {}% at {} ({}, {})",
            if live {
                "plan fired: sell"
            } else {
                "would have sold"
            },
            (fill.fraction * Decimal::ONE_HUNDRED)
                .round_dp(1)
                .normalize(),
            price(fill.price),
            fill.reason,
            hhmm(fill.ts_ms)
        ));
    }
    parts.join(", ")
}

fn signed_pct(value: Decimal) -> String {
    let value = value.round_dp(2).normalize();
    if value > Decimal::ZERO {
        format!("+{value}%")
    } else {
        format!("{value}%")
    }
}

/// One closed position: which rule sold what at which price, and the result after fees.
fn closed_line(view: &ClosedView, live: bool) -> String {
    let sells: Vec<String> = view
        .plan_fills
        .iter()
        .map(|fill| {
            format!(
                "{} {}% at {} by {} ({})",
                if live { "plan sold" } else { "plan would have sold" },
                (fill.fraction * Decimal::ONE_HUNDRED)
                    .round_dp(1)
                    .normalize(),
                price(fill.price),
                fill.reason,
                hhmm(fill.ts_ms)
            )
        })
        .collect();
    let sells = if sells.is_empty() {
        "no plan rule fired".to_owned()
    } else {
        sells.join(", ")
    };
    let fees = format!(
        "fees {}% per side",
        (view.fee_rate * Decimal::ONE_HUNDRED).round_dp(4).normalize()
    );
    let result = if live {
        format!("net after fees {} ({fees})", signed_pct(view.plan_net_pct))
    } else {
        format!(
            "net after fees: your exit {}, with the plan {} ({fees})",
            signed_pct(view.exit_net_pct),
            signed_pct(view.plan_net_pct)
        )
    };
    format!(
        "- closed {} {}: bought at {} ({}); {}; position closed at {} ({}, after {} min); {result}",
        view.asset,
        view.spec,
        price(view.entry_price),
        hhmm(view.entry_ms),
        sells,
        price(view.exit_price),
        hhmm(view.exit_ms),
        (view.exit_ms - view.entry_ms) / 60_000
    )
}

/// Cycle-prompt section on exit plans; empty when there is nothing to show. Lists every
/// running plan (Claude's or the default), Claude's plans not picked up yet, and the
/// newest closed positions with the rule that sold and the result after fees.
/// `watcher_stale_ms` marks the watcher as not running when its state is older.
pub fn prompt_section(paths: &ExitPaths, now_ms: i64, watcher_stale_ms: i64, live: bool) -> String {
    let book = PlanBook::load(&paths.plans).unwrap_or_default();
    let state = WatchState::load(&paths.state);
    let waiting: Vec<&PlanEntry> = book
        .plans
        .values()
        .filter(|p| !state.finalized.contains(&p.key()))
        .filter(|p| state.active.get(&p.asset).is_none_or(|v| v.key != p.key()))
        .collect();
    if state.active.is_empty() && waiting.is_empty() && state.closed_trials == 0 {
        return String::new();
    }
    let mut out = String::from(if live {
        "\n## Exit plans (LIVE: the trader sells a position when its plan fires)\n"
    } else {
        "\n## Exit plans (paper trial: the watcher records what each plan would sell; it sells nothing)\n"
    });
    if now_ms - state.updated_at_ms > watcher_stale_ms {
        out.push_str("- watcher not running: plans are stored but not watched\n");
    }
    for (asset, view) in &state.active {
        let _ = writeln!(
            out,
            "- {asset} {} (entry {}): {}",
            view.spec,
            price(view.entry_price),
            plan_line(view, live)
        );
    }
    for plan in waiting {
        let _ = writeln!(
            out,
            "- {} {} (entry {}, set {}): waiting for the watcher",
            plan.asset,
            plan.spec,
            price(plan.entry_price),
            hhmm(plan.set_at_ms)
        );
    }
    for view in state.recent_closed.iter().rev() {
        let _ = writeln!(out, "{}", closed_line(view, live));
    }
    if state.closed_trials > 0 {
        let _ = writeln!(
            out,
            "- closed positions with a plan: {}; plan vs your own exits: {} USDT",
            state.closed_trials,
            state.plan_vs_claude_usdt.round_dp(4).normalize()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    fn args(value: Value) -> SetExitPlanArgs {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn tool_arguments_build_menu_plans() {
        let plan = args(json!({
            "asset": "ENA",
            "atr_multiple": 3,
            "atr_timeframe": "15m",
            "take_part_gain_pct": 3,
            "take_part_fraction": 0.5,
            "stretch_z": 2,
            "stretch_fraction": 0.5,
            "slowdown_fraction": 1,
            "slowdown_minutes": 5,
            "deadline_minutes": 240
        }))
        .plan()
        .unwrap();
        assert_eq!(
            plan.to_string(),
            "atr=3:15m,part=3:0.5,stretch=2:0.5,slow=1:5,deadline=240"
        );

        for (bad, message) in [
            (
                json!({"asset": "X", "trail_pct": 2, "atr_multiple": 3, "atr_timeframe": "1m"}),
                "not both",
            ),
            (json!({"asset": "X", "atr_multiple": 3}), "go together"),
            (
                json!({"asset": "X", "atr_multiple": 3, "atr_timeframe": "4h"}),
                "1m, 5m, 15m, 1h",
            ),
            (
                json!({"asset": "X", "take_part_gain_pct": 3}),
                "go together",
            ),
            (json!({"asset": "X", "stretch_z": 2}), "go together"),
            (json!({"asset": "X", "slowdown_minutes": 5}), "go together"),
            (json!({"asset": "X", "fade_minutes": 10}), "go together"),
            (json!({"asset": "X"}), "at least one rule"),
            (
                json!({"asset": "X", "trail_pct": "abc"}),
                "must be a number",
            ),
            (json!({"asset": "X", "trail_pct": 150}), "(0, 100)"),
        ] {
            let error = args(bad.clone()).plan().unwrap_err();
            assert!(error.contains(message), "{bad}: {error}");
        }
        assert!(
            serde_json::from_value::<SetExitPlanArgs>(json!({"asset": "X", "stop": 1})).is_err()
        );
    }

    fn entry(asset: &str, set_at_ms: i64) -> PlanEntry {
        PlanEntry {
            asset: asset.into(),
            symbol: format!("{asset}/USDT"),
            plan: ExitPlan::parse_spec("trail=3").unwrap(),
            spec: "trail=3".into(),
            entry_ms: 1_000,
            entry_price: dec!(0.2),
            set_at_ms,
            cycle_id: "c1".into(),
        }
    }

    #[test]
    fn prompt_section_shows_live_trials_and_skips_finished_ones() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ExitPaths::from_lookup(|name| {
            let file = match name {
                "CLAUDE_SPOT_EXIT_PLANS_PATH" => "plans.json",
                "CLAUDE_SPOT_EXIT_WATCH_STATE_PATH" => "state.json",
                _ => "log.jsonl",
            };
            Some(dir.path().join(file).display().to_string())
        });
        assert_eq!(prompt_section(&paths, 10_000, 60_000, false), "");

        let ena = entry("ENA", 2_000);
        let op = entry("OP", 3_000);
        let sol = entry("SOL", 4_000);
        let mut book = PlanBook::default();
        book.plans.insert("ENA".into(), ena.clone());
        book.plans.insert("OP".into(), op.clone());
        book.plans.insert("SOL".into(), sol.clone());
        book.save(&paths.plans).unwrap();
        let mut state = WatchState {
            updated_at_ms: 9_000,
            closed_trials: 2,
            plan_vs_claude_usdt: dec!(0.125),
            ..WatchState::default()
        };
        state.finalized.insert(op.key());
        state.active.insert(
            "ENA".into(),
            WatchView {
                key: ena.key(),
                spec: ena.spec.clone(),
                symbol: ena.symbol.clone(),
                entry_price: ena.entry_price,
                trail_pct: Some(dec!(3)),
                peak: dec!(0.21),
                sell_line: Some(dec!(0.2037)),
                remaining: Decimal::ZERO,
                last_bid: Some(dec!(0.2030)),
                last_ms: 9_000,
                fills: vec![FillView {
                    ts_ms: 8_000,
                    price: dec!(0.2036),
                    fraction: Decimal::ONE,
                    reason: "follow_peak".into(),
                }],
            },
        );
        state.recent_closed.push(ClosedView {
            asset: "ZEC".into(),
            spec: "atr=3:1h,stretch=2.5:1".into(),
            entry_ms: 1_000,
            entry_price: dec!(1172.13),
            exit_ms: 4_021_000,
            exit_price: dec!(1172.85),
            plan_fills: vec![FillView {
                ts_ms: 4_020_000,
                price: dec!(1172.94),
                fraction: Decimal::ONE,
                reason: "stretch".into(),
            }],
            fee_rate: dec!(0.001),
            plan_net_pct: dec!(-0.1309),
            exit_net_pct: dec!(-0.1385),
        });
        state.save(&paths.state).unwrap();

        let section = prompt_section(&paths, 10_000, 60_000, false);
        assert!(section.contains("- ENA trail=3 (entry 0.2): peak 0.21, sell line 0.2037, last bid 0.203, would have sold 100% at 0.2036 (follow_peak, 00:00:08)"), "{section}");
        assert!(
            section.contains("- SOL trail=3 (entry 0.2, set 00:00:04): waiting for the watcher"),
            "{section}"
        );
        assert!(!section.contains("OP"), "{section}");
        assert!(section.contains("closed positions with a plan: 2; plan vs your own exits: 0.125"));
        assert!(!section.contains("watcher not running"));
        assert!(prompt_section(&paths, 100_000, 60_000, false).contains("watcher not running"));

        let live = prompt_section(&paths, 10_000, 60_000, true);
        assert!(
            live.contains("LIVE: the trader sells a position when its plan fires"),
            "{live}"
        );
        assert!(live.contains("plan fired: sell 100% at 0.2036"), "{live}");
        assert!(
            live.contains("- closed ZEC atr=3:1h,stretch=2.5:1: bought at 1172.13 (00:00:01); plan sold 100% at 1172.94 by stretch (01:07:00); position closed at 1172.85 (01:07:01, after 67 min); net after fees -0.13% (fees 0.1% per side)"),
            "{live}"
        );
        assert!(
            section.contains("net after fees: your exit -0.14%, with the plan -0.13%"),
            "{section}"
        );
    }
}

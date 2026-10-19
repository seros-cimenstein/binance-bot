//! Phase 45 exit plans as the dashboard shows them: the run settings, the bot's plan book,
//! the watch state (running plans with their sell lines) and the newest watch-log events.
//!
//! Everything is read from the files the trader's exit guard (live) or the paper watcher
//! writes; nothing here changes a plan or places an order.

use std::collections::BTreeSet;
use std::path::Path;

use bb_core::audit::JsonlLog;
use bb_trader::exit_book::{ExitPaths, ExitSettings, PlanBook, WatchState, WatchView};
use bb_trader::exit_watch::closed_by_plan;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};

/// Newest watch-log events sent to the page.
pub const EVENT_LIMIT: usize = 60;
/// The watch state counts as stale when it is older than this. The live guard rewrites it
/// every poll (5 s by default) and the paper watcher every loop.
pub const STATE_STALE_MS: i64 = 120_000;

/// Env vars that configure exit plans; when none is set this host does not know how the
/// trader runs them.
const SETTING_VARS: [&str; 4] = [
    "BOT_SPOT_EXIT_PLANS_LIVE",
    "BOT_SPOT_DEFAULT_EXIT_PLAN",
    "BOT_SPOT_DEFAULT_EXIT_PLAN_FROM",
    "BOT_SPOT_EXIT_POLL_SECONDS",
];

/// Exit-plan view. `lookup` reads the `BOT_SPOT_*EXIT*` settings (the process env in
/// production); `plan_exits` are the audit's live exit-plan sells, which mark the watch
/// log's positions a plan closed (see [`closed_by_plan`]).
pub fn exits_view(
    paths: &ExitPaths,
    lookup: impl Fn(&str) -> Option<String>,
    plan_exits: &BTreeSet<(String, i64)>,
    now_ms: i64,
) -> Value {
    let configured = SETTING_VARS
        .iter()
        .any(|name| lookup(name).is_some_and(|v| !v.trim().is_empty()));
    let settings = match ExitSettings::from_lookup(&lookup) {
        Ok(s) => json!({
            "configured": configured,
            "live": s.live,
            "default_plan": s.default_plan.map(|d| d.plan.to_string()),
            "default_plan_from": s.default_plan.and_then(|d| iso(d.from_ms)),
            "poll_seconds": s.poll_seconds,
        }),
        Err(error) => json!({"configured": configured, "error": error}),
    };
    let (book, book_error) = match PlanBook::load(&paths.plans) {
        Ok(book) => (book, None),
        Err(error) => (
            PlanBook::default(),
            Some(format!("cannot read {}: {error}", paths.plans.display())),
        ),
    };
    let state_present = paths.state.exists();
    // Parsed here instead of `WatchState::load`, which turns an unreadable file into an
    // empty state: the page must say the file is broken rather than show no positions.
    let (state, state_error) = match std::fs::read(&paths.state) {
        Ok(bytes) => match serde_json::from_slice::<WatchState>(&bytes) {
            Ok(state) => (state, None),
            Err(error) => (
                WatchState::default(),
                Some(format!("cannot parse {}: {error}", paths.state.display())),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (WatchState::default(), None),
        Err(error) => (
            WatchState::default(),
            Some(format!("cannot read {}: {error}", paths.state.display())),
        ),
    };
    let active: Vec<Value> = state
        .active
        .iter()
        .map(|(asset, view)| {
            let by_bot = book.plans.get(asset).is_some_and(|p| p.key() == view.key);
            active_view(asset, view, by_bot)
        })
        .collect();
    let waiting: Vec<Value> = book
        .plans
        .values()
        .filter(|p| !state.finalized.contains(&p.key()))
        .filter(|p| state.active.get(&p.asset).is_none_or(|v| v.key != p.key()))
        .map(|p| {
            json!({
                "asset": p.asset,
                "symbol": p.symbol,
                "spec": p.spec,
                "entry_price": p.entry_price,
                "entry_at": iso(p.entry_ms),
                "set_at": iso(p.set_at_ms),
                "cycle_id": p.cycle_id,
            })
        })
        .collect();
    let (events, events_error) = recent_events(&paths.log, plan_exits);
    json!({
        "settings": settings,
        "state_present": state_present,
        "state_path": paths.state.display().to_string(),
        "updated_at": state_present.then(|| iso(state.updated_at_ms)).flatten(),
        "stale": !state_present || now_ms - state.updated_at_ms > STATE_STALE_MS,
        "active": active,
        "waiting": waiting,
        "closed_trials": state.closed_trials,
        "finalized_count": state.finalized.len(),
        "plan_vs_bot_usdt": state.plan_vs_bot_usdt,
        "events": events,
        "book_error": book_error,
        "state_error": state_error,
        "events_error": events_error,
    })
}

fn active_view(asset: &str, view: &WatchView, by_bot: bool) -> Value {
    let pct = |num: Decimal, den: Decimal| {
        (den > Decimal::ZERO)
            .then(|| bb_core::money::to_f64((num / den - Decimal::ONE) * Decimal::ONE_HUNDRED))
    };
    let bid = view.last_bid;
    json!({
        "asset": asset,
        "key": view.key,
        "spec": view.spec,
        "symbol": view.symbol,
        "source": if by_bot { "bot" } else { "default" },
        "entry_price": view.entry_price,
        "peak": view.peak,
        "sell_line": view.sell_line,
        "last_bid": bid,
        "last_at": iso(view.last_ms),
        "trail_pct": view.trail_pct,
        "remaining": view.remaining,
        "fills": view.fills.iter().map(|f| json!({
            "at": iso(f.ts_ms),
            "price": f.price,
            "fraction": f.fraction,
            "reason": f.reason,
        })).collect::<Vec<_>>(),
        "gain_pct": bid.and_then(|b| pct(b, view.entry_price)),
        "peak_gain_pct": pct(view.peak, view.entry_price),
        // How far the bid may still fall before the trailing line sells (negative: below it).
        "room_to_sell_line_pct": bid.zip(view.sell_line).and_then(|(b, line)| pct(b, line)),
    })
}

/// Newest watch-log events first, with `ts_ms` turned into `at`. A `closed` event of a
/// position a live plan sell closed gets `closed_by_plan` and its real result as
/// `exit_net_pct`; its plan-vs-bot fields (in rows from before 2026-10-19 they compared the
/// plan's sell with itself) are dropped.
fn recent_events(path: &Path, plan_exits: &BTreeSet<(String, i64)>) -> (Vec<Value>, Option<String>) {
    match JsonlLog::new(path).read_tail::<Value>(EVENT_LIMIT) {
        Ok(rows) => (
            rows.into_iter()
                .rev()
                .map(|mut row| {
                    if let Some(at) = row.get("ts_ms").and_then(Value::as_i64).and_then(iso) {
                        row["at"] = json!(at);
                    }
                    if row.get("event").and_then(Value::as_str) == Some("closed") {
                        let by_plan = closed_by_plan(&row, plan_exits);
                        if let Some(fields) = row.as_object_mut() {
                            if by_plan && !fields.contains_key("exit_net_pct") {
                                let net = fields.get("bot_alone_net_pct").cloned().unwrap_or(Value::Null);
                                fields.insert("exit_net_pct".into(), net);
                            }
                            if by_plan {
                                for key in ["bot_alone_net_pct", "with_plan_net_pct", "plan_vs_bot_usdt"] {
                                    fields.remove(key);
                                }
                            }
                            fields.insert("closed_by_plan".into(), json!(by_plan));
                        }
                    }
                    row
                })
                .collect(),
            None,
        ),
        Err(error) => (Vec::new(), Some(error.to_string())),
    }
}

fn iso(ms: i64) -> Option<String> {
    (ms > 0)
        .then(|| DateTime::<Utc>::from_timestamp_millis(ms))
        .flatten()
        .map(|t| t.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bb_trader::exit_book::{FillView, PlanEntry};
    use bb_trader::exit_plan::ExitPlan;
    use rust_decimal_macros::dec;

    use super::*;

    fn paths(dir: &Path) -> ExitPaths {
        ExitPaths {
            plans: dir.join("plans.json"),
            state: dir.join("state.json"),
            log: dir.join("log.jsonl"),
        }
    }

    fn entry(asset: &str, spec: &str, set_at_ms: i64) -> PlanEntry {
        PlanEntry {
            asset: asset.into(),
            symbol: format!("{asset}/USDT"),
            plan: ExitPlan::parse_spec(spec).unwrap(),
            spec: spec.into(),
            entry_ms: 1_000,
            entry_price: dec!(2),
            set_at_ms,
            cycle_id: "c1".into(),
        }
    }

    fn watch(key: String, spec: &str) -> WatchView {
        WatchView {
            key,
            spec: spec.into(),
            symbol: "X/USDT".into(),
            entry_price: dec!(2),
            trail_pct: Some(dec!(5)),
            peak: dec!(2.5),
            sell_line: Some(dec!(2.375)),
            remaining: dec!(0.5),
            last_bid: Some(dec!(2.4)),
            last_ms: 9_000,
            fills: vec![FillView {
                ts_ms: 8_000,
                price: dec!(2.45),
                fraction: dec!(0.5),
                reason: "stretch".into(),
            }],
        }
    }

    #[test]
    fn running_plans_show_their_source_and_sell_line_and_unwatched_plans_wait() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        // The bot set plans for SOL (running) and ENA (not picked up yet); ETH runs the default.
        let sol = entry("SOL", "trail=5", 2_000);
        let ena = entry("ENA", "deadline=60", 3_000);
        let book = PlanBook {
            plans: BTreeMap::from([("SOL".to_owned(), sol.clone()), ("ENA".to_owned(), ena)]),
        };
        std::fs::write(&paths.plans, serde_json::to_vec(&book).unwrap()).unwrap();
        let state = WatchState {
            updated_at_ms: 10_000,
            active: BTreeMap::from([
                ("SOL".to_owned(), watch(sol.key(), "trail=5")),
                ("ETH".to_owned(), watch("ETH@1000@0".into(), "stretch=2:1")),
            ]),
            closed_trials: 2,
            plan_vs_bot_usdt: dec!(0.125),
            ..WatchState::default()
        };
        state.save(&paths.state).unwrap();
        let log = JsonlLog::new(&paths.log);
        log.append(&json!({"event": "started", "asset": "SOL"}))
            .unwrap();
        log.append(&json!({"event": "would_sell", "asset": "SOL", "ts_ms": 8_000}))
            .unwrap();

        let settings = |name: &str| match name {
            "BOT_SPOT_EXIT_PLANS_LIVE" => Some("true".to_owned()),
            "BOT_SPOT_DEFAULT_EXIT_PLAN" => Some("stretch=2:1".to_owned()),
            "BOT_SPOT_DEFAULT_EXIT_PLAN_FROM" => Some("2026-10-15T09:17:34Z".to_owned()),
            _ => None,
        };
        let view = exits_view(&paths, settings, &BTreeSet::new(), 10_000 + STATE_STALE_MS);

        assert_eq!(view["settings"]["live"], json!(true));
        assert_eq!(view["settings"]["default_plan"], json!("stretch=2:1"));
        assert_eq!(view["stale"], json!(false));
        let sources: Vec<(&str, &str)> = view["active"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["asset"].as_str().unwrap(), a["source"].as_str().unwrap()))
            .collect();
        assert_eq!(sources, [("ETH", "default"), ("SOL", "bot")]);
        let sol_view = &view["active"][1];
        // Bid 2.4 on entry 2 is +20%; the 2.375 sell line is ~1.05% below the bid.
        assert!((sol_view["gain_pct"].as_f64().unwrap() - 20.0).abs() < 1e-9);
        assert!(
            (sol_view["room_to_sell_line_pct"].as_f64().unwrap() - 1.052_631_578_9).abs() < 1e-6
        );
        assert_eq!(sol_view["fills"][0]["reason"], json!("stretch"));
        let waiting: Vec<&str> = view["waiting"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["asset"].as_str().unwrap())
            .collect();
        assert_eq!(waiting, ["ENA"]);
        assert_eq!(view["closed_trials"], json!(2));
        // Newest event first, with its time.
        assert_eq!(view["events"][0]["event"], json!("would_sell"));
        assert!(view["events"][0]["at"].as_str().is_some());
    }

    #[test]
    fn missing_files_and_unset_env_report_an_unknown_setup_and_a_stale_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let view = exits_view(&paths(dir.path()), |_| None, &BTreeSet::new(), 50_000);

        assert_eq!(view["settings"]["configured"], json!(false));
        assert_eq!(view["state_present"], json!(false));
        assert_eq!(view["stale"], json!(true));
        assert_eq!(view["updated_at"], Value::Null);
        assert_eq!(view["active"], json!([]));
        assert_eq!(view["events"], json!([]));
        assert_eq!(view["state_error"], Value::Null);
    }

    #[test]
    fn unparseable_watch_state_is_reported_not_shown_as_no_positions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.state, r#"{"updated_at_ms": 1.5}"#).unwrap();
        let view = exits_view(&paths, |_| None, &BTreeSet::new(), 50_000);
        assert!(view["state_error"].as_str().unwrap().contains("cannot parse"));
        assert_eq!(view["active"], json!([]));
    }

    #[test]
    fn watch_state_older_than_the_limit_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let state = WatchState {
            updated_at_ms: 1_000,
            ..WatchState::default()
        };
        state.save(&paths.state).unwrap();
        assert_eq!(
            exits_view(&paths, |_| None, &BTreeSet::new(), 1_000 + STATE_STALE_MS)["stale"],
            json!(false)
        );
        assert_eq!(
            exits_view(&paths, |_| None, &BTreeSet::new(), 1_001 + STATE_STALE_MS)["stale"],
            json!(true)
        );
    }

    #[test]
    fn closed_events_of_plan_sold_positions_show_their_result_not_a_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let log = JsonlLog::new(&paths.log);
        // An old row the audit shows a plan sold, a new row that says so itself, and a
        // position the bot sold (a real comparison).
        for row in [
            json!({"event": "closed", "asset": "ENA", "bot_exit_ms": 5_000, "bot_alone_net_pct": "0.5",
                   "with_plan_net_pct": "0.6", "plan_vs_bot_usdt": "-0.01"}),
            json!({"event": "closed", "asset": "UNI", "closed_by_plan": true, "exit_net_pct": "1.5",
                   "fill_vs_trigger_usdt": "-0.002"}),
            json!({"event": "closed", "asset": "SOL", "bot_exit_ms": 9_000, "bot_alone_net_pct": "-1",
                   "with_plan_net_pct": "1", "plan_vs_bot_usdt": "0.2"}),
        ] {
            log.append(&row).unwrap();
        }
        let plan_exits = BTreeSet::from([("ENA".to_owned(), 5_000)]);
        let view = exits_view(&paths, |_| None, &plan_exits, 50_000);
        let events = view["events"].as_array().unwrap();

        let sol = &events[0];
        assert_eq!(sol["closed_by_plan"], json!(false));
        assert_eq!(sol["plan_vs_bot_usdt"], json!("0.2"));
        let uni = &events[1];
        assert_eq!((&uni["closed_by_plan"], &uni["exit_net_pct"]), (&json!(true), &json!("1.5")));
        let ena = &events[2];
        assert_eq!((&ena["closed_by_plan"], &ena["exit_net_pct"]), (&json!(true), &json!("0.5")));
        assert!(ena.get("plan_vs_bot_usdt").is_none() && ena.get("with_plan_net_pct").is_none());
    }
}

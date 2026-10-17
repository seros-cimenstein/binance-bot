//! Results per trading session instead of per coin.
//!
//! A session is one setup of the trader: the same prompt version, the same omp session and
//! (for cycles from 2026-10-17 on) the same recorded run configuration (`run_config` on the
//! cycle summary rows). Each trade belongs to the session of the cycle that opened it; older
//! rows without a cycle id go to the session of the first cycle summary written after them.
//! The exit plans a session ran come from the watch log's `started` events, matched to the
//! trade they followed.
//!
//! The winner is the session with the highest realized P&L (closed trades, after fees).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::history::{CycleRecord, ParsedAudit, Trade, TradeStatus};

/// A watch-log plan whose position was opened within this long of a trade belongs to it.
const PLAN_MATCH_MS: i64 = 10 * 60_000;

fn millis(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc).timestamp_millis())
}

/// `claude-spot-v11-agentic` -> `v11`.
fn short_prompt(version: &str) -> &str {
    version
        .strip_prefix("claude-spot-")
        .and_then(|v| v.strip_suffix("-agentic"))
        .unwrap_or(version)
}

struct Session<'a> {
    prompt_version: String,
    session_id: String,
    run_config: Option<Value>,
    first_at: String,
    last_at: String,
    cycles: usize,
    models: BTreeMap<String, usize>,
    thinking: BTreeMap<String, usize>,
    pick_modes: BTreeMap<String, usize>,
    llm_cost_usd: Decimal,
    trades: Vec<(&'a Trade, Option<String>)>,
    plans: BTreeMap<String, usize>,
}

impl<'a> Session<'a> {
    fn new(cycle: &CycleRecord) -> Self {
        Self {
            prompt_version: cycle.prompt_version.clone(),
            session_id: cycle.session_id.clone(),
            run_config: cycle.run_config.clone(),
            first_at: cycle.timestamp.clone(),
            last_at: cycle.timestamp.clone(),
            cycles: 0,
            models: BTreeMap::new(),
            thinking: BTreeMap::new(),
            pick_modes: BTreeMap::new(),
            llm_cost_usd: Decimal::ZERO,
            trades: Vec::new(),
            plans: BTreeMap::new(),
        }
    }

    fn add_cycle(&mut self, cycle: &CycleRecord) {
        self.cycles += 1;
        self.last_at.clone_from(&cycle.timestamp);
        for (map, value) in [
            (&mut self.models, &cycle.model),
            (&mut self.thinking, &cycle.thinking),
            (&mut self.pick_modes, &cycle.market_pick_mode),
        ] {
            if !value.is_empty() {
                *map.entry(value.clone()).or_default() += 1;
            }
        }
        self.llm_cost_usd += cycle.cost_usd.unwrap_or_default();
    }

    /// Realized P&L of closed trades (wins and losses), the session's score.
    fn realized(&self) -> (Decimal, Decimal, usize) {
        let mut pnl = Decimal::ZERO;
        let mut cost = Decimal::ZERO;
        let mut closed = 0;
        for (trade, _) in &self.trades {
            if matches!(trade.status, TradeStatus::Win | TradeStatus::Loss) {
                pnl += trade.pnl_usdt.unwrap_or_default();
                cost += trade.cost_usdt.unwrap_or_default();
                closed += 1;
            }
        }
        (pnl, cost, closed)
    }

    fn stats(&self) -> Value {
        let (pnl, cost, closed) = self.realized();
        let count = |status: TradeStatus| self.trades.iter().filter(|(t, _)| t.status == status).count();
        let wins = count(TradeStatus::Win);
        let closed_pcts: Vec<f64> = self
            .trades
            .iter()
            .filter(|(t, _)| matches!(t.status, TradeStatus::Win | TradeStatus::Loss))
            .filter_map(|(t, _)| t.pnl_pct)
            .collect();
        let holds: Vec<i64> = self
            .trades
            .iter()
            .filter(|(t, _)| matches!(t.status, TradeStatus::Win | TradeStatus::Loss))
            .filter_map(|(t, _)| t.hold_seconds)
            .collect();
        #[allow(clippy::cast_precision_loss)]
        let mean = |values: &[i64]| {
            (!values.is_empty()).then(|| values.iter().sum::<i64>() as f64 / values.len() as f64)
        };
        let fees: Decimal = self
            .trades
            .iter()
            .filter(|(t, _)| matches!(t.status, TradeStatus::Win | TradeStatus::Loss))
            .filter_map(|(t, _)| t.fees_usdt)
            .sum();
        let open_pnl: Decimal = self
            .trades
            .iter()
            .filter(|(t, _)| t.status == TradeStatus::Open)
            .filter_map(|(t, _)| t.pnl_usdt)
            .sum();
        #[allow(clippy::cast_precision_loss)]
        let win_rate = (closed > 0).then(|| wins as f64 * 100.0 / closed as f64);
        json!({
            "trades": self.trades.len(),
            "closed": closed,
            "wins": wins,
            "losses": count(TradeStatus::Loss),
            "open": count(TradeStatus::Open),
            "unknown": count(TradeStatus::Unknown),
            "win_rate_pct": win_rate,
            "realized_pnl_usdt": pnl,
            "realized_cost_usdt": cost,
            "realized_pct": (cost > Decimal::ZERO)
                .then(|| bb_core::money::to_f64(pnl / cost * Decimal::ONE_HUNDRED)),
            "open_pnl_usdt": open_pnl,
            "fees_usdt": fees,
            "best_pct": closed_pcts.iter().copied().reduce(f64::max),
            "worst_pct": closed_pcts.iter().copied().reduce(f64::min),
            "avg_hold_minutes": mean(&holds).map(|s| s / 60.0),
            "closed_by_plan": self
                .trades
                .iter()
                .filter(|(t, _)| t.closed_by_plan && matches!(t.status, TradeStatus::Win | TradeStatus::Loss))
                .count(),
        })
    }
}

/// `started` events of the exit-plan watch log: (asset, position entry ms, plan spec).
fn started_plans(watch_log: &Path) -> Vec<(String, i64, String)> {
    let Ok(text) = std::fs::read_to_string(watch_log) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row.get("event").and_then(Value::as_str) == Some("started"))
        .filter_map(|row| {
            let asset = row.get("asset")?.as_str()?.to_owned();
            let entry_ms = row.get("key")?.as_str()?.split('@').nth(1)?.parse().ok()?;
            let spec = row.get("spec")?.as_str()?.to_owned();
            Some((asset, entry_ms, spec))
        })
        .collect()
}

/// Sessions ranked by realized P&L, each with its stats, trades and configuration.
pub fn sessions_view(audit: &ParsedAudit, trades: &[Trade], watch_log: &Path) -> Value {
    let mut sessions: Vec<Session<'_>> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut cycle_session: HashMap<&str, usize> = HashMap::new();
    // (cycle time, session) in log order, for rows without a cycle id.
    let mut timeline: Vec<(i64, usize)> = Vec::new();
    for cycle in &audit.cycles {
        let key = format!(
            "{}|{}|{}",
            cycle.prompt_version,
            cycle.session_id,
            cycle.run_config.as_ref().map(Value::to_string).unwrap_or_default()
        );
        let index = *by_key.entry(key).or_insert_with(|| {
            sessions.push(Session::new(cycle));
            sessions.len() - 1
        });
        sessions[index].add_cycle(cycle);
        if !cycle.cycle_id.is_empty() {
            cycle_session.insert(&cycle.cycle_id, index);
        }
        if let Some(at) = millis(&cycle.timestamp) {
            timeline.push((at, index));
        }
    }

    let plans = started_plans(watch_log);
    let mut unattributed = 0;
    for trade in trades {
        if crate::history::CASH_ASSETS.contains(&trade.destination_asset.as_str()) {
            continue;
        }
        let opened = millis(&trade.opened_at);
        let session = cycle_session.get(trade.cycle_id.as_str()).copied().or_else(|| {
            let opened = opened?;
            timeline.iter().find(|(at, _)| *at >= opened).map(|(_, index)| *index)
        });
        let Some(index) = session else {
            unattributed += 1;
            continue;
        };
        let plan = opened.and_then(|opened| {
            plans
                .iter()
                .find(|(asset, entry, _)| {
                    *asset == trade.destination_asset && (entry - opened).abs() <= PLAN_MATCH_MS
                })
                .map(|(_, _, spec)| spec.clone())
        });
        if let Some(spec) = &plan {
            *sessions[index].plans.entry(spec.clone()).or_default() += 1;
        }
        sessions[index].trades.push((trade, plan));
    }

    // Labels: prompt and session, numbered when one pair ran several setups.
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let labels: Vec<String> = sessions
        .iter()
        .map(|s| {
            let n = seen
                .entry((s.prompt_version.clone(), s.session_id.clone()))
                .or_default();
            *n += 1;
            let id: String = s.session_id.chars().take(8).collect();
            let base = format!(
                "{} · {}",
                short_prompt(&s.prompt_version),
                if id.is_empty() { "no session id" } else { id.as_str() }
            );
            if *n > 1 { format!("{base} · setup {n}") } else { base }
        })
        .collect();

    let mut order: Vec<usize> = (0..sessions.len()).collect();
    order.sort_by(|&a, &b| {
        let (pa, _, ca) = sessions[a].realized();
        let (pb, _, cb) = sessions[b].realized();
        (cb > 0)
            .cmp(&(ca > 0))
            .then(pb.cmp(&pa))
            .then(sessions[b].last_at.cmp(&sessions[a].last_at))
    });
    let winner = order
        .first()
        .copied()
        .filter(|&i| sessions[i].realized().2 > 0);

    let views: Vec<Value> = order
        .iter()
        .enumerate()
        .map(|(rank, &index)| {
            let s = &sessions[index];
            let counts = |map: &BTreeMap<String, usize>| -> Vec<Value> {
                map.iter().map(|(name, n)| json!({"name": name, "count": n})).collect()
            };
            json!({
                "key": format!("{}|{}|{}", s.prompt_version, s.session_id, s.first_at),
                "label": labels[index],
                "rank": rank + 1,
                "winner": winner == Some(index),
                "prompt_version": s.prompt_version,
                "session_id": s.session_id,
                "first_at": s.first_at,
                "last_at": s.last_at,
                "cycles": s.cycles,
                "llm_cost_usd": s.llm_cost_usd,
                "models": counts(&s.models),
                "thinking": counts(&s.thinking),
                "market_pick_modes": counts(&s.pick_modes),
                "exit_plans": counts(&s.plans),
                "run_config": s.run_config,
                "stats": s.stats(),
                "trades": s.trades.iter().rev().map(|(t, plan)| json!({
                    "opened_at": t.opened_at,
                    "closed_at": t.closed_at,
                    "closed_into": t.closed_into,
                    "asset": t.destination_asset,
                    "status": t.status,
                    "cost_usdt": t.cost_usdt,
                    "pnl_usdt": t.pnl_usdt,
                    "pnl_pct": t.pnl_pct,
                    "hold_seconds": t.hold_seconds,
                    "closed_by_plan": t.closed_by_plan,
                    "plan": plan,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "sessions": views,
        "winner": winner.map(|i| labels[i].clone()),
        "unattributed_trades": unattributed,
        "error": audit.error,
    })
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::history::Valuation;

    fn cycle(at: &str, id: &str, prompt: &str, session: &str, config: Option<Value>) -> CycleRecord {
        CycleRecord {
            timestamp: at.into(),
            cycle_id: id.into(),
            prompt_version: prompt.into(),
            session_id: session.into(),
            model: "openai-codex/gpt-6.1-sol".into(),
            thinking: "high".into(),
            market_pick_mode: "small_market_aggressive".into(),
            cost_usd: Some(dec!(0.01)),
            run_config: config,
        }
    }

    fn trade(at: &str, cycle_id: &str, asset: &str, status: TradeStatus, pnl: Decimal) -> Trade {
        Trade {
            opened_at: at.into(),
            closed_at: None,
            closed_into: None,
            cycle_id: cycle_id.into(),
            route_id: format!("USDT->{asset}"),
            source_asset: "USDT".into(),
            destination_asset: asset.into(),
            reason: String::new(),
            legs: 1,
            closed_by_plan: false,
            valuation: Valuation::Fills,
            status,
            cost_usdt: Some(dec!(10)),
            exit_usdt: Some(dec!(10) + pnl),
            pnl_usdt: Some(pnl),
            pnl_pct: Some(bb_core::money::to_f64(pnl * dec!(10))),
            fees_usdt: Some(dec!(0.02)),
            hold_seconds: Some(3600),
        }
    }

    #[test]
    fn trades_join_the_session_that_bought_them_and_the_best_realized_pnl_wins() {
        let deadline = json!({"model": "sol", "exit_plans": {"default_plan": "deadline=120"}});
        let audit = ParsedAudit {
            cycles: vec![
                cycle("2026-10-15T10:00:00+00:00", "a1", "claude-spot-v7-agentic", "S1", None),
                cycle("2026-10-15T10:20:00+00:00", "a2", "claude-spot-v7-agentic", "S1", None),
                cycle("2026-10-16T20:00:00+00:00", "b1", "claude-spot-v8-agentic", "S2", None),
                // Same prompt and session, a different recorded setup: its own session.
                cycle("2026-10-17T13:00:00+00:00", "c1", "claude-spot-v11-agentic", "S1", Some(deadline.clone())),
                cycle("2026-10-17T13:20:00+00:00", "c2", "claude-spot-v11-agentic", "S1", Some(json!({"model": "other"}))),
            ],
            ..ParsedAudit::default()
        };
        let trades = vec![
            trade("2026-10-15T10:05:00+00:00", "a2", "UNI", TradeStatus::Win, dec!(0.35)),
            // An older row without a cycle id: the next cycle summary after it is a1.
            trade("2026-10-15T09:59:00+00:00", "", "NEAR", TradeStatus::Win, dec!(0.23)),
            trade("2026-10-16T19:59:00+00:00", "b1", "ZEC", TradeStatus::Loss, dec!(-0.1)),
            trade("2026-10-17T12:59:00+00:00", "c1", "SOL", TradeStatus::Open, dec!(0.5)),
            // Selling into USDT is parking, not a session's bet.
            trade("2026-10-17T13:10:00+00:00", "c1", "USDT", TradeStatus::Cash, dec!(0)),
        ];
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("watch.jsonl");
        let uni_entry = millis("2026-10-15T10:05:02+00:00").unwrap();
        std::fs::write(
            &log,
            format!("{}\n", json!({"event": "started", "asset": "UNI", "key": format!("UNI@{uni_entry}@{uni_entry}"), "spec": "deadline=180"})),
        )
        .unwrap();

        let view = sessions_view(&audit, &trades, &log);
        let sessions = view["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 4);
        let first = &sessions[0];
        assert_eq!(first["label"], "v7 · S1");
        assert_eq!((first["rank"].as_u64(), first["winner"].as_bool()), (Some(1), Some(true)));
        assert_eq!(first["stats"]["closed"], 2);
        assert_eq!(first["stats"]["realized_pnl_usdt"], json!("0.58"));
        assert_eq!(first["exit_plans"], json!([{"name": "deadline=180", "count": 1}]));
        assert_eq!(view["winner"], "v7 · S1");
        // The loser is ranked after it; sessions without closed trades come last.
        assert_eq!(sessions[1]["label"], "v8 · S2");
        let v11: Vec<&Value> = sessions.iter().filter(|s| s["prompt_version"] == "claude-spot-v11-agentic").collect();
        assert_eq!(v11.len(), 2);
        let with_sol = v11.iter().find(|s| s["run_config"] == deadline).unwrap();
        assert_eq!(with_sol["stats"]["open"], 1);
        assert_eq!(with_sol["stats"]["trades"], 1);
        assert!(v11.iter().any(|s| s["label"] == "v11 · S1 · setup 2"));
    }
}

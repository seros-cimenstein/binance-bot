//! Edge-style weights (Phase 53 D2): a rule whose weights follow the measured edge of the
//! tag scoreboard ([`crate::scoreboard`]) instead of the operator's numbers.
//!
//! For a sleeve with `edge_horizon` `h`, every scoreboard row at `h` gives its signal
//! `weight = clamp(edge_scale × all.excess_pct, −edge_cap, +edge_cap)` when
//! `|all.t| ≥ edge_min_t` and the row is not `decayed`; other names weigh 0. Per-asset
//! names (`talk.6h`, `stance.6h`, `momentum.*`, `<kind>.*`) read the `assets` row,
//! market-wide names (`market.*`) the `basket` row (operator, 2026-10-31).
//!
//! Evidence is valid at decision time `D` only when the scoreboard parses (schema
//! `tag-scoreboard-v1`), is usable at `D` ([`Scoreboard::usable_at`]: every outcome in it
//! closed by `D`, not merely its history), was measured with the sleeve's `min_posters`,
//! and has rows for `h`. No maximum age. Without valid evidence the sleeve holds: no orders,
//! and the decision row says why ([`crate::decider::Verdict::blocked`]).
//!
//! Live, [`EdgeBook`] reads the file at start, again after `edge_refresh_hours`, and after a
//! failed read; every decision checks the cached board against its own time. A backtest
//! picks, per step, the newest of its point-in-time scoreboards usable at the step
//! ([`newest_usable`]).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::rules::{Edge, Sleeve};
use crate::scoreboard::{Scoreboard, Target};
use crate::signals::Name;

const HOUR_MS: f64 = 3_600_000.0;

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// The weights `board` gives `sleeve` at `decision_ms`, or why it gives none (see the
/// module docs).
pub fn weights(
    board: &Scoreboard,
    sleeve: &Sleeve,
    edge: &Edge,
    decision_ms: i64,
) -> Result<BTreeMap<Name, f64>, String> {
    if !board.usable_at(decision_ms) {
        return Err(match &board.available_at {
            Some(at) => format!(
                "scoreboard available_at {at} is after the decision time {}",
                iso(decision_ms)
            ),
            None => "scoreboard has no outcome (available_at none)".to_owned(),
        });
    }
    if board.min_posters != sleeve.min_posters {
        return Err(format!(
            "scoreboard measured with min_posters {}, sleeve {} uses {}",
            board.min_posters, sleeve.name, sleeve.min_posters
        ));
    }
    let horizon = sleeve
        .edge_horizon
        .as_deref()
        .ok_or_else(|| format!("sleeve {} has no edge_horizon", sleeve.name))?;
    if !board.horizons.contains_key(horizon) {
        return Err(format!("scoreboard has no {horizon} rows"));
    }
    let mut out = BTreeMap::new();
    for row in board.rows.iter().filter(|r| r.horizon == horizon) {
        let name: Name = row
            .name
            .parse()
            .map_err(|e| format!("scoreboard row: {e}"))?;
        let target = match name {
            Name::Market { .. } => Target::Basket,
            _ => Target::Assets,
        };
        if row.target != target || row.decayed || row.all.t.abs() < edge.min_t {
            continue;
        }
        let weight = (edge.scale * row.all.excess_pct).clamp(-edge.cap, edge.cap);
        if weight != 0.0 && weight.is_finite() {
            out.insert(name, weight);
        }
    }
    Ok(out)
}

/// Of `boards` (point-in-time snapshots), the one with the latest `available_at` that a
/// decision at `decision_ms` may use.
pub fn newest_usable(boards: &[Scoreboard], decision_ms: i64) -> Option<&Scoreboard> {
    boards
        .iter()
        .filter(|b| b.usable_at(decision_ms))
        .max_by_key(|b| b.available_at_ms)
}

/// Valid evidence for one decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub weights: BTreeMap<Name, f64>,
    /// What the decision row records (`algo.edge`).
    pub record: Value,
}

/// The live trader's scoreboard: read at start, after `edge_refresh_hours`, and after a
/// failed read.
#[derive(Debug)]
pub struct EdgeBook {
    edge: Edge,
    /// When the file was last read and what it gave.
    read: Option<(i64, Result<Scoreboard, String>)>,
}

impl EdgeBook {
    pub fn new(edge: Edge) -> Self {
        Self { edge, read: None }
    }

    /// Read the file when due; true when it was read.
    pub fn refresh(&mut self, now_ms: i64) -> bool {
        let due = match &self.read {
            None | Some((_, Err(_))) => true,
            Some((at, Ok(_))) => (now_ms - at) as f64 >= self.edge.refresh_hours * HOUR_MS,
        };
        if due {
            self.read = Some((now_ms, Scoreboard::read(&self.edge.scoreboard)));
        }
        due
    }

    /// The weights for `sleeve` at `decision_ms` from the board last read, or why there
    /// are none, with what the decision row records either way.
    pub fn evidence(&self, sleeve: &Sleeve, decision_ms: i64) -> Result<Evidence, (String, Value)> {
        let path = self.edge.scoreboard.display().to_string();
        let fail = |reason: String| {
            let record = json!({"status": "unavailable", "scoreboard": path, "reason": reason});
            (reason, record)
        };
        let Some((read_at, board)) = &self.read else {
            return Err(fail("scoreboard not read yet".to_owned()));
        };
        let board = board.as_ref().map_err(|e| fail(e.clone()))?;
        let weights = weights(board, sleeve, &self.edge, decision_ms).map_err(fail)?;
        let record = json!({
            "status": "valid",
            "scoreboard": path,
            "read_at": iso(*read_at),
            "generated_at": board.generated_at,
            "available_at": board.available_at,
            "history": {"from": board.history.from, "to": board.history.to},
            "horizon": sleeve.edge_horizon,
            "weighted": weights.len(),
        });
        Ok(Evidence { weights, record })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rules::Rule;

    const H: i64 = 3_600_000;
    /// 2026-10-31T00:00:00Z.
    pub(crate) const T0: i64 = 1_793_404_800_000;

    fn stats(excess: f64, t: f64) -> Value {
        json!({"n": 40, "excess_pct": excess, "net_pct": excess - 0.15, "hit_rate": 0.6, "t": t, "blocks": 20})
    }

    fn row(name: &str, target: &str, horizon: &str, excess: f64, t: f64, decayed: bool) -> Value {
        json!({"name": name, "target": target, "horizon": horizon, "all": stats(excess, t),
               "older": stats(excess, t), "newer": stats(excess, t), "decayed": decayed})
    }

    /// A small `tag-scoreboard-v1` file: history to `T0`, outcomes (`available_at`) to
    /// `T0 + 7d`, measured with 2 posters.
    pub(crate) fn fixture() -> String {
        fixture_available_at(T0 + 7 * 24 * H)
    }

    /// [`fixture`] with its history ending 7 days before `available_ms`, the close of its
    /// last +7d outcome.
    pub(crate) fn fixture_available_at(available_ms: i64) -> String {
        let at = |ms: i64| iso(ms);
        let to = available_ms - 7 * 24 * H;
        json!({
            "schema": "tag-scoreboard-v1", "generated_at": at(available_ms + 600_000),
            "as_of": at(available_ms), "available_at": at(available_ms),
            "history": {"from": at(to - 30 * 24 * H), "to": at(to), "split": at(to - 15 * 24 * H), "feeds": 721},
            "horizons": {
                "1h": {"signals_to": at(to), "outcomes_to": at(to + H)},
                "24h": {"signals_to": at(to), "outcomes_to": at(to + 24 * H)},
                "7d": {"signals_to": at(to), "outcomes_to": at(available_ms)}},
            "fee_bps": 15.0, "min_posters": 2, "assets": 41,
            "rows": [
                // Strong lead, capped at +2 (scale 3 × 1.5 = 4.5).
                row("stance.6h", "assets", "24h", 1.5, 4.0, false),
                // Fading signal: negative weight −3 × 0.4 = −1.2.
                row("momentum.24h", "assets", "24h", -0.4, -2.5, false),
                // Under the t gate.
                row("talk.6h", "assets", "24h", 0.9, 1.2, false),
                // Decayed.
                row("asset.surge.24h", "assets", "24h", 0.8, 3.1, true),
                // Market name: the basket row counts, the BTC row does not.
                row("market.mood.direction.24h", "basket", "24h", -1.0, -3.0, false),
                row("market.mood.direction.24h", "BTC", "24h", 5.0, 9.0, false),
                // Another horizon.
                row("stance.6h", "assets", "1h", 0.3, 5.0, false)
            ]
        })
        .to_string()
    }

    /// An edge rule file's text (scale 3, cap 2, refresh 6h, min |t| 2) reading `path` at
    /// `horizon`.
    pub(crate) fn edge_rule_text(path: &str, horizon: &str) -> String {
        json!({"rules_version": "edge-test", "style": "edge", "edge_scoreboard": path,
            "edge_scale": 3.0, "edge_cap": 2.0, "edge_refresh_hours": 6.0, "edge_min_t": 2.0,
            "sleeves": [{"name": "fast", "share": 1.0, "horizon": "any", "defensive": ["USDT"],
                "risk_off_below": null, "buy_above": 0.5, "sell_below": -0.5, "min_posters": 2,
                "weights": {}, "edge_horizon": horizon}]})
        .to_string()
    }

    fn edge_rule(path: &str, horizon: &str) -> Rule {
        Rule::parse(&edge_rule_text(path, horizon)).unwrap()
    }

    fn by_name(weights: &BTreeMap<Name, f64>) -> Vec<(String, f64)> {
        weights.iter().map(|(n, w)| (n.to_string(), *w)).collect()
    }

    #[test]
    fn weights_are_the_scaled_capped_t_gated_edge() {
        let board = Scoreboard::parse(&fixture()).unwrap();
        let rule = edge_rule("unused", "24h");
        let (sleeve, edge) = (rule.trading_sleeve(), rule.edge.as_ref().unwrap());
        let later = T0 + 8 * 24 * H;
        let got = by_name(&weights(&board, sleeve, edge, later).unwrap());
        let want = [
            ("stance.6h", 2.0),
            ("momentum.24h", -1.2000000000000002),
            ("market.mood.direction.24h", -2.0),
        ];
        assert_eq!(got.len(), 3, "{got:?}");
        for (name, w) in want {
            let found = got.iter().find(|(n, _)| n == name).unwrap().1;
            assert!((found - w).abs() < 1e-12, "{name}: {found} != {w}");
        }
        // The 1h horizon reads only its own rows.
        let one_hour = edge_rule("unused", "1h");
        let got = weights(&board, one_hour.trading_sleeve(), edge, later).unwrap();
        assert_eq!(
            by_name(&got),
            [("stance.6h".to_owned(), 0.8999999999999999)]
        );
    }

    #[test]
    fn a_scoreboard_is_evidence_only_from_its_available_at_on() {
        let board = Scoreboard::parse(&fixture()).unwrap();
        let rule = edge_rule("unused", "24h");
        let (sleeve, edge) = (rule.trading_sleeve(), rule.edge.as_ref().unwrap());
        let available = T0 + 7 * 24 * H;
        // The history ended at T0, but its +7d outcomes closed only at T0 + 7d: a decision
        // between the two may not use it.
        let err = weights(&board, sleeve, edge, T0 + 24 * H).unwrap_err();
        assert!(
            err.contains("is after the decision time 2026-11-01T00:00:00Z"),
            "{err}"
        );
        assert!(weights(&board, sleeve, edge, available - 1).is_err());
        assert!(weights(&board, sleeve, edge, available).is_ok());
        // Another poster gate or a missing horizon is no evidence either.
        let mut other = board.clone();
        other.min_posters = 3;
        assert!(
            weights(&other, sleeve, edge, available)
                .unwrap_err()
                .contains("min_posters 3")
        );
        other = board.clone();
        other.horizons.remove("24h");
        assert!(
            weights(&other, sleeve, edge, available)
                .unwrap_err()
                .contains("no 24h rows")
        );
        // Walk-forward: the newest snapshot usable at each step.
        let mut early = board.clone();
        early.available_at_ms = Some(T0);
        early.generated_at = "early".into();
        let snaps = [early, board.clone()];
        assert_eq!(newest_usable(&snaps, T0 - 1), None);
        assert_eq!(
            newest_usable(&snaps, T0 + H).map(|b| b.generated_at.as_str()),
            Some("early")
        );
        assert_eq!(
            newest_usable(&snaps, available).map(|b| &b.generated_at),
            Some(&board.generated_at)
        );
    }

    #[test]
    fn the_book_rereads_on_schedule_and_checks_each_decision_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest.json");
        let rule = edge_rule(path.to_str().unwrap(), "24h");
        let sleeve = rule.trading_sleeve();
        let mut book = EdgeBook::new(rule.edge.clone().unwrap());
        let now = T0 + 8 * 24 * H;
        // No file: read again on every refresh, no evidence, the reason recorded.
        assert!(book.refresh(now));
        let (reason, record) = book.evidence(sleeve, now).unwrap_err();
        assert!(
            reason.contains("cannot read") && record["status"] == "unavailable",
            "{record}"
        );
        assert!(book.refresh(now + 15_000));
        std::fs::write(&path, fixture()).unwrap();
        assert!(book.refresh(now + 30_000));
        let ok = book.evidence(sleeve, now + 30_000).unwrap();
        assert_eq!(
            (ok.record["status"].as_str(), ok.record["weighted"].as_u64()),
            (Some("valid"), Some(3))
        );
        // The same cached board, asked for an earlier decision time, is not evidence.
        let (reason, _) = book.evidence(sleeve, T0 + 2 * 24 * H).unwrap_err();
        assert!(reason.contains("after the decision time"), "{reason}");
        // A good read is kept for edge_refresh_hours (6), then read again.
        std::fs::write(&path, "{}").unwrap();
        assert!(!book.refresh(now + 6 * H - 1));
        assert!(book.evidence(sleeve, now + 6 * H - 1).is_ok());
        assert!(book.refresh(now + 6 * H + 30_000));
        let (reason, _) = book.evidence(sleeve, now + 6 * H + 30_000).unwrap_err();
        assert!(reason.contains("missing field"), "{reason}");
        // Another schema is refused.
        std::fs::write(
            &path,
            fixture().replace("tag-scoreboard-v1", "tag-scoreboard-v2"),
        )
        .unwrap();
        assert!(book.refresh(now + 7 * H));
        let (reason, _) = book.evidence(sleeve, now + 7 * H).unwrap_err();
        assert!(reason.contains("schema \"tag-scoreboard-v2\""), "{reason}");
    }
}

//! The operator's rule file ([`crate::rules`]) as a backtest rule: every sleeve decides
//! with the trader's algorithm ([`crate::decider`]) on the tags feed visible at the step,
//! as the live trader does.
//!
//! - Candidates: the tradable (cached) assets the visible feed's live asset tags reach,
//!   directly or through the operator's proxies ([`TagsAt::candidates`]); none without a
//!   visible feed.
//! - Sleeves: a sleeve decides when it has a share or holds something
//!   ([`crate::rules::Sleeve::active`]), on its own holdings; its sells sell exactly its
//!   quantity and its buys spend its share of the portfolio value ([`super::engine`]).
//! - Edge style: per step and sleeve, the newest of the point-in-time scoreboards usable at
//!   the step ([`crate::edge::newest_usable`]: every outcome in it closed by then) gives the
//!   weights; without one the sleeve holds with no orders, as live.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::data::{TagsAt, TagsHistory};
use super::engine::{Books, Intent, StepRule, Trace};
use crate::book::{CoinView, Snapshot};
use crate::decider::{Action, Inputs, Verdict, decide};
use crate::rules::{Rule, Sleeve};
use crate::scoreboard::Scoreboard;

/// Per-step trace header of each sleeve.
pub const TRACE_HEADER: &str = "time,feed_as_of,candidates,scoreboard,best_coin,best_score,held_coin,held_score,market_score,risk_off,action";

/// The reason a sleeve's sell is listed with in the trade list.
const SELL_REASON: &str = "rule";

/// The rule file stepped over the tags history (see the module docs).
pub struct RuleFile {
    rule: Rule,
    tags: Arc<TagsHistory>,
    /// Edge style: the point-in-time scoreboards.
    boards: Arc<Vec<Scoreboard>>,
    steps: usize,
    steps_with_feed: usize,
    candidates: usize,
    /// Scored-only asset tags (no cached market, no proxy) by steps seen live.
    scored_only: BTreeMap<String, usize>,
    /// Per sleeve: steps decided, steps held for want of evidence, trace rows.
    sleeves: BTreeMap<String, SleeveLog>,
}

#[derive(Debug, Default)]
struct SleeveLog {
    decided: usize,
    blocked: usize,
    risk_off: usize,
    rows: Vec<String>,
}

impl RuleFile {
    /// `boards`: the point-in-time scoreboards an edge rule reads (empty for a fixed rule).
    pub fn new(rule: Rule, tags: Arc<TagsHistory>, boards: Arc<Vec<Scoreboard>>) -> Self {
        let sleeves = rule
            .sleeves
            .iter()
            .map(|s| (s.name.clone(), SleeveLog::default()))
            .collect();
        Self {
            rule,
            tags,
            boards,
            steps: 0,
            steps_with_feed: 0,
            candidates: 0,
            scored_only: BTreeMap::new(),
            sleeves,
        }
    }

    /// `sleeve` with the weights the scoreboard usable at `t` gives it, or why there are
    /// none; a fixed rule's sleeve as it is.
    fn weighted(&self, sleeve: &Sleeve, t: i64) -> (Sleeve, Option<String>, Result<(), String>) {
        let Some(edge) = &self.rule.edge else {
            return (sleeve.clone(), None, Ok(()));
        };
        let Some(board) = crate::edge::newest_usable(&self.boards, t) else {
            return (
                sleeve.clone(),
                None,
                Err("no scoreboard available at the step".to_owned()),
            );
        };
        let used = board.available_at.clone();
        match crate::edge::weights(board, sleeve, edge, t) {
            Ok(weights) => {
                let mut weighted = sleeve.clone();
                weighted.weights = weights;
                (weighted, used, Ok(()))
            }
            Err(reason) => (sleeve.clone(), used, Err(reason)),
        }
    }
}

fn iso_secs(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

impl StepRule for RuleFile {
    fn name(&self) -> &str {
        &self.rule.rules_version
    }

    fn candidates(&mut self, t: i64, coins: &BTreeMap<String, CoinView>) -> Vec<String> {
        let Some(feed) = self.tags.at(t) else {
            return Vec::new();
        };
        for name in &feed.reach.scored_only {
            *self.scored_only.entry(name.clone()).or_default() += 1;
        }
        feed.candidates()
            .filter(|a| coins.contains_key(*a))
            .cloned()
            .collect()
    }

    fn decide(&mut self, snap: &Snapshot, books: &Books) -> Vec<Intent> {
        let tags = Arc::clone(&self.tags);
        let feed: Option<&TagsAt> = tags.at(snap.t);
        self.steps += 1;
        self.steps_with_feed += usize::from(feed.is_some());
        self.candidates += snap.candidates.len();
        let mut intents = Vec::new();
        let sleeves = self.rule.sleeves.clone();
        for fixed in &sleeves {
            let book = books.sleeve(&fixed.name);
            if !fixed.active(!book.holdings.is_empty()) {
                continue;
            }
            let (sleeve, board, evidence) = self.weighted(fixed, snap.t);
            let inputs = Inputs {
                snap,
                book: &book,
                tags: feed.map(|f| &f.assets),
                feed: feed.map(|f| (&f.feed, &f.reach)),
            };
            let verdict = match &evidence {
                Ok(()) => decide(&inputs, &sleeve),
                Err(reason) => Verdict::no_evidence(format!("no valid scoreboard: {reason}")),
            };
            let scored = |pair: Option<&(String, f64)>| {
                pair.map_or_else(
                    || (String::new(), String::new()),
                    |(a, s)| (a.clone(), format!("{s:.4}")),
                )
            };
            let (best_coin, best_score) = scored(verdict.best.as_ref());
            let (held_coin, held_score) = scored(verdict.held.first());
            let mut action = Vec::new();
            for a in &verdict.actions {
                match a {
                    Action::Sell { asset, .. } => {
                        action.push(format!("sell {asset}"));
                        intents.push(Intent::Sell {
                            sleeve: sleeve.name.clone(),
                            asset: asset.clone(),
                            reason: SELL_REASON,
                        });
                    }
                    Action::Buy { asset, .. } => {
                        action.push(format!("buy {asset}"));
                        intents.push(Intent::Buy {
                            sleeve: sleeve.name.clone(),
                            asset: asset.clone(),
                            equity_fraction: sleeve.share,
                        });
                    }
                }
            }
            let action = match (&verdict.blocked, action.is_empty()) {
                (Some(_), _) => "hold (no evidence)".to_owned(),
                (None, true) => "hold".to_owned(),
                (None, false) => action.join(" + "),
            };
            let log = self.sleeves.entry(sleeve.name.clone()).or_default();
            log.decided += 1;
            log.blocked += usize::from(verdict.blocked.is_some());
            log.risk_off += usize::from(verdict.risk_off);
            log.rows.push(format!(
                "{},{},{},{},{best_coin},{best_score},{held_coin},{held_score},{:.4},{},{action}",
                iso_secs(snap.t),
                feed.map_or_else(String::new, |f| iso_secs(f.as_of_ms)),
                snap.candidates.len(),
                board.unwrap_or_default(),
                verdict.market_score + 0.0,
                verdict.risk_off,
            ));
        }
        intents
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = vec![format!(
            "tags feed visible at {} of {} steps; {:.1} candidates per step on average",
            self.steps_with_feed,
            self.steps,
            self.candidates as f64 / self.steps.max(1) as f64
        )];
        let mut scored: Vec<(&String, &usize)> = self.scored_only.iter().collect();
        scored.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let top: Vec<String> = scored
            .iter()
            .take(10)
            .map(|(name, steps)| format!("{name} {steps}"))
            .collect();
        if !top.is_empty() {
            notes.push(format!(
                "scored-only asset tags (no cached market, no proxy; never bought), {} distinct, most steps: {}",
                scored.len(),
                top.join(", ")
            ));
        }
        for sleeve in &self.rule.sleeves {
            let log = &self.sleeves[&sleeve.name];
            let mut line = format!(
                "sleeve `{}` ({sleeve}): decided at {} steps",
                sleeve.name, log.decided
            );
            if log.decided == 0 {
                line.push_str(" (no share and nothing held: never active)");
            }
            if self.rule.edge.is_some() {
                line.push_str(&format!(
                    ", held without a usable scoreboard at {}",
                    log.blocked
                ));
            }
            if sleeve.risk_off_below.is_some() {
                line.push_str(&format!(", risk-off at {}", log.risk_off));
            }
            notes.push(line);
        }
        notes
    }

    fn traces(&self) -> Vec<(String, Trace)> {
        self.rule
            .sleeves
            .iter()
            .filter_map(|s| {
                let log = &self.sleeves[&s.name];
                (log.decided > 0).then(|| {
                    let trace = Trace {
                        header: TRACE_HEADER.to_owned(),
                        rows: log.rows.clone(),
                    };
                    (s.name.clone(), trace)
                })
            })
            .collect()
    }
}

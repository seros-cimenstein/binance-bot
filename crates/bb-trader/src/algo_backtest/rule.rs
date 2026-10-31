//! `biz_algo`: the trader's algorithm ([`crate::decider`]) as a backtest rule.
//!
//! It reads only the step's [`Snapshot`], its own [`Book`] and the tags feed visible at the
//! step time.

use std::sync::Arc;

use super::data::TagsHistory;
use super::engine::{Intent, StepRule, Trace};
use crate::book::{Book, Snapshot};

/// Per-step trace header of `biz_algo`.
pub const BIZ_TRACE_HEADER: &str = "time,feed_as_of,best_coin,best_score,held_coin,held_score,action";

/// `biz_algo`: the trader's algorithm ([`crate::decider`]) on the tags feed history and
/// the market snapshot.
///
/// - Each step reads the feed visible at the step time ([`TagsHistory::at`]); without
///   history, or with none visible, the /biz/ terms are 0.
/// - The decider's sells and its one buy (all cash) become the intents; no exit plan,
///   only the score sells.
pub struct BizAlgo {
    rule: crate::decider::Rule,
    tags: Option<Arc<TagsHistory>>,
    steps: usize,
    steps_with_feed: usize,
    trace: Vec<String>,
}

impl BizAlgo {
    pub fn new(rule: crate::decider::Rule, tags: Option<Arc<TagsHistory>>) -> Self {
        Self {
            rule,
            tags,
            steps: 0,
            steps_with_feed: 0,
            trace: Vec::new(),
        }
    }
}

fn iso_secs(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

impl StepRule for BizAlgo {
    fn name(&self) -> &str {
        "biz_algo"
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        use crate::decider::{Action, Inputs, decide};
        let feed = self.tags.as_ref().and_then(|h| h.at(snap.t));
        self.steps += 1;
        self.steps_with_feed += usize::from(feed.is_some());
        let verdict = decide(
            &Inputs {
                snap,
                book,
                tags: feed.map(|f| &f.assets),
            },
            &self.rule,
        );
        let scored = |pair: Option<&(String, f64)>| {
            pair.map_or_else(|| (String::new(), String::new()), |(a, s)| (a.clone(), format!("{s:.4}")))
        };
        let (best_coin, best_score) = scored(verdict.best.as_ref());
        let (held_coin, held_score) = scored(verdict.held.first());
        let mut intents = Vec::new();
        let mut action = Vec::new();
        for a in verdict.actions {
            match a {
                Action::Sell { asset, .. } => {
                    action.push(format!("sell {asset}"));
                    intents.push(Intent::Sell {
                        asset,
                        reason: "biz_algo",
                    });
                }
                Action::Buy { asset, .. } => {
                    action.push(format!("buy {asset}"));
                    intents.push(Intent::Buy {
                        asset,
                        equity_fraction: 1.0,
                    });
                }
            }
        }
        self.trace.push(format!(
            "{},{},{best_coin},{best_score},{held_coin},{held_score},{}",
            iso_secs(snap.t),
            feed.map_or_else(String::new, |f| iso_secs(f.as_of_ms)),
            if action.is_empty() { "hold".to_string() } else { action.join(" + ") }
        ));
        intents
    }

    fn notes(&self) -> Vec<String> {
        let feeds = self.tags.as_ref().map_or_else(
            || "no tags feed history: the talk and stance terms were 0".to_string(),
            |_| format!("tags feed visible at {} of {} steps", self.steps_with_feed, self.steps),
        );
        vec![format!(
            "rule: {}; stances need {} posters; {feeds}",
            self.rule.weights, self.rule.min_posters
        )]
    }

    fn trace(&self) -> Option<Trace> {
        Some(Trace {
            header: BIZ_TRACE_HEADER.to_string(),
            rows: self.trace.clone(),
        })
    }
}

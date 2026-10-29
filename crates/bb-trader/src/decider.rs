//! The trader's algorithm: buy, sell or hold from the operator's fixed rule, no LLM.
//!
//! One plain function, [`decide`], used by the `biz_algo` backtest rule and by every
//! trader cycle (`Runtime::algo_verdict`). It reads the step's market
//! [`Snapshot`], the account [`Book`] and the /biz/ feed visible at that time, and scores
//! each coin:
//!
//! `score = w_talk·talk + w_stance·stance + w_momentum·momentum`
//!
//! - `talk`: the coin's /biz/ mention rate over the last 6h against its 24h rate, minus 1
//!   (0 = steady, +1 = twice the usual rate), capped to −1..3; 0 under
//!   [`SURGE_MIN_MENTIONS`] mentions in 24h or without a feed.
//! - `stance`: (bullish − bearish) / (bullish + bearish) of the 6h AI tags, −1..1; 0 when
//!   the 6h window has fewer than `min_posters` distinct posters, no tags, or no feed.
//! - `momentum`: the coin's 1h return capped to ±[`MOMENTUM_CAP_PCT`] and divided by it,
//!   −1..1.
//!
//! Weights may be negative (a negative `w_talk` fades /biz/ hype). Rules: a held coin
//! whose score falls below `sell_below` is sold; with nothing held after that, the
//! candidate (the step's buy candidates in order; live: the /biz/ feed coins with a USDT
//! market) with the highest score is bought with all cash when its score reaches
//! `buy_above`. One coin at a time; no stop-loss, take-profit or
//! exit plan, only the score.

use std::collections::BTreeMap;
use std::fmt;

use crate::biz::CoinWindows;
use crate::book::{Book, Snapshot};

/// 24h mentions a coin needs before its talk surge counts.
pub const SURGE_MIN_MENTIONS: u64 = 4;
/// 1h move that saturates the momentum term.
pub const MOMENTUM_CAP_PCT: f64 = 5.0;

/// Weights and levels of the rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub talk: f64,
    pub stance: f64,
    pub momentum: f64,
    /// A candidate is bought when its score is at least this.
    pub buy_above: f64,
    /// A held coin is sold when its score is below this.
    pub sell_below: f64,
}

impl Weights {
    /// `talk,stance,momentum,buy_above,sell_below`, e.g. `1,0.5,0.5,0.5,0`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let values: Vec<f64> = spec
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| format!("weights {spec:?}: {part:?} is not a number"))
            })
            .collect::<Result<_, _>>()?;
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
}

impl fmt::Display for Weights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "talk {:+}, stance {:+}, momentum {:+}; buy at score ≥ {:+}, sell below {:+}",
            self.talk, self.stance, self.momentum, self.buy_above, self.sell_below
        )
    }
}

/// The rule: weights plus the poster minimum for a stance to count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub weights: Weights,
    pub min_posters: u64,
}

/// One coin's score terms (before weighting).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Terms {
    pub talk: f64,
    pub stance: f64,
    pub momentum: f64,
}

impl Terms {
    pub fn score(&self, w: &Weights) -> f64 {
        w.talk * self.talk + w.stance * self.stance + w.momentum * self.momentum
    }

    fn describe(&self, w: &Weights) -> String {
        format!(
            "score {:+.3} (talk {:+.2}, stance {:+.2}, momentum {:+.2})",
            self.score(w),
            self.talk,
            self.stance,
            self.momentum
        )
    }
}

/// What the rule sees at a step.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub snap: &'a Snapshot,
    pub book: &'a Book,
    /// Coins of the /biz/ feed visible at `snap.t`; `None` without one.
    pub biz: Option<&'a BTreeMap<String, CoinWindows>>,
}

/// One order the rule asks for, with the reason in words.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Buy { asset: String, reason: String },
    Sell { asset: String, reason: String },
}

/// The rule's answer at a step: the orders (none = hold) and the scores behind them.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub actions: Vec<Action>,
    /// The best candidate and its score (computed whenever nothing is held after sells).
    pub best: Option<(String, f64)>,
    /// Each held coin and its score.
    pub held: Vec<(String, f64)>,
}

/// /biz/ talk surge of a coin (see the module docs).
pub fn talk_surge(windows: &CoinWindows) -> f64 {
    if windows.h24.mentions < SURGE_MIN_MENTIONS {
        return 0.0;
    }
    (windows.h6.mentions as f64 * 4.0 / windows.h24.mentions as f64 - 1.0).clamp(-1.0, 3.0)
}

/// /biz/ 6h stance of a coin (see the module docs).
pub fn stance(windows: &CoinWindows, min_posters: u64) -> f64 {
    let w = &windows.h6;
    let tagged = w.bullish + w.bearish;
    if w.posters < min_posters || tagged <= 0.0 {
        0.0
    } else {
        (w.bullish - w.bearish) / tagged
    }
}

fn momentum(snap: &Snapshot, asset: &str) -> f64 {
    snap.coin(asset)
        .and_then(|c| c.ret_1h)
        .map_or(0.0, |r| r.clamp(-MOMENTUM_CAP_PCT, MOMENTUM_CAP_PCT) / MOMENTUM_CAP_PCT)
}

fn biz_terms(inputs: &Inputs<'_>, asset: &str, min_posters: u64) -> (f64, f64) {
    inputs
        .biz
        .and_then(|coins| coins.get(asset))
        .map_or((0.0, 0.0), |w| (talk_surge(w), stance(w, min_posters)))
}

/// Terms of `asset` (the same whether it is bought or held).
pub fn terms(inputs: &Inputs<'_>, asset: &str, min_posters: u64) -> Terms {
    let (talk, stance) = biz_terms(inputs, asset, min_posters);
    Terms {
        talk,
        stance,
        momentum: momentum(inputs.snap, asset),
    }
}

/// Buy, sell or hold at `inputs.snap.t` (see the module docs).
pub fn decide(inputs: &Inputs<'_>, rule: &Rule) -> Verdict {
    let w = &rule.weights;
    let mut actions = Vec::new();
    let mut held = Vec::new();
    let mut kept = 0usize;
    for asset in inputs.book.holdings.keys() {
        let terms = terms(inputs, asset, rule.min_posters);
        let score = terms.score(w);
        held.push((asset.clone(), score));
        if score < w.sell_below {
            actions.push(Action::Sell {
                asset: asset.clone(),
                reason: format!("{asset} {} < sell level {:+}", terms.describe(w), w.sell_below),
            });
        } else {
            kept += 1;
        }
    }
    let mut best: Option<(String, f64, Terms)> = None;
    if kept == 0 {
        for asset in &inputs.snap.liquid_top {
            if inputs.snap.coin(asset).is_none() {
                continue;
            }
            let terms = terms(inputs, asset, rule.min_posters);
            let score = terms.score(w);
            if best.as_ref().is_none_or(|(_, b, _)| score > *b) {
                best = Some((asset.clone(), score, terms));
            }
        }
        if let Some((asset, score, terms)) = &best
            && *score >= w.buy_above
        {
            actions.push(Action::Buy {
                asset: asset.clone(),
                reason: format!("{asset} {} ≥ buy level {:+}", terms.describe(w), w.buy_above),
            });
        }
    }
    Verdict {
        actions,
        best: best.map(|(asset, score, _)| (asset, score)),
        held,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biz::Window;
    use crate::book::tests::{flat_book, holding, snap};

    fn rule(talk: f64, stance: f64, momentum: f64, buy_above: f64, sell_below: f64) -> Rule {
        Rule {
            weights: Weights {
                talk,
                stance,
                momentum,
                buy_above,
                sell_below,
            },
            min_posters: 2,
        }
    }

    fn windows(m6: u64, m24: u64, posters: u64, bullish: f64, bearish: f64) -> CoinWindows {
        CoinWindows {
            h6: Window {
                mentions: m6,
                posters,
                bullish,
                bearish,
            },
            h24: Window {
                mentions: m24,
                posters,
                bullish,
                bearish,
            },
            ..Default::default()
        }
    }

    #[test]
    fn biz_terms_need_enough_talk_and_posters() {
        // 6 mentions in 6h against 12 in 24h: twice the usual rate.
        assert!((talk_surge(&windows(6, 12, 3, 0.0, 0.0)) - 1.0).abs() < 1e-12);
        assert_eq!(talk_surge(&windows(3, 3, 3, 0.0, 0.0)), 0.0);
        assert!((stance(&windows(4, 8, 3, 3.0, 1.0), 2) - 0.5).abs() < 1e-12);
        assert_eq!(stance(&windows(4, 8, 1, 3.0, 1.0), 2), 0.0);
        assert_eq!(stance(&windows(4, 8, 3, 0.0, 0.0), 2), 0.0);
    }

    #[test]
    fn buys_the_best_scored_candidate_only_above_the_buy_level() {
        // SOL leads on 1h, ETH is surging and bullish on /biz/.
        let snap = snap(&[("BTC", 0.0, 0.5, 1.0, 9e9), ("ETH", 0.0, 0.2, 1.0, 5e9), ("SOL", 0.0, 2.5, 1.0, 1e9)]);
        let biz = BTreeMap::from([("ETH".to_string(), windows(6, 12, 3, 3.0, 1.0))]);
        let book = flat_book();
        let inputs = Inputs { snap: &snap, book: &book, biz: Some(&biz) };
        // talk 1 + stance 0.5 + 0.04 momentum beats SOL's 0.5 momentum.
        let verdict = decide(&inputs, &rule(1.0, 1.0, 1.0, 1.0, 0.0));
        assert_eq!(verdict.best.as_ref().map(|(a, _)| a.as_str()), Some("ETH"));
        assert!(matches!(&verdict.actions[..], [Action::Buy { asset, .. }] if asset == "ETH"));
        // Without the feed only momentum is left, and 0.5 is under the level.
        let blind = Inputs { biz: None, ..inputs };
        let verdict = decide(&blind, &rule(1.0, 1.0, 1.0, 1.0, 0.0));
        assert_eq!(verdict.best.as_ref().map(|(a, _)| a.as_str()), Some("SOL"));
        assert!(verdict.actions.is_empty());
        // A negative talk weight fades the hype.
        let verdict = decide(&inputs, &rule(-1.0, 0.0, 1.0, 0.1, 0.0));
        assert!(matches!(&verdict.actions[..], [Action::Buy { asset, .. }] if asset == "SOL"));
    }

    #[test]
    fn a_held_coin_is_sold_below_the_sell_level_and_otherwise_blocks_buys() {
        let snap = snap(&[("BTC", 0.0, -2.5, 1.0, 9e9), ("SOL", 0.0, 4.0, 1.0, 1e9)]);
        let book = holding(&["BTC"]);
        let inputs = Inputs { snap: &snap, book: &book, biz: None };
        // BTC scores −0.5: sold, and SOL (0.8) is bought in the same step.
        let verdict = decide(&inputs, &rule(0.0, 0.0, 1.0, 0.5, 0.0));
        assert_eq!(verdict.held, vec![("BTC".to_string(), -0.5)]);
        assert!(matches!(
            &verdict.actions[..],
            [Action::Sell { asset: s, .. }, Action::Buy { asset: b, .. }] if s == "BTC" && b == "SOL"
        ));
        // Above the sell level BTC is kept and nothing else is scored.
        let verdict = decide(&inputs, &rule(0.0, 0.0, 1.0, 0.5, -1.0));
        assert!(verdict.actions.is_empty() && verdict.best.is_none());
    }

    #[test]
    fn weights_parse_five_numbers_with_sell_not_above_buy() {
        let w = Weights::parse("1, -0.5, 0.5, 0.5, 0").unwrap();
        assert_eq!((w.talk, w.stance, w.momentum, w.buy_above, w.sell_below), (1.0, -0.5, 0.5, 0.5, 0.0));
        assert!(Weights::parse("1,2,3").is_err());
        // The six-number LLM-era format (with a value weight) is refused, not misread.
        assert!(Weights::parse("1,0.5,0.5,0,0.5,0").is_err());
        assert!(Weights::parse("1,0,0,0,x").is_err());
        assert!(Weights::parse("0,0,0,0.5,1").is_err());
    }
}

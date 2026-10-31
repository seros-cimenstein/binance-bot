//! The trader's algorithm: buy, sell or hold from the operator's fixed rule, no LLM.
//!
//! One plain function, [`decide`], used by the `biz_algo` backtest rule and by every
//! trader cycle (`Runtime::algo_verdict`). It reads the step's market
//! [`Snapshot`], the account [`Book`] and the asset tags of the tags feed visible at that
//! time ([`crate::tags`]), and scores each coin:
//!
//! `score = w_talk·talk + w_stance·stance + w_momentum·momentum`
//!
//! - `talk`: the coin's /biz/ mention rate over the last 6h against its 24h rate, minus 1
//!   (0 = steady, +1 = twice the usual rate), capped to −1..3; 0 under
//!   [`SURGE_MIN_MENTIONS`] mentions in 24h or without a feed. Mentions of Binance coins
//!   are counted over every post, without AI.
//! - `stance`: (up − down) / (up + down) of the 6h AI tags (bullish, bearish), −1..1; 0 when
//!   the 6h window has fewer than `min_posters` distinct posters, no tags, or no feed.
//! - `momentum`: the coin's 1h return capped to ±[`MOMENTUM_CAP_PCT`] and divided by it,
//!   −1..1.
//!
//! Weights may be negative (a negative `w_talk` fades /biz/ hype). Rules: a held coin
//! whose score falls below `sell_below` is sold; with nothing held after that, the
//! candidate (the step's buy candidates in order; live: the feed's asset tags with a USDT
//! market) with the highest score is bought with all cash when its score reaches
//! `buy_above`. One coin at a time; no stop-loss, take-profit or
//! exit plan, only the score.

use std::collections::BTreeMap;
use std::fmt;

use crate::book::{Book, Snapshot};
use crate::tags::Windows;

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
    /// Asset tags of the tags feed visible at `snap.t`, by asset; `None` without a feed.
    pub tags: Option<&'a BTreeMap<String, Windows>>,
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
pub fn talk_surge(windows: &Windows) -> f64 {
    if windows.h24.mentions < SURGE_MIN_MENTIONS {
        return 0.0;
    }
    (windows.h6.mentions as f64 * 4.0 / windows.h24.mentions as f64 - 1.0).clamp(-1.0, 3.0)
}

/// /biz/ 6h stance of a coin (see the module docs).
pub fn stance(windows: &Windows, min_posters: u64) -> f64 {
    let w = &windows.h6;
    let tagged = w.up + w.down;
    if w.posters < min_posters || tagged <= 0.0 {
        0.0
    } else {
        (w.up - w.down) / tagged
    }
}

fn momentum(snap: &Snapshot, asset: &str) -> f64 {
    snap.coin(asset)
        .and_then(|c| c.ret_1h)
        .map_or(0.0, |r| r.clamp(-MOMENTUM_CAP_PCT, MOMENTUM_CAP_PCT) / MOMENTUM_CAP_PCT)
}

fn tag_terms(inputs: &Inputs<'_>, asset: &str, min_posters: u64) -> (f64, f64) {
    inputs
        .tags
        .and_then(|assets| assets.get(asset))
        .map_or((0.0, 0.0), |w| (talk_surge(w), stance(w, min_posters)))
}

/// Terms of `asset` (the same whether it is bought or held).
pub fn terms(inputs: &Inputs<'_>, asset: &str, min_posters: u64) -> Terms {
    let (talk, stance) = tag_terms(inputs, asset, min_posters);
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
    use crate::book::tests::{flat_book, holding, snap};
    use crate::tags::Window;

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

    fn windows(m6: u64, m24: u64, posters: u64, up: f64, down: f64) -> Windows {
        let window = |mentions| Window {
            mentions,
            posters,
            up,
            down,
            ..Default::default()
        };
        Windows {
            h6: window(m6),
            h24: window(m24),
            ..Default::default()
        }
    }

    #[test]
    fn tag_terms_need_enough_talk_and_posters() {
        // 6 mentions in 6h against 12 in 24h: twice the usual rate.
        assert!((talk_surge(&windows(6, 12, 3, 0.0, 0.0)) - 1.0).abs() < 1e-12);
        assert_eq!(talk_surge(&windows(3, 3, 3, 0.0, 0.0)), 0.0);
        assert!((stance(&windows(4, 8, 3, 3.0, 1.0), 2) - 0.5).abs() < 1e-12);
        assert_eq!(stance(&windows(4, 8, 1, 3.0, 1.0), 2), 0.0);
        assert_eq!(stance(&windows(4, 8, 3, 0.0, 0.0), 2), 0.0);
    }

    /// Real coin windows of the old /biz/ coin feed (biz-app on the archive at
    /// 2026-10-25T13:00:00Z, the trader's fixture until Phase 53): per `1h`, `6h`, `24h` the
    /// AI-free mentions, posters, and the AI's bullish and bearish weights; then the talk and
    /// stance the old reader gave them (`algo-2026-10-29`, 2 posters).
    #[allow(clippy::type_complexity)]
    const V1_COINS: [(&str, [(u64, u64, f64, f64); 3], f64, f64); 9] = [
        ("BTC", [(0, 0, 0.0, 1.0), (7, 7, 6.0, 3.0), (34, 32, 19.0, 36.0)], -3.0 / 17.0, 1.0 / 3.0),
        ("ETH", [(1, 1, 0.0, 2.0), (6, 6, 4.0, 3.0), (10, 9, 7.0, 3.0)], 1.4, 1.0 / 7.0),
        ("ICP", [(0, 0, 0.0, 0.0), (3, 3, 0.0, 7.0), (4, 4, 2.0, 11.0)], 2.0, -1.0),
        ("LINK", [(1, 1, 1.0, 0.0), (4, 4, 5.0, 10.0), (10, 10, 13.0, 17.0)], 0.6, -1.0 / 3.0),
        ("QNT", [(1, 1, 0.0, 4.0), (9, 7, 11.0, 10.0), (13, 9, 17.0, 13.0)], 23.0 / 13.0, 1.0 / 21.0),
        ("DOGE", [(0, 0, 0.0, 0.0), (0, 0, 0.0, 0.0), (5, 5, 1.0, 4.0)], -1.0, 0.0),
        // Under 4 mentions in 24h: no talk; stance from 3 posters.
        ("HBAR", [(0, 0, 0.0, 0.0), (3, 3, 5.0, 0.0), (3, 3, 5.0, 0.0)], 0.0, 1.0),
        // AI tags on posts the AI-free count never matched: no mentions, no posters, so 0.
        ("DGB", [(0, 0, 0.0, 0.0), (0, 0, 2.0, 0.0), (0, 0, 3.0, 0.0)], 0.0, 0.0),
        // One poster: no stance.
        ("AVAX", [(1, 1, 0.0, 2.0), (1, 1, 0.0, 3.0), (1, 1, 0.0, 3.0)], 0.0, 0.0),
    ];

    #[test]
    fn tags_asset_windows_give_the_old_feeds_talk_and_stance() {
        // The same coins as `tags-feed-v1` asset tags: biz-app keeps the AI-free mentions and
        // posters of Binance coins and writes bullish / bearish as up / down. The longer
        // windows, threads and horizons carry other numbers the rule must not read.
        let window = |(mentions, posters, up, down): (u64, u64, f64, f64)| {
            serde_json::json!({"mentions": mentions, "posters": posters, "threads": posters, "up": up, "down": down,
                "horizon": {"short": up, "swing": 0.0, "long": down, "unstated": 1.0}, "posts": []})
        };
        let tags: serde_json::Map<String, serde_json::Value> = V1_COINS
            .iter()
            .map(|(coin, [h1, h6, h24], _, _)| {
                let wide = (h24.0 * 7 + 40, h24.1 * 3 + 5, h24.2 * 9.0 + 8.0, h24.3 * 2.0);
                (
                    format!("asset:{coin}"),
                    serde_json::json!({"kind": "asset", "name": coin, "first_seen": "2026-10-01T00:00:00Z",
                        "live_since": "2026-10-02T00:00:00Z",
                        "windows": {"1h": window(*h1), "6h": window(*h6), "24h": window(*h24),
                                    "7d": window(wide), "30d": window(wide)}}),
                )
            })
            .collect();
        let text = serde_json::json!({"schema": "tags-feed-v1", "as_of": "2026-10-25T13:00:00Z",
            "windows": ["1h", "6h", "24h", "7d", "30d"], "sources": [], "tags": tags, "members": {}, "links": {}})
        .to_string();
        let as_of_ms = 1_792_933_200_000; // 2026-10-25T13:00:00Z
        let feed = crate::tags::parse(&text, as_of_ms + 5 * 60_000, 45, |_| true, &Default::default()).unwrap();
        let snap = snap(&[("BTC", 0.0, 1.0, 1.0, 9e9)]);
        let book = flat_book();
        let inputs = Inputs { snap: &snap, book: &book, tags: Some(&feed.assets) };
        for (coin, _, talk, stance) in V1_COINS {
            let terms = terms(&inputs, coin, 2);
            assert!((terms.talk - talk).abs() < 1e-12, "{coin} talk {} != {talk}", terms.talk);
            assert!((terms.stance - stance).abs() < 1e-12, "{coin} stance {} != {stance}", terms.stance);
        }
    }

    #[test]
    fn buys_the_best_scored_candidate_only_above_the_buy_level() {
        // SOL leads on 1h, ETH is surging and bullish on /biz/.
        let snap = snap(&[("BTC", 0.0, 0.5, 1.0, 9e9), ("ETH", 0.0, 0.2, 1.0, 5e9), ("SOL", 0.0, 2.5, 1.0, 1e9)]);
        let tags = BTreeMap::from([("ETH".to_string(), windows(6, 12, 3, 3.0, 1.0))]);
        let book = flat_book();
        let inputs = Inputs { snap: &snap, book: &book, tags: Some(&tags) };
        // talk 1 + stance 0.5 + 0.04 momentum beats SOL's 0.5 momentum.
        let verdict = decide(&inputs, &rule(1.0, 1.0, 1.0, 1.0, 0.0));
        assert_eq!(verdict.best.as_ref().map(|(a, _)| a.as_str()), Some("ETH"));
        assert!(matches!(&verdict.actions[..], [Action::Buy { asset, .. }] if asset == "ETH"));
        // Without the feed only momentum is left, and 0.5 is under the level.
        let blind = Inputs { tags: None, ..inputs };
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
        let inputs = Inputs { snap: &snap, book: &book, tags: None };
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

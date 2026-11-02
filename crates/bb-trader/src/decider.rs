//! The trader's algorithm: buy, sell or hold from the operator's rule file, no LLM.
//!
//! One plain function, [`decide`], used by the backtest (`algo_backtest::rule`) and by every
//! trader cycle (`Runtime::algo_verdict`). It reads the step's market
//! [`Snapshot`], the account [`Book`] and the tags feed visible at that time
//! ([`crate::tags`]), and scores each coin with a sleeve of the operator's rule
//! ([`crate::rules`]):
//!
//! `score = Σ weight(name) · value(name)`
//!
//! over the sleeve's weighted per-coin signal names ([`crate::signals`]). Market-wide
//! names (`market.*`, the same for every coin) never enter a coin's score: they only make
//! the sleeve's market score, which `risk_off_below` compares (a step-aside switch that
//! does not change which coin is picked; operator 2026-11-02). The first algorithm rule,
//! `algo-2026-10-29` (no longer run), weighted `talk.6h` 0, `stance.6h` 1 and `momentum.1h` 1:
//!
//! - `talk.6h`: the coin's /biz/ mention rate over the last 6h against its 24h rate,
//!   minus 1 (0 = steady, +1 = twice the usual rate), capped to −1..3; 0 under
//!   [`SURGE_MIN_MENTIONS`](crate::signals::SURGE_MIN_MENTIONS) mentions in 24h or
//!   without a feed. Mentions of Binance coins are counted over every post, without AI.
//! - `stance.6h`: (up − down) / (up + down) of the 6h AI tags (bullish, bearish), −1..1;
//!   0 when the 6h window has fewer than the sleeve's `min_posters` distinct posters, no
//!   tags, or no feed.
//! - `momentum.1h`: the coin's 1h return capped to
//!   ±[`MOMENTUM_CAP_PCT`](crate::signals::MOMENTUM_CAP_PCT) and divided by it, −1..1.
//!
//! Without a feed every tag signal is 0. Weights may be negative (a negative `talk.6h`
//! weight fades /biz/ hype). Rules: a held coin whose score falls below `sell_below` is
//! sold; with nothing held after that, the candidate (the step's buy candidates in order;
//! live: the feed's asset tags with a USDT market) with the highest score is bought when
//! its score reaches `buy_above` (live, for the sleeve's share of the portfolio:
//! `cycle::Sizing`; the backtest sizes the same way). One coin at a time per sleeve; a sleeve's `horizon` filters its candidates and below
//! its `risk_off_below` it moves to a defensive asset ([`decide`]); no stop-loss,
//! take-profit or exit plan, only the score.

use std::collections::BTreeMap;

use crate::book::{Book, Snapshot};
use crate::reach::Reach;
use crate::rules::{Horizon, Sleeve};
use crate::signals::{Name, Signals, own_value};
use crate::tags::{Feed, Windows};

/// What the rule sees at a step.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub snap: &'a Snapshot,
    pub book: &'a Book,
    /// Asset tags of the tags feed visible at `snap.t`, by asset; `None` without a feed.
    pub tags: Option<&'a BTreeMap<String, Windows>>,
    /// The whole visible feed and its reach, for the tag and market names; `None` without
    /// a feed.
    pub feed: Option<(&'a Feed, &'a Reach)>,
}

/// One coin's weighted signals and its score.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub score: f64,
    /// The value of each weighted name, in the sleeve's order.
    pub values: Vec<(Name, f64)>,
    /// `weight × value` of each, in the same order.
    pub terms: Vec<f64>,
}

/// Terms an order's reason names (Phase 53 D6: the reason is never trimmed from a row).
pub const REASON_TERMS: usize = 8;

impl Scored {
    /// The score and its terms: every term up to [`REASON_TERMS`], else the
    /// [`REASON_TERMS`] largest `|weight × value|` in the sleeve's order and how many more.
    fn describe(&self) -> String {
        let mut shown: Vec<usize> = (0..self.values.len()).collect();
        if shown.len() > REASON_TERMS {
            shown.sort_by(|a, b| self.terms[*b].abs().total_cmp(&self.terms[*a].abs()));
            shown.truncate(REASON_TERMS);
            shown.sort_unstable();
        }
        let mut values: Vec<String> = shown
            .iter()
            .map(|i| {
                let (name, value) = &self.values[*i];
                format!("{name} {value:+.2}")
            })
            .collect();
        if self.values.len() > shown.len() {
            values.push(format!("{} more", self.values.len() - shown.len()));
        }
        format!("score {:+.3} ({})", self.score, values.join(", "))
    }
}

/// Scores coins with one sleeve's weights.
pub struct Scorer<'a> {
    inputs: Inputs<'a>,
    sleeve: &'a Sleeve,
    signals: Option<Signals<'a>>,
}

impl<'a> Scorer<'a> {
    pub fn new(inputs: Inputs<'a>, sleeve: &'a Sleeve) -> Self {
        let signals = inputs
            .feed
            .map(|(feed, reach)| Signals::new(feed, reach, sleeve.min_posters));
        Self {
            inputs,
            sleeve,
            signals,
        }
    }

    /// The weighted per-coin signals of `asset` (the same whether it is bought or held);
    /// market-wide names are left out (see the module docs).
    pub fn score(&self, asset: &str) -> Scored {
        let own = self.inputs.tags.and_then(|assets| assets.get(asset));
        let coin = self.inputs.snap.coin(asset);
        let mut score = 0.0;
        let mut terms = Vec::with_capacity(self.sleeve.weights.len());
        let values = self
            .sleeve
            .weights
            .iter()
            .filter(|(name, _)| !name.is_market())
            .map(|(name, weight)| {
                let value = match &self.signals {
                    Some(signals) => signals.value(name, asset, own, coin),
                    None => own_value(name, own, coin, self.sleeve.min_posters).unwrap_or(0.0),
                };
                score += weight * value;
                terms.push(weight * value);
                (name.clone(), value)
            })
            .collect();
        Scored {
            score,
            values,
            terms,
        }
    }

    /// The sleeve's market-wide score: `Σ weight × value` over its weighted `market.*`
    /// names (0 without a feed or without such names).
    pub fn market_score(&self) -> f64 {
        let Some(signals) = &self.signals else {
            return 0.0;
        };
        self.sleeve
            .weights
            .iter()
            .filter(|(name, _)| name.is_market())
            .map(|(name, weight)| weight * signals.value(name, "", None, None))
            .sum::<f64>()
            + 0.0 // an empty sum is −0.0; record 0
    }

    /// Every market-wide signal (`market.<kind>.<signal>[.<window>]`, weighted or not) with
    /// the sleeve's poster minimum, as its decision row records them; empty without a feed.
    pub fn market_signals(&self) -> BTreeMap<String, f64> {
        let Some(signals) = &self.signals else {
            return BTreeMap::new();
        };
        Name::all()
            .into_iter()
            .filter(|name| name.is_market())
            .map(|name| (name.to_string(), signals.value(&name, "", None, None) + 0.0))
            .collect()
    }
}

/// The horizons a coin's own asset tag leans to: the largest of short, swing and long over
/// its 7d window (all tied ones); none without a stated horizon (operator, 2026-10-31).
pub fn dominant_horizons(windows: Option<&Windows>) -> Vec<Horizon> {
    let Some(mix) = windows.map(|w| w.d7.horizon) else {
        return Vec::new();
    };
    let parts = [
        (Horizon::Short, mix.short),
        (Horizon::Swing, mix.swing),
        (Horizon::Long, mix.long),
    ];
    let top = parts.iter().map(|(_, v)| *v).fold(0.0, f64::max);
    if top <= 0.0 {
        return Vec::new();
    }
    parts
        .into_iter()
        .filter(|(_, v)| *v >= top)
        .map(|(h, _)| h)
        .collect()
}

/// Whether `asset` may be bought by `sleeve`: any coin for `horizon` `any`, else coins whose
/// own tag leans to the sleeve's horizon ([`dominant_horizons`]).
fn fits(inputs: &Inputs<'_>, sleeve: &Sleeve, asset: &str) -> bool {
    sleeve.horizon == Horizon::Any
        || dominant_horizons(inputs.tags.and_then(|t| t.get(asset))).contains(&sleeve.horizon)
}

/// One order the rule asks for, with the reason in words.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Buy { asset: String, reason: String },
    Sell { asset: String, reason: String },
}

/// The rule's answer at a step: the orders (none = hold) and the scores behind them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verdict {
    pub actions: Vec<Action>,
    /// The best candidate and its score (computed whenever nothing is held after sells;
    /// risk-off: the defensive target).
    pub best: Option<(String, f64)>,
    /// Each held coin and its score.
    pub held: Vec<(String, f64)>,
    /// Why the rule could not score at all (an edge rule without a valid scoreboard): no
    /// orders, nothing scored.
    pub blocked: Option<String>,
    /// The sleeve's market-wide score ([`Scorer::market_score`]).
    pub market_score: f64,
    /// The market-wide score was below the sleeve's `risk_off_below`.
    pub risk_off: bool,
}

impl Verdict {
    /// Hold without scoring: no orders, `reason` recorded (an edge rule without valid
    /// evidence, [`crate::edge`]).
    pub fn no_evidence(reason: String) -> Self {
        Self {
            blocked: Some(reason),
            ..Self::default()
        }
    }
}

/// Buy, sell or hold at `inputs.snap.t` with `sleeve` (see the module docs): the sleeve's
/// holdings are `inputs.book`; candidates outside its horizon are skipped; below its
/// `risk_off_below` it moves to its best defensive asset instead (see `risk_off`).
pub fn decide(inputs: &Inputs<'_>, sleeve: &Sleeve) -> Verdict {
    let scorer = Scorer::new(*inputs, sleeve);
    let market_score = scorer.market_score();
    if let Some(level) = sleeve.risk_off_below
        && market_score < level
    {
        return risk_off(inputs, sleeve, &scorer, market_score, level);
    }
    let mut actions = Vec::new();
    let mut held = Vec::new();
    let mut kept = 0usize;
    for asset in inputs.book.holdings.keys() {
        let scored = scorer.score(asset);
        held.push((asset.clone(), scored.score));
        if scored.score < sleeve.sell_below {
            actions.push(Action::Sell {
                asset: asset.clone(),
                reason: format!(
                    "{asset} {} < sell level {:+}",
                    scored.describe(),
                    sleeve.sell_below
                ),
            });
        } else {
            kept += 1;
        }
    }
    let mut best: Option<(String, Scored)> = None;
    if kept == 0 {
        for asset in &inputs.snap.candidates {
            if inputs.snap.coin(asset).is_none() || !fits(inputs, sleeve, asset) {
                continue;
            }
            let scored = scorer.score(asset);
            if best.as_ref().is_none_or(|(_, b)| scored.score > b.score) {
                best = Some((asset.clone(), scored));
            }
        }
        if let Some((asset, scored)) = &best
            && scored.score >= sleeve.buy_above
        {
            actions.push(Action::Buy {
                asset: asset.clone(),
                reason: format!(
                    "{asset} {} ≥ buy level {:+}",
                    scored.describe(),
                    sleeve.buy_above
                ),
            });
        }
    }
    Verdict {
        actions,
        best: best.map(|(asset, scored)| (asset, scored.score)),
        held,
        market_score,
        ..Verdict::default()
    }
}

/// Risk-off (operator "Yes" 2026-10-30; rule 2026-10-31): the target is the sleeve's
/// defensive asset with the highest score among those priced ([`Scorer::score`], which
/// leaves the market-wide names out; USDT scores 0; ties go to the earlier one listed); a
/// holding that is not the target is sold and the target bought (no buy level).
/// Nothing else is bought.
fn risk_off(
    inputs: &Inputs<'_>,
    sleeve: &Sleeve,
    scorer: &Scorer<'_>,
    market_score: f64,
    level: f64,
) -> Verdict {
    let mut target: Option<(String, f64)> = None;
    for asset in &sleeve.defensive {
        let score = if asset == "USDT" {
            0.0
        } else if inputs.snap.coin(asset).is_some() {
            scorer.score(asset).score
        } else {
            continue;
        };
        if target.as_ref().is_none_or(|(_, best)| score > *best) {
            target = Some((asset.clone(), score));
        }
    }
    let (to, to_score) = target.unwrap_or_else(|| ("USDT".to_owned(), 0.0));
    let why = format!(
        "risk-off: market score {market_score:+.3} < risk-off level {level:+}; defensive {to} (score {to_score:+.3})"
    );
    let mut actions = Vec::new();
    let mut held = Vec::new();
    for asset in inputs.book.holdings.keys() {
        held.push((asset.clone(), scorer.score(asset).score));
        if *asset != to {
            actions.push(Action::Sell {
                asset: asset.clone(),
                reason: format!("{asset}: {why}"),
            });
        }
    }
    if to != "USDT" && !inputs.book.holdings.contains_key(&to) {
        actions.push(Action::Buy {
            asset: to.clone(),
            reason: format!("{to}: {why}"),
        });
    }
    Verdict {
        actions,
        best: Some((to, to_score)),
        held,
        market_score,
        risk_off: true,
        ..Verdict::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_names_at_most_its_largest_terms() {
        let names = ["talk.6h", "stance.6h", "momentum.1h", "momentum.24h", "momentum.7d",
            "asset.direction.24h", "asset.level.24h", "asset.surge.6h", "asset.breadth.7d", "asset.novelty"];
        let scored = |n: usize| {
            let values: Vec<(Name, f64)> = names[..n].iter().enumerate()
                .map(|(i, name)| (name.parse().unwrap(), i as f64 / 10.0)).collect();
            // Weight 1: each term is its value.
            let terms: Vec<f64> = values.iter().map(|(_, v)| *v).collect();
            Scored { score: terms.iter().sum(), values, terms }
        };
        // Up to REASON_TERMS terms: all of them, in the rule's order (the shipped rule's 3).
        assert_eq!(scored(3).describe(), "score +0.300 (talk.6h +0.00, stance.6h +0.10, momentum.1h +0.20)");
        // More: the largest |weight × value| in the rule's order, and how many are left out.
        let text = scored(10).describe();
        assert!(text.starts_with("score +4.500 (momentum.1h +0.20, "), "{text}");
        assert!(text.ends_with("asset.novelty +0.90, 2 more)"), "{text}");
        assert!(!text.contains("talk.6h") && !text.contains("stance.6h"), "{text}");
    }
    use crate::book::tests::{flat_book, holding, snap};
    use crate::rules::Rule;
    use crate::rules::tests::{SHIPPED, one_sleeve};
    use crate::signals::{Span, momentum, stance, talk};
    use crate::tags::Window;

    /// A one-sleeve rule with the legacy names' weights and the levels, 2 posters.
    fn sleeve(talk: f64, stance: f64, momentum: f64, buy_above: f64, sell_below: f64) -> Sleeve {
        let text = one_sleeve(
            &[("talk.6h", talk), ("stance.6h", stance), ("momentum.1h", momentum)],
            buy_above,
            sell_below,
        );
        Rule::parse(&text).unwrap().trading_sleeve().clone()
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

    /// The pre-rule-file decider (`algo-2026-10-29` as five numbers
    /// `talk,stance,momentum,buy_above,sell_below`): sides and assets of its orders, its best
    /// candidate and held scores.
    #[allow(clippy::type_complexity)]
    fn old_decide(
        inputs: &Inputs<'_>,
        [w_talk, w_stance, w_momentum, buy_above, sell_below]: [f64; 5],
        min_posters: u64,
    ) -> (Vec<(&'static str, String)>, Option<(String, f64)>, Vec<(String, f64)>) {
        let score = |asset: &str| {
            let own = inputs.tags.and_then(|assets| assets.get(asset));
            w_talk * own.map_or(0.0, talk)
                + w_stance * own.map_or(0.0, |w| stance(w, min_posters))
                + w_momentum * momentum(inputs.snap.coin(asset), Span::H1)
        };
        let (mut actions, mut held, mut kept) = (Vec::new(), Vec::new(), 0);
        for asset in inputs.book.holdings.keys() {
            let s = score(asset);
            held.push((asset.clone(), s));
            if s < sell_below {
                actions.push(("sell", asset.clone()));
            } else {
                kept += 1;
            }
        }
        let mut best: Option<(String, f64)> = None;
        if kept == 0 {
            for asset in &inputs.snap.candidates {
                if inputs.snap.coin(asset).is_none() {
                    continue;
                }
                let s = score(asset);
                if best.as_ref().is_none_or(|(_, b)| s > *b) {
                    best = Some((asset.clone(), s));
                }
            }
            if let Some((asset, s)) = &best
                && *s >= buy_above
            {
                actions.push(("buy", asset.clone()));
            }
        }
        (actions, best, held)
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

    /// The V1 coins as a parsed `tags-feed-v1` (biz-app keeps the AI-free mentions and
    /// posters of Binance coins and writes bullish / bearish as up / down; the longer
    /// windows, threads and horizons carry other numbers the rule must not read).
    fn v1_feed() -> crate::tags::TagsFeed {
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
        crate::tags::parse(&text, as_of_ms + 5 * 60_000, 45, |_| true, &Default::default()).unwrap()
    }

    #[test]
    fn tags_asset_windows_give_the_old_feeds_talk_and_stance() {
        let feed = v1_feed();
        let snap = snap(&[("BTC", 0.0, 1.0, 1.0, 9e9)]);
        let book = flat_book();
        let inputs = Inputs { snap: &snap, book: &book, tags: Some(&feed.assets), feed: Some((&feed.feed, &feed.reach)) };
        let both = sleeve(1.0, 1.0, 0.0, 0.5, 0.0);
        let scorer = Scorer::new(inputs, &both);
        for (coin, _, talk, stance) in V1_COINS {
            let values = scorer.score(coin).values;
            let value = |name: &str| values.iter().find(|(n, _)| n.to_string() == name).unwrap().1;
            assert!((value("talk.6h") - talk).abs() < 1e-12, "{coin} talk {} != {talk}", value("talk.6h"));
            assert!((value("stance.6h") - stance).abs() < 1e-12, "{coin} stance {} != {stance}", value("stance.6h"));
        }
    }

    #[test]
    fn the_rule_file_gives_the_old_formulas_scores_and_actions() {
        // The decider's cases: a flat book with ETH surging and bullish and SOL leading on
        // 1h; BTC held and falling; the old /biz/ coins with mixed moves; with and without a
        // feed and with the full feed (tag signals available but unweighted).
        let feed = v1_feed();
        let surging = BTreeMap::from([("ETH".to_string(), windows(6, 12, 3, 3.0, 1.0))]);
        let flat = snap(&[("BTC", 0.0, 0.5, 1.0, 9e9), ("ETH", 0.0, 0.2, 1.0, 5e9), ("SOL", 0.0, 2.5, 1.0, 1e9)]);
        let falling = snap(&[("BTC", 0.0, -2.5, 1.0, 9e9), ("SOL", 0.0, 4.0, 1.0, 1e9)]);
        let v1 = snap(&[
            ("BTC", 0.0, 0.4, 1.0, 9e9),
            ("ETH", 0.0, -1.2, 1.0, 5e9),
            ("ICP", 0.0, 3.1, 1.0, 1e9),
            ("LINK", 0.0, 0.9, 1.0, 1e9),
            ("QNT", 0.0, -0.3, 1.0, 1e9),
            ("DOGE", 0.0, 6.0, 1.0, 1e9),
            ("HBAR", 0.0, 0.1, 1.0, 1e9),
            ("DGB", 0.0, -4.0, 1.0, 1e9),
            ("AVAX", 0.0, 1.7, 1.0, 1e9),
        ]);
        let (none, btc, hbar) = (flat_book(), holding(&["BTC"]), holding(&["HBAR", "QNT"]));
        let cases = [
            Inputs { snap: &flat, book: &none, tags: Some(&surging), feed: None },
            Inputs { snap: &flat, book: &none, tags: None, feed: None },
            Inputs { snap: &falling, book: &btc, tags: None, feed: None },
            Inputs { snap: &falling, book: &btc, tags: Some(&surging), feed: None },
            Inputs { snap: &v1, book: &none, tags: Some(&feed.assets), feed: Some((&feed.feed, &feed.reach)) },
            Inputs { snap: &v1, book: &btc, tags: Some(&feed.assets), feed: Some((&feed.feed, &feed.reach)) },
            Inputs { snap: &v1, book: &hbar, tags: Some(&feed.assets), feed: None },
        ];
        let shipped = Rule::load(std::path::Path::new(SHIPPED)).unwrap();
        let mut rules = vec![([0.0, 1.0, 1.0, 0.5, -0.5], shipped.trading_sleeve().clone())];
        for w in [
            [1.0, 1.0, 1.0, 1.0, 0.0],
            [-1.0, 0.0, 1.0, 0.1, 0.0],
            [0.0, 0.0, 1.0, 0.5, 0.0],
            [0.0, 0.0, 1.0, 0.5, -1.0],
            [1.0, -0.5, 0.5, 0.5, 0.0],
            [1.0, 1.0, 0.0, 0.2, 0.2],
        ] {
            rules.push((w, sleeve(w[0], w[1], w[2], w[3], w[4])));
        }
        let mut trades = 0;
        for (weights, sleeve) in &rules {
            for inputs in &cases {
                let (old_actions, old_best, old_held) = old_decide(inputs, *weights, 2);
                let verdict = decide(inputs, sleeve);
                let actions: Vec<(&str, String)> = verdict
                    .actions
                    .iter()
                    .map(|a| match a {
                        Action::Sell { asset, .. } => ("sell", asset.clone()),
                        Action::Buy { asset, .. } => ("buy", asset.clone()),
                    })
                    .collect();
                assert_eq!(actions, old_actions, "{weights:?}");
                assert_eq!(verdict.best, old_best, "{weights:?}");
                assert_eq!(verdict.held, old_held, "{weights:?}");
                trades += actions.len();
            }
        }
        // The cases cover buys, sells and holds, not only quiet steps.
        assert!(trades >= 20, "{trades}");
        // The shipped rule on the old coins: HBAR's unanimous 3-poster stance beats DOGE's
        // +6% 1h move (momentum capped at 1, talk weighted 0).
        let verdict = decide(&cases[4], shipped.trading_sleeve());
        assert!(matches!(&verdict.actions[..], [Action::Buy { asset, .. }] if asset == "HBAR"));
        assert!(verdict.best.is_some_and(|(_, score)| (score - 1.02).abs() < 1e-12));
    }

    #[test]
    fn buys_the_best_scored_candidate_only_above_the_buy_level() {
        // SOL leads on 1h, ETH is surging and bullish on /biz/.
        let snap = snap(&[("BTC", 0.0, 0.5, 1.0, 9e9), ("ETH", 0.0, 0.2, 1.0, 5e9), ("SOL", 0.0, 2.5, 1.0, 1e9)]);
        let tags = BTreeMap::from([("ETH".to_string(), windows(6, 12, 3, 3.0, 1.0))]);
        let book = flat_book();
        let inputs = Inputs { snap: &snap, book: &book, tags: Some(&tags), feed: None };
        // talk 1 + stance 0.5 + 0.04 momentum beats SOL's 0.5 momentum.
        let verdict = decide(&inputs, &sleeve(1.0, 1.0, 1.0, 1.0, 0.0));
        assert_eq!(verdict.best.as_ref().map(|(a, _)| a.as_str()), Some("ETH"));
        assert!(matches!(&verdict.actions[..], [Action::Buy { asset, .. }] if asset == "ETH"));
        // Without the feed only momentum is left, and 0.5 is under the level.
        let blind = Inputs { tags: None, ..inputs };
        let verdict = decide(&blind, &sleeve(1.0, 1.0, 1.0, 1.0, 0.0));
        assert_eq!(verdict.best.as_ref().map(|(a, _)| a.as_str()), Some("SOL"));
        assert!(verdict.actions.is_empty());
        // A negative talk weight fades the hype.
        let verdict = decide(&inputs, &sleeve(-1.0, 0.0, 1.0, 0.1, 0.0));
        assert!(matches!(&verdict.actions[..], [Action::Buy { asset, .. }] if asset == "SOL"));
    }

    #[test]
    fn a_held_coin_is_sold_below_the_sell_level_and_otherwise_blocks_buys() {
        let snap = snap(&[("BTC", 0.0, -2.5, 1.0, 9e9), ("SOL", 0.0, 4.0, 1.0, 1e9)]);
        let book = holding(&["BTC"]);
        let inputs = Inputs { snap: &snap, book: &book, tags: None, feed: None };
        // BTC scores −0.5: sold, and SOL (0.8) is bought in the same step.
        let verdict = decide(&inputs, &sleeve(0.0, 0.0, 1.0, 0.5, 0.0));
        assert_eq!(verdict.held, vec![("BTC".to_string(), -0.5)]);
        assert!(matches!(
            &verdict.actions[..],
            [Action::Sell { asset: s, .. }, Action::Buy { asset: b, .. }] if s == "BTC" && b == "SOL"
        ));
        // Above the sell level BTC is kept and nothing else is scored.
        let verdict = decide(&inputs, &sleeve(0.0, 0.0, 1.0, 0.5, -1.0));
        assert!(verdict.actions.is_empty() && verdict.best.is_none());
    }

    #[test]
    fn tag_names_score_from_the_whole_feed() {
        // ETH's own asset tag leans up over 24h (+0.4 with 9 posters), ICP's down: names
        // outside the legacy three read the whole feed through reach.
        let feed = v1_feed();
        let snap = snap(&[("ETH", 0.0, 0.0, 1.0, 5e9), ("ICP", 0.0, 0.0, 1.0, 1e9)]);
        let book = flat_book();
        let full = Inputs { snap: &snap, book: &book, tags: Some(&feed.assets), feed: Some((&feed.feed, &feed.reach)) };
        let rule = Rule::parse(&one_sleeve(&[("asset.direction.24h", 1.0)], 0.3, 0.0)).unwrap();
        let sleeve = rule.trading_sleeve();
        let scorer = Scorer::new(full, sleeve);
        // ETH 24h up 7 down 3: 0.4; ICP up 2 down 11: −9/13.
        assert!((scorer.score("ETH").score - 0.4).abs() < 1e-12);
        assert!((scorer.score("ICP").score + 9.0 / 13.0).abs() < 1e-12);
        assert!(matches!(&decide(&full, sleeve).actions[..], [Action::Buy { asset, .. }] if asset == "ETH"));
        // Without the whole feed a tag name has nothing to read: 0, no buy.
        let windows_only = Inputs { feed: None, ..full };
        assert_eq!(Scorer::new(windows_only, sleeve).score("ETH").score, 0.0);
        assert!(decide(&windows_only, sleeve).actions.is_empty());
    }

    /// A sleeve from JSON fields over the one-sleeve test rule (2 posters, USDT defensive).
    fn sleeve_with(weights: &[(&str, f64)], buy_above: f64, sell_below: f64, extra: serde_json::Value) -> Sleeve {
        let mut rule: serde_json::Value = serde_json::from_str(&one_sleeve(weights, buy_above, sell_below)).unwrap();
        for (key, value) in extra.as_object().unwrap() {
            rule["sleeves"][0][key] = value.clone();
        }
        Rule::parse(&rule.to_string()).unwrap().sleeves[0].clone()
    }

    fn mix(short: f64, swing: f64, long: f64) -> Windows {
        let mut windows = Windows::default();
        windows.d7.horizon = crate::tags::Horizon { short, swing, long, unstated: 3.0 };
        windows
    }

    #[test]
    fn a_horizon_sleeve_buys_only_coins_whose_own_tag_leans_its_way() {
        // ETH: long theses over 7d; SOL: short; ADA: swing and long tied; DOGE: none stated.
        let tags = BTreeMap::from([
            ("ETH".to_string(), mix(1.0, 2.0, 5.0)),
            ("SOL".to_string(), mix(6.0, 1.0, 0.0)),
            ("ADA".to_string(), mix(0.0, 2.0, 2.0)),
            ("DOGE".to_string(), mix(0.0, 0.0, 0.0)),
        ]);
        assert_eq!(dominant_horizons(tags.get("ADA")), [Horizon::Swing, Horizon::Long]);
        assert!(dominant_horizons(tags.get("DOGE")).is_empty() && dominant_horizons(None).is_empty());
        // SOL and DOGE lead on momentum, but a long sleeve buys only ETH or ADA.
        let snap = snap(&[("SOL", 0.0, 4.0, 1.0, 1e9), ("DOGE", 0.0, 3.5, 1.0, 1e9), ("ETH", 0.0, 1.0, 1.0, 1e9), ("ADA", 0.0, 2.0, 1.0, 1e9)]);
        let book = flat_book();
        let inputs = Inputs { snap: &snap, book: &book, tags: Some(&tags), feed: None };
        let weights = [("momentum.1h", 1.0)];
        let long = sleeve_with(&weights, 0.1, 0.0, serde_json::json!({"horizon": "long"}));
        assert!(matches!(&decide(&inputs, &long).actions[..], [Action::Buy { asset, .. }] if asset == "ADA"));
        let short = sleeve_with(&weights, 0.1, 0.0, serde_json::json!({"horizon": "short"}));
        assert!(matches!(&decide(&inputs, &short).actions[..], [Action::Buy { asset, .. }] if asset == "SOL"));
        let swing = sleeve_with(&weights, 0.1, 0.0, serde_json::json!({"horizon": "swing"}));
        assert!(matches!(&decide(&inputs, &swing).actions[..], [Action::Buy { asset, .. }] if asset == "ADA"));
        let any = sleeve_with(&weights, 0.1, 0.0, serde_json::json!({}));
        assert!(matches!(&decide(&inputs, &any).actions[..], [Action::Buy { asset, .. }] if asset == "SOL"));
        // Without a feed no coin has a horizon: a filtered sleeve buys nothing.
        assert!(decide(&Inputs { tags: None, ..inputs }, &long).actions.is_empty());
        // A held coin outside the horizon is still scored and sold.
        let held = holding(&["SOL"]);
        let selling = sleeve_with(&weights, 5.0, 5.0, serde_json::json!({"horizon": "long"}));
        let verdict = decide(&Inputs { book: &held, ..inputs }, &selling);
        assert!(matches!(&verdict.actions[..], [Action::Sell { asset, .. }] if asset == "SOL"));
    }

    #[test]
    fn risk_off_moves_the_sleeve_to_its_best_defensive_asset() {
        // The board's mood: 5 posters, up 1 down 4 (direction −0.6) or the reverse (+0.6).
        let feed = |up: f64, down: f64| {
            let text = serde_json::json!({"schema": "tags-feed-v1", "as_of": "2026-10-25T13:00:00Z",
                "tags": {"mood:fear": {"kind": "mood", "name": "fear",
                    "windows": {"24h": {"mentions": 10, "posters": 5, "up": up, "down": down}}}}})
            .to_string();
            crate::tags::parse(&text, 1_792_933_200_000 + 60_000, 45, |_| true, &Default::default()).unwrap()
        };
        let (gloomy, bright) = (feed(1.0, 4.0), feed(4.0, 1.0));
        let sleeve = sleeve_with(
            &[("market.mood.direction.24h", 1.0), ("momentum.1h", 1.0)],
            0.5,
            -0.5,
            serde_json::json!({"risk_off_below": -0.2, "defensive": ["USDT", "PAXG", "XAUT"]}),
        );
        let book = holding(&["BTC"]);
        let decide_on = |paxg: f64, xaut: f64, feed: &crate::tags::TagsFeed, book: &Book| {
            let snap = snap(&[("BTC", 0.0, 4.0, 1.0, 9e9), ("PAXG", 0.0, paxg, 1.0, 1e8), ("XAUT", 0.0, xaut, 1.0, 1e8)]);
            decide(&Inputs { snap: &snap, book, tags: Some(&feed.assets), feed: Some((&feed.feed, &feed.reach)) }, &sleeve)
        };
        let orders = |v: &Verdict| -> Vec<(String, String)> {
            v.actions
                .iter()
                .map(|a| match a {
                    Action::Sell { asset, .. } => ("sell".to_owned(), asset.clone()),
                    Action::Buy { asset, .. } => ("buy".to_owned(), asset.clone()),
                })
                .collect()
        };
        let pair = |side: &str, asset: &str| (side.to_owned(), asset.to_owned());
        // Gloomy board (market score −0.6 < −0.2): BTC is sold though its +4% scores well,
        // and the best defensive asset is bought: XAUT, then PAXG, then plain USDT.
        let to_xaut = decide_on(1.0, 2.0, &gloomy, &book);
        assert!(to_xaut.risk_off && (to_xaut.market_score + 0.6).abs() < 1e-12);
        assert_eq!(orders(&to_xaut), [pair("sell", "BTC"), pair("buy", "XAUT")]);
        assert_eq!(orders(&decide_on(3.0, 2.0, &gloomy, &book)), [pair("sell", "BTC"), pair("buy", "PAXG")]);
        let to_usdt = decide_on(-1.0, -2.0, &gloomy, &book);
        assert_eq!(orders(&to_usdt), [pair("sell", "BTC")]);
        assert_eq!(to_usdt.best.as_ref().map(|(a, _)| a.as_str()), Some("USDT"));
        // Already in the target: nothing to do.
        let in_paxg = holding(&["PAXG"]);
        assert!(decide_on(3.0, 2.0, &gloomy, &in_paxg).actions.is_empty());
        // Bright board: the normal rule keeps BTC (score 0.8: the mood is market-wide and
        // stays out of coin scores, ≥ −0.5).
        let normal = decide_on(3.0, 2.0, &bright, &book);
        assert!(!normal.risk_off && normal.actions.is_empty());
        // The market-wide name never moves a coin's score: gloomy or bright, BTC scores its
        // momentum alone, so the board's mood cannot change which coin is picked.
        let snap = snap(&[("BTC", 0.0, 4.0, 1.0, 9e9)]);
        let btc = |feed: &crate::tags::TagsFeed| {
            let inputs = Inputs {
                snap: &snap,
                book: &book,
                tags: Some(&feed.assets),
                feed: Some((&feed.feed, &feed.reach)),
            };
            let scored = Scorer::new(inputs, &sleeve).score("BTC");
            (
                scored.score,
                scored
                    .values
                    .iter()
                    .map(|(n, _)| n.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(btc(&gloomy), btc(&bright));
        assert_eq!(btc(&gloomy).1, ["momentum.1h"]);
    }
}

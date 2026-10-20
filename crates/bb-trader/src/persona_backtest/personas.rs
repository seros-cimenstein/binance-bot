//! The personas: deterministic rule sets, each documented by its doc comment.
//!
//! Every persona reads only the step's [`Snapshot`] and its own [`Book`]. Exit plans use
//! the Phase 45 menu (`ExitPlan::parse_spec`).

use super::engine::{Book, Intent, Persona, Snapshot};
use crate::error::{Result, TraderError};
use crate::exit_plan::ExitPlan;
use crate::market_mood::LEAN_SCORE;

/// The live default exit plan since Phase 47 (Pi `BOT_SPOT_DEFAULT_EXIT_PLAN`).
pub const V13_EXIT_PLAN: &str = "atr=3.5:1h,deadline=360";
/// Phase 47 finding: 85% of losing live entries came with a 4h market median under +1%.
pub const V13_MIN_MEDIAN_4H_PCT: f64 = 1.0;

fn plan(spec: &str) -> Result<ExitPlan> {
    ExitPlan::parse_spec(spec).map_err(|e| TraderError::config(format!("exit plan {spec:?}: {e}")))
}

/// Sell every holding.
fn sell_all(book: &Book, reason: &'static str) -> Vec<Intent> {
    book.holdings
        .keys()
        .map(|asset| Intent::Sell {
            asset: asset.clone(),
            reason,
        })
        .collect()
}

/// The candidate with the largest `key`, ties to the first in `candidates` order.
fn best_by<'a>(
    candidates: impl Iterator<Item = &'a String>,
    key: impl Fn(&str) -> Option<f64>,
) -> Option<&'a String> {
    let mut best: Option<(&String, f64)> = None;
    for asset in candidates {
        if let Some(value) = key(asset)
            && best.is_none_or(|(_, b)| value > b)
        {
            best = Some((asset, value));
        }
    }
    best.map(|(asset, _)| asset)
}

/// `v13`: a deterministic **proxy** of the live Bot Spot v13/v14 guidance, not the LLM
/// trader (which cannot be replayed: cost, quota and live-only tools).
///
/// - Candidates: the liquid top 10 by trailing 24h quote volume (`liquid_top_value`,
///   `BOT_SPOT_CANDIDATE_LIMIT=10`).
/// - Buy only when flat, the breadth score is at least +15 (`LEAN_SCORE`) and the 4h market
///   median is at least +1.0%: the candidate with the highest 4h return among those whose
///   1h and 4h returns are both positive, with all cash.
/// - Exit: the live default plan `atr=3.5:1h,deadline=360`, plus a sell at a step when the
///   breadth score is at most −15 ("cut failed theses fast" when the regime turns).
#[derive(Debug, Clone)]
pub struct V13 {
    plan: ExitPlan,
}

impl V13 {
    pub fn new() -> Result<Self> {
        Ok(Self {
            plan: plan(V13_EXIT_PLAN)?,
        })
    }
}

impl Persona for V13 {
    fn name(&self) -> &str {
        "v13"
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        if !book.is_flat() {
            return if snap.score.is_some_and(|s| s <= -LEAN_SCORE) {
                sell_all(book, "regime_turn")
            } else {
                Vec::new()
            };
        }
        let bullish = snap.score.is_some_and(|s| s >= LEAN_SCORE)
            && snap
                .median_return_pct("4h")
                .is_some_and(|m| m >= V13_MIN_MEDIAN_4H_PCT);
        if !bullish {
            return Vec::new();
        }
        let pick = best_by(snap.liquid_top.iter(), |asset| {
            let coin = snap.coin(asset)?;
            (coin.ret_1h? > 0.0 && coin.ret_4h? > 0.0).then_some(coin.ret_4h?)
        });
        pick.map(|asset| {
            vec![Intent::Buy {
                asset: asset.clone(),
                equity_fraction: 1.0,
            }]
        })
        .unwrap_or_default()
    }

    fn exit_plan(&self) -> Option<ExitPlan> {
        Some(self.plan)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::market_mood::HorizonBreadth;
    use crate::persona_backtest::data::{MINUTE_MS, Market};
    use crate::persona_backtest::engine::tests::{T0, cfg, coin};
    use crate::persona_backtest::engine::{CoinView, Holding, run};

    /// `(asset, 15m, 1h, 4h, 24h quote volume)`.
    pub type Row<'a> = (&'a str, f64, f64, f64, f64);

    /// A snapshot with the given breadth score, 4h median and coins; every coin is a liquid
    /// candidate in the given order.
    pub fn snap(score: f64, median_4h: f64, rows: &[Row<'_>]) -> Snapshot {
        let horizon = |name: &'static str, median: Option<f64>| HorizonBreadth {
            name,
            weight: 0.25,
            up: 0,
            down: 0,
            flat: 0,
            breadth: 0.0,
            median_return_pct: median,
        };
        Snapshot {
            t: T0,
            score: Some(score),
            horizons: vec![
                horizon("15m", None),
                horizon("1h", None),
                horizon("4h", Some(median_4h)),
                horizon("24h", None),
            ],
            coins: rows
                .iter()
                .map(|&(a, r15, r1, r4, qv)| {
                    let view = CoinView {
                        ret_15m: Some(r15),
                        ret_1h: Some(r1),
                        ret_4h: Some(r4),
                        quote_volume_24h: qv,
                    };
                    (a.to_string(), view)
                })
                .collect(),
            liquid_top: rows.iter().map(|r| r.0.to_string()).collect(),
        }
    }

    pub fn flat_book() -> Book {
        Book {
            t: T0,
            cash: 1000.0,
            equity: 1000.0,
            holdings: BTreeMap::new(),
        }
    }

    pub fn holding(assets: &[&str]) -> Book {
        Book {
            t: T0,
            cash: 0.0,
            equity: 1000.0,
            holdings: assets
                .iter()
                .map(|a| {
                    let h = Holding {
                        qty: 1.0,
                        entry_price: 100.0,
                        entry_ms: T0 - 60 * MINUTE_MS,
                        value: 1000.0 / assets.len() as f64,
                    };
                    (a.to_string(), h)
                })
                .collect(),
        }
    }

    pub fn bought(intents: &[Intent]) -> Vec<&str> {
        intents
            .iter()
            .filter_map(|i| match i {
                Intent::Buy { asset, .. } => Some(asset.as_str()),
                Intent::Sell { .. } => None,
            })
            .collect()
    }

    pub fn sold(intents: &[Intent]) -> Vec<&str> {
        intents
            .iter()
            .filter_map(|i| match i {
                Intent::Sell { asset, .. } => Some(asset.as_str()),
                Intent::Buy { .. } => None,
            })
            .collect()
    }

    const ROWS: [Row<'static>; 3] = [
        ("BTC", 0.2, 0.5, 1.5, 3e8),
        ("SOL", 0.3, 0.8, 3.0, 1e8),
        ("ETH", 0.1, -0.1, 4.0, 2e8),
    ];

    #[test]
    fn v13_buys_only_in_a_bullish_regime_with_a_strong_4h_median() {
        let mut v13 = V13::new().unwrap();
        assert!(v13.decide(&snap(14.9, 2.0, &ROWS), &flat_book()).is_empty());
        assert!(v13.decide(&snap(30.0, 0.9, &ROWS), &flat_book()).is_empty());
        let intents = v13.decide(&snap(15.0, 1.0, &ROWS), &flat_book());
        // ETH has the best 4h move but a negative 1h, so SOL is the strongest qualifier.
        assert_eq!(bought(&intents), vec!["SOL"]);
        assert_eq!(
            intents[0],
            Intent::Buy {
                asset: "SOL".into(),
                equity_fraction: 1.0
            }
        );
        // Already holding: no second buy even in a bullish regime.
        assert!(
            v13.decide(&snap(60.0, 3.0, &ROWS), &holding(&["BTC"]))
                .is_empty()
        );
    }

    #[test]
    fn v13_sells_when_the_regime_turns_bearish() {
        let mut v13 = V13::new().unwrap();
        assert!(
            v13.decide(&snap(-14.9, -1.0, &ROWS), &holding(&["SOL"]))
                .is_empty()
        );
        let intents = v13.decide(&snap(-15.0, -1.0, &ROWS), &holding(&["SOL"]));
        assert_eq!(sold(&intents), vec!["SOL"]);
        assert!(bought(&intents).is_empty());
    }

    /// Three liquid coins rising steadily (SOL fastest), with a small zig-zag so bars have a
    /// range: bullish breadth all the time.
    pub fn rising_market(minutes: i64) -> Market {
        let rise =
            |rate: f64| move |m: i64| 100.0 * (1.0 + rate * m as f64) + (m % 2) as f64 * 0.01;
        Market::from_series([
            coin("BTC", minutes, rise(0.0001)),
            coin("ETH", minutes, rise(0.00008)),
            coin("SOL", minutes, rise(0.00015)),
        ])
    }

    #[test]
    fn v13_holds_a_winner_until_the_360_minute_deadline() {
        let market = rising_market(2 * 1440);
        let c = cfg(1500, 2880, 20);
        let r = run(&market, &c, Box::new(V13::new().unwrap())).unwrap();
        let first = &r.trades[0];
        assert_eq!(first.asset, "SOL");
        assert_eq!(first.entry_ms, c.start_ms);
        assert_eq!(first.reason, "deadline");
        assert_eq!(first.exit_ms - first.entry_ms, 360 * MINUTE_MS);
        assert!(first.net_pnl_usdt > 0.0);
        assert_eq!(r.skipped_orders, 0);
    }
}

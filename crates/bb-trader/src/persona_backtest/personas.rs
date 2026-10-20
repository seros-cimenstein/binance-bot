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

/// The contrarian exit plan: sell into a stretch above the upper Bollinger band (only above
/// break-even), or after 6 hours.
pub const CONTRARIAN_EXIT_PLAN: &str = "stretch=2:1,deadline=360";
/// A held coin's 1h move at which the contrarian takes the bounce.
pub const CONTRARIAN_TAKE_1H_PCT: f64 = 1.5;

/// `contrarian`: fades the market breadth: buys weakness, sells strength.
///
/// - Candidates: the liquid top 10, as `v13`.
/// - Buy only when flat and the breadth score is at most −15: the candidate with the lowest
///   4h return (the biggest laggard), with all cash.
/// - Exit: plan `stretch=2:1,deadline=360`, plus a sell at a step when the held coin's 1h
///   return is at least +1.5% or the breadth score is at least +15.
#[derive(Debug, Clone)]
pub struct Contrarian {
    plan: ExitPlan,
}

impl Contrarian {
    pub fn new() -> Result<Self> {
        Ok(Self {
            plan: plan(CONTRARIAN_EXIT_PLAN)?,
        })
    }
}

impl Persona for Contrarian {
    fn name(&self) -> &str {
        "contrarian"
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        if !book.is_flat() {
            if snap.score.is_some_and(|s| s >= LEAN_SCORE) {
                return sell_all(book, "breadth_strength");
            }
            return book
                .holdings
                .keys()
                .filter(|asset| {
                    snap.coin(asset)
                        .and_then(|c| c.ret_1h)
                        .is_some_and(|r| r >= CONTRARIAN_TAKE_1H_PCT)
                })
                .map(|asset| Intent::Sell {
                    asset: asset.clone(),
                    reason: "bounce",
                })
                .collect();
        }
        if !snap.score.is_some_and(|s| s <= -LEAN_SCORE) {
            return Vec::new();
        }
        best_by(snap.liquid_top.iter(), |asset| {
            snap.coin(asset)?.ret_4h.map(|r| -r)
        })
        .map(|asset| {
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

/// Minimum 1h move (percent) for a `detached` entry: one modeled round trip (25 bps).
pub const DETACHED_MIN_1H_PCT: f64 = 0.25;
/// 1h lead (percent) another coin needs over the held one to make `detached` rotate.
pub const DETACHED_ROTATE_GAP_PCT: f64 = 0.5;

/// `detached`: raw price action only: no breadth, no 24h volume, no liquid filter and no
/// exit plan.
///
/// - Universe: every cached coin with a bar at the step.
/// - Buy when flat: the coin with the highest 1h return, if that return is above +0.25%
///   (one modeled round trip) and its 15m return is positive, with all cash.
/// - Sell at a step when the held coin's 15m and 1h returns are both negative.
/// - Rotate: sell the held coin and buy the best 1h coin when that coin's 1h return beats
///   the held coin's by more than 0.5 percentage points.
#[derive(Debug, Clone, Default)]
pub struct Detached;

impl Detached {
    /// The coin with the highest 1h return, ties to the first by name.
    fn leader(snap: &Snapshot) -> Option<(&String, f64)> {
        let leader = best_by(snap.coins.keys(), |asset| snap.coin(asset)?.ret_1h)?;
        Some((leader, snap.coin(leader)?.ret_1h?))
    }
}

impl Persona for Detached {
    fn name(&self) -> &str {
        "detached"
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        let buy = |asset: &String| Intent::Buy {
            asset: asset.clone(),
            equity_fraction: 1.0,
        };
        let Some(held) = book.holdings.keys().next() else {
            let entry = Self::leader(snap).filter(|(asset, r1h)| {
                *r1h > DETACHED_MIN_1H_PCT
                    && snap
                        .coin(asset)
                        .and_then(|c| c.ret_15m)
                        .is_some_and(|r| r > 0.0)
            });
            return entry.map(|(asset, _)| vec![buy(asset)]).unwrap_or_default();
        };
        let Some(coin) = snap.coin(held) else {
            return Vec::new();
        };
        let (Some(r15), Some(r1h)) = (coin.ret_15m, coin.ret_1h) else {
            return Vec::new();
        };
        if r15 < 0.0 && r1h < 0.0 {
            return vec![Intent::Sell {
                asset: held.clone(),
                reason: "price_turn",
            }];
        }
        match Self::leader(snap) {
            Some((leader, lead)) if leader != held && lead - r1h > DETACHED_ROTATE_GAP_PCT => vec![
                Intent::Sell {
                    asset: held.clone(),
                    reason: "rotate",
                },
                buy(leader),
            ],
            _ => Vec::new(),
        }
    }
}

/// The `high_variance` exit plan: a wide ATR trail and a 24h cap.
pub const HIGH_VARIANCE_EXIT_PLAN: &str = "atr=6:1h,deadline=1440";
/// Minimum 1h move (percent) for a `high_variance` entry.
pub const HIGH_VARIANCE_MIN_1H_PCT: f64 = 0.5;
/// Most positions `high_variance` holds at once.
pub const HIGH_VARIANCE_MAX_POSITIONS: usize = 3;

/// `high_variance`: more trades, wider stops, swinging size.
///
/// - Candidates: the liquid top 10.
/// - Buy at every step, whatever the breadth: each candidate not held whose 1h return is at
///   least +0.5%, strongest 1h first, up to 3 positions at once.
/// - Size: `clamp(|1h return| / 2%, 0.25, 1.0)` of the step's equity, capped by cash.
/// - Exit: plan `atr=6:1h,deadline=1440` only; never sells at a step.
#[derive(Debug, Clone)]
pub struct HighVariance {
    plan: ExitPlan,
}

impl HighVariance {
    pub fn new() -> Result<Self> {
        Ok(Self {
            plan: plan(HIGH_VARIANCE_EXIT_PLAN)?,
        })
    }

    /// Share of equity for a coin that moved `ret_1h` percent over the last hour.
    pub fn size(ret_1h: f64) -> f64 {
        (ret_1h.abs() / 2.0).clamp(0.25, 1.0)
    }
}

impl Persona for HighVariance {
    fn name(&self) -> &str {
        "high_variance"
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        let room = HIGH_VARIANCE_MAX_POSITIONS.saturating_sub(book.holdings.len());
        let mut movers: Vec<(&String, f64)> = snap
            .liquid_top
            .iter()
            .filter(|asset| !book.holdings.contains_key(*asset))
            .filter_map(|asset| Some((asset, snap.coin(asset)?.ret_1h?)))
            .filter(|(_, r1h)| *r1h >= HIGH_VARIANCE_MIN_1H_PCT)
            .collect();
        // Strongest first; the stable sort keeps the liquid-top order on ties.
        movers.sort_by(|a, b| b.1.total_cmp(&a.1));
        movers
            .into_iter()
            .take(room)
            .map(|(asset, r1h)| Intent::Buy {
                asset: asset.clone(),
                equity_fraction: Self::size(r1h),
            })
            .collect()
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

    const LAGGARDS: [Row<'static>; 3] = [
        ("BTC", -0.2, -0.5, -1.5, 3e8),
        ("SOL", -0.3, -0.8, -4.0, 1e8),
        ("ETH", -0.1, -0.1, -2.0, 2e8),
    ];

    #[test]
    fn contrarian_buys_the_worst_4h_coin_only_under_bearish_breadth() {
        let mut c = Contrarian::new().unwrap();
        assert!(
            c.decide(&snap(-14.9, -2.0, &LAGGARDS), &flat_book())
                .is_empty()
        );
        assert!(
            c.decide(&snap(40.0, 2.0, &LAGGARDS), &flat_book())
                .is_empty()
        );
        let intents = c.decide(&snap(-15.0, -2.0, &LAGGARDS), &flat_book());
        assert_eq!(
            intents,
            vec![Intent::Buy {
                asset: "SOL".into(),
                equity_fraction: 1.0
            }]
        );
    }

    #[test]
    fn contrarian_sells_the_bounce_or_when_breadth_turns_bullish() {
        let mut c = Contrarian::new().unwrap();
        let bounce = |r1h: f64| [("SOL", 0.3, r1h, -1.0, 1e8), ("BTC", 0.0, 0.0, 0.0, 3e8)];
        // A 1.49% bounce in neutral breadth holds; 1.5% sells.
        assert!(
            c.decide(&snap(0.0, 0.0, &bounce(1.49)), &holding(&["SOL"]))
                .is_empty()
        );
        let intents = c.decide(&snap(0.0, 0.0, &bounce(1.5)), &holding(&["SOL"]));
        assert_eq!(sold(&intents), vec!["SOL"]);
        // Breadth flipping to +15 sells everything even without a bounce.
        assert!(
            c.decide(&snap(14.9, 0.0, &bounce(0.2)), &holding(&["SOL"]))
                .is_empty()
        );
        let intents = c.decide(&snap(15.0, 0.0, &bounce(0.2)), &holding(&["SOL"]));
        assert_eq!(sold(&intents), vec!["SOL"]);
        assert!(bought(&intents).is_empty());
    }

    #[test]
    fn contrarian_holds_a_falling_laggard_until_its_deadline() {
        // Every coin falls (SOL fastest): breadth −100, no bounce, no stretch above
        // break-even, so the 6h deadline sells.
        let fall =
            |rate: f64| move |m: i64| 100.0 * (1.0 - rate * m as f64) + (m % 2) as f64 * 0.01;
        let market = Market::from_series([
            coin("BTC", 2880, fall(0.00005)),
            coin("ETH", 2880, fall(0.00004)),
            coin("SOL", 2880, fall(0.0001)),
        ]);
        let c = cfg(1500, 2880, 20);
        let r = run(&market, &c, Box::new(Contrarian::new().unwrap())).unwrap();
        let first = &r.trades[0];
        assert_eq!((first.asset.as_str(), first.entry_ms), ("SOL", c.start_ms));
        assert_eq!(first.reason, "deadline");
        assert_eq!(first.exit_ms - first.entry_ms, 360 * MINUTE_MS);
        assert!(first.net_pnl_usdt < 0.0);
        assert_eq!(r.skipped_orders, 0);
    }

    #[test]
    fn detached_ignores_breadth_volume_and_the_liquid_filter() {
        let rows = [
            ("BTC", 0.1, 0.3, 1.0, 3e8),
            ("INJ", 0.2, 0.9, -1.0, 1.0),
            ("SOL", -0.1, 1.2, 2.0, 1e8),
        ];
        let mut d = Detached;
        let mut bear = snap(-100.0, -5.0, &rows);
        // INJ is not a liquid candidate and has almost no volume: it can still be picked.
        bear.liquid_top = vec!["BTC".into(), "SOL".into()];
        let mut bull = bear.clone();
        bull.score = Some(100.0);
        bull.horizons[2].median_return_pct = Some(5.0);
        let a = d.decide(&bear, &flat_book());
        let b = d.decide(&bull, &flat_book());
        // SOL leads on 1h but its 15m is negative, so no entry; INJ is not the leader.
        assert!(a.is_empty());
        assert_eq!(a, b);
        let rows = [("BTC", 0.1, 0.3, 1.0, 3e8), ("INJ", 0.2, 0.9, -1.0, 1.0)];
        let mut s = snap(-100.0, -5.0, &rows);
        s.liquid_top = vec!["BTC".into()];
        assert_eq!(bought(&d.decide(&s, &flat_book())), vec!["INJ"]);
        s.score = Some(100.0);
        assert_eq!(bought(&d.decide(&s, &flat_book())), vec!["INJ"]);
        // At or under +0.25% 1h nobody qualifies.
        let weak = snap(0.0, 0.0, &[("BTC", 0.1, 0.25, 1.0, 3e8)]);
        assert!(d.decide(&weak, &flat_book()).is_empty());
    }

    #[test]
    fn detached_rotates_on_a_lead_over_half_a_point_and_sells_a_turn() {
        let mut d = Detached;
        let gap = |other: f64| {
            snap(
                0.0,
                0.0,
                &[("BTC", 0.1, 1.0, 0.0, 3e8), ("ETH", 0.1, other, 0.0, 2e8)],
            )
        };
        assert!(d.decide(&gap(1.49), &holding(&["BTC"])).is_empty());
        let intents = d.decide(&gap(1.51), &holding(&["BTC"]));
        assert_eq!(sold(&intents), vec!["BTC"]);
        assert_eq!(bought(&intents), vec!["ETH"]);
        // 15m and 1h both negative: sell, whatever the others do.
        let turn = snap(
            0.0,
            0.0,
            &[("BTC", -0.1, -0.2, 0.0, 3e8), ("ETH", 0.1, 0.1, 0.0, 2e8)],
        );
        let intents = d.decide(&turn, &holding(&["BTC"]));
        assert_eq!((sold(&intents), bought(&intents)), (vec!["BTC"], vec![]));
        // Only one of them negative: hold.
        let mixed = snap(
            0.0,
            0.0,
            &[("BTC", -0.1, 0.2, 0.0, 3e8), ("ETH", 0.1, 0.1, 0.0, 2e8)],
        );
        assert!(d.decide(&mixed, &holding(&["BTC"])).is_empty());
    }

    #[test]
    fn detached_trades_without_an_exit_plan_in_the_engine() {
        let market = rising_market(2 * 1440);
        let c = cfg(1500, 2880, 20);
        let r = run(&market, &c, Box::new(Detached)).unwrap();
        assert!(Detached.exit_plan().is_none());
        // Rising market: it buys the fastest coin (SOL) and never has a reason to sell.
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].asset, "SOL");
        assert!(r.trades[0].open_at_end);
        assert_eq!(r.skipped_orders, 0);
    }

    fn fractions(intents: &[Intent]) -> Vec<(&str, f64)> {
        intents
            .iter()
            .filter_map(|i| match i {
                Intent::Buy {
                    asset,
                    equity_fraction,
                } => Some((asset.as_str(), *equity_fraction)),
                Intent::Sell { .. } => None,
            })
            .collect()
    }

    #[test]
    fn high_variance_sizes_by_the_move_and_ignores_breadth() {
        let rows = [
            ("BTC", 0.0, 0.49, 0.0, 3e8),
            ("ETH", 0.0, 0.5, 0.0, 2e8),
            ("SOL", 0.0, 1.0, 0.0, 1e8),
            ("XRP", 0.0, 2.0, 0.0, 9e7),
            ("ZEC", 0.0, 3.5, 0.0, 8e7),
        ];
        let mut hv = HighVariance::new().unwrap();
        let mut all = holding(&[]);
        all.cash = 1000.0;
        let intents = hv.decide(&snap(-100.0, -5.0, &rows), &all);
        // Up to 3 at once, strongest first: ZEC (capped at 1.0), XRP (1.0), SOL (0.5).
        assert_eq!(
            fractions(&intents),
            vec![("ZEC", 1.0), ("XRP", 1.0), ("SOL", 0.5)]
        );
        assert_eq!(intents, hv.decide(&snap(100.0, 5.0, &rows), &all));
        assert!((HighVariance::size(0.5) - 0.25).abs() < 1e-12);
        assert!((HighVariance::size(2.0) - 1.0).abs() < 1e-12);
        // BTC at +0.49% never qualifies, even with room.
        let intents = hv.decide(&snap(0.0, 0.0, &rows[..2]), &all);
        assert_eq!(fractions(&intents), vec![("ETH", 0.25)]);
    }

    #[test]
    fn high_variance_refuses_a_fourth_position() {
        let rows = [("SOL", 0.0, 1.0, 0.0, 1e8), ("XRP", 0.0, 2.0, 0.0, 9e7)];
        let mut hv = HighVariance::new().unwrap();
        assert!(
            hv.decide(&snap(0.0, 0.0, &rows), &holding(&["BTC", "ETH", "ZEC"]))
                .is_empty()
        );
        // With two held, only the strongest new mover fits; held coins are not bought again.
        let intents = hv.decide(&snap(0.0, 0.0, &rows), &holding(&["BTC", "ETH"]));
        assert_eq!(bought(&intents), vec!["XRP"]);
        assert!(
            hv.decide(&snap(0.0, 0.0, &rows), &holding(&["SOL", "XRP"]))
                .is_empty()
        );
    }

    #[test]
    fn the_atr_6_trail_rides_out_a_drop_that_atr_3_5_sells() {
        // SOL rises 0.01 per minute to 100 at minute 1500 (1h ATR = 0.6, 1h return ≈ +0.6%),
        // is bought there, then sits 4.75 ATR lower: below the ATR-3.5 line, above ATR-6.
        let market = Market::from_series([coin("SOL", 1800, |m| {
            if m <= 1500 {
                100.0 + 0.01 * (m - 1500) as f64
            } else {
                100.05 - 4.75 * 0.6
            }
        })]);
        let c = cfg(1500, 1800, 20);
        let wide = run(&market, &c, Box::new(HighVariance::new().unwrap())).unwrap();
        assert_eq!(wide.trades.len(), 1);
        assert!(wide.trades[0].open_at_end, "{:?}", wide.trades[0]);
        // The 1h return at the step compares the closes of minutes 1499 and 1439.
        let ret_1h = (99.99 / 99.39 - 1.0) * 100.0;
        assert!((wide.trades[0].cost_usdt - 1000.0 * HighVariance::size(ret_1h)).abs() < 1e-6);
        let tight = HighVariance {
            plan: plan("atr=3.5:1h,deadline=1440").unwrap(),
        };
        let narrow = run(&market, &c, Box::new(tight)).unwrap();
        assert_eq!(narrow.trades[0].reason, "follow_peak");
        assert!(!narrow.trades[0].open_at_end);
    }
}

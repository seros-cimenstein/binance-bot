//! The personas: deterministic rule sets, each documented by its doc comment.
//!
//! Every persona reads only the step's [`Snapshot`] and its own [`Book`]. Exit plans use
//! the Phase 45 menu (`ExitPlan::parse_spec`).

use std::collections::{BTreeMap, BTreeSet};

use super::data::Market;
use super::engine::{Account, Book, Config, Intent, Persona, Snapshot, Trace};
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

/// The four base personas, `v13` first.
pub fn base_personas() -> Result<Vec<Box<dyn Persona>>> {
    Ok(vec![
        Box::new(V13::new()?),
        Box::new(Contrarian::new()?),
        Box::new(Detached),
        Box::new(HighVariance::new()?),
    ])
}

/// Trailing window `rotation` scores its shadows over.
pub const ROTATION_LOOKBACK_MS: i64 = 72 * 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// Index of the shadow with the best net return over the `lookback_ms` before `t`, from
/// each shadow's step equities (time order, nothing after `t` used). Index 0 wins ties and
/// keeps the job while there is not yet a step at or before `t - lookback_ms`.
pub fn best_trailing(step_equities: &[&[(i64, f64)]], t: i64, lookback_ms: i64) -> usize {
    let trailing = |equity: &[(i64, f64)]| -> Option<f64> {
        let upto = &equity[..equity.partition_point(|(ts, _)| *ts <= t)];
        let now = upto.last()?.1;
        let then_index = upto.partition_point(|(ts, _)| *ts <= t - lookback_ms);
        let then = upto[..then_index].last()?.1;
        (then > 0.0).then(|| now / then - 1.0)
    };
    let mut best = 0;
    let mut best_return = step_equities.first().and_then(|e| trailing(e));
    for (i, equity) in step_equities.iter().enumerate().skip(1) {
        if let Some(r) = trailing(equity)
            && best_return.is_some_and(|b| r > b)
        {
            best = i;
            best_return = Some(r);
        }
    }
    best
}

/// `rotation`: the "rotate" half of the multi-persona mode.
///
/// - Shadow-runs every base persona (`v13`, `contrarian`, `detached`, `high_variance`) on
///   its own simulated account, fed the same steps and minutes, so only shadow equity up to
///   the step is ever read.
/// - At the first step of each UTC day (steps rarely land on 00:00 exactly), it makes
///   active the base persona whose shadow made the best net return over the trailing 72h.
///   Ties, and every day before 72h of shadow history exist, go to `v13`.
/// - On a switch it sells every holding at that step (both fees); the new persona trades
///   from the next step on a flat book. Otherwise it follows the active persona's intents
///   and exit plan.
pub struct Rotation {
    deciders: Vec<Box<dyn Persona>>,
    shadows: Vec<Account>,
    active: usize,
    day: Option<i64>,
    switching: bool,
    switches: Vec<(i64, usize, usize)>,
    active_steps: Vec<usize>,
}

impl Rotation {
    pub fn new(cfg: Config) -> Result<Self> {
        Self::with(cfg, base_personas)
    }

    /// A rotation over the personas `make` builds (called twice: deciders and shadows).
    pub fn with(cfg: Config, make: impl Fn() -> Result<Vec<Box<dyn Persona>>>) -> Result<Self> {
        let deciders = make()?;
        if deciders.is_empty() {
            return Err(TraderError::config("rotation needs at least one persona"));
        }
        let shadows = make()?
            .into_iter()
            .map(|persona| Account::new(persona, cfg))
            .collect();
        Ok(Self {
            active_steps: vec![0; deciders.len()],
            deciders,
            shadows,
            active: 0,
            day: None,
            switching: false,
            switches: Vec::new(),
        })
    }

    fn decider_name(&self, i: usize) -> &str {
        self.deciders[i].name()
    }
}

impl Persona for Rotation {
    fn name(&self) -> &str {
        "rotation"
    }

    fn before_step(&mut self, market: &Market, snap: &Snapshot) {
        for shadow in &mut self.shadows {
            shadow.step(market, snap);
        }
        let day = snap.t.div_euclid(DAY_MS);
        let new_day = self.day.is_some_and(|d| d != day);
        self.day = Some(day);
        if !new_day {
            return;
        }
        let equities: Vec<&[(i64, f64)]> = self.shadows.iter().map(Account::step_equity).collect();
        let best = best_trailing(&equities, snap.t, ROTATION_LOOKBACK_MS);
        if best != self.active {
            self.switches.push((snap.t, self.active, best));
            self.active = best;
            self.switching = true;
        }
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        self.active_steps[self.active] += 1;
        if std::mem::take(&mut self.switching) {
            return sell_all(book, "switch");
        }
        self.deciders[self.active].decide(snap, book)
    }

    fn exit_plan(&self) -> Option<ExitPlan> {
        self.deciders[self.active].exit_plan()
    }

    fn after_minute(&mut self, market: &Market, m: i64) {
        for shadow in &mut self.shadows {
            shadow.minute(market, m);
        }
    }

    fn notes(&self) -> Vec<String> {
        let time = |t: i64| {
            chrono::DateTime::from_timestamp_millis(t)
                .map_or_else(|| t.to_string(), |d| d.format("%Y-%m-%d %H:%M").to_string())
        };
        let steps: Vec<String> = (0..self.deciders.len())
            .map(|i| format!("{} {}", self.decider_name(i), self.active_steps[i]))
            .collect();
        let mut notes = vec![
            format!("switches: {}", self.switches.len()),
            format!("active steps: {}", steps.join(", ")),
        ];
        notes.extend(self.switches.iter().map(|(t, from, to)| {
            format!(
                "{} UTC: {} -> {}",
                time(*t),
                self.decider_name(*from),
                self.decider_name(*to)
            )
        }));
        notes
    }
}

/// Votes `ensemble` needs to buy a coin or to sell the held one.
pub const ENSEMBLE_MIN_VOTES: usize = 2;

/// The coin to buy from `(asset, votes)`: at least [`ENSEMBLE_MIN_VOTES`], most votes first,
/// then the highest 24h quote volume at the step, then the first by name.
pub fn ensemble_pick<'a>(
    votes: &'a BTreeMap<String, usize>,
    snap: &Snapshot,
) -> Option<&'a String> {
    let volume = |asset: &str| snap.coin(asset).map_or(0.0, |c| c.quote_volume_24h);
    votes
        .iter()
        .filter(|(_, n)| **n >= ENSEMBLE_MIN_VOTES)
        .fold(
            None,
            |best: Option<(&String, usize)>, (asset, n)| match best {
                Some((b, bn)) if bn > *n || (bn == *n && volume(b) >= volume(asset)) => {
                    Some((b, bn))
                }
                _ => Some((asset, *n)),
            },
        )
        .map(|(asset, _)| asset)
}

/// `ensemble`: the "vote" half of the multi-persona mode. One position, all cash.
///
/// - At each step every base persona (`v13`, `contrarian`, `detached`, `high_variance`)
///   decides on the ensemble's own book; each persona gives at most one buy and one sell
///   vote per coin.
/// - Flat: buy the coin with at least 2 buy votes; most votes first, then the highest 24h
///   quote volume.
/// - Holding: sell when at least 2 personas vote to sell the held coin, or when the `v13`
///   exit plan (`atr=3.5:1h,deadline=360`) fires. Buy votes while holding are ignored.
pub struct Ensemble {
    voters: Vec<Box<dyn Persona>>,
    plan: ExitPlan,
    buys_by_votes: BTreeMap<usize, usize>,
    vote_sells: usize,
}

impl Ensemble {
    pub fn new() -> Result<Self> {
        Self::with(base_personas()?)
    }

    pub fn with(voters: Vec<Box<dyn Persona>>) -> Result<Self> {
        Ok(Self {
            voters,
            plan: plan(V13_EXIT_PLAN)?,
            buys_by_votes: BTreeMap::new(),
            vote_sells: 0,
        })
    }
}

impl Persona for Ensemble {
    fn name(&self) -> &str {
        "ensemble"
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        let votes = Votes::cast(&mut self.voters, None, snap, book);
        if !book.is_flat() {
            let sells: Vec<Intent> = votes
                .sells(book)
                .into_iter()
                .map(|asset| Intent::Sell {
                    asset: asset.clone(),
                    reason: "vote",
                })
                .collect();
            self.vote_sells += sells.len();
            return sells;
        }
        let Some(asset) = votes.pick(snap) else {
            return Vec::new();
        };
        *self.buys_by_votes.entry(votes.buy[asset]).or_default() += 1;
        vec![Intent::Buy {
            asset: asset.clone(),
            equity_fraction: 1.0,
        }]
    }

    fn exit_plan(&self) -> Option<ExitPlan> {
        Some(self.plan)
    }

    fn notes(&self) -> Vec<String> {
        let buys: Vec<String> = self
            .buys_by_votes
            .iter()
            .map(|(votes, n)| format!("{n} with {votes} votes"))
            .collect();
        vec![
            format!(
                "buy intents: {}",
                if buys.is_empty() {
                    "none".to_string()
                } else {
                    buys.join(", ")
                }
            ),
            format!("vote sells: {}", self.vote_sells),
        ]
    }
}

/// How one step treated the mood: the first GCP2 reading picked it (`Initial`), switched
/// (GCP2 zone changed), held (zone unchanged), or held because a GCP2 reading was missing
/// now or at the previous step (`NoReading`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MoodStep {
    Initial,
    Switched,
    Held,
    NoReading,
}

impl MoodStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Switched => "switched",
            Self::Held => "held",
            Self::NoReading => "no reading",
        }
    }
}

/// One step of the mood ring, as the report's per-step trace records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoodRecord {
    pub t: i64,
    /// This step's GCP2 value in 1e-4 units (`None` without a reading).
    pub units: Option<u64>,
    /// The previous step's zone (`None` when it had no reading or this is the first step).
    pub from_zone: Option<String>,
    pub to_zone: Option<String>,
    pub dice: Option<u64>,
    pub result: MoodStep,
    pub mood_before: Option<String>,
    pub mood_after: Option<String>,
    pub yolo: bool,
}

/// Header of [`MoodRecord::csv_row`].
pub const MOOD_TRACE_HEADER: &str =
    "time,gcp2_value,from_zone,to_zone,dice,result,mood_before,mood_after,yolo";

impl MoodRecord {
    pub fn csv_row(&self) -> String {
        let time = chrono::DateTime::from_timestamp_millis(self.t).map_or_else(
            || self.t.to_string(),
            |d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        let value = self
            .units
            .map(|u| format!("{}.{:04}", u / 10_000, u % 10_000))
            .unwrap_or_default();
        let text = |s: &Option<String>| s.clone().unwrap_or_default();
        format!(
            "{time},{value},{},{},{},{},{},{},{}",
            text(&self.from_zone),
            text(&self.to_zone),
            self.dice.map(|d| d.to_string()).unwrap_or_default(),
            self.result.as_str(),
            text(&self.mood_before),
            text(&self.mood_after),
            self.yolo
        )
    }
}

/// The mood after one step of the GCP2 mood ring (the mood follows GCP2).
///
/// - `active`: index into `moods` (`None` until the first GCP2 reading).
/// - `zone`, `prev_zone`: this step's and the previous step's GCP2 zone.
/// - `dice`: this step's GCP2 dice.
///
/// The first reading picks a mood with `dice % moods`. After that the mood switches, to
/// the other mood `dice % (moods − 1)` in the fixed order, only when the zone changed (up or
/// down) since the previous step; an unchanged zone or a missing reading keeps it.
pub fn next_mood(
    active: Option<usize>,
    moods: usize,
    zone: Option<usize>,
    prev_zone: Option<usize>,
    dice: Option<u64>,
) -> (Option<usize>, MoodStep) {
    let (Some(zone), Some(dice)) = (zone, dice) else {
        return (active, MoodStep::NoReading);
    };
    let Some(current) = active else {
        let first = usize::try_from(dice % moods as u64).unwrap_or(0);
        return (Some(first), MoodStep::Initial);
    };
    match prev_zone {
        Some(prev) if prev != zone => {
            let others: Vec<usize> = (0..moods).filter(|m| *m != current).collect();
            let pick = usize::try_from(dice % others.len() as u64).unwrap_or(0);
            (Some(others[pick]), MoodStep::Switched)
        }
        Some(_) => (Some(current), MoodStep::Held),
        None => (Some(current), MoodStep::NoReading),
    }
}

/// Whether a GCP2 zone triggers YOLO: one of the two highest zones GCP2 publishes.
pub fn yolo_enters(zone: usize, zone_count: usize) -> bool {
    zone + 2 >= zone_count
}

/// Whether YOLO keeps going: the zone is still at or above the third-highest.
pub fn yolo_stays(zone: usize, zone_count: usize) -> bool {
    zone + 3 >= zone_count
}

/// YOLO after a step: `zone` is `(zone index, zone count)` of this step's GCP2 reading.
/// Entering needs one of the two highest zones, staying the third-highest or above; without
/// a reading there is no YOLO.
pub fn yolo_next(was_yolo: bool, zone: Option<(usize, usize)>) -> bool {
    match zone {
        Some((z, n)) if was_yolo => yolo_stays(z, n),
        Some((z, n)) => yolo_enters(z, n),
        None => false,
    }
}

/// The YOLO coin: the liquid-top coin with the highest 1h return (ties: liquid-top order).
pub fn yolo_target(snap: &Snapshot) -> Option<&String> {
    best_by(snap.liquid_top.iter(), |a| snap.coin(a)?.ret_1h)
}

/// One persona's votes at a step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PersonaVote {
    pub persona: String,
    /// 2 for the active mood, 1 otherwise.
    pub weight: usize,
    pub buy: Vec<String>,
    pub sell: Vec<String>,
}

/// The base personas arguing over one book: each gives at most one buy and one sell vote
/// per coin, weighted by [`PersonaVote::weight`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Votes {
    pub by_persona: Vec<PersonaVote>,
    /// Weighted buy votes per coin.
    pub buy: BTreeMap<String, usize>,
    /// Weighted sell votes per coin.
    pub sell: BTreeMap<String, usize>,
}

impl Votes {
    /// Every voter decides on `book`; `active` (index into `voters`) counts double.
    pub fn cast(
        voters: &mut [Box<dyn Persona>],
        active: Option<usize>,
        snap: &Snapshot,
        book: &Book,
    ) -> Self {
        let mut votes = Self::default();
        for (i, voter) in voters.iter_mut().enumerate() {
            let weight = if Some(i) == active { 2 } else { 1 };
            let mut bought = BTreeSet::new();
            let mut sold = BTreeSet::new();
            for intent in voter.decide(snap, book) {
                match intent {
                    Intent::Buy { asset, .. } => {
                        bought.insert(asset);
                    }
                    Intent::Sell { asset, .. } => {
                        sold.insert(asset);
                    }
                }
            }
            for asset in &bought {
                *votes.buy.entry(asset.clone()).or_default() += weight;
            }
            for asset in &sold {
                *votes.sell.entry(asset.clone()).or_default() += weight;
            }
            votes.by_persona.push(PersonaVote {
                persona: voter.name().to_string(),
                weight,
                buy: bought.into_iter().collect(),
                sell: sold.into_iter().collect(),
            });
        }
        votes
    }

    /// Held coins with at least [`ENSEMBLE_MIN_VOTES`] weighted sell votes.
    pub fn sells<'a>(&self, book: &'a Book) -> Vec<&'a String> {
        book.holdings
            .keys()
            .filter(|a| self.sell.get(*a).is_some_and(|n| *n >= ENSEMBLE_MIN_VOTES))
            .collect()
    }

    /// The coin a flat book buys (see [`ensemble_pick`]).
    pub fn pick(&self, snap: &Snapshot) -> Option<&String> {
        ensemble_pick(&self.buy, snap)
    }
}

/// `schizo` (Phase 49): the four base personas in one head, ruled by GCP2.
///
/// - **Dice:** the GCP2 dice of the minute ending before the step. No seed; without a
///   reading nothing random happens.
/// - **Mood ring, following GCP2:** the active mood is one of `v13`, `contrarian`,
///   `detached`, `high_variance`. The first reading picks it with the dice. Then, when the
///   GCP2 zone changed (up or down) since the previous step, the mood switches to another one
///   picked by the dice; when the zone is the same, or a reading is missing, it is kept.
/// - **YOLO:** when the zone reaches one of GCP2's two highest zones, sell every other
///   holding and put all cash into the liquid coin with the highest 1h return. Hold it
///   until the zone drops below the third-highest zone (or its exit plan sells it); then the
///   normal rules take over.
/// - **Argues with itself:** outside YOLO all four personas decide on the schizo's book.
///   The active mood's vote counts 2, the others 1. Flat: buy the coin with the most votes
///   if it reaches 2 (ties: higher 24h volume, then name). Holding: sell on 2 or more sell
///   votes.
/// - **Extreme bets:** one coin, all cash.
/// - **Exits:** a position runs the exit plan of the mood that bought it (`v13`'s for a
///   YOLO buy); no added stop-loss.
pub struct Schizo {
    voters: Vec<Box<dyn Persona>>,
    yolo_plan: ExitPlan,
    active: Option<usize>,
    prev_zone: Option<(usize, String)>,
    yolo: bool,
    yolo_asset: Option<String>,
    next_plan: Option<ExitPlan>,
    next_tag: Option<&'static str>,
    trace: Vec<MoodRecord>,
    active_steps: BTreeMap<String, usize>,
    yolo_steps: usize,
    yolo_buys: Vec<(i64, String)>,
}

impl Schizo {
    pub fn new() -> Result<Self> {
        Ok(Self {
            voters: base_personas()?,
            yolo_plan: plan(V13_EXIT_PLAN)?,
            active: None,
            prev_zone: None,
            yolo: false,
            yolo_asset: None,
            next_plan: None,
            next_tag: None,
            trace: Vec::new(),
            active_steps: BTreeMap::new(),
            yolo_steps: 0,
            yolo_buys: Vec::new(),
        })
    }

    /// The active mood's name (`None` before the first GCP2 reading).
    pub fn mood(&self) -> Option<&str> {
        self.active.map(|i| self.voters[i].name())
    }

    /// Times and coins of every YOLO buy.
    pub fn yolo_buys(&self) -> &[(i64, String)] {
        &self.yolo_buys
    }

    /// Every step's mood-ring record, oldest first.
    pub fn trace_records(&self) -> &[MoodRecord] {
        &self.trace
    }

    /// Advance the mood ring for `snap`; returns the record without the YOLO flag.
    fn update_mood(&mut self, snap: &Snapshot) -> MoodRecord {
        let view = snap.gcp2.as_ref();
        let mood_before = self.mood().map(str::to_string);
        let (active, result) = next_mood(
            self.active,
            self.voters.len(),
            view.map(|v| v.zone),
            self.prev_zone.as_ref().map(|(z, _)| *z),
            view.map(|v| v.dice),
        );
        let from_zone = self.prev_zone.as_ref().map(|(_, label)| label.clone());
        self.active = active;
        self.prev_zone = view.map(|v| (v.zone, v.zone_label.clone()));
        MoodRecord {
            t: snap.t,
            units: view.map(|v| v.units),
            from_zone,
            to_zone: view.map(|v| v.zone_label.clone()),
            dice: view.map(|v| v.dice),
            result,
            mood_before,
            mood_after: self.mood().map(str::to_string),
            yolo: false,
        }
    }

    fn update_yolo(&mut self, snap: &Snapshot) {
        let zone = snap.gcp2.as_ref().map(|v| (v.zone, v.zone_count));
        self.yolo = yolo_next(self.yolo, zone);
        if !self.yolo {
            self.yolo_asset = None;
        }
    }

    fn yolo_intents(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        self.yolo_steps += 1;
        if self
            .yolo_asset
            .as_ref()
            .is_some_and(|a| book.holdings.contains_key(a))
        {
            return Vec::new();
        }
        let Some(target) = yolo_target(snap).cloned() else {
            return Vec::new();
        };
        let mut intents: Vec<Intent> = book
            .holdings
            .keys()
            .filter(|a| **a != target)
            .map(|a| Intent::Sell {
                asset: a.clone(),
                reason: "yolo",
            })
            .collect();
        if !book.holdings.contains_key(&target) {
            intents.push(Intent::Buy {
                asset: target.clone(),
                equity_fraction: 1.0,
            });
            self.next_plan = Some(self.yolo_plan);
            self.next_tag = Some("yolo");
            self.yolo_buys.push((snap.t, target.clone()));
        }
        self.yolo_asset = Some(target);
        intents
    }

    fn vote_intents(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        let votes = Votes::cast(&mut self.voters, self.active, snap, book);
        if !book.is_flat() {
            return votes
                .sells(book)
                .into_iter()
                .map(|a| Intent::Sell {
                    asset: a.clone(),
                    reason: "vote",
                })
                .collect();
        }
        let Some(asset) = votes.pick(snap).cloned() else {
            return Vec::new();
        };
        self.next_plan = self.active.and_then(|i| self.voters[i].exit_plan());
        self.next_tag = Some("vote");
        vec![Intent::Buy {
            asset,
            equity_fraction: 1.0,
        }]
    }
}

impl Persona for Schizo {
    fn name(&self) -> &str {
        "schizo"
    }

    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
        let mut record = self.update_mood(snap);
        self.update_yolo(snap);
        record.yolo = self.yolo;
        self.trace.push(record);
        if let Some(name) = self.mood() {
            *self.active_steps.entry(name.to_string()).or_default() += 1;
        }
        self.next_plan = None;
        self.next_tag = None;
        if self.yolo {
            self.yolo_intents(snap, book)
        } else {
            self.vote_intents(snap, book)
        }
    }

    fn exit_plan(&self) -> Option<ExitPlan> {
        self.next_plan
    }

    fn entry_tag(&self) -> Option<&'static str> {
        self.next_tag
    }

    fn notes(&self) -> Vec<String> {
        let mut by_result: BTreeMap<MoodStep, usize> = BTreeMap::new();
        let mut moves: BTreeMap<(&str, &str, MoodStep), usize> = BTreeMap::new();
        for r in &self.trace {
            *by_result.entry(r.result).or_default() += 1;
            if let (Some(from), Some(to)) = (&r.from_zone, &r.to_zone) {
                *moves.entry((from, to, r.result)).or_default() += 1;
            }
        }
        let count = |r: MoodStep| by_result.get(&r).copied().unwrap_or(0);
        let mut notes = vec![
            format!(
                "mood steps: {} initial, {} switched, {} held, {} without a GCP2 reading",
                count(MoodStep::Initial),
                count(MoodStep::Switched),
                count(MoodStep::Held),
                count(MoodStep::NoReading)
            ),
            format!(
                "active steps: {}",
                self.active_steps
                    .iter()
                    .map(|(m, n)| format!("{m} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "YOLO steps: {}, YOLO buys: {}",
                self.yolo_steps,
                self.yolo_buys.len()
            ),
        ];
        notes.extend(
            moves
                .iter()
                .map(|((from, to, r), n)| format!("zone {from} -> {to}: {} {n}", r.as_str())),
        );
        notes
    }

    fn trace(&self) -> Option<Trace> {
        Some(Trace {
            header: MOOD_TRACE_HEADER.to_string(),
            rows: self.trace.iter().map(MoodRecord::csv_row).collect(),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::gcp2::{ColorZones, Gcp2Reading, Gcp2View};
    use crate::market_mood::HorizonBreadth;
    use crate::persona_backtest::data::{MINUTE_MS, Market};
    use crate::persona_backtest::engine::tests::{
        T0, cfg, coin, gcp2_feed, gcp2_wave, gcp2_zones, truncated, wavy_market,
    };
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
            gcp2: None,
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

    #[test]
    fn rotation_picks_the_best_trailing_72h_shadow_with_v13_on_ties() {
        let h = 3_600_000;
        let t = 100 * h;
        // Index 2 gained 10% since 72h ago (its old peak 30h earlier does not count).
        let a = [(0, 100.0), (t - 72 * h, 100.0), (t, 104.0)];
        let b = [(0, 100.0), (t - 72 * h, 100.0), (t, 101.0)];
        let c = [(0, 200.0), (t - 72 * h, 100.0), (t, 110.0)];
        assert_eq!(best_trailing(&[&a, &b, &c], t, 72 * h), 2);
        // Data after t is ignored.
        let late = [(0, 100.0), (t - 72 * h, 100.0), (t, 100.0), (t + h, 999.0)];
        assert_eq!(best_trailing(&[&a, &late], t, 72 * h), 0);
        // A tie stays with index 0 (v13).
        assert_eq!(best_trailing(&[&a, &a.clone()], t, 72 * h), 0);
        // Without a step 72h back, nobody can be scored: v13 keeps the job.
        let young = [(t - 10 * h, 100.0), (t, 150.0)];
        assert_eq!(best_trailing(&[&young, &young.clone()], t, 72 * h), 0);
    }

    /// Buys one coin with all cash whenever flat.
    struct Buyer(&'static str, &'static str);

    impl Persona for Buyer {
        fn name(&self) -> &str {
            self.0
        }
        fn decide(&mut self, _snap: &Snapshot, book: &Book) -> Vec<Intent> {
            if book.is_flat() {
                vec![Intent::Buy {
                    asset: self.1.into(),
                    equity_fraction: 1.0,
                }]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn a_rotation_switch_sells_the_old_book_with_fees_then_follows_the_winner() {
        // SOL is flat, ETH climbs: the ETH buyer's shadow wins once 72h of history exist.
        let market = Market::from_series([
            coin("SOL", 5600, |_| 100.0),
            coin("ETH", 5600, |m| 10.0 + 0.001 * m as f64),
        ]);
        let c = cfg(0, 5600, 20);
        let make = || -> Result<Vec<Box<dyn Persona>>> {
            Ok(vec![
                Box::new(Buyer("sol_buyer", "SOL")),
                Box::new(Buyer("eth_buyer", "ETH")),
            ])
        };
        let r = run(&market, &c, Box::new(Rotation::with(c, make).unwrap())).unwrap();
        // T0 is 2026-07-19T08:00Z; day starts at minutes 960, 2400, 3840 and 5280. Only the
        // last one has a step 72h back (minute 960).
        let switch_ms = T0 + 5280 * MINUTE_MS;
        let sol = &r.trades[0];
        assert_eq!((sol.asset.as_str(), sol.entry_ms), ("SOL", c.start_ms));
        assert_eq!((sol.reason.as_str(), sol.exit_ms), ("switch", switch_ms));
        // Flat price: the round trip loses exactly both fees and both slippages.
        let qty = 1000.0 / (100.0 * 1.0005 * 1.00075);
        let fees = qty * 100.0 * 1.0005 * 0.00075 + qty * 100.0 * 0.9995 * 0.00075;
        assert!((sol.fees_usdt - fees).abs() < 1e-9);
        assert!((sol.net_pnl_usdt - (qty * 100.0 * 0.9995 * 0.99925 - 1000.0)).abs() < 1e-9);
        let eth = &r.trades[1];
        assert_eq!(
            (eth.asset.as_str(), eth.entry_ms),
            ("ETH", switch_ms + 20 * MINUTE_MS)
        );
        assert_eq!(r.trades.len(), 2);
        assert_eq!(r.notes[0], "switches: 1");
        assert_eq!(r.notes[2], "2026-07-23 00:00 UTC: sol_buyer -> eth_buyer");
    }

    #[test]
    fn rotation_decisions_up_to_a_step_do_not_change_when_later_bars_are_removed() {
        let market = wavy_market(5 * 1440);
        let c = cfg(1440 + 30, 5 * 1440, 20);
        let intents = |market: &Market| -> Vec<(i64, Vec<Intent>)> {
            let mut account = Account::new(Box::new(Rotation::new(c).unwrap()), c);
            let mut seen = Vec::new();
            let mut m = c.start_ms;
            while m < c.end_ms {
                if (m - c.start_ms) % (c.step_minutes * MINUTE_MS) == 0 {
                    let snap = Snapshot::at(market, m);
                    seen.push((m, account.step(market, &snap)));
                }
                account.minute(market, m);
                m += MINUTE_MS;
            }
            seen
        };
        // Cut after the 4th day start, so a scored switch decision falls before the cut.
        let cut = T0 + (4 * 1440 + 960 + 200) * MINUTE_MS;
        let full = intents(&market);
        let short = intents(&truncated(&market, cut));
        let upto = |s: &[(i64, Vec<Intent>)]| -> Vec<(i64, Vec<Intent>)> {
            s.iter().filter(|(t, _)| *t <= cut).cloned().collect()
        };
        assert!(upto(&full).iter().any(|(_, i)| !i.is_empty()));
        assert_eq!(upto(&full), upto(&short));
    }

    #[test]
    fn ensemble_pick_needs_two_votes_then_most_votes_then_volume() {
        let rows = [
            ("BTC", 0.0, 0.0, 0.0, 3e8),
            ("ETH", 0.0, 0.0, 0.0, 2e8),
            ("SOL", 0.0, 0.0, 0.0, 1e8),
        ];
        let s = snap(0.0, 0.0, &rows);
        let votes = |v: &[(&str, usize)]| -> BTreeMap<String, usize> {
            v.iter().map(|(a, n)| (a.to_string(), *n)).collect()
        };
        assert_eq!(ensemble_pick(&votes(&[("BTC", 1), ("SOL", 1)]), &s), None);
        assert_eq!(
            ensemble_pick(&votes(&[("BTC", 1), ("SOL", 2)]), &s).map(String::as_str),
            Some("SOL")
        );
        // Equal votes: the higher 24h quote volume wins (BTC over ETH over SOL).
        assert_eq!(
            ensemble_pick(&votes(&[("ETH", 2), ("SOL", 2)]), &s).map(String::as_str),
            Some("ETH")
        );
        assert_eq!(
            ensemble_pick(&votes(&[("SOL", 2), ("BTC", 2)]), &s).map(String::as_str),
            Some("BTC")
        );
        // More votes beat more volume.
        assert_eq!(
            ensemble_pick(&votes(&[("BTC", 2), ("SOL", 3)]), &s).map(String::as_str),
            Some("SOL")
        );
    }

    #[test]
    fn ensemble_buys_on_two_real_votes_but_not_one() {
        let mut e = Ensemble::new().unwrap();
        // Neutral breadth: v13 and contrarian abstain. SOL leads on 1h with a rising 15m:
        // at +0.4% only `detached` votes (high_variance needs +0.5%).
        let one = [("BTC", 0.1, 0.1, 0.0, 3e8), ("SOL", 0.2, 0.4, 0.0, 1e8)];
        assert!(e.decide(&snap(0.0, 0.0, &one), &flat_book()).is_empty());
        // At +0.6% `detached` and `high_variance` both vote for SOL.
        let two = [("BTC", 0.1, 0.1, 0.0, 3e8), ("SOL", 0.2, 0.6, 0.0, 1e8)];
        let intents = e.decide(&snap(0.0, 0.0, &two), &flat_book());
        assert_eq!(
            intents,
            vec![Intent::Buy {
                asset: "SOL".into(),
                equity_fraction: 1.0
            }]
        );
        assert_eq!(e.notes()[0], "buy intents: 1 with 2 votes");
    }

    #[test]
    fn ensemble_sells_on_two_sell_votes_but_not_one() {
        let mut e = Ensemble::new().unwrap();
        // Score −15: v13 votes to sell. SOL's 15m is up, so `detached` holds: one vote.
        let one = [("SOL", 0.1, -0.2, -1.0, 1e8)];
        assert!(
            e.decide(&snap(-15.0, -1.0, &one), &holding(&["SOL"]))
                .is_empty()
        );
        // 15m and 1h both down: `detached` joins v13.
        let two = [("SOL", -0.1, -0.2, -1.0, 1e8)];
        let intents = e.decide(&snap(-15.0, -1.0, &two), &holding(&["SOL"]));
        assert_eq!(
            intents,
            vec![Intent::Sell {
                asset: "SOL".into(),
                reason: "vote"
            }]
        );
        assert_eq!(e.notes()[1], "vote sells: 1");
    }

    #[test]
    fn ensemble_exit_plan_sells_with_no_sell_votes() {
        // SOL climbs while six liquid coins stay flat: breadth ≈ +14 (neither +15 nor −15),
        // so nobody votes to sell; `detached` and `high_variance` vote SOL in, and the
        // `v13` exit plan's 6h deadline sells it.
        let mut coins: Vec<_> = ["BTC", "ETH", "ZEC", "XRP", "BNB", "DOGE"]
            .iter()
            .map(|a| coin(a, 2880, |_| 50.0))
            .collect();
        coins.push(coin("SOL", 2880, |m| 100.0 * (1.0 + 0.0001 * m as f64)));
        let market = Market::from_series(coins);
        let c = cfg(1500, 2880, 20);
        let first_snap = Snapshot::at(&market, c.start_ms);
        let score = first_snap.score.unwrap();
        assert!(score > 0.0 && score < 15.0, "{score}");
        let r = run(&market, &c, Box::new(Ensemble::new().unwrap())).unwrap();
        let first = &r.trades[0];
        assert_eq!((first.asset.as_str(), first.entry_ms), ("SOL", c.start_ms));
        assert_eq!(first.reason, "deadline");
        assert_eq!(first.exit_ms - first.entry_ms, 360 * MINUTE_MS);
        assert!(
            r.notes.contains(&"vote sells: 0".to_string()),
            "{:?}",
            r.notes
        );
    }

    fn view(value: f64, zones: &ColorZones) -> Gcp2View {
        let reading = Gcp2Reading {
            end_s: 1,
            units: (value * 10_000.0).round() as u64,
        };
        Gcp2View::from_reading(reading, zones)
    }

    #[test]
    fn the_mood_switches_only_when_the_gcp2_zone_changes() {
        // First reading: the dice pick the mood, recorded as the initial pick, not a switch.
        assert_eq!(
            next_mood(None, 4, Some(0), None, Some(6)),
            (Some(2), MoodStep::Initial)
        );
        // Zone changed, up or down: switch to another mood picked by the dice; never the
        // same one.
        for dice in 0..300 {
            let others = [0, 2, 3];
            for (zone, prev) in [(3, 2), (1, 2)] {
                let (next, step) = next_mood(Some(1), 4, Some(zone), Some(prev), Some(dice));
                assert_eq!(step, MoodStep::Switched);
                assert_eq!(next, Some(others[(dice % 3) as usize]));
            }
        }
        // Same zone: keep.
        assert_eq!(
            next_mood(Some(1), 4, Some(2), Some(2), Some(7)),
            (Some(1), MoodStep::Held)
        );
        // A missing reading now or at the previous step: keep.
        assert_eq!(
            next_mood(Some(1), 4, None, Some(2), None),
            (Some(1), MoodStep::NoReading)
        );
        assert_eq!(
            next_mood(Some(1), 4, Some(2), None, Some(7)),
            (Some(1), MoodStep::NoReading)
        );
        // A stable zone keeps the mood for 10,000 steps, whatever the dice.
        for step in 0..10_000_u64 {
            let dice = crate::gcp2::dice(step);
            assert_eq!(
                next_mood(Some(0), 4, Some(0), Some(0), Some(dice)),
                (Some(0), MoodStep::Held)
            );
        }
    }

    #[test]
    fn published_zone_bounds_decide_which_steps_switch() {
        let zones = gcp2_zones();
        let mut shifted = gcp2_zones();
        shifted.zones[1].start = 150.0;
        // 130 then 145: Normal → Elevated with GCP2's bounds (switched), Normal → Normal
        // when Elevated starts at 150 (held).
        let step = |z: &ColorZones| {
            let (a, b) = (view(130.0, z), view(145.0, z));
            next_mood(Some(0), 4, Some(b.zone), Some(a.zone), Some(b.dice)).1
        };
        assert_eq!(step(&zones), MoodStep::Switched);
        assert_eq!(step(&shifted), MoodStep::Held);
        // YOLO thresholds follow the published zone list, not labels in code.
        assert!(yolo_enters(3, 5) && yolo_enters(4, 5) && !yolo_enters(2, 5));
        assert!(yolo_stays(2, 5) && !yolo_stays(1, 5));
        assert!(yolo_enters(4, 6) && !yolo_enters(3, 6));
    }

    #[test]
    fn yolo_goes_all_in_on_the_hottest_coin_and_holds_it() {
        let zones = gcp2_zones();
        let rows = [
            ("BTC", 0.0, 0.2, 0.0, 3e8),
            ("SOL", 0.0, 1.4, 0.0, 1e8),
            ("ETH", 0.0, 0.9, 0.0, 2e8),
        ];
        let mut s = Schizo::new().unwrap();
        let mut very_high = snap(0.0, 0.0, &rows);
        very_high.gcp2 = Some(view(230.0, &zones));
        assert_eq!(very_high.gcp2.as_ref().unwrap().zone_label, "Very High");
        let intents = s.decide(&very_high, &holding(&["BTC"]));
        assert_eq!(sold(&intents), vec!["BTC"]);
        assert_eq!(
            intents.last(),
            Some(&Intent::Buy {
                asset: "SOL".into(),
                equity_fraction: 1.0
            })
        );
        assert_eq!(s.exit_plan(), Some(plan(V13_EXIT_PLAN).unwrap()));
        assert_eq!(s.yolo_buys().len(), 1);
        // Still High (third-highest): YOLO holds SOL, nothing else happens.
        let mut high = snap(0.0, 0.0, &rows);
        high.gcp2 = Some(view(170.0, &zones));
        assert!(s.decide(&high, &holding(&["SOL"])).is_empty());
        // Back to Normal: YOLO is over and the votes decide again.
        let mut normal = snap(0.0, 0.0, &rows);
        normal.gcp2 = Some(view(50.0, &zones));
        s.decide(&normal, &holding(&["SOL"]));
        assert!(!s.yolo);
    }

    #[test]
    fn the_active_mood_votes_twice() {
        // Neutral breadth, no GCP2 reading (the mood stays as set): SOL at 1h +0.4% with a
        // rising 15m gets one vote, from `detached`.
        let rows = [("BTC", 0.1, 0.1, 0.0, 3e8), ("SOL", 0.2, 0.4, 0.0, 1e8)];
        let mut as_v13 = Schizo::new().unwrap();
        as_v13.active = Some(0);
        assert!(
            as_v13
                .decide(&snap(0.0, 0.0, &rows), &flat_book())
                .is_empty()
        );
        let mut as_detached = Schizo::new().unwrap();
        as_detached.active = Some(2);
        assert_eq!(
            bought(&as_detached.decide(&snap(0.0, 0.0, &rows), &flat_book())),
            vec!["SOL"]
        );
        // `detached` has no exit plan, so the buy carries none.
        assert_eq!(as_detached.exit_plan(), None);
        // Selling: the active mood's sell vote alone is enough (2), another's is not (1).
        let turn = [("SOL", -0.1, -0.2, -1.0, 1e8)];
        let intents = as_detached.decide(&snap(0.0, 0.0, &turn), &holding(&["SOL"]));
        assert_eq!(sold(&intents), vec!["SOL"]);
        let intents = as_v13.decide(&snap(0.0, 0.0, &turn), &holding(&["SOL"]));
        assert!(intents.is_empty());
    }

    #[test]
    fn schizo_decisions_up_to_a_step_do_not_change_when_later_data_is_removed() {
        let market = wavy_market(5 * 1440).with_gcp2(Some(gcp2_feed(5 * 1440, gcp2_wave)));
        let c = cfg(1440 + 30, 5 * 1440, 20);
        let intents = |market: &Market| -> (Vec<(i64, Vec<Intent>)>, Vec<String>) {
            let mut account = Account::new(Box::new(Schizo::new().unwrap()), c);
            let mut seen = Vec::new();
            let mut m = c.start_ms;
            while m < c.end_ms {
                if (m - c.start_ms) % (c.step_minutes * MINUTE_MS) == 0 {
                    let snap = Snapshot::at(market, m);
                    seen.push((m, account.step(market, &snap)));
                }
                account.minute(market, m);
                m += MINUTE_MS;
            }
            let result = account.finish(c.end_ms);
            (seen, result.trace.unwrap().rows)
        };
        let cut = T0 + (3 * 1440 + 200) * MINUTE_MS;
        let (full, trace) = intents(&market);
        let (short, _) = intents(&truncated(&market, cut));
        let upto = |s: &[(i64, Vec<Intent>)]| -> Vec<(i64, Vec<Intent>)> {
            s.iter().filter(|(t, _)| *t <= cut).cloned().collect()
        };
        assert!(upto(&full).iter().any(|(_, i)| !i.is_empty()));
        assert_eq!(upto(&full), upto(&short));
        // The wave produces every kind of mood step: one initial pick, then switches on a
        // stable zone and holds on a zone change; one trace row per step.
        assert_eq!(trace.len(), full.len());
        let count = |r: &str| {
            trace
                .iter()
                .filter(|row| row.split(',').nth(5) == Some(r))
                .count()
        };
        assert_eq!(count("initial"), 1);
        assert!(count("switched") > 0 && count("held") > 0, "{trace:?}");
        assert!(
            trace.iter().any(|row| row.ends_with(",true")),
            "no YOLO step"
        );
    }
}

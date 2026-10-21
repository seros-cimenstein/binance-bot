//! Phase 50: the long-term value voice, a Markov decision process on 20-minute steps.
//!
//! The hosted LLM cannot be trained, so the reward-and-discount idea lives in a model fitted
//! on price history and shown to the trader every cycle (information, never a gate):
//!
//! - **Step:** 20 minutes, the live cycle.
//! - **State** of a held coin ([`State`]): its gain since entry, the time it has been held,
//!   its 1h move and the 20-coin market breadth, each cut into a few buckets.
//! - **Actions:** hold (reward: the coin's return over the next step, in percent) or sell
//!   (reward: minus the exit cost; the episode ends). Buying a coin starts it in the entry
//!   state after the entry cost; staying in cash is worth 0.
//! - **Fit** ([`fit`]): every liquid coin at every 20-minute step of the cached 1m candles is an
//!   entry, followed for 24 h ([`EPISODE_STEPS`]): (state, reward, next state) transitions.
//!   Value iteration with the discount γ (default 0.95 per step: a reward one hour away
//!   counts 86%, six hours away 21%; about 20 steps of horizon) gives Q(state, hold),
//!   Q(state, sell) = −exit cost and V = max of the two.
//! - **Backoff:** a full state seen fewer than [`MIN_SAMPLES`] times, a coin without a 1h move
//!   or a market without breadth uses the coarse state (gain and time held only).
//!
//! `bb bot-spot-mdp-fit` writes the model file; the trader reads it every cycle.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, SecondsFormat, Utc};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

use crate::error::{Result, TraderError};
use crate::market_mood::LEAN_SCORE;
use crate::memory::CurrentHolding;
use crate::persona_backtest::data::{MINUTE_MS, Market, floor_to};
use crate::persona_backtest::engine::{LIQUID, Snapshot};

/// Model file format; a file of another version is refused.
pub const MODEL_VERSION: &str = "mdp-v1";
/// Minutes per step (the live cycle).
pub const STEP_MINUTES: i64 = 20;
/// Default discount per step (operator decision 2026-10-21).
pub const DEFAULT_GAMMA: f64 = 0.95;
/// Steps each fitted entry is followed (24 h).
pub const EPISODE_STEPS: usize = 72;
/// A full state seen fewer times than this uses its coarse state.
pub const MIN_SAMPLES: usize = 100;
/// Default model file (`BOT_SPOT_MDP_MODEL_PATH`).
pub const DEFAULT_MODEL_PATH: &str = "data/bot-spot-mdp.json";
/// Gain since entry, percent.
pub const GAIN_EDGES_PCT: [f64; 6] = [-3.0, -1.0, 0.0, 1.0, 2.0, 4.0];
/// Time held, minutes.
pub const HELD_EDGES_MIN: [i64; 3] = [60, 180, 360];
/// The coin's 1h move, percent.
pub const MOMENTUM_EDGES_PCT: [f64; 3] = [-1.0, 0.0, 1.0];
/// The 20-coin breadth score (the `## Market breadth` lean bounds).
pub const BREADTH_EDGES: [f64; 2] = [-LEAN_SCORE, LEAN_SCORE];

const GAIN_LABELS: [&str; 7] = [
    "below -3%",
    "-3% to -1%",
    "-1% to 0%",
    "0% to +1%",
    "+1% to +2%",
    "+2% to +4%",
    "+4% or more",
];
const HELD_LABELS: [&str; 4] = ["under 1 h", "1-3 h", "3-6 h", "6 h or more"];
const MOMENTUM_LABELS: [&str; 4] = ["below -1%", "-1% to 0%", "0% to +1%", "+1% or more"];
const BREADTH_LABELS: [&str; 3] = ["below -15", "-15 to +15", "+15 or more"];
const STEP_MS: i64 = STEP_MINUTES * MINUTE_MS;
const DAY_MS: i64 = 86_400_000;
/// Value iteration stops when no value moves more than this (percent).
const TOLERANCE: f64 = 1e-10;
const MAX_ITERATIONS: usize = 100_000;

fn bucket(x: f64, edges: &[f64]) -> u8 {
    u8::try_from(edges.iter().filter(|e| x >= **e).count()).unwrap_or(u8::MAX)
}

/// One bucketed state of a held coin. `momentum` and `breadth` are `None` in a coarse state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct State {
    pub gain: u8,
    pub held: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub momentum: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breadth: Option<u8>,
}

impl State {
    pub fn new(
        gain_pct: f64,
        held_minutes: i64,
        momentum_1h_pct: Option<f64>,
        breadth: Option<f64>,
    ) -> Self {
        let held = u8::try_from(HELD_EDGES_MIN.iter().filter(|e| held_minutes >= **e).count())
            .unwrap_or(u8::MAX);
        Self {
            gain: bucket(gain_pct, &GAIN_EDGES_PCT),
            held,
            momentum: momentum_1h_pct.map(|m| bucket(m, &MOMENTUM_EDGES_PCT)),
            breadth: breadth.map(|b| bucket(b, &BREADTH_EDGES)),
        }
    }

    pub fn is_full(&self) -> bool {
        self.momentum.is_some() && self.breadth.is_some()
    }

    pub fn coarse(self) -> Self {
        Self {
            momentum: None,
            breadth: None,
            ..self
        }
    }

    /// e.g. `gain 0% to +1%, held 1-3 h, 1h +1% or more, breadth +15 or more`.
    pub fn describe(&self) -> String {
        fn label(labels: &[&'static str], i: u8) -> &'static str {
            labels.get(usize::from(i)).copied().unwrap_or("?")
        }
        let mut out = format!(
            "gain {}, held {}",
            label(&GAIN_LABELS, self.gain),
            label(&HELD_LABELS, self.held)
        );
        if let Some(m) = self.momentum {
            out.push_str(&format!(", 1h {}", label(&MOMENTUM_LABELS, m)));
        }
        if let Some(b) = self.breadth {
            out.push_str(&format!(", breadth {}", label(&BREADTH_LABELS, b)));
        }
        out
    }
}

/// The solved values of one state, in percent of the position.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StateValue {
    pub samples: usize,
    pub mean_reward_pct: f64,
    pub q_hold_pct: f64,
    pub q_sell_pct: f64,
}

impl StateValue {
    pub fn value(&self) -> f64 {
        self.q_hold_pct.max(self.q_sell_pct)
    }

    /// Holding is worth more than selling now.
    pub fn holds(&self) -> bool {
        self.q_hold_pct > self.q_sell_pct
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateRow {
    pub state: State,
    pub value: StateValue,
}

/// Held-out check written with the model: the fit on the train days, followed on entries in
/// the test days (every liquid coin at every step, not the bot's picks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub train_from: String,
    pub train_to: String,
    pub test_from: String,
    pub test_to: String,
    pub entries: usize,
    /// Sell at the first step the model values selling over holding, else after 24 h.
    pub policy_mean_net_pct: f64,
    pub policy_win_pct: f64,
    pub policy_mean_hold_minutes: f64,
    pub hold_6h_mean_net_pct: f64,
    pub hold_24h_mean_net_pct: f64,
}

/// A fitted model, as stored in the model file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub version: String,
    pub fitted_at: String,
    pub data_from: String,
    pub data_to: String,
    pub coins: Vec<String>,
    pub gamma: f64,
    pub step_minutes: i64,
    pub entry_cost_pct: f64,
    pub exit_cost_pct: f64,
    pub transitions: usize,
    pub states: Vec<StateRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<Evaluation>,
    #[serde(skip)]
    index: BTreeMap<State, StateValue>,
}

impl Model {
    fn index(mut self) -> Self {
        self.index = self.states.iter().map(|r| (r.state, r.value)).collect();
        self
    }

    /// The model file at `path` (refuses another [`MODEL_VERSION`]).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            TraderError::config(format!("MDP model {} unreadable: {e}", path.display()))
        })?;
        let model: Self = serde_json::from_str(&text).map_err(|e| {
            TraderError::config(format!("MDP model {} is not valid: {e}", path.display()))
        })?;
        if model.version != MODEL_VERSION {
            return Err(TraderError::config(format!(
                "MDP model {} has version {:?}, expected {MODEL_VERSION:?}; refit with bb bot-spot-mdp-fit",
                path.display(),
                model.version
            )));
        }
        Ok(model.index())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Steps of horizon, `1 / (1 − γ)`.
    pub fn horizon_steps(&self) -> f64 {
        1.0 / (1.0 - self.gamma)
    }

    /// The values of `state`: the full state when it has [`MIN_SAMPLES`], else its coarse
    /// state. `None` when neither was seen.
    pub fn lookup(&self, state: State) -> Option<(&'static str, &StateValue)> {
        if state.is_full()
            && let Some(v) = self.index.get(&state).filter(|v| v.samples >= MIN_SAMPLES)
        {
            return Some(("full", v));
        }
        self.index.get(&state.coarse()).map(|v| ("coarse", v))
    }

    /// Long-term value of buying a coin now, after the entry cost (cash is worth 0).
    pub fn buy_value(&self, momentum_1h_pct: Option<f64>, breadth: Option<f64>) -> Option<(&'static str, usize, f64)> {
        self.lookup(State::new(0.0, 0, momentum_1h_pct, breadth))
            .map(|(level, v)| (level, v.samples, v.value() - self.entry_cost_pct))
    }
}

/// Discount and costs of a fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitConfig {
    pub gamma: f64,
    pub entry_cost_pct: f64,
    pub exit_cost_pct: f64,
}

impl FitConfig {
    fn validate(&self) -> Result<()> {
        if !(self.gamma > 0.0 && self.gamma < 1.0) {
            return Err(TraderError::config(format!("gamma must be in (0, 1), got {}", self.gamma)));
        }
        if self.entry_cost_pct < 0.0 || self.exit_cost_pct < 0.0 {
            return Err(TraderError::config("costs must not be negative"));
        }
        Ok(())
    }
}

/// Closes, 1h moves and breadth on the 20-minute grid `t0 + i × 20 min`.
pub struct Grid {
    t0: i64,
    len: usize,
    breadth: Vec<Option<f64>>,
    coins: Vec<CoinGrid>,
}

struct CoinGrid {
    close: Vec<Option<f64>>,
    ret_1h: Vec<Option<f64>>,
}

impl Grid {
    /// The grid over `[from_ms, to_ms]` (from rounded up to a step) of the [`LIQUID`] coins in
    /// `market`; every value uses bars closed by its step only.
    pub fn new(market: &Market, from_ms: i64, to_ms: i64) -> Self {
        let t0 = floor_to(from_ms + STEP_MS - 1, STEP_MS);
        let len = if to_ms < t0 {
            0
        } else {
            usize::try_from((to_ms - t0) / STEP_MS + 1).unwrap_or(0)
        };
        let times: Vec<i64> = (0..len).map(|i| t0 + i as i64 * STEP_MS).collect();
        let breadth = times.iter().map(|t| Snapshot::at(market, *t).score).collect();
        let coins = LIQUID
            .iter()
            .filter_map(|a| market.get(a))
            .map(|s| CoinGrid {
                close: times.iter().map(|t| s.close_at(*t)).collect(),
                ret_1h: times.iter().map(|t| s.return_pct(*t, 60)).collect(),
            })
            .collect();
        Self {
            t0,
            len,
            breadth,
            coins,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn time(&self, i: usize) -> i64 {
        self.t0 + i as i64 * STEP_MS
    }

    /// Index of the first step at or after `t`.
    pub fn index_at(&self, t: i64) -> usize {
        if t <= self.t0 {
            return 0;
        }
        usize::try_from((t - self.t0 + STEP_MS - 1) / STEP_MS)
            .unwrap_or(usize::MAX)
            .min(self.len)
    }

    fn state(&self, coin: &CoinGrid, i: usize, k: usize, entry: f64) -> Option<State> {
        let close = coin.close[i]?;
        Some(State::new(
            (close / entry - 1.0) * 100.0,
            k as i64 * STEP_MINUTES,
            coin.ret_1h[i],
            self.breadth[i],
        ))
    }
}

#[derive(Default)]
struct Counts {
    n: usize,
    reward_sum: f64,
    next: BTreeMap<State, usize>,
}

impl Counts {
    fn add(&mut self, reward: f64, next: State) {
        self.n += 1;
        self.reward_sum += reward;
        *self.next.entry(next).or_default() += 1;
    }
}

/// Transitions of the episodes entered at `entries`, never reading a step at or after `end`.
fn collect(grid: &Grid, entries: Range<usize>, end: usize) -> (BTreeMap<State, Counts>, BTreeMap<State, Counts>, usize) {
    let mut full: BTreeMap<State, Counts> = BTreeMap::new();
    let mut coarse: BTreeMap<State, Counts> = BTreeMap::new();
    let mut transitions = 0;
    let end = end.min(grid.len);
    for coin in &grid.coins {
        for i0 in entries.clone() {
            let Some(entry) = coin.close.get(i0).copied().flatten() else {
                continue;
            };
            for k in 0..EPISODE_STEPS {
                let (i, j) = (i0 + k, i0 + k + 1);
                if j >= end {
                    break;
                }
                let (Some(now), Some(next)) = (coin.close[i], coin.close[j]) else {
                    break;
                };
                let (Some(s), Some(s2)) = (grid.state(coin, i, k, entry), grid.state(coin, j, k + 1, entry)) else {
                    break;
                };
                let reward = (next / now - 1.0) * 100.0;
                coarse.entry(s.coarse()).or_default().add(reward, s2.coarse());
                if s.is_full() && s2.is_full() {
                    full.entry(s).or_default().add(reward, s2);
                }
                transitions += 1;
            }
        }
    }
    (full, coarse, transitions)
}

/// Value iteration: Q(hold) = mean reward + γ · E[V(next)], Q(sell) = −exit cost,
/// V = max. A next state never seen as a source is worth `fallback`.
fn solve(
    counts: &BTreeMap<State, Counts>,
    cfg: &FitConfig,
    fallback: &dyn Fn(State) -> f64,
) -> BTreeMap<State, StateValue> {
    let sell = -cfg.exit_cost_pct;
    let mut v: BTreeMap<State, f64> = counts.keys().map(|s| (*s, sell)).collect();
    let q_hold = |c: &Counts, v: &BTreeMap<State, f64>| {
        let n = c.n as f64;
        let expected: f64 = c
            .next
            .iter()
            .map(|(s2, m)| *m as f64 * v.get(s2).copied().unwrap_or_else(|| fallback(*s2)))
            .sum::<f64>()
            / n;
        c.reward_sum / n + cfg.gamma * expected
    };
    for _ in 0..MAX_ITERATIONS {
        let mut delta: f64 = 0.0;
        let next: BTreeMap<State, f64> = counts
            .iter()
            .map(|(s, c)| {
                let value = q_hold(c, &v).max(sell);
                delta = delta.max((value - v[s]).abs());
                (*s, value)
            })
            .collect();
        v = next;
        if delta < TOLERANCE {
            break;
        }
    }
    counts
        .iter()
        .map(|(s, c)| {
            (
                *s,
                StateValue {
                    samples: c.n,
                    mean_reward_pct: c.reward_sum / c.n as f64,
                    q_hold_pct: q_hold(c, &v),
                    q_sell_pct: sell,
                },
            )
        })
        .collect()
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map_or_else(|| ms.to_string(), |d| d.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Fit on the episodes entered at `entries` that end before step `end`.
pub fn fit(grid: &Grid, entries: Range<usize>, end: usize, cfg: &FitConfig, fitted_at: String) -> Result<Model> {
    cfg.validate()?;
    let (full, coarse, transitions) = collect(grid, entries.clone(), end);
    if transitions == 0 {
        return Err(TraderError::config("no transitions to fit: the window has no usable candles"));
    }
    let sell = -cfg.exit_cost_pct;
    let coarse_values = solve(&coarse, cfg, &|_| sell);
    let full_values = solve(&full, cfg, &|s| {
        coarse_values.get(&s.coarse()).map_or(sell, StateValue::value)
    });
    let states = coarse_values
        .iter()
        .chain(full_values.iter())
        .map(|(state, value)| StateRow {
            state: *state,
            value: *value,
        })
        .collect();
    let last = end.min(grid.len).saturating_sub(1);
    Ok(Model {
        version: MODEL_VERSION.into(),
        fitted_at,
        data_from: iso(grid.time(entries.start)),
        data_to: iso(grid.time(last)),
        coins: LIQUID.iter().map(|a| (*a).to_string()).collect(),
        gamma: cfg.gamma,
        step_minutes: STEP_MINUTES,
        entry_cost_pct: cfg.entry_cost_pct,
        exit_cost_pct: cfg.exit_cost_pct,
        transitions,
        states,
        evaluation: None,
        index: BTreeMap::new(),
    }
    .index())
}

/// Follow `model` on the entries at `entries` (each needs 24 h of candles before `grid.len`).
pub fn evaluate(model: &Model, grid: &Grid, entries: Range<usize>) -> Option<(usize, f64, f64, f64, f64, f64)> {
    let costs = model.entry_cost_pct + model.exit_cost_pct;
    let (mut n, mut policy, mut wins, mut hold_minutes, mut six, mut day) = (0usize, 0.0, 0usize, 0.0, 0.0, 0.0);
    for coin in &grid.coins {
        for i0 in entries.clone() {
            if i0 + EPISODE_STEPS >= grid.len {
                break;
            }
            let closes: Option<Vec<f64>> = (i0..=i0 + EPISODE_STEPS).map(|i| coin.close[i]).collect();
            let Some(closes) = closes else {
                continue;
            };
            let entry = closes[0];
            let mut exit_k = EPISODE_STEPS;
            for k in 0..EPISODE_STEPS {
                let sells = grid
                    .state(coin, i0 + k, k, entry)
                    .and_then(|s| model.lookup(s))
                    .is_some_and(|(_, v)| !v.holds());
                if sells {
                    exit_k = k;
                    break;
                }
            }
            let net = |k: usize| (closes[k] / entry - 1.0) * 100.0 - costs;
            let realized = net(exit_k);
            n += 1;
            policy += realized;
            wins += usize::from(realized > 0.0);
            hold_minutes += (exit_k as i64 * STEP_MINUTES) as f64;
            six += net(18);
            day += net(EPISODE_STEPS);
        }
    }
    (n > 0).then(|| {
        let m = n as f64;
        (n, policy / m, wins as f64 * 100.0 / m, hold_minutes / m, six / m, day / m)
    })
}

/// The voice on the coin the bot holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeldValue {
    pub asset: String,
    pub gain_pct: f64,
    pub held_minutes: i64,
    pub momentum_1h_pct: Option<f64>,
    pub state: String,
    /// `full` or `coarse` (see [`Model::lookup`]).
    pub level: &'static str,
    pub samples: usize,
    pub q_hold_pct: f64,
    pub q_sell_pct: f64,
    /// `hold` or `sell`: the larger long-term value.
    pub verdict: &'static str,
}

/// The voice on a coin the bot could buy now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BuyValue {
    pub asset: String,
    pub momentum_1h_pct: Option<f64>,
    /// Long-term value after the entry cost; cash is 0.
    pub value_pct: f64,
    pub level: &'static str,
    pub samples: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MdpVoice {
    pub gamma: f64,
    pub horizon_cycles: f64,
    pub data_from: String,
    pub data_to: String,
    pub fitted_at: String,
    pub breadth: Option<f64>,
    pub held: Option<HeldValue>,
    pub buys: Vec<BuyValue>,
}

/// The voice for this cycle, or why it is missing.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MdpNow {
    Ready(Box<MdpVoice>),
    Unavailable { reason: String },
}

/// The voice from the model, the bot's open position (trade memory) and the market view.
pub fn voice(model: &Model, holding: Option<&CurrentHolding>, snap: Option<&Snapshot>, now: DateTime<Utc>) -> MdpVoice {
    let breadth = snap.and_then(|s| s.score);
    let momentum = |asset: &str| snap.and_then(|s| s.coin(asset)).and_then(|c| c.ret_1h);
    let held = holding.filter(|h| h.status == "open").and_then(|h| {
        let asset = h.asset.clone()?;
        let entry = h.entry_value_usdt?.to_f64().filter(|v| *v > 0.0)?;
        let current = h.current_value_usdt?.to_f64()?;
        let entered = DateTime::parse_from_rfc3339(h.entered_at.as_deref()?).ok()?;
        let gain_pct = (current / entry - 1.0) * 100.0;
        let held_minutes = (now - entered.with_timezone(&Utc)).num_minutes().max(0);
        let momentum_1h_pct = momentum(&asset);
        let state = State::new(gain_pct, held_minutes, momentum_1h_pct, breadth);
        let (level, v) = model.lookup(state)?;
        let used = if level == "full" { state } else { state.coarse() };
        Some(HeldValue {
            asset,
            gain_pct,
            held_minutes,
            momentum_1h_pct,
            state: used.describe(),
            level,
            samples: v.samples,
            q_hold_pct: v.q_hold_pct,
            q_sell_pct: v.q_sell_pct,
            verdict: if v.holds() { "hold" } else { "sell" },
        })
    });
    let held_asset = held.as_ref().map(|h| h.asset.clone());
    let mut buys: Vec<BuyValue> = snap
        .map(|s| s.liquid_top.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|a| Some(a) != held_asset.as_ref())
        .filter_map(|asset| {
            let momentum_1h_pct = momentum(&asset);
            let (level, samples, value_pct) = model.buy_value(momentum_1h_pct, breadth)?;
            Some(BuyValue {
                asset,
                momentum_1h_pct,
                value_pct,
                level,
                samples,
            })
        })
        .collect();
    buys.sort_by(|a, b| b.value_pct.total_cmp(&a.value_pct).then_with(|| a.asset.cmp(&b.asset)));
    MdpVoice {
        gamma: model.gamma,
        horizon_cycles: model.horizon_steps(),
        data_from: model.data_from.clone(),
        data_to: model.data_to.clone(),
        fitted_at: model.fitted_at.clone(),
        breadth,
        held,
        buys,
    }
}

/// Fit the long-term value model on cached 1m candles (offline, no orders).
#[derive(clap::Args, Debug, Clone)]
pub struct MdpFitArgs {
    /// Directory of `1m_<BASE>_USDT_<start>_<end>.json` files (the `bb entry-study` cache).
    #[arg(long, default_value = "data/entry-study-cache")]
    pub cache_dir: PathBuf,
    /// Last data time used (ISO 8601); default: the newest cached candle.
    #[arg(long)]
    pub end: Option<String>,
    /// Days of data before `--end` (default: everything cached).
    #[arg(long)]
    pub days: Option<i64>,
    /// Discount per 20-minute step.
    #[arg(long, default_value_t = DEFAULT_GAMMA)]
    pub gamma: f64,
    /// Fee per side in bps.
    #[arg(long, default_value_t = 7.5)]
    pub fee_bps: f64,
    /// Slippage per side in bps.
    #[arg(long, default_value_t = 5.0)]
    pub slippage_bps: f64,
    /// Days at the end held out for the evaluation written with the model (0: none). The
    /// saved model is then refitted on every day.
    #[arg(long, default_value_t = 20)]
    pub test_days: i64,
    /// Model file to write.
    #[arg(long, default_value = DEFAULT_MODEL_PATH)]
    pub output: PathBuf,
}

fn parse_time(value: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|d| d.timestamp_millis())
        .map_err(|e| TraderError::config(format!("bad time {value:?}: {e}")))
}

/// Fit (and evaluate) per `args`; returns the model, not yet saved.
pub fn fit_from_args(args: &MdpFitArgs, market: &Market, fitted_at: String) -> Result<Model> {
    let bounds: Vec<(i64, i64)> = LIQUID
        .iter()
        .filter_map(|a| market.get(a))
        .filter_map(|s| Some((s.first_open_ms()?, s.last_open_ms()? + MINUTE_MS)))
        .collect();
    let (Some(first), Some(last)) = (
        bounds.iter().map(|b| b.0).min(),
        bounds.iter().map(|b| b.1).max(),
    ) else {
        return Err(TraderError::config("the cache has none of the liquid coins"));
    };
    let to = match &args.end {
        Some(end) => parse_time(end)?.min(last),
        None => last,
    };
    let from = args.days.map_or(first, |d| (to - d * DAY_MS).max(first));
    // One day of warm-up for the 24h breadth.
    let grid = Grid::new(market, from + DAY_MS, to);
    if grid.is_empty() {
        return Err(TraderError::config("the fit window is shorter than its one-day warm-up"));
    }
    let cfg = FitConfig {
        gamma: args.gamma,
        entry_cost_pct: (args.fee_bps + args.slippage_bps) / 100.0,
        exit_cost_pct: (args.fee_bps + args.slippage_bps) / 100.0,
    };
    let mut model = fit(&grid, 0..grid.len(), grid.len(), &cfg, fitted_at.clone())?;
    if args.test_days > 0 {
        let split = grid.index_at(to - args.test_days * DAY_MS);
        if split == 0 || split >= grid.len() {
            return Err(TraderError::config("--test-days leaves no train or no test days"));
        }
        let train = fit(&grid, 0..split, split, &cfg, fitted_at)?;
        if let Some((entries, policy, win, hold, six, day)) = evaluate(&train, &grid, split..grid.len()) {
            model.evaluation = Some(Evaluation {
                train_from: iso(grid.time(0)),
                train_to: iso(grid.time(split - 1)),
                test_from: iso(grid.time(split)),
                test_to: iso(grid.time(grid.len() - 1)),
                entries,
                policy_mean_net_pct: policy,
                policy_win_pct: win,
                policy_mean_hold_minutes: hold,
                hold_6h_mean_net_pct: six,
                hold_24h_mean_net_pct: day,
            });
        }
    }
    Ok(model)
}

/// `bb bot-spot-mdp-fit`.
pub fn run_fit_cli(args: &MdpFitArgs) -> Result<ExitCode> {
    let market = Market::load_dir(&args.cache_dir)?;
    let fitted_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let model = fit_from_args(args, &market, fitted_at)?;
    model.save(&args.output)?;
    println!(
        "MDP model {}: γ={} per {}-minute step (horizon about {:.0} steps), data {} → {}, {} transitions, {} states",
        args.output.display(),
        model.gamma,
        model.step_minutes,
        model.horizon_steps(),
        model.data_from,
        model.data_to,
        model.transitions,
        model.states.len()
    );
    for row in model.states.iter().filter(|r| !r.state.is_full()) {
        println!(
            "  {:<45} n={:>8}  next step {:+.4}%  hold {:+.3}%  sell {:+.3}%  → {}",
            row.state.describe(),
            row.value.samples,
            row.value.mean_reward_pct,
            row.value.q_hold_pct,
            row.value.q_sell_pct,
            if row.value.holds() { "hold" } else { "sell" }
        );
    }
    match &model.evaluation {
        Some(e) => println!(
            "Held-out (fit {} → {}, test {} → {}, {} entries, net after costs): policy {:+.3}% (win {:.1}%, mean hold {:.0} min); hold 6 h {:+.3}%; hold 24 h {:+.3}%",
            e.train_from,
            e.train_to,
            e.test_from,
            e.test_to,
            e.entries,
            e.policy_mean_net_pct,
            e.policy_win_pct,
            e.policy_mean_hold_minutes,
            e.hold_6h_mean_net_pct,
            e.hold_24h_mean_net_pct
        ),
        None => println!("No held-out evaluation (--test-days 0 or no complete test entry)."),
    }
    Ok(ExitCode::SUCCESS)
}

/// A model with the given coarse or full states (all with 5,000 samples) for tests.
#[cfg(test)]
pub(crate) fn test_model(states: &[(State, f64)]) -> Model {
    Model {
        version: MODEL_VERSION.into(),
        fitted_at: "test".into(),
        data_from: "a".into(),
        data_to: "b".into(),
        coins: Vec::new(),
        gamma: DEFAULT_GAMMA,
        step_minutes: STEP_MINUTES,
        entry_cost_pct: 0.125,
        exit_cost_pct: 0.125,
        transitions: 1,
        states: states
            .iter()
            .map(|(state, q)| StateRow {
                state: *state,
                value: StateValue {
                    samples: 5_000,
                    mean_reward_pct: 0.0,
                    q_hold_pct: *q,
                    q_sell_pct: -0.125,
                },
            })
            .collect(),
        evaluation: None,
        index: BTreeMap::new(),
    }
    .index()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona_backtest::data::{Bar, Series};

    const CFG: FitConfig = FitConfig {
        gamma: 0.95,
        entry_cost_pct: 0.125,
        exit_cost_pct: 0.125,
    };

    fn s(gain: u8) -> State {
        State {
            gain,
            held: 0,
            momentum: None,
            breadth: None,
        }
    }

    fn counts(reward: f64, next: State, n: usize) -> Counts {
        Counts {
            n,
            reward_sum: reward * n as f64,
            next: [(next, n)].into(),
        }
    }

    #[test]
    fn buckets_take_the_lower_edge_inclusive() {
        let st = State::new(0.0, 59, Some(1.0), Some(-15.0));
        assert_eq!((st.gain, st.held, st.momentum, st.breadth), (3, 0, Some(3), Some(1)));
        let st = State::new(-3.01, 360, Some(-1.01), Some(15.0));
        assert_eq!((st.gain, st.held, st.momentum, st.breadth), (0, 3, Some(0), Some(2)));
        assert_eq!(State::new(4.0, 61, None, None).gain, 6);
        assert!(!State::new(1.0, 1, Some(0.1), None).is_full());
        assert_eq!(
            State::new(1.5, 200, Some(2.0), Some(30.0)).describe(),
            "gain +1% to +2%, held 3-6 h, 1h +1% or more, breadth +15 or more"
        );
    }

    #[test]
    fn steady_gains_are_worth_holding_and_steady_losses_selling() {
        // One state that keeps paying +0.1% per step: V = 0.1 / (1 − 0.95) = 2.
        let up: BTreeMap<State, Counts> = [(s(4), counts(0.1, s(4), 10))].into();
        let v = solve(&up, &CFG, &|_| -0.125)[&s(4)];
        assert!((v.q_hold_pct - 2.0).abs() < 1e-6, "{v:?}");
        assert!(v.holds());
        // −0.5% per step: holding loses more than the exit cost.
        let down: BTreeMap<State, Counts> = [(s(1), counts(-0.5, s(1), 10))].into();
        let v = solve(&down, &CFG, &|_| -0.125)[&s(1)];
        assert!(!v.holds());
        assert!((v.q_hold_pct - (-0.5 + 0.95 * -0.125)).abs() < 1e-9, "{v:?}");
    }

    #[test]
    fn a_higher_gamma_holds_through_a_dip_that_a_lower_one_sells() {
        // A: −0.3% to reach B; B pays +0.05% per step forever.
        let chain: BTreeMap<State, Counts> = [(s(2), counts(-0.3, s(5), 10)), (s(5), counts(0.05, s(5), 10))].into();
        let patient = solve(&chain, &CFG, &|_| -0.125);
        assert!(patient[&s(2)].holds(), "{:?}", patient[&s(2)]);
        let impatient = solve(&chain, &FitConfig { gamma: 0.6, ..CFG }, &|_| -0.125);
        assert!(!impatient[&s(2)].holds(), "{:?}", impatient[&s(2)]);
    }

    /// A rising coin and a falling one on the 20-minute grid, 3 days of 1m bars.
    fn market() -> Market {
        let t0: i64 = 1_784_448_000_000;
        let series = |asset: &str, per_minute: f64| {
            let bars = (0..3 * 1440)
                .map(|m| {
                    let price = 100.0 * (1.0 + per_minute).powi(m);
                    Bar {
                        open_ms: t0 + i64::from(m) * MINUTE_MS,
                        open: price,
                        high: price,
                        low: price,
                        close: price,
                        volume: 1.0,
                    }
                })
                .collect();
            Series::new(asset, bars)
        };
        Market::from_series([series("BTC", 0.0001), series("ETH", -0.0001)])
    }

    #[test]
    fn a_fit_holds_the_winning_states_sells_the_losing_ones_and_stays_point_in_time() {
        let market = market();
        let t0: i64 = 1_784_448_000_000;
        let grid = Grid::new(&market, t0 + 2 * 60 * MINUTE_MS, t0 + 3 * 1440 * MINUTE_MS);
        let model = fit(&grid, 0..grid.len(), grid.len(), &CFG, "now".into()).unwrap();
        // BTC rises +0.6% per hour: its states pay; ETH falls as much: its states cost.
        let rising = State::new(1.5, 100, Some(0.6), None);
        let falling = State::new(-1.5, 100, Some(-0.6), None);
        let (_, up) = model.lookup(rising).unwrap();
        let (_, down) = model.lookup(falling).unwrap();
        assert!(up.holds() && up.mean_reward_pct > 0.0, "{up:?}");
        assert!(!down.holds() && down.mean_reward_pct < 0.0, "{down:?}");
        // A fit that ends at step `split` reads nothing after it: the same as a fit on a grid
        // that stops there.
        let split = grid.len() / 2;
        let short = Grid::new(&market, t0 + 2 * 60 * MINUTE_MS, grid.time(split - 1));
        let a = fit(&grid, 0..split, split, &CFG, "now".into()).unwrap();
        let b = fit(&short, 0..short.len(), short.len(), &CFG, "now".into()).unwrap();
        assert_eq!(a.states, b.states);
        assert_eq!(a.transitions, b.transitions);
    }

    #[test]
    fn a_rare_full_state_backs_off_to_its_coarse_state() {
        let full = State::new(0.5, 30, Some(0.5), Some(20.0));
        let row = |state: State, samples: usize, q: f64| StateRow {
            state,
            value: StateValue {
                samples,
                mean_reward_pct: 0.0,
                q_hold_pct: q,
                q_sell_pct: -0.125,
            },
        };
        let mut model = Model {
            version: MODEL_VERSION.into(),
            fitted_at: "now".into(),
            data_from: "a".into(),
            data_to: "b".into(),
            coins: vec![],
            gamma: 0.95,
            step_minutes: STEP_MINUTES,
            entry_cost_pct: 0.125,
            exit_cost_pct: 0.125,
            transitions: 1,
            states: vec![row(full.coarse(), 5_000, 0.4), row(full, MIN_SAMPLES - 1, -1.0)],
            evaluation: None,
            index: BTreeMap::new(),
        }
        .index();
        assert_eq!(model.lookup(full).map(|(l, v)| (l, v.q_hold_pct)), Some(("coarse", 0.4)));
        model.states[1].value.samples = MIN_SAMPLES;
        let model = model.index();
        assert_eq!(model.lookup(full).map(|(l, v)| (l, v.q_hold_pct)), Some(("full", -1.0)));
        // Buying: the entry state's value minus the entry cost (no 1h move: coarse).
        let (level, _, value) = model.buy_value(None, Some(20.0)).unwrap();
        assert_eq!(level, "coarse");
        assert!((value - (0.4 - 0.125)).abs() < 1e-12);
    }

    #[test]
    fn the_live_voice_values_the_open_position_and_ranks_buys_without_it() {
        use rust_decimal_macros::dec;

        use crate::persona_backtest::engine::CoinView;
        let value = |samples: usize, q: f64| StateValue {
            samples,
            mean_reward_pct: 0.0,
            q_hold_pct: q,
            q_sell_pct: -0.125,
        };
        let rows = vec![
            // Held ENA: +2% after 130 min (coarse: ENA has no 1h move in the view).
            StateRow { state: State::new(2.0, 130, None, None), value: value(5_000, 0.3) },
            // Entry state: 0.2 coarse; a +1% hour in a bullish market: 0.5.
            StateRow { state: State::new(0.0, 0, None, None), value: value(5_000, 0.2) },
            StateRow { state: State::new(0.0, 0, Some(2.0), Some(30.0)), value: value(MIN_SAMPLES, 0.5) },
        ];
        let model = Model {
            version: MODEL_VERSION.into(),
            fitted_at: "f".into(),
            data_from: "a".into(),
            data_to: "b".into(),
            coins: vec![],
            gamma: 0.95,
            step_minutes: STEP_MINUTES,
            entry_cost_pct: 0.125,
            exit_cost_pct: 0.125,
            transitions: 1,
            states: rows,
            evaluation: None,
            index: BTreeMap::new(),
        }
        .index();
        let coin = |ret_1h: f64| CoinView {
            ret_15m: None,
            ret_1h: Some(ret_1h),
            ret_4h: None,
            quote_volume_24h: 1.0,
        };
        let snap = Snapshot {
            t: 0,
            score: Some(30.0),
            horizons: Vec::new(),
            coins: [("BTC".to_string(), coin(0.2)), ("SOL".to_string(), coin(2.0))].into(),
            liquid_top: vec!["BTC".into(), "ENA".into(), "SOL".into()],
            gcp2: None,
        };
        let holding = CurrentHolding {
            status: "open",
            asset: Some("ENA".into()),
            reason: None,
            amount: Some(dec!(69.17)),
            entry_value_usdt: Some(dec!(10)),
            current_value_usdt: Some(dec!(10.2)),
            unrealized_pnl_bps: None,
            entered_at: Some("2026-10-21T10:00:00+00:00".into()),
            route_id: None,
        };
        let now = DateTime::parse_from_rfc3339("2026-10-21T12:10:00Z").unwrap().with_timezone(&Utc);
        let voice = voice(&model, Some(&holding), Some(&snap), now);
        let held = voice.held.unwrap();
        assert_eq!((held.asset.as_str(), held.held_minutes, held.level, held.verdict), ("ENA", 130, "coarse", "hold"));
        assert!((held.gain_pct - 2.0).abs() < 1e-9);
        assert_eq!(held.state, "gain +2% to +4%, held 1-3 h");
        // Buys: SOL's +2% hour in a bullish market ranks first; the held ENA is left out.
        let buys: Vec<(&str, &str)> = voice.buys.iter().map(|b| (b.asset.as_str(), b.level)).collect();
        assert_eq!(buys, vec![("SOL", "full"), ("BTC", "coarse")]);
        assert!((voice.buys[0].value_pct - (0.5 - 0.125)).abs() < 1e-12);
        // Cash, or a position that is not open, gets no held value.
        let cash = CurrentHolding { status: "cash", ..holding };
        assert!(super::voice(&model, Some(&cash), None, now).held.is_none());
    }

    #[test]
    fn the_model_file_round_trips_and_refuses_another_version() {
        let market = market();
        let t0: i64 = 1_784_448_000_000;
        let grid = Grid::new(&market, t0 + 2 * 60 * MINUTE_MS, t0 + 3 * 1440 * MINUTE_MS);
        let model = fit(&grid, 0..grid.len(), grid.len(), &CFG, "now".into()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.json");
        model.save(&path).unwrap();
        let loaded = Model::load(&path).unwrap();
        // Every state gives the same verdict and values (to float precision) after reload.
        assert_eq!(loaded.states.len(), model.states.len());
        for row in &model.states {
            let (level, v) = loaded.lookup(row.state).unwrap();
            let (want_level, want) = model.lookup(row.state).unwrap();
            assert_eq!((level, v.samples, v.holds()), (want_level, want.samples, want.holds()));
            assert!((v.q_hold_pct - want.q_hold_pct).abs() < 1e-12);
        }
        let mut other = model.clone();
        other.version = "mdp-v0".into();
        other.save(&path).unwrap();
        assert!(Model::load(&path).unwrap_err().to_string().contains("mdp-v0"));
    }
}

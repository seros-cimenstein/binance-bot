//! Exit-plan menu (Phase 45): the sell plans Claude may choose for a position.
//!
//! Claude never writes code. It picks parameters for a fixed menu and the bot evaluates
//! the plan on prices it sees between cycles:
//!
//! - follow the peak: sell everything once the price falls a set distance below the
//!   highest price since entry, either a fixed percent or a multiple of the coin's
//!   average true range on a chosen candle size (Chandelier exit);
//! - take part: sell a fraction of the position once, at a set gain;
//! - stretch: sell a fraction once the price is a set number of standard deviations
//!   above its 20-minute average (upper Bollinger band), while above entry;
//! - slowdown: sell a fraction once the price is still rising but its speed has fallen
//!   for 3 minutes in a row (velocity > 0, acceleration < 0), while above entry;
//! - momentum fade: after being up at least a set gain, sell everything once no new
//!   high has printed for N minutes;
//! - deadline: sell everything N minutes after entry.
//!
//! A plan with no rule holds. Bars are evaluated pessimistically for a long position:
//! the low is assumed to come before the high, and the peak only moves after the stop
//! was checked. Stretch and slowdown read completed 1-minute closes only; a slowdown
//! seen at a minute's end sells at the next observed price. A live tick is a bar whose
//! four prices are equal, so a watcher can feed [`ExitWatch::on_bar`] directly.

use std::collections::VecDeque;
use std::fmt;

use rust_decimal::{Decimal, MathematicalOps};
use serde::{Deserialize, Serialize};

const MS_PER_MINUTE: i64 = 60_000;
/// Candles averaged for the true range (the Chandelier exit's standard 22).
pub const ATR_PERIODS: usize = 22;
/// Minute closes behind the stretch band (Bollinger's standard 20).
pub const STRETCH_MINUTES: usize = 20;
/// Consecutive slowing minutes that trigger the slowdown rule.
pub const SLOWDOWN_PERSIST_MINUTES: u32 = 3;
const MAX_SLOW_MINUTES: u32 = 60;
/// Minute closes kept: the stretch window or the longest slowdown window plus history.
const MINUTE_HISTORY: usize = MAX_SLOW_MINUTES as usize + 2;

/// One price bar. A tick is a bar with `open == high == low == close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceBar {
    pub open_ms: i64,
    pub close_ms: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
}

impl PriceBar {
    /// A single observed price.
    pub fn tick(ts_ms: i64, price: Decimal) -> Self {
        Self {
            open_ms: ts_ms,
            close_ms: ts_ms,
            open: price,
            high: price,
            low: price,
            close: price,
        }
    }
}

/// Candle size of the average true range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AtrTimeframe {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "1h")]
    H1,
}

impl AtrTimeframe {
    pub const ALL: [Self; 4] = [Self::M1, Self::M5, Self::M15, Self::H1];

    /// Binance kline interval.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::H1 => "1h",
        }
    }

    pub fn millis(self) -> i64 {
        MS_PER_MINUTE
            * match self {
                Self::M1 => 1,
                Self::M5 => 5,
                Self::M15 => 15,
                Self::H1 => 60,
            }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|tf| tf.as_str() == value.trim())
            .ok_or_else(|| format!("ATR timeframe must be one of 1m, 5m, 15m, 1h, got {value:?}"))
    }
}

/// How far below the peak the follow-the-peak rule sells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trail {
    /// Percent below the peak.
    Pct(Decimal),
    /// `multiple` average true ranges of `timeframe` candles below the peak (Chandelier).
    Atr {
        multiple: Decimal,
        timeframe: AtrTimeframe,
    },
}

/// Sell `fraction` of the original position once the price is `gain_pct` above entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakePart {
    pub gain_pct: Decimal,
    pub fraction: Decimal,
}

/// Sell `fraction` once the price reaches `z` standard deviations above its 20-minute
/// average, while that line is above entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stretch {
    pub z: Decimal,
    pub fraction: Decimal,
}

/// Sell `fraction` once the `minutes`-minute speed is positive but has fallen for
/// [`SLOWDOWN_PERSIST_MINUTES`] minutes in a row, while the price is above entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slowdown {
    pub fraction: Decimal,
    pub minutes: u32,
}

/// After the peak is at least `min_gain_pct` above entry, sell everything once no new
/// high has printed for `minutes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fade {
    pub min_gain_pct: Decimal,
    pub minutes: u32,
}

/// One sell plan from the menu. Every rule is optional; an empty plan holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitPlan {
    pub trail: Option<Trail>,
    pub take_part: Option<TakePart>,
    #[serde(default)]
    pub stretch: Option<Stretch>,
    #[serde(default)]
    pub slowdown: Option<Slowdown>,
    pub fade: Option<Fade>,
    pub deadline_minutes: Option<u32>,
}

/// Why a fill happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    FollowPeak,
    TakePart,
    Stretch,
    Slowdown,
    Fade,
    Deadline,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FollowPeak => "follow_peak",
            Self::TakePart => "take_part",
            Self::Stretch => "stretch",
            Self::Slowdown => "slowdown",
            Self::Fade => "fade",
            Self::Deadline => "deadline",
        }
    }
}

/// One simulated sell: `fraction` of the original position at `price`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitFill {
    pub ts_ms: i64,
    pub price: Decimal,
    pub fraction: Decimal,
    pub reason: ExitReason,
}

fn check_fraction(name: &str, fraction: Decimal, allow_all: bool) -> Result<(), String> {
    let too_big = if allow_all {
        fraction > Decimal::ONE
    } else {
        fraction >= Decimal::ONE
    };
    if fraction <= Decimal::ZERO || too_big {
        let range = if allow_all { "(0, 1]" } else { "(0, 1)" };
        return Err(format!(
            "{name} fraction must be in {range}, got {fraction}"
        ));
    }
    Ok(())
}

impl ExitPlan {
    /// Reject values that cannot describe a sell plan.
    pub fn validate(&self) -> Result<(), String> {
        match self.trail {
            Some(Trail::Pct(pct)) if pct <= Decimal::ZERO || pct >= Decimal::ONE_HUNDRED => {
                return Err(format!("trail percent must be in (0, 100), got {pct}"));
            }
            Some(Trail::Atr { multiple, .. }) if multiple <= Decimal::ZERO => {
                return Err(format!("ATR multiple must be positive, got {multiple}"));
            }
            _ => {}
        }
        if let Some(part) = self.take_part {
            if part.gain_pct <= Decimal::ZERO {
                return Err(format!(
                    "take-part gain must be positive, got {}",
                    part.gain_pct
                ));
            }
            check_fraction("take-part", part.fraction, false)?;
        }
        if let Some(stretch) = self.stretch {
            if stretch.z <= Decimal::ZERO {
                return Err(format!("stretch z must be positive, got {}", stretch.z));
            }
            check_fraction("stretch", stretch.fraction, true)?;
        }
        if let Some(slow) = self.slowdown {
            if !(2..=MAX_SLOW_MINUTES).contains(&slow.minutes) {
                return Err(format!(
                    "slowdown minutes must be in 2..={MAX_SLOW_MINUTES}, got {}",
                    slow.minutes
                ));
            }
            check_fraction("slowdown", slow.fraction, true)?;
        }
        if let Some(fade) = self.fade {
            if fade.min_gain_pct < Decimal::ZERO {
                return Err(format!(
                    "fade minimum gain must not be negative, got {}",
                    fade.min_gain_pct
                ));
            }
            if fade.minutes == 0 {
                return Err("fade minutes must be positive".into());
            }
        }
        if self.deadline_minutes == Some(0) {
            return Err("deadline minutes must be positive".into());
        }
        Ok(())
    }

    /// Candle size whose average true range the plan needs, if any.
    pub fn atr_timeframe(&self) -> Option<AtrTimeframe> {
        match self.trail {
            Some(Trail::Atr { timeframe, .. }) => Some(timeframe),
            _ => None,
        }
    }

    /// Parse the compact spec used on the command line: comma-separated `key=value`
    /// with `trail=<pct>`, `atr=<multiple>:<1m|5m|15m|1h>`, `part=<gain_pct>:<fraction>`,
    /// `stretch=<z>:<fraction>`, `slow=<fraction>:<minutes>`,
    /// `fade=<min_gain_pct>:<minutes>`, `deadline=<minutes>`; `hold` is the empty plan.
    pub fn parse_spec(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let mut plan = Self::default();
        if spec.eq_ignore_ascii_case("hold") {
            return Ok(plan);
        }
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, value) = item
                .split_once('=')
                .ok_or_else(|| format!("expected key=value, got {item:?}"))?;
            let value = value.trim();
            match key.trim() {
                "trail" | "atr" if plan.trail.is_some() => {
                    return Err("only one of trail / atr may be set".into());
                }
                "trail" => plan.trail = Some(Trail::Pct(decimal(value)?)),
                "atr" => {
                    let (multiple, timeframe) = pair(value)?;
                    plan.trail = Some(Trail::Atr {
                        multiple: decimal(multiple)?,
                        timeframe: AtrTimeframe::parse(timeframe)?,
                    });
                }
                "part" => {
                    let (gain, fraction) = pair(value)?;
                    plan.take_part = Some(TakePart {
                        gain_pct: decimal(gain)?,
                        fraction: decimal(fraction)?,
                    });
                }
                "stretch" => {
                    let (z, fraction) = pair(value)?;
                    plan.stretch = Some(Stretch {
                        z: decimal(z)?,
                        fraction: decimal(fraction)?,
                    });
                }
                "slow" => {
                    let (fraction, minutes) = pair(value)?;
                    plan.slowdown = Some(Slowdown {
                        fraction: decimal(fraction)?,
                        minutes: minutes_value(minutes)?,
                    });
                }
                "fade" => {
                    let (gain, minutes) = pair(value)?;
                    plan.fade = Some(Fade {
                        min_gain_pct: decimal(gain)?,
                        minutes: minutes_value(minutes)?,
                    });
                }
                "deadline" => plan.deadline_minutes = Some(minutes_value(value)?),
                other => return Err(format!("unknown exit-plan key {other:?}")),
            }
        }
        if plan == Self::default() {
            return Err(format!("empty exit plan {spec:?} (use \"hold\")"));
        }
        plan.validate()?;
        Ok(plan)
    }
}

impl fmt::Display for ExitPlan {
    /// The canonical [`ExitPlan::parse_spec`] form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        match self.trail {
            Some(Trail::Pct(pct)) => parts.push(format!("trail={}", pct.normalize())),
            Some(Trail::Atr {
                multiple,
                timeframe,
            }) => parts.push(format!(
                "atr={}:{}",
                multiple.normalize(),
                timeframe.as_str()
            )),
            None => {}
        }
        if let Some(part) = self.take_part {
            parts.push(format!(
                "part={}:{}",
                part.gain_pct.normalize(),
                part.fraction.normalize()
            ));
        }
        if let Some(stretch) = self.stretch {
            parts.push(format!(
                "stretch={}:{}",
                stretch.z.normalize(),
                stretch.fraction.normalize()
            ));
        }
        if let Some(slow) = self.slowdown {
            parts.push(format!(
                "slow={}:{}",
                slow.fraction.normalize(),
                slow.minutes
            ));
        }
        if let Some(fade) = self.fade {
            parts.push(format!(
                "fade={}:{}",
                fade.min_gain_pct.normalize(),
                fade.minutes
            ));
        }
        if let Some(minutes) = self.deadline_minutes {
            parts.push(format!("deadline={minutes}"));
        }
        if parts.is_empty() {
            f.write_str("hold")
        } else {
            f.write_str(&parts.join(","))
        }
    }
}

/// Spec keys of the menu rules, in [`ExitPlan`] spec order.
pub const RULE_KEYS: [&str; 7] = ["trail", "atr", "part", "stretch", "slow", "fade", "deadline"];

/// A set of menu rules by spec key, e.g. `atr,stretch`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuleSet(u8);

impl RuleSet {
    /// The rules `plan` sets.
    pub fn of(plan: &ExitPlan) -> Self {
        let present = [
            matches!(plan.trail, Some(Trail::Pct(_))),
            matches!(plan.trail, Some(Trail::Atr { .. })),
            plan.take_part.is_some(),
            plan.stretch.is_some(),
            plan.slowdown.is_some(),
            plan.fade.is_some(),
            plan.deadline_minutes.is_some(),
        ];
        Self(
            present
                .iter()
                .enumerate()
                .filter(|(_, set)| **set)
                .fold(0, |bits, (i, _)| bits | 1 << i),
        )
    }

    /// A comma-separated list of spec keys, e.g. `atr,stretch`.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut bits = 0u8;
        for key in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            let index = RULE_KEYS.iter().position(|k| *k == key).ok_or_else(|| {
                format!(
                    "unknown exit-plan rule {key:?} (menu: {})",
                    RULE_KEYS.join(", ")
                )
            })?;
            bits |= 1 << index;
        }
        let set = Self(bits);
        if bits == 0 {
            return Err(format!("no exit-plan rule in {list:?}"));
        }
        if set.contains("trail") && set.contains("atr") {
            return Err("only one of trail / atr may be set".into());
        }
        Ok(set)
    }

    /// Spec keys in the set, in spec order.
    pub fn keys(self) -> impl Iterator<Item = &'static str> {
        RULE_KEYS
            .into_iter()
            .enumerate()
            .filter(move |(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, key)| key)
    }

    pub fn contains(self, key: &str) -> bool {
        self.keys().any(|k| k == key)
    }
}

impl fmt::Display for RuleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.keys().collect::<Vec<_>>().join(","))
    }
}

fn decimal(value: &str) -> Result<Decimal, String> {
    value
        .parse::<Decimal>()
        .map_err(|_| format!("not a number: {value:?}"))
}

fn minutes_value(value: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("not a whole number of minutes: {value:?}"))
}

fn pair(value: &str) -> Result<(&str, &str), String> {
    value
        .split_once(':')
        .ok_or_else(|| format!("expected a:b, got {value:?}"))
}

/// Average true range of the last [`ATR_PERIODS`] candles in `bars` (oldest first): the
/// mean of `max(high - low, |high - previous close|, |low - previous close|)`. `None`
/// without a candle or when the range is zero.
pub fn average_true_range(bars: &[PriceBar]) -> Option<Decimal> {
    let start = bars.len().saturating_sub(ATR_PERIODS);
    let ranges: Vec<Decimal> = bars
        .iter()
        .enumerate()
        .skip(start)
        .map(|(i, bar)| {
            let range = bar.high - bar.low;
            match i.checked_sub(1).map(|p| bars[p].close) {
                Some(prev) => range
                    .max((bar.high - prev).abs())
                    .max((bar.low - prev).abs()),
                None => range,
            }
        })
        .collect();
    if ranges.is_empty() {
        return None;
    }
    let atr = ranges.iter().sum::<Decimal>() / Decimal::from(ranges.len());
    (atr > Decimal::ZERO).then_some(atr)
}

/// Completed 1-minute closes with their EMA-based speed and acceleration.
#[derive(Debug, Clone, Default)]
struct MinuteSeries {
    /// Minute (ms / 60 000) of the last observation and its price.
    current: Option<(i64, Decimal)>,
    closes: VecDeque<Decimal>,
    emas: VecDeque<Decimal>,
    last_velocity: Option<Decimal>,
    slowing_minutes: u32,
}

impl MinuteSeries {
    /// Record a price; returns the close of the minute it completed, if any.
    fn observe(&mut self, ts_ms: i64, price: Decimal, ema_minutes: Option<u32>) -> Option<Decimal> {
        let minute = ts_ms.div_euclid(MS_PER_MINUTE);
        let completed = match self.current {
            Some((open_minute, close)) if minute > open_minute => {
                self.complete(close, ema_minutes);
                Some(close)
            }
            _ => None,
        };
        self.current = Some((minute, price));
        completed
    }

    fn complete(&mut self, close: Decimal, ema_minutes: Option<u32>) {
        push_bounded(&mut self.closes, close);
        let Some(n) = ema_minutes else {
            return;
        };
        let alpha = Decimal::TWO / Decimal::from(n + 1);
        let ema = self
            .emas
            .back()
            .map_or(close, |prev| *prev + alpha * (close - *prev));
        push_bounded(&mut self.emas, ema);
        let lag = n as usize;
        let velocity = (self.emas.len() > lag)
            .then(|| {
                let past = self.emas[self.emas.len() - 1 - lag];
                (past > Decimal::ZERO).then(|| ema / past - Decimal::ONE)
            })
            .flatten();
        let slowing = match (velocity, self.last_velocity) {
            (Some(v), Some(prev)) => v > Decimal::ZERO && v < prev,
            _ => false,
        };
        self.slowing_minutes = if slowing { self.slowing_minutes + 1 } else { 0 };
        self.last_velocity = velocity;
    }

    /// `mean + z * stdev` of the last [`STRETCH_MINUTES`] closes.
    fn upper_band(&self, z: Decimal) -> Option<Decimal> {
        if self.closes.len() < STRETCH_MINUTES {
            return None;
        }
        let window: Vec<Decimal> = self
            .closes
            .iter()
            .rev()
            .take(STRETCH_MINUTES)
            .copied()
            .collect();
        let n = Decimal::from(STRETCH_MINUTES);
        let mean = window.iter().sum::<Decimal>() / n;
        let variance = window
            .iter()
            .map(|c| (*c - mean) * (*c - mean))
            .sum::<Decimal>()
            / n;
        Some(mean + z * variance.sqrt()?)
    }
}

fn push_bounded(series: &mut VecDeque<Decimal>, value: Decimal) {
    if series.len() == MINUTE_HISTORY {
        series.pop_front();
    }
    series.push_back(value);
}

/// Concrete follow-the-peak distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrailDistance {
    Pct(Decimal),
    Absolute(Decimal),
}

/// A running plan for one long position.
#[derive(Debug, Clone)]
pub struct ExitWatch {
    entry_price: Decimal,
    entry_ms: i64,
    trail: Option<TrailDistance>,
    take_part: Option<TakePart>,
    stretch: Option<Stretch>,
    slowdown: Option<Slowdown>,
    fade: Option<Fade>,
    deadline_ms: Option<i64>,
    peak: Decimal,
    last_high_ms: i64,
    part_taken: bool,
    stretch_taken: bool,
    slowdown_taken: bool,
    /// A slowdown seen at the last minute's end, sold at the next observed price.
    slowdown_due: bool,
    minutes: MinuteSeries,
    remaining: Decimal,
}

impl ExitWatch {
    /// Start watching. An ATR trail needs `atr`, the average true range of its timeframe
    /// before entry (see [`average_true_range`]).
    pub fn new(
        plan: &ExitPlan,
        entry_price: Decimal,
        entry_ms: i64,
        atr: Option<Decimal>,
    ) -> Result<Self, String> {
        plan.validate()?;
        if entry_price <= Decimal::ZERO {
            return Err(format!("entry price must be positive, got {entry_price}"));
        }
        let trail = match plan.trail {
            None => None,
            Some(Trail::Pct(pct)) => Some(TrailDistance::Pct(pct)),
            Some(Trail::Atr {
                multiple,
                timeframe,
            }) => {
                let atr = atr.filter(|a| *a > Decimal::ZERO).ok_or_else(|| {
                    format!(
                        "ATR trail needs the coin's {} average true range",
                        timeframe.as_str()
                    )
                })?;
                Some(TrailDistance::Absolute(atr * multiple))
            }
        };
        Ok(Self {
            entry_price,
            entry_ms,
            trail,
            take_part: plan.take_part,
            stretch: plan.stretch,
            slowdown: plan.slowdown,
            fade: plan.fade,
            deadline_ms: plan
                .deadline_minutes
                .map(|m| entry_ms + i64::from(m) * MS_PER_MINUTE),
            peak: entry_price,
            last_high_ms: entry_ms,
            part_taken: false,
            stretch_taken: false,
            slowdown_taken: false,
            slowdown_due: false,
            minutes: MinuteSeries::default(),
            remaining: Decimal::ONE,
        })
    }

    /// Fraction of the original position still held.
    pub fn remaining(&self) -> Decimal {
        self.remaining
    }

    /// Highest price seen since entry (entry price before any bar).
    pub fn peak(&self) -> Decimal {
        self.peak
    }

    /// Follow-the-peak distance as a percent of the current peak.
    pub fn trail_pct(&self) -> Option<Decimal> {
        match self.trail? {
            TrailDistance::Pct(pct) => Some(pct),
            TrailDistance::Absolute(distance) => {
                Some((distance / self.peak * Decimal::ONE_HUNDRED).round_dp(4))
            }
        }
    }

    /// Price at or below which follow-the-peak sells on the next observation.
    pub fn sell_line(&self) -> Option<Decimal> {
        match self.trail? {
            TrailDistance::Pct(pct) => {
                Some(self.peak * (Decimal::ONE - pct / Decimal::ONE_HUNDRED))
            }
            TrailDistance::Absolute(distance) => Some(self.peak - distance),
        }
    }

    /// Price at or above which the stretch rule sells, from completed minutes.
    pub fn stretch_line(&self) -> Option<Decimal> {
        let stretch = self.stretch.filter(|_| !self.stretch_taken)?;
        self.minutes.upper_band(stretch.z)
    }

    fn observe_minute(&mut self, ts_ms: i64, price: Decimal) {
        let ema_minutes = self.slowdown.map(|s| s.minutes);
        if let Some(close) = self.minutes.observe(ts_ms, price, ema_minutes) {
            self.slowdown_due = self.slowdown.is_some()
                && !self.slowdown_taken
                && close > self.entry_price
                && self.minutes.slowing_minutes >= SLOWDOWN_PERSIST_MINUTES;
        }
    }

    /// Feed a 1-minute bar from before entry: builds the minute history only.
    pub fn warm_up_before_entry(&mut self, bar: &PriceBar) {
        self.observe_minute(bar.close_ms, bar.close);
        self.slowdown_due = false;
    }

    /// Feed a bar between entry and the moment watching started: raises the peak (a plan
    /// set after entry follows the highest price since entry) and builds minute history,
    /// without selling.
    pub fn warm_up_since_entry(&mut self, bar: &PriceBar) {
        self.observe_minute(bar.close_ms, bar.close);
        if bar.high > self.peak {
            self.peak = bar.high;
            self.last_high_ms = bar.close_ms;
        }
    }

    /// Feed the next bar (bars must arrive in time order); returns the sells it triggers.
    pub fn on_bar(&mut self, bar: &PriceBar) -> Vec<ExitFill> {
        let mut fills = Vec::new();
        if self.remaining <= Decimal::ZERO || bar.open_ms < self.entry_ms {
            return fills;
        }
        // A minute completed by this bar's arrival is judged before the bar itself.
        self.observe_minute(bar.open_ms, bar.open);
        if self
            .deadline_ms
            .is_some_and(|deadline| bar.open_ms >= deadline)
        {
            self.sell_rest(bar.open_ms, bar.open, ExitReason::Deadline, &mut fills);
            return fills;
        }
        if let Some(stop) = self.sell_line() {
            if bar.open <= stop {
                self.sell_rest(bar.open_ms, bar.open, ExitReason::FollowPeak, &mut fills);
                return fills;
            }
            if bar.low <= stop {
                self.sell_rest(bar.close_ms, stop, ExitReason::FollowPeak, &mut fills);
                return fills;
            }
        }
        if self.slowdown_due {
            self.slowdown_due = false;
            self.slowdown_taken = true;
            if let Some(slow) = self.slowdown {
                self.sell_part(
                    bar.open_ms,
                    bar.open,
                    slow.fraction,
                    ExitReason::Slowdown,
                    &mut fills,
                );
            }
        }
        if let Some(part) = self.take_part.filter(|_| !self.part_taken) {
            let target = self.entry_price * (Decimal::ONE + part.gain_pct / Decimal::ONE_HUNDRED);
            if bar.high >= target {
                self.part_taken = true;
                self.sell_part(
                    bar.close_ms,
                    bar.open.max(target),
                    part.fraction,
                    ExitReason::TakePart,
                    &mut fills,
                );
            }
        }
        if let (Some(stretch), Some(line)) = (self.stretch, self.stretch_line())
            && line > self.entry_price
            && bar.high >= line
        {
            self.stretch_taken = true;
            self.sell_part(
                bar.close_ms,
                bar.open.max(line),
                stretch.fraction,
                ExitReason::Stretch,
                &mut fills,
            );
        }
        if bar.high > self.peak {
            self.peak = bar.high;
            self.last_high_ms = bar.close_ms;
        }
        if let Some(fade) = self.fade {
            let armed = self.peak
                >= self.entry_price * (Decimal::ONE + fade.min_gain_pct / Decimal::ONE_HUNDRED);
            let quiet_ms = bar.close_ms - self.last_high_ms;
            if armed && quiet_ms >= i64::from(fade.minutes) * MS_PER_MINUTE {
                self.sell_rest(bar.close_ms, bar.close, ExitReason::Fade, &mut fills);
            }
        }
        self.observe_minute(bar.close_ms, bar.close);
        fills
    }

    /// Sell `fraction` of the original position (at most what is left).
    fn sell_part(
        &mut self,
        ts_ms: i64,
        price: Decimal,
        fraction: Decimal,
        reason: ExitReason,
        fills: &mut Vec<ExitFill>,
    ) {
        let fraction = fraction.min(self.remaining);
        if fraction > Decimal::ZERO {
            self.remaining -= fraction;
            fills.push(ExitFill {
                ts_ms,
                price,
                fraction,
                reason,
            });
        }
    }

    fn sell_rest(
        &mut self,
        ts_ms: i64,
        price: Decimal,
        reason: ExitReason,
        fills: &mut Vec<ExitFill>,
    ) {
        let rest = self.remaining;
        self.sell_part(ts_ms, price, rest, reason, fills);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const M: i64 = MS_PER_MINUTE;

    fn bar(minute: i64, open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> PriceBar {
        PriceBar {
            open_ms: minute * M,
            close_ms: (minute + 1) * M - 1,
            open,
            high,
            low,
            close,
        }
    }

    fn flat(minute: i64, price: Decimal) -> PriceBar {
        bar(minute, price, price, price, price)
    }

    fn plan(spec: &str) -> ExitPlan {
        ExitPlan::parse_spec(spec).unwrap()
    }

    fn run(spec: &str, bars: &[PriceBar]) -> Vec<ExitFill> {
        let mut watch = ExitWatch::new(&plan(spec), dec!(100), 0, Some(dec!(0.5))).unwrap();
        bars.iter().flat_map(|b| watch.on_bar(b)).collect()
    }

    #[test]
    fn rule_sets_parse_menu_keys_and_match_plans() {
        let set = RuleSet::parse(" stretch , atr ").unwrap();
        assert_eq!(set.to_string(), "atr,stretch");
        assert_eq!(RuleSet::of(&plan("stretch=2:1,atr=3:1h")), set);
        assert_ne!(RuleSet::of(&plan("trail=3,stretch=2:1")), set);
        assert_ne!(RuleSet::of(&plan("atr=3:1h,stretch=2:1,deadline=60")), set);
        for (bad, message) in [
            ("atr,stop", "unknown exit-plan rule \"stop\""),
            ("trail,atr", "only one of trail / atr"),
            (" , ", "no exit-plan rule"),
        ] {
            let error = RuleSet::parse(bad).unwrap_err();
            assert!(error.contains(message), "{bad}: {error}");
        }
    }

    #[test]
    fn spec_round_trips_and_rejects_bad_values() {
        let p = plan(" atr=3:15m , part=3:0.5, stretch=2:1, slow=0.5:5, fade=2:20, deadline=240 ");
        assert_eq!(
            p.to_string(),
            "atr=3:15m,part=3:0.5,stretch=2:1,slow=0.5:5,fade=2:20,deadline=240"
        );
        assert_eq!(ExitPlan::parse_spec(&p.to_string()).unwrap(), p);
        assert_eq!(p.atr_timeframe(), Some(AtrTimeframe::M15));
        assert_eq!(plan("hold"), ExitPlan::default());
        assert_eq!(ExitPlan::default().to_string(), "hold");
        for bad in [
            "",
            "trail=0",
            "trail=100",
            "trail=2,atr=3:1m",
            "atr=3:4h",
            "atr=0:1m",
            "part=3:1",
            "part=0:0.5",
            "stretch=0:0.5",
            "stretch=2:1.5",
            "slow=0.5:1",
            "slow=0.5:61",
            "fade=1:0",
            "deadline=0",
            "deadline=1.5",
            "stop=3",
            "trail",
        ] {
            assert!(ExitPlan::parse_spec(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn follow_peak_sells_at_the_stop_below_the_running_peak() {
        let fills = run(
            "trail=2",
            &[
                bar(0, dec!(100), dec!(105), dec!(99), dec!(104)),
                // stop 105 * 0.98 = 102.9; low 102.9 touches it
                bar(1, dec!(104), dec!(104.5), dec!(102.9), dec!(103)),
            ],
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price, dec!(102.900));
        assert_eq!(fills[0].fraction, Decimal::ONE);
        assert_eq!(fills[0].reason, ExitReason::FollowPeak);
    }

    #[test]
    fn stop_is_checked_before_the_same_bar_raises_the_peak() {
        // Bar 0 dips to 98.5 (above the 98 stop from the entry peak) and spikes to 110:
        // the dip does not sell, the spike moves the peak for later bars only.
        let fills = run(
            "trail=2",
            &[
                bar(0, dec!(100), dec!(110), dec!(98.5), dec!(109)),
                bar(1, dec!(109), dec!(109), dec!(107.7), dec!(108)),
            ],
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price, dec!(107.800)); // 110 * 0.98
    }

    #[test]
    fn gap_below_the_stop_fills_at_the_open() {
        let fills = run(
            "trail=2",
            &[
                bar(0, dec!(100), dec!(101), dec!(99), dec!(100)),
                bar(1, dec!(95), dec!(96), dec!(94), dec!(95)),
            ],
        );
        assert_eq!(fills[0].price, dec!(95));
    }

    #[test]
    fn atr_trail_hangs_multiples_of_the_true_range_below_the_peak() {
        // True ranges: 2 (first bar), then max(1, |104-101|, |103-101|) = 3 → ATR 2.5.
        let candles = [
            bar(-2, dec!(100), dec!(101), dec!(99), dec!(101)),
            bar(-1, dec!(103), dec!(104), dec!(103), dec!(103)),
        ];
        assert_eq!(average_true_range(&candles), Some(dec!(2.5)));
        assert_eq!(average_true_range(&[]), None);
        assert_eq!(average_true_range(&[flat(0, dec!(1))]), None);

        let mut watch = ExitWatch::new(
            &plan("atr=2:1h"),
            dec!(100),
            0,
            average_true_range(&candles),
        )
        .unwrap();
        assert!(
            watch
                .on_bar(&bar(0, dec!(100), dec!(110), dec!(100), dec!(109)))
                .is_empty()
        );
        assert_eq!(watch.sell_line(), Some(dec!(105.0))); // 110 - 2 * 2.5
        let fills = watch.on_bar(&bar(1, dec!(108), dec!(108), dec!(104), dec!(104)));
        assert_eq!(
            (fills[0].price, fills[0].reason),
            (dec!(105.0), ExitReason::FollowPeak)
        );
        assert!(ExitWatch::new(&plan("atr=2:1h"), dec!(100), 0, None).is_err());
    }

    #[test]
    fn take_part_sells_once_then_the_rest_follows_the_peak() {
        let fills = run(
            "part=3:0.5,trail=2",
            &[
                bar(0, dec!(100), dec!(103.5), dec!(99), dec!(103)),
                bar(1, dec!(103), dec!(106), dec!(103), dec!(105)),
                bar(2, dec!(105), dec!(105), dec!(103), dec!(103.5)),
            ],
        );
        assert_eq!(fills.len(), 2);
        assert_eq!(
            (fills[0].reason, fills[0].price, fills[0].fraction),
            (ExitReason::TakePart, dec!(103.00), dec!(0.5))
        );
        assert_eq!(
            (fills[1].reason, fills[1].price, fills[1].fraction),
            (ExitReason::FollowPeak, dec!(103.880), dec!(0.5))
        );
    }

    #[test]
    fn stretch_sells_when_the_price_leaves_the_upper_band_above_entry() {
        let mut watch = ExitWatch::new(&plan("stretch=2:0.5"), dec!(100), 0, None).unwrap();
        // 20 minutes alternating 101 / 103 before entry: mean 102, stdev 1 → band 104.
        for m in -20..0 {
            let price = if m % 2 == 0 { dec!(101) } else { dec!(103) };
            watch.warm_up_before_entry(&flat(m, price));
        }
        // Minute 0 keeps the alternating pattern, so the band over minutes -20..-1 and
        // over -19..0 is the same 104.
        assert!(watch.on_bar(&flat(0, dec!(101))).is_empty());
        assert_eq!(watch.stretch_line(), Some(dec!(104)));
        let fills = watch.on_bar(&bar(1, dec!(103.6), dec!(104.2), dec!(103.6), dec!(104)));
        assert_eq!(
            (fills[0].reason, fills[0].price, fills[0].fraction),
            (ExitReason::Stretch, dec!(104), dec!(0.5))
        );
        // Once only.
        assert!(
            watch
                .on_bar(&bar(2, dec!(106), dec!(107), dec!(106), dec!(107)))
                .is_empty()
        );
        assert_eq!(watch.remaining(), dec!(0.5));
    }

    #[test]
    fn stretch_line_below_entry_never_sells() {
        let mut watch = ExitWatch::new(&plan("stretch=1:1"), dec!(100), 0, None).unwrap();
        for m in -20..0 {
            watch.warm_up_before_entry(&flat(m, if m % 2 == 0 { dec!(90) } else { dec!(92) }));
        }
        assert!(watch.on_bar(&flat(0, dec!(95))).is_empty());
        assert!(watch.on_bar(&flat(1, dec!(96))).is_empty());
    }

    #[test]
    fn slowdown_sells_after_three_slowing_minutes_while_rising() {
        let mut watch = ExitWatch::new(&plan("slow=0.5:2"), dec!(100), 0, None).unwrap();
        // Rising fast, then still rising but each minute by less.
        let closes = [100, 104, 108, 112, 115, 117, 118, 118];
        let mut fills = Vec::new();
        for (m, close) in closes.iter().enumerate() {
            let p = Decimal::from(*close);
            fills.extend(watch.on_bar(&flat(m as i64, p)));
        }
        assert_eq!(fills.len(), 1, "{fills:?}");
        assert_eq!(fills[0].reason, ExitReason::Slowdown);
        assert_eq!(fills[0].fraction, dec!(0.5));
        // Sold at the price observed after the third slowing minute, never at a past price.
        assert!(fills[0].price >= dec!(117));
        // An accelerating climb (each minute up more than the last) never slows.
        let mut climbing = ExitWatch::new(&plan("slow=1:2"), dec!(100), 0, None).unwrap();
        for m in 0..9 {
            let price = Decimal::from(100 + (1_i64 << m));
            assert!(climbing.on_bar(&flat(m, price)).is_empty());
        }
    }

    #[test]
    fn slowdown_below_entry_is_ignored() {
        let mut watch = ExitWatch::new(&plan("slow=1:2"), dec!(200), 0, None).unwrap();
        let closes = [100, 104, 108, 112, 115, 117, 118, 118, 118];
        for (m, close) in closes.iter().enumerate() {
            assert!(
                watch
                    .on_bar(&flat(m as i64, Decimal::from(*close)))
                    .is_empty()
            );
        }
    }

    #[test]
    fn fade_only_arms_after_the_minimum_gain() {
        // Flat for 30 minutes below the 1% arm level: no fade exit.
        let flat_bars: Vec<PriceBar> = (0..30)
            .map(|m| bar(m, dec!(100), dec!(100.5), dec!(99.5), dec!(100)))
            .collect();
        assert!(run("fade=1:10", &flat_bars).is_empty());

        // Up 2% at minute 0, then 10 quiet minutes: sells at the close of minute 10.
        let mut bars = vec![bar(0, dec!(100), dec!(102), dec!(100), dec!(101.5))];
        bars.extend((1..15).map(|m| bar(m, dec!(101.5), dec!(101.8), dec!(101), dec!(101.2))));
        let fills = run("fade=1:10", &bars);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].reason, ExitReason::Fade);
        assert_eq!(fills[0].ts_ms, 11 * M - 1);
    }

    #[test]
    fn deadline_sells_at_the_first_bar_after_it() {
        let bars: Vec<PriceBar> = (0..10)
            .map(|m| {
                bar(
                    m,
                    Decimal::from(100 + m),
                    Decimal::from(101 + m),
                    Decimal::from(99 + m),
                    Decimal::from(100 + m),
                )
            })
            .collect();
        let fills = run("deadline=5", &bars);
        assert_eq!(fills.len(), 1);
        assert_eq!((fills[0].ts_ms, fills[0].price), (5 * M, dec!(105)));
    }

    #[test]
    fn ticks_drive_the_same_rules() {
        let mut watch = ExitWatch::new(&plan("trail=1"), dec!(100), 0, None).unwrap();
        assert!(watch.on_bar(&PriceBar::tick(1_000, dec!(103))).is_empty());
        assert!(watch.on_bar(&PriceBar::tick(2_000, dec!(102))).is_empty());
        let fills = watch.on_bar(&PriceBar::tick(3_000, dec!(101.9)));
        assert_eq!(fills[0].price, dec!(101.9)); // a tick at or below 101.97 fills at itself
        assert_eq!(watch.remaining(), Decimal::ZERO);
        assert!(watch.on_bar(&PriceBar::tick(4_000, dec!(90))).is_empty());
    }

    #[test]
    fn ticks_build_minute_closes_from_the_last_price_of_each_minute() {
        let mut series = MinuteSeries::default();
        assert_eq!(series.observe(1_000, dec!(1), None), None);
        assert_eq!(series.observe(59_000, dec!(2), None), None);
        assert_eq!(series.observe(61_000, dec!(3), None), Some(dec!(2)));
        assert_eq!(series.observe(185_000, dec!(4), None), Some(dec!(3)));
        assert_eq!(series.closes, [dec!(2), dec!(3)]);
    }

    #[test]
    fn warm_up_since_entry_raises_the_peak_without_selling() {
        let mut watch = ExitWatch::new(&plan("trail=2"), dec!(100), 0, None).unwrap();
        watch.warm_up_since_entry(&bar(0, dec!(100), dec!(110), dec!(90), dec!(105)));
        assert_eq!(watch.peak(), dec!(110));
        assert_eq!(watch.remaining(), Decimal::ONE);
        let fills = watch.on_bar(&PriceBar::tick(2 * M, dec!(107)));
        assert_eq!(fills[0].price, dec!(107)); // under the 107.8 line from the seeded peak
    }
}

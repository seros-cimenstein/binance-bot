//! The persona backtest engine: one USDT account per persona, stepped on a fixed clock over
//! cached 1m bars.
//!
//! - Every `step_minutes` the persona sees a point-in-time [`Snapshot`] (only bars closed
//!   by the step time `T`) and its [`Book`], and returns [`Intent`]s.
//! - Intents fill at the open of the 1m bar that opens at `T`: sells first, then buys. A buy
//!   pays `price × (1 + slippage)` plus the fee; a sell receives `price × (1 − slippage)`
//!   minus the fee. Buys under `min_notional` USDT are skipped.
//! - A position bought under a persona with an exit plan gets an [`ExitWatch`] (ATR from the
//!   closed 1h bars before entry, 60 minutes of 1m warm-up as `bb bot-spot-exit-replay`
//!   does). Its sells fill at the [`crate::exit_plan::ExitFill`] price minus slippage and
//!   fee.
//! - Equity (cash plus holdings at their last close, no exit costs) is marked at every 1m
//!   close; open positions at the window end are marked the same way.
//!
//! No network and no order path: the engine only reads [`Market`].

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

use super::data::{Bar, MINUTE_MS, Market, Series};
use crate::error::{Result, TraderError};
use crate::exit_plan::{ATR_PERIODS, ExitPlan, ExitWatch, PriceBar, average_true_range};
use crate::market_mood::{HorizonBreadth, breadth_score_at};

/// The 20-coin liquid universe: the `bb entry-study` top 20 by quote volume on 2026-10-18
/// (the Phase 47 basket). Fixed, so the backtest has survivorship bias.
pub const LIQUID: [&str; 20] = [
    "BTC", "ETH", "ZEC", "SOL", "NEAR", "XRP", "BNB", "WLD", "HYPE", "DOGE", "SUI", "SAND", "TRX",
    "UNI", "ENA", "STRK", "TAO", "HBAR", "TIA", "LINK",
];
/// Candidates per step, as the live `BOT_SPOT_CANDIDATE_LIMIT=10`.
pub const CANDIDATE_LIMIT: usize = 10;
/// Breadth bars, as `market_mood` (15m bars, 24h = 96 bars back).
const BREADTH_BAR_MINUTES: i64 = 15;
const BREADTH_LOOKBACK_BARS: usize = 96;
/// 1m bars before entry fed to the exit watch (`bb bot-spot-exit-replay --history-minutes`).
const HISTORY_MINUTES: i64 = 60;
const MINUTES_PER_YEAR: f64 = 525_600.0;
const DAY_MINUTES: i64 = 1_440;

/// Run settings shared by every persona.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// First step (inclusive); bars that open in `[start_ms, end_ms)` are simulated.
    pub start_ms: i64,
    pub end_ms: i64,
    pub step_minutes: i64,
    /// Fee per side, as a fraction of the traded value.
    pub fee_rate: f64,
    /// Slippage per side, as a fraction of the price.
    pub slippage: f64,
    pub starting_balance: f64,
    pub min_notional: f64,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.start_ms % MINUTE_MS != 0 || self.end_ms % MINUTE_MS != 0 {
            return Err(TraderError::config("window bounds must be whole minutes"));
        }
        if self.end_ms <= self.start_ms {
            return Err(TraderError::config("window end must be after its start"));
        }
        if self.step_minutes <= 0 {
            return Err(TraderError::config("step minutes must be positive"));
        }
        if !(0.0..1.0).contains(&self.fee_rate) || !(0.0..1.0).contains(&self.slippage) {
            return Err(TraderError::config("fee and slippage must be in [0, 1)"));
        }
        if self.starting_balance <= 0.0 || self.min_notional < 0.0 {
            return Err(TraderError::config(
                "starting balance must be positive and min notional not negative",
            ));
        }
        Ok(())
    }

    fn step_ms(&self) -> i64 {
        self.step_minutes * MINUTE_MS
    }

    fn is_step(&self, t: i64) -> bool {
        (t - self.start_ms) % self.step_ms() == 0
    }
}

/// One coin at a step: returns of the close at `T` over 15m/1h/4h and its trailing 24h
/// quote volume. Only coins with a bar closing at `T` are present.
#[derive(Debug, Clone, PartialEq)]
pub struct CoinView {
    pub ret_15m: Option<f64>,
    pub ret_1h: Option<f64>,
    pub ret_4h: Option<f64>,
    pub quote_volume_24h: f64,
}

/// What every persona may see at step time `T`.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub t: i64,
    /// 20-coin breadth score (−100..100), as the live `## Market breadth` section.
    pub score: Option<f64>,
    pub horizons: Vec<HorizonBreadth>,
    pub coins: BTreeMap<String, CoinView>,
    /// The [`LIQUID`] coins present at `T`, top [`CANDIDATE_LIMIT`] by 24h quote volume.
    pub liquid_top: Vec<String>,
}

impl Snapshot {
    /// Build the view at `t` from bars closed by `t` only.
    pub fn at(market: &Market, t: i64) -> Self {
        let coins: BTreeMap<String, CoinView> = market
            .iter()
            .filter(|s| s.close_at(t).is_some())
            .map(|s| {
                let view = CoinView {
                    ret_15m: s.return_pct(t, 15),
                    ret_1h: s.return_pct(t, 60),
                    ret_4h: s.return_pct(t, 240),
                    quote_volume_24h: s.quote_volume(t, DAY_MINUTES),
                };
                (s.asset().to_string(), view)
            })
            .collect();
        let closes: Vec<_> = LIQUID
            .iter()
            .filter_map(|a| market.get(a))
            .map(|s| s.closes_by_open(t, BREADTH_BAR_MINUTES, BREADTH_LOOKBACK_BARS))
            .collect();
        let (horizons, score) =
            breadth_score_at(&closes, Series::newest_bucket_open(t, BREADTH_BAR_MINUTES));
        let mut liquid: Vec<(&str, f64)> = LIQUID
            .iter()
            .filter_map(|a| coins.get(*a).map(|c| (*a, c.quote_volume_24h)))
            .collect();
        liquid.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let liquid_top = liquid
            .into_iter()
            .take(CANDIDATE_LIMIT)
            .map(|(a, _)| a.to_string())
            .collect();
        Self {
            t,
            score,
            horizons,
            coins,
            liquid_top,
        }
    }

    pub fn coin(&self, asset: &str) -> Option<&CoinView> {
        self.coins.get(asset)
    }

    /// Median coin return over a breadth horizon (`15m`, `1h`, `4h`, `24h`).
    pub fn median_return_pct(&self, horizon: &str) -> Option<f64> {
        self.horizons
            .iter()
            .find(|h| h.name == horizon)
            .and_then(|h| h.median_return_pct)
    }
}

/// What a persona asks for at a step.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Spend `equity_fraction` of the step's equity (capped by cash) on `asset`.
    Buy { asset: String, equity_fraction: f64 },
    /// Sell all of `asset`; `reason` goes to the trade list.
    Sell { asset: String, reason: &'static str },
}

/// One holding as the persona sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    pub qty: f64,
    pub entry_price: f64,
    pub entry_ms: i64,
    /// `qty` at the last close.
    pub value: f64,
}

/// The persona's account at a step.
#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub t: i64,
    pub cash: f64,
    pub equity: f64,
    pub holdings: BTreeMap<String, Holding>,
}

impl Book {
    pub fn is_flat(&self) -> bool {
        self.holdings.is_empty()
    }
}

/// A deterministic trading rule set.
pub trait Persona {
    fn name(&self) -> &str;
    /// Intents at `snap.t`. Must use only `snap` and `book`.
    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent>;
    /// The exit plan attached to a position bought now; `None` holds until a `Sell`.
    fn exit_plan(&self) -> Option<ExitPlan> {
        None
    }
    /// Called before [`Self::decide`] at each step (multi-persona shadows step here).
    fn before_step(&mut self, _market: &Market, _snap: &Snapshot) {}
    /// Called after the account has processed minute `m` (multi-persona shadows advance).
    fn after_minute(&mut self, _market: &Market, _m: i64) {}
}

/// One round trip (or a position still open at the window end).
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub asset: String,
    pub entry_ms: i64,
    /// Buy fill price (open plus slippage).
    pub entry_price: f64,
    /// Last sell time, or the window end when open.
    pub exit_ms: i64,
    /// Quantity-weighted sell fill price, or the last close when open.
    pub exit_price: f64,
    pub qty: f64,
    /// USDT spent, buy fee included.
    pub cost_usdt: f64,
    /// USDT received after sell fees, plus the marked value of what is still held.
    pub proceeds_usdt: f64,
    pub fees_usdt: f64,
    pub net_pnl_usdt: f64,
    /// Sell reasons in order, joined by `+` (`open_at_end` when still held).
    pub reason: String,
    pub open_at_end: bool,
}

/// Result of one persona over the window.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub name: String,
    pub starting_balance: f64,
    pub final_equity: f64,
    pub net_pnl_usdt: f64,
    pub net_pnl_pct: f64,
    pub max_drawdown_pct: f64,
    /// Closed round trips.
    pub round_trips: usize,
    pub wins: usize,
    pub win_rate_pct: f64,
    pub sharpe: f64,
    pub fees_usdt: f64,
    pub steps: usize,
    /// Intents that could not fill (no bar, under min notional, missing ATR, …).
    pub skipped_orders: usize,
    pub trades: Vec<Trade>,
}

struct Position {
    qty: f64,
    original_qty: f64,
    entry_price: f64,
    entry_ms: i64,
    cost: f64,
    fees: f64,
    proceeds: f64,
    sold_qty: f64,
    sold_value: f64,
    last_ms: i64,
    last_close: f64,
    reasons: Vec<&'static str>,
    watch: Option<ExitWatch>,
}

/// One persona's account, advanced step by step and minute by minute.
pub struct Account {
    persona: Box<dyn Persona>,
    cfg: Config,
    cash: f64,
    positions: BTreeMap<String, Position>,
    trades: Vec<Trade>,
    step_equity: Vec<(i64, f64)>,
    peak_equity: f64,
    max_drawdown_pct: f64,
    steps: usize,
    skipped: usize,
}

fn decimal(value: f64) -> Option<Decimal> {
    Decimal::from_f64(value).filter(|_| value.is_finite())
}

fn price_bar(bar: &Bar) -> Option<PriceBar> {
    Some(PriceBar {
        open_ms: bar.open_ms,
        close_ms: bar.open_ms + MINUTE_MS - 1,
        open: decimal(bar.open)?,
        high: decimal(bar.high)?,
        low: decimal(bar.low)?,
        close: decimal(bar.close)?,
    })
}

impl Account {
    pub fn new(persona: Box<dyn Persona>, cfg: Config) -> Self {
        Self {
            persona,
            cfg,
            cash: cfg.starting_balance,
            positions: BTreeMap::new(),
            trades: Vec::new(),
            step_equity: Vec::new(),
            peak_equity: cfg.starting_balance,
            max_drawdown_pct: 0.0,
            steps: 0,
            skipped: 0,
        }
    }

    pub fn name(&self) -> &str {
        self.persona.name()
    }

    /// Cash plus holdings at their last close.
    pub fn equity(&self) -> f64 {
        self.cash
            + self
                .positions
                .values()
                .map(|p| p.qty * p.last_close)
                .sum::<f64>()
    }

    /// Equity recorded at each step time, before that step's orders.
    pub fn step_equity(&self) -> &[(i64, f64)] {
        &self.step_equity
    }

    pub fn book(&self, t: i64) -> Book {
        Book {
            t,
            cash: self.cash,
            equity: self.equity(),
            holdings: self
                .positions
                .iter()
                .map(|(asset, p)| {
                    let holding = Holding {
                        qty: p.qty,
                        entry_price: p.entry_price,
                        entry_ms: p.entry_ms,
                        value: p.qty * p.last_close,
                    };
                    (asset.clone(), holding)
                })
                .collect(),
        }
    }

    /// Run the step at `snap.t`: record equity, ask the persona, fill its intents at the
    /// open of the bar that opens at `snap.t`. Returns the intents (for tests and votes).
    pub fn step(&mut self, market: &Market, snap: &Snapshot) -> Vec<Intent> {
        let t = snap.t;
        self.steps += 1;
        let equity = self.equity();
        self.step_equity.push((t, equity));
        self.persona.before_step(market, snap);
        let book = self.book(t);
        let intents = self.persona.decide(snap, &book);
        for intent in &intents {
            if let Intent::Sell { asset, reason } = intent {
                self.sell_intent(market, asset, t, reason);
            }
        }
        for intent in &intents {
            if let Intent::Buy {
                asset,
                equity_fraction,
            } = intent
            {
                self.buy(market, asset, t, equity * equity_fraction);
            }
        }
        intents
    }

    fn fill_bar<'m>(market: &'m Market, asset: &str, t: i64) -> Option<&'m Bar> {
        market
            .get(asset)?
            .next_bar(t)
            .filter(|bar| bar.open_ms == t)
    }

    fn sell_intent(&mut self, market: &Market, asset: &str, t: i64, reason: &'static str) {
        if !self.positions.contains_key(asset) {
            return;
        }
        let Some(bar) = Self::fill_bar(market, asset, t) else {
            self.skipped += 1;
            return;
        };
        let price = bar.open;
        if let Some(position) = self.positions.get_mut(asset) {
            let qty = position.qty;
            Self::sell_qty(&mut self.cash, &self.cfg, position, qty, price, t, reason);
        }
        self.close_if_done(asset);
    }

    fn buy(&mut self, market: &Market, asset: &str, t: i64, wanted: f64) {
        if self.positions.contains_key(asset) {
            return;
        }
        let notional = wanted.min(self.cash);
        let Some(series) = market.get(asset) else {
            self.skipped += 1;
            return;
        };
        let Some(bar) = Self::fill_bar(market, asset, t) else {
            self.skipped += 1;
            return;
        };
        if !(notional >= self.cfg.min_notional && notional > 0.0) {
            self.skipped += 1;
            return;
        }
        let fill = bar.open * (1.0 + self.cfg.slippage);
        let watch = match self.persona.exit_plan() {
            None => None,
            Some(plan) => match Self::watch(series, &plan, fill, t, self.cfg.fee_rate) {
                Some(watch) => Some(watch),
                None => {
                    self.skipped += 1;
                    return;
                }
            },
        };
        let qty = notional / (fill * (1.0 + self.cfg.fee_rate));
        let fee = qty * fill * self.cfg.fee_rate;
        self.cash -= notional;
        self.positions.insert(
            asset.to_string(),
            Position {
                qty,
                original_qty: qty,
                entry_price: fill,
                entry_ms: t,
                cost: notional,
                fees: fee,
                proceeds: 0.0,
                sold_qty: 0.0,
                sold_value: 0.0,
                last_ms: t,
                last_close: bar.open,
                reasons: Vec::new(),
                watch,
            },
        );
    }

    /// The exit watch for a buy at `entry_ms`: ATR from the closed bars of the plan's
    /// timeframe before entry, then 60 minutes of 1m warm-up. `None` when it cannot start.
    fn watch(
        series: &Series,
        plan: &ExitPlan,
        entry_price: f64,
        entry_ms: i64,
        fee_rate: f64,
    ) -> Option<ExitWatch> {
        let atr = match plan.atr_timeframe() {
            None => None,
            Some(tf) => {
                let bars: Vec<PriceBar> = series
                    .closed_bars(entry_ms, tf.millis() / MINUTE_MS, ATR_PERIODS + 1)
                    .iter()
                    .filter_map(price_bar)
                    .collect();
                Some(average_true_range(&bars)?)
            }
        };
        let mut watch = ExitWatch::new(
            plan,
            decimal(entry_price)?,
            entry_ms,
            atr,
            decimal(fee_rate)?,
        )
        .ok()?;
        let history = series.closed_by(entry_ms);
        let from = history.len().saturating_sub(HISTORY_MINUTES as usize);
        for bar in history[from..]
            .iter()
            .filter(|b| b.open_ms >= entry_ms - HISTORY_MINUTES * MINUTE_MS)
            .filter_map(price_bar)
        {
            watch.warm_up_before_entry(&bar);
        }
        Some(watch)
    }

    fn sell_qty(
        cash: &mut f64,
        cfg: &Config,
        position: &mut Position,
        qty: f64,
        price: f64,
        ts_ms: i64,
        reason: &'static str,
    ) {
        let qty = qty.min(position.qty);
        if qty <= 0.0 {
            return;
        }
        let fill = price * (1.0 - cfg.slippage);
        let gross = qty * fill;
        let fee = gross * cfg.fee_rate;
        *cash += gross - fee;
        position.qty -= qty;
        position.fees += fee;
        position.proceeds += gross - fee;
        position.sold_qty += qty;
        position.sold_value += gross;
        position.last_ms = ts_ms;
        if position.reasons.last() != Some(&reason) {
            position.reasons.push(reason);
        }
    }

    fn close_if_done(&mut self, asset: &str) {
        let done = self.positions.get(asset).is_some_and(|p| {
            p.qty <= p.original_qty * 1e-12
                || p.watch
                    .as_ref()
                    .is_some_and(|w| w.remaining() <= Decimal::ZERO)
        });
        if done && let Some(mut position) = self.positions.remove(asset) {
            position.qty = 0.0;
            self.trades.push(Self::trade(asset, &position, false));
        }
    }

    fn trade(asset: &str, p: &Position, open: bool) -> Trade {
        let marked = if open { p.qty * p.last_close } else { 0.0 };
        let proceeds = p.proceeds + marked;
        let exit_price = if open || p.sold_qty <= 0.0 {
            p.last_close
        } else {
            p.sold_value / p.sold_qty
        };
        let reason = if open {
            let mut reasons = p.reasons.clone();
            reasons.push("open_at_end");
            reasons.join("+")
        } else {
            p.reasons.join("+")
        };
        Trade {
            asset: asset.to_string(),
            entry_ms: p.entry_ms,
            entry_price: p.entry_price,
            exit_ms: p.last_ms,
            exit_price,
            qty: p.original_qty,
            cost_usdt: p.cost,
            proceeds_usdt: proceeds,
            fees_usdt: p.fees,
            net_pnl_usdt: proceeds - p.cost,
            reason,
            open_at_end: open,
        }
    }

    /// Process the 1m bar that opens at `m`: exit-plan sells, then the mark at its close.
    pub fn minute(&mut self, market: &Market, m: i64) {
        let assets: Vec<String> = self.positions.keys().cloned().collect();
        for asset in assets {
            let Some(bar) = Self::fill_bar(market, &asset, m) else {
                continue;
            };
            let Some(position) = self.positions.get_mut(&asset) else {
                continue;
            };
            if let (Some(watch), Some(minute_bar)) = (position.watch.as_mut(), price_bar(bar)) {
                for fill in watch.on_bar(&minute_bar) {
                    let (Some(price), Some(fraction)) =
                        (fill.price.to_f64(), fill.fraction.to_f64())
                    else {
                        continue;
                    };
                    let qty = fraction * position.original_qty;
                    Self::sell_qty(
                        &mut self.cash,
                        &self.cfg,
                        position,
                        qty,
                        price,
                        fill.ts_ms,
                        fill.reason.as_str(),
                    );
                }
            }
            position.last_close = bar.close;
            if position.qty > 0.0 {
                position.last_ms = bar.open_ms + MINUTE_MS;
            }
            self.close_if_done(&asset);
        }
        self.persona.after_minute(market, m);
        let equity = self.equity();
        self.peak_equity = self.peak_equity.max(equity);
        if self.peak_equity > 0.0 {
            let drawdown = (self.peak_equity - equity) / self.peak_equity * 100.0;
            self.max_drawdown_pct = self.max_drawdown_pct.max(drawdown);
        }
    }

    /// Close the books at `end_ms`: still-held positions are marked at their last close.
    pub fn finish(mut self, end_ms: i64) -> RunResult {
        let open: Vec<Trade> = self
            .positions
            .iter()
            .map(|(asset, p)| {
                let mut trade = Self::trade(asset, p, true);
                trade.exit_ms = end_ms;
                trade
            })
            .collect();
        let final_equity = self.equity();
        self.step_equity.push((end_ms, final_equity));
        let closed = self.trades.len();
        let wins = self.trades.iter().filter(|t| t.net_pnl_usdt > 0.0).count();
        let mut trades = std::mem::take(&mut self.trades);
        trades.extend(open);
        trades.sort_by(|a, b| {
            a.entry_ms
                .cmp(&b.entry_ms)
                .then_with(|| a.asset.cmp(&b.asset))
        });
        let start = self.cfg.starting_balance;
        let equities: Vec<f64> = self.step_equity.iter().map(|(_, e)| *e).collect();
        RunResult {
            name: self.persona.name().to_string(),
            starting_balance: start,
            final_equity,
            net_pnl_usdt: final_equity - start,
            net_pnl_pct: (final_equity / start - 1.0) * 100.0,
            max_drawdown_pct: self.max_drawdown_pct,
            round_trips: closed,
            wins,
            win_rate_pct: if closed == 0 {
                0.0
            } else {
                wins as f64 / closed as f64 * 100.0
            },
            sharpe: sharpe(&equities, self.cfg.step_minutes),
            fees_usdt: trades.iter().map(|t| t.fees_usdt).sum(),
            steps: self.steps,
            skipped_orders: self.skipped,
            trades,
        }
    }
}

/// Largest peak-to-trough fall of an equity path, in percent of the peak.
pub fn max_drawdown_pct(equity: &[f64]) -> f64 {
    let mut peak = f64::MIN;
    let mut worst: f64 = 0.0;
    for &e in equity {
        peak = peak.max(e);
        if peak > 0.0 {
            worst = worst.max((peak - e) / peak * 100.0);
        }
    }
    worst
}

/// Annualized Sharpe of per-step equity returns: mean / population std ×
/// √(minutes per year / step minutes). 0 without variance.
pub fn sharpe(equity: &[f64], step_minutes: i64) -> f64 {
    let returns: Vec<f64> = equity
        .windows(2)
        .filter(|w| w[0] > 0.0)
        .map(|w| w[1] / w[0] - 1.0)
        .collect();
    if returns.len() < 2 {
        return 0.0;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let std = var.sqrt();
    if std <= 0.0 {
        return 0.0;
    }
    mean / std * (MINUTES_PER_YEAR / step_minutes as f64).sqrt()
}

/// Run one persona over the window.
pub fn run(market: &Market, cfg: &Config, persona: Box<dyn Persona>) -> Result<RunResult> {
    cfg.validate()?;
    let mut account = Account::new(persona, *cfg);
    let mut m = cfg.start_ms;
    while m < cfg.end_ms {
        if cfg.is_step(m) {
            let snap = Snapshot::at(market, m);
            account.step(market, &snap);
        }
        account.minute(market, m);
        m += MINUTE_MS;
    }
    Ok(account.finish(cfg.end_ms))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const T0: i64 = 1_784_448_000_000; // 2026-07-19T08:00Z, an hour boundary

    pub fn cfg(start_minute: i64, end_minute: i64, step_minutes: i64) -> Config {
        Config {
            start_ms: T0 + start_minute * MINUTE_MS,
            end_ms: T0 + end_minute * MINUTE_MS,
            step_minutes,
            fee_rate: 0.00075,
            slippage: 0.0005,
            starting_balance: 1000.0,
            min_notional: 5.0,
        }
    }

    /// Minutes `0..n` of a coin whose open and close are `price(minute)`.
    pub fn coin(asset: &str, n: i64, price: impl Fn(i64) -> f64) -> Series {
        let bars = (0..n)
            .map(|m| {
                let p = price(m);
                Bar {
                    open_ms: T0 + m * MINUTE_MS,
                    open: p,
                    high: p,
                    low: p,
                    close: p,
                    volume: 1.0,
                }
            })
            .collect();
        Series::new(asset, bars)
    }

    /// Plays a fixed script: `(step time, intents)`; optional exit plan.
    struct Script {
        plan: Option<ExitPlan>,
        script: Vec<(i64, Vec<Intent>)>,
    }

    impl Persona for Script {
        fn name(&self) -> &str {
            "script"
        }
        fn decide(&mut self, snap: &Snapshot, _book: &Book) -> Vec<Intent> {
            self.script
                .iter()
                .filter(|(t, _)| *t == snap.t)
                .flat_map(|(_, i)| i.clone())
                .collect()
        }
        fn exit_plan(&self) -> Option<ExitPlan> {
            self.plan
        }
    }

    fn buy(asset: &str, fraction: f64) -> Intent {
        Intent::Buy {
            asset: asset.into(),
            equity_fraction: fraction,
        }
    }

    fn sell(asset: &str) -> Intent {
        Intent::Sell {
            asset: asset.into(),
            reason: "persona",
        }
    }

    fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn one_trade_matches_the_hand_calculation_with_both_fees_and_slippage() {
        // 100 until minute 75, then 110. Buy at the 60 step, sell at the 80 step.
        let market =
            Market::from_series([coin("SOL", 200, |m| if m < 75 { 100.0 } else { 110.0 })]);
        let c = cfg(60, 100, 20);
        let persona = Script {
            plan: None,
            script: vec![
                (c.start_ms, vec![buy("SOL", 1.0)]),
                (c.start_ms + 20 * MINUTE_MS, vec![sell("SOL")]),
            ],
        };
        let r = run(&market, &c, Box::new(persona)).unwrap();
        // qty = 1000 / (100 × 1.0005 × 1.00075); proceeds = qty × 110 × 0.9995 × 0.99925.
        let expected = 97.253_434_237_165_72;
        assert_eq!(r.trades.len(), 1);
        assert!(
            near(r.trades[0].net_pnl_usdt, expected, 1e-9),
            "{:?}",
            r.trades[0]
        );
        assert!(near(r.net_pnl_usdt, expected, 1e-9));
        let qty = 1000.0 / (100.0 * 1.0005 * 1.00075);
        let fees = qty * 100.0 * 1.0005 * 0.00075 + qty * 110.0 * 0.9995 * 0.00075;
        assert!(near(r.fees_usdt, fees, 1e-9));
        assert_eq!((r.round_trips, r.wins), (1, 1));
        assert!(near(r.win_rate_pct, 100.0, 1e-12));
        assert_eq!(r.trades[0].reason, "persona");
        assert_eq!(r.steps, 2);
    }

    #[test]
    fn an_exit_plan_sells_at_its_fill_price_minus_fee_and_slippage() {
        // deadline=30: bought at minute 60 (100), sold at the open of minute 90 (105).
        let market =
            Market::from_series([coin("SOL", 200, |m| if m < 90 { 100.0 } else { 105.0 })]);
        let c = cfg(60, 150, 120);
        let persona = Script {
            plan: Some(ExitPlan::parse_spec("deadline=30").unwrap()),
            script: vec![(c.start_ms, vec![buy("SOL", 1.0)])],
        };
        let r = run(&market, &c, Box::new(persona)).unwrap();
        assert_eq!(r.trades.len(), 1);
        let trade = &r.trades[0];
        assert_eq!(trade.reason, "deadline");
        assert_eq!(trade.exit_ms, c.start_ms + 30 * MINUTE_MS);
        assert!(!trade.open_at_end);
        assert!(
            near(trade.net_pnl_usdt, 47.378_278_135_476_42, 1e-9),
            "{trade:?}"
        );
    }

    #[test]
    fn small_buys_and_missing_atr_are_skipped() {
        let market = Market::from_series([coin("SOL", 200, |_| 100.0)]);
        let c = cfg(60, 100, 20);
        let tiny = Script {
            plan: None,
            script: vec![(c.start_ms, vec![buy("SOL", 0.004)])],
        };
        let r = run(&market, &c, Box::new(tiny)).unwrap();
        assert_eq!((r.trades.len(), r.skipped_orders), (0, 1));
        // An hourly ATR needs closed 1h bars; a flat price has a zero range.
        let atr = Script {
            plan: Some(ExitPlan::parse_spec("atr=3.5:1h").unwrap()),
            script: vec![(c.start_ms, vec![buy("SOL", 1.0)])],
        };
        let r = run(&market, &c, Box::new(atr)).unwrap();
        assert_eq!((r.trades.len(), r.skipped_orders), (0, 1));
        assert!(near(r.final_equity, 1000.0, 1e-12));
    }

    #[test]
    fn max_drawdown_is_the_worst_fall_from_a_peak() {
        assert!(near(
            max_drawdown_pct(&[100.0, 120.0, 90.0, 130.0, 117.0, 140.0]),
            25.0,
            1e-12
        ));
        assert!(near(max_drawdown_pct(&[100.0, 101.0, 102.0]), 0.0, 1e-12));
        // The engine's running drawdown on a coin that rises 10% and falls back.
        let market = Market::from_series([coin("SOL", 200, |m| match m {
            0..70 => 100.0,
            70..80 => 110.0,
            _ => 88.0,
        })]);
        let c = cfg(60, 100, 60);
        let persona = Script {
            plan: None,
            script: vec![(c.start_ms, vec![buy("SOL", 1.0)])],
        };
        let r = run(&market, &c, Box::new(persona)).unwrap();
        assert!(
            near(r.max_drawdown_pct, 20.0, 1e-9),
            "{}",
            r.max_drawdown_pct
        );
    }

    /// Each step's snapshot and the intents returned for it.
    type Seen = std::rc::Rc<std::cell::RefCell<Vec<(Snapshot, Vec<Intent>)>>>;

    /// Records every snapshot it sees and buys the best 1h coin, selling on a 15m drop.
    struct Recorder {
        seen: Seen,
    }

    impl Persona for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
            let mut intents = Vec::new();
            for asset in book.holdings.keys() {
                if snap
                    .coin(asset)
                    .and_then(|c| c.ret_15m)
                    .is_some_and(|r| r < 0.0)
                {
                    intents.push(sell(asset));
                }
            }
            if book.is_flat()
                && let Some(best) = snap
                    .liquid_top
                    .iter()
                    .filter_map(|a| Some((a, snap.coin(a)?.ret_1h?)))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
            {
                intents.push(buy(best.0, 1.0));
            }
            self.seen.borrow_mut().push((snap.clone(), intents.clone()));
            intents
        }
    }

    pub fn wavy_market(minutes: i64) -> Market {
        Market::from_series([
            coin("BTC", minutes, |m| 100.0 + 5.0 * (m as f64 / 97.0).sin()),
            coin("ETH", minutes, |m| 50.0 + 3.0 * (m as f64 / 61.0).cos()),
            coin("SOL", minutes, |m| {
                20.0 + (m as f64 / 13.0).sin() + m as f64 / 2000.0
            }),
        ])
    }

    fn truncated(market: &Market, t: i64) -> Market {
        Market::from_series(
            market
                .iter()
                .map(|s| Series::new(s.asset(), s.closed_by(t).to_vec())),
        )
    }

    #[test]
    fn decisions_up_to_a_step_do_not_change_when_later_bars_are_removed() {
        let market = wavy_market(3 * DAY_MINUTES);
        let c = cfg(DAY_MINUTES + 30, 3 * DAY_MINUTES, 20);
        let full = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        run(&market, &c, Box::new(Recorder { seen: full.clone() })).unwrap();
        let cut_t = c.start_ms + 40 * c.step_ms();
        let cut = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        run(
            &truncated(&market, cut_t),
            &c,
            Box::new(Recorder { seen: cut.clone() }),
        )
        .unwrap();

        let full = full.borrow();
        let cut = cut.borrow();
        let upto = |seen: &[(Snapshot, Vec<Intent>)]| -> Vec<(Snapshot, Vec<Intent>)> {
            seen.iter().filter(|(s, _)| s.t <= cut_t).cloned().collect()
        };
        assert_eq!(upto(&full).len(), 41);
        assert!(full.iter().any(|(_, i)| !i.is_empty()));
        assert!(full.iter().all(|(s, _)| s.score.is_some()));
        assert_eq!(upto(&full), upto(&cut));
    }

    #[test]
    fn trade_pnl_sums_to_the_equity_change() {
        let market = wavy_market(3 * DAY_MINUTES);
        let c = cfg(DAY_MINUTES, 3 * DAY_MINUTES, 20);
        let last_step = c.end_ms - c.step_ms();
        // Flat at the end: every holding is sold at the last step.
        struct Churn {
            last_step: i64,
            inner: Recorder,
        }
        impl Persona for Churn {
            fn name(&self) -> &str {
                "churn"
            }
            fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent> {
                if snap.t == self.last_step {
                    return book.holdings.keys().map(|a| sell(a)).collect();
                }
                self.inner.decide(snap, book)
            }
        }
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let churn = Churn {
            last_step,
            inner: Recorder { seen },
        };
        let r = run(&market, &c, Box::new(churn)).unwrap();
        assert!(r.round_trips > 5, "{}", r.round_trips);
        assert!(r.trades.iter().all(|t| !t.open_at_end));
        let sum: f64 = r.trades.iter().map(|t| t.net_pnl_usdt).sum();
        assert!(
            near(sum, r.final_equity - 1000.0, 1e-9),
            "{sum} vs {}",
            r.net_pnl_usdt
        );
        assert!(r.fees_usdt > 0.0);

        // Still holding at the end: the open position is marked and the sum still holds.
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let r = run(&market, &c, Box::new(Recorder { seen })).unwrap();
        let sum: f64 = r.trades.iter().map(|t| t.net_pnl_usdt).sum();
        assert!(near(sum, r.net_pnl_usdt, 1e-9));
        assert_eq!(
            r.round_trips,
            r.trades.iter().filter(|t| !t.open_at_end).count()
        );
    }

    #[test]
    fn sharpe_is_zero_without_variance_and_signed_by_the_mean() {
        assert!(near(sharpe(&[100.0, 100.0, 100.0], 20), 0.0, 1e-12));
        assert!(sharpe(&[100.0, 101.0, 101.5, 103.0], 20) > 0.0);
        assert!(sharpe(&[100.0, 99.0, 98.5, 97.0], 20) < 0.0);
    }

    #[test]
    fn snapshot_ranks_the_liquid_top_by_quote_volume_and_skips_missing_coins() {
        let thin = coin("ETH", 200, |_| 10.0);
        let big = Series::new(
            "ETH",
            thin.bars()
                .iter()
                .map(|b| Bar {
                    volume: 1000.0,
                    ..*b
                })
                .collect(),
        );
        let market = Market::from_series([
            coin("BTC", 200, |_| 100.0),
            big,
            coin("SOL", 100, |_| 20.0),
            coin("INJ", 200, |_| 1.0),
        ]);
        let snap = Snapshot::at(&market, T0 + 150 * MINUTE_MS);
        // ETH 10 × 1000 per minute beats BTC 100 × 1; SOL has no bar at 150; INJ is not liquid.
        assert_eq!(snap.liquid_top, vec!["ETH", "BTC"]);
        assert!(snap.coin("INJ").is_some());
        assert!(snap.coin("SOL").is_none());
    }
}

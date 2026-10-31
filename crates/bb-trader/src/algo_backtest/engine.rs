//! The backtest engine: one USDT account per rule, stepped on a fixed clock over cached 1m
//! bars.
//!
//! - Every `step_minutes` the rule sees a point-in-time [`Snapshot`] (only bars closed
//!   by the step time `T`) and its [`Book`], and returns [`Intent`]s.
//! - Intents fill at the open of the 1m bar that opens at `T`: sells first, then buys. A buy
//!   pays `price × (1 + slippage)` plus the fee; a sell receives `price × (1 − slippage)`
//!   minus the fee. Buys under `min_notional` USDT are skipped.
//! - Equity (cash plus holdings at their last close, no exit costs) is marked at every 1m
//!   close; open positions at the window end are marked the same way.
//!
//! No network and no order path: the engine only reads [`Market`].

use std::collections::BTreeMap;

use super::data::{Bar, MINUTE_MS, Market};
use crate::error::{Result, TraderError};
use crate::book::{Book, CoinView, Holding, Snapshot};

/// The 20-coin liquid universe: the `bb entry-study` top 20 by quote volume on 2026-10-18
/// (the Phase 47 basket). Fixed, so the backtest has survivorship bias.
pub const LIQUID: [&str; 20] = [
    "BTC", "ETH", "ZEC", "SOL", "NEAR", "XRP", "BNB", "WLD", "HYPE", "DOGE", "SUI", "SAND", "TRX",
    "UNI", "ENA", "STRK", "TAO", "HBAR", "TIA", "LINK",
];
/// Candidates per step, as the live `BOT_SPOT_CANDIDATE_LIMIT=10`.
pub const CANDIDATE_LIMIT: usize = 10;
const MINUTES_PER_YEAR: f64 = 525_600.0;
const DAY_MINUTES: i64 = 1_440;

/// Run settings of one backtest run.
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
                    ret_24h: s.return_pct(t, DAY_MINUTES),
                    ret_7d: s.return_pct(t, 7 * DAY_MINUTES),
                    quote_volume_24h: s.quote_volume(t, DAY_MINUTES),
                };
                (s.asset().to_string(), view)
            })
            .collect();
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
            coins,
            liquid_top,
        }
    }
}

/// What a rule asks for at a step.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Spend `equity_fraction` of the step's equity (capped by cash) on `asset`.
    Buy { asset: String, equity_fraction: f64 },
    /// Sell all of `asset`; `reason` goes to the trade list.
    Sell { asset: String, reason: &'static str },
}

/// A deterministic trading rule set.
pub trait StepRule {
    fn name(&self) -> &str;
    /// Intents at `snap.t`. Must use only `snap` and `book`.
    fn decide(&mut self, snap: &Snapshot, book: &Book) -> Vec<Intent>;
    /// Lines for the report about how the rule behaved.
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
    /// A per-step CSV the report writes next to the trades (e.g. `biz_algo`'s scores).
    fn trace(&self) -> Option<Trace> {
        None
    }
}

/// A rule's per-step record: a CSV header and one row per step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub header: String,
    pub rows: Vec<String>,
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

/// Result of one rule over the window.
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
    /// Intents that could not fill, in total and by reason (`no_cash`, `under_min_notional`,
    /// `buy_no_bar`, `sell_no_bar`, `unknown_asset`).
    pub skipped_orders: usize,
    pub skip_reasons: BTreeMap<&'static str, usize>,
    pub trades: Vec<Trade>,
    /// [`StepRule::notes`] at the end of the run.
    pub notes: Vec<String>,
    /// [`StepRule::trace`] at the end of the run.
    pub trace: Option<Trace>,
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
}

/// One rule's account, advanced step by step and minute by minute.
pub struct Account {
    rule: Box<dyn StepRule>,
    cfg: Config,
    cash: f64,
    positions: BTreeMap<String, Position>,
    trades: Vec<Trade>,
    step_equity: Vec<(i64, f64)>,
    peak_equity: f64,
    max_drawdown_pct: f64,
    steps: usize,
    skips: BTreeMap<&'static str, usize>,
}

impl Account {
    pub fn new(rule: Box<dyn StepRule>, cfg: Config) -> Self {
        Self {
            rule,
            cfg,
            cash: cfg.starting_balance,
            positions: BTreeMap::new(),
            trades: Vec::new(),
            step_equity: Vec::new(),
            peak_equity: cfg.starting_balance,
            max_drawdown_pct: 0.0,
            steps: 0,
            skips: BTreeMap::new(),
        }
    }

    pub fn name(&self) -> &str {
        self.rule.name()
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
                        value: p.qty * p.last_close,
                        entry: Some((p.entry_price, p.entry_ms)),
                    };
                    (asset.clone(), holding)
                })
                .collect(),
        }
    }

    /// Run the step at `snap.t`: record equity, ask the rule, fill its intents at the
    /// open of the bar that opens at `snap.t`. Returns the intents (for tests and votes).
    pub fn step(&mut self, market: &Market, snap: &Snapshot) -> Vec<Intent> {
        let t = snap.t;
        self.steps += 1;
        let equity = self.equity();
        self.step_equity.push((t, equity));
        let book = self.book(t);
        let intents = self.rule.decide(snap, &book);
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
            self.skip("sell_no_bar");
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
        if market.get(asset).is_none() {
            self.skip("unknown_asset");
            return;
        }
        let Some(bar) = Self::fill_bar(market, asset, t) else {
            self.skip("buy_no_bar");
            return;
        };
        if notional <= 0.0 {
            self.skip("no_cash");
            return;
        }
        if notional < self.cfg.min_notional {
            self.skip("under_min_notional");
            return;
        }
        let fill = bar.open * (1.0 + self.cfg.slippage);
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
            },
        );
    }

    fn skip(&mut self, reason: &'static str) {
        *self.skips.entry(reason).or_default() += 1;
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
        let done = self
            .positions
            .get(asset)
            .is_some_and(|p| p.qty <= p.original_qty * 1e-12);
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

    /// Process the 1m bar that opens at `m`: the mark at its close.
    pub fn minute(&mut self, market: &Market, m: i64) {
        let assets: Vec<String> = self.positions.keys().cloned().collect();
        for asset in assets {
            let Some(bar) = Self::fill_bar(market, &asset, m) else {
                continue;
            };
            let Some(position) = self.positions.get_mut(&asset) else {
                continue;
            };
            position.last_close = bar.close;
            if position.qty > 0.0 {
                position.last_ms = bar.open_ms + MINUTE_MS;
            }
            self.close_if_done(&asset);
        }
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
            name: self.rule.name().to_string(),
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
            skipped_orders: self.skips.values().sum(),
            skip_reasons: self.skips,
            trades,
            notes: self.rule.notes(),
            trace: self.rule.trace(),
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

/// Run one rule over the window.
pub fn run(market: &Market, cfg: &Config, rule: Box<dyn StepRule>) -> Result<RunResult> {
    cfg.validate()?;
    let mut account = Account::new(rule, *cfg);
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
    use crate::algo_backtest::data::Series;

    pub use crate::book::tests::T0;

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

    /// Plays a fixed script: `(step time, intents)`.
    struct Script {
        script: Vec<(i64, Vec<Intent>)>,
    }

    impl StepRule for Script {
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
            reason: "rule",
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
        let rule = Script {
            script: vec![
                (c.start_ms, vec![buy("SOL", 1.0)]),
                (c.start_ms + 20 * MINUTE_MS, vec![sell("SOL")]),
            ],
        };
        let r = run(&market, &c, Box::new(rule)).unwrap();
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
        assert_eq!(r.trades[0].reason, "rule");
        assert_eq!(r.steps, 2);
    }

    #[test]
    fn small_buys_and_spent_cash_are_skipped_by_reason() {
        let market = Market::from_series([coin("SOL", 200, |_| 100.0), coin("ETH", 200, |_| 10.0)]);
        let c = cfg(60, 100, 20);
        let tiny = Script {
            script: vec![(c.start_ms, vec![buy("SOL", 0.004)])],
        };
        let r = run(&market, &c, Box::new(tiny)).unwrap();
        assert_eq!((r.trades.len(), r.skipped_orders), (0, 1));
        assert_eq!(r.skip_reasons.get("under_min_notional"), Some(&1));
        // The first buy spends all cash, so the second one in the same step has none.
        let all_in = Script {
            script: vec![(c.start_ms, vec![buy("ETH", 1.0), buy("SOL", 1.0)])],
        };
        let r = run(&market, &c, Box::new(all_in)).unwrap();
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.skip_reasons.get("no_cash"), Some(&1));
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
        let rule = Script {
            script: vec![(c.start_ms, vec![buy("SOL", 1.0)])],
        };
        let r = run(&market, &c, Box::new(rule)).unwrap();
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

    impl StepRule for Recorder {
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

    /// The market as known at `t`: candles closed by `t`.
    pub fn truncated(market: &Market, t: i64) -> Market {
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
        impl StepRule for Churn {
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

    #[test]
    fn snapshot_returns_cover_24h_and_7d_from_the_1m_closes() {
        let market = Market::from_series([coin("BTC", 10_200, |m| 100.0 + m as f64)]);
        // At minute 1500 the close is bar 1499's (1599); 24h earlier it is bar 59's (159).
        let snap = Snapshot::at(&market, T0 + 1500 * MINUTE_MS);
        let btc = snap.coin("BTC").unwrap();
        assert!((btc.ret_24h.unwrap() - (1599.0 / 159.0 - 1.0) * 100.0).abs() < 1e-9);
        assert!((btc.ret_1h.unwrap() - (1599.0 / 1539.0 - 1.0) * 100.0).abs() < 1e-9);
        // Less than a week of bars: the 7d return is unknown, not 0.
        assert_eq!(btc.ret_7d, None);
        // At minute 10 100 the close is 10 199; 7 days (10 080 minutes) earlier, bar 19's 119.
        let week = Snapshot::at(&market, T0 + 10_100 * MINUTE_MS);
        let btc = week.coin("BTC").unwrap();
        assert!((btc.ret_7d.unwrap() - (10_199.0 / 119.0 - 1.0) * 100.0).abs() < 1e-9);
        // Less than a day of bars: the 24h return is unknown too.
        let early = Snapshot::at(&market, T0 + 1000 * MINUTE_MS);
        assert_eq!(early.coin("BTC").unwrap().ret_24h, None);
    }
}

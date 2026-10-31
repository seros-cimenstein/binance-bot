//! The trader's market and account view shared by the live algorithm and the backtest
//! engine: a point-in-time [`Snapshot`] of coins and the [`Book`] (cash and holdings) the
//! rule decides on, plus [`live_book`], which builds the book from live balances.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::universe::Balances;

/// One coin at a step: returns (percent) of the close at `T` over 15m/1h/4h/24h/7d and its
/// trailing 24h quote volume. Only coins with a bar closing at `T` are present. Live, the
/// 24h return is the market's 24h ticker change (refreshed with the catalog) and the 7d
/// return compares the newest price with the hourly close 7 days back (1h klines read at
/// most hourly); in the backtest both come from the 1m closes.
#[derive(Debug, Clone, PartialEq)]
pub struct CoinView {
    pub ret_15m: Option<f64>,
    pub ret_1h: Option<f64>,
    pub ret_4h: Option<f64>,
    pub ret_24h: Option<f64>,
    pub ret_7d: Option<f64>,
    pub quote_volume_24h: f64,
}

/// What the rule may see at step time `T`.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub t: i64,
    pub coins: BTreeMap<String, CoinView>,
    /// The [`LIQUID`] coins present at `T`, top [`CANDIDATE_LIMIT`] by 24h quote volume.
    pub liquid_top: Vec<String>,
}

impl Snapshot {
    /// The live algorithm's view: the coins it read prices for and the coins it may buy.
    pub fn live(t: i64, coins: BTreeMap<String, CoinView>, candidates: Vec<String>) -> Self {
        Self {
            t,
            coins,
            liquid_top: candidates,
        }
    }

    pub fn coin(&self, asset: &str) -> Option<&CoinView> {
        self.coins.get(asset)
    }
}

/// One holding as the rule sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    pub qty: f64,
    /// `qty` at the last close.
    pub value: f64,
    /// Buy fill price and time (`None` in the live book, which has no entry per coin).
    pub entry: Option<(f64, i64)>,
}

/// The rule's account at a step.
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

const MINUTE_MS: i64 = 60_000;

/// A coin's view from its closed 1m bars `(open time ms, close)`, oldest first: the 15m and
/// 1h returns of the newest close, `None` when the bar 15 or 60 minutes back is missing
/// (the live trader adds the 24h and 7d returns). `None` without bars.
pub fn coin_view_from_minutes(bars: &[(i64, f64)]) -> Option<CoinView> {
    let &(newest_open, newest_close) = bars.last()?;
    let ret = |minutes: i64| {
        let open = newest_open - minutes * MINUTE_MS;
        bars.iter()
            .find(|(t, _)| *t == open)
            .filter(|(_, close)| *close > 0.0)
            .map(|(_, close)| (newest_close / close - 1.0) * 100.0)
    };
    Some(CoinView {
        ret_15m: ret(15),
        ret_1h: ret(60),
        ret_4h: None,
        ret_24h: None,
        ret_7d: None,
        quote_volume_24h: 0.0,
    })
}

fn float(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

/// The live book the rule decides on: USDT cash, and the sleeve's own holdings (`owned`,
/// asset → quantity, capped at the free balance) worth at least their USDT market's
/// minimum order (`min_notional`), leaving out USDT itself and cash-like coins
/// (`assets::is_low_upside`). Dust a market order could not sell is not a position, and a
/// balance the sleeves do not own is neither held nor counted (Phase 53,
/// [`crate::sleeves`]): the portfolio value is the cash plus those holdings.
pub fn live_book(
    t: i64,
    balances: &Balances,
    owned: &BTreeMap<String, Decimal>,
    value_usdt: impl Fn(&str, Decimal) -> Option<Decimal>,
    min_notional_usdt: impl Fn(&str) -> Option<Decimal>,
) -> Book {
    let holdings: BTreeMap<String, Holding> = owned
        .iter()
        .filter(|(asset, _)| *asset != "USDT" && !crate::assets::is_low_upside(asset))
        .filter_map(|(asset, qty)| {
            let qty = (*qty).min(balances.free(asset));
            let value = value_usdt(asset, qty)?;
            let min = min_notional_usdt(asset)?;
            (value > Decimal::ZERO && value >= min).then(|| {
                let holding = Holding {
                    qty: float(qty),
                    value: float(value),
                    entry: None,
                };
                (asset.clone(), holding)
            })
        })
        .collect();
    let cash = float(balances.free("USDT"));
    Book {
        t,
        cash,
        equity: cash + holdings.values().map(|h| h.value).sum::<f64>(),
        holdings,
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::BTreeMap;

    use rust_decimal_macros::dec;

    use super::*;

    pub const T0: i64 = 1_784_448_000_000; // 2026-07-19T08:00Z, an hour boundary

    /// `(asset, 15m, 1h, 4h, 24h quote volume)`.
    pub type Row<'a> = (&'a str, f64, f64, f64, f64);

    /// A snapshot of the given coins; every coin is a liquid candidate in the given order.
    pub fn snap(rows: &[Row<'_>]) -> Snapshot {
        Snapshot {
            t: T0,
            coins: rows
                .iter()
                .map(|&(a, r15, r1, r4, qv)| {
                    let view = CoinView {
                        ret_15m: Some(r15),
                        ret_1h: Some(r1),
                        ret_4h: Some(r4),
                        ret_24h: None,
                        ret_7d: None,
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
                        value: 1000.0 / assets.len() as f64,
                        entry: None,
                    };
                    (a.to_string(), h)
                })
                .collect(),
        }
    }

    #[test]
    fn the_live_book_holds_only_owned_sellable_coins() {
        let balances = Balances {
            free: [
                ("USDT".to_string(), dec!(12)),
                ("USDC".to_string(), dec!(50)),
                ("SOL".to_string(), dec!(0.1)),
                ("PEPE".to_string(), dec!(100)),
                ("ODD".to_string(), dec!(1)),
                ("BNB".to_string(), dec!(1)),
            ]
            .into(),
            ..Balances::default()
        };
        let value = |asset: &str, qty: Decimal| match asset {
            "USDC" => Some(qty),
            "SOL" => Some(qty * dec!(180)),
            "PEPE" => Some(dec!(0.4)),
            "BNB" => Some(qty * dec!(600)),
            _ => None,
        };
        // Owned: SOL (more than the wallet has: capped), PEPE dust, USDC and ODD (no
        // value); BNB is in the wallet but unowned.
        let owned: BTreeMap<String, Decimal> = [("SOL", dec!(0.2)), ("PEPE", dec!(100)), ("USDC", dec!(50)), ("ODD", dec!(1))]
            .map(|(a, q)| (a.to_owned(), q))
            .into();
        let book = live_book(T0, &balances, &owned, value, |_| Some(dec!(5)));
        assert_eq!(book.holdings.keys().collect::<Vec<_>>(), vec!["SOL"]);
        assert_eq!((book.holdings["SOL"].qty, book.holdings["SOL"].value), (0.1, 18.0));
        // Cash plus the owned SOL; the unowned 600 USDT of BNB is not counted.
        assert_eq!((book.cash, book.equity), (12.0, 30.0));
        let none = live_book(T0, &balances, &BTreeMap::new(), value, |_| Some(dec!(5)));
        assert!(none.holdings.is_empty());
    }

    #[test]
    fn coin_views_come_from_the_newest_closed_minute_and_need_the_bars_back() {
        let bar = |minute: i64, close: f64| (T0 + minute * MINUTE_MS, close);
        // 61 bars: 100 an hour before the newest close of 103, 102 fifteen minutes before.
        let mut bars: Vec<(i64, f64)> = (0..=60).map(|m| bar(m, 100.0)).collect();
        bars[45].1 = 102.0;
        bars[60].1 = 103.0;
        let view = coin_view_from_minutes(&bars).unwrap();
        assert!((view.ret_1h.unwrap() - 3.0).abs() < 1e-9);
        assert!((view.ret_15m.unwrap() - 103.0 / 102.0 * 100.0 + 100.0).abs() < 1e-9);
        // A missing bar 60 minutes back leaves the 1h return unknown, not zero.
        let view = coin_view_from_minutes(&bars[1..]).unwrap();
        assert_eq!(view.ret_1h, None);
        assert!(view.ret_15m.is_some());
        assert_eq!(coin_view_from_minutes(&[]), None);
    }
}

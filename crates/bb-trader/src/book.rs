//! The trader's market and account view shared by the live algorithm and the backtest
//! engine: a point-in-time [`Snapshot`] of coins and the [`Book`] (cash and holdings) the
//! rule decides on, plus [`live_book`], which builds the book from live balances.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::gcp2::Gcp2View;
use crate::market_mood::HorizonBreadth;
use crate::universe::Balances;

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
    /// GCP2 at `T` (the hour ending at the last second before `T`'s minute); `None` without
    /// GCP2 history or a reading.
    pub gcp2: Option<Gcp2View>,
}

impl Snapshot {
    /// The live algorithm's view: the coins it read prices for and the coins it may buy,
    /// with no market-wide breadth score.
    pub fn live(t: i64, coins: BTreeMap<String, CoinView>, candidates: Vec<String>) -> Self {
        Self {
            t,
            score: None,
            horizons: Vec::new(),
            coins,
            liquid_top: candidates,
            gcp2: None,
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

/// One holding as the persona sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    pub qty: f64,
    /// `qty` at the last close.
    pub value: f64,
    /// Buy fill price and time (`None` in the live book, which has no entry per coin).
    pub entry: Option<(f64, i64)>,
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

const MINUTE_MS: i64 = 60_000;

/// A coin's view from its closed 1m bars `(open time ms, close)`, oldest first: the 15m and
/// 1h returns of the newest close, `None` when the bar 15 or 60 minutes back is missing.
/// `None` without bars.
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
        quote_volume_24h: 0.0,
    })
}

fn float(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

/// The live book the personas vote on: USDT cash, the portfolio value, and every free
/// balance worth at least its USDT market's minimum order (`min_notional`), leaving out
/// USDT itself and cash-like coins (`assets::is_low_upside`). Dust a market order could
/// not sell is not a position.
pub fn live_book(
    t: i64,
    balances: &Balances,
    equity: Decimal,
    value_usdt: impl Fn(&str, Decimal) -> Option<Decimal>,
    min_notional_usdt: impl Fn(&str) -> Option<Decimal>,
) -> Book {
    let holdings = balances
        .free
        .iter()
        .filter(|(asset, _)| *asset != "USDT" && !crate::assets::is_low_upside(asset))
        .filter_map(|(asset, qty)| {
            let value = value_usdt(asset, *qty)?;
            let min = min_notional_usdt(asset)?;
            (value > Decimal::ZERO && value >= min).then(|| {
                let holding = Holding {
                    qty: float(*qty),
                    value: float(value),
                    entry: None,
                };
                (asset.clone(), holding)
            })
        })
        .collect();
    Book {
        t,
        cash: float(balances.free("USDT")),
        equity: float(equity),
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
                        entry: None,
                    };
                    (a.to_string(), h)
                })
                .collect(),
        }
    }

    #[test]
    fn the_live_book_skips_usdt_cash_like_coins_and_unsellable_dust() {
        let balances = Balances {
            free: [
                ("USDT".to_string(), dec!(12)),
                ("USDC".to_string(), dec!(50)),
                ("SOL".to_string(), dec!(0.1)),
                ("PEPE".to_string(), dec!(100)),
                ("ODD".to_string(), dec!(1)),
            ]
            .into(),
            ..Balances::default()
        };
        let value = |asset: &str, qty: Decimal| match asset {
            "USDC" => Some(qty),
            "SOL" => Some(qty * dec!(180)),
            "PEPE" => Some(dec!(0.4)),
            _ => None,
        };
        let book = live_book(T0, &balances, dec!(80), value, |_| Some(dec!(5)));
        assert_eq!(book.holdings.keys().collect::<Vec<_>>(), vec!["SOL"]);
        assert_eq!(book.holdings["SOL"].value, 18.0);
        assert_eq!((book.cash, book.equity), (12.0, 80.0));
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

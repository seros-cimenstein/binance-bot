//! Polymarket binary-arbitrage scanner (read-only, advisory).
//!
//! For each active binary (YES/NO) market, both tokens' orderbooks are fetched and
//! checked for two patterns:
//! - **buy both**: `best_ask(YES) + best_ask(NO) < 1`; one share of each pays exactly
//!   $1 at resolution, so the gross profit per pair is `1 - (ask_yes + ask_no)`.
//! - **sell both**: `best_bid(YES) + best_bid(NO) > 1`; collecting more than $1 against
//!   a $1 liability. Needs short capacity, so it is primarily advisory.
//!
//! The per-leg taker fee is charged on an upper-bound $1 notional per leg. No orders
//! are ever placed here.

use std::collections::HashMap;

use futures::TryStreamExt;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::client::{Orderbook, PolymarketClient, PolymarketError, SimplifiedMarket};

const MARKETS_PER_PAGE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanConfig {
    pub max_markets: usize,
    pub taker_fee_bps_per_leg: Decimal,
    /// Required depth at the best price on each leg.
    pub min_size_per_leg: Decimal,
    /// Net profit per pair that must be strictly exceeded.
    pub min_gross_profit: Decimal,
    pub fetch_concurrency: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            max_markets: 500,
            taker_fee_bps_per_leg: Decimal::TEN,
            min_size_per_leg: Decimal::new(5, 0),
            min_gross_profit: Decimal::new(5, 3),
            fetch_concurrency: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    BuyBoth,
    SellBoth,
    None,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuyBoth => "buy_both",
            Self::SellBoth => "sell_both",
            Self::None => "none",
        }
    }
}

/// One scored arbitrage candidate from a single binary market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArbOpportunity {
    pub condition_id: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub yes_outcome: String,
    pub no_outcome: String,

    pub best_ask_yes: Decimal,
    pub best_ask_no: Decimal,
    pub best_bid_yes: Decimal,
    pub best_bid_no: Decimal,
    pub size_at_best_ask_yes: Decimal,
    pub size_at_best_ask_no: Decimal,
    pub size_at_best_bid_yes: Decimal,
    pub size_at_best_bid_no: Decimal,

    /// `1 - (ask_yes + ask_no)`.
    pub buy_both_gross_profit: Decimal,
    pub buy_both_net_profit: Decimal,
    /// `(bid_yes + bid_no) - 1`.
    pub sell_both_gross_profit: Decimal,
    pub sell_both_net_profit: Decimal,

    pub max_executable_pairs: Decimal,
    pub direction: Direction,
}

impl ArbOpportunity {
    /// Net profit per pair for the chosen direction (zero when there is none).
    pub fn net_margin(&self) -> Decimal {
        match self.direction {
            Direction::BuyBoth => self.buy_both_net_profit,
            Direction::SellBoth => self.sell_both_net_profit,
            Direction::None => Decimal::ZERO,
        }
    }

    fn rank_key(&self) -> Decimal {
        self.buy_both_net_profit.max(self.sell_both_net_profit) * self.max_executable_pairs
    }
}

/// Total taker fee on a YES + NO pair, assuming a $1 notional per leg (upper bound).
pub fn round_trip_fee_per_pair(taker_fee_bps_per_leg: Decimal) -> Decimal {
    Decimal::TWO * bb_core::money::apply_bps(Decimal::ONE, taker_fee_bps_per_leg)
}

/// Arb metrics for one market; `None` if it is not a binary market with both books.
pub fn evaluate_market(
    market: &SimplifiedMarket,
    orderbooks: &HashMap<String, Orderbook>,
    cfg: &ScanConfig,
) -> Option<ArbOpportunity> {
    let [yes_token, no_token] = market.tokens.as_slice() else {
        return None;
    };
    let book_yes = orderbooks.get(&yes_token.token_id)?;
    let book_no = orderbooks.get(&no_token.token_id)?;
    let ask_yes = *book_yes.best_ask()?;
    let ask_no = *book_no.best_ask()?;
    let bid_yes = *book_yes.best_bid()?;
    let bid_no = *book_no.best_bid()?;

    let fee_per_pair = round_trip_fee_per_pair(cfg.taker_fee_bps_per_leg);
    let buy_gross = Decimal::ONE - (ask_yes.price + ask_no.price);
    let sell_gross = (bid_yes.price + bid_no.price) - Decimal::ONE;
    let buy_net = buy_gross - fee_per_pair;
    let sell_net = sell_gross - fee_per_pair;

    // Pairs trade together, so the executable size is the thinner leg.
    let buy_max_pairs = if buy_net > Decimal::ZERO {
        ask_yes.size.min(ask_no.size)
    } else {
        Decimal::ZERO
    };
    let sell_max_pairs = if sell_net > Decimal::ZERO {
        bid_yes.size.min(bid_no.size)
    } else {
        Decimal::ZERO
    };

    let qualifies =
        |net: Decimal, pairs: Decimal| net > cfg.min_gross_profit && pairs >= cfg.min_size_per_leg;
    let (direction, max_pairs) = if buy_net >= sell_net {
        let direction = if qualifies(buy_net, buy_max_pairs) {
            Direction::BuyBoth
        } else {
            Direction::None
        };
        (direction, buy_max_pairs)
    } else {
        let direction = if qualifies(sell_net, sell_max_pairs) {
            Direction::SellBoth
        } else {
            Direction::None
        };
        (direction, sell_max_pairs)
    };

    Some(ArbOpportunity {
        condition_id: market.condition_id.clone(),
        yes_token_id: yes_token.token_id.clone(),
        no_token_id: no_token.token_id.clone(),
        yes_outcome: yes_token.outcome.clone(),
        no_outcome: no_token.outcome.clone(),
        best_ask_yes: ask_yes.price,
        best_ask_no: ask_no.price,
        best_bid_yes: bid_yes.price,
        best_bid_no: bid_no.price,
        size_at_best_ask_yes: ask_yes.size,
        size_at_best_ask_no: ask_no.size,
        size_at_best_bid_yes: bid_yes.size,
        size_at_best_bid_no: bid_no.size,
        buy_both_gross_profit: buy_gross,
        buy_both_net_profit: buy_net,
        sell_both_gross_profit: sell_gross,
        sell_both_net_profit: sell_net,
        max_executable_pairs: max_pairs,
        direction,
    })
}

/// One scan pass: actionable opportunities ranked by net margin × executable size.
pub async fn scan(
    client: &PolymarketClient,
    cfg: &ScanConfig,
) -> Result<Vec<ArbOpportunity>, PolymarketError> {
    let mut markets: Vec<SimplifiedMarket> = Vec::new();
    if cfg.max_markets > 0 {
        let max_pages = cfg.max_markets / MARKETS_PER_PAGE + 1;
        let stream = client.active_markets(max_pages, MARKETS_PER_PAGE);
        futures::pin_mut!(stream);
        while let Some(market) = stream.try_next().await? {
            if !market.accepting_orders || market.closed || market.tokens.len() != 2 {
                continue;
            }
            markets.push(market);
            if markets.len() >= cfg.max_markets {
                break;
            }
        }
    }

    let token_ids: Vec<String> = markets
        .iter()
        .flat_map(|m| m.tokens.iter().map(|t| t.token_id.clone()))
        .collect();
    let orderbooks = client
        .fetch_orderbooks(&token_ids, cfg.fetch_concurrency)
        .await?;

    let mut opportunities: Vec<ArbOpportunity> = markets
        .iter()
        .filter_map(|market| evaluate_market(market, &orderbooks, cfg))
        .filter(|opp| opp.direction != Direction::None)
        .collect();
    opportunities.sort_by_key(|opp| std::cmp::Reverse(opp.rank_key()));
    Ok(opportunities)
}

/// Report row; fields are declared alphabetically so JSON output is key-sorted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SummaryRow {
    pub condition_id: String,
    pub direction: Direction,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub executable_pairs: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub max_dollar_profit: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub net_margin_per_pair: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub no_ask: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub no_bid: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub yes_ask: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub yes_bid: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanSummary {
    pub opportunities_found: usize,
    pub top_rows: Vec<SummaryRow>,
}

pub fn summarize(opportunities: &[ArbOpportunity], max_rows: usize) -> ScanSummary {
    let top_rows = opportunities
        .iter()
        .take(max_rows)
        .map(|opp| {
            let margin = opp.net_margin();
            SummaryRow {
                condition_id: opp.condition_id.clone(),
                direction: opp.direction,
                executable_pairs: opp.max_executable_pairs.normalize(),
                max_dollar_profit: (margin * opp.max_executable_pairs).round_dp(4).normalize(),
                net_margin_per_pair: margin.round_dp(6).normalize(),
                no_ask: opp.best_ask_no.normalize(),
                no_bid: opp.best_bid_no.normalize(),
                yes_ask: opp.best_ask_yes.normalize(),
                yes_bid: opp.best_bid_yes.normalize(),
            }
        })
        .collect();
    ScanSummary {
        opportunities_found: opportunities.len(),
        top_rows,
    }
}

/// Human-readable report, one line per row.
pub fn render_text(summary: &ScanSummary) -> String {
    let mut out = format!(
        "Found {} opportunities. Top {}:\n",
        summary.opportunities_found,
        summary.top_rows.len()
    );
    for row in &summary.top_rows {
        out.push_str(&format!(
            "  {:<10}  margin/pair=${:+.4}  size={:>9.2}  total=${:+.4}  yes={:.3}/{:.3}  no={:.3}/{:.3}\n",
            row.direction.as_str(),
            row.net_margin_per_pair.round_dp(4),
            row.executable_pairs.round_dp(2),
            row.max_dollar_profit.round_dp(4),
            row.yes_ask.round_dp(3),
            row.yes_bid.round_dp(3),
            row.no_ask.round_dp(3),
            row.no_bid.round_dp(3),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{MarketToken, OrderbookLevel};
    use pretty_assertions::assert_eq;
    use rust_decimal_macros::dec;

    fn market() -> SimplifiedMarket {
        SimplifiedMarket {
            condition_id: "0xabc".into(),
            tokens: vec![
                MarketToken {
                    token_id: "YES".into(),
                    outcome: "Yes".into(),
                    last_price: dec!(0.5),
                },
                MarketToken {
                    token_id: "NO".into(),
                    outcome: "No".into(),
                    last_price: dec!(0.5),
                },
            ],
            accepting_orders: true,
            closed: false,
        }
    }

    fn book(bid: Decimal, ask: Decimal, bid_size: Decimal, ask_size: Decimal) -> Orderbook {
        let level = |price: Decimal, size| {
            (price > Decimal::ZERO)
                .then_some(OrderbookLevel { price, size })
                .into_iter()
                .collect()
        };
        Orderbook {
            market: "x".into(),
            asset_id: "y".into(),
            timestamp_ms: 0,
            bids: level(bid, bid_size),
            asks: level(ask, ask_size),
        }
    }

    fn books(yes: Orderbook, no: Orderbook) -> HashMap<String, Orderbook> {
        HashMap::from([("YES".to_owned(), yes), ("NO".to_owned(), no)])
    }

    fn cfg(min_size: Decimal, min_gross: Decimal) -> ScanConfig {
        ScanConfig {
            min_size_per_leg: min_size,
            min_gross_profit: min_gross,
            ..ScanConfig::default()
        }
    }

    #[test]
    fn round_trip_fee_doubles_per_pair() {
        assert_eq!(round_trip_fee_per_pair(dec!(10)), dec!(0.002));
    }

    #[test]
    fn evaluate_returns_none_for_non_binary_market() {
        let m = SimplifiedMarket {
            tokens: vec![],
            ..market()
        };
        assert_eq!(
            evaluate_market(&m, &HashMap::new(), &ScanConfig::default()),
            None
        );
    }

    #[test]
    fn evaluate_returns_none_when_orderbook_missing() {
        let only_yes = HashMap::from([(
            "YES".to_owned(),
            book(dec!(0.4), dec!(0.5), dec!(100), dec!(100)),
        )]);
        assert_eq!(
            evaluate_market(&market(), &only_yes, &ScanConfig::default()),
            None
        );
    }

    #[test]
    fn evaluate_finds_buy_both_opportunity() {
        let books = books(
            book(dec!(0.30), dec!(0.40), dec!(100), dec!(50)),
            book(dec!(0.40), dec!(0.50), dec!(80), dec!(60)),
        );
        let opp = evaluate_market(&market(), &books, &cfg(dec!(5), dec!(0.005))).unwrap();
        assert_eq!(opp.direction, Direction::BuyBoth);
        assert_eq!(opp.buy_both_gross_profit, dec!(0.10));
        assert_eq!(opp.buy_both_net_profit, dec!(0.098));
        assert_eq!(opp.max_executable_pairs, dec!(50));
    }

    #[test]
    fn evaluate_finds_sell_both_opportunity() {
        let books = books(
            book(dec!(0.55), dec!(0.65), dec!(40), dec!(10)),
            book(dec!(0.50), dec!(0.60), dec!(30), dec!(10)),
        );
        let opp = evaluate_market(&market(), &books, &cfg(dec!(5), dec!(0.005))).unwrap();
        assert_eq!(opp.direction, Direction::SellBoth);
        assert_eq!(opp.sell_both_gross_profit, dec!(0.05));
        assert_eq!(opp.max_executable_pairs, dec!(30));
    }

    #[test]
    fn evaluate_classifies_no_opportunity_when_book_in_line() {
        let books = books(
            book(dec!(0.45), dec!(0.55), dec!(100), dec!(100)),
            book(dec!(0.45), dec!(0.55), dec!(100), dec!(100)),
        );
        let opp = evaluate_market(&market(), &books, &ScanConfig::default()).unwrap();
        assert_eq!(opp.direction, Direction::None);
    }

    #[test]
    fn evaluate_rejects_when_size_below_minimum() {
        let books = books(
            book(dec!(0.30), dec!(0.40), dec!(100), dec!(2)),
            book(dec!(0.40), dec!(0.50), dec!(100), dec!(3)),
        );
        let opp = evaluate_market(&market(), &books, &cfg(dec!(10), dec!(0.005))).unwrap();
        assert_eq!(opp.direction, Direction::None);
    }

    #[test]
    fn evaluate_rejects_when_net_below_threshold() {
        let books = books(
            book(dec!(0.40), dec!(0.495), dec!(100), dec!(100)),
            book(dec!(0.40), dec!(0.500), dec!(100), dec!(100)),
        );
        let opp = evaluate_market(&market(), &books, &cfg(dec!(5), dec!(0.01))).unwrap();
        assert_eq!(opp.direction, Direction::None);
    }

    #[test]
    fn summary_reports_rounded_rows_as_numbers_and_text() {
        let books = books(
            book(dec!(0.30), dec!(0.40), dec!(100), dec!(50)),
            book(dec!(0.40), dec!(0.50), dec!(80), dec!(60)),
        );
        let opp = evaluate_market(&market(), &books, &cfg(dec!(5), dec!(0.005))).unwrap();
        let summary = summarize(&[opp], 20);
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(
            json,
            r#"{"opportunities_found":1,"top_rows":[{"condition_id":"0xabc","direction":"buy_both","executable_pairs":50,"max_dollar_profit":4.9,"net_margin_per_pair":0.098,"no_ask":0.5,"no_bid":0.4,"yes_ask":0.4,"yes_bid":0.3}]}"#
        );
        assert_eq!(
            render_text(&summary),
            "Found 1 opportunities. Top 1:\n  buy_both    margin/pair=$+0.0980  size=    50.00  total=$+4.9000  yes=0.400/0.300  no=0.500/0.400\n"
        );
    }
}

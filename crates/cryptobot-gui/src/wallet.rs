//! Detailed, read-only wallet view from the Binance API keys: spot balances (free and
//! locked) valued in USDT with 24h change, Binance wallet totals (Spot, Funding, Earn, …),
//! account flags and fees, and what the API key itself is allowed to do.

use std::collections::HashMap;

use bb_binance::BinanceClient;
use bb_binance::spot::{AccountInfo, ApiRestrictions, Balance};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::history::Mark;

/// One spot balance, valued in USDT when a price is known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Holding {
    pub asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub free: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub locked: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub total: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub price_usdt: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub change_24h_pct: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    pub value_usdt: Option<Decimal>,
    /// Simple Earn flexible position (`LD<asset>`), valued at the underlying asset.
    pub earn: bool,
}

/// One Binance wallet total (`/sapi/v1/asset/wallet/balance`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalletTotal {
    pub name: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub balance_usdt: Decimal,
}

/// Account flags and fees from `/api/v3/account`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountSummary {
    pub account_type: String,
    pub can_trade: bool,
    pub can_withdraw: bool,
    pub can_deposit: bool,
    pub permissions: Vec<String>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub maker_fee_pct: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub taker_fee_pct: Decimal,
}

/// Everything the wallet tab shows. Optional sections carry their own error.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WalletView {
    pub fetched_at: String,
    /// Set when the wallet could not be read at all (no credentials, account read failed).
    pub error: Option<String>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub total_usdt: Decimal,
    /// Value change over 24h at today's quantities.
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub change_24h_usdt: Decimal,
    /// Largest first.
    pub holdings: Vec<Holding>,
    pub prices_error: Option<String>,
    pub wallets: Vec<WalletTotal>,
    pub wallets_error: Option<String>,
    pub account: Option<AccountSummary>,
    pub api_key: Option<ApiRestrictions>,
    pub api_key_error: Option<String>,
}

impl WalletView {
    fn failed(error: String) -> Self {
        Self {
            fetched_at: now_text(),
            error: Some(error),
            ..Self::default()
        }
    }

    /// Current holding of `asset` for marking open trades.
    pub fn mark(&self, asset: &str) -> Option<Mark> {
        let holding = self.holdings.iter().find(|h| h.asset == asset)?;
        Some(Mark {
            free: holding.free,
            price_usdt: holding.price_usdt?,
        })
    }
}

fn now_text() -> String {
    bb_core::time::now().to_rfc3339()
}

/// `symbol → (last price, 24h change %)`.
pub type Tickers = HashMap<String, (Decimal, Option<Decimal>)>;

/// USDT price and 24h change of `asset`. `LD<asset>` (Simple Earn) uses the underlying asset,
/// but only when `LD<asset>` itself has no market (so `LDO` stays `LDO`).
fn price_of(asset: &str, tickers: &Tickers) -> Option<(Decimal, Option<Decimal>, bool)> {
    let direct = |name: &str| -> Option<(Decimal, Option<Decimal>)> {
        if name == "USDT" {
            return Some((Decimal::ONE, Some(Decimal::ZERO)));
        }
        tickers
            .get(&format!("{name}USDT"))
            .copied()
            .filter(|(price, _)| *price > Decimal::ZERO)
    };
    if let Some((price, change)) = direct(asset) {
        return Some((price, change, false));
    }
    let underlying = asset.strip_prefix("LD").filter(|rest| !rest.is_empty())?;
    direct(underlying).map(|(price, change)| (price, change, true))
}

/// Non-zero balances valued in USDT, largest first; unpriced balances go last.
pub fn value_balances(balances: &[Balance], tickers: &Tickers) -> Vec<Holding> {
    let mut holdings: Vec<Holding> = balances
        .iter()
        .filter(|b| b.total() > Decimal::ZERO)
        .map(|b| {
            let priced = price_of(&b.asset, tickers);
            Holding {
                asset: b.asset.clone(),
                free: b.free,
                locked: b.locked,
                total: b.total(),
                price_usdt: priced.map(|(price, _, _)| price),
                change_24h_pct: priced.and_then(|(_, change, _)| change),
                value_usdt: priced.map(|(price, _, _)| price * b.total()),
                earn: priced.is_some_and(|(_, _, earn)| earn),
            }
        })
        .collect();
    holdings.sort_by(|a, b| {
        b.value_usdt
            .unwrap_or(Decimal::MIN)
            .cmp(&a.value_usdt.unwrap_or(Decimal::MIN))
            .then_with(|| a.asset.cmp(&b.asset))
    });
    holdings
}

/// What the holdings were worth 24h ago at the same quantities, subtracted from today.
pub fn change_24h(holdings: &[Holding]) -> Decimal {
    holdings
        .iter()
        .filter_map(|h| {
            let value = h.value_usdt?;
            let pct = h.change_24h_pct?;
            let before = Decimal::ONE_HUNDRED + pct;
            (before > Decimal::ZERO).then(|| value - value * Decimal::ONE_HUNDRED / before)
        })
        .sum()
}

fn account_summary(account: &AccountInfo) -> AccountSummary {
    // `commissionRates` are fractions (0.001); the legacy integer fields are in 1/100 bp.
    let (maker, taker) = match &account.commission_rates {
        Some(rates) => (rates.maker, rates.taker),
        None => (
            Decimal::new(account.maker_commission, 4),
            Decimal::new(account.taker_commission, 4),
        ),
    };
    AccountSummary {
        account_type: account.account_type.clone(),
        can_trade: account.can_trade,
        can_withdraw: account.can_withdraw,
        can_deposit: account.can_deposit,
        permissions: account.permissions.clone(),
        maker_fee_pct: maker * Decimal::ONE_HUNDRED,
        taker_fee_pct: taker * Decimal::ONE_HUNDRED,
    }
}

/// Read the wallet through signed GET endpoints only.
pub async fn fetch(client: Option<&BinanceClient>) -> WalletView {
    let Some(client) = client.filter(|c| c.is_authenticated()) else {
        return WalletView::failed(
            "no Binance credentials (BINANCE_API_KEY / BINANCE_API_SECRET) in the environment or .env"
                .into(),
        );
    };
    let (account, tickers, wallets, api_key) = tokio::join!(
        client.account(),
        client.tickers_24h(),
        client.wallet_balances("USDT"),
        client.api_restrictions(),
    );
    let account = match account {
        Ok(account) => account,
        Err(error) => return WalletView::failed(format!("spot account read failed: {error}")),
    };
    let (tickers, prices_error) = match tickers {
        Ok(rows) => (
            rows.into_iter()
                .map(|t| (t.symbol, (t.last_price, Some(t.price_change_percent))))
                .collect(),
            None,
        ),
        Err(error) => match client.prices().await {
            Ok(rows) => (
                rows.into_iter().map(|p| (p.symbol, (p.price, None))).collect(),
                Some(format!("24h change unavailable: {error}")),
            ),
            Err(error) => (Tickers::new(), Some(format!("prices unavailable: {error}"))),
        },
    };
    let holdings = value_balances(&account.balances, &tickers);
    let (wallets, wallets_error) = match wallets {
        Ok(rows) => {
            let mut rows: Vec<WalletTotal> = rows
                .into_iter()
                .filter(|w| w.balance > Decimal::ZERO)
                .map(|w| WalletTotal {
                    name: w.wallet_name,
                    balance_usdt: w.balance,
                })
                .collect();
            rows.sort_by_key(|w| std::cmp::Reverse(w.balance_usdt));
            (rows, None)
        }
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let (api_key, api_key_error) = match api_key {
        Ok(restrictions) => (Some(restrictions), None),
        Err(error) => (None, Some(error.to_string())),
    };
    WalletView {
        fetched_at: now_text(),
        error: None,
        total_usdt: holdings.iter().filter_map(|h| h.value_usdt).sum(),
        change_24h_usdt: change_24h(&holdings),
        holdings,
        prices_error,
        wallets,
        wallets_error,
        account: Some(account_summary(&account)),
        api_key,
        api_key_error,
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn balance(asset: &str, free: Decimal, locked: Decimal) -> Balance {
        Balance {
            asset: asset.into(),
            free,
            locked,
        }
    }

    #[test]
    fn values_spot_earn_and_unpriced_balances() {
        let tickers: Tickers = [
            ("BTCUSDT", dec!(60000), dec!(2)),
            ("LDOUSDT", dec!(2), dec!(-5)),
            ("ETHUSDT", dec!(2500), dec!(0)),
        ]
        .into_iter()
        .map(|(s, p, c)| (s.to_owned(), (p, Some(c))))
        .collect();
        let balances = [
            balance("USDT", dec!(10), dec!(5)),
            balance("BTC", dec!(0.001), Decimal::ZERO),
            balance("LDO", dec!(3), Decimal::ZERO),
            balance("LDETH", dec!(0.01), Decimal::ZERO),
            balance("NOPE", dec!(7), Decimal::ZERO),
            balance("BNB", Decimal::ZERO, Decimal::ZERO),
        ];

        let holdings = value_balances(&balances, &tickers);

        let assets: Vec<&str> = holdings.iter().map(|h| h.asset.as_str()).collect();
        assert_eq!(assets, ["BTC", "LDETH", "USDT", "LDO", "NOPE"]);
        assert_eq!(holdings[0].value_usdt, Some(dec!(60)));
        // Earn position valued at the underlying; a real LD-prefixed token stays itself.
        assert!(holdings[1].earn);
        assert_eq!(holdings[1].value_usdt, Some(dec!(25)));
        assert!(!holdings[3].earn);
        assert_eq!(holdings[3].value_usdt, Some(dec!(6)));
        // Locked balances count toward the value.
        assert_eq!(holdings[2].value_usdt, Some(dec!(15)));
        assert_eq!(holdings[4].value_usdt, None);
    }

    #[test]
    fn change_24h_compares_against_yesterdays_price() {
        let holding = |value: Decimal, pct: Decimal| Holding {
            asset: String::new(),
            free: Decimal::ONE,
            locked: Decimal::ZERO,
            total: Decimal::ONE,
            price_usdt: Some(value),
            change_24h_pct: Some(pct),
            value_usdt: Some(value),
            earn: false,
        };
        // 110 after +10% was 100; 90 after -10% was 100.
        assert_eq!(
            change_24h(&[holding(dec!(110), dec!(10)), holding(dec!(90), dec!(-10))]).round_dp(6),
            Decimal::ZERO
        );
        assert_eq!(change_24h(&[holding(dec!(110), dec!(10))]).round_dp(6), dec!(10));
    }
}

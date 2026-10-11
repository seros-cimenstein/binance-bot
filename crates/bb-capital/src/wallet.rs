//! Best-effort Binance wallet snapshot (spot balances valued in USDT + USD-M free USDT), port of
//! `bot_status._fetch_wallet`.

use std::collections::BTreeMap;

use bb_binance::BinanceClient;
use rust_decimal::Decimal;
use serde::Serialize;

/// Spot rows worth less than this are dropped.
const MIN_POSITION_VALUE: Decimal = Decimal::from_parts(1, 0, 0, false, 3);

/// One valued spot balance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalletPosition {
    pub asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub value_usdt: Decimal,
    pub venue: String,
}

/// Wallet summary used by the fleet status, the distributor treasury and the capital pool equity.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WalletSnapshot {
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub spot_usdt_free: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub futures_usdt_free: Decimal,
    pub positions: Vec<WalletPosition>,
    /// Free spot USDT + free futures USDT + valued non-USDT spot holdings (locked USDT excluded).
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub total_value_usdt: Decimal,
    /// Last spot price by exchange symbol id (`BTCUSDT`).
    #[serde(skip)]
    pub last_prices: BTreeMap<String, Decimal>,
}

impl WalletSnapshot {
    /// Last price of a unified (`BTC/USDT`) or exchange (`BTCUSDT`) symbol.
    pub fn last_price(&self, symbol: &str) -> Option<Decimal> {
        let id = bb_binance::exchange_id(symbol).ok()?;
        self.last_prices.get(&id).copied()
    }

    /// Build from raw balances `(asset, total, free)` and prices; values `<ASSET>USDT`.
    pub fn from_parts(
        spot: &[(String, Decimal, Decimal)],
        last_prices: BTreeMap<String, Decimal>,
        futures_usdt_free: Decimal,
    ) -> Self {
        let mut snapshot = Self {
            futures_usdt_free,
            last_prices,
            ..Self::default()
        };
        for (asset, total, free) in spot {
            if asset == "USDT" && *free > Decimal::ZERO {
                snapshot.spot_usdt_free = *free;
            }
            if *total <= Decimal::ZERO {
                continue;
            }
            let value = if asset == "USDT" {
                *total
            } else {
                *total
                    * snapshot
                        .last_prices
                        .get(&format!("{asset}USDT"))
                        .copied()
                        .unwrap_or_default()
            };
            if value < MIN_POSITION_VALUE {
                continue;
            }
            snapshot.positions.push(WalletPosition {
                asset: asset.clone(),
                qty: *total,
                value_usdt: value,
                venue: "spot".into(),
            });
        }
        snapshot.total_value_usdt = snapshot.spot_usdt_free
            + snapshot.futures_usdt_free
            + snapshot
                .positions
                .iter()
                .filter(|p| p.asset != "USDT")
                .map(|p| p.value_usdt)
                .sum::<Decimal>();
        snapshot
    }
}

/// Read-only wallet snapshot; every failure degrades to zeros (logged on stderr).
pub async fn fetch_wallet(client: &BinanceClient) -> WalletSnapshot {
    if !client.is_authenticated() {
        tracing::warn!("no Binance credentials; wallet snapshot is empty");
        return WalletSnapshot::default();
    }
    let account = match client.account().await {
        Ok(account) => account,
        Err(error) => {
            tracing::warn!(%error, "spot account read failed; wallet snapshot is empty");
            return WalletSnapshot::default();
        }
    };
    let prices = match client.prices().await {
        Ok(prices) => prices.into_iter().map(|p| (p.symbol, p.price)).collect(),
        Err(error) => {
            tracing::warn!(%error, "spot price read failed; non-USDT holdings valued at 0");
            BTreeMap::new()
        }
    };
    let futures_usdt_free = match client.futures_balances().await {
        Ok(rows) => rows
            .iter()
            .find(|row| row.asset == "USDT")
            .map(|row| row.available_balance)
            .unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, "USD-M balance read failed");
            Decimal::ZERO
        }
    };
    let spot: Vec<(String, Decimal, Decimal)> = account
        .balances
        .iter()
        .map(|b| (b.asset.clone(), b.total(), b.free))
        .collect();
    WalletSnapshot::from_parts(&spot, prices, futures_usdt_free)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn wallet_values_spot_holdings_and_adds_futures_free_usdt() {
        let prices = BTreeMap::from([
            ("BNBUSDT".to_owned(), dec!(600)),
            ("DUSTUSDT".to_owned(), dec!(0.0001)),
        ]);
        let spot = vec![
            ("USDT".to_owned(), dec!(12), dec!(10)),
            ("BNB".to_owned(), dec!(0.5), dec!(0.5)),
            ("DUST".to_owned(), dec!(1), dec!(1)),
            ("NOPRICE".to_owned(), dec!(3), dec!(3)),
        ];
        let wallet = WalletSnapshot::from_parts(&spot, prices, dec!(4));
        assert_eq!(wallet.spot_usdt_free, dec!(10));
        assert_eq!(wallet.positions.len(), 2);
        // Locked USDT (2) is excluded from the total.
        assert_eq!(wallet.total_value_usdt, dec!(10) + dec!(4) + dec!(300));
        assert_eq!(wallet.last_price("BNB/USDT"), Some(dec!(600)));
    }
}

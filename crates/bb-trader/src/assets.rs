//! Asset classes the trader treats specially.

/// Stablecoins and major fiats: modelled as flat (cash-like), never bought as a bet.
pub const LOW_UPSIDE_ASSETS: [&str; 20] = [
    "AUD", "BFUSD", "BRL", "BUSD", "DAI", "EUR", "EURI", "FDUSD", "GBP", "JPY", "RLUSD", "TUSD",
    "TRY", "USDC", "USDE", "USDP", "USDS", "USD1", "USDT", "XUSD",
];

pub fn is_low_upside(asset: &str) -> bool {
    LOW_UPSIDE_ASSETS.contains(&asset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binance_usd_and_eur_stablecoins_are_cash_like_but_gold_and_coins_are_not() {
        // Stablecoins with a Binance USDT market on 2026-10-29 that /biz/ feeds name.
        for stable in ["RLUSD", "USDE", "USDS", "XUSD", "BFUSD", "EURI", "USDC", "FDUSD"] {
            assert!(is_low_upside(stable), "{stable}");
        }
        for coin in ["BTC", "SOL", "PAXG", "XAUT", "USUAL"] {
            assert!(!is_low_upside(coin), "{coin}");
        }
    }
}

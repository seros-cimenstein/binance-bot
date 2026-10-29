//! Asset classes the trader treats specially.

/// Stablecoins and major fiats: modelled as flat (cash-like), never bought as a bet.
pub const LOW_UPSIDE_ASSETS: [&str; 14] = [
    "AUD", "BRL", "BUSD", "DAI", "EUR", "FDUSD", "GBP", "JPY", "TUSD", "TRY", "USDC", "USDP",
    "USD1", "USDT",
];

pub fn is_low_upside(asset: &str) -> bool {
    LOW_UPSIDE_ASSETS.contains(&asset)
}

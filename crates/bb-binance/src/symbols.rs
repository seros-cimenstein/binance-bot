//! Symbol conversion between ccxt-style unified symbols and Binance ids.
//!
//! - Spot unified: `BTC/USDT` (a ccxt futures suffix `:USDT` is accepted and dropped).
//! - Exchange id: `BTCUSDT`.
//!
//! [`normalize_symbol`] keeps the exact semantics of `src/data/feeds.py`
//! (separator handling + known quote-asset suffix inference). When exchange
//! metadata is available prefer [`crate::markets::Markets::unified`], which
//! uses the listed base/quote instead of heuristics.

use crate::error::{BinanceError, Result};

/// Quote assets tried (in order) when splitting a compact id like `BTCUSDT`.
pub const QUOTE_ASSETS: [&str; 18] = [
    "FDUSD", "BUSD", "USDT", "USDC", "TUSD", "USDP", "BIDR", "BKRW", "BRL", "TRY", "EUR", "GBP",
    "AUD", "DAI", "BTC", "ETH", "BNB", "USD",
];

fn is_separator(c: char) -> bool {
    c.is_whitespace() || c == '_' || c == '-'
}

/// Normalise a symbol to ccxt spot format `BASE/QUOTE`.
///
/// `"BTCUSDT"`, `"btc-usdt"`, `"btc_usdt"`, `"BTC/USDT:USDT"` all become `"BTC/USDT"`.
pub fn normalize_symbol(symbol: &str) -> Result<String> {
    let raw = symbol.trim().to_uppercase();
    if raw.is_empty() {
        return Err(BinanceError::InvalidSymbol(
            "symbol must not be blank".into(),
        ));
    }
    // Collapse runs of whitespace/_/- into a single '/'.
    let mut separated = String::with_capacity(raw.len());
    let mut in_sep = false;
    for c in raw.chars() {
        if is_separator(c) {
            if !in_sep {
                separated.push('/');
            }
            in_sep = true;
        } else {
            separated.push(c);
            in_sep = false;
        }
    }

    if separated.contains('/') {
        let (base, quote) = split_parts(&separated, '/', symbol)?;
        return Ok(format!("{base}/{quote}"));
    }
    if separated.contains(':') {
        let (base, quote) = split_parts(&separated, ':', symbol)?;
        return Ok(format!("{base}/{quote}"));
    }

    let compact: String = separated
        .chars()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        .collect();
    for quote in QUOTE_ASSETS {
        if compact.len() > quote.len() && compact.ends_with(quote) {
            return Ok(format!(
                "{}/{quote}",
                &compact[..compact.len() - quote.len()]
            ));
        }
    }
    Err(BinanceError::InvalidSymbol(format!(
        "could not infer quote asset from symbol: {symbol:?}"
    )))
}

fn split_parts<'a>(symbol: &'a str, separator: char, original: &str) -> Result<(&'a str, &'a str)> {
    let parts: Vec<&str> = symbol.split(separator).filter(|p| !p.is_empty()).collect();
    let [base, quote] = parts.as_slice() else {
        return Err(BinanceError::InvalidSymbol(format!(
            "symbol must contain exactly one base and quote: {original:?}"
        )));
    };
    let quote = quote.split(':').next().unwrap_or_default();
    if base.is_empty() || quote.is_empty() {
        return Err(BinanceError::InvalidSymbol(format!(
            "symbol must contain base and quote assets: {original:?}"
        )));
    }
    Ok((base, quote))
}

/// Binance exchange id (`BTCUSDT`) for any accepted spelling
/// (`BTC/USDT`, `BTC/USDT:USDT`, `btc-usdt`, `BTCUSDT`). Ports
/// `HistoryFetcher._rest_symbol`: split on `/ : whitespace _ -` and join the first two parts.
pub fn exchange_id(symbol: &str) -> Result<String> {
    let normalized = symbol.trim().to_uppercase();
    if normalized.is_empty() {
        return Err(BinanceError::InvalidSymbol(
            "symbol must not be blank".into(),
        ));
    }
    let parts: Vec<&str> = normalized
        .split(|c: char| c == '/' || c == ':' || is_separator(c))
        .filter(|p| !p.is_empty())
        .collect();
    Ok(match parts.as_slice() {
        [base, quote, ..] => format!("{base}{quote}"),
        _ => normalized,
    })
}

/// Split a unified symbol into `(base, quote)`.
pub fn split_unified(symbol: &str) -> Result<(String, String)> {
    let normalized = normalize_symbol(symbol)?;
    match normalized.split_once('/') {
        Some((base, quote)) => Ok((base.to_owned(), quote.to_owned())),
        None => Err(BinanceError::InvalidSymbol(normalized)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        normalize_symbol(s).unwrap_or_else(|e| e.to_string())
    }

    #[test]
    fn normalize_symbol_matches_python_semantics() {
        assert_eq!(norm("BTCUSDT"), "BTC/USDT");
        assert_eq!(norm("btc-usdt"), "BTC/USDT");
        assert_eq!(norm("btc_usdt"), "BTC/USDT");
        assert_eq!(norm(" eth  usdt "), "ETH/USDT");
        assert_eq!(norm("BTC/USDT:USDT"), "BTC/USDT");
        assert_eq!(norm("BTC:USDT"), "BTC/USDT");
        assert_eq!(norm("ETHBTC"), "ETH/BTC");
        assert_eq!(norm("BTCFDUSD"), "BTC/FDUSD");
        assert_eq!(norm("PAXGUSDC"), "PAXG/USDC");
        assert!(normalize_symbol("").is_err());
        assert!(normalize_symbol("USDT").is_err());
        assert!(normalize_symbol("A/B/C").is_err());
        assert!(normalize_symbol("XYZABC").is_err());
    }

    #[test]
    fn exchange_ids() -> Result<()> {
        assert_eq!(exchange_id("BTC/USDT")?, "BTCUSDT");
        assert_eq!(exchange_id("BTC/USDT:USDT")?, "BTCUSDT");
        assert_eq!(exchange_id("eth-usdt")?, "ETHUSDT");
        assert_eq!(exchange_id("BTCUSDT")?, "BTCUSDT");
        assert_eq!(split_unified("ETH/BTC")?, ("ETH".into(), "BTC".into()));
        Ok(())
    }
}

//! Operator scope (hard safety): allow/block lists, blocked quotes and fiat blocking.
//! Applied to the market universe, to every quoted route, and again before every execution.

use bb_pool::{MarketSnapshot, RouteLeg, RouteQuote};
use serde::Serialize;

use crate::error::{Result, TraderError};

use bb_binance::fiat::is_fiat;

/// Operator-configured trading scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScopeConfig {
    pub allowed_symbols: Vec<String>,
    pub allowed_assets: Vec<String>,
    pub blocked_symbols: Vec<String>,
    pub blocked_assets: Vec<String>,
    pub blocked_quotes: Vec<String>,
    pub block_fiat_assets: bool,
}

impl ScopeConfig {
    /// Normalises and validates raw list entries (Python `_normalize_*_list`).
    pub fn new(
        allowed_symbols: &[String],
        allowed_assets: &[String],
        blocked_symbols: &[String],
        blocked_assets: &[String],
        blocked_quotes: &[String],
        block_fiat_assets: bool,
    ) -> Result<Self> {
        Ok(Self {
            allowed_symbols: symbol_list(allowed_symbols, "allowed_symbols")?,
            allowed_assets: asset_list(allowed_assets, "allowed_assets")?,
            blocked_symbols: symbol_list(blocked_symbols, "blocked_symbols")?,
            blocked_assets: asset_list(blocked_assets, "blocked_assets")?,
            blocked_quotes: asset_list(blocked_quotes, "blocked_quotes")?,
            block_fiat_assets,
        })
    }

    /// Why `asset` is out of scope, if it is.
    pub fn asset_block_reason(&self, asset: &str) -> Option<String> {
        let normalized = asset.trim().to_uppercase();
        if normalized.is_empty() {
            return Some("asset is blank".into());
        }
        if self.blocked_assets.contains(&normalized) {
            return Some(format!(
                "asset {normalized} is in Bot Spot local blocklist"
            ));
        }
        if self.block_fiat_assets && is_fiat(&normalized) {
            return Some(format!("asset {normalized} is blocked as fiat"));
        }
        if !self.allowed_assets.is_empty() && !self.allowed_assets.contains(&normalized) {
            return Some(format!(
                "asset {normalized} is outside Bot Spot local allowlist"
            ));
        }
        None
    }

    fn symbol_block_reason(&self, symbol: &str) -> Option<String> {
        if symbol_matches(&self.blocked_symbols, symbol) {
            return Some("symbol is in Bot Spot local blocklist".into());
        }
        if !self.allowed_symbols.is_empty() && !symbol_matches(&self.allowed_symbols, symbol) {
            return Some("symbol is outside Bot Spot local allowlist".into());
        }
        None
    }

    /// Why a catalog market is out of scope.
    pub fn market_block_reason(&self, market: &MarketSnapshot) -> Option<String> {
        let symbol = normalize_symbol(&market.symbol);
        if let Some(reason) = self.symbol_block_reason(&symbol) {
            return Some(reason);
        }
        let quote = market.quote.trim().to_uppercase();
        if self.blocked_quotes.contains(&quote) {
            return Some(format!(
                "quote asset {quote} is in Bot Spot local blocklist"
            ));
        }
        [&market.base, &market.quote]
            .into_iter()
            .find_map(|asset| self.asset_block_reason(asset))
    }

    /// Why one route leg is out of scope.
    pub fn leg_block_reason(&self, leg: &RouteLeg) -> Option<String> {
        let symbol = normalize_symbol(&leg.symbol);
        if let Some(reason) = self.symbol_block_reason(&symbol) {
            return Some(reason);
        }
        let (base, quote) = split_symbol(&symbol);
        if !quote.is_empty() && self.blocked_quotes.iter().any(|blocked| *blocked == quote) {
            return Some(format!(
                "quote asset {quote} is in Bot Spot local blocklist"
            ));
        }
        [
            leg.source_asset.as_str(),
            leg.destination_asset.as_str(),
            base,
            quote,
        ]
        .into_iter()
        .filter(|asset| !asset.is_empty())
        .find_map(|asset| self.asset_block_reason(asset))
    }

    /// Why a route is out of scope (source, destination, then every leg).
    pub fn route_block_reason(&self, route: &RouteQuote) -> Option<String> {
        if let Some(reason) = self.asset_block_reason(&route.source_asset) {
            return Some(format!(
                "selected route source {} blocked: {reason}",
                route.source_asset
            ));
        }
        if let Some(reason) = self.asset_block_reason(&route.destination_asset) {
            return Some(format!(
                "selected route destination {} blocked: {reason}",
                route.destination_asset
            ));
        }
        route.legs.iter().find_map(|leg| {
            self.leg_block_reason(leg)
                .map(|reason| format!("selected route leg {} blocked: {reason}", leg.symbol))
        })
    }

    /// Fails closed when the route is out of scope.
    pub fn assert_route_allowed(&self, route: &RouteQuote) -> Result<()> {
        match self.route_block_reason(route) {
            Some(reason) => Err(TraderError::safety(reason)),
            None => Ok(()),
        }
    }
}

/// `" btc/usdt:usdt "` → `BTC/USDT`; compact symbols stay compact.
pub fn normalize_symbol(value: &str) -> String {
    let symbol: String = value.trim().to_uppercase().replace(' ', "");
    match symbol.split_once('/') {
        None => symbol,
        Some((base, quote)) => {
            let quote = quote.split_once(':').map_or(quote, |(quote, _)| quote);
            format!("{base}/{quote}")
        }
    }
}

fn symbol_matches(configured: &[String], symbol: &str) -> bool {
    if configured.is_empty() {
        return false;
    }
    let normalized = normalize_symbol(symbol);
    let compact = normalized.replace('/', "");
    configured
        .iter()
        .any(|entry| *entry == normalized || *entry == compact)
}

fn split_symbol(symbol: &str) -> (&str, &str) {
    match symbol.split_once('/') {
        None => ("", ""),
        Some((base, quote)) => (
            base,
            quote.split_once(':').map_or(quote, |(quote, _)| quote),
        ),
    }
}

fn symbol_list(values: &[String], field: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        let symbol = normalize_symbol(value);
        if symbol.is_empty() {
            return Err(TraderError::config(format!(
                "{field} contains a blank symbol"
            )));
        }
        if !symbol
            .chars()
            .filter(|c| !matches!(c, '/' | ':' | '-'))
            .all(char::is_alphanumeric)
        {
            return Err(TraderError::config(format!(
                "{field} contains an invalid symbol"
            )));
        }
        if !out.contains(&symbol) {
            out.push(symbol);
        }
    }
    Ok(out)
}

fn asset_list(values: &[String], field: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        let asset = value.trim().to_uppercase();
        if asset.is_empty() {
            return Err(TraderError::config(format!(
                "{field} contains a blank asset"
            )));
        }
        if !asset
            .chars()
            .filter(|c| !matches!(c, '-' | '_'))
            .all(char::is_alphanumeric)
        {
            return Err(TraderError::config(format!(
                "{field} contains an invalid asset"
            )));
        }
        if !out.contains(&asset) {
            out.push(asset);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use bb_pool::LegSide;
    use rust_decimal::Decimal;

    use super::*;

    pub(crate) fn leg(symbol: &str, side: LegSide, source: &str, destination: &str) -> RouteLeg {
        RouteLeg {
            symbol: symbol.into(),
            side,
            source_asset: source.into(),
            destination_asset: destination.into(),
            input_amount: Decimal::ONE,
            spent_amount: Decimal::ONE,
            gross_output_amount: Decimal::ONE,
            fee_rate: Decimal::ZERO,
            fee_output_amount: Decimal::ZERO,
            net_output_amount: Decimal::ONE,
            commission_discount_asset: String::new(),
        }
    }

    pub(crate) fn route(source: &str, destination: &str, legs: Vec<RouteLeg>) -> RouteQuote {
        RouteQuote {
            source_asset: source.into(),
            destination_asset: destination.into(),
            input_amount: Decimal::ONE,
            output_amount: Decimal::ONE,
            legs,
            slippage_bps: Decimal::from(25),
            generated_at: bb_core::time::now(),
        }
    }

    fn scope(blocked_quotes: &[&str], fiat: bool) -> ScopeConfig {
        let quotes: Vec<String> = blocked_quotes.iter().map(|q| (*q).to_owned()).collect();
        ScopeConfig::new(&[], &[], &[], &[], &quotes, fiat).unwrap()
    }

    #[test]
    fn blocked_quote_rejects_route_before_submission() {
        let route = route(
            "POL",
            "SUI",
            vec![
                leg("POL/IDR", LegSide::Sell, "POL", "IDR"),
                leg("SUI/IDR", LegSide::Buy, "IDR", "SUI"),
            ],
        );
        let reason = scope(&["IDR"], false).route_block_reason(&route).unwrap();
        assert!(
            reason.contains("quote asset IDR is in Bot Spot local blocklist"),
            "{reason}"
        );
        assert!(scope(&["IDR"], false).assert_route_allowed(&route).is_err());
    }

    #[test]
    fn default_fiat_blocking_rejects_fiat_legs() {
        let route = route(
            "USDT",
            "BTC",
            vec![
                leg("USDT/TRY", LegSide::Sell, "USDT", "TRY"),
                leg("BTC/TRY", LegSide::Buy, "TRY", "BTC"),
            ],
        );
        let reason = scope(&[], true).route_block_reason(&route).unwrap();
        assert!(reason.contains("asset TRY is blocked as fiat"), "{reason}");
        assert!(scope(&[], false).route_block_reason(&route).is_none());
    }

    #[test]
    fn allowlists_match_unified_and_compact_symbols() {
        let scope = ScopeConfig::new(&["BTCUSDT".into()], &[], &[], &[], &[], true).unwrap();
        let ok = route(
            "USDT",
            "BTC",
            vec![leg("BTC/USDT", LegSide::Buy, "USDT", "BTC")],
        );
        let bad = route(
            "USDT",
            "ETH",
            vec![leg("ETH/USDT", LegSide::Buy, "USDT", "ETH")],
        );
        assert!(scope.route_block_reason(&ok).is_none());
        assert!(
            scope
                .route_block_reason(&bad)
                .unwrap()
                .contains("outside Bot Spot local allowlist")
        );
        let assets =
            ScopeConfig::new(&[], &["btc".into(), "usdt".into()], &[], &[], &[], true).unwrap();
        assert!(assets.route_block_reason(&ok).is_none());
        assert!(assets.asset_block_reason("ETH").is_some());
    }

    #[test]
    fn symbol_normalisation() {
        assert_eq!(normalize_symbol(" btc/usdt:usdt "), "BTC/USDT");
        assert_eq!(normalize_symbol("ethusdt"), "ETHUSDT");
    }
}

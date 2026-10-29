//! Spot `exchangeInfo` cache and unified-symbol lookup.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{BinanceError, Result};
use crate::rest::BinanceClient;
use crate::rules::{MarketKind, SymbolRules};
use crate::spot::SpotSymbolInfo;
use crate::symbols::{exchange_id, normalize_symbol};

/// Typed rules for every listed symbol of one market, indexed by exchange id
/// and unified symbol (ccxt `load_markets` replacement).
#[derive(Debug, Clone, Default)]
pub struct Markets {
    by_id: HashMap<String, SymbolRules>,
    by_unified: HashMap<String, String>,
}

impl Markets {
    /// Index a list of rules.
    pub fn from_rules(rules: impl IntoIterator<Item = SymbolRules>) -> Self {
        let mut markets = Self::default();
        for rule in rules {
            markets
                .by_unified
                .insert(rule.unified.clone(), rule.symbol.clone());
            markets.by_id.insert(rule.symbol.clone(), rule);
        }
        markets
    }

    /// Look up by unified symbol (`BTC/USDT`, `BTC/USDT:USDT`) or exchange id (`BTCUSDT`).
    pub fn get(&self, symbol: &str) -> Option<&SymbolRules> {
        let upper = symbol.trim().to_uppercase();
        if let Some(id) = self.by_unified.get(&upper) {
            return self.by_id.get(id);
        }
        if let Some(rules) = self.by_id.get(&upper) {
            return Some(rules);
        }
        if let Ok(id) = exchange_id(&upper)
            && let Some(rules) = self.by_id.get(&id)
        {
            return Some(rules);
        }
        normalize_symbol(&upper)
            .ok()
            .and_then(|unified| self.by_unified.get(&unified))
            .and_then(|id| self.by_id.get(id))
    }

    /// Like [`Self::get`] but an [`BinanceError::InvalidSymbol`] when unlisted.
    pub fn require(&self, symbol: &str) -> Result<&SymbolRules> {
        self.get(symbol)
            .ok_or_else(|| BinanceError::InvalidSymbol(format!("{symbol} is not listed")))
    }

    /// Unified symbol for an exchange id.
    pub fn unified(&self, id: &str) -> Option<&str> {
        self.by_id
            .get(&id.to_uppercase())
            .map(|r| r.unified.as_str())
    }

    /// All rules (unordered).
    pub fn iter(&self) -> impl Iterator<Item = &SymbolRules> {
        self.by_id.values()
    }

    /// Number of listed symbols.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether no symbols are listed.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Exchange ids of `TRADING` symbols quoted in `quote`, sorted.
    pub fn trading_symbols_for_quote(&self, quote: &str) -> Vec<String> {
        let quote = quote.to_uppercase();
        let mut out: Vec<String> = self
            .by_id
            .values()
            .filter(|r| r.is_trading() && r.quote_asset == quote)
            .map(|r| r.symbol.clone())
            .collect();
        out.sort();
        out
    }
}

impl SymbolRules {
    /// Rules for a spot symbol.
    pub fn from_spot(info: &SpotSymbolInfo) -> Self {
        let mut permissions: Vec<String> = info.permissions.clone();
        for set in &info.permission_sets {
            for p in set {
                if !permissions.contains(p) {
                    permissions.push(p.clone());
                }
            }
        }
        let mut rules = Self {
            market: MarketKind::Spot,
            symbol: info.symbol.clone(),
            unified: format!("{}/{}", info.base_asset, info.quote_asset),
            status: info.status.clone(),
            base_asset: info.base_asset.clone(),
            quote_asset: info.quote_asset.clone(),
            settle_asset: None,
            contract_type: None,
            permissions,
            order_types: info.order_types.clone(),
            spot_trading_allowed: info.is_spot_trading_allowed,
            margin_trading_allowed: info.is_margin_trading_allowed,
            quote_order_qty_market_allowed: info.quote_order_qty_market_allowed,
            base_asset_precision: info.base_asset_precision,
            quote_asset_precision: info.quote_asset_precision.or(info.quote_precision),
            price: None,
            lot: None,
            market_lot: None,
            notional: None,
            percent_price: None,
            max_num_orders: None,
            filters: Vec::new(),
        };
        rules.apply_filters(info.filters.clone());
        rules
    }
}

type Slot = Mutex<Option<(Instant, Arc<Markets>)>>;

/// TTL cache of spot [`Markets`], shared by client clones.
#[derive(Debug)]
pub(crate) struct MarketsCache {
    ttl: Duration,
    spot: Slot,
}

impl MarketsCache {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            spot: Mutex::new(None),
        }
    }
}

impl BinanceClient {
    /// Spot markets from `GET /api/v3/exchangeInfo`, cached for
    /// [`crate::BinanceConfig::exchange_info_ttl`]. `reload` forces a refresh
    /// (e.g. after `-1121 Invalid symbol` on a fresh listing).
    pub async fn spot_markets(&self, reload: bool) -> Result<Arc<Markets>> {
        let mut slot = self.markets_cache().spot.lock().await;
        if !reload
            && let Some((at, markets)) = slot.as_ref()
            && at.elapsed() < self.markets_cache().ttl
        {
            return Ok(markets.clone());
        }
        let info = self.exchange_info(None).await?;
        let markets = Arc::new(Markets::from_rules(
            info.symbols.iter().map(SymbolRules::from_spot),
        ));
        *slot = Some((Instant::now(), markets.clone()));
        Ok(markets)
    }

    /// Spot rules for one symbol (any spelling), from the cache.
    pub async fn symbol_rules(&self, symbol: &str) -> Result<SymbolRules> {
        self.spot_markets(false).await?.require(symbol).cloned()
    }
}

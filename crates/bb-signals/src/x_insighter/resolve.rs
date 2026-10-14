//! Cashtag / mint -> venue resolution (design doc "Venues", rules 1-5).
//!
//! Everything here is pure: network results (Binance universe, DexScreener
//! search pairs) are fed in by the engine and cached in [`ResolverState`],
//! which is persisted inside `x-insighter-state.json`.

use std::collections::{BTreeMap, BTreeSet};

use bb_binance::spot::{ExchangeInfo, Ticker24h};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

use super::schema::{Resolution, Venue, XPost};
use crate::jsonv::{Object, float_or_zero, nested, py_str};

/// Binance universe (listings + major/stablecoin exclusions) cache lifetime.
pub const BINANCE_UNIVERSE_TTL_MS: i64 = 6 * 3_600_000;
/// Resolved DexScreener search results are cached for 6h.
pub const RESOLVED_TTL_MS: i64 = 6 * 3_600_000;
/// Unresolved DexScreener search results are cached for 30 min.
pub const UNRESOLVED_TTL_MS: i64 = 30 * 60_000;
/// Rule 3: a cashtag -> mint link from a post is valid for 24h.
pub const CASHTAG_LINK_TTL_MS: i64 = 24 * 3_600_000;
/// Rule 4: share of summed Solana liquidity one mint must hold.
pub const DOMINANT_LIQUIDITY_SHARE: f64 = 0.8;
/// Rule 4: minimum summed liquidity of the dominant mint.
pub const MIN_DOMINANT_LIQUIDITY_USD: f64 = 5_000.0;
/// Binance bases in the top N USDT pairs by 24h quote volume are majors.
pub const MAJORS_TOP_N: usize = 20;
/// Stablecoin rule: last price within 2% of 1.0 and 24h range under 2%.
pub const STABLECOIN_TOLERANCE: f64 = 0.02;

/// A resolved market for one token key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRef {
    pub token_key: String,
    pub venue: Venue,
    /// Solana mint or Binance base asset.
    pub token_address: String,
    /// `BASEUSDT` for Binance, `None` for Solana.
    pub symbol: Option<String>,
    /// Solana pair from DexScreener search (rule 4); `None` otherwise.
    pub pair_address: Option<String>,
    pub cashtag: Option<String>,
    pub resolution: Resolution,
}

impl TokenRef {
    pub fn binance(base: &str) -> Self {
        Self {
            token_key: format!("binance:{base}"),
            venue: Venue::Binance,
            token_address: base.to_owned(),
            symbol: Some(format!("{base}USDT")),
            pair_address: None,
            cashtag: Some(base.to_owned()),
            resolution: Resolution::BinanceListing,
        }
    }

    pub fn solana(
        mint: &str,
        cashtag: Option<String>,
        pair_address: Option<String>,
        resolution: Resolution,
    ) -> Self {
        Self {
            token_key: format!("solana:{mint}"),
            venue: Venue::Solana,
            token_address: mint.to_owned(),
            symbol: None,
            pair_address,
            cashtag,
            resolution,
        }
    }
}

/// Precedence of resolution rules (lower = stronger).
pub const fn resolution_rank(resolution: Resolution) -> u8 {
    match resolution {
        Resolution::MintInPost => 0,
        Resolution::BinanceListing => 1,
        Resolution::CashtagLink => 2,
        Resolution::DexscreenerSearch => 3,
    }
}

/// Binance USDT spot listings plus the rule-based exclusions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BinanceUniverse {
    pub fetched_at_ms: i64,
    /// Bases with a `TRADING` `<BASE>USDT` spot symbol.
    pub trading_usdt_bases: BTreeSet<String>,
    /// Majors (top USDT quote volume) and stablecoins.
    pub excluded_bases: BTreeSet<String>,
}

impl BinanceUniverse {
    /// Build from `exchangeInfo` and `ticker/24hr` (all symbols).
    pub fn from_market(info: &ExchangeInfo, tickers: &[Ticker24h], now_ms: i64) -> Self {
        let trading_usdt_bases: BTreeSet<String> = info
            .symbols
            .iter()
            .filter(|s| s.quote_asset == "USDT" && s.status == "TRADING")
            .map(|s| s.base_asset.to_uppercase())
            .collect();
        let base_of = |symbol: &str| -> Option<String> {
            let base = symbol.strip_suffix("USDT")?.to_uppercase();
            trading_usdt_bases.contains(&base).then_some(base)
        };
        let mut usdt: Vec<(String, f64, &Ticker24h)> = tickers
            .iter()
            .filter_map(|t| {
                let base = base_of(&t.symbol)?;
                Some((base, t.quote_volume.to_f64().unwrap_or(0.0), t))
            })
            .collect();
        usdt.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut excluded_bases: BTreeSet<String> = usdt
            .iter()
            .take(MAJORS_TOP_N)
            .map(|(base, _, _)| base.clone())
            .collect();
        for (base, _, ticker) in &usdt {
            let last = ticker.last_price.to_f64().unwrap_or(0.0);
            let high = ticker.high_price.to_f64().unwrap_or(0.0);
            let low = ticker.low_price.to_f64().unwrap_or(0.0);
            if is_stablecoin(last, Some((high, low))) {
                excluded_bases.insert(base.clone());
            }
        }
        Self {
            fetched_at_ms: now_ms,
            trading_usdt_bases,
            excluded_bases,
        }
    }

    pub fn is_fresh(&self, now_ms: i64) -> bool {
        now_ms - self.fetched_at_ms < BINANCE_UNIVERSE_TTL_MS
    }
}

/// Stablecoin rule. `range` is `(high, low)` over 24h; when only a percent
/// change proxy is available the caller converts it to a range first.
pub fn is_stablecoin(last_price: f64, range: Option<(f64, f64)>) -> bool {
    if (last_price - 1.0).abs() > STABLECOIN_TOLERANCE {
        return false;
    }
    match range {
        Some((high, low)) if low > 0.0 => high / low - 1.0 < STABLECOIN_TOLERANCE,
        _ => false,
    }
}

/// Stablecoin rule on a DexScreener pair: `priceUsd` within 2% of 1.0 and every
/// reported change (h1/h6/h24) under 2% (DexScreener has no 24h high/low).
pub fn is_stablecoin_pair(pair: &Object) -> bool {
    let price = float_or_zero(pair.get("priceUsd"));
    let changes: Vec<f64> = ["h1", "h6", "h24"]
        .iter()
        .filter_map(|k| crate::jsonv::float_or_none(nested(pair, "priceChange", k)))
        .collect();
    if changes.is_empty() {
        return false;
    }
    let max_abs = changes.iter().fold(0.0_f64, |acc, c| acc.max(c.abs())) / 100.0;
    is_stablecoin(price, Some((1.0 + max_abs, 1.0)))
}

/// Cached DexScreener search outcome for a cashtag (rule 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DexSearchEntry {
    pub checked_at_ms: i64,
    pub mint: Option<String>,
    pub pair_address: Option<String>,
}

impl DexSearchEntry {
    pub fn is_fresh(&self, now_ms: i64) -> bool {
        let ttl = if self.mint.is_some() {
            RESOLVED_TTL_MS
        } else {
            UNRESOLVED_TTL_MS
        };
        now_ms - self.checked_at_ms < ttl
    }
}

/// Rule 1 link: a cashtag seen next to a mint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashtagLink {
    pub mint: String,
    pub linked_at_ms: i64,
}

/// Rule 4 decision on DexScreener search pairs: Solana pairs whose base symbol
/// equals `tag`; one mint must hold >= 80% of their summed liquidity and at
/// least $5,000. Returns the mint and its most liquid pair.
pub fn dominant_solana_mint(tag: &str, pairs: &[Object]) -> Option<(String, String)> {
    let mut by_mint: BTreeMap<String, (f64, f64, String)> = BTreeMap::new();
    let mut total = 0.0;
    for pair in pairs {
        if !py_str(pair.get("chainId")).eq_ignore_ascii_case("solana") {
            continue;
        }
        if !py_str(nested(pair, "baseToken", "symbol")).eq_ignore_ascii_case(tag) {
            continue;
        }
        let mint = py_str(nested(pair, "baseToken", "address"));
        if mint.is_empty() {
            continue;
        }
        let liquidity = float_or_zero(nested(pair, "liquidity", "usd")).max(0.0);
        total += liquidity;
        let entry = by_mint.entry(mint).or_insert((0.0, -1.0, String::new()));
        entry.0 += liquidity;
        if liquidity > entry.1 {
            entry.1 = liquidity;
            entry.2 = py_str(pair.get("pairAddress"));
        }
    }
    if total <= 0.0 {
        return None;
    }
    let (mint, (liquidity, _, pair_address)) = by_mint
        .into_iter()
        .max_by(|a, b| (a.1).0.total_cmp(&(b.1).0))?;
    (liquidity >= DOMINANT_LIQUIDITY_SHARE * total && liquidity >= MIN_DOMINANT_LIQUIDITY_USD)
        .then_some((mint, pair_address))
}

/// Outcome of resolving one cashtag at a point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CashtagOutcome {
    Token(TokenRef),
    /// Binance major or stablecoin: never scored.
    Excluded,
    /// DexScreener search found no dominant Solana mint (recorded, never scored).
    Unresolved,
    /// Needs a Binance universe or a DexScreener search first.
    Pending,
}

/// Mentions of one post after resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostResolution {
    pub tokens: Vec<TokenRef>,
    pub unresolved: Vec<String>,
    pub pending: Vec<String>,
}

/// Persistent resolution caches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolverState {
    pub binance: Option<BinanceUniverse>,
    pub dex_search: BTreeMap<String, DexSearchEntry>,
    pub links: BTreeMap<String, CashtagLink>,
}

impl ResolverState {
    /// Rule 1 links: a post with exactly one mint links its cashtags to it
    /// (most recent link wins).
    pub fn learn_links(&mut self, post: &XPost, post_ts_ms: i64) {
        let [mint] = post.solana_addresses.as_slice() else {
            return;
        };
        for tag in &post.cashtags {
            let tag = tag.to_uppercase();
            let newer = self
                .links
                .get(&tag)
                .is_none_or(|link| link.linked_at_ms <= post_ts_ms);
            if newer {
                self.links.insert(
                    tag,
                    CashtagLink {
                        mint: mint.clone(),
                        linked_at_ms: post_ts_ms,
                    },
                );
            }
        }
    }

    /// Rules 2-4 for one cashtag.
    pub fn resolve_cashtag(&self, tag: &str, now_ms: i64) -> CashtagOutcome {
        let tag = tag.to_uppercase();
        let Some(universe) = &self.binance else {
            return CashtagOutcome::Pending;
        };
        if universe.trading_usdt_bases.contains(&tag) {
            if universe.excluded_bases.contains(&tag) {
                return CashtagOutcome::Excluded;
            }
            return CashtagOutcome::Token(TokenRef::binance(&tag));
        }
        if let Some(link) = self.links.get(&tag)
            && now_ms - link.linked_at_ms <= CASHTAG_LINK_TTL_MS
        {
            return CashtagOutcome::Token(TokenRef::solana(
                &link.mint,
                Some(tag),
                None,
                Resolution::CashtagLink,
            ));
        }
        match self.dex_search.get(&tag) {
            Some(entry) if entry.is_fresh(now_ms) => match &entry.mint {
                Some(mint) => CashtagOutcome::Token(TokenRef::solana(
                    mint,
                    Some(tag),
                    entry.pair_address.clone().filter(|p| !p.is_empty()),
                    Resolution::DexscreenerSearch,
                )),
                None => CashtagOutcome::Unresolved,
            },
            _ => CashtagOutcome::Pending,
        }
    }

    /// All mentions of one post. Rule 1 (a mint in the post) wins over the
    /// post's cashtags; EVM addresses are never resolved (rule 5).
    pub fn resolve_post(&self, post: &XPost, now_ms: i64) -> PostResolution {
        let mut out = PostResolution::default();
        if !post.solana_addresses.is_empty() {
            let cashtag = match post.solana_addresses.as_slice() {
                [_] => post.cashtags.first().map(|t| t.to_uppercase()),
                _ => None,
            };
            let mut seen = BTreeSet::new();
            for mint in &post.solana_addresses {
                if seen.insert(mint.as_str()) {
                    out.tokens.push(TokenRef::solana(
                        mint,
                        cashtag.clone(),
                        None,
                        Resolution::MintInPost,
                    ));
                }
            }
            return out;
        }
        let mut seen = BTreeSet::new();
        for tag in &post.cashtags {
            let tag = tag.to_uppercase();
            if !seen.insert(tag.clone()) {
                continue;
            }
            match self.resolve_cashtag(&tag, now_ms) {
                CashtagOutcome::Token(token) => {
                    if !out.tokens.iter().any(|t| t.token_key == token.token_key) {
                        out.tokens.push(token);
                    }
                }
                CashtagOutcome::Excluded => {}
                CashtagOutcome::Unresolved => out.unresolved.push(tag),
                CashtagOutcome::Pending => out.pending.push(tag),
            }
        }
        out
    }

    /// Store a DexScreener search result for `tag`.
    pub fn record_search(&mut self, tag: &str, pairs: &[Object], now_ms: i64) {
        let found = dominant_solana_mint(tag, pairs);
        self.dex_search.insert(
            tag.to_uppercase(),
            DexSearchEntry {
                checked_at_ms: now_ms,
                pair_address: found.as_ref().map(|(_, pair)| pair.clone()),
                mint: found.map(|(mint, _)| mint),
            },
        );
    }

    /// Drop expired links and search entries.
    pub fn prune(&mut self, now_ms: i64) {
        self.links
            .retain(|_, link| now_ms - link.linked_at_ms <= CASHTAG_LINK_TTL_MS);
        self.dex_search.retain(|_, entry| entry.is_fresh(now_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bb_binance::spot::SpotSymbolInfo;
    use rust_decimal::Decimal;
    use serde_json::json;

    const NOW: i64 = 1_791_900_000_000;

    fn sym(base: &str, status: &str) -> SpotSymbolInfo {
        SpotSymbolInfo {
            symbol: format!("{base}USDT"),
            status: status.into(),
            base_asset: base.into(),
            quote_asset: "USDT".into(),
            ..SpotSymbolInfo::default()
        }
    }

    fn ticker(base: &str, quote_volume: i64, last: &str, high: &str, low: &str) -> Ticker24h {
        Ticker24h {
            symbol: format!("{base}USDT"),
            quote_volume: Decimal::from(quote_volume),
            last_price: last.parse().unwrap(),
            high_price: high.parse().unwrap(),
            low_price: low.parse().unwrap(),
            ..Ticker24h::default()
        }
    }

    fn universe() -> BinanceUniverse {
        let mut symbols: Vec<SpotSymbolInfo> = (0..20)
            .map(|i| sym(&format!("MAJ{i}"), "TRADING"))
            .collect();
        symbols.extend([
            sym("PEPE", "TRADING"),
            sym("FDUSD", "TRADING"),
            sym("OLD", "BREAK"),
        ]);
        let mut tickers: Vec<Ticker24h> = (0..20)
            .map(|i| ticker(&format!("MAJ{i}"), 1_000_000_000 - i, "10", "11", "9"))
            .collect();
        tickers.push(ticker("PEPE", 5_000_000, "0.00001", "0.000012", "0.000009"));
        tickers.push(ticker("FDUSD", 1_000, "0.999", "1.001", "0.998"));
        BinanceUniverse::from_market(
            &ExchangeInfo {
                symbols,
                ..ExchangeInfo::default()
            },
            &tickers,
            NOW,
        )
    }

    fn pair(mint: &str, symbol: &str, chain: &str, liquidity: f64, address: &str) -> Object {
        json!({
            "chainId": chain, "pairAddress": address,
            "baseToken": {"address": mint, "symbol": symbol},
            "liquidity": {"usd": liquidity}
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn post(cashtags: &[&str], mints: &[&str]) -> XPost {
        XPost {
            cashtags: cashtags.iter().map(|s| (*s).to_owned()).collect(),
            solana_addresses: mints.iter().map(|s| (*s).to_owned()).collect(),
            evm_addresses: vec!["0x0000000000000000000000000000000000000001".into()],
            ..XPost::default()
        }
    }

    #[test]
    fn universe_excludes_top20_majors_and_stablecoins_only() {
        let u = universe();
        assert!(u.excluded_bases.contains("MAJ0") && u.excluded_bases.contains("MAJ19"));
        assert!(u.excluded_bases.contains("FDUSD"));
        assert!(!u.excluded_bases.contains("PEPE"));
        assert!(!u.trading_usdt_bases.contains("OLD"));
    }

    #[test]
    fn stablecoin_rule_boundaries() {
        assert!(is_stablecoin(1.019, Some((1.01, 1.0))));
        assert!(!is_stablecoin(1.021, Some((1.01, 1.0))));
        assert!(!is_stablecoin(1.0, Some((1.02, 1.0))));
    }

    #[test]
    fn dominant_mint_requires_80_percent_and_5000_usd() {
        let pairs = vec![
            pair("MINTA", "pepe", "solana", 8_000.0, "PA1"),
            pair("MINTA", "PEPE", "solana", 1_000.0, "PA2"),
            pair("MINTB", "PEPE", "solana", 1_000.0, "PB"),
            pair("EVM", "PEPE", "ethereum", 1_000_000.0, "PE"),
            pair("OTHER", "PEPE2", "solana", 1_000_000.0, "PO"),
        ];
        // 9000 / 10000 = 90% -> resolved to the most liquid pair of MINTA.
        assert_eq!(
            dominant_solana_mint("PEPE", &pairs),
            Some(("MINTA".into(), "PA1".into()))
        );
        // Exactly 80% passes; below fails.
        let at = vec![
            pair("A", "X", "solana", 8_000.0, "P1"),
            pair("B", "X", "solana", 2_000.0, "P2"),
        ];
        assert!(dominant_solana_mint("X", &at).is_some());
        let below = vec![
            pair("A", "X", "solana", 7_999.0, "P1"),
            pair("B", "X", "solana", 2_001.0, "P2"),
        ];
        assert_eq!(dominant_solana_mint("X", &below), None);
        // Dominant but under $5,000.
        let thin = vec![pair("A", "X", "solana", 4_999.0, "P1")];
        assert_eq!(dominant_solana_mint("X", &thin), None);
        assert_eq!(
            dominant_solana_mint("X", &[pair("A", "X", "solana", 5_000.0, "P1")]),
            Some(("A".into(), "P1".into()))
        );
    }

    #[test]
    fn resolution_precedence_mint_then_listing_then_link_then_search() {
        let mut state = ResolverState {
            binance: Some(universe()),
            ..ResolverState::default()
        };
        // Rule 1: mint wins over the post's cashtags (even a Binance listing).
        let r = state.resolve_post(&post(&["PEPE"], &["MINT1"]), NOW);
        assert_eq!(r.tokens.len(), 1);
        assert_eq!(r.tokens[0].token_key, "solana:MINT1");
        assert_eq!(r.tokens[0].resolution, Resolution::MintInPost);

        // Links from a single-mint post.
        state.learn_links(&post(&["PEPE", "WIF"], &["MINT1"]), NOW - 1_000);
        // Rule 2 beats the link for a Binance listing.
        let r = state.resolve_post(&post(&["PEPE"], &[]), NOW);
        assert_eq!(r.tokens[0].token_key, "binance:PEPE");
        assert_eq!(r.tokens[0].symbol.as_deref(), Some("PEPEUSDT"));
        // Rule 3 for a non-listed tag; beats a cached search result.
        state.record_search(
            "WIF",
            &[pair("MINT9", "WIF", "solana", 50_000.0, "P9")],
            NOW,
        );
        let r = state.resolve_post(&post(&["wif"], &[]), NOW);
        assert_eq!(r.tokens[0].token_key, "solana:MINT1");
        assert_eq!(r.tokens[0].resolution, Resolution::CashtagLink);
        // Link expires after 24h -> rule 4 search result.
        let later = NOW - 1_000 + CASHTAG_LINK_TTL_MS + 1;
        state.dex_search.get_mut("WIF").unwrap().checked_at_ms = later;
        let r = state.resolve_post(&post(&["WIF"], &[]), later);
        assert_eq!(r.tokens[0].token_key, "solana:MINT9");
        assert_eq!(r.tokens[0].resolution, Resolution::DexscreenerSearch);
        assert_eq!(r.tokens[0].pair_address.as_deref(), Some("P9"));

        // Unresolved search, majors excluded, unknown tags pending, EVM ignored.
        state.record_search("NOPE", &[], NOW);
        let r = state.resolve_post(&post(&["NOPE", "MAJ3", "FDUSD", "NEW"], &[]), NOW);
        assert!(r.tokens.is_empty());
        assert_eq!(r.unresolved, vec!["NOPE".to_owned()]);
        assert_eq!(r.pending, vec!["NEW".to_owned()]);
    }

    #[test]
    fn search_cache_ttl_6h_resolved_30m_unresolved() {
        let mut state = ResolverState {
            binance: Some(universe()),
            ..ResolverState::default()
        };
        state.record_search("AAA", &[pair("M", "AAA", "solana", 9_000.0, "P")], NOW);
        state.record_search("BBB", &[], NOW);
        assert!(matches!(
            state.resolve_cashtag("AAA", NOW + RESOLVED_TTL_MS - 1),
            CashtagOutcome::Token(_)
        ));
        assert_eq!(
            state.resolve_cashtag("AAA", NOW + RESOLVED_TTL_MS),
            CashtagOutcome::Pending
        );
        assert_eq!(
            state.resolve_cashtag("BBB", NOW + UNRESOLVED_TTL_MS - 1),
            CashtagOutcome::Unresolved
        );
        assert_eq!(
            state.resolve_cashtag("BBB", NOW + UNRESOLVED_TTL_MS),
            CashtagOutcome::Pending
        );
    }

    #[test]
    fn multi_mint_post_does_not_link_cashtags() {
        let mut state = ResolverState::default();
        state.learn_links(&post(&["AAA"], &["M1", "M2"]), NOW);
        assert!(state.links.is_empty());
        let r = state.resolve_post(&post(&["AAA"], &["M1", "M2", "M1"]), NOW);
        assert_eq!(r.tokens.len(), 2);
        assert!(r.tokens.iter().all(|t| t.cashtag.is_none()));
    }

    #[test]
    fn stablecoin_pair_rule() {
        let stable =
            json!({"priceUsd": "0.9995", "priceChange": {"h1": 0.01, "h6": -0.1, "h24": 0.3}});
        assert!(is_stablecoin_pair(stable.as_object().unwrap()));
        let moving =
            json!({"priceUsd": "1.0", "priceChange": {"h1": 0.01, "h6": -0.1, "h24": 5.0}});
        assert!(!is_stablecoin_pair(moving.as_object().unwrap()));
    }
}

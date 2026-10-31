//! How a tag reaches something the trader can buy (Phase 53, operator decisions
//! 2026-10-30).
//!
//! A tradable asset (a base asset with an eligible Binance Spot market) collects signal
//! from four paths ([`Via`]):
//! - `direct`: its own asset tag (`asset:BTC` reaches BTC);
//! - `proxy`: an asset tag without a Spot market of its own, through the operator's proxy
//!   file `BOT_SPOT_PROXIES` (`{"GOLD": ["PAXG", "XAUT"]}`: `asset:GOLD` reaches PAXG and
//!   XAUT);
//! - `member`: a narrative basket listing the asset (`members.<narrative>.<asset>`, weighted
//!   by the asset's share of the basket);
//! - `link`: posts tying any tag to the asset (`links.<tag>.<asset>`).
//!
//! Members and links land on an asset tag and then follow that asset's direct and proxy
//! paths. Only live asset tags make buy candidates (directly or through a proxy); baskets
//! and links add signal but no candidates. Asset tags with neither a Spot market nor a
//! tradable proxy are scored and recorded, never bought. AI never picks a proxy: the
//! mapping is the operator's file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path as FsPath;

use serde::Serialize;

use crate::tags::{ASSET, Feed};

/// The operator's proxy file: asset tag name -> Binance base assets that stand in for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Proxies(BTreeMap<String, Vec<String>>);

impl Proxies {
    /// Parse `{"GOLD": ["PAXG", "XAUT"]}`. Names are trimmed; tickers trimmed and
    /// upper-cased. An empty name, an empty list, a blank or non-alphanumeric ticker or a
    /// repeated ticker is an error.
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: BTreeMap<String, Vec<String>> =
            serde_json::from_str(text).map_err(|e| format!("not a proxy map: {e}"))?;
        let mut map = BTreeMap::new();
        for (name, tickers) in raw {
            let name = name.trim().to_owned();
            if name.is_empty() {
                return Err("empty tag name".into());
            }
            if tickers.is_empty() {
                return Err(format!("{name}: no proxy listed"));
            }
            let mut seen = BTreeSet::new();
            let mut list = Vec::with_capacity(tickers.len());
            for ticker in tickers {
                let ticker = ticker.trim().to_ascii_uppercase();
                if ticker.is_empty() || !ticker.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return Err(format!("{name}: bad ticker {ticker:?}"));
                }
                if !seen.insert(ticker.clone()) {
                    return Err(format!("{name}: {ticker} listed twice"));
                }
                list.push(ticker);
            }
            if map.insert(name.clone(), list).is_some() {
                return Err(format!("{name} listed twice"));
            }
        }
        Ok(Self(map))
    }

    /// Read the file at `path`.
    pub fn load(path: &FsPath) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The proxies of asset tag `name` (none when it has no entry).
    pub fn of(&self, name: &str) -> &[String] {
        self.0.get(name).map_or(&[], Vec::as_slice)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// How a path reaches a tradable asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Direct,
    Proxy,
    Member,
    Link,
}

/// One tag reaching one tradable asset.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Path {
    pub via: Via,
    /// The tag whose numbers flow along the path (`asset:GOLD`, `narrative:AI`, ...).
    pub tag: String,
    /// The asset tag the path lands on before its direct or proxy step.
    pub asset_tag: String,
    /// 1 for direct, proxy and link paths; the asset's basket share for members.
    pub weight: f64,
}

/// Every path of a feed, plus the buy candidates it makes. Recorded as the proxied asset
/// tags, the scored-only names and the path counts per kind (`reach_paths`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reach {
    /// Paths by tradable asset.
    #[serde(rename = "reach_paths", serialize_with = "count_paths")]
    pub paths: BTreeMap<String, Vec<Path>>,
    /// Tradable assets reached by a live asset tag, directly or through a proxy.
    #[serde(skip)]
    pub candidates: BTreeSet<String>,
    /// Asset tags with an eligible market of their own.
    #[serde(skip)]
    pub direct: Vec<String>,
    /// Asset tags reaching tradable proxies, with those proxies.
    pub proxied: BTreeMap<String, Vec<String>>,
    /// Asset tags with neither: scored and recorded, never bought.
    pub scored_only: Vec<String>,
}

impl Reach {
    /// Paths per kind.
    pub fn counts(&self) -> BTreeMap<Via, usize> {
        tally(&self.paths)
    }
}

fn tally(paths: &BTreeMap<String, Vec<Path>>) -> BTreeMap<Via, usize> {
    let mut counts = BTreeMap::new();
    for path in paths.values().flatten() {
        *counts.entry(path.via).or_insert(0) += 1;
    }
    counts
}

fn count_paths<S: serde::Serializer>(
    paths: &BTreeMap<String, Vec<Path>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.collect_map(tally(paths))
}

/// The tradable assets asset tag `name` lands on: itself when it has a market, and its
/// tradable proxies.
fn targets(name: &str, proxies: &Proxies, tradable: &impl Fn(&str) -> bool) -> Vec<(String, Via)> {
    let own = tradable(name).then(|| (name.to_owned(), Via::Direct));
    own.into_iter()
        .chain(
            proxies
                .of(name)
                .iter()
                .filter(|p| p.as_str() != name && tradable(p))
                .map(|p| (p.clone(), Via::Proxy)),
        )
        .collect()
}

/// Every path in `feed` (see the module docs). `tradable` says whether a base asset has an
/// eligible Binance market.
pub fn build(feed: &Feed, proxies: &Proxies, tradable: impl Fn(&str) -> bool) -> Reach {
    let mut reach = Reach::default();
    let asset_name = |id: &str| id.strip_prefix("asset:").map(str::to_owned);
    for (id, tag) in feed.tags.iter().filter(|(_, t)| t.kind == ASSET) {
        let reached = targets(&tag.name, proxies, &tradable);
        if reached.is_empty() {
            reach.scored_only.push(tag.name.clone());
        }
        for (asset, via) in reached {
            match via {
                Via::Proxy => reach
                    .proxied
                    .entry(tag.name.clone())
                    .or_default()
                    .push(asset.clone()),
                _ => reach.direct.push(tag.name.clone()),
            }
            reach.candidates.insert(asset.clone());
            reach.paths.entry(asset).or_default().push(Path {
                via,
                tag: id.clone(),
                asset_tag: id.clone(),
                weight: 1.0,
            });
        }
    }
    let mut land = |via: Via, tag: &str, asset_tag: &str, weight: f64| {
        let Some(name) = asset_name(asset_tag) else {
            return;
        };
        for (asset, _) in targets(&name, proxies, &tradable) {
            reach.paths.entry(asset).or_default().push(Path {
                via,
                tag: tag.to_owned(),
                asset_tag: asset_tag.to_owned(),
                weight,
            });
        }
    };
    for (narrative, members) in &feed.members {
        for (asset_tag, share) in members {
            land(Via::Member, narrative, asset_tag, *share);
        }
    }
    for (tag, assets) in &feed.links {
        for asset_tag in assets.keys().filter(|a| *a != tag) {
            land(Via::Link, tag, asset_tag, 1.0);
        }
    }
    reach.scored_only.sort();
    reach.direct.sort();
    reach
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"{"schema":"tags-feed-v1","as_of":"2026-10-20T10:00:00Z",
        "tags":{"asset:BTC":{"kind":"asset","name":"BTC"},
                "asset:GOLD":{"kind":"asset","name":"GOLD"},
                "asset:NVDA":{"kind":"asset","name":"NVDA"},
                "asset:SOL":{"kind":"asset","name":"SOL"},
                "narrative:AI":{"kind":"narrative","name":"AI"},
                "narrative:hard money":{"kind":"narrative","name":"hard money"},
                "macro:war":{"kind":"macro","name":"war"}},
        "members":{"narrative:AI":{"asset:NVDA":0.6,"asset:SOL":0.4},
                   "narrative:hard money":{"asset:GOLD":0.75,"asset:BTC":0.25}},
        "links":{"macro:war":{"asset:GOLD":{"30d":{"posters":7,"up":9.0,"down":1.0}}},
                 "asset:NVDA":{"asset:SOL":{"30d":{"posters":2}},"asset:NVDA":{}}}}"#;

    fn listed(asset: &str) -> bool {
        matches!(asset, "BTC" | "SOL" | "PAXG" | "XAUT")
    }

    fn gold() -> Proxies {
        Proxies::parse(r#"{"GOLD": ["paxg", " XAUT "], "SILVER": ["XAG"]}"#).unwrap()
    }

    fn paths<'a>(reach: &'a Reach, asset: &str) -> Vec<(Via, &'a str, f64)> {
        reach.paths[asset]
            .iter()
            .map(|p| (p.via, p.tag.as_str(), p.weight))
            .collect()
    }

    #[test]
    fn each_path_reaches_the_tradable_assets() {
        let feed = crate::tags::parse_raw(FEED).unwrap();
        let reach = build(&feed, &gold(), listed);
        // Direct: BTC and SOL have markets.
        assert_eq!(reach.direct, ["BTC", "SOL"]);
        // Proxy: GOLD has no market and reaches PAXG and XAUT through the operator's file.
        assert_eq!(
            reach.proxied,
            BTreeMap::from([(
                "GOLD".to_owned(),
                vec!["PAXG".to_owned(), "XAUT".to_owned()]
            )])
        );
        // Candidates come from live asset tags only.
        assert_eq!(
            reach.candidates.iter().collect::<Vec<_>>(),
            ["BTC", "PAXG", "SOL", "XAUT"]
        );
        assert_eq!(
            paths(&reach, "PAXG"),
            [
                (Via::Proxy, "asset:GOLD", 1.0),
                // Member: the hard-money basket lands on GOLD, then its proxy.
                (Via::Member, "narrative:hard money", 0.75),
                // Link: war talk tied to GOLD.
                (Via::Link, "macro:war", 1.0),
            ]
        );
        assert_eq!(paths(&reach, "XAUT"), paths(&reach, "PAXG"));
        assert_eq!(
            paths(&reach, "SOL"),
            [
                (Via::Direct, "asset:SOL", 1.0),
                (Via::Member, "narrative:AI", 0.4),
                (Via::Link, "asset:NVDA", 1.0)
            ]
        );
        assert_eq!(
            paths(&reach, "BTC"),
            [
                (Via::Direct, "asset:BTC", 1.0),
                (Via::Member, "narrative:hard money", 0.25)
            ]
        );
        assert_eq!(
            reach.counts(),
            BTreeMap::from([
                (Via::Direct, 2),
                (Via::Proxy, 2),
                (Via::Member, 4),
                (Via::Link, 3)
            ])
        );
    }

    #[test]
    fn a_name_without_market_or_proxy_is_scored_only_and_never_a_candidate() {
        let feed = crate::tags::parse_raw(FEED).unwrap();
        let reach = build(&feed, &gold(), listed);
        // NVDA: no Spot market, no proxy. Its basket and link still reach SOL, but NVDA
        // itself is neither a candidate nor a path target.
        assert_eq!(reach.scored_only, ["NVDA"]);
        assert!(!reach.candidates.contains("NVDA") && !reach.paths.contains_key("NVDA"));
        // Without the proxy file GOLD is scored only too, and PAXG is no candidate.
        let bare = build(&feed, &Proxies::default(), listed);
        assert_eq!(bare.scored_only, ["GOLD", "NVDA"]);
        assert!(!bare.candidates.contains("PAXG") && bare.proxied.is_empty());
        // A proxy without an eligible market is no target.
        let reach = build(&feed, &gold(), |a| matches!(a, "BTC" | "SOL" | "XAUT"));
        assert_eq!(reach.proxied["GOLD"], ["XAUT"]);
        assert!(!reach.paths.contains_key("PAXG"));
    }

    #[test]
    fn the_proxy_file_is_strict() {
        let p = gold();
        assert_eq!(p.of("GOLD"), ["PAXG", "XAUT"]);
        assert!(p.of("OIL").is_empty());
        for bad in [
            r#"["GOLD"]"#,
            r#"{"GOLD": []}"#,
            r#"{"GOLD": [""]}"#,
            r#"{"GOLD": ["PAX G"]}"#,
            r#"{"GOLD": ["PAXG", "paxg"]}"#,
            r#"{" ": ["PAXG"]}"#,
            r#"{"GOLD": ["PAXG"], " GOLD": ["XAUT"]}"#,
        ] {
            assert!(Proxies::parse(bad).is_err(), "{bad}");
        }
        let missing = Proxies::load(FsPath::new("/nonexistent/proxies.json")).unwrap_err();
        assert!(missing.contains("/nonexistent/proxies.json"), "{missing}");
    }
}

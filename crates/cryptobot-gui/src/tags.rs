//! The Tags tab: the tags feed (`tags-feed-v1`) and the tag registry (`tags-registry.json`)
//! as biz-app wrote them, read directly (Phase 53 E2).
//!
//! - Every live tag by kind with its 24h and 7d numbers, its registry aliases, and `new`
//!   when it went live within [`NEW_HOURS`] of the feed's `as_of`.
//! - Asset tags are marked as the trader's reach marks them ([`bb_trader::reach`]): an own
//!   Binance Spot market (`binance-spot-assets.json`, biz-app's list next to the feed), a
//!   market through the operator's proxy file (`BOT_SPOT_PROXIES`), or scored only.
//! - Baskets (narrative members with weights), links (posts tying a tag to an asset) with
//!   7d posters, events by date and claims with target, deadline and status (`open` or
//!   `deadline passed` against the feed's `as_of`; no price check, operator 2026-10-31).
//!
//! The registry and the asset list are the feed writer's siblings: `tags-registry.json` and
//! `binance-spot-assets.json` in the feed file's directory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bb_trader::reach::Proxies;
use bb_trader::tags::{Feed, Tag, Window};
use serde::Deserialize;
use serde_json::{Value, json};

/// A tag that went live this many hours before the feed's `as_of` (or later) is new.
pub const NEW_HOURS: i64 = 24;
/// biz-app's registry file, next to the feed.
pub const REGISTRY_FILE: &str = "tags-registry.json";
/// biz-app's list of Binance Spot base assets, next to the feed.
pub const SPOT_ASSETS_FILE: &str = "binance-spot-assets.json";
/// Tag kinds in display order (biz-app's seven).
pub const KINDS: [&str; 7] = [
    "asset",
    "narrative",
    "macro",
    "event",
    "entity",
    "claim",
    "mood",
];

/// The files the Tags tab reads.
#[derive(Debug, Clone)]
pub struct TagFiles {
    /// `BOT_SPOT_TAGS_FEED_PATH`, as the trader.
    pub feed_path: PathBuf,
    /// `BOT_SPOT_PROXIES` (none: no proxies, as the trader).
    pub proxies_path: Option<PathBuf>,
}

impl TagFiles {
    pub fn registry_path(&self) -> PathBuf {
        self.sibling(REGISTRY_FILE)
    }

    pub fn spot_assets_path(&self) -> PathBuf {
        self.sibling(SPOT_ASSETS_FILE)
    }

    fn sibling(&self, name: &str) -> PathBuf {
        self.feed_path
            .parent()
            .map_or_else(|| PathBuf::from(name), |dir| dir.join(name))
    }
}

#[derive(Debug, Deserialize)]
struct Registry {
    schema: String,
    #[serde(default)]
    as_of: Option<String>,
    #[serde(default)]
    prompt_version: Option<String>,
    #[serde(default)]
    live_posters: Option<u64>,
    #[serde(default)]
    live_window_days: Option<u64>,
    tags: Vec<RegistryTag>,
}

#[derive(Debug, Deserialize)]
struct RegistryTag {
    id: String,
    kind: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SpotAssets {
    assets: Vec<String>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn iso_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// (up − down) / (up + down); `None` without tagged posts.
fn lean(w: &Window) -> Option<f64> {
    let tagged = w.up + w.down;
    (tagged > 0.0).then(|| (w.up - w.down) / tagged)
}

/// The largest of short, swing and long over a window (ties: the shorter); `None` when no
/// post stated one.
fn horizon(w: &Window) -> Option<&'static str> {
    let h = w.horizon;
    let parts = [("short", h.short), ("swing", h.swing), ("long", h.long)];
    let top = parts.iter().map(|(_, v)| *v).fold(0.0, f64::max);
    (top > 0.0).then(|| {
        parts
            .iter()
            .find(|(_, v)| *v >= top)
            .map_or("short", |(n, _)| n)
    })
}

/// A claim's status at the feed's `as_of`: `open` until its deadline's day ends,
/// `deadline passed` after, `deadline unclear` when the deadline is not a date.
fn claim_status(deadline: Option<&str>, as_of_ms: i64) -> Option<&'static str> {
    let deadline = deadline?;
    let day = deadline.get(..10).unwrap_or(deadline);
    let Ok(date) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
        return Some("deadline unclear");
    };
    let end_ms = date
        .succ_opt()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp_millis())?;
    Some(if as_of_ms < end_ms {
        "open"
    } else {
        "deadline passed"
    })
}

/// How the trader's reach marks an asset tag.
struct Market<'a> {
    direct: BTreeSet<&'a str>,
    proxied: &'a BTreeMap<String, Vec<String>>,
}

impl Market<'_> {
    fn of(&self, name: &str) -> Value {
        if self.direct.contains(name) {
            json!({"market": "tradable"})
        } else if let Some(proxies) = self.proxied.get(name) {
            json!({"market": "proxy", "proxies": proxies})
        } else {
            json!({"market": "scored_only"})
        }
    }
}

fn tag_row(
    id: &str,
    tag: &Tag,
    aliases: Option<&Vec<String>>,
    as_of_ms: i64,
    market: Option<&Market<'_>>,
) -> Value {
    let w = &tag.windows;
    let live_ms = tag.live_since.as_deref().and_then(iso_ms);
    let aliases: Vec<&String> = aliases
        .map(|a| a.iter().filter(|alias| **alias != tag.name).collect())
        .unwrap_or_default();
    let mut row = json!({
        "id": id,
        "kind": tag.kind,
        "name": tag.name,
        "aliases": aliases,
        "first_seen": tag.first_seen,
        "live_since": tag.live_since,
        "new": live_ms.is_some_and(|t| as_of_ms - t <= NEW_HOURS * 3_600_000),
        "mentions_24h": w.h24.mentions,
        "posters_24h": w.h24.posters,
        "posters_7d": w.d7.posters,
        "posters_30d": w.d30.posters,
        "up_7d": w.d7.up,
        "down_7d": w.d7.down,
        "lean_24h": lean(&w.h24),
        "lean_7d": lean(&w.d7),
        "horizon_7d": horizon(&w.d7),
        "event_at": tag.event_at,
        "target": tag.target,
        "deadline": tag.deadline,
        "claim_status": (tag.kind == "claim")
            .then(|| claim_status(tag.deadline.as_deref(), as_of_ms))
            .flatten(),
    });
    if tag.kind == "asset"
        && let (Some(market), Some(obj)) = (market, row.as_object_mut())
        && let Some(marks) = market.of(&tag.name).as_object()
    {
        obj.extend(marks.clone());
    }
    row
}

/// The Tags tab's data (see the module docs). `now_ms` gives the feed's age.
pub fn tags_view(files: &TagFiles, now_ms: i64) -> Value {
    let registry_path = files.registry_path();
    let spot_path = files.spot_assets_path();
    let registry = read_json::<Registry>(&registry_path).and_then(|r| {
        if r.schema == "tags-registry-v1" {
            Ok(r)
        } else {
            Err(format!("schema {:?}, expected tags-registry-v1", r.schema))
        }
    });
    let registry_json = match &registry {
        Ok(r) => {
            let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
            for t in &r.tags {
                *by_kind.entry(t.kind.as_str()).or_default() += 1;
            }
            json!({"path": registry_path, "error": null, "as_of": r.as_of,
                   "prompt_version": r.prompt_version, "live_posters": r.live_posters,
                   "live_window_days": r.live_window_days, "tags": r.tags.len(), "by_kind": by_kind})
        }
        Err(e) => json!({"path": registry_path, "error": e}),
    };
    let feed = std::fs::read_to_string(&files.feed_path)
        .map_err(|e| format!("{}: {e}", files.feed_path.display()))
        .and_then(|text| bb_trader::tags::parse_raw(&text));
    let feed = match feed {
        Ok(feed) => feed,
        Err(error) => {
            return json!({
                "feed": {"path": files.feed_path, "error": error},
                "registry": registry_json,
                "new_hours": NEW_HOURS,
            });
        }
    };
    let spot = read_json::<SpotAssets>(&spot_path);
    let proxies = match &files.proxies_path {
        Some(path) => Proxies::load(path),
        None => Ok(Proxies::default()),
    };
    let reach = match (&spot, &proxies) {
        (Ok(spot), Ok(proxies)) => {
            let listed: BTreeSet<&str> = spot.assets.iter().map(String::as_str).collect();
            Some(bb_trader::reach::build(&feed, proxies, |a| {
                listed.contains(a)
            }))
        }
        _ => None,
    };
    let market = reach.as_ref().map(|r| Market {
        direct: r.direct.iter().map(String::as_str).collect(),
        proxied: &r.proxied,
    });
    view(
        &feed,
        registry.ok().as_ref(),
        market.as_ref(),
        json!({
            "spot_assets_path": spot_path,
            "spot_assets_error": spot.err(),
            "proxies_path": files.proxies_path,
            "proxies_error": proxies.err(),
        }),
        files,
        now_ms,
        registry_json,
    )
}

fn view(
    feed: &Feed,
    registry: Option<&Registry>,
    market: Option<&Market<'_>>,
    market_files: Value,
    files: &TagFiles,
    now_ms: i64,
    registry_json: Value,
) -> Value {
    let aliases: BTreeMap<&str, &Vec<String>> = registry
        .map(|r| r.tags.iter().map(|t| (t.id.as_str(), &t.aliases)).collect())
        .unwrap_or_default();
    let registered: BTreeMap<&str, usize> = registry
        .map(|r| {
            let mut by_kind = BTreeMap::new();
            for t in &r.tags {
                *by_kind.entry(t.kind.as_str()).or_default() += 1;
            }
            by_kind
        })
        .unwrap_or_default();
    let as_of_ms = feed.as_of_ms;
    let mut tags: Vec<Value> = feed
        .tags
        .iter()
        .map(|(id, tag)| tag_row(id, tag, aliases.get(id.as_str()).copied(), as_of_ms, market))
        .collect();
    let rank = |kind: &str| KINDS.iter().position(|k| *k == kind).unwrap_or(KINDS.len());
    tags.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                rank(v["kind"].as_str().unwrap_or_default()),
                std::cmp::Reverse(v["posters_7d"].as_u64().unwrap_or(0)),
                v["name"].as_str().unwrap_or_default().to_owned(),
            )
        };
        key(a).cmp(&key(b))
    });
    let mut kinds: Vec<&str> = KINDS.to_vec();
    for tag in feed.tags.values() {
        if !kinds.contains(&tag.kind.as_str()) {
            kinds.push(tag.kind.as_str());
        }
    }
    let kinds: Vec<Value> = kinds
        .into_iter()
        .map(|kind| {
            let live = tags.iter().filter(|t| t["kind"] == kind);
            let (count, new) = live.fold((0, 0), |(n, k), t| {
                (n + 1, k + usize::from(t["new"] == true))
            });
            json!({"kind": kind, "live": count, "new": new, "registered": registered.get(kind)})
        })
        .collect();
    let asset_market = |asset_id: &str| -> Value {
        let name = asset_id.strip_prefix("asset:").unwrap_or(asset_id);
        market.map_or(Value::Null, |m| m.of(name)["market"].clone())
    };
    let mut baskets: Vec<Value> = feed
        .members
        .iter()
        .map(|(id, members)| {
            let mut members: Vec<(&String, &f64)> = members.iter().collect();
            members.sort_by(|a, b| b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0)));
            let tag = feed.tags.get(id);
            json!({
                "id": id,
                "name": tag.map_or_else(|| id.trim_start_matches("narrative:").to_owned(), |t| t.name.clone()),
                "posters_7d": tag.map_or(0, |t| t.windows.d7.posters),
                "members": members.iter().map(|(asset, weight)| json!({
                    "asset": asset.trim_start_matches("asset:"),
                    "weight": weight,
                    "market": asset_market(asset),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    baskets.sort_by_key(|b| std::cmp::Reverse(b["posters_7d"].as_u64().unwrap_or(0)));
    let mut links: Vec<Value> = feed
        .links
        .iter()
        .flat_map(|(tag, assets)| {
            assets.iter().map(move |(asset, w)| {
                json!({
                    "tag": tag,
                    "asset": asset.trim_start_matches("asset:"),
                    "posters_7d": w.d7.posters,
                    "up_7d": w.d7.up,
                    "down_7d": w.d7.down,
                    "posters_30d": w.d30.posters,
                })
            })
        })
        .collect();
    let links_total = links.len();
    links.retain(|l| l["posters_7d"].as_u64().unwrap_or(0) > 0);
    links.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                std::cmp::Reverse(v["posters_7d"].as_u64().unwrap_or(0)),
                std::cmp::Reverse(v["posters_30d"].as_u64().unwrap_or(0)),
            )
        };
        key(a).cmp(&key(b))
    });
    json!({
        "feed": {
            "path": files.feed_path,
            "error": null,
            "as_of": feed.as_of,
            "age_minutes": (now_ms - as_of_ms).max(0) / 60_000,
            "sources": feed.sources,
        },
        "registry": registry_json,
        "market_files": market_files,
        "new_hours": NEW_HOURS,
        "kinds": kinds,
        "tags": tags,
        "baskets": baskets,
        "links": links,
        "links_total": links_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-31T00:00:00Z, the B5 fixture's `as_of`.
    const AS_OF_MS: i64 = 1_793_404_800_000;
    const H: i64 = 3_600_000;

    fn files(dir: &Path, proxies: bool) -> TagFiles {
        let feed = include_str!("../../bb-trader/tests/fixtures/tags-feed-v1.json");
        std::fs::write(dir.join("tags-feed.json"), feed).unwrap();
        std::fs::write(
            dir.join(REGISTRY_FILE),
            json!({"schema": "tags-registry-v1", "as_of": "2026-10-31T00:00:00Z", "prompt_version": "merge-2",
                   "live_posters": 5, "live_window_days": 7,
                   "tags": [{"id": "asset:BTC", "kind": "asset", "name": "BTC", "aliases": ["BTC", "Bitcoin", "btc"]},
                            {"id": "asset:OLD", "kind": "asset", "name": "OLD", "aliases": ["OLD"]}]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join(SPOT_ASSETS_FILE),
            r#"{"assets": ["BTC", "ETH", "PAXG", "XAUT", "LINK"]}"#,
        )
        .unwrap();
        let proxies_path = proxies.then(|| {
            let path = dir.join("proxies.json");
            std::fs::write(&path, r#"{"GOLD": ["PAXG", "XAUT"]}"#).unwrap();
            path
        });
        TagFiles {
            feed_path: dir.join("tags-feed.json"),
            proxies_path,
        }
    }

    fn tag<'a>(view: &'a Value, id: &str) -> &'a Value {
        view["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .unwrap()
    }

    #[test]
    fn every_live_tag_shows_by_kind_with_its_registry_aliases_and_market() {
        let dir = tempfile::tempdir().unwrap();
        let view = tags_view(&files(dir.path(), true), AS_OF_MS + 20 * 60_000);
        assert_eq!(view["feed"]["age_minutes"], 20);
        assert_eq!(view["registry"]["tags"], 2);
        // Every kind of the B5 feed is listed with its live count, in biz-app's order.
        let kinds: Vec<(&str, u64)> = view["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| (k["kind"].as_str().unwrap(), k["live"].as_u64().unwrap()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("asset", 230),
                ("narrative", 77),
                ("macro", 19),
                ("event", 28),
                ("entity", 83),
                ("claim", 19),
                ("mood", 6)
            ]
        );
        assert_eq!(view["tags"].as_array().unwrap().len(), 462);
        // Sorted by kind, then 7d posters.
        let assets: Vec<u64> = view["tags"]
            .as_array()
            .unwrap()
            .iter()
            .take_while(|t| t["kind"] == "asset")
            .map(|t| t["posters_7d"].as_u64().unwrap())
            .collect();
        assert_eq!(assets.len(), 230);
        assert!(assets.windows(2).all(|w| w[0] >= w[1]));
        // Aliases come from the registry (the name itself left out).
        assert_eq!(
            tag(&view, "asset:BTC")["aliases"],
            json!(["Bitcoin", "btc"])
        );
        // Markets as the trader's reach: own Binance market, a proxy, or scored only.
        assert_eq!(tag(&view, "asset:BTC")["market"], "tradable");
        assert_eq!(
            (
                tag(&view, "asset:GOLD")["market"].as_str(),
                &tag(&view, "asset:GOLD")["proxies"]
            ),
            (Some("proxy"), &json!(["PAXG", "XAUT"]))
        );
        assert_eq!(tag(&view, "asset:NVDA")["market"], "scored_only");
        // Only asset tags carry a market.
        assert!(
            view["tags"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| (t["kind"] == "asset") == t.get("market").is_some())
        );
        // Without the proxy file gold is scored only, as the trader without BOT_SPOT_PROXIES.
        let bare = tags_view(&files(dir.path(), false), AS_OF_MS);
        assert_eq!(tag(&bare, "asset:GOLD")["market"], "scored_only");
    }

    #[test]
    fn new_tags_events_claims_baskets_and_links_are_derived_from_the_feed() {
        let dir = tempfile::tempdir().unwrap();
        let view = tags_view(&files(dir.path(), true), AS_OF_MS);
        // New: live within 24h of as_of.
        for t in view["tags"].as_array().unwrap() {
            let live = t["live_since"].as_str().and_then(iso_ms);
            assert_eq!(
                t["new"],
                json!(live.is_some_and(|l| AS_OF_MS - l <= 24 * H)),
                "{t}"
            );
        }
        assert!(
            view["kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["new"].as_u64().unwrap() > 0)
        );
        // Claims carry target, deadline and status against as_of.
        let claim = view["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "BTC to 1M")
            .unwrap();
        assert_eq!(
            (
                claim["target"].as_str(),
                claim["deadline"].as_str(),
                claim["claim_status"].as_str()
            ),
            (Some("1000000"), Some("2026-10-12"), Some("deadline passed"))
        );
        let open = view["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "BTC dip")
            .unwrap();
        assert_eq!(open["claim_status"], "open");
        // Events carry their date.
        let fomc = view["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "FOMC")
            .unwrap();
        assert_eq!(
            (fomc["kind"].as_str(), fomc["event_at"].as_str()),
            (Some("event"), Some("2026-10-28"))
        );
        // Baskets list members by weight with their market.
        let ai = view["baskets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "narrative:AI")
            .unwrap();
        assert_eq!(
            ai["members"][0],
            json!({"asset": "GOOG", "weight": 0.209, "market": "scored_only"})
        );
        assert!(
            ai["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["asset"] == "BTC" && m["market"] == "tradable")
        );
        assert_eq!(view["baskets"].as_array().unwrap().len(), 75);
        // Links with 7d posters only, loudest first; the total counts all.
        assert_eq!(view["links_total"], 965);
        let links = view["links"].as_array().unwrap();
        assert!(!links.is_empty() && links.len() < 965);
        assert!(links.iter().all(|l| l["posters_7d"].as_u64().unwrap() > 0));
        assert!(
            links
                .windows(2)
                .all(|w| w[0]["posters_7d"].as_u64() >= w[1]["posters_7d"].as_u64())
        );
    }

    #[test]
    fn missing_or_foreign_files_are_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let files = TagFiles {
            feed_path: dir.path().join("tags-feed.json"),
            proxies_path: None,
        };
        let view = tags_view(&files, AS_OF_MS);
        assert!(
            view["feed"]["error"]
                .as_str()
                .unwrap()
                .contains("tags-feed.json")
        );
        assert!(
            view["registry"]["error"]
                .as_str()
                .unwrap()
                .contains(REGISTRY_FILE)
        );
        std::fs::write(
            &files.feed_path,
            r#"{"schema":"biz-feed-v1","as_of":"2026-10-31T00:00:00Z","tags":{}}"#,
        )
        .unwrap();
        assert!(
            tags_view(&files, AS_OF_MS)["feed"]["error"]
                .as_str()
                .unwrap()
                .contains("tags-feed-v1")
        );
        // A feed without the asset list: tags show, markets unknown.
        let with_feed = self::files(dir.path(), false);
        std::fs::remove_file(dir.path().join(SPOT_ASSETS_FILE)).unwrap();
        let view = tags_view(&with_feed, AS_OF_MS);
        assert!(
            view["market_files"]["spot_assets_error"]
                .as_str()
                .unwrap()
                .contains(SPOT_ASSETS_FILE)
        );
        assert!(tag(&view, "asset:BTC").get("market").is_none());
    }

    #[test]
    fn a_claim_is_open_through_its_deadline_day() {
        let day = AS_OF_MS; // 2026-10-31T00:00Z
        assert_eq!(claim_status(Some("2026-10-31"), day + 23 * H), Some("open"));
        assert_eq!(
            claim_status(Some("2026-10-31"), day + 24 * H),
            Some("deadline passed")
        );
        assert_eq!(
            claim_status(Some("2026-10-31T12:00:00Z"), day),
            Some("open")
        );
        assert_eq!(
            claim_status(Some("end of year"), day),
            Some("deadline unclear")
        );
        assert_eq!(claim_status(None, day), None);
    }
}

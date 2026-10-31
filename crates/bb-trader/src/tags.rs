//! The tags feed (`tags-feed-v1`), read every tick and summarised on each decision row
//! (audit field `tags`); the algorithm ([`crate::decider`]) scores the asset tags of the
//! coins it may buy.
//!
//! biz-app writes the feed into the trader's data dir (on the Pi, the `biz-feed` systemd
//! user unit, `deploy/seref/biz-feed.service`). It lists the tags live at `as_of` (5 or more
//! distinct posters in 7 days): `tags.<kind>:<name>` with the kind (asset, narrative, macro,
//! event, entity, claim, mood), dates, and per window (`1h`, `6h`, `24h`, `7d`, `30d`) the
//! mentions, distinct posters and threads, the AI's `up` / `down` weighted by post score
//! (1-3) and the posters' horizon mix; `members` (narrative baskets) and `links` (posts
//! tying a tag to an asset). Asset tags of live Binance Spot coins count `mentions`,
//! `posters` and `threads` over every post naming the coin without AI (as the old coin feed
//! did); their `up` / `down` are the AI's bullish / bearish tags.
//!
//! A missing or unreadable file, another schema, or an `as_of` older than
//! `BOT_SPOT_TAGS_MAX_AGE_MINUTES` makes the tick record "tags unavailable" with the reason.
//! Asset tags without an eligible Binance market (gold, stocks, unlisted tokens) are listed
//! as `scored_only`: recorded, never bought.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// The only feed format the trader accepts.
pub const FEED_SCHEMA: &str = "tags-feed-v1";
/// Default `BOT_SPOT_TAGS_FEED_PATH`.
pub const DEFAULT_FEED_PATH: &str = "data/tags-feed.json";
/// Default `BOT_SPOT_TAGS_MAX_AGE_MINUTES`: biz-app rewrites the feed every 15 minutes.
pub const DEFAULT_MAX_AGE_MINUTES: u64 = 45;
/// The kind of tags that name one asset (a coin, a metal, a stock).
pub const ASSET: &str = "asset";
/// An `as_of` further ahead of the trader's clock than this is refused.
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;

/// Score-weighted horizon mix of the posts in a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Horizon {
    pub short: f64,
    pub swing: f64,
    pub long: f64,
    pub unstated: f64,
}

/// One tag's numbers over one window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    /// Posts naming the tag (asset tags of Binance coins: every post, no AI).
    pub mentions: u64,
    /// Distinct posters among them.
    pub posters: u64,
    /// Distinct threads among them.
    pub threads: u64,
    /// AI "up" tags (bullish, rising) weighted by post score (1-3).
    pub up: f64,
    /// AI "down" tags (bearish, easing) weighted by post score (1-3).
    pub down: f64,
    pub horizon: Horizon,
}

/// One tag's windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Windows {
    #[serde(rename = "1h")]
    pub h1: Window,
    #[serde(rename = "6h")]
    pub h6: Window,
    #[serde(rename = "24h")]
    pub h24: Window,
    #[serde(rename = "7d")]
    pub d7: Window,
    #[serde(rename = "30d")]
    pub d30: Window,
}

/// One live tag.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Tag {
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub first_seen: Option<String>,
    #[serde(default)]
    pub live_since: Option<String>,
    /// Events: the date the posters expect it.
    #[serde(default)]
    pub event_at: Option<String>,
    /// Claims: the predicted target and its deadline.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub deadline: Option<String>,
    #[serde(default)]
    pub windows: Windows,
}

/// Posts tying a tag to an asset over one window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LinkWindow {
    pub posters: u64,
    pub up: f64,
    pub down: f64,
}

/// One tag-to-asset link's windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LinkWindows {
    #[serde(rename = "1h")]
    pub h1: LinkWindow,
    #[serde(rename = "6h")]
    pub h6: LinkWindow,
    #[serde(rename = "24h")]
    pub h24: LinkWindow,
    #[serde(rename = "7d")]
    pub d7: LinkWindow,
    #[serde(rename = "30d")]
    pub d30: LinkWindow,
}

/// One source of the feed (now only `biz`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub id: String,
    pub as_of: String,
    /// The AI that tagged the posts, and its instruction version.
    pub model: String,
    pub prompt_version: String,
    pub registry_version: String,
    /// Posts read, and those the AI left untagged.
    pub items: u64,
    pub untagged: u64,
}

#[derive(Debug, Deserialize)]
struct FeedFile {
    schema: String,
    as_of: String,
    #[serde(default)]
    sources: Vec<Source>,
    #[serde(default)]
    tags: BTreeMap<String, Tag>,
    #[serde(default)]
    members: BTreeMap<String, BTreeMap<String, f64>>,
    #[serde(default)]
    links: BTreeMap<String, BTreeMap<String, LinkWindows>>,
}

/// A feed checked for its schema, `as_of` and tag ids, before the age and market checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Feed {
    pub as_of: String,
    pub as_of_ms: i64,
    pub sources: Vec<Source>,
    /// Live tags by id (`<kind>:<name>`).
    pub tags: BTreeMap<String, Tag>,
    /// Narrative baskets: `members.<narrative id>.<asset id>` = weight.
    pub members: BTreeMap<String, BTreeMap<String, f64>>,
    /// `links.<tag id>.<asset id>`: posts that tie the tag to the asset.
    pub links: BTreeMap<String, BTreeMap<String, LinkWindows>>,
}

impl Feed {
    /// Asset tags by asset name (`BTC`, `GOLD`, `NVDA`).
    pub fn assets(&self) -> impl Iterator<Item = (&str, &Windows)> {
        self.tags
            .values()
            .filter(|tag| tag.kind == ASSET)
            .map(|tag| (tag.name.as_str(), &tag.windows))
    }

    /// Every source's `model@prompt_version`.
    pub fn readers(&self) -> impl Iterator<Item = String> + '_ {
        self.sources
            .iter()
            .map(|s| format!("{}@{}", s.model, s.prompt_version))
    }
}

/// A fresh feed as the tick records it: a summary on the row, the assets for the rule.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TagsFeed {
    pub as_of: String,
    /// Minutes between `as_of` and the read.
    pub age_minutes: i64,
    /// sha256 (hex) of the feed file.
    pub digest: String,
    pub sources: Vec<Source>,
    /// Live tags per kind.
    pub kinds: BTreeMap<String, usize>,
    /// Asset tags with an eligible Binance market, by name, with their windows (the rule's
    /// input); recorded as the list of names.
    #[serde(rename = "tradable", serialize_with = "names")]
    pub assets: BTreeMap<String, Windows>,
    /// Asset tags without an eligible Binance market: scored, never bought.
    pub scored_only: Vec<String>,
}

fn names<S: Serializer>(assets: &BTreeMap<String, Windows>, s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(assets.keys())
}

/// What the tick knows about the tags feed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TagsNow {
    Fresh(Box<TagsFeed>),
    Unavailable { reason: String },
}

impl TagsNow {
    /// The part of the reading hashed into `context_hash`: the feed's `as_of` and digest, or
    /// why it was unavailable.
    pub fn fingerprint(&self) -> serde_json::Value {
        match self {
            Self::Fresh(feed) => serde_json::json!({
                "status": "fresh",
                "as_of": feed.as_of,
                "digest": feed.digest,
            }),
            Self::Unavailable { reason } => serde_json::json!({
                "status": "unavailable",
                "reason": reason,
            }),
        }
    }
}

/// Parse a `tags-feed-v1` file; `Err` holds the reason it cannot be used.
pub fn parse_raw(text: &str) -> Result<Feed, String> {
    let file: FeedFile = serde_json::from_str(text).map_err(|e| format!("not a tags feed: {e}"))?;
    if file.schema != FEED_SCHEMA {
        return Err(format!("schema {:?}, expected {FEED_SCHEMA}", file.schema));
    }
    let as_of = chrono::DateTime::parse_from_rfc3339(file.as_of.trim())
        .map_err(|e| format!("as_of {:?}: {e}", file.as_of))?;
    if let Some((id, tag)) = file
        .tags
        .iter()
        .find(|(id, tag)| id.split_once(':') != Some((tag.kind.as_str(), tag.name.as_str())))
    {
        return Err(format!(
            "tag id {id:?} is not kind:name ({}:{})",
            tag.kind, tag.name
        ));
    }
    Ok(Feed {
        as_of_ms: as_of.timestamp_millis(),
        as_of: file.as_of,
        sources: file.sources,
        tags: file.tags,
        members: file.members,
        links: file.links,
    })
}

/// Parse a feed read at `now_ms`; `Err` holds the reason it cannot be used. `tradable`
/// says whether a base asset has an eligible Binance market.
pub fn parse(
    text: &str,
    now_ms: i64,
    max_age_minutes: u64,
    tradable: impl Fn(&str) -> bool,
) -> Result<TagsFeed, String> {
    let feed = parse_raw(text)?;
    let age_ms = now_ms - feed.as_of_ms;
    if age_ms < -MAX_CLOCK_SKEW_MS {
        return Err(format!("as_of {} is ahead of this clock", feed.as_of));
    }
    let age_minutes = age_ms.max(0) / 60_000;
    if age_minutes.unsigned_abs() > max_age_minutes {
        return Err(format!(
            "stale: as_of {} is {age_minutes} min old (limit {max_age_minutes})",
            feed.as_of
        ));
    }
    let mut kinds = BTreeMap::new();
    for tag in feed.tags.values() {
        *kinds.entry(tag.kind.clone()).or_insert(0) += 1;
    }
    let mut assets = BTreeMap::new();
    let mut scored_only = Vec::new();
    for (name, windows) in feed.assets() {
        if tradable(name) {
            assets.insert(name.to_owned(), *windows);
        } else {
            scored_only.push(name.to_owned());
        }
    }
    scored_only.sort();
    Ok(TagsFeed {
        as_of: feed.as_of,
        age_minutes,
        digest: hex::encode(Sha256::digest(text.as_bytes())),
        sources: feed.sources,
        kinds,
        assets,
        scored_only,
    })
}

/// Read the feed at `path` for this tick.
pub fn read(
    path: &Path,
    now_ms: i64,
    max_age_minutes: u64,
    tradable: impl Fn(&str) -> bool,
) -> TagsNow {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("{}: {e}", path.display()))
        .and_then(|text| parse(&text, now_ms, max_age_minutes, tradable));
    match parsed {
        Ok(feed) => TagsNow::Fresh(Box::new(feed)),
        Err(reason) => {
            tracing::info!(%reason, "tags feed unavailable this tick");
            TagsNow::Unavailable { reason }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real feed (`tests/fixtures/tags-feed-v1.json`): biz-app's B5 writer on the laptop
    /// archive, `biz-insights feed --at 2026-10-31T00:00:00Z --no-merge` (verbatim).
    const REAL: &str = include_str!("../tests/fixtures/tags-feed-v1.json");
    /// Its `as_of`, 2026-10-31T00:00:00Z. Tests read it at this time plus a few minutes,
    /// never at the wall clock (the fixture only grows older).
    const REAL_AS_OF_MS: i64 = 1_793_404_800_000;

    const AS_OF_MS: i64 = 1_792_490_400_000; // 2026-10-20T10:00:00Z
    const FEED: &str = r#"{"schema":"tags-feed-v1","as_of":"2026-10-20T10:00:00Z",
        "windows":["1h","6h","24h","7d","30d"],
        "sources":[{"id":"biz","as_of":"2026-10-20T10:00:00Z","model":"gemini","prompt_version":"tags-1",
                    "registry_version":"merge-2","items":900,"untagged":3}],
        "tags":{"asset:XMR":{"kind":"asset","name":"XMR","windows":{"6h":{"mentions":7,"posters":4,"up":3.0,"down":1.0,"posts":[62804123]}}},
                "asset:NVDA":{"kind":"asset","name":"NVDA","windows":{"24h":{"mentions":2}}},
                "asset:BTC":{"kind":"asset","name":"BTC","windows":{"1h":{"mentions":1}}},
                "macro:rates":{"kind":"macro","name":"rates","windows":{"24h":{"mentions":9}}}}}"#;

    fn listed(coin: &str) -> bool {
        matches!(coin, "BTC" | "XMR")
    }

    #[test]
    fn a_fresh_feed_keeps_assets_with_a_binance_market_and_lists_the_rest_as_scored_only() {
        let feed = parse(FEED, AS_OF_MS + 20 * 60_000, 45, listed).unwrap();
        assert_eq!(feed.age_minutes, 20);
        assert_eq!(feed.assets.keys().collect::<Vec<_>>(), ["BTC", "XMR"]);
        assert_eq!(feed.assets["XMR"].h6.posters, 4);
        assert_eq!(
            (feed.assets["XMR"].h6.up, feed.assets["XMR"].h6.down),
            (3.0, 1.0)
        );
        assert_eq!(feed.scored_only, ["NVDA"]);
        assert_eq!(
            feed.kinds,
            BTreeMap::from([("asset".to_owned(), 3), ("macro".to_owned(), 1)])
        );
        assert_eq!(
            (feed.sources[0].model.as_str(), feed.sources[0].untagged),
            ("gemini", 3)
        );
        // The row carries the summary only: names, not windows.
        let row = serde_json::to_value(TagsNow::Fresh(Box::new(feed.clone()))).unwrap();
        assert_eq!(row["status"], "fresh");
        assert_eq!(row["tradable"], serde_json::json!(["BTC", "XMR"]));
        assert_eq!(row["scored_only"], serde_json::json!(["NVDA"]));
        assert_eq!(row["digest"].as_str().unwrap().len(), 64);
        assert!(row.get("assets").is_none() && row.get("tags").is_none());
    }

    #[test]
    fn a_stale_future_foreign_or_inconsistent_feed_is_unavailable_with_its_reason() {
        assert!(parse(FEED, AS_OF_MS + 45 * 60_000, 45, listed).is_ok());
        let stale = parse(FEED, AS_OF_MS + 46 * 60_000, 45, listed).unwrap_err();
        assert!(stale.contains("stale"), "{stale}");
        let ahead = parse(FEED, AS_OF_MS - 6 * 60_000, 45, listed).unwrap_err();
        assert!(ahead.contains("ahead"), "{ahead}");
        let other = FEED.replace("tags-feed-v1", "tags-feed-v2");
        assert!(
            parse(&other, AS_OF_MS, 45, listed)
                .unwrap_err()
                .contains("expected tags-feed-v1")
        );
        // A tag filed under another kind or name is refused, not scored as something else.
        let moved = FEED.replace(
            r#""asset:BTC":{"kind":"asset""#,
            r#""asset:BTC":{"kind":"macro""#,
        );
        assert!(
            parse(&moved, AS_OF_MS, 45, listed)
                .unwrap_err()
                .contains("not kind:name")
        );
        let renamed = FEED.replace(r#""name":"BTC""#, r#""name":"ETH""#);
        assert!(
            parse(&renamed, AS_OF_MS, 45, listed)
                .unwrap_err()
                .contains("not kind:name")
        );

        let missing = read(
            Path::new("/nonexistent/tags-feed.json"),
            AS_OF_MS,
            45,
            listed,
        );
        let value = serde_json::to_value(&missing).unwrap();
        assert_eq!(value["status"], "unavailable");
        assert!(
            value["reason"]
                .as_str()
                .unwrap()
                .contains("/nonexistent/tags-feed.json")
        );
    }

    #[test]
    fn the_fingerprint_changes_with_the_file_not_with_its_age() {
        let now = |minutes: i64| AS_OF_MS + minutes * 60_000;
        let fresh =
            |text: &str, at: i64| TagsNow::Fresh(Box::new(parse(text, at, 45, listed).unwrap()));
        assert_eq!(
            fresh(FEED, now(1)).fingerprint(),
            fresh(FEED, now(30)).fingerprint()
        );
        let edited = FEED.replace(r#""mentions":7"#, r#""mentions":8"#);
        assert_ne!(
            fresh(FEED, now(1)).fingerprint(),
            fresh(&edited, now(1)).fingerprint()
        );
    }

    #[test]
    fn a_real_biz_app_feed_parses_with_every_kind_basket_and_link() {
        let feed = parse_raw(REAL).unwrap();
        assert_eq!(feed.as_of_ms, REAL_AS_OF_MS);
        assert_eq!(
            feed.readers().collect::<Vec<_>>(),
            ["google-antigravity/gemini-3.8-flash@tags-1"]
        );
        let kinds: std::collections::BTreeSet<&str> =
            feed.tags.values().map(|t| t.kind.as_str()).collect();
        assert_eq!(
            kinds.into_iter().collect::<Vec<_>>(),
            [
                "asset",
                "claim",
                "entity",
                "event",
                "macro",
                "mood",
                "narrative"
            ]
        );
        let btc = feed.assets().find(|(name, _)| *name == "BTC").unwrap().1;
        let windows = [btc.h1, btc.h6, btc.h24, btc.d7, btc.d30];
        assert!(
            windows
                .windows(2)
                .all(|w| w[0].mentions <= w[1].mentions && w[0].posters <= w[1].posters)
        );
        assert!(btc.d30.posters >= 5 && btc.d30.up > 0.0 && btc.d30.horizon.long > 0.0);
        assert!(
            feed.tags
                .values()
                .any(|t| t.kind == "claim" && t.target.is_some() && t.deadline.is_some())
        );
        assert!(
            feed.tags
                .values()
                .any(|t| t.kind == "event" && t.event_at.is_some())
        );
        assert!(
            feed.members
                .values()
                .all(|m| (m.values().sum::<f64>() - 1.0).abs() < 1e-3)
        );
        assert!(
            feed.links
                .values()
                .flat_map(BTreeMap::values)
                .any(|l| l.d30.posters > 0)
        );
        // Read ten minutes after its as_of (fresh), and an hour after (stale).
        let fresh = parse(REAL, REAL_AS_OF_MS + 10 * 60_000, 45, |c| {
            matches!(c, "BTC" | "ETH" | "PAXG")
        })
        .unwrap();
        assert_eq!(fresh.assets.keys().collect::<Vec<_>>(), ["BTC", "ETH"]);
        assert!(
            fresh.scored_only.iter().any(|n| n == "GOLD")
                && fresh.scored_only.iter().any(|n| n == "SILVER")
        );
        assert_eq!(fresh.kinds.values().sum::<usize>(), feed.tags.len());
        assert!(
            parse(REAL, REAL_AS_OF_MS + 60 * 60_000, 45, |_| true)
                .unwrap_err()
                .contains("stale")
        );
    }
}

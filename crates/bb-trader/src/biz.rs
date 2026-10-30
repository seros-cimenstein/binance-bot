//! The /biz/ feed, read once per cycle and recorded in the cycle summary (audit field
//! `biz`); the algorithm ([`crate::decider`]) scores its coins on it.
//!
//! biz-app writes the feed (`biz-feed-v1`) into the trader's data dir (on the Pi, the
//! `biz-feed` systemd user unit, `deploy/seref/biz-feed.service`). Per coin and per window
//! (`1h`, `6h`, `24h`) it holds the mentions over every post, the distinct posters, and the
//! AI's bullish and bearish tags weighted by post score; `as_of` is the newest moment whose
//! posts it includes. A missing or unreadable file, another schema, or an `as_of` older than
//! `BOT_SPOT_BIZ_MAX_AGE_MINUTES` makes the cycle record "/biz/ unavailable" with the
//! reason. Coins without an eligible Binance market
//! (stocks, gold, unlisted tokens) are listed under `dropped`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The only feed format the trader accepts.
pub const FEED_SCHEMA: &str = "biz-feed-v1";
/// Default `BOT_SPOT_BIZ_FEED_PATH`.
pub const DEFAULT_FEED_PATH: &str = "data/biz-feed.json";
/// Default `BOT_SPOT_BIZ_MAX_AGE_MINUTES`: biz-app updates every 15 minutes and the copy
/// runs every 5.
pub const DEFAULT_MAX_AGE_MINUTES: u64 = 45;
/// An `as_of` further ahead of the trader's clock than this is refused.
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;

/// One coin's numbers over one window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    /// Posts naming the coin (every post, no AI).
    pub mentions: u64,
    /// Distinct posters among them.
    pub posters: u64,
    /// AI bullish tags weighted by post score (1-3).
    pub bullish: f64,
    /// AI bearish tags weighted by post score (1-3).
    pub bearish: f64,
}

/// One coin's windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoinWindows {
    #[serde(rename = "1h")]
    pub h1: Window,
    #[serde(rename = "6h")]
    pub h6: Window,
    #[serde(rename = "24h")]
    pub h24: Window,
}

#[derive(Debug, Deserialize)]
struct FeedFile {
    schema: String,
    as_of: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt_version: String,
    #[serde(default)]
    coins: BTreeMap<String, CoinWindows>,
}

/// A fresh feed as the cycle records it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BizFeed {
    pub as_of: String,
    /// Minutes between `as_of` and the read.
    pub age_minutes: i64,
    /// The AI that tagged the stances, and its instruction version.
    pub model: String,
    pub prompt_version: String,
    /// Coins with an eligible Binance market, by base asset.
    pub coins: BTreeMap<String, CoinWindows>,
    /// Coins the feed names without an eligible Binance market.
    pub dropped: Vec<String>,
}

/// What the cycle knows about /biz/.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BizNow {
    Fresh(Box<BizFeed>),
    Unavailable { reason: String },
}

/// A feed checked for its schema and `as_of`, before the age and market checks. Coin names
/// are trimmed and upper-cased.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFeed {
    pub as_of: String,
    pub as_of_ms: i64,
    pub model: String,
    pub prompt_version: String,
    pub coins: BTreeMap<String, CoinWindows>,
}

/// Parse a `biz-feed-v1` file; `Err` holds the reason it cannot be used.
pub fn parse_raw(text: &str) -> Result<RawFeed, String> {
    let file: FeedFile =
        serde_json::from_str(text).map_err(|e| format!("not a /biz/ feed: {e}"))?;
    if file.schema != FEED_SCHEMA {
        return Err(format!(
            "schema {:?}, expected {FEED_SCHEMA}",
            file.schema
        ));
    }
    let as_of = chrono::DateTime::parse_from_rfc3339(file.as_of.trim())
        .map_err(|e| format!("as_of {:?}: {e}", file.as_of))?;
    Ok(RawFeed {
        as_of_ms: as_of.timestamp_millis(),
        as_of: file.as_of,
        model: file.model,
        prompt_version: file.prompt_version,
        coins: file
            .coins
            .into_iter()
            .map(|(coin, windows)| (coin.trim().to_ascii_uppercase(), windows))
            .collect(),
    })
}

/// Parse a feed read at `now_ms`; `Err` holds the reason it cannot be used. `tradable`
/// says whether a base asset has an eligible Binance market.
pub fn parse(
    text: &str,
    now_ms: i64,
    max_age_minutes: u64,
    tradable: impl Fn(&str) -> bool,
) -> Result<BizFeed, String> {
    let raw = parse_raw(text)?;
    let age_ms = now_ms - raw.as_of_ms;
    if age_ms < -MAX_CLOCK_SKEW_MS {
        return Err(format!("as_of {} is ahead of this clock", raw.as_of));
    }
    let age_minutes = age_ms.max(0) / 60_000;
    if age_minutes.unsigned_abs() > max_age_minutes {
        return Err(format!(
            "stale: as_of {} is {age_minutes} min old (limit {max_age_minutes})",
            raw.as_of
        ));
    }
    // `raw.coins` is sorted and unique, so `dropped` is too.
    let mut coins = BTreeMap::new();
    let mut dropped = Vec::new();
    for (coin, windows) in raw.coins {
        if tradable(&coin) {
            coins.insert(coin, windows);
        } else {
            dropped.push(coin);
        }
    }
    Ok(BizFeed {
        as_of: raw.as_of,
        age_minutes,
        model: raw.model,
        prompt_version: raw.prompt_version,
        coins,
        dropped,
    })
}

/// Read the feed at `path` for this cycle.
pub fn read(
    path: &Path,
    now_ms: i64,
    max_age_minutes: u64,
    tradable: impl Fn(&str) -> bool,
) -> BizNow {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("{}: {e}", path.display()))
        .and_then(|text| parse(&text, now_ms, max_age_minutes, tradable));
    match parsed {
        Ok(feed) => BizNow::Fresh(Box::new(feed)),
        Err(reason) => {
            tracing::info!(%reason, "/biz/ feed unavailable this cycle");
            BizNow::Unavailable { reason }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AS_OF_MS: i64 = 1_792_490_400_000; // 2026-10-20T10:00:00Z
    const FEED: &str = r#"{"schema":"biz-feed-v1","as_of":"2026-10-20T10:00:00Z","model":"gemini","prompt_version":"p1",
        "coins":{"xmr":{"6h":{"mentions":7,"posters":4,"bullish":3.0,"bearish":1.0,"posts":[62804123]}},
                 "NVDA":{"24h":{"mentions":2}},"BTC":{"1h":{"mentions":1}}}}"#;

    fn listed(coin: &str) -> bool {
        matches!(coin, "BTC" | "XMR")
    }

    #[test]
    fn a_fresh_feed_keeps_only_coins_with_a_binance_market() {
        let feed = parse(FEED, AS_OF_MS + 20 * 60_000, 45, listed).unwrap();
        assert_eq!(feed.age_minutes, 20);
        assert_eq!(feed.coins.keys().collect::<Vec<_>>(), ["BTC", "XMR"]);
        assert_eq!(feed.coins["XMR"].h6.posters, 4);
        assert_eq!(feed.dropped, ["NVDA"]);
        assert_eq!((feed.model.as_str(), feed.prompt_version.as_str()), ("gemini", "p1"));
    }

    #[test]
    fn a_stale_future_or_foreign_feed_is_unavailable_with_its_reason() {
        let stale = parse(FEED, AS_OF_MS + 46 * 60_000, 45, listed).unwrap_err();
        assert!(stale.contains("stale"), "{stale}");
        let ahead = parse(FEED, AS_OF_MS - 6 * 60_000, 45, listed).unwrap_err();
        assert!(ahead.contains("ahead"), "{ahead}");
        let other = FEED.replace("biz-feed-v1", "biz-feed-v2");
        assert!(parse(&other, AS_OF_MS, 45, listed).unwrap_err().contains("expected biz-feed-v1"));

        let missing = read(Path::new("/nonexistent/biz-feed.json"), AS_OF_MS, 45, listed);
        let value = serde_json::to_value(&missing).unwrap();
        assert_eq!(value["status"], "unavailable");
        assert!(value["reason"].as_str().unwrap().contains("/nonexistent/biz-feed.json"));
    }

    #[test]
    fn a_real_biz_app_feed_parses_with_its_extra_fields() {
        // `biz-insights feed --at 2026-10-25T13:00:00Z` on the real archive (biz-app `feed.py`).
        let text = include_str!("../tests/fixtures/biz-feed-v1.json");
        let raw = parse_raw(text).unwrap();
        assert_eq!(raw.as_of_ms, 1_792_933_200_000);
        assert_eq!(raw.prompt_version, "stance-1");
        let btc = raw.coins["BTC"];
        assert_eq!((btc.h24.mentions, btc.h24.posters), (34, 32));
        assert!(btc.h6.mentions <= btc.h24.mentions && btc.h1.mentions <= btc.h6.mentions);
        let feed = parse(text, raw.as_of_ms + 10 * 60_000, 45, |c| c != "QNT").unwrap();
        assert!(feed.coins.contains_key("XRP") && feed.dropped == ["QNT"]);
    }
}

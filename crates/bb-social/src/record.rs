//! Privacy-reduced social records and the heuristic text classifier.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

const BULLISH_TERMS: &[&str] = &[
    "buy",
    "bull",
    "breakout",
    "moon",
    "gem",
    "long",
    "accumulate",
    "listing",
];
const BEARISH_TERMS: &[&str] = &[
    "dump", "sell", "exit", "crash", "tank", "rug", "scam", "drain",
];
const MANIPULATION_TERMS: &[&str] = &[
    "pump",
    "pump group",
    "pump and dump",
    "signal group",
    "guaranteed",
    "100x",
    "1000x",
    "ape now",
    "buy now",
    "coordinate",
    "rug",
    "scam",
];
const EXCERPT_CHARS: usize = 280;
const MAX_URLS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SocialSource {
    Telegram,
    X,
}

impl SocialSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::X => "x",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "String")]
pub enum SocialLabel {
    BullishCall,
    BearishWarning,
    ManipulationRisk,
    #[default]
    NeutralMention,
}

impl SocialLabel {
    pub const ALL: [Self; 4] = [
        Self::BullishCall,
        Self::BearishWarning,
        Self::ManipulationRisk,
        Self::NeutralMention,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BullishCall => "bullish_call",
            Self::BearishWarning => "bearish_warning",
            Self::ManipulationRisk => "manipulation_risk",
            Self::NeutralMention => "neutral_mention",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|label| label.as_str() == value)
    }
}

impl From<String> for SocialLabel {
    /// Stored labels outside the known set are treated as neutral mentions.
    fn from(value: String) -> Self {
        Self::parse(&value).unwrap_or_default()
    }
}

/// Privacy-reduced record derived from one permitted social post. Field order is
/// alphabetical so each JSONL line has sorted keys, like the Python writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocialSignalRecord {
    #[serde(default, deserialize_with = "null_default")]
    pub assets: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub author_hash: String,
    #[serde(default, deserialize_with = "null_default")]
    pub channel_hash: String,
    #[serde(default, deserialize_with = "null_default")]
    pub confidence: f64,
    #[serde(default, deserialize_with = "null_default")]
    pub engagement: BTreeMap<String, i64>,
    #[serde(default, deserialize_with = "null_default")]
    pub excerpt: String,
    #[serde(default, deserialize_with = "null_default")]
    pub label: SocialLabel,
    #[serde(default, deserialize_with = "null_default")]
    pub manipulation_risk: f64,
    #[serde(default, deserialize_with = "null_default")]
    pub message_id: String,
    pub source: SocialSource,
    pub timestamp: String,
    #[serde(default, deserialize_with = "null_default")]
    pub urls: Vec<String>,
}

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Raw post fields before classification and redaction.
#[derive(Debug, Clone)]
pub(crate) struct RawPost<'a> {
    pub source: SocialSource,
    pub message_id: String,
    pub timestamp: String,
    pub author_key: &'a str,
    pub channel_key: &'a str,
    pub text: &'a str,
    pub assets: Vec<String>,
    pub engagement: BTreeMap<String, i64>,
}

impl SocialSignalRecord {
    pub(crate) fn from_post(post: RawPost<'_>) -> Self {
        let (label, confidence, manipulation_risk) = classify_text(post.text);
        let excerpt = post
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(EXCERPT_CHARS)
            .collect();
        Self {
            assets: post.assets,
            author_hash: hash_identity(post.author_key),
            channel_hash: hash_identity(post.channel_key),
            confidence,
            engagement: post.engagement,
            excerpt,
            label,
            manipulation_risk,
            message_id: post.message_id,
            source: post.source,
            timestamp: post.timestamp,
            urls: extract_urls(post.text, MAX_URLS),
        }
    }
}

/// Keyword heuristic: `(label, confidence, manipulation_risk)`.
pub fn classify_text(text: &str) -> (SocialLabel, f64, f64) {
    let lowered = text.to_lowercase();
    let hits = |terms: &[&str]| terms.iter().filter(|t| lowered.contains(*t)).count() as f64;
    let manipulation = hits(MANIPULATION_TERMS);
    let bullish = hits(BULLISH_TERMS);
    let bearish = hits(BEARISH_TERMS);
    if manipulation > 0.0 {
        let score = (0.65 + manipulation * 0.12).min(1.0);
        return (SocialLabel::ManipulationRisk, score, score);
    }
    if bullish > bearish {
        return (
            SocialLabel::BullishCall,
            (0.55 + bullish * 0.1).min(0.95),
            0.0,
        );
    }
    if bearish > bullish {
        return (
            SocialLabel::BearishWarning,
            (0.55 + bearish * 0.1).min(0.95),
            0.2,
        );
    }
    (SocialLabel::NeutralMention, 0.4, 0.0)
}

/// Normalized tickers from `assets` mentioned in `text` as standalone tokens
/// (optionally `$`/`#` prefixed), deduplicated in input order.
pub fn extract_assets<S: AsRef<str>>(text: &str, assets: &[S]) -> Vec<String> {
    let haystack = text.to_uppercase();
    let mut found: Vec<String> = Vec::new();
    for asset in assets {
        let normalized = asset.as_ref().trim().to_uppercase();
        if normalized.is_empty() || found.contains(&normalized) {
            continue;
        }
        if mentions_token(&haystack, &normalized) {
            found.push(normalized);
        }
    }
    found
}

fn is_ticker_char(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

fn mentions_token(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(needle) {
        let at = start + offset;
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + needle.len()..].chars().next();
        if !before.is_some_and(is_ticker_char) && !after.is_some_and(is_ticker_char) {
            return true;
        }
        start = at + haystack[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Up to `max` `http(s)://` URLs, ending at whitespace or `<>()`.
pub fn extract_urls(text: &str, max: usize) -> Vec<String> {
    let mut urls = Vec::new();
    let mut at = 0;
    while at < text.len() && urls.len() < max {
        let rest = &text[at..];
        if let Some(len) = url_len(rest) {
            urls.push(rest[..len].to_owned());
            at += len;
        } else {
            at += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    urls
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn url_len(s: &str) -> Option<usize> {
    if !starts_with_ignore_case(s, "http") {
        return None;
    }
    let scheme = if starts_with_ignore_case(&s[4..], "s://") {
        8
    } else if s[4..].starts_with("://") {
        7
    } else {
        return None;
    };
    let body: usize = s[scheme..]
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '<' | '>' | '(' | ')'))
        .map(char::len_utf8)
        .sum();
    (body > 0).then_some(scheme + body)
}

/// SHA-256 prefix used instead of raw author/channel identifiers.
pub fn hash_identity(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)[..16].to_owned()
}

/// Python `datetime.isoformat()` style UTC timestamp (`+00:00` offset).
pub fn isoformat(ts: DateTime<Utc>) -> String {
    let format = if ts.timestamp_subsec_nanos() == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    ts.to_rfc3339_opts(format, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn classifier_scores_manipulation_bullish_bearish_and_neutral() {
        let (label, confidence, risk) = classify_text("$BONK pump group says buy now 100x");
        assert_eq!(label, SocialLabel::ManipulationRisk);
        // pump, pump group, 100x, buy now → 0.65 + 4 * 0.12 capped at 1.0.
        assert!((confidence - 1.0).abs() < 1e-12 && (risk - 1.0).abs() < 1e-12);
        let (label, confidence, risk) = classify_text("EDEN breakout after listing");
        assert_eq!(label, SocialLabel::BullishCall);
        assert!((confidence - 0.75).abs() < 1e-12 && risk.abs() < 1e-12);
        let (label, _, risk) = classify_text("time to exit, it will crash");
        assert_eq!(label, SocialLabel::BearishWarning);
        assert!((risk - 0.2).abs() < 1e-12);
        assert_eq!(
            classify_text("BTC is a coin").0,
            SocialLabel::NeutralMention
        );
    }

    #[test]
    fn asset_extraction_requires_token_boundaries() {
        assert_eq!(
            extract_assets(
                "$eth and #sol, not ETHER or SOLANA",
                &["ETH", "sol", "ETH", "", "BTC"]
            ),
            vec!["ETH", "SOL"]
        );
        assert!(extract_assets("WBTC2 BTCB", &["BTC"]).is_empty());
        assert_eq!(extract_assets("é BTC", &["BTC"]), vec!["BTC"]);
    }

    #[test]
    fn url_extraction_stops_at_delimiters_and_caps_count() {
        let text =
            "see (https://a.io/x) http://b.io<c> HTTPS://c.io d https://d.io https://e.io http://";
        assert_eq!(
            extract_urls(text, 4),
            vec![
                "https://a.io/x",
                "http://b.io",
                "HTTPS://c.io",
                "https://d.io"
            ]
        );
    }

    #[test]
    fn excerpt_collapses_whitespace_and_truncates() {
        let text = format!("BTC\n\n  {}", "x".repeat(400));
        let record = SocialSignalRecord::from_post(RawPost {
            source: SocialSource::X,
            message_id: "1".into(),
            timestamp: "t".into(),
            author_key: "",
            channel_key: "public_search",
            text: &text,
            assets: vec!["BTC".into()],
            engagement: BTreeMap::new(),
        });
        assert_eq!(record.excerpt.chars().count(), 280);
        assert!(record.excerpt.starts_with("BTC x"));
        assert_eq!(record.author_hash, "");
    }

    #[test]
    fn record_round_trips_with_sorted_keys_and_tolerates_nulls() {
        let line = r#"{"source": "telegram", "timestamp": "2026-01-01T00:00:00+00:00", "label": "weird", "urls": null, "engagement": {"like_count": 3}}"#;
        let record: SocialSignalRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.label, SocialLabel::NeutralMention);
        assert!(record.urls.is_empty());
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.starts_with(r#"{"assets":[],"author_hash":"","channel_hash":"","confidence":0.0,"engagement":{"like_count":3}"#));
        assert!(json.ends_with(
            r#""source":"telegram","timestamp":"2026-01-01T00:00:00+00:00","urls":[]}"#
        ));
    }

    #[test]
    fn isoformat_matches_python() {
        let ts = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(isoformat(ts), "2023-11-14T22:13:20+00:00");
        let ts = DateTime::from_timestamp(1_700_000_000, 5_000).unwrap();
        assert_eq!(isoformat(ts), "2023-11-14T22:13:20.000005+00:00");
    }
}

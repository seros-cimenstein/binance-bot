//! Parsers for the official Telegram Bot API and X recent-search payloads.
//!
//! Payloads are genuinely dynamic (optional fields, mixed types), so they are walked
//! as `serde_json::Value` with Python-equivalent truthiness rules.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::record::{RawPost, SocialSignalRecord, SocialSource, extract_assets, isoformat};

/// Records from Telegram `getUpdates` messages/channel posts mentioning `assets`.
pub fn parse_telegram_updates<S: AsRef<str>>(
    payload: &Value,
    assets: &[S],
    now: DateTime<Utc>,
) -> Vec<SocialSignalRecord> {
    let Some(updates) = payload.get("result").and_then(Value::as_array) else {
        return Vec::new();
    };
    updates
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|update| {
            let message = truthy(update.get("channel_post"))
                .or_else(|| update.get("message"))
                .and_then(Value::as_object)?;
            let text = first_text(message, &["text", "caption"]);
            let text = text.trim();
            let mentioned = extract_assets(text, assets);
            if text.is_empty() || mentioned.is_empty() {
                return None;
            }
            let chat = object(message, "chat");
            let author = object(message, "from");
            let chat_id = chat.and_then(|c| truthy_string(c.get("id")));
            let author_key = author
                .and_then(|a| truthy_string(a.get("id")))
                .or_else(|| chat_id.clone())
                .unwrap_or_default();
            let channel_key = chat_id.unwrap_or_default();
            Some(SocialSignalRecord::from_post(RawPost {
                source: SocialSource::Telegram,
                message_id: truthy_string(message.get("message_id"))
                    .or_else(|| truthy_string(update.get("update_id")))
                    .unwrap_or_default(),
                timestamp: unix_timestamp(message.get("date"), now),
                author_key: &author_key,
                channel_key: &channel_key,
                text,
                assets: mentioned,
                engagement: BTreeMap::new(),
            }))
        })
        .collect()
}

/// Records from an X API v2 `tweets/search/recent` response mentioning `assets`.
pub fn parse_x_recent_search<S: AsRef<str>>(
    payload: &Value,
    assets: &[S],
    now: DateTime<Utc>,
) -> Vec<SocialSignalRecord> {
    let Some(posts) = payload.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    posts
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|post| {
            let text = first_text(post, &["text"]);
            let text = text.trim();
            let mentioned = extract_assets(text, assets);
            if text.is_empty() || mentioned.is_empty() {
                return None;
            }
            let author_key = truthy_string(post.get("author_id")).unwrap_or_default();
            Some(SocialSignalRecord::from_post(RawPost {
                source: SocialSource::X,
                message_id: truthy_string(post.get("id")).unwrap_or_default(),
                timestamp: truthy_string(post.get("created_at")).unwrap_or_else(|| isoformat(now)),
                author_key: &author_key,
                channel_key: "public_search",
                text,
                assets: mentioned,
                engagement: engagement(post.get("public_metrics")),
            }))
        })
        .collect()
}

fn engagement(metrics: Option<&Value>) -> BTreeMap<String, i64> {
    metrics
        .and_then(Value::as_object)
        .map(|metrics| {
            metrics
                .iter()
                .filter_map(|(key, value)| {
                    let n = value.as_number()?;
                    // `int(value)`: floats truncate toward zero.
                    let count = n.as_i64().or_else(|| {
                        n.as_f64()
                            .filter(|f| f.is_finite())
                            .map(|f| f.trunc() as i64)
                    })?;
                    Some((key.clone(), count))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn object<'a>(parent: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    parent.get(key).and_then(Value::as_object)
}

/// `str(a or b or "")` over the given keys.
fn first_text(obj: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| truthy_string(obj.get(*key)))
        .unwrap_or_default()
}

/// Python truthiness of a JSON value.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn truthy(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| is_truthy(v))
}

/// `str(value)` for truthy values.
fn truthy_string(value: Option<&Value>) -> Option<String> {
    truthy(value).map(|value| match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// Unix seconds → ISO timestamp; missing or invalid input falls back to `now`.
fn unix_timestamp(value: Option<&Value>, now: DateTime<Utc>) -> String {
    let seconds = match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    let ts = seconds
        .filter(|s| s.is_finite())
        .and_then(|s| {
            let whole = s.floor();
            let micros = ((s - whole) * 1_000_000.0).round() as u32;
            DateTime::from_timestamp(whole as i64, micros * 1_000)
        })
        .unwrap_or(now);
    isoformat(ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::SocialLabel;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn telegram_updates_redact_identity_and_classify_risk() {
        let payload = json!({"result": [{
            "update_id": 1,
            "message": {
                "message_id": 22,
                "date": 1_700_000_000,
                "text": "$BONK pump group says buy now 100x",
                "from": {"id": 12345},
                "chat": {"id": -999},
            },
        }]});
        let records = parse_telegram_updates(&payload, &["BONK", "ETH"], now());
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.source, SocialSource::Telegram);
        assert_eq!(record.assets, vec!["BONK"]);
        assert_eq!(record.message_id, "22");
        assert_eq!(record.timestamp, "2023-11-14T22:13:20+00:00");
        assert_eq!(record.author_hash.len(), 16);
        assert_ne!(record.author_hash, "12345");
        assert_eq!(record.channel_hash.len(), 16);
        assert_ne!(record.channel_hash, "-999");
        assert_ne!(record.author_hash, record.channel_hash);
        assert_eq!(record.label, SocialLabel::ManipulationRisk);
        assert!(record.manipulation_risk >= 0.8);
    }

    #[test]
    fn telegram_falls_back_to_caption_chat_id_and_update_id() {
        let payload = json!({"result": [
            {"update_id": 7, "channel_post": {
                "caption": "ETH looks heavy", "chat": {"id": 55}, "date": "bad"}},
            {"update_id": 8, "message": {"text": "nothing relevant", "chat": {"id": 1}}},
            "junk",
        ]});
        let records = parse_telegram_updates(&payload, &["ETH"], now());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message_id, "7");
        assert_eq!(records[0].author_hash, records[0].channel_hash);
        assert_eq!(records[0].timestamp, isoformat(now()));
    }

    #[test]
    fn x_recent_search_extracts_assets_and_engagement() {
        let payload = json!({"data": [{
            "id": "post-1",
            "author_id": "user-1",
            "created_at": "2026-05-27T12:00:00+00:00",
            "text": "Watching #EDEN breakout after listing rumor",
            "public_metrics": {"retweet_count": 3, "like_count": 9},
        }]});
        let records = parse_x_recent_search(&payload, &["EDEN", "BONK"], now());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, SocialSource::X);
        assert_eq!(records[0].assets, vec!["EDEN"]);
        assert_eq!(records[0].label, SocialLabel::BullishCall);
        assert_eq!(records[0].engagement["like_count"], 9);
        assert_eq!(
            records[0].channel_hash,
            crate::record::hash_identity("public_search")
        );
    }

    #[test]
    fn x_payload_without_data_list_is_empty() {
        assert!(parse_x_recent_search(&json!({"meta": {}}), &["BTC"], now()).is_empty());
    }
}

//! Collection from the permitted official APIs into the local audit file.

use std::collections::HashSet;
use std::time::Duration;

use crate::SocialSignalError;
use crate::analyzer::HeadlessSocialAnalyzer;
use crate::evidence::{load_records, write_audit};
use crate::parse::{parse_telegram_updates, parse_x_recent_search};
use crate::record::{SocialSignalRecord, SocialSource};
use crate::settings::SocialSettings;

pub const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
pub const X_API_BASE: &str = "https://api.x.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// `getUpdates` offset returning the newest 100 updates. Without it Telegram keeps
/// replaying the oldest 100 unconfirmed updates and new posts are never seen.
const TELEGRAM_NEWEST_UPDATES_OFFSET: &str = "-100";

/// Reads Telegram/X with the configured credentials and appends new records.
#[derive(Debug, Clone)]
pub struct SocialCollector {
    http: reqwest::Client,
    telegram_base: String,
    x_base: String,
}

impl SocialCollector {
    pub fn new() -> Result<Self, SocialSignalError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self::with_endpoints(http, TELEGRAM_API_BASE, X_API_BASE))
    }

    /// Custom HTTP client and API bases (used by tests).
    pub fn with_endpoints(http: reqwest::Client, telegram_base: &str, x_base: &str) -> Self {
        Self {
            http,
            telegram_base: telegram_base.trim_end_matches('/').to_owned(),
            x_base: x_base.trim_end_matches('/').to_owned(),
        }
    }

    /// Collect enabled sources, optionally reclassify with AI, and append records not
    /// already stored. Returns the newly collected records (empty when disabled).
    pub async fn collect<S: AsRef<str>>(
        &self,
        settings: &SocialSettings,
        assets: &[S],
    ) -> Result<Vec<SocialSignalRecord>, SocialSignalError> {
        if !settings.enabled {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        if settings.telegram_enabled {
            records.extend(self.telegram(settings, assets).await?);
        }
        if settings.x_enabled {
            records.extend(self.x(settings, assets).await?);
        }
        if settings.ai_enabled && !records.is_empty() {
            records = HeadlessSocialAnalyzer::new(settings)
                .classify(records)
                .await;
        }
        let fresh = drop_already_stored(records, &load_records(&settings.audit_path)?);
        write_audit(&settings.audit_path, &fresh)?;
        Ok(fresh)
    }

    async fn telegram<S: AsRef<str>>(
        &self,
        settings: &SocialSettings,
        assets: &[S],
    ) -> Result<Vec<SocialSignalRecord>, SocialSignalError> {
        let token = settings.telegram_bot_token.trim();
        if token.is_empty() {
            return Err(SocialSignalError::MissingCredentials(
                "Telegram collection enabled without bot token",
            ));
        }
        let payload = self
            .http
            .get(format!("{}/bot{token}/getUpdates", self.telegram_base))
            .query(&[
                ("timeout", "0"),
                ("offset", TELEGRAM_NEWEST_UPDATES_OFFSET),
                ("allowed_updates", r#"["message", "channel_post"]"#),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_telegram_updates(
            &payload,
            assets,
            bb_core::time::now(),
        ))
    }

    async fn x<S: AsRef<str>>(
        &self,
        settings: &SocialSettings,
        assets: &[S],
    ) -> Result<Vec<SocialSignalRecord>, SocialSignalError> {
        let token = settings.x_bearer_token.trim();
        let query = settings.x_query.trim();
        if token.is_empty() || query.is_empty() {
            return Err(SocialSignalError::MissingCredentials(
                "X collection enabled without bearer token/query",
            ));
        }
        let payload = self
            .http
            .get(format!("{}/2/tweets/search/recent", self.x_base))
            .bearer_auth(token)
            .query(&[
                ("query", query),
                ("max_results", "100"),
                ("tweet.fields", "created_at,author_id,public_metrics"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_x_recent_search(
            &payload,
            assets,
            bb_core::time::now(),
        ))
    }
}

type PostKey = (SocialSource, String, String);

fn post_key(record: &SocialSignalRecord) -> Option<PostKey> {
    (!record.message_id.is_empty()).then(|| {
        (
            record.source,
            record.channel_hash.clone(),
            record.message_id.clone(),
        )
    })
}

/// Repeated polls return overlapping posts; store each identifiable post once.
fn drop_already_stored(
    records: Vec<SocialSignalRecord>,
    stored: &[SocialSignalRecord],
) -> Vec<SocialSignalRecord> {
    let mut seen: HashSet<PostKey> = stored.iter().filter_map(post_key).collect();
    records
        .into_iter()
        .filter(|record| post_key(record).is_none_or(|key| seen.insert(key)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::SocialLabel;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn settings(dir: &std::path::Path) -> SocialSettings {
        SocialSettings {
            enabled: true,
            telegram_enabled: true,
            x_enabled: true,
            audit_path: dir.join("social.jsonl"),
            telegram_bot_token: " tok ".into(),
            x_bearer_token: "bearer".into(),
            x_query: "crypto".into(),
            ..SocialSettings::default()
        }
    }

    async fn mount_sources(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/bottok/getUpdates"))
            .and(query_param("timeout", "0"))
            .and(query_param(
                "allowed_updates",
                r#"["message", "channel_post"]"#,
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": [
                    {"update_id": 1, "message": {"message_id": 5, "date": 1_700_000_000,
                        "text": "$BTC breakout", "chat": {"id": 9}}},
                ]})),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2/tweets/search/recent"))
            .and(header("authorization", "Bearer bearer"))
            .and(query_param("query", "crypto"))
            .and(query_param("max_results", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"id": "t1", "author_id": "u1", "created_at": "2026-05-27T12:00:00Z",
                    "text": "BTC crash incoming, sell", "public_metrics": {"like_count": 2}},
            ]})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn collects_both_sources_and_stores_each_post_once() {
        let server = MockServer::start().await;
        mount_sources(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path());
        let collector =
            SocialCollector::with_endpoints(reqwest::Client::new(), &server.uri(), &server.uri());

        let first = collector.collect(&settings, &["BTC"]).await.unwrap();
        assert_eq!(
            first
                .iter()
                .map(|r| (r.source, r.label))
                .collect::<Vec<_>>(),
            vec![
                (SocialSource::Telegram, SocialLabel::BullishCall),
                (SocialSource::X, SocialLabel::BearishWarning),
            ]
        );
        let second = collector.collect(&settings, &["BTC"]).await.unwrap();
        assert!(second.is_empty());
        assert_eq!(load_records(&settings.audit_path).unwrap(), first);
    }

    #[tokio::test]
    async fn disabled_collection_makes_no_requests_or_writes() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SocialSettings {
            enabled: false,
            ..settings(dir.path())
        };
        let collector = SocialCollector::with_endpoints(
            reqwest::Client::new(),
            "http://127.0.0.1:9",
            "http://127.0.0.1:9",
        );
        assert!(
            collector
                .collect(&settings, &["BTC"])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!settings.audit_path.exists());
    }

    #[tokio::test]
    async fn enabled_source_without_credentials_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SocialSettings {
            telegram_enabled: false,
            x_query: String::new(),
            ..settings(dir.path())
        };
        let collector = SocialCollector::new().unwrap();
        let err = collector.collect(&settings, &["BTC"]).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "X collection enabled without bearer token/query"
        );
    }

    #[tokio::test]
    async fn http_errors_do_not_leak_the_bot_token() {
        let server = MockServer::start().await;
        Mock::given(path("/botsecret-token/getUpdates"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let settings = SocialSettings {
            x_enabled: false,
            telegram_bot_token: "secret-token".into(),
            ..settings(dir.path())
        };
        let collector =
            SocialCollector::with_endpoints(reqwest::Client::new(), &server.uri(), &server.uri());
        let err = collector.collect(&settings, &["BTC"]).await.unwrap_err();
        assert!(!format!("{err} {err:?}").contains("secret-token"));
    }
}

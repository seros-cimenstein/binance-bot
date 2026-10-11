//! Stored social records → per-asset evidence for route selection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use serde::Serialize;

use crate::SocialSignalError;
use crate::record::{SocialLabel, SocialSignalRecord, SocialSource};
use crate::settings::SocialSettings;

/// Only the newest lines of the audit file are considered.
const MAX_AUDIT_LINES: usize = 2000;
const RECENT_SIGNALS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceStatus {
    /// Social signals are disabled (`SOCIAL_SIGNAL_ENABLED` unset/false).
    Disabled,
    /// Enabled, but no recent stored record mentions the asset.
    Empty,
    /// At least one recent record mentions the asset.
    Ready,
}

/// One recent record as exposed to the trader prompt.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecentSignal {
    pub confidence: f64,
    pub excerpt: String,
    pub label: SocialLabel,
    pub source: SocialSource,
    pub timestamp: String,
}

/// Aggregates over recent records for one asset (present unless disabled). Fields are
/// alphabetical so the serialized object has sorted keys, matching the Python output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssetSocialSummary {
    pub asset: String,
    pub bearish_mentions: usize,
    /// True when manipulation-risk blocking is on and the score reached the threshold.
    pub block_route: bool,
    pub bullish_bearish_balance: i64,
    pub bullish_mentions: usize,
    /// Highest manipulation risk in `[0, 1]` among the recent records.
    pub manipulation_risk_score: f64,
    pub mention_count: usize,
    pub min_source_diversity_met: bool,
    pub recent_signals: Vec<RecentSignal>,
    pub source_diversity: usize,
    pub source_types: Vec<SocialSource>,
}

/// Social evidence for one asset; serializes to the same object the Python module
/// produced (`{"status": "disabled"}` or the full summary plus `status`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SocialEvidence {
    #[serde(flatten)]
    pub summary: Option<AssetSocialSummary>,
    pub status: EvidenceStatus,
}

impl SocialEvidence {
    pub const DISABLED: Self = Self {
        summary: None,
        status: EvidenceStatus::Disabled,
    };

    /// Whether a configured manipulation-risk block removes routes into this asset.
    pub fn blocks_route(&self) -> bool {
        self.summary.as_ref().is_some_and(|s| s.block_route)
    }

    pub fn mention_count(&self) -> usize {
        self.summary.as_ref().map_or(0, |s| s.mention_count)
    }

    pub fn manipulation_risk_score(&self) -> f64 {
        self.summary
            .as_ref()
            .map_or(0.0, |s| s.manipulation_risk_score)
    }
}

/// Summarize recent stored records for each (normalized, deduplicated) asset.
pub fn evidence_for_assets<I, S>(
    settings: &SocialSettings,
    assets: I,
    now: DateTime<Utc>,
) -> Result<BTreeMap<String, SocialEvidence>, SocialSignalError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let assets: BTreeSet<String> = assets
        .into_iter()
        .map(|a| a.as_ref().trim().to_uppercase())
        .filter(|a| !a.is_empty())
        .collect();
    if !settings.enabled {
        return Ok(assets
            .into_iter()
            .map(|asset| (asset, SocialEvidence::DISABLED))
            .collect());
    }
    let rows = load_records(&settings.audit_path)?;
    let since = now - Duration::minutes(i64::from(settings.max_age_minutes));
    Ok(assets
        .into_iter()
        .map(|asset| {
            let evidence = summarize_asset(settings, &rows, &asset, now, since);
            (asset, evidence)
        })
        .collect())
}

/// [`evidence_for_assets`] at the current time.
pub fn load_evidence<I, S>(
    settings: &SocialSettings,
    assets: I,
) -> Result<BTreeMap<String, SocialEvidence>, SocialSignalError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    evidence_for_assets(settings, assets, bb_core::time::now())
}

fn summarize_asset(
    settings: &SocialSettings,
    rows: &[SocialSignalRecord],
    asset: &str,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> SocialEvidence {
    // Unparseable timestamps count as current, like the Python reference.
    let mut matching: Vec<&SocialSignalRecord> = rows
        .iter()
        .filter(|row| row.assets.iter().any(|a| a == asset))
        .filter(|row| parse_timestamp(&row.timestamp).unwrap_or(now) >= since)
        .collect();
    if settings.max_records_per_asset > 0 {
        let skip = matching
            .len()
            .saturating_sub(settings.max_records_per_asset);
        matching.drain(..skip);
    }
    let source_types: Vec<SocialSource> = matching
        .iter()
        .map(|row| row.source)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let manipulation = matching
        .iter()
        .map(|row| row.manipulation_risk)
        .fold(0.0_f64, f64::max);
    let count = |label| matching.iter().filter(|row| row.label == label).count();
    let bullish = count(SocialLabel::BullishCall);
    let bearish = count(SocialLabel::BearishWarning);
    let recent_signals = matching[matching.len().saturating_sub(RECENT_SIGNALS)..]
        .iter()
        .map(|row| RecentSignal {
            confidence: row.confidence,
            excerpt: row.excerpt.clone(),
            label: row.label,
            source: row.source,
            timestamp: row.timestamp.clone(),
        })
        .collect();
    SocialEvidence {
        status: if matching.is_empty() {
            EvidenceStatus::Empty
        } else {
            EvidenceStatus::Ready
        },
        summary: Some(AssetSocialSummary {
            asset: asset.to_owned(),
            bearish_mentions: bearish,
            block_route: settings.block_manipulation_risk
                && manipulation >= settings.manipulation_block_score,
            bullish_bearish_balance: bullish as i64 - bearish as i64,
            bullish_mentions: bullish,
            manipulation_risk_score: manipulation,
            mention_count: matching.len(),
            min_source_diversity_met: source_types.len() >= settings.min_source_diversity,
            recent_signals,
            source_diversity: source_types.len(),
            source_types,
        }),
    }
}

/// Well-formed records among the newest audit lines (missing file = none).
pub fn load_records(path: &Path) -> Result<Vec<SocialSignalRecord>, SocialSignalError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SocialSignalError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    Ok(lines[lines.len().saturating_sub(MAX_AUDIT_LINES)..]
        .iter()
        .filter_map(|line| serde_json::from_str::<SocialSignalRecord>(line).ok())
        .map(|mut record| {
            for asset in &mut record.assets {
                *asset = asset.to_uppercase();
            }
            record
        })
        .collect())
}

/// Append records to the JSONL audit file (fsync'd); no-op for an empty batch.
pub fn write_audit(path: &Path, records: &[SocialSignalRecord]) -> Result<(), SocialSignalError> {
    if records.is_empty() {
        return Ok(());
    }
    let log = bb_core::audit::JsonlLog::new(path);
    for record in records {
        log.append(record)?;
    }
    Ok(())
}

/// ISO-8601 timestamp → UTC; offset-less values are taken as UTC.
fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(ts) = DateTime::parse_from_rfc3339(value) {
        return Some(ts.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Some(naive.and_utc());
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::isoformat;
    use pretty_assertions::assert_eq;

    fn record(
        source: SocialSource,
        id: &str,
        ts: DateTime<Utc>,
        label: SocialLabel,
        confidence: f64,
        risk: f64,
    ) -> SocialSignalRecord {
        SocialSignalRecord {
            assets: vec!["EDEN".into()],
            author_hash: "a".into(),
            channel_hash: "c".into(),
            confidence,
            engagement: BTreeMap::new(),
            excerpt: format!("EDEN post {id}"),
            label,
            manipulation_risk: risk,
            message_id: id.into(),
            source,
            timestamp: isoformat(ts),
            urls: vec![],
        }
    }

    fn enabled(path: &Path) -> SocialSettings {
        SocialSettings {
            enabled: true,
            audit_path: path.to_owned(),
            max_age_minutes: 10,
            min_source_diversity: 2,
            block_manipulation_risk: true,
            manipulation_block_score: 0.8,
            ..SocialSettings::default()
        }
    }

    #[test]
    fn summarizes_recent_records_and_blocks_manipulation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("social.jsonl");
        let now = bb_core::time::now();
        let records = [
            record(
                SocialSource::Telegram,
                "1",
                now,
                SocialLabel::ManipulationRisk,
                0.9,
                0.9,
            ),
            record(
                SocialSource::X,
                "2",
                now - Duration::minutes(1),
                SocialLabel::BullishCall,
                0.7,
                0.0,
            ),
            // Too old for the 10-minute window.
            record(
                SocialSource::X,
                "3",
                now - Duration::minutes(30),
                SocialLabel::BearishWarning,
                0.7,
                0.2,
            ),
        ];
        write_audit(&path, &records).unwrap();
        let evidence = evidence_for_assets(&enabled(&path), ["eden", "BONK", " "], now).unwrap();

        assert_eq!(evidence.keys().collect::<Vec<_>>(), vec!["BONK", "EDEN"]);
        let eden = &evidence["EDEN"];
        assert_eq!(eden.status, EvidenceStatus::Ready);
        let summary = eden.summary.as_ref().unwrap();
        assert_eq!(summary.mention_count, 2);
        assert_eq!(summary.source_diversity, 2);
        assert_eq!(
            summary.source_types,
            vec![SocialSource::Telegram, SocialSource::X]
        );
        assert_eq!(summary.bullish_bearish_balance, 1);
        assert!(summary.min_source_diversity_met);
        assert!(eden.blocks_route());
        assert_eq!(evidence["BONK"].status, EvidenceStatus::Empty);
        assert!(!evidence["BONK"].blocks_route());
    }

    #[test]
    fn disabled_settings_report_disabled_without_reading() {
        let settings = SocialSettings {
            audit_path: "/nonexistent/dir/social.jsonl".into(),
            ..SocialSettings::default()
        };
        let evidence = evidence_for_assets(&settings, ["btc"], bb_core::time::now()).unwrap();
        assert_eq!(
            serde_json::to_string(&evidence).unwrap(),
            r#"{"BTC":{"status":"disabled"}}"#
        );
    }

    #[test]
    fn serialized_evidence_matches_python_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.jsonl");
        let evidence = evidence_for_assets(&enabled(&path), ["BTC"], bb_core::time::now()).unwrap();
        assert_eq!(
            serde_json::to_string(&evidence["BTC"]).unwrap(),
            concat!(
                r#"{"asset":"BTC","bearish_mentions":0,"block_route":false,"bullish_bearish_balance":0,"#,
                r#""bullish_mentions":0,"manipulation_risk_score":0.0,"mention_count":0,"#,
                r#""min_source_diversity_met":false,"recent_signals":[],"source_diversity":0,"#,
                r#""source_types":[],"status":"empty"}"#
            )
        );
    }

    #[test]
    fn keeps_newest_records_per_asset_and_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("social.jsonl");
        let now = bb_core::time::now();
        let records: Vec<_> = (0..5)
            .map(|i| {
                record(
                    SocialSource::X,
                    &i.to_string(),
                    now,
                    SocialLabel::NeutralMention,
                    0.4,
                    0.1 * f64::from(i),
                )
            })
            .collect();
        write_audit(&path, &records).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}not json\n{{\"source\": \"x\"}}\n{{\"source\": \"x\", \"timestamp\": \"garbage\", \"assets\": [\"eden\"], \"confidence\": null}}\n",
                std::fs::read_to_string(&path).unwrap()
            ),
        )
        .unwrap();
        let settings = SocialSettings {
            max_records_per_asset: 3,
            ..enabled(&path)
        };
        let evidence = evidence_for_assets(&settings, ["EDEN"], now).unwrap();
        let summary = evidence["EDEN"].summary.as_ref().unwrap();
        // Records 3 and 4 plus the garbage-timestamp row (treated as current).
        assert_eq!(summary.mention_count, 3);
        assert_eq!(summary.recent_signals.len(), 3);
        assert_eq!(summary.recent_signals[0].excerpt, "EDEN post 3");
        assert!((summary.manipulation_risk_score - 0.4).abs() < 1e-9);
        assert!(!evidence["EDEN"].blocks_route());
    }

    #[test]
    fn parses_python_isoformat_variants() {
        let expected = DateTime::from_timestamp(1_779_883_200, 0).unwrap();
        assert_eq!(parse_timestamp("2026-05-27T12:00:00+00:00"), Some(expected));
        assert_eq!(parse_timestamp("2026-05-27T12:00:00Z"), Some(expected));
        assert_eq!(parse_timestamp("2026-05-27T12:00:00"), Some(expected));
        assert_eq!(parse_timestamp("nope"), None);
    }
}

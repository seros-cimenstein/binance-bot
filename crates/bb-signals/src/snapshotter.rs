//! V2 post-detection snapshots: for every ledger detection, capture the pair's
//! market state at fixed offsets (+5m … +7d) into
//! `data/signals-v2-snapshots.jsonl`. Idempotent: a `(detection_id, offset)`
//! is captured once; `api_error` rows are retried on the next run.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bb_core::env;
use serde_json::{Value, json};

use crate::common::{Clock, append_ledger, load_jsonl_objects};
use crate::dexscreener_api::DexScreenerApi;
use crate::error::SignalsError;
use crate::jsonv::{
    Object, first_max_by, float_or_none, float_or_zero, int_or_zero, nested, object, py_str,
};

pub const SNAPSHOT_OFFSETS_SECONDS: [i64; 7] = [
    5 * 60,
    15 * 60,
    60 * 60,
    6 * 60 * 60,
    24 * 60 * 60,
    3 * 24 * 60 * 60,
    7 * 24 * 60 * 60,
];
/// DexScreener accepts up to 30 comma-separated token addresses per call.
pub const BATCH_SIZE: usize = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotterSettings {
    pub ledger_path: PathBuf,
    pub snapshots_path: PathBuf,
    pub max_snapshots_per_run: usize,
    pub request_timeout_seconds: f64,
    pub snapshot_offsets_seconds: Vec<i64>,
}

impl Default for SnapshotterSettings {
    fn default() -> Self {
        Self {
            ledger_path: "data/signals-v2-ledger.jsonl".into(),
            snapshots_path: "data/signals-v2-snapshots.jsonl".into(),
            max_snapshots_per_run: 500,
            request_timeout_seconds: 10.0,
            snapshot_offsets_seconds: SNAPSHOT_OFFSETS_SECONDS.to_vec(),
        }
    }
}

impl SnapshotterSettings {
    pub fn from_env() -> Result<Self, SignalsError> {
        let d = Self::default();
        Ok(Self {
            ledger_path: env::string(&["SIGNALS_V2_LEDGER_PATH"], "data/signals-v2-ledger.jsonl")
                .into(),
            snapshots_path: env::string(
                &["SIGNALS_V2_SNAPSHOTS_PATH"],
                "data/signals-v2-snapshots.jsonl",
            )
            .into(),
            max_snapshots_per_run: env::integer(
                &["SIGNALS_V2_SNAPSHOTTER_MAX_PER_RUN"],
                d.max_snapshots_per_run,
            )?,
            request_timeout_seconds: d.request_timeout_seconds,
            snapshot_offsets_seconds: env::optional_string(&[
                "SIGNALS_V2_SNAPSHOT_OFFSETS_SECONDS",
            ])
            .map_or(d.snapshot_offsets_seconds, |raw| parse_offsets(&raw)),
        })
    }
}

/// Comma-separated offsets; invalid parts are skipped with a warning and an
/// all-invalid value falls back to the defaults.
fn parse_offsets(raw: &str) -> Vec<i64> {
    let offsets: Vec<i64> = raw
        .split(',')
        .filter_map(|part| match part.trim().parse::<i64>() {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(part, "ignoring invalid snapshot offset");
                None
            }
        })
        .collect();
    if offsets.is_empty() {
        SNAPSHOT_OFFSETS_SECONDS.to_vec()
    } else {
        offsets
    }
}

/// A scheduled snapshot that has no persisted (non-error) row yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueSnapshot {
    pub detection_id: String,
    pub token_address: String,
    pub pair_address: String,
    pub scheduled_offset_seconds: i64,
    pub scheduled_for_ms: i64,
}

pub fn load_detections(path: &Path) -> Vec<Object> {
    load_jsonl_objects(path)
}

pub fn load_snapshots(path: &Path) -> Vec<Object> {
    load_jsonl_objects(path)
}

pub fn compute_due_snapshots(
    detections: &[Object],
    snapshots: &[Object],
    now_ms: i64,
    offsets_seconds: &[i64],
) -> Vec<DueSnapshot> {
    let captured: HashSet<(String, i64)> = snapshots
        .iter()
        .filter(|s| py_str(s.get("status")) != "api_error")
        .map(|s| {
            (
                py_str(s.get("detection_id")),
                int_or_zero(s.get("scheduled_offset_seconds")),
            )
        })
        .collect();
    let mut due = Vec::new();
    for detection in detections {
        let detection_id = py_str(detection.get("detection_id"));
        let token_address = py_str(detection.get("token_address"));
        let pair_address = py_str(detection.get("pair_address"));
        let detected_at_ms = int_or_zero(detection.get("detected_at_ms"));
        if detection_id.is_empty() || token_address.is_empty() || detected_at_ms <= 0 {
            continue;
        }
        for &offset in offsets_seconds {
            let scheduled_for_ms = detected_at_ms + offset * 1000;
            if now_ms >= scheduled_for_ms && !captured.contains(&(detection_id.clone(), offset)) {
                due.push(DueSnapshot {
                    detection_id: detection_id.clone(),
                    token_address: token_address.clone(),
                    pair_address: pair_address.clone(),
                    scheduled_offset_seconds: offset,
                    scheduled_for_ms,
                });
            }
        }
    }
    due
}

/// Pairs grouped by requested base-token address (every requested token gets an entry).
pub async fn fetch_token_pairs_batch(
    api: &DexScreenerApi,
    token_addresses: &[String],
) -> Result<Vec<(String, Vec<Object>)>, SignalsError> {
    if token_addresses.is_empty() {
        return Ok(Vec::new());
    }
    let pairs = api.token_pairs(&token_addresses.join(",")).await?;
    let mut grouped: Vec<(String, Vec<Object>)> = token_addresses
        .iter()
        .map(|a| (a.clone(), Vec::new()))
        .collect();
    for pair in pairs {
        let base = py_str(object(&pair, "baseToken").and_then(|b| b.get("address")));
        if let Some((_, bucket)) = grouped.iter_mut().find(|(a, _)| *a == base) {
            bucket.push(pair);
        }
    }
    Ok(grouped)
}

fn empty_volume() -> Value {
    json!({"m5": null, "h1": null, "h6": null, "h24": null})
}

pub fn build_snapshot_record(due: &DueSnapshot, pairs: &[Object], captured_at_ms: i64) -> Value {
    let Some(pair) = select_snapshot_pair(pairs, &due.pair_address) else {
        return json!({
            "detection_id": due.detection_id,
            "scheduled_offset_seconds": due.scheduled_offset_seconds,
            "scheduled_for_ms": due.scheduled_for_ms,
            "captured_at_ms": captured_at_ms,
            "status": "pair_gone",
            "pair_exists": false,
            "price_usd": null,
            "liquidity_usd": null,
            "volume": empty_volume(),
            "fdv": null,
            "market_cap": null,
            "pair_url": null,
            "raw_pair": null,
        });
    };
    let vol = |key: &str| float_or_none(nested(pair, "volume", key));
    json!({
        "detection_id": due.detection_id,
        "scheduled_offset_seconds": due.scheduled_offset_seconds,
        "scheduled_for_ms": due.scheduled_for_ms,
        "captured_at_ms": captured_at_ms,
        "status": "ok",
        "pair_exists": true,
        "price_usd": float_or_none(pair.get("priceUsd")),
        "liquidity_usd": float_or_none(nested(pair, "liquidity", "usd")),
        "volume": {"m5": vol("m5"), "h1": vol("h1"), "h6": vol("h6"), "h24": vol("h24")},
        "fdv": float_or_none(pair.get("fdv")),
        "market_cap": float_or_none(pair.get("marketCap")),
        "pair_url": pair.get("url").cloned().unwrap_or(Value::Null),
        "raw_pair": Value::Object(pair.clone()),
    })
}

pub fn build_api_error_snapshot(due: &DueSnapshot, captured_at_ms: i64, error: &str) -> Value {
    json!({
        "detection_id": due.detection_id,
        "scheduled_offset_seconds": due.scheduled_offset_seconds,
        "scheduled_for_ms": due.scheduled_for_ms,
        "captured_at_ms": captured_at_ms,
        "status": "api_error",
        "error": error,
        "pair_exists": null,
        "price_usd": null,
        "liquidity_usd": null,
        "volume": empty_volume(),
        "raw_pair": null,
    })
}

/// The recorded pair if still listed on Solana, else the most liquid Solana pair.
fn select_snapshot_pair<'a>(pairs: &'a [Object], pair_address: &str) -> Option<&'a Object> {
    let solana: Vec<&Object> = pairs
        .iter()
        .filter(|p| py_str(p.get("chainId")).to_lowercase() == "solana")
        .collect();
    if let Some(exact) = solana
        .iter()
        .find(|p| py_str(p.get("pairAddress")) == pair_address)
    {
        return Some(exact);
    }
    first_max_by(solana, |p| float_or_zero(nested(p, "liquidity", "usd")))
}

/// Capture currently due snapshots (capped per run); returns rows written.
pub async fn run_snapshotter_once(
    settings: &SnapshotterSettings,
    api: &DexScreenerApi,
    clock: Arc<dyn Clock>,
    now_ms: i64,
) -> Result<usize, SignalsError> {
    let detections = load_detections(&settings.ledger_path);
    let snapshots = load_snapshots(&settings.snapshots_path);
    let mut due = compute_due_snapshots(
        &detections,
        &snapshots,
        now_ms,
        &settings.snapshot_offsets_seconds,
    );
    if due.len() > settings.max_snapshots_per_run {
        tracing::warn!(
            due = due.len(),
            processing = settings.max_snapshots_per_run,
            "V2 snapshotter due backlog exceeds per-run cap"
        );
        due.truncate(settings.max_snapshots_per_run);
    }
    if due.is_empty() {
        tracing::info!("V2 snapshotter found no due snapshots");
        return Ok(0);
    }

    let mut written = 0;
    let batches: Vec<&[DueSnapshot]> = due.chunks(BATCH_SIZE).collect();
    for (index, batch) in batches.iter().enumerate() {
        let tokens: Vec<String> = batch
            .iter()
            .map(|d| d.token_address.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let captured_at_ms = clock.now_ms();
        let records: Vec<Value> = match fetch_token_pairs_batch(api, &tokens).await {
            Ok(grouped) => batch
                .iter()
                .map(|item| {
                    let pairs = grouped
                        .iter()
                        .find(|(a, _)| *a == item.token_address)
                        .map_or(&[][..], |(_, p)| p.as_slice());
                    build_snapshot_record(item, pairs, captured_at_ms)
                })
                .collect(),
            Err(err) => {
                tracing::warn!(%err, "V2 snapshotter batch fetch failed");
                batch
                    .iter()
                    .map(|item| build_api_error_snapshot(item, captured_at_ms, &err.to_string()))
                    .collect()
            }
        };
        for record in &records {
            append_ledger(&settings.snapshots_path, record)?;
            written += 1;
        }
        if index + 1 < batches.len() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    tracing::info!(written, "V2 snapshotter wrote snapshot rows");
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::FixedClock;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW_MS: i64 = 1_800_000_000_000;

    fn detection(id: &str, detected_at_ms: i64, token: &str, pair: &str) -> Object {
        json!({
            "detection_id": id,
            "detected_at_ms": detected_at_ms,
            "token_address": token,
            "pair_address": pair,
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn pair(token: &str, pair_address: &str) -> Value {
        json!({
            "chainId": "solana",
            "pairAddress": pair_address,
            "baseToken": {"address": token},
            "priceUsd": "2.5",
            "liquidity": {"usd": "15000"},
            "volume": {"m5": 10, "h1": 20, "h6": 30, "h24": 40},
            "fdv": 100_000,
            "marketCap": 90_000,
            "url": "https://dexscreener.com/solana/PAIR1",
        })
    }

    fn snapshot(id: &str, offset: i64, status: &str) -> Object {
        json!({"detection_id": id, "scheduled_offset_seconds": offset, "status": status})
            .as_object()
            .cloned()
            .unwrap()
    }

    fn offsets(due: &[DueSnapshot]) -> Vec<i64> {
        due.iter().map(|d| d.scheduled_offset_seconds).collect()
    }

    fn write_jsonl(path: &Path, rows: &[Object]) {
        let text: String = rows
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        std::fs::write(path, text).unwrap();
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn computes_due_offsets_relative_to_now() {
        let d = [detection("d1", NOW_MS - 3_600_000, "TOKEN1", "PAIR1")];
        let due = compute_due_snapshots(&d, &[], NOW_MS, &SNAPSHOT_OFFSETS_SECONDS);
        assert_eq!(offsets(&due), vec![300, 900, 3600]);
    }

    #[test]
    fn skips_already_captured_and_retries_api_errors() {
        let d = [detection("d1", NOW_MS - 3_600_000, "TOKEN1", "PAIR1")];
        let due = compute_due_snapshots(
            &d,
            &[snapshot("d1", 300, "ok")],
            NOW_MS,
            &SNAPSHOT_OFFSETS_SECONDS,
        );
        assert_eq!(offsets(&due), vec![900, 3600]);

        let d = [detection("d1", NOW_MS - 300_000, "TOKEN1", "PAIR1")];
        let due = compute_due_snapshots(
            &d,
            &[snapshot("d1", 300, "api_error")],
            NOW_MS,
            &SNAPSHOT_OFFSETS_SECONDS,
        );
        assert_eq!(offsets(&due), vec![300]);
    }

    #[test]
    fn handles_pair_gone() {
        let due = DueSnapshot {
            detection_id: "d1".into(),
            token_address: "TOKEN1".into(),
            pair_address: "PAIR1".into(),
            scheduled_offset_seconds: 300,
            scheduled_for_ms: NOW_MS,
        };
        let record = build_snapshot_record(&due, &[], NOW_MS + 1);
        assert_eq!(record["pair_exists"], json!(false));
        assert_eq!(record["status"], "pair_gone");
        assert!(record["price_usd"].is_null() && record["liquidity_usd"].is_null());
    }

    #[test]
    fn env_offsets_skip_invalid_parts() {
        assert_eq!(parse_offsets("60, x ,120"), vec![60, 120]);
        assert_eq!(parse_offsets("x"), SNAPSHOT_OFFSETS_SECONDS.to_vec());
    }

    async fn pairs_server(body: Value, status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn api(server: &MockServer) -> DexScreenerApi {
        DexScreenerApi::with_base_url(reqwest::Client::new(), server.uri())
    }

    #[tokio::test]
    async fn per_run_cap_honored() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SnapshotterSettings {
            ledger_path: dir.path().join("ledger.jsonl"),
            snapshots_path: dir.path().join("snapshots.jsonl"),
            max_snapshots_per_run: 2,
            ..SnapshotterSettings::default()
        };
        let rows: Vec<Object> = (0..3)
            .map(|i| {
                detection(
                    &format!("d{i}"),
                    NOW_MS - 3_600_000,
                    &format!("TOKEN{i}"),
                    &format!("PAIR{i}"),
                )
            })
            .collect();
        write_jsonl(&settings.ledger_path, &rows);
        let server = pairs_server(
            json!({"pairs": [pair("TOKEN0", "PAIR0"), pair("TOKEN1", "PAIR1"), pair("TOKEN2", "PAIR2")]}),
            200,
        )
        .await;
        let written = run_snapshotter_once(
            &settings,
            &api(&server),
            Arc::new(FixedClock(NOW_MS)),
            NOW_MS,
        )
        .await
        .unwrap();
        assert_eq!(written, 2);
        assert_eq!(read_jsonl(&settings.snapshots_path).len(), 2);
    }

    #[tokio::test]
    async fn idempotent_second_run_writes_no_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SnapshotterSettings {
            ledger_path: dir.path().join("ledger.jsonl"),
            snapshots_path: dir.path().join("snapshots.jsonl"),
            ..SnapshotterSettings::default()
        };
        write_jsonl(
            &settings.ledger_path,
            &[detection("d1", NOW_MS - 300_000, "TOKEN1", "PAIR1")],
        );
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest/dex/tokens/TOKEN1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"pairs": [pair("TOKEN1", "PAIR1")]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let clock: Arc<dyn Clock> = Arc::new(FixedClock(NOW_MS));
        let first = run_snapshotter_once(&settings, &api(&server), clock.clone(), NOW_MS)
            .await
            .unwrap();
        let second = run_snapshotter_once(&settings, &api(&server), clock, NOW_MS)
            .await
            .unwrap();
        assert_eq!((first, second), (1, 0));
        let rows = read_jsonl(&settings.snapshots_path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"], "ok");
        assert_eq!(rows[0]["price_usd"].as_f64(), Some(2.5));
        assert_eq!(rows[0]["liquidity_usd"].as_f64(), Some(15_000.0));
    }

    #[tokio::test]
    async fn api_error_is_written_but_retried_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SnapshotterSettings {
            ledger_path: dir.path().join("ledger.jsonl"),
            snapshots_path: dir.path().join("snapshots.jsonl"),
            ..SnapshotterSettings::default()
        };
        write_jsonl(
            &settings.ledger_path,
            &[detection("d1", NOW_MS - 300_000, "TOKEN1", "PAIR1")],
        );
        let server = pairs_server(json!({}), 500).await;
        let written = run_snapshotter_once(
            &settings,
            &api(&server),
            Arc::new(FixedClock(NOW_MS)),
            NOW_MS,
        )
        .await
        .unwrap();
        assert_eq!(written, 1);
        assert_eq!(
            read_jsonl(&settings.snapshots_path)[0]["status"],
            "api_error"
        );
        let due_after = compute_due_snapshots(
            &load_detections(&settings.ledger_path),
            &load_snapshots(&settings.snapshots_path),
            NOW_MS,
            &SNAPSHOT_OFFSETS_SECONDS,
        );
        assert_eq!(offsets(&due_after), vec![300]);
    }
}

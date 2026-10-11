//! End-to-end `spec_snapshot` run against wiremock, compared byte-for-byte with outputs the
//! Python `scripts/spec_snapshot.py` produced from the same routes
//! (`tests/fixtures/spec_snapshot/<scenario>/`; SUMMARY.md timestamp/out-dir lines templated).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use bb_strategies::spec_snapshot::{self, SnapshotConfig};
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::Value;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OUTPUT_FILES: [&str; 5] = [
    "spot_tickers.csv",
    "funding.csv",
    "open_interest.csv",
    "momentum.csv",
    "SUMMARY.md",
];

#[derive(Deserialize, Clone)]
struct Route {
    path: String,
    query: serde_json::Map<String, Value>,
    status: u16,
    body: Value,
}

#[derive(Deserialize)]
struct Fixture {
    routes: Vec<Route>,
    scenarios: std::collections::BTreeMap<String, Vec<Route>>,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spec_snapshot")
}

fn routes_for(fixture: &Fixture, scenario: &str) -> Vec<Route> {
    let mut routes = fixture.routes.clone();
    for override_route in &fixture.scenarios[scenario] {
        routes.retain(|r| !(r.path == override_route.path && r.query == override_route.query));
        routes.push(override_route.clone());
    }
    routes
}

async fn run_scenario(scenario: &str) -> (bool, PathBuf, tempfile::TempDir) {
    let fixture: Fixture =
        serde_json::from_str(&std::fs::read_to_string(fixture_dir().join("routes.json")).unwrap())
            .unwrap();
    let server = MockServer::start().await;
    for route in routes_for(&fixture, scenario) {
        let mut mock = Mock::given(method("GET")).and(path(route.path.as_str()));
        for (key, value) in &route.query {
            mock = mock.and(query_param(key.as_str(), value.as_str().unwrap()));
        }
        mock.respond_with(ResponseTemplate::new(route.status).set_body_json(&route.body))
            .mount(&server)
            .await;
    }
    let config = SnapshotConfig {
        spot_base: server.uri(),
        fapi_base: server.uri(),
        // Sequential, like the Python golden run, so "last status" is deterministic.
        concurrency: 1,
        retry_backoff: Duration::ZERO,
        oi_pacing: Duration::ZERO,
        kline_pacing: Duration::ZERO,
    };
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("nested").join(scenario);
    let snapshot = spec_snapshot::run(&out_dir, &config).await.unwrap();

    let summary_path = out_dir.join("SUMMARY.md");
    let summary = std::fs::read_to_string(&summary_path).unwrap();
    assert!(summary.contains(&format!(
        "- Fetch timestamp (UTC): {}\n",
        snapshot.generated_at
    )));
    assert!(summary.contains(&format!("- Output directory: `{}`\n", out_dir.display())));
    let templated = summary
        .replace(&snapshot.generated_at, "@GENERATED_AT@")
        .replace(&out_dir.display().to_string(), "@OUT_DIR@");
    std::fs::write(&summary_path, templated).unwrap();
    (snapshot.spot_failed, out_dir, tmp)
}

async fn assert_matches_python(scenario: &str, expect_spot_failed: bool) {
    let (spot_failed, out_dir, _tmp) = run_scenario(scenario).await;
    assert_eq!(spot_failed, expect_spot_failed);
    for name in OUTPUT_FILES {
        let expected = std::fs::read_to_string(fixture_dir().join(scenario).join(name)).unwrap();
        let actual = std::fs::read_to_string(out_dir.join(name)).unwrap();
        assert_eq!(actual, expected, "{scenario}/{name}");
    }
}

#[tokio::test]
async fn full_snapshot_matches_python_output() {
    assert_matches_python("full", false).await;
}

#[tokio::test]
async fn futures_blocked_snapshot_is_spot_only() {
    assert_matches_python("futures_blocked", false).await;
}

#[tokio::test]
async fn spot_failure_writes_partial_snapshot_and_flags_failure() {
    assert_matches_python("spot_failed", true).await;
}

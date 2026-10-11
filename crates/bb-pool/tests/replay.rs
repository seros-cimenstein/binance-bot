//! Fixture replays must reproduce the decisions recorded from the original Python
//! implementation (`src.pool.replay`, retired) on the same fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use bb_pool::replay::{ReplayFixture, run_replay, run_replay_fixture};
use bb_pool::report::{ReportOptions, build_report, render_report};
use rusqlite::Connection;
use rust_decimal_macros::dec;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

type DecisionRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    String,
);

fn decisions(db: &std::path::Path) -> Vec<DecisionRow> {
    let connection = Connection::open(db).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT decided_at, portfolio_id, action, source_asset, destination_asset, input_amount, output_amount, score, reason
             FROM paper_decisions ORDER BY rowid",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

const WARMUP: &str = "insufficient benchmark history for paper ranking (requires 4 cycles)";
const LARGEST: &str = "highest-ranked asset is already the largest holding";
const REBALANCE: &str = "simulated reallocation passed route, cost, reserve, and allocation gates";

#[tokio::test]
async fn main_fixture_matches_python_reference_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("replay.db");
    let summary = run_replay(fixture("pool_market_replay.json"), Some(8), &db, true)
        .await
        .unwrap();

    assert_eq!(summary.cycles, 8);
    assert_eq!(
        summary.decisions,
        BTreeMap::from([("hold".into(), 14), ("rebalance".into(), 2)])
    );
    let expected = BTreeMap::from([
        ("BNB".to_owned(), dec!(0.025)),
        ("EDEN".to_owned(), dec!(12.47875391)),
    ]);
    assert_eq!(summary.final_balances["simple_momentum"], expected);
    assert_eq!(summary.final_balances["risk_adjusted_momentum"], expected);
    assert_eq!(summary.balance_reads, 8);
    assert_eq!(summary.commission_reads, 2);
    assert_eq!(
        summary.request_counts,
        BTreeMap::from([("fetch_tickers".into(), 8), ("load_markets".into(), 8)])
    );

    let rows = decisions(&db);
    assert_eq!(rows.len(), 16);
    let python_scores = [
        (0.23809523809523803, 0.21921865014148464),
        (0.22727272727272724, 0.20412677529738935),
        (0.16666666666666652, 0.14517027360984755),
        (0.1153846153846152, 0.11426279776091815),
    ];
    for (cycle, pair) in rows.chunks(2).enumerate() {
        let minute = cycle * 15;
        let timestamp = format!("2026-05-25T{:02}:{:02}:00+00:00", minute / 60, minute % 60);
        assert_eq!(pair[0].1, "simple_momentum");
        assert_eq!(pair[1].1, "risk_adjusted_momentum");
        for row in pair {
            assert_eq!(row.0, timestamp);
            match cycle {
                0..=2 => assert_eq!((row.2.as_str(), row.8.as_str()), ("hold", WARMUP)),
                3 => {
                    assert_eq!((row.2.as_str(), row.8.as_str()), ("rebalance", REBALANCE));
                    assert_eq!(
                        (row.3.as_deref(), row.4.as_deref()),
                        (Some("BNB"), Some("EDEN"))
                    );
                    assert_eq!(row.5, Some(0.025));
                    assert_eq!(row.6, Some(12.47875391));
                }
                _ => assert_eq!((row.2.as_str(), row.8.as_str()), ("hold", LARGEST)),
            }
        }
        if cycle == 3 {
            assert!((pair[0].7.unwrap() - 0.2).abs() < 1e-9);
            assert!((pair[1].7.unwrap() - 0.18013028406705767).abs() < 1e-9);
        }
        if cycle >= 4 {
            let (simple, risk) = python_scores[cycle - 4];
            assert!((pair[0].7.unwrap() - simple).abs() < 1e-9);
            assert!((pair[1].7.unwrap() - risk).abs() < 1e-9);
        }
    }

    let connection = Connection::open(&db).unwrap();
    let route_json: String = connection
        .query_row(
            "SELECT route_json FROM paper_decisions WHERE action = 'rebalance' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        route_json,
        r#"[{"destination": "USDT", "fee": 0.011248875, "fee_rate": 0.00075, "input": 0.025, "output": 14.987251125, "side": "sell", "source": "BNB", "symbol": "BNB/USDT"}, {"destination": "EDEN", "fee": 0.00936609, "fee_rate": 0.00075, "input": 14.987251125, "output": 12.47875391, "side": "buy", "source": "USDT", "symbol": "EDEN/USDT"}]"#
    );
    let catalog_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM market_catalog_cycles", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(catalog_count, 8);
}

#[tokio::test]
async fn tiny_balances_stay_below_market_minimums() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = ReplayFixture::load(fixture("pool_market_replay.json")).unwrap();
    loaded.initial_balances = BTreeMap::from([("BNB".into(), dec!(0.001))]);
    let db = dir.path().join("tiny-replay.db");
    let summary = run_replay_fixture(loaded, Some(6), &db, true)
        .await
        .unwrap();
    assert_eq!(summary.decisions.get("rebalance"), None);
    assert_eq!(
        summary.final_balances["simple_momentum"],
        BTreeMap::from([("BNB".into(), dec!(0.001))])
    );
    assert!(
        decisions(&db)
            .iter()
            .any(|row| row.8.contains("does not satisfy routed market minimums"))
    );
}

#[tokio::test]
async fn refuses_production_paper_db_path() {
    let dir = tempfile::tempdir().unwrap();
    let err = run_replay(
        fixture("pool_market_replay.json"),
        Some(1),
        dir.path().join("pool-paper.db"),
        true,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("production paper ledger"));
}

#[tokio::test]
async fn stress_fixture_matches_python_reference() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("stress.db");
    let summary = run_replay(
        fixture("pool_market_replay_stress.json"),
        Some(6),
        &db,
        true,
    )
    .await
    .unwrap();
    assert_eq!(summary.decisions, BTreeMap::from([("hold".into(), 12)]));
    assert_eq!(summary.commission_reads, 0);
    assert_eq!(
        summary.final_balances["simple_momentum"],
        BTreeMap::from([("BNB".into(), dec!(0.001))])
    );
    let rows = decisions(&db);
    assert!(rows[..6].iter().all(|r| r.8 == WARMUP && r.7.is_none()));
    assert!(rows[6..].iter().all(|r| r.8 == LARGEST && r.7 == Some(0.0)));

    let connection = Connection::open(&db).unwrap();
    let reason = |symbol: &str| -> String {
        connection
            .query_row(
                "SELECT rejection_reasons FROM market_catalog_latest WHERE symbol = ?",
                [symbol],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        reason("WIDE/USDT"),
        r#"["spread exceeds configured threshold"]"#
    );
    assert_eq!(
        reason("THIN/USDT"),
        r#"["quote volume below configured USDT threshold"]"#
    );
}

#[tokio::test]
async fn report_explains_warmup_without_writes_and_ranks_after_observations() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("stress.db");
    run_replay(
        fixture("pool_market_replay_stress.json"),
        Some(2),
        &db,
        true,
    )
    .await
    .unwrap();
    let modified_before = std::fs::metadata(&db).unwrap().modified().unwrap();
    let options = ReportOptions {
        required_observations: 4,
        ..ReportOptions::default()
    };
    let report = build_report(&db, &options).unwrap();
    let rendered = render_report(&report);
    assert_eq!(report.completed_cycles, 2);
    assert_eq!(report.remaining_observations(), 2);
    assert!(!report.ranking_available());
    assert_eq!(
        report.balances["simple_momentum"],
        BTreeMap::from([("BNB".into(), dec!(0.001))])
    );
    let rejection = |name: &str| {
        report
            .rejection_reasons
            .iter()
            .find(|(r, _)| r == name)
            .map(|(_, c)| *c)
    };
    assert_eq!(rejection("spread exceeds configured threshold"), Some(1));
    assert_eq!(
        rejection("quote volume below configured USDT threshold"),
        Some(1)
    );
    assert!(rendered.contains("Ranked candidates: unavailable"));
    assert!(rendered.contains("Decision reasons:"));
    assert!(rendered.contains("Live execution: disabled"));
    assert!(rendered.contains("simple_momentum: BNB=0.001"));
    assert_eq!(
        std::fs::metadata(&db).unwrap().modified().unwrap(),
        modified_before
    );
    let json: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    assert_eq!(json["remaining_observations"], 2);
    assert_eq!(json["ranking_available"], false);

    let ranked_db = dir.path().join("ranked.db");
    run_replay(
        fixture("pool_market_replay.json"),
        Some(6),
        &ranked_db,
        true,
    )
    .await
    .unwrap();
    let report = build_report(&ranked_db, &options).unwrap();
    assert!(report.ranking_available());
    assert_eq!(report.rankings[0].0, "simple_momentum");
    assert_eq!(report.rankings[0].1[0].asset, "EDEN");
    assert!(report.decision_counts["rebalance"] >= 1);
    assert!(build_report(dir.path().join("missing.db"), &options).is_err());
}

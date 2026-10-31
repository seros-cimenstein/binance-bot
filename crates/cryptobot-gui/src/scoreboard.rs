//! The Scoreboard tab: the newest `bb tag-scoreboard` file (`tag-scoreboard-v1`), which
//! tags and signals moved prices their way at which horizon (Phase 53 E3).
//!
//! Per row (signal name × target × horizon) the whole history's samples, excess, net after
//! fees, hit rate and `t`, the older and newer halves' excess, and the `decayed` flag; the
//! file's span, `available_at` and settings.

use std::path::Path;

use bb_trader::scoreboard::{Row, Scoreboard, Stats};
use serde_json::{Value, json};

/// Where `bb tag-scoreboard` writes its newest file by default.
pub const DEFAULT_PATH: &str = "data/scoreboard/latest.json";

fn stats(s: &Stats) -> Value {
    json!({"n": s.n, "excess_pct": s.excess_pct, "net_pct": s.net_pct, "hit_rate": s.hit_rate,
           "t": s.t, "blocks": s.blocks})
}

fn row(r: &Row) -> Value {
    json!({
        "name": r.name,
        "target": r.target,
        "horizon": r.horizon,
        "all": stats(&r.all),
        "older_excess_pct": r.older.excess_pct,
        "newer_excess_pct": r.newer.excess_pct,
        "older_n": r.older.n,
        "newer_n": r.newer.n,
        "decayed": r.decayed,
    })
}

/// The scoreboard at `path`, its rows strongest `|t|` first per horizon; `error` when it is
/// missing or not a `tag-scoreboard-v1` file.
pub fn scoreboard_view(path: &Path) -> Value {
    let board = match Scoreboard::read(path) {
        Ok(board) => board,
        Err(error) => return json!({"path": path, "error": error}),
    };
    let mut rows: Vec<&Row> = board.rows.iter().collect();
    rows.sort_by(|a, b| {
        a.horizon
            .cmp(&b.horizon)
            .then_with(|| b.all.t.abs().total_cmp(&a.all.t.abs()))
            .then_with(|| a.name.cmp(&b.name))
    });
    json!({
        "path": path,
        "error": null,
        "generated_at": board.generated_at,
        "as_of": board.as_of,
        "available_at": board.available_at,
        "history": board.history,
        "horizons": board.horizons,
        "fee_bps": board.fee_bps,
        "min_posters": board.min_posters,
        "assets": board.assets,
        "min_blocks": bb_trader::scoreboard::MIN_BLOCKS,
        "rows": rows.into_iter().map(row).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(excess: f64, t: f64) -> Value {
        json!({"n": 40, "excess_pct": excess, "net_pct": excess - 0.15, "hit_rate": 0.6, "t": t, "blocks": 20})
    }

    #[test]
    fn rows_come_strongest_first_per_horizon_with_halves_and_decay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest.json");
        let row = |name: &str, horizon: &str, t: f64, decayed: bool| {
            json!({"name": name, "target": "assets", "horizon": horizon, "all": stats(0.4, t),
                   "older": stats(0.8, t), "newer": stats(0.1, t), "decayed": decayed})
        };
        let board = json!({
            "schema": "tag-scoreboard-v1", "generated_at": "2026-10-31T12:44:45Z",
            "as_of": "2026-10-31T12:44:37Z", "available_at": "2026-10-31T12:00:00Z",
            "history": {"from": "2026-10-01T00:00:00Z", "to": "2026-10-31T00:00:00Z", "split": "2026-10-16T00:00:00Z", "feeds": 721},
            "horizons": {"24h": {"signals_to": "2026-10-30T12:00:00Z", "outcomes_to": "2026-10-31T12:00:00Z"}},
            "fee_bps": 15.0, "min_posters": 2, "assets": 41,
            "rows": [row("talk.6h", "24h", 1.0, false), row("stance.6h", "1h", -0.5, false),
                     row("momentum.1h", "24h", -3.0, true), row("asset.level.24h", "1h", 2.5, false)]
        });
        std::fs::write(&path, board.to_string()).unwrap();
        let view = scoreboard_view(&path);
        assert_eq!(view["error"], Value::Null);
        assert_eq!(view["available_at"], "2026-10-31T12:00:00Z");
        let order: Vec<(&str, &str)> = view["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["horizon"].as_str().unwrap(), r["name"].as_str().unwrap()))
            .collect();
        assert_eq!(
            order,
            [
                ("1h", "asset.level.24h"),
                ("1h", "stance.6h"),
                ("24h", "momentum.1h"),
                ("24h", "talk.6h")
            ]
        );
        let decayed = &view["rows"][2];
        assert_eq!(
            (
                decayed["decayed"].as_bool(),
                decayed["older_excess_pct"].as_f64(),
                decayed["newer_excess_pct"].as_f64()
            ),
            (Some(true), Some(0.8), Some(0.1))
        );
        assert_eq!(decayed["all"]["t"], -3.0);

        // Missing or foreign: an error, not a failure.
        assert!(
            scoreboard_view(&dir.path().join("none.json"))["error"]
                .as_str()
                .unwrap()
                .contains("none.json")
        );
        std::fs::write(
            &path,
            board
                .to_string()
                .replace("tag-scoreboard-v1", "tag-scoreboard-v2"),
        )
        .unwrap();
        assert!(
            scoreboard_view(&path)["error"]
                .as_str()
                .unwrap()
                .contains("tag-scoreboard-v2")
        );
    }
}

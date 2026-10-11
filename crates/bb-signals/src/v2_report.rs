//! Weekly V2 evaluation report: detection volume, snapshot completeness, rug /
//! catastrophic / 2x rates, path-return and drawdown distributions, and
//! friction-adjusted model P&L. Written to
//! `<output_dir>/signals-v2-report-<date>.md`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use bb_core::Decimal;
use bb_core::env;
use chrono::DateTime;
use rust_decimal_macros::dec;
use serde_json::Value;

use crate::error::SignalsError;
use crate::jsonv::{Object, float_or_none, int_or_zero, object, py_str};
use crate::snapshotter::{SNAPSHOT_OFFSETS_SECONDS, load_detections, load_snapshots};

pub const V1_CATASTROPHIC_RATE: f64 = 0.753;
const SEVEN_DAYS_S: i64 = 604_800;

#[derive(Debug, Clone, PartialEq)]
pub struct ReportSettings {
    pub ledger_path: PathBuf,
    pub snapshots_path: PathBuf,
    pub output_dir: PathBuf,
    pub notional_usd: Decimal,
    pub solana_round_trip_fixed_cost_usd: Decimal,
    pub entry_slippage_bps: Decimal,
    pub exit_slippage_bps: Decimal,
}

impl Default for ReportSettings {
    fn default() -> Self {
        Self {
            ledger_path: "data/signals-v2-ledger.jsonl".into(),
            snapshots_path: "data/signals-v2-snapshots.jsonl".into(),
            output_dir: "data".into(),
            notional_usd: dec!(15.63),
            solana_round_trip_fixed_cost_usd: dec!(0.50),
            entry_slippage_bps: dec!(200),
            exit_slippage_bps: dec!(200),
        }
    }
}

impl ReportSettings {
    pub fn from_env() -> Self {
        Self {
            ledger_path: env::string(&["SIGNALS_V2_LEDGER_PATH"], "data/signals-v2-ledger.jsonl")
                .into(),
            snapshots_path: env::string(
                &["SIGNALS_V2_SNAPSHOTS_PATH"],
                "data/signals-v2-snapshots.jsonl",
            )
            .into(),
            output_dir: env::string(&["SIGNALS_V2_REPORT_OUTPUT_DIR"], "data").into(),
            ..Self::default()
        }
    }
}

/// Order statistics of a sample of fractional returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Distribution {
    pub sample_size: usize,
    pub insufficient_sample: bool,
    pub p25: Option<f64>,
    pub median: Option<f64>,
    pub p75: Option<f64>,
    pub p90: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completeness {
    pub due: usize,
    pub captured: usize,
}

impl Completeness {
    pub fn rate(&self) -> Option<f64> {
        rate(self.captured, self.due)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrictionPnl {
    pub notional_usd: Decimal,
    pub solana_round_trip_fixed_cost_usd: Decimal,
    pub best_exit_model: Distribution,
    pub scheduled_exit_1h_model: Distribution,
    pub best_exit_profitable_count: usize,
    pub scheduled_exit_1h_profitable_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    pub detection_count: usize,
    pub counts_by_day: BTreeMap<String, usize>,
    pub counts_by_filter_version: BTreeMap<String, usize>,
    /// Keyed by offset seconds, in [`SNAPSHOT_OFFSETS_SECONDS`] order.
    pub snapshot_completeness: Vec<(i64, Completeness)>,
    pub rug_count: usize,
    pub catastrophic_count: usize,
    pub two_x_count: usize,
    pub path_return_distribution: Vec<(i64, Distribution)>,
    pub max_drawdown_distribution: Distribution,
    pub friction_adjusted_pnl: FrictionPnl,
}

impl Metrics {
    pub fn rug_rate(&self) -> Option<f64> {
        rate(self.rug_count, self.detection_count)
    }
    pub fn catastrophic_rate(&self) -> Option<f64> {
        rate(self.catastrophic_count, self.detection_count)
    }
    pub fn two_x_rate(&self) -> Option<f64> {
        rate(self.two_x_count, self.detection_count)
    }
    pub fn path_return(&self, offset: i64) -> Option<&Distribution> {
        self.path_return_distribution
            .iter()
            .find(|(o, _)| *o == offset)
            .map(|(_, d)| d)
    }
    pub fn completeness(&self, offset: i64) -> Option<Completeness> {
        self.snapshot_completeness
            .iter()
            .find(|(o, _)| *o == offset)
            .map(|(_, c)| *c)
    }
}

/// Net return of a model round trip with slippage on both legs and a fixed
/// Solana cost amortised over the notional.
pub fn friction_adjusted_net_return(exit_to_entry_ratio: f64, settings: &ReportSettings) -> f64 {
    let bps = bb_core::money::BPS_PER_UNIT;
    let entry = bb_core::money::to_f64(settings.entry_slippage_bps / bps);
    let exit = bb_core::money::to_f64(settings.exit_slippage_bps / bps);
    let fixed_cost = if settings.notional_usd.is_zero() {
        0.0
    } else {
        bb_core::money::to_f64(settings.solana_round_trip_fixed_cost_usd / settings.notional_usd)
    };
    exit_to_entry_ratio * (1.0 - entry) * (1.0 - exit) - 1.0 - fixed_cost
}

pub fn compute_metrics(
    detections: &[Object],
    snapshots: &[Object],
    settings: &ReportSettings,
    now_ms: i64,
) -> Metrics {
    let latest = latest_snapshots(snapshots);
    let detection_by_id: HashMap<String, &Object> = detections
        .iter()
        .filter_map(|d| {
            let id = py_str(d.get("detection_id"));
            (!id.is_empty()).then_some((id, d))
        })
        .collect();
    let mut by_detection: HashMap<&str, Vec<&Object>> = HashMap::new();
    for ((id, _), snap) in &latest {
        if py_str(snap.get("status")) != "api_error" {
            by_detection.entry(id.as_str()).or_default().push(snap);
        }
    }
    for rows in by_detection.values_mut() {
        rows.sort_by_key(|s| int_or_zero(s.get("scheduled_offset_seconds")));
    }

    let mut counts_by_day = BTreeMap::new();
    let mut counts_by_filter = BTreeMap::new();
    for d in detections {
        *counts_by_day
            .entry(day_from_ms(int_or_zero(d.get("detected_at_ms"))))
            .or_insert(0) += 1;
        let version = match d.get("filter_version") {
            None => "unknown".to_owned(),
            Some(Value::Null) => "None".to_owned(),
            Some(v) => py_str(Some(v)),
        };
        *counts_by_filter.entry(version).or_insert(0) += 1;
    }

    let completeness = snapshot_completeness(detections, &latest, now_ms);

    let mut rug = 0usize;
    let mut catastrophic = 0usize;
    let mut two_x = 0usize;
    let mut path_returns: Vec<(i64, Vec<f64>)> = SNAPSHOT_OFFSETS_SECONDS
        .iter()
        .map(|o| (*o, Vec::new()))
        .collect();
    let mut max_drawdowns = Vec::new();
    let mut best_exit = Vec::new();
    let mut one_hour = Vec::new();

    for (id, detection) in &detection_by_id {
        let Some(entry_price) = detection_price(detection).filter(|p| *p > 0.0) else {
            continue;
        };
        let entry_liquidity = detection_liquidity(detection);
        let mut returns = Vec::new();
        let mut best_price: Option<f64> = None;
        let mut one_hour_price = None;
        let mut is_rug = false;
        let mut is_two_x = false;

        for snap in by_detection.get(id.as_str()).map_or(&[][..], Vec::as_slice) {
            let offset = int_or_zero(snap.get("scheduled_offset_seconds"));
            let liquidity = float_or_none(snap.get("liquidity_usd"));
            let price = float_or_none(snap.get("price_usd"));
            if snap.get("pair_exists") == Some(&Value::Bool(false)) {
                is_rug = true;
            }
            if let Some(liq) = liquidity {
                if liq < 1_000.0 {
                    is_rug = true;
                }
                if entry_liquidity.is_some_and(|e| e > 0.0 && liq < 0.2 * e) {
                    is_rug = true;
                }
            }
            let Some(price) = price.filter(|p| *p > 0.0) else {
                continue;
            };
            let r = price / entry_price - 1.0;
            returns.push(r);
            if let Some((_, bucket)) = path_returns.iter_mut().find(|(o, _)| *o == offset) {
                bucket.push(r);
            }
            if price >= 2.0 * entry_price {
                is_two_x = true;
            }
            if best_price.is_none_or(|b| price > b) {
                best_price = Some(price);
            }
            if offset == 3_600 {
                one_hour_price = Some(price);
            }
        }

        if is_rug {
            rug += 1;
        }
        if is_rug || returns.iter().any(|r| *r <= -0.75) {
            catastrophic += 1;
        }
        if is_two_x {
            two_x += 1;
        }
        if let Some(worst) = returns.iter().copied().reduce(f64::min) {
            max_drawdowns.push(worst);
        }
        if let Some(best) = best_price {
            best_exit.push(friction_adjusted_net_return(best / entry_price, settings));
        }
        if let Some(p) = one_hour_price {
            one_hour.push(friction_adjusted_net_return(p / entry_price, settings));
        }
    }

    let profitable = |v: &[f64]| v.iter().filter(|x| **x > 0.0).count();
    Metrics {
        detection_count: detections.len(),
        counts_by_day,
        counts_by_filter_version: counts_by_filter,
        snapshot_completeness: completeness,
        rug_count: rug,
        catastrophic_count: catastrophic,
        two_x_count: two_x,
        path_return_distribution: path_returns
            .into_iter()
            .map(|(offset, values)| {
                let threshold = if offset == SEVEN_DAYS_S { 10 } else { 1 };
                (offset, distribution(values, threshold))
            })
            .collect(),
        max_drawdown_distribution: distribution(max_drawdowns, 1),
        friction_adjusted_pnl: FrictionPnl {
            notional_usd: settings.notional_usd,
            solana_round_trip_fixed_cost_usd: settings.solana_round_trip_fixed_cost_usd,
            best_exit_profitable_count: profitable(&best_exit),
            scheduled_exit_1h_profitable_count: profitable(&one_hour),
            best_exit_model: distribution(best_exit, 1),
            scheduled_exit_1h_model: distribution(one_hour, 1),
        },
    }
}

/// Latest row per `(detection_id, offset)`; later rows win ties on `captured_at_ms`.
fn latest_snapshots(snapshots: &[Object]) -> HashMap<(String, i64), &Object> {
    let mut latest: HashMap<(String, i64), &Object> = HashMap::new();
    for snap in snapshots {
        let id = py_str(snap.get("detection_id"));
        let offset = int_or_zero(snap.get("scheduled_offset_seconds"));
        if id.is_empty() || offset <= 0 {
            continue;
        }
        let captured = int_or_zero(snap.get("captured_at_ms"));
        let replace = latest
            .get(&(id.clone(), offset))
            .is_none_or(|prev| captured >= int_or_zero(prev.get("captured_at_ms")));
        if replace {
            latest.insert((id, offset), snap);
        }
    }
    latest
}

fn snapshot_completeness(
    detections: &[Object],
    latest: &HashMap<(String, i64), &Object>,
    now_ms: i64,
) -> Vec<(i64, Completeness)> {
    SNAPSHOT_OFFSETS_SECONDS
        .iter()
        .map(|&offset| {
            let mut c = Completeness {
                due: 0,
                captured: 0,
            };
            for d in detections {
                let id = py_str(d.get("detection_id"));
                let detected_at_ms = int_or_zero(d.get("detected_at_ms"));
                if id.is_empty() || detected_at_ms <= 0 || now_ms < detected_at_ms + offset * 1000 {
                    continue;
                }
                c.due += 1;
                if latest
                    .get(&(id, offset))
                    .is_some_and(|s| py_str(s.get("status")) != "api_error")
                {
                    c.captured += 1;
                }
            }
            (offset, c)
        })
        .collect()
}

fn distribution(mut values: Vec<f64>, insufficient_threshold: usize) -> Distribution {
    let n = values.len();
    if n < insufficient_threshold || n == 0 {
        return Distribution {
            sample_size: n,
            insufficient_sample: n < insufficient_threshold,
            p25: None,
            median: None,
            p75: None,
            p90: None,
        };
    }
    values.sort_by(f64::total_cmp);
    let median = if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    };
    Distribution {
        sample_size: n,
        insufficient_sample: false,
        p25: Some(percentile(&values, 25.0)),
        median: Some(median),
        p75: Some(percentile(&values, 75.0)),
        p90: Some(percentile(&values, 90.0)),
    }
}

/// Linear interpolation between closest ranks.
fn percentile(ordered: &[f64], pct: f64) -> f64 {
    match ordered {
        [] => 0.0,
        [only] => *only,
        _ => {
            let rank = (ordered.len() - 1) as f64 * pct / 100.0;
            let lower = rank.floor() as usize;
            let upper = (lower + 1).min(ordered.len() - 1);
            let weight = rank - lower as f64;
            ordered[lower] * (1.0 - weight) + ordered[upper] * weight
        }
    }
}

fn detection_price(detection: &Object) -> Option<f64> {
    if let Some(features) = object(detection, "features")
        && let Some(p) = float_or_none(features.get("price_usd"))
    {
        return Some(p);
    }
    object(detection, "raw_pair").and_then(|raw| float_or_none(raw.get("priceUsd")))
}

fn detection_liquidity(detection: &Object) -> Option<f64> {
    if let Some(features) = object(detection, "features")
        && let Some(l) = float_or_none(features.get("liquidity_usd"))
    {
        return Some(l);
    }
    object(detection, "raw_pair")
        .and_then(|raw| object(raw, "liquidity"))
        .and_then(|liq| float_or_none(liq.get("usd")))
}

fn rate(count: usize, total: usize) -> Option<f64> {
    (total > 0).then(|| count as f64 / total as f64)
}

fn day_from_ms(ms: i64) -> String {
    if ms <= 0 {
        return "unknown".into();
    }
    DateTime::from_timestamp_millis(ms).map_or_else(|| "unknown".into(), bb_core::time::utc_date)
}

pub fn offset_label(offset: i64) -> String {
    match offset {
        300 => "+5m".into(),
        900 => "+15m".into(),
        3_600 => "+1h".into(),
        21_600 => "+6h".into(),
        86_400 => "+24h".into(),
        259_200 => "+3d".into(),
        604_800 => "+7d".into(),
        other => format!("+{other}s"),
    }
}

fn fmt_pct(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".into(), |v| format!("{:.1}%", v * 100.0))
}

fn pnl_row(label: &str, dist: &Distribution, profitable: usize) -> String {
    format!(
        "| {label} | {} | {profitable} | {} | {} | {} | {} |",
        dist.sample_size,
        fmt_pct(dist.p25),
        fmt_pct(dist.median),
        fmt_pct(dist.p75),
        fmt_pct(dist.p90),
    )
}

pub fn render_markdown(metrics: &Metrics, report_date: &str) -> String {
    let mut lines: Vec<String> = vec![
        format!("# V2 Signal Report - {report_date}"),
        String::new(),
        "Both P&L variants below are model exits from scheduled snapshots, not realized fills."
            .into(),
        String::new(),
    ];
    if metrics.detection_count == 0 {
        lines.extend(
            [
                "No detections yet.",
                "",
                "## What Changed Since V1",
                "",
                "V1 catastrophic rate was 75.3%; V2 has no sample yet.",
                "",
            ]
            .map(String::from),
        );
        return lines.join("\n");
    }

    lines.extend([
        "## Detection Volume".into(),
        String::new(),
        format!("- Total detections: {}", metrics.detection_count),
        format!(
            "- Rug proxy rate: {} ({})",
            fmt_pct(metrics.rug_rate()),
            metrics.rug_count
        ),
        format!(
            "- Catastrophic rate: {} ({})",
            fmt_pct(metrics.catastrophic_rate()),
            metrics.catastrophic_count
        ),
        format!(
            "- 2x rate: {} ({})",
            fmt_pct(metrics.two_x_rate()),
            metrics.two_x_count
        ),
        String::new(),
        "### By Day".into(),
        String::new(),
        "| Day | Detections |".into(),
        "|---|---:|".into(),
    ]);
    for (day, count) in &metrics.counts_by_day {
        lines.push(format!("| {day} | {count} |"));
    }
    lines.extend(
        [
            "",
            "### By Filter Version",
            "",
            "| Filter version | Detections |",
            "|---|---:|",
        ]
        .map(String::from),
    );
    for (version, count) in &metrics.counts_by_filter_version {
        lines.push(format!("| {version} | {count} |"));
    }

    lines.extend(
        [
            "",
            "## Snapshot Completeness",
            "",
            "| Offset | Due | Captured | Completeness |",
            "|---|---:|---:|---:|",
        ]
        .map(String::from),
    );
    for (offset, c) in &metrics.snapshot_completeness {
        lines.push(format!(
            "| {} | {} | {} | {} |",
            offset_label(*offset),
            c.due,
            c.captured,
            fmt_pct(c.rate())
        ));
    }

    lines.extend(
        [
            "",
            "## Path Returns",
            "",
            "| Offset | Sample | P25 | Median | P75 | P90 |",
            "|---|---:|---:|---:|---:|---:|",
        ]
        .map(String::from),
    );
    for (offset, dist) in &metrics.path_return_distribution {
        let label = offset_label(*offset);
        if dist.insufficient_sample {
            let na = "insufficient sample";
            lines.push(format!(
                "| {label} | {} | {na} | {na} | {na} | {na} |",
                dist.sample_size
            ));
        } else {
            lines.push(format!(
                "| {label} | {} | {} | {} | {} | {} |",
                dist.sample_size,
                fmt_pct(dist.p25),
                fmt_pct(dist.median),
                fmt_pct(dist.p75),
                fmt_pct(dist.p90),
            ));
        }
    }

    let dd = &metrics.max_drawdown_distribution;
    let pnl = &metrics.friction_adjusted_pnl;
    lines.extend([
        String::new(),
        "## Max Drawdown".into(),
        String::new(),
        format!(
            "Sample {}: P25 {}, median {}, P75 {}, P90 {}.",
            dd.sample_size,
            fmt_pct(dd.p25),
            fmt_pct(dd.median),
            fmt_pct(dd.p75),
            fmt_pct(dd.p90),
        ),
        String::new(),
        "## Friction-Adjusted Paper P&L".into(),
        String::new(),
        format!(
            "Assumptions: ${:.2} notional, ${:.2} Solana round-trip fixed, 2% entry slippage, 2% exit slippage.",
            pnl.notional_usd, pnl.solana_round_trip_fixed_cost_usd
        ),
        String::new(),
        "| Model exit | Sample | Profitable | P25 | Median | P75 | P90 |".into(),
        "|---|---:|---:|---:|---:|---:|---:|".into(),
        pnl_row(
            "Best post-detection snapshot",
            &pnl.best_exit_model,
            pnl.best_exit_profitable_count,
        ),
        pnl_row(
            "+1h scheduled snapshot",
            &pnl.scheduled_exit_1h_model,
            pnl.scheduled_exit_1h_profitable_count,
        ),
        String::new(),
        "## What Changed Since V1".into(),
        String::new(),
        format!(
            "V1 catastrophic rate was 75.3%. V2 catastrophic rate is {} on the same apples-to-apples definition: price down >=75% or rug proxy.",
            fmt_pct(metrics.catastrophic_rate())
        ),
        String::new(),
    ]);
    lines.join("\n")
}

/// Write the report and return its path.
pub fn write_report(
    markdown: &str,
    output_dir: &Path,
    report_date: &str,
) -> Result<PathBuf, SignalsError> {
    let path = output_dir.join(format!("signals-v2-report-{report_date}.md"));
    bb_core::fsutil::atomic_write(&path, markdown.as_bytes())
        .map_err(|e| SignalsError::io(&path, e))?;
    Ok(path)
}

/// Compute, render and write the report for `report_date`.
pub fn generate(
    settings: &ReportSettings,
    report_date: &str,
    now_ms: i64,
) -> Result<PathBuf, SignalsError> {
    let metrics = compute_metrics(
        &load_detections(&settings.ledger_path),
        &load_snapshots(&settings.snapshots_path),
        settings,
        now_ms,
    );
    write_report(
        &render_markdown(&metrics, report_date),
        &settings.output_dir,
        report_date,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW_MS: i64 = 1_800_000_000_000;

    fn detection(id: &str, price: f64, liquidity: f64, detected_at_ms: i64) -> Object {
        json!({
            "detection_id": id,
            "detected_at_ms": detected_at_ms,
            "filter_version": "v2-test",
            "features": {"price_usd": price, "liquidity_usd": liquidity},
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn det(id: &str) -> Object {
        detection(id, 1.0, 10_000.0, NOW_MS - 8 * 24 * 3_600_000)
    }

    fn snapshot(id: &str, offset: i64, price: f64, liquidity: f64, captured_at_ms: i64) -> Object {
        json!({
            "detection_id": id,
            "scheduled_offset_seconds": offset,
            "scheduled_for_ms": NOW_MS - 1_000,
            "captured_at_ms": captured_at_ms,
            "status": "ok",
            "pair_exists": true,
            "price_usd": price,
            "liquidity_usd": liquidity,
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn approx(a: Option<f64>, b: f64) {
        let a = a.unwrap();
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn empty_ledger_report_renders_cleanly() {
        let m = compute_metrics(&[], &[], &ReportSettings::default(), NOW_MS);
        assert_eq!(m.detection_count, 0);
        assert!(render_markdown(&m, "2026-06-27").contains("No detections yet."));
    }

    #[test]
    fn synthetic_prices_compute_rug_2x_and_path_returns() {
        let snaps = [
            snapshot("d1", 300, 2.0, 8_000.0, NOW_MS),
            snapshot("d2", 300, 0.1, 500.0, NOW_MS),
        ];
        let m = compute_metrics(
            &[det("d1"), det("d2")],
            &snaps,
            &ReportSettings::default(),
            NOW_MS,
        );
        assert_eq!(m.rug_rate(), Some(0.5));
        assert_eq!(m.catastrophic_rate(), Some(0.5));
        assert_eq!(m.two_x_rate(), Some(0.5));
        let p = m.path_return(300).unwrap();
        assert_eq!(p.sample_size, 2);
        approx(p.median, 0.05);
        approx(p.p25, -0.425);
        approx(p.p75, 0.525);
        approx(p.p90, 0.81);
        let md = render_markdown(&m, "2026-06-27");
        assert!(
            md.contains("| +5m | 2 | -42.5% | 5.0% | 52.5% | 81.0% |"),
            "{md}"
        );
        assert!(md.contains("Assumptions: $15.63 notional, $0.50 Solana round-trip fixed"));
    }

    #[test]
    fn seven_day_insufficient_sample_marked() {
        let snaps = [snapshot("d1", 604_800, 1.5, 11_000.0, NOW_MS)];
        let m = compute_metrics(&[det("d1")], &snaps, &ReportSettings::default(), NOW_MS);
        let d = m.path_return(604_800).unwrap();
        assert!(d.insufficient_sample);
        assert_eq!(d.sample_size, 1);
    }

    #[test]
    fn friction_calc_matches_formula() {
        let got = friction_adjusted_net_return(2.0, &ReportSettings::default());
        approx(
            Some(got),
            2.0 * (1.0 - 0.02_f64).powi(2) - 1.0 - 0.50 / 15.63,
        );
    }

    #[test]
    fn report_dedupes_snapshot_rows_keeping_latest() {
        let snaps = [
            snapshot("d1", 300, 1.1, 9_000.0, NOW_MS),
            snapshot("d1", 300, 3.0, 9_000.0, NOW_MS + 1),
        ];
        let m = compute_metrics(&[det("d1")], &snaps, &ReportSettings::default(), NOW_MS);
        assert_eq!(m.two_x_rate(), Some(1.0));
        approx(m.path_return(300).unwrap().median, 2.0);
    }

    #[test]
    fn snapshot_completeness_ignores_api_errors() {
        let d = detection("d1", 1.0, 10_000.0, NOW_MS - 300_000);
        let s = json!({
            "detection_id": "d1",
            "scheduled_offset_seconds": 300,
            "captured_at_ms": NOW_MS,
            "status": "api_error",
        })
        .as_object()
        .cloned()
        .unwrap();
        let m = compute_metrics(&[d], &[s], &ReportSettings::default(), NOW_MS);
        let c = m.completeness(300).unwrap();
        assert_eq!(
            c,
            Completeness {
                due: 1,
                captured: 0
            }
        );
        assert_eq!(c.rate(), Some(0.0));
    }

    #[test]
    fn generate_writes_dated_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ReportSettings {
            ledger_path: dir.path().join("missing.jsonl"),
            snapshots_path: dir.path().join("missing-snaps.jsonl"),
            output_dir: dir.path().join("out"),
            ..ReportSettings::default()
        };
        let path = generate(&settings, "2026-10-11", NOW_MS).unwrap();
        assert!(path.ends_with("out/signals-v2-report-2026-10-11.md"));
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .starts_with("# V2 Signal Report - 2026-10-11")
        );
    }
}

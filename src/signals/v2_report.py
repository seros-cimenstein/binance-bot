"""V2 signal evaluation reports."""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from statistics import median
from typing import Any

from loguru import logger

from src.signals.common import utc_ms
from src.signals.snapshotter import SNAPSHOT_OFFSETS_SECONDS


V1_CATASTROPHIC_RATE = 0.753


@dataclass(frozen=True, slots=True)
class ReportSettings:
    """Configuration for weekly V2 evaluation reports."""

    ledger_path: str = "data/signals-v2-ledger.jsonl"
    snapshots_path: str = "data/signals-v2-snapshots.jsonl"
    output_dir: str = "data"
    notional_usd: float = 15.63
    solana_round_trip_fixed_cost_usd: float = 0.50
    entry_slippage_bps: float = 200.0
    exit_slippage_bps: float = 200.0


def settings_from_env() -> ReportSettings:
    """Build report settings from environment variables."""

    return ReportSettings(
        ledger_path=os.getenv("SIGNALS_V2_LEDGER_PATH", "data/signals-v2-ledger.jsonl"),
        snapshots_path=os.getenv(
            "SIGNALS_V2_SNAPSHOTS_PATH",
            "data/signals-v2-snapshots.jsonl",
        ),
        output_dir=os.getenv("SIGNALS_V2_REPORT_OUTPUT_DIR", "data"),
    )


def load_detections(path: str | Path) -> list[dict[str, Any]]:
    """Load V2 detection ledger rows."""

    return _load_jsonl(path)


def load_snapshots(path: str | Path) -> list[dict[str, Any]]:
    """Load V2 snapshot rows."""

    return _load_jsonl(path)


def compute_metrics(
    detections: list[dict[str, Any]],
    snapshots: list[dict[str, Any]],
    *,
    settings: ReportSettings,
    now_ms: int | None = None,
) -> dict[str, Any]:
    """Compute weekly V2 outcome metrics."""

    if now_ms is None:
        now_ms = utc_ms()
    latest_snapshots = _latest_snapshots(snapshots)
    detection_by_id = {
        str(detection.get("detection_id", "")): detection
        for detection in detections
        if detection.get("detection_id")
    }
    snapshots_by_detection: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for (detection_id, _offset), snapshot in latest_snapshots.items():
        if str(snapshot.get("status", "")) == "api_error":
            continue
        snapshots_by_detection[detection_id].append(snapshot)
    for rows in snapshots_by_detection.values():
        rows.sort(key=lambda item: _as_int(item.get("scheduled_offset_seconds")))

    counts_by_day: Counter[str] = Counter()
    counts_by_filter: Counter[str] = Counter()
    for detection in detections:
        counts_by_day[_day_from_ms(_as_int(detection.get("detected_at_ms")))] += 1
        counts_by_filter[str(detection.get("filter_version", "unknown"))] += 1

    completeness = _snapshot_completeness(
        detections,
        latest_snapshots,
        now_ms=now_ms,
        offsets=SNAPSHOT_OFFSETS_SECONDS,
    )

    rug_ids: set[str] = set()
    catastrophic_ids: set[str] = set()
    two_x_ids: set[str] = set()
    path_returns: dict[int, list[float]] = {offset: [] for offset in SNAPSHOT_OFFSETS_SECONDS}
    max_drawdowns: list[float] = []
    best_exit_net_returns: list[float] = []
    one_hour_net_returns: list[float] = []

    for detection_id, detection in detection_by_id.items():
        entry_price = _detection_price(detection)
        entry_liquidity = _detection_liquidity(detection)
        detection_snapshots = snapshots_by_detection.get(detection_id, [])
        if entry_price is None or entry_price <= 0.0:
            continue

        price_returns: list[float] = []
        best_price: float | None = None
        one_hour_price: float | None = None
        rug = False

        for snapshot in detection_snapshots:
            offset = _as_int(snapshot.get("scheduled_offset_seconds"))
            pair_exists = snapshot.get("pair_exists")
            liquidity = _as_float_or_none(snapshot.get("liquidity_usd"))
            price = _as_float_or_none(snapshot.get("price_usd"))
            if pair_exists is False:
                rug = True
            if liquidity is not None and liquidity < 1_000.0:
                rug = True
            if (
                liquidity is not None
                and entry_liquidity is not None
                and entry_liquidity > 0.0
                and liquidity < 0.2 * entry_liquidity
            ):
                rug = True
            if price is None or price <= 0.0:
                continue
            path_return = price / entry_price - 1.0
            price_returns.append(path_return)
            if offset in path_returns:
                path_returns[offset].append(path_return)
            if price >= 2.0 * entry_price:
                two_x_ids.add(detection_id)
            if best_price is None or price > best_price:
                best_price = price
            if offset == 3_600:
                one_hour_price = price

        if rug:
            rug_ids.add(detection_id)
        if rug or any(item <= -0.75 for item in price_returns):
            catastrophic_ids.add(detection_id)
        if price_returns:
            max_drawdowns.append(min(price_returns))
        if best_price is not None:
            best_exit_net_returns.append(
                friction_adjusted_net_return(
                    best_price / entry_price,
                    settings=settings,
                )
            )
        if one_hour_price is not None:
            one_hour_net_returns.append(
                friction_adjusted_net_return(
                    one_hour_price / entry_price,
                    settings=settings,
                )
            )

    detection_count = len(detections)
    return {
        "detection_count": detection_count,
        "counts_by_day": dict(sorted(counts_by_day.items())),
        "counts_by_filter_version": dict(sorted(counts_by_filter.items())),
        "snapshot_completeness": completeness,
        "rug_count": len(rug_ids),
        "rug_rate": _rate(len(rug_ids), detection_count),
        "catastrophic_count": len(catastrophic_ids),
        "catastrophic_rate": _rate(len(catastrophic_ids), detection_count),
        "two_x_count": len(two_x_ids),
        "two_x_rate": _rate(len(two_x_ids), detection_count),
        "path_return_distribution": {
            offset: _distribution(values, insufficient_threshold=10 if offset == 604_800 else 1)
            for offset, values in path_returns.items()
        },
        "max_drawdown_distribution": _distribution(max_drawdowns),
        "friction_adjusted_pnl": {
            "assumptions": {
                "notional_usd": settings.notional_usd,
                "solana_round_trip_fixed_cost_usd": settings.solana_round_trip_fixed_cost_usd,
                "entry_slippage_bps": settings.entry_slippage_bps,
                "exit_slippage_bps": settings.exit_slippage_bps,
            },
            "best_exit_model": _distribution(best_exit_net_returns),
            "scheduled_exit_1h_model": _distribution(one_hour_net_returns),
            "best_exit_profitable_count": sum(1 for item in best_exit_net_returns if item > 0.0),
            "scheduled_exit_1h_profitable_count": sum(1 for item in one_hour_net_returns if item > 0.0),
            "best_exit_sample_size": len(best_exit_net_returns),
            "scheduled_exit_1h_sample_size": len(one_hour_net_returns),
        },
        "v1_comparison": {
            "v1_catastrophic_rate": V1_CATASTROPHIC_RATE,
            "v2_catastrophic_rate": _rate(len(catastrophic_ids), detection_count),
            "definition": (
                "Catastrophic means price down >=75% or rug proxy; rug proxy means "
                "pair gone, liquidity < $1k, or liquidity down >80%."
            ),
        },
    }


def friction_adjusted_net_return(
    exit_to_entry_ratio: float,
    *,
    settings: ReportSettings,
) -> float:
    entry_slippage = settings.entry_slippage_bps / 10_000.0
    exit_slippage = settings.exit_slippage_bps / 10_000.0
    fixed_cost = settings.solana_round_trip_fixed_cost_usd / settings.notional_usd
    return exit_to_entry_ratio * (1.0 - entry_slippage) * (1.0 - exit_slippage) - 1.0 - fixed_cost


def render_markdown(metrics: dict[str, Any], *, report_date: str) -> str:
    """Render a V2 weekly report as markdown."""

    lines = [
        f"# V2 Signal Report - {report_date}",
        "",
        "Both P&L variants below are model exits from scheduled snapshots, not realized fills.",
        "",
    ]
    if metrics["detection_count"] == 0:
        lines.extend([
            "No detections yet.",
            "",
            "## What Changed Since V1",
            "",
            "V1 catastrophic rate was 75.3%; V2 has no sample yet.",
            "",
        ])
        return "\n".join(lines)

    lines.extend([
        "## Detection Volume",
        "",
        f"- Total detections: {metrics['detection_count']}",
        f"- Rug proxy rate: {_fmt_pct(metrics['rug_rate'])} ({metrics['rug_count']})",
        f"- Catastrophic rate: {_fmt_pct(metrics['catastrophic_rate'])} ({metrics['catastrophic_count']})",
        f"- 2x rate: {_fmt_pct(metrics['two_x_rate'])} ({metrics['two_x_count']})",
        "",
        "### By Day",
        "",
        "| Day | Detections |",
        "|---|---:|",
    ])
    for day, count in metrics["counts_by_day"].items():
        lines.append(f"| {day} | {count} |")
    lines.extend(["", "### By Filter Version", "", "| Filter version | Detections |", "|---|---:|"])
    for version, count in metrics["counts_by_filter_version"].items():
        lines.append(f"| {version} | {count} |")

    lines.extend([
        "",
        "## Snapshot Completeness",
        "",
        "| Offset | Due | Captured | Completeness |",
        "|---|---:|---:|---:|",
    ])
    for offset, item in metrics["snapshot_completeness"].items():
        lines.append(
            f"| {_offset_label(int(offset))} | {item['due']} | {item['captured']} | "
            f"{_fmt_pct(item['rate'])} |"
        )

    lines.extend([
        "",
        "## Path Returns",
        "",
        "| Offset | Sample | P25 | Median | P75 | P90 |",
        "|---|---:|---:|---:|---:|---:|",
    ])
    for offset, dist in metrics["path_return_distribution"].items():
        if dist.get("insufficient_sample"):
            lines.append(f"| {_offset_label(int(offset))} | {dist['sample_size']} | insufficient sample | insufficient sample | insufficient sample | insufficient sample |")
        else:
            lines.append(
                f"| {_offset_label(int(offset))} | {dist['sample_size']} | "
                f"{_fmt_pct(dist.get('p25'))} | {_fmt_pct(dist.get('median'))} | "
                f"{_fmt_pct(dist.get('p75'))} | {_fmt_pct(dist.get('p90'))} |"
            )

    drawdown = metrics["max_drawdown_distribution"]
    lines.extend([
        "",
        "## Max Drawdown",
        "",
        (
            f"Sample {drawdown['sample_size']}: P25 {_fmt_pct(drawdown.get('p25'))}, "
            f"median {_fmt_pct(drawdown.get('median'))}, P75 {_fmt_pct(drawdown.get('p75'))}, "
            f"P90 {_fmt_pct(drawdown.get('p90'))}."
        ),
        "",
        "## Friction-Adjusted Paper P&L",
        "",
    ])
    pnl = metrics["friction_adjusted_pnl"]
    assumptions = pnl["assumptions"]
    lines.extend([
        (
            f"Assumptions: ${assumptions['notional_usd']:.2f} notional, "
            f"${assumptions['solana_round_trip_fixed_cost_usd']:.2f} Solana round-trip fixed, "
            "2% entry slippage, 2% exit slippage."
        ),
        "",
        "| Model exit | Sample | Profitable | P25 | Median | P75 | P90 |",
        "|---|---:|---:|---:|---:|---:|---:|",
    ])
    best = pnl["best_exit_model"]
    one_hour = pnl["scheduled_exit_1h_model"]
    lines.append(_pnl_row("Best post-detection snapshot", best, pnl["best_exit_profitable_count"]))
    lines.append(_pnl_row("+1h scheduled snapshot", one_hour, pnl["scheduled_exit_1h_profitable_count"]))

    comparison = metrics["v1_comparison"]
    lines.extend([
        "",
        "## What Changed Since V1",
        "",
        (
            f"V1 catastrophic rate was 75.3%. V2 catastrophic rate is "
            f"{_fmt_pct(comparison['v2_catastrophic_rate'])} on the same apples-to-apples "
            "definition: price down >=75% or rug proxy."
        ),
        "",
    ])
    return "\n".join(lines)


def write_report(markdown: str, *, output_dir: str | Path, report_date: str) -> Path:
    """Write a V2 weekly report and return its path."""

    out_dir = Path(output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / f"signals-v2-report-{report_date}.md"
    path.write_text(markdown, encoding="utf-8")
    return path


def run(argv: list[str] | None = None) -> int:
    """CLI entry point for weekly V2 reporting."""

    parser = argparse.ArgumentParser(description="Write the V2 signal evaluation report.")
    parser.add_argument("--date", default=None, help="Report date, defaults to UTC today.")
    args = parser.parse_args(argv)
    settings = settings_from_env()
    report_date = args.date or datetime.now(tz=timezone.utc).date().isoformat()
    metrics = compute_metrics(
        load_detections(settings.ledger_path),
        load_snapshots(settings.snapshots_path),
        settings=settings,
    )
    path = write_report(
        render_markdown(metrics, report_date=report_date),
        output_dir=settings.output_dir,
        report_date=report_date,
    )
    logger.info(f"Wrote V2 signal report: {path}")
    return 0


def main() -> None:
    raise SystemExit(run(sys.argv[1:]))


def _latest_snapshots(snapshots: list[dict[str, Any]]) -> dict[tuple[str, int], dict[str, Any]]:
    latest: dict[tuple[str, int], dict[str, Any]] = {}
    for snapshot in snapshots:
        detection_id = str(snapshot.get("detection_id", ""))
        offset = _as_int(snapshot.get("scheduled_offset_seconds"))
        if not detection_id or offset <= 0:
            continue
        key = (detection_id, offset)
        if key not in latest or _as_int(snapshot.get("captured_at_ms")) >= _as_int(latest[key].get("captured_at_ms")):
            latest[key] = snapshot
    return latest


def _snapshot_completeness(
    detections: list[dict[str, Any]],
    latest_snapshots: dict[tuple[str, int], dict[str, Any]],
    *,
    now_ms: int,
    offsets: tuple[int, ...],
) -> dict[int, dict[str, Any]]:
    result: dict[int, dict[str, Any]] = {}
    for offset in offsets:
        due = 0
        captured = 0
        for detection in detections:
            detection_id = str(detection.get("detection_id", ""))
            detected_at_ms = _as_int(detection.get("detected_at_ms"))
            if not detection_id or detected_at_ms <= 0:
                continue
            if now_ms < detected_at_ms + offset * 1000:
                continue
            due += 1
            snapshot = latest_snapshots.get((detection_id, offset))
            if snapshot is not None and str(snapshot.get("status", "")) != "api_error":
                captured += 1
        result[offset] = {"due": due, "captured": captured, "rate": _rate(captured, due)}
    return result


def _distribution(values: list[float], *, insufficient_threshold: int = 1) -> dict[str, Any]:
    if len(values) < insufficient_threshold:
        return {"sample_size": len(values), "insufficient_sample": True}
    if not values:
        return {
            "sample_size": 0,
            "insufficient_sample": False,
            "p25": None,
            "median": None,
            "p75": None,
            "p90": None,
        }
    ordered = sorted(values)
    return {
        "sample_size": len(values),
        "insufficient_sample": False,
        "p25": _percentile(ordered, 25.0),
        "median": median(ordered),
        "p75": _percentile(ordered, 75.0),
        "p90": _percentile(ordered, 90.0),
    }


def _percentile(ordered: list[float], percentile: float) -> float:
    if not ordered:
        return 0.0
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * percentile / 100.0
    lower = int(rank)
    upper = min(lower + 1, len(ordered) - 1)
    weight = rank - lower
    return ordered[lower] * (1.0 - weight) + ordered[upper] * weight


def _detection_price(detection: dict[str, Any]) -> float | None:
    features = detection.get("features", {})
    if isinstance(features, dict):
        price = _as_float_or_none(features.get("price_usd"))
        if price is not None:
            return price
    raw_pair = detection.get("raw_pair", {})
    if isinstance(raw_pair, dict):
        return _as_float_or_none(raw_pair.get("priceUsd"))
    return None


def _detection_liquidity(detection: dict[str, Any]) -> float | None:
    features = detection.get("features", {})
    if isinstance(features, dict):
        liquidity = _as_float_or_none(features.get("liquidity_usd"))
        if liquidity is not None:
            return liquidity
    raw_pair = detection.get("raw_pair", {})
    if isinstance(raw_pair, dict):
        liquidity = raw_pair.get("liquidity", {})
        if isinstance(liquidity, dict):
            return _as_float_or_none(liquidity.get("usd"))
    return None


def _load_jsonl(path: str | Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        lines = Path(path).read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    except OSError as exc:
        logger.warning(f"Unable to read {path}: {exc}")
        return []
    for line in lines:
        if not line.strip():
            continue
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            logger.warning(f"Skipping malformed JSONL row in {path}")
            continue
        if isinstance(item, dict):
            rows.append(item)
    return rows


def _pnl_row(label: str, dist: dict[str, Any], profitable_count: int) -> str:
    return (
        f"| {label} | {dist['sample_size']} | {profitable_count} | "
        f"{_fmt_pct(dist.get('p25'))} | {_fmt_pct(dist.get('median'))} | "
        f"{_fmt_pct(dist.get('p75'))} | {_fmt_pct(dist.get('p90'))} |"
    )


def _day_from_ms(ms: int) -> str:
    if ms <= 0:
        return "unknown"
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).date().isoformat()


def _offset_label(offset: int) -> str:
    labels = {
        300: "+5m",
        900: "+15m",
        3_600: "+1h",
        21_600: "+6h",
        86_400: "+24h",
        259_200: "+3d",
        604_800: "+7d",
    }
    return labels.get(offset, f"+{offset}s")


def _fmt_pct(value: Any) -> str:
    if value is None:
        return "n/a"
    try:
        return f"{float(value) * 100:.1f}%"
    except (TypeError, ValueError):
        return "n/a"


def _rate(count: int, total: int) -> float | None:
    if total <= 0:
        return None
    return count / total


def _as_int(value: Any) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def _as_float_or_none(value: Any) -> float | None:
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


if __name__ == "__main__":
    main()

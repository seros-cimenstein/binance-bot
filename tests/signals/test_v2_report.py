from __future__ import annotations

from typing import Any

import pytest

from src.signals import v2_report


NOW_MS = 1_800_000_000_000


def detection(
    detection_id: str,
    *,
    price: float = 1.0,
    liquidity: float = 10_000.0,
    detected_at_ms: int = NOW_MS - 8 * 24 * 3_600_000,
) -> dict[str, Any]:
    return {
        "detection_id": detection_id,
        "detected_at_ms": detected_at_ms,
        "filter_version": "v2-test",
        "features": {"price_usd": price, "liquidity_usd": liquidity},
    }


def snapshot(
    detection_id: str,
    offset: int,
    *,
    price: float | None = 1.0,
    liquidity: float | None = 10_000.0,
    pair_exists: bool = True,
    captured_at_ms: int = NOW_MS,
) -> dict[str, Any]:
    return {
        "detection_id": detection_id,
        "scheduled_offset_seconds": offset,
        "scheduled_for_ms": NOW_MS - 1_000,
        "captured_at_ms": captured_at_ms,
        "status": "ok" if pair_exists else "pair_gone",
        "pair_exists": pair_exists,
        "price_usd": price,
        "liquidity_usd": liquidity,
    }


def settings() -> v2_report.ReportSettings:
    return v2_report.ReportSettings()


def test_empty_ledger_report_renders_cleanly() -> None:
    metrics = v2_report.compute_metrics([], [], settings=settings(), now_ms=NOW_MS)
    markdown = v2_report.render_markdown(metrics, report_date="2026-06-27")

    assert metrics["detection_count"] == 0
    assert "No detections yet." in markdown


def test_synthetic_prices_compute_rug_2x_and_path_returns() -> None:
    detections = [detection("d1"), detection("d2")]
    snapshots = [
        snapshot("d1", 300, price=2.0, liquidity=8_000.0),
        snapshot("d2", 300, price=0.1, liquidity=500.0),
    ]

    metrics = v2_report.compute_metrics(detections, snapshots, settings=settings(), now_ms=NOW_MS)

    assert metrics["rug_rate"] == 0.5
    assert metrics["catastrophic_rate"] == 0.5
    assert metrics["two_x_rate"] == 0.5
    path = metrics["path_return_distribution"][300]
    assert path["sample_size"] == 2
    assert path["median"] == pytest.approx(0.05)
    assert path["p25"] == pytest.approx(-0.425)
    assert path["p75"] == pytest.approx(0.525)
    assert path["p90"] == pytest.approx(0.81)


def test_seven_day_insufficient_sample_marked() -> None:
    detections = [detection("d1")]
    snapshots = [snapshot("d1", 604_800, price=1.5, liquidity=11_000.0)]

    metrics = v2_report.compute_metrics(detections, snapshots, settings=settings(), now_ms=NOW_MS)

    assert metrics["path_return_distribution"][604_800]["insufficient_sample"] is True
    assert metrics["path_return_distribution"][604_800]["sample_size"] == 1


def test_friction_calc_matches_formula() -> None:
    result = v2_report.friction_adjusted_net_return(2.0, settings=settings())

    assert result == pytest.approx(2.0 * (1 - 0.02) ** 2 - 1 - 0.50 / 15.63)


def test_report_dedupes_snapshot_rows_keeping_latest() -> None:
    detections = [detection("d1")]
    snapshots = [
        snapshot("d1", 300, price=1.1, liquidity=9_000.0, captured_at_ms=NOW_MS),
        snapshot("d1", 300, price=3.0, liquidity=9_000.0, captured_at_ms=NOW_MS + 1),
    ]

    metrics = v2_report.compute_metrics(detections, snapshots, settings=settings(), now_ms=NOW_MS)

    assert metrics["two_x_rate"] == 1.0
    assert metrics["path_return_distribution"][300]["median"] == pytest.approx(2.0)


def test_snapshot_completeness_ignores_api_errors() -> None:
    detections = [detection("d1", detected_at_ms=NOW_MS - 300_000)]
    snapshots = [
        {
            "detection_id": "d1",
            "scheduled_offset_seconds": 300,
            "captured_at_ms": NOW_MS,
            "status": "api_error",
        }
    ]

    metrics = v2_report.compute_metrics(detections, snapshots, settings=settings(), now_ms=NOW_MS)

    assert metrics["snapshot_completeness"][300] == {"due": 1, "captured": 0, "rate": 0.0}

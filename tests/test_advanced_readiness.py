from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.advanced_readiness import (
    ADVANCED_CANARY_PLANS,
    AdvancedReadinessError,
    ProductReadinessEvidence,
    build_advanced_readiness_report,
    ensure_serial_live_canary_execution,
    render_advanced_readiness_report,
    run,
)


def _reviewed(*, approved: bool = False) -> ProductReadinessEvidence:
    return ProductReadinessEvidence(
        focused_tests_passed=True,
        full_tests_passed=True,
        replay_passed=True,
        dry_run_report_reviewed=True,
        permission_diagnostics_reviewed=True,
        canary_plan_reviewed=True,
        audit_reviewed=True,
        explicit_execution_approved=approved,
    )


def test_complete_evidence_does_not_enable_live_mutation_or_automation() -> None:
    report = build_advanced_readiness_report(
        {module_id: _reviewed(approved=True) for module_id in ADVANCED_CANARY_PLANS}
    )

    assert report.all_plan_reviews_complete is True
    assert report.automation_enabled is False
    assert report.live_mutation_enabled is False
    assert all(check.live_canary_allowed is False for check in report.products)
    assert all(check.execution_approval_recorded is True for check in report.products)


def test_missing_evidence_is_reported_per_product() -> None:
    report = build_advanced_readiness_report(
        {"margin_canary_plan": ProductReadinessEvidence(focused_tests_passed=True)}
    )

    margin = report.products[0]
    assert margin.module_id == "margin_canary_plan"
    assert margin.plan_review_complete is False
    assert "full_tests_passed" in margin.missing_evidence
    assert "transfer_canary_plan" in render_advanced_readiness_report(report)


def test_mutable_canary_requests_must_be_serial() -> None:
    report = build_advanced_readiness_report(
        requested_live_canaries=("margin_canary_plan", "futures_canary_plan")
    )

    assert report.serial_execution_allowed is False
    with pytest.raises(AdvancedReadinessError, match="execute serially"):
        ensure_serial_live_canary_execution(report)


def test_cli_reads_local_evidence_and_fails_on_serial_conflict(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    evidence_path = tmp_path / "readiness.json"
    evidence_path.write_text(
        json.dumps({"margin_canary_plan": _reviewed().as_dict()}),
        encoding="utf-8",
    )

    assert run(["--evidence-file", str(evidence_path), "--json"]) == 0
    assert '"live_mutation_enabled": false' in capsys.readouterr().out
    assert (
        run(
            [
                "--requested-live-canary",
                "margin_canary_plan",
                "--requested-live-canary",
                "transfer_canary_plan",
                "--fail-on-serial-conflict",
            ]
        )
        == 1
    )


def test_background_service_units_do_not_enable_advanced_canaries() -> None:
    deploy_root = Path(__file__).resolve().parents[1] / "deploy"
    service_text = "\n".join(
        path.read_text(encoding="utf-8")
        for path in deploy_root.rglob("*.service")
    )

    for module_id in ADVANCED_CANARY_PLANS:
        assert module_id not in service_text

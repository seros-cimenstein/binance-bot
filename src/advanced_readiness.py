"""Local readiness gate for advanced-product canary planning.

This module records review evidence only. It does not call Binance and cannot
enable margin, futures, or universal-transfer write endpoints.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Mapping, Sequence

from src.product_modules import get_product_module

ADVANCED_CANARY_PLANS = (
    "margin_canary_plan",
    "futures_canary_plan",
    "transfer_canary_plan",
)
EVIDENCE_FIELDS = (
    "focused_tests_passed",
    "full_tests_passed",
    "replay_passed",
    "dry_run_report_reviewed",
    "permission_diagnostics_reviewed",
    "canary_plan_reviewed",
    "audit_reviewed",
    "explicit_execution_approved",
)


class AdvancedReadinessError(RuntimeError):
    """Raised when readiness evidence or serial-execution policy is invalid."""


@dataclass(frozen=True, slots=True)
class ProductReadinessEvidence:
    """Operator-reviewed evidence for one advanced-product canary."""

    focused_tests_passed: bool = False
    full_tests_passed: bool = False
    replay_passed: bool = False
    dry_run_report_reviewed: bool = False
    permission_diagnostics_reviewed: bool = False
    canary_plan_reviewed: bool = False
    audit_reviewed: bool = False
    explicit_execution_approved: bool = False

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ProductReadinessCheck:
    """Readiness outcome for one advanced-product plan module."""

    module_id: str
    evidence: dict[str, Any]
    missing_evidence: tuple[str, ...]
    plan_review_complete: bool
    execution_approval_recorded: bool
    live_mutation_enabled: bool
    live_canary_allowed: bool
    detail: str

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class AdvancedReadinessReport:
    """Cross-product local readiness and serial-execution result."""

    products: tuple[ProductReadinessCheck, ...]
    requested_live_canaries: tuple[str, ...]
    serial_execution_allowed: bool
    automation_enabled: bool = False
    live_mutation_enabled: bool = False

    @property
    def all_plan_reviews_complete(self) -> bool:
        return all(product.plan_review_complete for product in self.products)

    def as_dict(self) -> dict[str, Any]:
        return {
            "products": [product.as_dict() for product in self.products],
            "requested_live_canaries": list(self.requested_live_canaries),
            "serial_execution_allowed": self.serial_execution_allowed,
            "all_plan_reviews_complete": self.all_plan_reviews_complete,
            "automation_enabled": self.automation_enabled,
            "live_mutation_enabled": self.live_mutation_enabled,
        }


def build_advanced_readiness_report(
    evidence: Mapping[str, ProductReadinessEvidence | Mapping[str, Any]] | None = None,
    *,
    requested_live_canaries: Sequence[str] = (),
) -> AdvancedReadinessReport:
    """Build a local readiness report without exchange access."""
    evidence = evidence or {}
    normalized_requested = tuple(
        _module_id(module_id) for module_id in requested_live_canaries
    )
    serial_execution_allowed = len(set(normalized_requested)) <= 1
    checks = tuple(
        _evaluate_product(module_id, evidence.get(module_id))
        for module_id in ADVANCED_CANARY_PLANS
    )
    return AdvancedReadinessReport(
        products=checks,
        requested_live_canaries=normalized_requested,
        serial_execution_allowed=serial_execution_allowed,
    )


def ensure_serial_live_canary_execution(report: AdvancedReadinessReport) -> None:
    """Reject concurrent state-mutating canary approvals."""
    if not report.serial_execution_allowed:
        raise AdvancedReadinessError(
            "advanced live canaries must execute serially; "
            "only one mutable product may be requested at a time"
        )


def render_advanced_readiness_report(report: AdvancedReadinessReport) -> str:
    """Render a concise advanced-product readiness report."""
    lines = [
        "Advanced product readiness:",
        f"Serial live canary rule: {'pass' if report.serial_execution_allowed else 'blocked'}",
        "Live mutation enabled: no",
        "Background automation enabled: no",
    ]
    for check in report.products:
        missing = ", ".join(check.missing_evidence) or "none"
        lines.extend(
            [
                f"  - {check.module_id}:",
                f"    plan review complete: {'yes' if check.plan_review_complete else 'no'}",
                f"    missing evidence: {missing}",
                "    live canary allowed: no",
            ]
        )
    return "\n".join(lines)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Review local evidence for advanced-product canary plans."
    )
    parser.add_argument("--evidence-file", help="JSON evidence mapping")
    parser.add_argument(
        "--requested-live-canary",
        action="append",
        choices=ADVANCED_CANARY_PLANS,
        help="record a proposed mutable canary for serial-execution review",
    )
    parser.add_argument(
        "--fail-on-serial-conflict",
        action="store_true",
        help="return non-zero when more than one mutable canary is proposed",
    )
    parser.add_argument("--json", action="store_true", help="print JSON output")
    args = parser.parse_args(argv)
    try:
        evidence = _load_evidence(args.evidence_file)
        report = build_advanced_readiness_report(
            evidence,
            requested_live_canaries=args.requested_live_canary or (),
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_advanced_readiness_report(report))
        if args.fail_on_serial_conflict:
            ensure_serial_live_canary_execution(report)
    except Exception as exc:
        print(f"advanced readiness check failed: {exc}")
        return 1
    return 0


def _evaluate_product(
    module_id: str,
    evidence: ProductReadinessEvidence | Mapping[str, Any] | None,
) -> ProductReadinessCheck:
    module = get_product_module(module_id)
    normalized = _coerce_evidence(evidence)
    values = normalized.as_dict()
    missing = tuple(field for field in EVIDENCE_FIELDS if not values[field])
    plan_missing = tuple(
        field for field in EVIDENCE_FIELDS if field != "explicit_execution_approved"
        and not values[field]
    )
    detail = (
        "review evidence is complete, but this planning module exposes no live "
        "mutation endpoint"
        if not plan_missing
        else "canary planning review evidence is incomplete"
    )
    return ProductReadinessCheck(
        module_id=module.module_id,
        evidence=values,
        missing_evidence=missing,
        plan_review_complete=not plan_missing,
        execution_approval_recorded=normalized.explicit_execution_approved,
        live_mutation_enabled=module.live_activation_enabled,
        live_canary_allowed=False,
        detail=detail,
    )


def _coerce_evidence(
    evidence: ProductReadinessEvidence | Mapping[str, Any] | None,
) -> ProductReadinessEvidence:
    if evidence is None:
        return ProductReadinessEvidence()
    if isinstance(evidence, ProductReadinessEvidence):
        return evidence
    if not isinstance(evidence, Mapping):
        raise AdvancedReadinessError("product evidence must be an object")
    unknown = set(evidence) - set(EVIDENCE_FIELDS)
    if unknown:
        raise AdvancedReadinessError(
            "unknown product evidence fields: " + ", ".join(sorted(unknown))
        )
    return ProductReadinessEvidence(
        **{field: bool(evidence.get(field, False)) for field in EVIDENCE_FIELDS}
    )


def _load_evidence(
    path: str | None,
) -> dict[str, ProductReadinessEvidence | Mapping[str, Any]]:
    if not path:
        return {}
    with Path(path).open("r", encoding="utf-8") as handle:
        raw = json.load(handle)
    if not isinstance(raw, Mapping):
        raise AdvancedReadinessError("evidence file must contain an object")
    unknown = set(raw) - set(ADVANCED_CANARY_PLANS)
    if unknown:
        raise AdvancedReadinessError(
            "unknown advanced canary modules: " + ", ".join(sorted(unknown))
        )
    return dict(raw)


def _module_id(value: str) -> str:
    normalized = value.strip().lower().replace("-", "_")
    if normalized not in ADVANCED_CANARY_PLANS:
        raise AdvancedReadinessError(f"unknown advanced canary module: {value}")
    return normalized


if __name__ == "__main__":
    raise SystemExit(run())

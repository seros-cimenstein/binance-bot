"""Read-only evidence report for the paper-only Spot allocation pool."""

from __future__ import annotations

import argparse
import json
import sqlite3
from collections import Counter, defaultdict
from dataclasses import asdict, dataclass
from pathlib import Path
from urllib.parse import quote

from src.pool.scoring import build_scorer


class PaperReportError(RuntimeError):
    """Raised when a paper-ledger report cannot be built safely."""


@dataclass(frozen=True, slots=True)
class CandidateMarket:
    """Currently eligible asset represented by its most liquid market."""

    asset: str
    routes: int
    best_quote_volume_usdt: float


@dataclass(frozen=True, slots=True)
class ScoredCandidate:
    """A candidate ranked from the retained paper observation window."""

    asset: str
    score: float


@dataclass(frozen=True, slots=True)
class LatestDecision:
    """Most recently persisted paper decision."""

    decided_at: str
    portfolio_id: str
    action: str
    reason: str
    source_asset: str | None
    destination_asset: str | None


@dataclass(frozen=True, slots=True)
class PaperPoolReport:
    """Read-only view over one paper pool ledger."""

    db_path: str
    required_observations: int
    completed_cycles: int
    latest_cycle_at: str | None
    total_markets: int
    eligible_markets: int
    eligible_assets: int
    balances: dict[str, dict[str, float]]
    decision_counts: dict[str, int]
    decision_reasons: dict[str, int]
    latest_decisions: tuple[LatestDecision, ...]
    top_eligible_assets: tuple[CandidateMarket, ...]
    rejection_reasons: dict[str, int]
    rankings: dict[str, tuple[ScoredCandidate, ...]]

    @property
    def remaining_observations(self) -> int:
        return max(self.required_observations - self.completed_cycles, 0)

    @property
    def ranking_available(self) -> bool:
        return any(self.rankings.values())

    def as_dict(self) -> dict[str, object]:
        payload = asdict(self)
        payload["remaining_observations"] = self.remaining_observations
        payload["ranking_available"] = self.ranking_available
        return payload


def build_report(
    db_path: str | Path,
    *,
    required_observations: int = 96,
    benchmarks: tuple[str, ...] = ("USDT", "BTC", "PAXG", "XAUT"),
    benchmark_weights: tuple[float, ...] = (0.25, 0.25, 0.25, 0.25),
    scorers: tuple[str, ...] = ("simple_momentum", "risk_adjusted_momentum"),
    limit: int = 10,
) -> PaperPoolReport:
    """Read persisted paper evidence without opening the ledger for writes."""
    path = Path(db_path).expanduser()
    if not path.is_file():
        raise PaperReportError(f"paper ledger does not exist: {path}")
    if required_observations <= 0:
        raise PaperReportError("required observations must be positive")
    if limit <= 0:
        raise PaperReportError("report limit must be positive")
    if len(benchmarks) != len(benchmark_weights) or not benchmarks:
        raise PaperReportError("benchmarks and weights must have matching non-empty values")

    normalized_benchmarks = tuple(value.strip().upper() for value in benchmarks)
    weights = dict(zip(normalized_benchmarks, benchmark_weights))
    uri = f"file:{quote(str(path.resolve()))}?mode=ro"
    try:
        with sqlite3.connect(uri, uri=True) as connection:
            connection.row_factory = sqlite3.Row
            cycles_row = connection.execute(
                "SELECT COUNT(*) AS count, MAX(observed_at) AS latest "
                "FROM market_catalog_cycles"
            ).fetchone()
            market_row = connection.execute(
                """
                SELECT COUNT(*) AS total,
                       COALESCE(SUM(eligible), 0) AS eligible,
                       COUNT(DISTINCT CASE WHEN eligible = 1 THEN base_asset END) AS assets
                FROM market_catalog_latest
                """
            ).fetchone()
            balances = _read_balances(connection)
            decision_counts, decision_reasons, latest_decisions = _read_decisions(
                connection,
                latest_limit=len(scorers),
                reason_limit=limit,
            )
            top_assets = _read_top_assets(connection, limit=limit)
            rejections = _read_rejection_reasons(connection, limit=limit)
            rankings = _read_rankings(
                connection,
                required_observations=required_observations,
                benchmarks=normalized_benchmarks,
                weights=weights,
                scorers=scorers,
                limit=limit,
            )
    except sqlite3.Error as exc:
        raise PaperReportError(f"failed to read paper ledger: {exc}") from exc

    return PaperPoolReport(
        db_path=str(path),
        required_observations=required_observations,
        completed_cycles=int(cycles_row["count"]),
        latest_cycle_at=cycles_row["latest"],
        total_markets=int(market_row["total"]),
        eligible_markets=int(market_row["eligible"]),
        eligible_assets=int(market_row["assets"]),
        balances=balances,
        decision_counts=decision_counts,
        decision_reasons=decision_reasons,
        latest_decisions=latest_decisions,
        top_eligible_assets=top_assets,
        rejection_reasons=rejections,
        rankings=rankings,
    )


def render_report(report: PaperPoolReport) -> str:
    """Render an operator-facing explanation of current paper evidence."""
    lines = [
        "Paper Spot pool report: read-only",
        f"Ledger: {report.db_path}",
        (
            f"Warm-up cycles: {report.completed_cycles}/{report.required_observations} "
            f"({report.remaining_observations} remaining)"
        ),
        f"Latest cycle: {report.latest_cycle_at or 'none'}",
        (
            f"Markets: {report.eligible_markets}/{report.total_markets} eligible; "
            f"{report.eligible_assets} candidate assets"
        ),
        "Balances:",
    ]
    for portfolio_id, balances in sorted(report.balances.items()):
        holdings = ", ".join(
            f"{asset}={amount:.12g}" for asset, amount in sorted(balances.items())
        )
        lines.append(f"  {portfolio_id}: {holdings or 'none'}")

    counts = ", ".join(
        f"{action}={count}" for action, count in sorted(report.decision_counts.items())
    )
    lines.append(f"Decisions: {counts or 'none'}")
    if report.decision_reasons:
        lines.append("Decision reasons:")
        for reason, count in report.decision_reasons.items():
            lines.append(f"  {count}: {reason}")
    if report.latest_decisions:
        lines.append("Latest decisions:")
        for decision in report.latest_decisions:
            lines.append(
                f"  {decision.portfolio_id}: {decision.action} - {decision.reason}"
            )

    if report.ranking_available:
        lines.append("Ranked candidates from retained observations:")
        for scorer, candidates in report.rankings.items():
            values = ", ".join(
                f"{candidate.asset} ({candidate.score:.6g})" for candidate in candidates
            )
            lines.append(f"  {scorer}: {values or 'none'}")
    else:
        lines.append(
            "Ranked candidates: unavailable until candidate assets have the required "
            "benchmark history."
        )

    if report.top_eligible_assets:
        lines.append("Current most-liquid eligible candidate assets:")
        lines.append(
            "  "
            + ", ".join(
                f"{candidate.asset} ({candidate.best_quote_volume_usdt:.2f} USDT)"
                for candidate in report.top_eligible_assets
            )
        )
    if report.rejection_reasons:
        lines.append("Most common market rejection reasons:")
        lines.append(
            "  "
            + ", ".join(
                f"{reason}={count}" for reason, count in report.rejection_reasons.items()
            )
        )
    lines.append("Live execution: disabled; this command only reads the paper ledger.")
    return "\n".join(lines)


def _read_balances(connection: sqlite3.Connection) -> dict[str, dict[str, float]]:
    balances: dict[str, dict[str, float]] = defaultdict(dict)
    rows = connection.execute(
        "SELECT portfolio_id, asset, amount FROM virtual_balances "
        "ORDER BY portfolio_id, asset"
    ).fetchall()
    for row in rows:
        balances[str(row["portfolio_id"])][str(row["asset"])] = float(row["amount"])
    return dict(balances)


def _read_decisions(
    connection: sqlite3.Connection,
    *,
    latest_limit: int,
    reason_limit: int,
) -> tuple[dict[str, int], dict[str, int], tuple[LatestDecision, ...]]:
    counts = {
        str(row["action"]): int(row["count"])
        for row in connection.execute(
            "SELECT action, COUNT(*) AS count FROM paper_decisions GROUP BY action"
        )
    }
    reasons = {
        str(row["reason"]): int(row["count"])
        for row in connection.execute(
            "SELECT reason, COUNT(*) AS count FROM paper_decisions "
            "GROUP BY reason ORDER BY count DESC, reason LIMIT ?",
            (reason_limit,),
        )
    }
    rows = connection.execute(
        """
        SELECT decided_at, portfolio_id, action, reason, source_asset, destination_asset
        FROM paper_decisions
        ORDER BY decided_at DESC, portfolio_id
        LIMIT ?
        """,
        (latest_limit,),
    ).fetchall()
    latest = tuple(
        LatestDecision(
            decided_at=str(row["decided_at"]),
            portfolio_id=str(row["portfolio_id"]),
            action=str(row["action"]),
            reason=str(row["reason"]),
            source_asset=row["source_asset"],
            destination_asset=row["destination_asset"],
        )
        for row in rows
    )
    return counts, reasons, latest


def _read_top_assets(
    connection: sqlite3.Connection,
    *,
    limit: int,
) -> tuple[CandidateMarket, ...]:
    rows = connection.execute(
        """
        SELECT base_asset,
               COUNT(*) AS routes,
               MAX(COALESCE(quote_volume_usdt, 0)) AS best_volume
        FROM market_catalog_latest
        WHERE eligible = 1
        GROUP BY base_asset
        ORDER BY best_volume DESC, base_asset
        LIMIT ?
        """,
        (limit,),
    ).fetchall()
    return tuple(
        CandidateMarket(
            asset=str(row["base_asset"]),
            routes=int(row["routes"]),
            best_quote_volume_usdt=float(row["best_volume"]),
        )
        for row in rows
    )


def _read_rejection_reasons(
    connection: sqlite3.Connection,
    *,
    limit: int,
) -> dict[str, int]:
    counts: Counter[str] = Counter()
    rows = connection.execute(
        "SELECT rejection_reasons FROM market_catalog_latest WHERE eligible = 0"
    ).fetchall()
    for row in rows:
        try:
            reasons = json.loads(str(row["rejection_reasons"]))
        except json.JSONDecodeError:
            reasons = ["invalid persisted rejection reason"]
        for reason in reasons:
            counts[str(reason)] += 1
    return dict(counts.most_common(limit))


def _read_rankings(
    connection: sqlite3.Connection,
    *,
    required_observations: int,
    benchmarks: tuple[str, ...],
    weights: dict[str, float],
    scorers: tuple[str, ...],
    limit: int,
) -> dict[str, tuple[ScoredCandidate, ...]]:
    eligible_assets = {
        str(row["base_asset"])
        for row in connection.execute(
            "SELECT DISTINCT base_asset FROM market_catalog_latest WHERE eligible = 1"
        )
    }
    rows = connection.execute(
        """
        SELECT asset, benchmark, observed_at, rate
        FROM pool_observations
        ORDER BY asset, benchmark, observed_at
        """
    ).fetchall()
    observations: dict[str, dict[str, list[float]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for row in rows:
        asset = str(row["asset"])
        if asset in eligible_assets and str(row["benchmark"]) in benchmarks:
            observations[asset][str(row["benchmark"])].append(float(row["rate"]))

    rankings: dict[str, tuple[ScoredCandidate, ...]] = {}
    for scorer_name in scorers:
        scorer = build_scorer(scorer_name)
        scored: list[ScoredCandidate] = []
        for asset, history in observations.items():
            if any(len(history.get(benchmark, [])) < required_observations for benchmark in benchmarks):
                continue
            result = scorer.score(
                asset,
                {
                    benchmark: history[benchmark][-required_observations:]
                    for benchmark in benchmarks
                },
                weights,
            )
            if result is not None:
                scored.append(ScoredCandidate(asset=result.asset, score=result.score))
        scored.sort(key=lambda item: item.score, reverse=True)
        rankings[scorer_name] = tuple(scored[:limit])
    return rankings


def main() -> None:
    parser = argparse.ArgumentParser(description="Read paper-pool decision evidence")
    parser.add_argument("--db-path", default="data/pool-paper.db")
    parser.add_argument("--required-observations", type=int, default=96)
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--json", action="store_true", help="render structured JSON")
    args = parser.parse_args()
    try:
        report = build_report(
            args.db_path,
            required_observations=args.required_observations,
            limit=args.limit,
        )
    except PaperReportError as exc:
        raise SystemExit(f"paper pool report failed: {exc}") from exc
    if args.json:
        print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
    else:
        print(render_report(report))


if __name__ == "__main__":
    main()

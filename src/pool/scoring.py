"""Paper-only candidate scorers for modular pool evaluation."""

from __future__ import annotations

import math
import statistics
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class AssetScore:
    """One benchmark-basket-relative candidate score."""

    asset: str
    score: float


class MomentumScorer:
    """Simple equal-benchmark momentum scorer."""

    name = "simple_momentum"

    def score(
        self,
        asset: str,
        history: dict[str, list[float]],
        weights: dict[str, float],
    ) -> AssetScore | None:
        returns = _benchmark_returns(history, weights)
        if returns is None:
            return None
        return AssetScore(
            asset=asset,
            score=sum(returns[benchmark] * weights[benchmark] for benchmark in weights),
        )


class RiskAdjustedMomentumScorer(MomentumScorer):
    """Momentum scorer penalized by benchmark-relative realized volatility."""

    name = "risk_adjusted_momentum"

    def score(
        self,
        asset: str,
        history: dict[str, list[float]],
        weights: dict[str, float],
    ) -> AssetScore | None:
        momentum = super().score(asset, history, weights)
        if momentum is None:
            return None
        period_returns: list[float] = []
        for benchmark, values in history.items():
            if benchmark not in weights:
                continue
            for previous, current in zip(values, values[1:]):
                if previous > 0:
                    period_returns.append((current / previous) - 1)
        volatility = statistics.pstdev(period_returns) if len(period_returns) > 1 else 0.0
        return AssetScore(asset=asset, score=momentum.score - volatility)


def build_scorer(name: str) -> MomentumScorer:
    """Build one configured paper scorer."""
    if name == MomentumScorer.name:
        return MomentumScorer()
    if name == RiskAdjustedMomentumScorer.name:
        return RiskAdjustedMomentumScorer()
    raise ValueError(f"unsupported paper scorer: {name}")


def basket_performance(
    current: dict[str, float],
    initial: dict[str, float],
    weights: dict[str, float],
) -> float | None:
    """Return weighted return of one portfolio against all benchmarks."""
    if not initial or any(initial.get(benchmark, 0) <= 0 for benchmark in weights):
        return None
    values = []
    for benchmark, weight in weights.items():
        latest = current.get(benchmark)
        if latest is None or not math.isfinite(latest):
            return None
        values.append(((latest / initial[benchmark]) - 1) * weight)
    return sum(values)


def _benchmark_returns(
    history: dict[str, list[float]],
    weights: dict[str, float],
) -> dict[str, float] | None:
    returns: dict[str, float] = {}
    for benchmark in weights:
        values = history.get(benchmark, [])
        if len(values) < 2 or values[0] <= 0 or not all(
            math.isfinite(value) and value > 0 for value in values
        ):
            return None
        returns[benchmark] = (values[-1] / values[0]) - 1
    return returns

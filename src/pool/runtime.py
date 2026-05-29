"""Paper-only portfolio cycle over the dynamically discovered Binance universe."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Any, Callable

from loguru import logger

from src.pool.catalog import MarketCatalog
from src.pool.config import PoolSettings
from src.pool.fees import CommissionService, estimate_bnb_reserve
from src.pool.ledger import PaperLedger
from src.pool.models import PaperDecision, VirtualPortfolio
from src.pool.routes import RouteEngine
from src.pool.scoring import AssetScore, basket_performance, build_scorer


@dataclass(frozen=True, slots=True)
class CycleReport:
    """Observable outcome of one paper-pool evaluation cycle."""

    observed_at: str
    catalog_markets: int
    eligible_markets: int
    seeded_balances: dict[str, float]
    benchmark_totals: dict[str, dict[str, float]]
    decisions: tuple[PaperDecision, ...]


class PaperPoolRuntime:
    """Run paper portfolios without owning any order-submission capability."""

    def __init__(
        self,
        client: Any,
        settings: PoolSettings,
        *,
        ledger: PaperLedger | None = None,
        clock: Callable[[], datetime] | None = None,
    ) -> None:
        self._client = client
        self.settings = settings
        self.ledger = ledger or PaperLedger(settings.db_path)
        self.catalog = MarketCatalog(client, settings)
        self.fees = CommissionService(client, cache_seconds=settings.fee_cache_seconds)
        self._clock = clock or (lambda: datetime.now(timezone.utc))

    async def cycle(self) -> CycleReport:
        """Discover, value, score, and optionally simulate one reallocation."""
        observed_at = self._clock().isoformat()
        markets = await self.catalog.refresh()
        self.ledger.record_catalog(observed_at, markets)
        routes = RouteEngine(markets, self.fees, self.settings)
        initial_balances = await self._read_nonzero_holdings()
        portfolios = [
            self.ledger.seed_if_absent(scorer, scorer, initial_balances)
            for scorer in self.settings.scorers
        ]

        observation_assets = set(self.catalog.scoring_assets())
        observation_assets.update(initial_balances)
        observation_assets.update(self.settings.benchmarks)
        rates = self._indicative_rates(routes, observation_assets)
        self.ledger.record_observations(
            observed_at,
            rates,
            retain_latest=self.settings.momentum_observations,
        )

        totals_by_portfolio: dict[str, dict[str, float]] = {}
        decisions: list[PaperDecision] = []
        weights = dict(zip(self.settings.benchmarks, self.settings.benchmark_weights))
        for portfolio in portfolios:
            totals = self._value_balances(portfolio.balances, routes)
            initial_totals = self.ledger.initial_totals(portfolio.portfolio_id)
            score = basket_performance(totals, initial_totals, weights)
            self.ledger.record_snapshot(observed_at, portfolio.portfolio_id, totals, score)
            if not initial_totals:
                score = 0.0
            totals_by_portfolio[portfolio.portfolio_id] = totals
            decision = await self._decide(
                portfolio,
                routes,
                rates,
                totals,
                score,
                weights,
            )
            self.ledger.record_decision(decision)
            if decision.action == "rebalance" and decision.route is not None:
                self.ledger.apply_route(portfolio.portfolio_id, decision.route)
            decisions.append(decision)

        report = CycleReport(
            observed_at=observed_at,
            catalog_markets=len(markets),
            eligible_markets=sum(market.eligible for market in markets.values()),
            seeded_balances=initial_balances,
            benchmark_totals=totals_by_portfolio,
            decisions=tuple(decisions),
        )
        logger.bind(
            catalog_markets=report.catalog_markets,
            eligible_markets=report.eligible_markets,
            seeded_assets=sorted(report.seeded_balances),
            decisions=[decision.action for decision in decisions],
        ).info("Paper pool evaluation cycle completed")
        return report

    async def _read_nonzero_holdings(self) -> dict[str, float]:
        raw = await self._client.get_balance()
        totals = raw.get("total", {})
        return {
            str(asset).upper(): float(amount)
            for asset, amount in totals.items()
            if float(amount or 0.0) > 0
        }

    def _indicative_rates(
        self,
        routes: RouteEngine,
        assets: set[str],
    ) -> dict[tuple[str, str], float]:
        rates: dict[tuple[str, str], float] = {}
        for asset in sorted(assets):
            for benchmark in self.settings.benchmarks:
                rate = 1.0 if asset == benchmark else routes.indicative_rate(asset, benchmark)
                if rate is not None and rate > 0:
                    rates[(asset, benchmark)] = rate
        return rates

    def _value_balances(
        self,
        balances: dict[str, float],
        routes: RouteEngine,
    ) -> dict[str, float]:
        totals = {benchmark: 0.0 for benchmark in self.settings.benchmarks}
        for asset, amount in balances.items():
            if amount <= 0:
                continue
            for benchmark in self.settings.benchmarks:
                rate = 1.0 if asset == benchmark else routes.indicative_rate(asset, benchmark)
                if rate is not None:
                    totals[benchmark] += amount * rate
        return totals

    async def _decide(
        self,
        portfolio: VirtualPortfolio,
        routes: RouteEngine,
        rates: dict[tuple[str, str], float],
        totals: dict[str, float],
        basket_score: float | None,
        weights: dict[str, float],
    ) -> PaperDecision:
        scorer = build_scorer(portfolio.scorer_name)
        scored: list[AssetScore] = []
        for asset in self.catalog.scoring_assets():
            history = self.ledger.observation_history(
                asset,
                self.settings.benchmarks,
                limit=self.settings.momentum_observations,
            )
            if any(
                len(history.get(benchmark, [])) < self.settings.momentum_observations
                for benchmark in self.settings.benchmarks
            ):
                continue
            result = scorer.score(asset, history, weights)
            if result is not None:
                scored.append(result)
        if not scored:
            return self._hold(
                portfolio,
                "insufficient benchmark history for paper ranking "
                f"(requires {self.settings.momentum_observations} cycles)",
            )
        if basket_score is not None and basket_score < -(self.settings.max_drawdown_percent / 100):
            return self._hold(portfolio, "paper portfolio drawdown gate is active")
        scored.sort(key=lambda item: item.score, reverse=True)
        target = scored[0]
        source = self._largest_holding(portfolio.balances, rates)
        if source is None or source == target.asset:
            return self._hold(portfolio, "highest-ranked asset is already the largest holding", target.score)
        source_score = next((item.score for item in scored if item.asset == source), None)
        if source_score is not None and (
            target.score - source_score
            <= self.settings.min_improvement_bps / 10_000
        ):
            return self._hold(portfolio, "rank advantage does not clear turnover threshold", target.score)
        last_rebalance = self.ledger.last_rebalance_at(portfolio.portfolio_id)
        if last_rebalance is not None and (
            self._clock() - last_rebalance
            < timedelta(seconds=self.settings.decision_cooldown_seconds)
        ):
            return self._hold(portfolio, "reallocation cooldown is active", target.score)

        amount = (
            portfolio.balances[source] * self.settings.max_reallocation_percent / 100
        )
        route = await routes.quote(
            source,
            target.asset,
            amount,
            apply_bnb_discount=portfolio.balances.get("BNB", 0.0)
            >= self.settings.min_bnb_fee_reserve,
        )
        if route is None:
            return self._hold(
                portfolio,
                "candidate reallocation does not satisfy routed market minimums or costs",
                target.score,
            )
        destination_to_bnb = {
            leg.destination_asset: rate
            for leg in route.legs
            if (rate := routes.indicative_rate(leg.destination_asset, "BNB")) is not None
        }
        reserve = estimate_bnb_reserve(
            [route],
            destination_to_bnb=destination_to_bnb,
            multiplier=self.settings.bnb_fee_buffer_multiplier,
            floor_bnb=self.settings.min_bnb_fee_reserve,
        )
        remaining_bnb = portfolio.balances.get("BNB", 0.0) - (
            route.input_amount if source == "BNB" else 0.0
        )
        if any(leg.commission_discount_asset == "BNB" for leg in route.legs) and (
            remaining_bnb < reserve
        ):
            route = await routes.quote(source, target.asset, amount, apply_bnb_discount=False)
            if route is None:
                return self._hold(portfolio, "route fails without available BNB fee reserve", target.score)
        usdt_rate = rates.get((target.asset, "USDT"))
        total_usdt = totals.get("USDT", 0.0)
        if usdt_rate and total_usdt > 0:
            destination_after = (
                portfolio.balances.get(target.asset, 0.0) + route.output_amount
            ) * usdt_rate
            if destination_after > total_usdt * self.settings.max_allocation_percent / 100:
                return self._hold(portfolio, "destination allocation cap would be exceeded", target.score)
        return PaperDecision(
            portfolio_id=portfolio.portfolio_id,
            scorer_name=portfolio.scorer_name,
            source_asset=source,
            destination_asset=target.asset,
            input_amount=amount,
            route=route,
            action="rebalance",
            reason="simulated reallocation passed route, cost, reserve, and allocation gates",
            score=target.score,
            decided_at=self._clock(),
        )

    @staticmethod
    def _largest_holding(
        balances: dict[str, float],
        rates: dict[tuple[str, str], float],
    ) -> str | None:
        valued = [
            (amount * rates.get((asset, "USDT"), 0.0), asset)
            for asset, amount in balances.items()
            if amount > 0
        ]
        return max(valued)[1] if valued else None

    def _hold(
        self,
        portfolio: VirtualPortfolio,
        reason: str,
        score: float | None = None,
    ) -> PaperDecision:
        return PaperDecision(
            portfolio_id=portfolio.portfolio_id,
            scorer_name=portfolio.scorer_name,
            source_asset=None,
            destination_asset=None,
            input_amount=None,
            route=None,
            action="hold",
            reason=reason,
            score=score,
            decided_at=self._clock(),
        )

    def close(self) -> None:
        self.ledger.close()

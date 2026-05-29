"""Bounded conversion-graph search with dynamic rule and fee costing."""

from __future__ import annotations

import heapq
import math
from collections import defaultdict
from decimal import Decimal, ROUND_DOWN
from itertools import count

from src.pool.config import PoolSettings
from src.pool.fees import CommissionService
from src.pool.models import MarketSnapshot, RouteLeg, RouteQuote


class RouteEngine:
    """Find the best valid simulated route between two Binance assets."""

    def __init__(
        self,
        markets: dict[str, MarketSnapshot],
        fees: CommissionService,
        settings: PoolSettings,
    ) -> None:
        self._settings = settings
        self._fees = fees
        self._adjacency: dict[str, list[tuple[MarketSnapshot, str, str]]] = defaultdict(list)
        for market in markets.values():
            if not market.eligible:
                continue
            self._adjacency[market.base].append((market, "sell", market.quote))
            self._adjacency[market.quote].append((market, "buy", market.base))

    def gross_quote(
        self,
        source_asset: str,
        destination_asset: str,
        amount: float,
    ) -> float | None:
        """Return the best rule-valid output before commissions."""
        candidates = self._candidate_paths(source_asset, destination_asset, amount)
        return max((candidate[0] for candidate in candidates), default=None)

    def indicative_rate(self, source_asset: str, destination_asset: str) -> float | None:
        """Return an order-rule-independent mark rate for portfolio reporting."""
        candidates = self._candidate_paths(
            source_asset,
            destination_asset,
            1.0,
            enforce_order_rules=False,
        )
        return max((candidate[0] for candidate in candidates), default=None)

    async def quote(
        self,
        source_asset: str,
        destination_asset: str,
        amount: float,
        *,
        apply_bnb_discount: bool = True,
    ) -> RouteQuote | None:
        """Return the best post-fee route using authenticated commissions."""
        candidates = self._candidate_paths(source_asset, destination_asset, amount)
        best: RouteQuote | None = None
        selected = list(candidates[: self._settings.max_candidate_routes])
        for leg_count in range(1, self._settings.max_route_legs + 1):
            shortest_class = next(
                (candidate for candidate in candidates if len(candidate[1]) == leg_count),
                None,
            )
            if shortest_class is not None and shortest_class not in selected:
                selected.append(shortest_class)
        for _, path in selected:
            route = await self._cost_path(
                source_asset,
                destination_asset,
                amount,
                path,
                apply_bnb_discount=apply_bnb_discount,
            )
            if route is not None and (
                best is None or route.output_amount > best.output_amount
            ):
                best = route
        return best

    def _candidate_paths(
        self,
        source_asset: str,
        destination_asset: str,
        amount: float,
        *,
        enforce_order_rules: bool = True,
    ) -> list[tuple[float, tuple[tuple[MarketSnapshot, str, str], ...]]]:
        source = source_asset.strip().upper()
        destination = destination_asset.strip().upper()
        if source == destination:
            return [(amount, ())]
        if amount <= 0 or not math.isfinite(amount):
            return []
        sequence = count()
        queue: list[
            tuple[
                float,
                int,
                int,
                str,
                float,
                tuple[str, ...],
                tuple[tuple[MarketSnapshot, str, str], ...],
            ]
        ] = [(-amount, 0, next(sequence), source, amount, (source,), ())]
        completed: list[tuple[float, tuple[tuple[MarketSnapshot, str, str], ...]]] = []
        expansions = 0
        while queue and expansions < self._settings.max_route_expansions:
            _, depth, _, asset, current_amount, visited, path = heapq.heappop(queue)
            if asset == destination:
                completed.append((current_amount, path))
                continue
            if depth >= self._settings.max_route_legs:
                continue
            for market, side, next_asset in self._adjacency.get(asset, []):
                if next_asset in visited:
                    continue
                leg = _gross_leg(
                    market,
                    side,
                    current_amount,
                    slippage_bps=self._settings.slippage_buffer_bps,
                    enforce_order_rules=enforce_order_rules,
                )
                if leg is None:
                    continue
                output, _ = leg
                next_path = (*path, (market, side, next_asset))
                heapq.heappush(
                    queue,
                    (
                        -output,
                        depth + 1,
                        next(sequence),
                        next_asset,
                        output,
                        (*visited, next_asset),
                        next_path,
                    ),
                )
                expansions += 1
        completed.sort(key=lambda item: item[0], reverse=True)
        return completed

    async def _cost_path(
        self,
        source: str,
        destination: str,
        amount: float,
        path: tuple[tuple[MarketSnapshot, str, str], ...],
        *,
        apply_bnb_discount: bool,
    ) -> RouteQuote | None:
        if not path and source.upper() == destination.upper():
            return RouteQuote(
                source_asset=source.upper(),
                destination_asset=destination.upper(),
                input_amount=amount,
                output_amount=amount,
                legs=(),
                slippage_bps=self._settings.slippage_buffer_bps,
            )
        current_asset = source.upper()
        current_amount = amount
        legs: list[RouteLeg] = []
        for market, side, next_asset in path:
            simulated = _gross_leg(
                market,
                side,
                current_amount,
                slippage_bps=self._settings.slippage_buffer_bps,
                enforce_order_rules=True,
            )
            if simulated is None:
                return None
            gross_output, spent = simulated
            commission = await self._fees.get(market.symbol)
            fee_rate = commission.taker_rate(side, apply_discount=apply_bnb_discount)
            fee_amount = gross_output * fee_rate
            net_output = gross_output - fee_amount
            if net_output <= 0:
                return None
            legs.append(
                RouteLeg(
                    symbol=market.symbol,
                    side=side,
                    source_asset=current_asset,
                    destination_asset=next_asset,
                    input_amount=current_amount,
                    spent_amount=spent,
                    gross_output_amount=gross_output,
                    fee_rate=fee_rate,
                    fee_output_amount=fee_amount,
                    net_output_amount=net_output,
                    commission_discount_asset=(
                        commission.discount_asset
                        if commission.discount_enabled and apply_bnb_discount
                        else ""
                    ),
                )
            )
            current_asset = next_asset
            current_amount = net_output
        return RouteQuote(
            source_asset=source.upper(),
            destination_asset=destination.upper(),
            input_amount=amount,
            output_amount=current_amount,
            legs=tuple(legs),
            slippage_bps=self._settings.slippage_buffer_bps,
        )


def _gross_leg(
    market: MarketSnapshot,
    side: str,
    input_amount: float,
    *,
    slippage_bps: float,
    enforce_order_rules: bool,
) -> tuple[float, float] | None:
    slip = slippage_bps / 10_000
    amount_step = market.amount_step if market.amount_step is not None else market.amount_precision
    if side == "sell":
        base_amount = (
            _round_down(input_amount, amount_step)
            if enforce_order_rules
            else input_amount
        )
        price = market.bid * (1 - slip)
        notional = base_amount * price
        if enforce_order_rules and (
            base_amount < market.min_amount or notional < market.min_notional
        ):
            return None
        return notional, base_amount
    price = market.ask * (1 + slip)
    base_amount = (
        _round_down(input_amount / price, amount_step)
        if enforce_order_rules
        else input_amount / price
    )
    spent = base_amount * price
    if enforce_order_rules and (
        base_amount < market.min_amount or spent < market.min_notional
    ):
        return None
    return base_amount, spent


def _round_down(amount: float, precision: int | float | None) -> float:
    if precision is None:
        return amount
    if isinstance(precision, int):
        quantum = Decimal("1").scaleb(-precision)
    else:
        quantum = Decimal(str(precision))
    if quantum <= 0:
        return amount
    decimal_amount = Decimal(str(amount))
    return float((decimal_amount / quantum).to_integral_value(rounding=ROUND_DOWN) * quantum)

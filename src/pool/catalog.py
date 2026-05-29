"""Dynamic Binance spot-market catalog and conservative eligibility gates."""

from __future__ import annotations

import heapq
import math
import time
from collections import defaultdict
from collections.abc import Mapping
from typing import Any

from src.pool.config import PoolSettings
from src.pool.models import MarketSnapshot


class MarketCatalog:
    """Fetch and classify the complete Binance spot-market universe."""

    def __init__(self, client: Any, settings: PoolSettings) -> None:
        self._client = client
        self._settings = settings
        self.markets: dict[str, MarketSnapshot] = {}

    async def refresh(self) -> dict[str, MarketSnapshot]:
        """Refresh all markets and current public tickers."""
        raw_markets = await self._client.request("load_markets", request_weight=10)
        raw_tickers = await self._client.request("fetch_tickers", request_weight=4)
        candidates: list[tuple[Mapping[str, Any], Mapping[str, Any], tuple[str, ...]]] = []
        for raw_market in raw_markets.values():
            if not isinstance(raw_market, Mapping) or not raw_market.get("spot"):
                continue
            symbol = str(raw_market.get("symbol", "")).strip()
            ticker = raw_tickers.get(symbol, {}) if isinstance(raw_tickers, Mapping) else {}
            reasons = tuple(self._basic_rejections(raw_market, ticker))
            candidates.append((raw_market, ticker, reasons))

        provisional = [
            self._snapshot(market, ticker, reasons, quote_volume_usdt=None)
            for market, ticker, reasons in candidates
            if not reasons
        ]
        volume_graph = _price_edges(provisional)
        quote_rates: dict[str, float | None] = {"USDT": 1.0}
        snapshots: dict[str, MarketSnapshot] = {}
        for market, ticker, basic_reasons in candidates:
            quote = str(market.get("quote", "")).strip().upper()
            if quote not in quote_rates:
                quote_rates[quote] = _best_rate(
                    volume_graph, quote, "USDT", max_legs=3
                )
            quote_rate = quote_rates[quote]
            quote_volume = _non_negative(ticker.get("quoteVolume"))
            volume_usdt = (
                quote_volume * quote_rate
                if quote_volume is not None and quote_rate is not None
                else None
            )
            reasons = list(basic_reasons)
            if not reasons:
                if volume_usdt is None:
                    reasons.append("quote volume cannot be valued in USDT")
                elif volume_usdt < self._settings.min_quote_volume_usdt:
                    reasons.append("quote volume below configured USDT threshold")
            snapshot = self._snapshot(
                market,
                ticker,
                tuple(reasons),
                quote_volume_usdt=volume_usdt,
            )
            snapshots[snapshot.symbol] = snapshot
        self.markets = snapshots
        return dict(snapshots)

    def eligible(self) -> dict[str, MarketSnapshot]:
        """Return currently eligible market edges."""
        return {
            symbol: market
            for symbol, market in self.markets.items()
            if market.eligible
        }

    def scoring_assets(self) -> tuple[str, ...]:
        """Return liquid base assets bounded for paper scoring workload."""
        sorted_markets = sorted(
            self.eligible().values(),
            key=lambda market: market.quote_volume_usdt or 0.0,
            reverse=True,
        )
        assets: list[str] = []
        for market in sorted_markets:
            if market.base not in assets:
                assets.append(market.base)
            if (
                self._settings.max_scoring_assets > 0
                and len(assets) >= self._settings.max_scoring_assets
            ):
                break
        return tuple(assets)

    def _basic_rejections(
        self,
        market: Mapping[str, Any],
        ticker: Mapping[str, Any],
    ) -> list[str]:
        reasons: list[str] = []
        if market.get("active") is not True:
            reasons.append("market is not active")
        bid = _positive(ticker.get("bid"))
        ask = _positive(ticker.get("ask"))
        last = _positive(ticker.get("last"))
        if bid is None or ask is None or last is None or ask < bid:
            reasons.append("ticker bid/ask/last is incomplete")
        timestamp = _integer(ticker.get("timestamp"))
        if timestamp is None:
            reasons.append("ticker timestamp is missing")
        elif (
            self._settings.ticker_stale_seconds > 0
            and max(time.time() * 1000 - timestamp, 0.0)
            > self._settings.ticker_stale_seconds * 1000
        ):
            reasons.append("ticker is stale")
        limits = market.get("limits") if isinstance(market.get("limits"), Mapping) else {}
        amount_limits = limits.get("amount") if isinstance(limits.get("amount"), Mapping) else {}
        cost_limits = limits.get("cost") if isinstance(limits.get("cost"), Mapping) else {}
        if _positive(amount_limits.get("min")) is None:
            reasons.append("minimum amount rule is missing")
        if _positive(cost_limits.get("min")) is None:
            reasons.append("minimum notional rule is missing")
        if bid is not None and ask is not None:
            spread_bps = (ask - bid) / ((ask + bid) / 2) * 10_000
            if spread_bps > self._settings.max_spread_bps:
                reasons.append("spread exceeds configured threshold")
        return reasons

    @staticmethod
    def _snapshot(
        market: Mapping[str, Any],
        ticker: Mapping[str, Any],
        reasons: tuple[str, ...],
        *,
        quote_volume_usdt: float | None,
    ) -> MarketSnapshot:
        limits = market.get("limits") if isinstance(market.get("limits"), Mapping) else {}
        amount_limits = limits.get("amount") if isinstance(limits.get("amount"), Mapping) else {}
        cost_limits = limits.get("cost") if isinstance(limits.get("cost"), Mapping) else {}
        precision = market.get("precision") if isinstance(market.get("precision"), Mapping) else {}
        bid = _positive(ticker.get("bid")) or 0.0
        ask = _positive(ticker.get("ask")) or 0.0
        spread_bps = (
            (ask - bid) / ((ask + bid) / 2) * 10_000
            if bid > 0 and ask > 0
            else math.inf
        )
        return MarketSnapshot(
            symbol=str(market.get("symbol", "")).strip(),
            base=str(market.get("base", "")).strip().upper(),
            quote=str(market.get("quote", "")).strip().upper(),
            bid=bid,
            ask=ask,
            last=_positive(ticker.get("last")) or 0.0,
            quote_volume=_non_negative(ticker.get("quoteVolume")) or 0.0,
            quote_volume_usdt=quote_volume_usdt,
            min_amount=_positive(amount_limits.get("min")) or 0.0,
            min_notional=_positive(cost_limits.get("min")) or 0.0,
            amount_precision=_precision(precision.get("amount")),
            timestamp_ms=_integer(ticker.get("timestamp")),
            spread_bps=spread_bps,
            eligible=not reasons,
            amount_step=_amount_step(market),
            rejection_reasons=reasons,
        )


def _price_edges(markets: list[MarketSnapshot]) -> dict[str, list[tuple[str, float]]]:
    edges: dict[str, list[tuple[str, float]]] = defaultdict(list)
    for market in markets:
        if market.bid <= 0 or market.ask <= 0:
            continue
        edges[market.base].append((market.quote, market.bid))
        edges[market.quote].append((market.base, 1 / market.ask))
    return edges


def _best_rate(
    edges: dict[str, list[tuple[str, float]]],
    source: str,
    destination: str,
    *,
    max_legs: int,
) -> float | None:
    if source == destination:
        return 1.0
    queue: list[tuple[float, int, str, tuple[str, ...]]] = [(-1.0, 0, source, (source,))]
    best: float | None = None
    expansions = 0
    while queue and expansions < 20_000:
        negative_amount, depth, asset, visited = heapq.heappop(queue)
        amount = -negative_amount
        if asset == destination:
            best = amount if best is None else max(best, amount)
            continue
        if depth >= max_legs:
            continue
        for next_asset, rate in edges.get(asset, []):
            if next_asset in visited:
                continue
            heapq.heappush(
                queue,
                (-amount * rate, depth + 1, next_asset, (*visited, next_asset)),
            )
            expansions += 1
    return best


def _positive(value: Any) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) and number > 0 else None


def _non_negative(value: Any) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) and number >= 0 else None


def _integer(value: Any) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _precision(value: Any) -> int | float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(number) or number < 0:
        return None
    if number.is_integer():
        return int(number)
    return number


def _amount_step(market: Mapping[str, Any]) -> float | None:
    info = market.get("info") if isinstance(market.get("info"), Mapping) else {}
    raw_filters = info.get("filters") if isinstance(info.get("filters"), list) else []
    steps: list[float] = []
    for raw_filter in raw_filters:
        if not isinstance(raw_filter, Mapping):
            continue
        if raw_filter.get("filterType") not in {"LOT_SIZE", "MARKET_LOT_SIZE"}:
            continue
        step = _positive(raw_filter.get("stepSize"))
        if step is not None:
            steps.append(step)
    if not steps:
        return None
    return max(steps)

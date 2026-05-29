"""Read-only Binance commission retrieval and dynamic BNB reserve estimates."""

from __future__ import annotations

import math
from datetime import datetime, timedelta, timezone
from typing import Any

from src.pool.models import CommissionSnapshot, RouteQuote


class CommissionService:
    """Cache authenticated per-symbol commission schedules for paper costing."""

    def __init__(self, client: Any, *, cache_seconds: float = 60.0) -> None:
        self._client = client
        self._cache_seconds = cache_seconds
        self._cache: dict[str, CommissionSnapshot] = {}

    async def get(self, symbol: str) -> CommissionSnapshot:
        """Read commission information when the cached value has expired."""
        normalized = symbol.strip().upper()
        current = self._cache.get(normalized)
        now = datetime.now(timezone.utc)
        if (
            current is not None
            and now - current.retrieved_at <= timedelta(seconds=self._cache_seconds)
        ):
            return current
        payload = await self._client.get_commission_rates(symbol)
        snapshot = _parse_commission(payload, fallback_symbol=normalized, retrieved_at=now)
        self._cache[normalized] = snapshot
        return snapshot


def estimate_bnb_reserve(
    routes: list[RouteQuote],
    *,
    destination_to_bnb: dict[str, float],
    multiplier: float,
    floor_bnb: float,
) -> float:
    """Return a fee reserve derived from all discounted simulated legs."""
    required_bnb = 0.0
    for route in routes:
        for leg in route.legs:
            if leg.commission_discount_asset != "BNB":
                continue
            rate = destination_to_bnb.get(leg.destination_asset)
            if rate is None or rate <= 0:
                continue
            required_bnb += leg.fee_output_amount * rate
    return max(floor_bnb, required_bnb * multiplier)


def _parse_commission(
    payload: dict[str, Any],
    *,
    fallback_symbol: str,
    retrieved_at: datetime,
) -> CommissionSnapshot:
    standard = payload.get("standardCommission") or {}
    tax = payload.get("taxCommission") or {}
    special = payload.get("specialCommission") or {}
    discount = payload.get("discount") or {}
    enabled = bool(discount.get("enabledForAccount")) and bool(
        discount.get("enabledForSymbol")
    )
    multiplier = _rate(discount.get("discount"), default=1.0) if enabled else 1.0
    return CommissionSnapshot(
        symbol=str(payload.get("symbol") or fallback_symbol),
        maker_standard=_rate(standard.get("maker")),
        taker_standard=_rate(standard.get("taker")),
        buyer_standard=_rate(standard.get("buyer")),
        seller_standard=_rate(standard.get("seller")),
        maker_tax=_rate(tax.get("maker")),
        taker_tax=_rate(tax.get("taker")),
        buyer_tax=_rate(tax.get("buyer")),
        seller_tax=_rate(tax.get("seller")),
        maker_special=_rate(special.get("maker")),
        taker_special=_rate(special.get("taker")),
        buyer_special=_rate(special.get("buyer")),
        seller_special=_rate(special.get("seller")),
        discount_enabled=enabled,
        discount_asset=str(discount.get("discountAsset") or "").upper(),
        discount_multiplier=multiplier,
        retrieved_at=retrieved_at,
    )


def _rate(value: Any, *, default: float = 0.0) -> float:
    try:
        result = float(value)
    except (TypeError, ValueError):
        return default
    if not math.isfinite(result) or result < 0:
        return default
    return result

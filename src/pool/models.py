"""Domain models for market discovery, routing, and paper portfolios."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone


def utcnow() -> datetime:
    """Return a timezone-aware UTC timestamp."""
    return datetime.now(timezone.utc)


@dataclass(frozen=True, slots=True)
class MarketSnapshot:
    """One current Binance spot-market edge and its eligibility decision."""

    symbol: str
    base: str
    quote: str
    bid: float
    ask: float
    last: float
    quote_volume: float
    quote_volume_usdt: float | None
    min_amount: float
    min_notional: float
    amount_precision: int | float | None
    timestamp_ms: int | None
    spread_bps: float
    eligible: bool
    amount_step: float | None = None
    rejection_reasons: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class CommissionSnapshot:
    """Authenticated commission components for one Binance symbol."""

    symbol: str
    maker_standard: float
    taker_standard: float
    buyer_standard: float
    seller_standard: float
    maker_tax: float = 0.0
    taker_tax: float = 0.0
    buyer_tax: float = 0.0
    seller_tax: float = 0.0
    maker_special: float = 0.0
    taker_special: float = 0.0
    buyer_special: float = 0.0
    seller_special: float = 0.0
    discount_enabled: bool = False
    discount_asset: str = ""
    discount_multiplier: float = 1.0
    retrieved_at: datetime = field(default_factory=utcnow)

    def taker_rate(self, side: str, *, apply_discount: bool = True) -> float:
        """Return a conservative market-order commission fraction."""
        is_buy = side.lower() == "buy"
        standard = self.taker_standard + (
            self.buyer_standard if is_buy else self.seller_standard
        )
        tax = self.taker_tax + (self.buyer_tax if is_buy else self.seller_tax)
        special = self.taker_special + (
            self.buyer_special if is_buy else self.seller_special
        )
        if apply_discount and self.discount_enabled:
            standard *= self.discount_multiplier
        return standard + tax + special


@dataclass(frozen=True, slots=True)
class RouteLeg:
    """One simulated market conversion in an asset-to-asset route."""

    symbol: str
    side: str
    source_asset: str
    destination_asset: str
    input_amount: float
    spent_amount: float
    gross_output_amount: float
    fee_rate: float
    fee_output_amount: float
    net_output_amount: float
    commission_discount_asset: str = ""


@dataclass(frozen=True, slots=True)
class RouteQuote:
    """Costed candidate conversion path."""

    source_asset: str
    destination_asset: str
    input_amount: float
    output_amount: float
    legs: tuple[RouteLeg, ...]
    slippage_bps: float
    generated_at: datetime = field(default_factory=utcnow)

    @property
    def symbols(self) -> tuple[str, ...]:
        return tuple(leg.symbol for leg in self.legs)


@dataclass(frozen=True, slots=True)
class AssetValuation:
    """Value of a holding measured in one configured benchmark asset."""

    asset: str
    amount: float
    benchmark: str
    benchmark_amount: float | None
    route: RouteQuote | None


@dataclass(slots=True)
class VirtualPortfolio:
    """Paper holdings owned by one allocation/scoring experiment."""

    portfolio_id: str
    scorer_name: str
    balances: dict[str, float]
    seeded_at: datetime = field(default_factory=utcnow)


@dataclass(frozen=True, slots=True)
class PaperDecision:
    """A scorer's proposed or rejected paper reallocation."""

    portfolio_id: str
    scorer_name: str
    source_asset: str | None
    destination_asset: str | None
    input_amount: float | None
    route: RouteQuote | None
    action: str
    reason: str
    score: float | None = None
    decided_at: datetime = field(default_factory=utcnow)

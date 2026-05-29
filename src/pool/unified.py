"""Unified dry-run asset-pool valuation across Binance account types.

This module is a valuation and constraint layer only. It does not submit spot
orders, margin actions, futures actions, or internal transfers.
"""

from __future__ import annotations

import argparse
import json
import math
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

from src.futures import FuturesDiscovery
from src.margin import MarginDiscovery
from src.transfers import (
    TRANSFER_ROUTES,
    TransferDiscovery,
    WALLET_CROSS_MARGIN,
    WALLET_FUNDING,
    WALLET_ISOLATED_MARGIN,
    WALLET_SPOT,
    WALLET_USD_M_FUTURES,
)

ACCOUNT_SPOT = "spot"
ACCOUNT_FUNDING = "funding"
ACCOUNT_MARGIN = "margin"
ACCOUNT_FUTURES = "futures"


class UnifiedPoolError(RuntimeError):
    """Raised when unified pool valuation cannot proceed."""


@dataclass(frozen=True, slots=True)
class UnifiedHolding:
    """One asset exposure inside one account type."""

    account_type: str
    wallet: str
    asset: str
    total: float
    free: float = 0.0
    locked: float = 0.0
    borrowed: float = 0.0
    interest: float = 0.0
    unrealized_pnl: float = 0.0
    available: float = 0.0
    initial_margin: float = 0.0
    maintenance_margin: float = 0.0
    leverage: float | None = None
    margin_type: str = ""
    symbol: str = ""
    transfer_enabled: bool | None = None
    liquidity_score: float | None = None
    volatility_percent: float | None = None
    source: str = ""

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class HoldingValuation:
    """USDT valuation for one unified holding."""

    account_type: str
    wallet: str
    asset: str
    symbol: str
    total: float
    free: float
    locked: float
    available: float
    price_usdt: float
    gross_usdt: float
    debt_usdt: float
    interest_usdt: float
    open_pnl_usdt: float
    initial_margin_usdt: float
    maintenance_margin_usdt: float
    net_usdt: float
    transferable_usdt: float
    leverage: float | None
    margin_type: str
    transfer_routes: tuple[str, ...]
    liquidity_score: float | None
    volatility_percent: float | None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class UnifiedPoolLimits:
    """Allocation and risk limits for unified dry-run valuation."""

    max_asset_allocation_percent: float = 60.0
    max_account_type_allocation_percent: float = 80.0
    max_leverage: float = 3.0
    max_debt_percent: float = 25.0
    min_transferable_usdt: float = 0.0
    min_liquidity_score: float = 0.0
    max_volatility_percent: float = 100.0
    asset_reserves: Mapping[str, float] = field(
        default_factory=lambda: {"BNB": 0.00005}
    )

    def as_dict(self) -> dict[str, Any]:
        return {
            **asdict(self),
            "asset_reserves": dict(self.asset_reserves),
        }


@dataclass(frozen=True, slots=True)
class AllocationConstraint:
    """One unified-pool allocation constraint result."""

    name: str
    status: str
    detail: str
    account_type: str = ""
    wallet: str = ""
    asset: str = ""
    value: float | None = None
    limit: float | None = None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class UnifiedPoolSnapshot:
    """Unified pool valuation and dry-run constraint report."""

    timestamp: str
    holdings: tuple[HoldingValuation, ...]
    total_gross_usdt: float
    total_net_usdt: float
    total_debt_usdt: float
    total_interest_usdt: float
    total_open_pnl_usdt: float
    total_initial_margin_usdt: float
    total_maintenance_margin_usdt: float
    total_transferable_usdt: float
    asset_values_usdt: dict[str, float]
    account_type_values_usdt: dict[str, float]
    wallet_values_usdt: dict[str, float]
    benchmark_values: dict[str, float]
    basket_components: dict[str, float]
    transfer_availability: dict[str, dict[str, Any]]
    constraints: tuple[AllocationConstraint, ...]
    allowed: bool
    live_trading_enabled: bool = False
    live_transfer_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


def build_unified_pool_snapshot(
    holdings: Sequence[UnifiedHolding],
    prices_usdt: Mapping[str, float],
    *,
    benchmarks: Sequence[str] = ("USDT", "BTC", "PAXG", "XAUT"),
    basket_weights: Mapping[str, float] | None = None,
    limits: UnifiedPoolLimits | None = None,
) -> UnifiedPoolSnapshot:
    """Value all supplied account-type holdings and run dry constraints."""
    limits = limits or UnifiedPoolLimits()
    _validate_limits(limits)
    normalized_prices = {
        _asset(asset): _positive_number(price, f"price for {asset}")
        for asset, price in prices_usdt.items()
    }
    normalized_prices["USDT"] = 1.0
    benchmark_assets = tuple(_asset(benchmark) for benchmark in benchmarks)
    weights = _basket_weights(benchmark_assets, basket_weights)
    valuations = tuple(
        _value_holding(holding, normalized_prices)
        for holding in holdings
        if holding.total != 0
        or holding.free != 0
        or holding.available != 0
        or holding.borrowed != 0
        or holding.interest != 0
        or holding.unrealized_pnl != 0
    )
    total_gross = sum(item.gross_usdt for item in valuations)
    total_debt = sum(item.debt_usdt for item in valuations)
    total_interest = sum(item.interest_usdt for item in valuations)
    total_pnl = sum(item.open_pnl_usdt for item in valuations)
    total_initial_margin = sum(item.initial_margin_usdt for item in valuations)
    total_maintenance_margin = sum(item.maintenance_margin_usdt for item in valuations)
    total_transferable = sum(item.transferable_usdt for item in valuations)
    total_net = sum(item.net_usdt for item in valuations)
    asset_values = _sum_by(valuations, "asset")
    account_values = _sum_by(valuations, "account_type")
    wallet_values = _sum_by(valuations, "wallet")
    benchmark_values = _benchmark_values(
        total_net,
        benchmark_assets,
        normalized_prices,
    )
    basket_components = {
        benchmark: benchmark_values[benchmark] * weights[benchmark]
        for benchmark in benchmark_assets
    }
    transfer_availability = _transfer_availability(valuations)
    constraints = _constraints(
        valuations,
        limits,
        total_net=total_net,
        total_debt=total_debt + total_interest,
        asset_values=asset_values,
        account_values=account_values,
    )
    allowed = all(constraint.status == "ok" for constraint in constraints)
    return UnifiedPoolSnapshot(
        timestamp=_utc_timestamp(),
        holdings=valuations,
        total_gross_usdt=total_gross,
        total_net_usdt=total_net,
        total_debt_usdt=total_debt,
        total_interest_usdt=total_interest,
        total_open_pnl_usdt=total_pnl,
        total_initial_margin_usdt=total_initial_margin,
        total_maintenance_margin_usdt=total_maintenance_margin,
        total_transferable_usdt=total_transferable,
        asset_values_usdt=asset_values,
        account_type_values_usdt=account_values,
        wallet_values_usdt=wallet_values,
        benchmark_values=benchmark_values,
        basket_components=basket_components,
        transfer_availability=transfer_availability,
        constraints=constraints,
        allowed=allowed,
    )


def holdings_from_transfer_discovery(
    discovery: TransferDiscovery,
) -> tuple[UnifiedHolding, ...]:
    """Convert transfer-wallet discovery balances to unified holdings."""
    holdings: list[UnifiedHolding] = []
    for balance in discovery.balances:
        account_type = _account_type_from_wallet(balance.wallet)
        total = (
            balance.wallet_balance
            if balance.wallet_balance > 0 and balance.wallet == WALLET_USD_M_FUTURES
            else balance.free + balance.locked
        )
        holdings.append(
            UnifiedHolding(
                account_type=account_type,
                wallet=balance.wallet,
                asset=balance.asset,
                total=total,
                free=balance.free,
                locked=balance.locked,
                borrowed=balance.borrowed,
                interest=balance.interest,
                available=balance.available,
                transfer_enabled=True,
                source="transfer_discovery",
            )
        )
    return tuple(holdings)


def holdings_from_margin_discovery(
    discovery: MarginDiscovery,
) -> tuple[UnifiedHolding, ...]:
    """Convert margin discovery snapshots to unified holdings."""
    holdings: list[UnifiedHolding] = []
    for balance in discovery.cross.assets:
        holdings.append(
            UnifiedHolding(
                account_type=ACCOUNT_MARGIN,
                wallet=WALLET_CROSS_MARGIN,
                asset=balance.asset,
                total=balance.free + balance.locked,
                free=balance.free,
                locked=balance.locked,
                borrowed=balance.borrowed,
                interest=balance.interest,
                available=balance.free,
                transfer_enabled=discovery.cross.transfer_out_enabled,
                source="margin_cross",
            )
        )
    for isolated in discovery.isolated:
        for balance in (isolated.base_asset, isolated.quote_asset):
            holdings.append(
                UnifiedHolding(
                    account_type=ACCOUNT_MARGIN,
                    wallet=WALLET_ISOLATED_MARGIN,
                    asset=balance.asset,
                    total=balance.total_asset or balance.free + balance.locked,
                    free=balance.free,
                    locked=balance.locked,
                    borrowed=balance.borrowed,
                    interest=balance.interest,
                    available=balance.free,
                    symbol=isolated.symbol,
                    transfer_enabled=isolated.enabled,
                    source="margin_isolated",
                )
            )
    return tuple(holdings)


def holdings_from_futures_discovery(
    discovery: FuturesDiscovery,
) -> tuple[UnifiedHolding, ...]:
    """Convert USD-M futures discovery data to unified holdings."""
    holdings: list[UnifiedHolding] = []
    for asset in discovery.account.assets:
        holdings.append(
            UnifiedHolding(
                account_type=ACCOUNT_FUTURES,
                wallet=WALLET_USD_M_FUTURES,
                asset=asset.asset,
                total=asset.wallet_balance,
                free=asset.available_balance,
                locked=asset.initial_margin + asset.maint_margin,
                unrealized_pnl=asset.unrealized_profit,
                available=asset.available_balance,
                initial_margin=asset.initial_margin,
                maintenance_margin=asset.maint_margin,
                transfer_enabled=True,
                source="futures_account",
            )
        )
    for position in discovery.positions:
        if abs(position.notional) <= 0 and abs(position.unrealized_profit) <= 0:
            continue
        holdings.append(
            UnifiedHolding(
                account_type=ACCOUNT_FUTURES,
                wallet=WALLET_USD_M_FUTURES,
                asset=position.margin_asset,
                total=0.0,
                unrealized_pnl=position.unrealized_profit,
                initial_margin=position.initial_margin,
                maintenance_margin=position.maint_margin,
                leverage=position.leverage,
                margin_type=position.margin_type,
                symbol=position.symbol,
                transfer_enabled=False,
                source="futures_position",
            )
        )
    return tuple(holdings)


def render_unified_snapshot(snapshot: UnifiedPoolSnapshot) -> str:
    """Render a concise human-readable unified pool report."""
    lines = [
        "Unified asset pool: dry-run",
        f"Total net USDT: {snapshot.total_net_usdt:.8f}",
        f"Debt USDT: {snapshot.total_debt_usdt:.8f}",
        f"Interest USDT: {snapshot.total_interest_usdt:.8f}",
        f"Open PnL USDT: {snapshot.total_open_pnl_usdt:.8f}",
        f"Transferable USDT: {snapshot.total_transferable_usdt:.8f}",
        f"Allowed: {_yes_no(snapshot.allowed)}",
        "Benchmarks:",
    ]
    for benchmark, value in snapshot.benchmark_values.items():
        lines.append(f"  - {benchmark}: {value:.8f}")
    failed = [item for item in snapshot.constraints if item.status != "ok"]
    if failed:
        lines.append("Constraints:")
        for item in failed:
            lines.append(f"  - {item.name}: {item.detail}")
    return "\n".join(lines)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Value a unified dry-run asset pool from local JSON inputs."
    )
    parser.add_argument("--dry-run", action="store_true", help="required")
    parser.add_argument("--holdings-json", required=False)
    parser.add_argument("--prices-json", required=False)
    parser.add_argument("--benchmarks", default="USDT,BTC,PAXG,XAUT")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    if not args.dry_run:
        print("unified pool check failed: --dry-run is required")
        return 1
    try:
        holdings = _load_holdings(Path(args.holdings_json)) if args.holdings_json else ()
        prices = _load_prices(Path(args.prices_json)) if args.prices_json else {"USDT": 1.0}
        snapshot = build_unified_pool_snapshot(
            holdings,
            prices,
            benchmarks=tuple(
                benchmark.strip().upper()
                for benchmark in args.benchmarks.split(",")
                if benchmark.strip()
            ),
        )
        if args.json:
            print(json.dumps(snapshot.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_unified_snapshot(snapshot))
    except Exception as exc:
        print(f"unified pool check failed: {exc}")
        return 1
    return 0


def _value_holding(
    holding: UnifiedHolding,
    prices_usdt: Mapping[str, float],
) -> HoldingValuation:
    asset = _asset(holding.asset)
    price = _price(asset, prices_usdt)
    total = _finite(holding.total, "holding total")
    free = _finite(holding.free, "holding free")
    locked = _finite(holding.locked, "holding locked")
    available = _finite(holding.available, "holding available")
    borrowed = _finite(holding.borrowed, "holding borrowed")
    interest = _finite(holding.interest, "holding interest")
    pnl = _finite(holding.unrealized_pnl, "holding unrealized_pnl") * price
    initial_margin = _finite(holding.initial_margin, "holding initial_margin") * price
    maintenance_margin = (
        _finite(holding.maintenance_margin, "holding maintenance_margin") * price
    )
    gross = total * price
    debt = borrowed * price
    interest_usdt = interest * price
    net = gross - debt - interest_usdt + pnl
    transferable_amount = available if available > 0 else free
    if holding.transfer_enabled is False:
        transferable_amount = 0.0
    routes = _outbound_transfer_routes(holding.wallet) if transferable_amount > 0 else ()
    return HoldingValuation(
        account_type=_account_type(holding.account_type),
        wallet=holding.wallet,
        asset=asset,
        symbol=holding.symbol,
        total=total,
        free=free,
        locked=locked,
        available=available,
        price_usdt=price,
        gross_usdt=gross,
        debt_usdt=debt,
        interest_usdt=interest_usdt,
        open_pnl_usdt=pnl,
        initial_margin_usdt=initial_margin,
        maintenance_margin_usdt=maintenance_margin,
        net_usdt=net,
        transferable_usdt=max(transferable_amount * price, 0.0),
        leverage=holding.leverage,
        margin_type=holding.margin_type,
        transfer_routes=routes,
        liquidity_score=holding.liquidity_score,
        volatility_percent=holding.volatility_percent,
    )


def _constraints(
    holdings: tuple[HoldingValuation, ...],
    limits: UnifiedPoolLimits,
    *,
    total_net: float,
    total_debt: float,
    asset_values: Mapping[str, float],
    account_values: Mapping[str, float],
) -> tuple[AllocationConstraint, ...]:
    constraints: list[AllocationConstraint] = []
    denominator = total_net if total_net > 0 else 1.0
    for asset, value in asset_values.items():
        percent = (value / denominator) * 100.0
        constraints.append(
            AllocationConstraint(
                name="asset_allocation",
                status="ok"
                if percent <= limits.max_asset_allocation_percent
                else "blocked",
                detail=(
                    f"{asset} allocation {percent:.4f}% "
                    f"vs limit {limits.max_asset_allocation_percent:.4f}%"
                ),
                asset=asset,
                value=percent,
                limit=limits.max_asset_allocation_percent,
            )
        )
    for account_type, value in account_values.items():
        percent = (value / denominator) * 100.0
        constraints.append(
            AllocationConstraint(
                name="account_type_allocation",
                status="ok"
                if percent <= limits.max_account_type_allocation_percent
                else "blocked",
                detail=(
                    f"{account_type} allocation {percent:.4f}% "
                    f"vs limit {limits.max_account_type_allocation_percent:.4f}%"
                ),
                account_type=account_type,
                value=percent,
                limit=limits.max_account_type_allocation_percent,
            )
        )
    debt_percent = (total_debt / denominator) * 100.0
    constraints.append(
        AllocationConstraint(
            name="debt_ratio",
            status="ok" if debt_percent <= limits.max_debt_percent else "blocked",
            detail=(
                f"debt ratio {debt_percent:.4f}% "
                f"vs limit {limits.max_debt_percent:.4f}%"
            ),
            value=debt_percent,
            limit=limits.max_debt_percent,
        )
    )
    for holding in holdings:
        if holding.leverage is not None:
            constraints.append(
                AllocationConstraint(
                    name="leverage",
                    status="ok"
                    if holding.leverage <= limits.max_leverage
                    else "blocked",
                    detail=(
                        f"{holding.symbol or holding.asset} leverage "
                        f"{holding.leverage:.4f} vs limit {limits.max_leverage:.4f}"
                    ),
                    account_type=holding.account_type,
                    wallet=holding.wallet,
                    asset=holding.asset,
                    value=holding.leverage,
                    limit=limits.max_leverage,
                )
            )
        if holding.transferable_usdt < limits.min_transferable_usdt:
            constraints.append(
                AllocationConstraint(
                    name="transferability",
                    status="blocked",
                    detail=(
                        f"{holding.wallet} {holding.asset} transferable "
                        f"{holding.transferable_usdt:.8f} USDT below minimum "
                        f"{limits.min_transferable_usdt:.8f}"
                    ),
                    account_type=holding.account_type,
                    wallet=holding.wallet,
                    asset=holding.asset,
                    value=holding.transferable_usdt,
                    limit=limits.min_transferable_usdt,
                )
            )
        if (
            holding.liquidity_score is not None
            and holding.liquidity_score < limits.min_liquidity_score
        ):
            constraints.append(
                AllocationConstraint(
                    name="liquidity",
                    status="blocked",
                    detail=(
                        f"{holding.asset} liquidity {holding.liquidity_score:.4f} "
                        f"below minimum {limits.min_liquidity_score:.4f}"
                    ),
                    account_type=holding.account_type,
                    wallet=holding.wallet,
                    asset=holding.asset,
                    value=holding.liquidity_score,
                    limit=limits.min_liquidity_score,
                )
            )
        if (
            holding.volatility_percent is not None
            and holding.volatility_percent > limits.max_volatility_percent
        ):
            constraints.append(
                AllocationConstraint(
                    name="volatility",
                    status="blocked",
                    detail=(
                        f"{holding.asset} volatility {holding.volatility_percent:.4f}% "
                        f"above maximum {limits.max_volatility_percent:.4f}%"
                    ),
                    account_type=holding.account_type,
                    wallet=holding.wallet,
                    asset=holding.asset,
                    value=holding.volatility_percent,
                    limit=limits.max_volatility_percent,
                )
            )
    for asset, reserve_amount in limits.asset_reserves.items():
        normalized_asset = _asset(asset)
        matching = [holding for holding in holdings if holding.asset == normalized_asset]
        free_amount = sum((holding.transferable_usdt / holding.price_usdt) for holding in matching)
        constraints.append(
            AllocationConstraint(
                name="reserve",
                status="ok" if free_amount >= reserve_amount else "blocked",
                detail=(
                    f"{normalized_asset} reserve {free_amount:.8f} "
                    f"vs required {reserve_amount:.8f}"
                ),
                asset=normalized_asset,
                value=free_amount,
                limit=reserve_amount,
            )
        )
    return tuple(constraints)


def _benchmark_values(
    total_usdt: float,
    benchmarks: tuple[str, ...],
    prices_usdt: Mapping[str, float],
) -> dict[str, float]:
    values: dict[str, float] = {}
    for benchmark in benchmarks:
        price = _price(benchmark, prices_usdt)
        values[benchmark] = total_usdt / price if price > 0 else 0.0
    return values


def _sum_by(
    holdings: tuple[HoldingValuation, ...],
    field_name: str,
) -> dict[str, float]:
    totals: dict[str, float] = {}
    for holding in holdings:
        key = str(getattr(holding, field_name))
        totals[key] = totals.get(key, 0.0) + holding.net_usdt
    return totals


def _transfer_availability(
    holdings: tuple[HoldingValuation, ...],
) -> dict[str, dict[str, Any]]:
    availability: dict[str, dict[str, Any]] = {}
    for holding in holdings:
        key = f"{holding.wallet}:{holding.asset}"
        availability[key] = {
            "wallet": holding.wallet,
            "asset": holding.asset,
            "transferable_usdt": holding.transferable_usdt,
            "routes": list(holding.transfer_routes),
        }
    return availability


def _outbound_transfer_routes(wallet: str) -> tuple[str, ...]:
    return tuple(
        sorted(
            transfer_type
            for (source, _destination), transfer_type in TRANSFER_ROUTES.items()
            if source == wallet
        )
    )


def _basket_weights(
    benchmarks: tuple[str, ...],
    basket_weights: Mapping[str, float] | None,
) -> dict[str, float]:
    if not benchmarks:
        raise UnifiedPoolError("benchmarks must not be empty")
    if basket_weights is None:
        equal = 1.0 / len(benchmarks)
        return {benchmark: equal for benchmark in benchmarks}
    weights = {
        _asset(asset): _positive_number(weight, f"basket weight for {asset}")
        for asset, weight in basket_weights.items()
    }
    missing = [benchmark for benchmark in benchmarks if benchmark not in weights]
    if missing:
        raise UnifiedPoolError("basket weights missing benchmarks: " + ", ".join(missing))
    total = sum(weights[benchmark] for benchmark in benchmarks)
    if not math.isclose(total, 1.0, rel_tol=0.0, abs_tol=1e-8):
        raise UnifiedPoolError("basket weights must sum to 1")
    return {benchmark: weights[benchmark] for benchmark in benchmarks}


def _load_holdings(path: Path) -> tuple[UnifiedHolding, ...]:
    raw = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(raw, Sequence) or isinstance(raw, (str, bytes)):
        raise UnifiedPoolError("holdings JSON must be a list")
    return tuple(UnifiedHolding(**item) for item in raw if isinstance(item, Mapping))


def _load_prices(path: Path) -> dict[str, float]:
    raw = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(raw, Mapping):
        raise UnifiedPoolError("prices JSON must be an object")
    return {_asset(asset): _positive_number(value, f"price for {asset}") for asset, value in raw.items()}


def _account_type_from_wallet(wallet: str) -> str:
    if wallet == WALLET_SPOT:
        return ACCOUNT_SPOT
    if wallet == WALLET_FUNDING:
        return ACCOUNT_FUNDING
    if wallet in {WALLET_CROSS_MARGIN, WALLET_ISOLATED_MARGIN}:
        return ACCOUNT_MARGIN
    if wallet == WALLET_USD_M_FUTURES:
        return ACCOUNT_FUTURES
    raise UnifiedPoolError(f"unsupported wallet: {wallet}")


def _account_type(value: str) -> str:
    normalized = value.strip().lower()
    if normalized not in {ACCOUNT_SPOT, ACCOUNT_FUNDING, ACCOUNT_MARGIN, ACCOUNT_FUTURES}:
        raise UnifiedPoolError(f"unsupported account_type: {value}")
    return normalized


def _validate_limits(limits: UnifiedPoolLimits) -> None:
    _positive_number(limits.max_asset_allocation_percent, "max_asset_allocation_percent")
    _positive_number(
        limits.max_account_type_allocation_percent,
        "max_account_type_allocation_percent",
    )
    _positive_number(limits.max_leverage, "max_leverage")
    _positive_number(limits.max_debt_percent, "max_debt_percent")
    _finite(limits.min_transferable_usdt, "min_transferable_usdt")
    _finite(limits.min_liquidity_score, "min_liquidity_score")
    _positive_number(limits.max_volatility_percent, "max_volatility_percent")
    for asset, amount in limits.asset_reserves.items():
        _asset(asset)
        _finite(amount, f"reserve for {asset}")


def _price(asset: str, prices_usdt: Mapping[str, float]) -> float:
    normalized = _asset(asset)
    if normalized == "USDT":
        return 1.0
    price = prices_usdt.get(normalized)
    if price is None:
        raise UnifiedPoolError(f"missing USDT price for {normalized}")
    return _positive_number(price, f"price for {normalized}")


def _asset(value: object) -> str:
    text = str(value or "").strip().upper()
    if not text:
        raise UnifiedPoolError("asset must not be blank")
    return text


def _positive_number(value: object, field_name: str) -> float:
    number = _finite(value, field_name)
    if number <= 0:
        raise UnifiedPoolError(f"{field_name} must be greater than zero")
    return number


def _finite(value: object, field_name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise UnifiedPoolError(f"{field_name} must be a number") from exc
    if not math.isfinite(number):
        raise UnifiedPoolError(f"{field_name} must be finite")
    return number


def _yes_no(value: bool) -> str:
    return "yes" if value else "no"


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


if __name__ == "__main__":
    raise SystemExit(run())

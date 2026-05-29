"""Dry-run margin account discovery and risk modeling.

This module intentionally has no live margin order, borrow, repay, or transfer
submission path. It uses read-only USER_DATA endpoints and local calculations
so future margin strategies have a tested risk model before activation work
starts.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import re
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from typing import Any, Mapping, Sequence

from src.config import Settings
from src.data.feeds import normalize_symbol
from src.exchange.client import BinanceClient

ACCOUNT_CROSS = "cross"
ACCOUNT_ISOLATED = "isolated"
WRITE_ENDPOINTS_BLOCKED = (
    "sapiPostMarginBorrowRepay",
    "sapiPostMarginOrder",
    "sapiPostMarginTransfer",
    "sapiPostMarginIsolatedTransfer",
)


class MarginModelError(RuntimeError):
    """Raised when dry-run margin modeling cannot proceed safely."""


@dataclass(frozen=True, slots=True)
class MarginAssetBalance:
    """One asset balance inside a cross or isolated margin account."""

    asset: str
    free: float = 0.0
    locked: float = 0.0
    borrowed: float = 0.0
    interest: float = 0.0
    net_asset: float = 0.0
    total_asset: float = 0.0
    net_asset_btc: float = 0.0
    borrow_enabled: bool | None = None
    repay_enabled: bool | None = None

    @property
    def liability(self) -> float:
        return self.borrowed + self.interest

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class CrossMarginSnapshot:
    """Read-only cross-margin account snapshot."""

    created: bool | None
    borrow_enabled: bool | None
    trade_enabled: bool | None
    transfer_in_enabled: bool | None
    transfer_out_enabled: bool | None
    account_type: str
    margin_level: float | None
    collateral_margin_level: float | None
    total_asset_btc: float
    total_liability_btc: float
    total_net_asset_btc: float
    total_collateral_usdt: float
    total_open_order_loss_usdt: float
    assets: tuple[MarginAssetBalance, ...] = ()

    def asset_balance(self, asset: str) -> MarginAssetBalance | None:
        normalized = _asset(asset)
        for balance in self.assets:
            if balance.asset == normalized:
                return balance
        return None

    def asset_liability(self, asset: str) -> float:
        balance = self.asset_balance(asset)
        return balance.liability if balance is not None else 0.0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class IsolatedMarginSnapshot:
    """Read-only isolated-margin account snapshot for one symbol."""

    symbol: str
    isolated_created: bool | None
    enabled: bool | None
    trade_enabled: bool | None
    margin_level: float | None
    margin_level_status: str
    margin_ratio: float | None
    index_price: float | None
    liquidate_price: float | None
    liquidate_rate: float | None
    base_asset: MarginAssetBalance
    quote_asset: MarginAssetBalance

    def asset_balance(self, asset: str) -> MarginAssetBalance | None:
        normalized = _asset(asset)
        if self.base_asset.asset == normalized:
            return self.base_asset
        if self.quote_asset.asset == normalized:
            return self.quote_asset
        return None

    def asset_liability(self, asset: str) -> float:
        balance = self.asset_balance(asset)
        return balance.liability if balance is not None else 0.0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class MarginDiscovery:
    """Combined cross and isolated margin discovery result."""

    timestamp: str
    cross: CrossMarginSnapshot
    isolated: tuple[IsolatedMarginSnapshot, ...] = ()
    live_trading_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def isolated_snapshot(self, symbol: str) -> IsolatedMarginSnapshot | None:
        normalized = normalize_symbol(symbol)
        exchange_symbol = _exchange_symbol(normalized)
        for snapshot in self.isolated:
            if snapshot.symbol in {normalized, exchange_symbol}:
                return snapshot
        return None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class MarginRiskLimits:
    """Conservative local risk limits for margin dry-run plans."""

    min_margin_level: float = 2.0
    liquidation_margin_level: float = 1.1
    min_liquidation_buffer_percent: float = 25.0
    max_borrow_utilization_percent: float = 50.0
    max_hourly_interest_usdt: float = 0.05
    max_total_liability_usdt: float = 25.0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class MarginBorrowRequest:
    """Requested borrow modeling inputs."""

    account_type: str
    asset: str
    requested_amount: float
    isolated_symbol: str | None = None
    asset_usdt_price: float | None = None


@dataclass(frozen=True, slots=True)
class MarginRepayRequest:
    """Requested repay modeling inputs."""

    account_type: str
    asset: str
    requested_amount: float
    isolated_symbol: str | None = None


@dataclass(frozen=True, slots=True)
class MarginBorrowPlan:
    """Dry-run borrow sizing and risk result."""

    timestamp: str
    account_type: str
    isolated_symbol: str | None
    asset: str
    requested_amount: float
    max_borrowable: float
    borrow_limit: float
    max_allowed_by_utilization: float
    planned_borrow_amount: float
    utilization_percent: float
    hourly_interest_rate: float
    projected_hourly_interest: float
    projected_hourly_interest_usdt: float | None
    debt_before: float
    projected_debt: float
    margin_level_before: float | None
    margin_level_after_estimate: float | None
    liquidation_reference_level: float
    liquidation_buffer_percent: float | None
    projected_total_liability_usdt: float | None
    allowed: bool
    rejection_reasons: tuple[str, ...] = ()
    warnings: tuple[str, ...] = ()
    risk_limits: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class MarginRepayPlan:
    """Dry-run repay sizing result."""

    timestamp: str
    account_type: str
    isolated_symbol: str | None
    asset: str
    requested_amount: float
    borrowed_amount: float
    interest_amount: float
    liability_amount: float
    planned_repay_amount: float
    remaining_liability: float
    allowed: bool
    rejection_reasons: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class MarginDryRunReport:
    """Full dry-run margin report."""

    timestamp: str
    mode: str
    discovery: dict[str, Any]
    borrow_plan: dict[str, Any] | None = None
    repay_plan: dict[str, Any] | None = None
    live_trading_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def discover_margin_accounts(
    client: Any,
    *,
    include_isolated: bool = True,
    isolated_symbols: Sequence[str] = (),
) -> MarginDiscovery:
    """Discover cross and optional isolated margin accounts with read-only calls."""
    cross_raw = await client.request("sapiGetMarginAccount", request_weight=10)
    cross = parse_cross_margin_snapshot(cross_raw)
    isolated: tuple[IsolatedMarginSnapshot, ...] = ()
    if include_isolated:
        params: dict[str, str] = {}
        if isolated_symbols:
            params["symbols"] = ",".join(
                _exchange_symbol(normalize_symbol(symbol)) for symbol in isolated_symbols
            )
        isolated_raw = await client.request(
            "sapiGetMarginIsolatedAccount",
            params,
            request_weight=10,
        )
        isolated = parse_isolated_margin_snapshots(isolated_raw)

    return MarginDiscovery(
        timestamp=_utc_timestamp(),
        cross=cross,
        isolated=isolated,
    )


async def build_margin_borrow_plan(
    client: Any,
    discovery: MarginDiscovery,
    request: MarginBorrowRequest,
    *,
    limits: MarginRiskLimits | None = None,
) -> MarginBorrowPlan:
    """Build a dry-run borrow plan from read-only Binance data."""
    limits = limits or MarginRiskLimits()
    _validate_limits(limits)
    account_type = _account_type(request.account_type)
    asset = _asset(request.asset)
    requested_amount = _positive_number(
        request.requested_amount,
        "requested borrow amount",
    )
    isolated_symbol = _required_isolated_symbol(account_type, request.isolated_symbol)

    max_borrow = await _query_max_borrowable(
        client,
        account_type=account_type,
        asset=asset,
        isolated_symbol=isolated_symbol,
    )
    hourly_rate = await _query_next_hourly_interest_rate(
        client,
        account_type=account_type,
        asset=asset,
    )
    snapshot = _account_snapshot(discovery, account_type, isolated_symbol)
    asset_usdt_price = _asset_usdt_price(asset, request.asset_usdt_price)
    if asset_usdt_price is None:
        asset_usdt_price = _infer_asset_usdt_price(snapshot, asset)
    account_liability = _asset_liability(snapshot, asset)
    margin_level_before = _snapshot_margin_level(snapshot)
    max_borrowable = _non_negative_float(max_borrow.get("amount"))
    borrow_limit = _non_negative_float(max_borrow.get("borrowLimit"))
    max_allowed_by_utilization = max_borrowable * (
        limits.max_borrow_utilization_percent / 100.0
    )
    planned_borrow_amount = min(requested_amount, max_allowed_by_utilization)
    utilization_percent = (
        (planned_borrow_amount / max_borrowable) * 100.0
        if max_borrowable > 0
        else 0.0
    )
    projected_hourly_interest = planned_borrow_amount * hourly_rate
    projected_debt = account_liability + planned_borrow_amount + projected_hourly_interest
    planned_borrow_usdt = (
        planned_borrow_amount * asset_usdt_price
        if asset_usdt_price is not None
        else None
    )
    projected_interest_usdt = (
        projected_hourly_interest * asset_usdt_price
        if asset_usdt_price is not None
        else None
    )
    projected_total_liability_usdt = _projected_liability_usdt(
        snapshot,
        asset,
        planned_borrow_amount + projected_hourly_interest,
        asset_usdt_price,
    )
    margin_level_after = _estimate_margin_level_after(
        snapshot,
        asset,
        planned_borrow_amount + projected_hourly_interest,
        asset_usdt_price,
    )
    buffer_percent = _liquidation_buffer_percent(
        margin_level_after,
        limits.liquidation_margin_level,
    )

    warnings: list[str] = []
    rejections: list[str] = []
    if requested_amount > max_allowed_by_utilization > 0:
        warnings.append(
            "requested amount was capped by max_borrow_utilization_percent"
        )
    if max_borrowable <= 0:
        rejections.append("max borrowable amount is zero")
    if planned_borrow_amount <= 0:
        rejections.append("planned borrow amount is zero")
    if not _borrow_enabled(snapshot, asset):
        rejections.append("borrowing is not enabled for this margin account or asset")
    if margin_level_before is not None and margin_level_before < limits.min_margin_level:
        rejections.append("current margin level is below the configured minimum")
    if margin_level_after is not None and margin_level_after < limits.min_margin_level:
        rejections.append("estimated margin level after borrow is below minimum")
    if (
        buffer_percent is not None
        and buffer_percent < limits.min_liquidation_buffer_percent
    ):
        rejections.append("estimated liquidation buffer is below minimum")
    if (
        projected_interest_usdt is not None
        and projected_interest_usdt > limits.max_hourly_interest_usdt
    ):
        rejections.append("projected hourly interest exceeds the configured cap")
    if (
        projected_total_liability_usdt is not None
        and projected_total_liability_usdt > limits.max_total_liability_usdt
    ):
        rejections.append("projected total liability exceeds the configured cap")
    if asset_usdt_price is None:
        rejections.append(
            "asset_usdt_price is required for non-stable asset liability caps"
        )
    if planned_borrow_usdt is None:
        warnings.append("planned borrow value in USDT was not calculated")

    return MarginBorrowPlan(
        timestamp=_utc_timestamp(),
        account_type=account_type,
        isolated_symbol=isolated_symbol,
        asset=asset,
        requested_amount=requested_amount,
        max_borrowable=max_borrowable,
        borrow_limit=borrow_limit,
        max_allowed_by_utilization=max_allowed_by_utilization,
        planned_borrow_amount=planned_borrow_amount,
        utilization_percent=utilization_percent,
        hourly_interest_rate=hourly_rate,
        projected_hourly_interest=projected_hourly_interest,
        projected_hourly_interest_usdt=projected_interest_usdt,
        debt_before=account_liability,
        projected_debt=projected_debt,
        margin_level_before=margin_level_before,
        margin_level_after_estimate=margin_level_after,
        liquidation_reference_level=limits.liquidation_margin_level,
        liquidation_buffer_percent=buffer_percent,
        projected_total_liability_usdt=projected_total_liability_usdt,
        allowed=not rejections,
        rejection_reasons=tuple(rejections),
        warnings=tuple(warnings),
        risk_limits=limits.as_dict(),
    )


def build_margin_repay_plan(
    discovery: MarginDiscovery,
    request: MarginRepayRequest,
) -> MarginRepayPlan:
    """Build a dry-run repay plan without submitting a repay request."""
    account_type = _account_type(request.account_type)
    asset = _asset(request.asset)
    requested_amount = _positive_number(
        request.requested_amount,
        "requested repay amount",
    )
    isolated_symbol = _required_isolated_symbol(account_type, request.isolated_symbol)
    snapshot = _account_snapshot(discovery, account_type, isolated_symbol)
    balance = _asset_balance(snapshot, asset)
    borrowed = balance.borrowed if balance is not None else 0.0
    interest = balance.interest if balance is not None else 0.0
    liability = borrowed + interest
    planned = min(requested_amount, liability)
    remaining = max(0.0, liability - planned)
    rejections: list[str] = []
    if liability <= 0:
        rejections.append("no outstanding liability exists for this asset")
    if planned <= 0:
        rejections.append("planned repay amount is zero")
    if not _repay_enabled(snapshot, asset):
        rejections.append("repay is not enabled for this margin account or asset")
    return MarginRepayPlan(
        timestamp=_utc_timestamp(),
        account_type=account_type,
        isolated_symbol=isolated_symbol,
        asset=asset,
        requested_amount=requested_amount,
        borrowed_amount=borrowed,
        interest_amount=interest,
        liability_amount=liability,
        planned_repay_amount=planned,
        remaining_liability=remaining,
        allowed=not rejections,
        rejection_reasons=tuple(rejections),
    )


async def build_margin_dry_run_report(
    settings: Settings,
    *,
    client: Any,
    include_isolated: bool,
    isolated_symbols: Sequence[str] = (),
    borrow_request: MarginBorrowRequest | None = None,
    repay_request: MarginRepayRequest | None = None,
    limits: MarginRiskLimits | None = None,
) -> MarginDryRunReport:
    """Build a full dry-run report from read-only margin endpoints."""
    discovery = await discover_margin_accounts(
        client,
        include_isolated=include_isolated,
        isolated_symbols=isolated_symbols,
    )
    borrow_plan = None
    if borrow_request is not None:
        borrow_plan = await build_margin_borrow_plan(
            client,
            discovery,
            borrow_request,
            limits=limits,
        )
    repay_plan = None
    if repay_request is not None:
        repay_plan = build_margin_repay_plan(discovery, repay_request)

    return MarginDryRunReport(
        timestamp=_utc_timestamp(),
        mode=_mode(settings),
        discovery=discovery.as_dict(),
        borrow_plan=borrow_plan.as_dict() if borrow_plan is not None else None,
        repay_plan=repay_plan.as_dict() if repay_plan is not None else None,
    )


def parse_cross_margin_snapshot(raw: Mapping[str, Any]) -> CrossMarginSnapshot:
    """Parse Binance's cross-margin account response."""
    if not isinstance(raw, Mapping):
        raise MarginModelError("cross margin account response was not a mapping")
    assets_raw = raw.get("userAssets")
    assets = (
        tuple(_parse_asset_balance(asset_raw) for asset_raw in assets_raw)
        if isinstance(assets_raw, Sequence)
        and not isinstance(assets_raw, (str, bytes))
        else ()
    )
    return CrossMarginSnapshot(
        created=_optional_bool(raw.get("created")),
        borrow_enabled=_optional_bool(raw.get("borrowEnabled")),
        trade_enabled=_optional_bool(raw.get("tradeEnabled")),
        transfer_in_enabled=_optional_bool(raw.get("transferInEnabled")),
        transfer_out_enabled=_optional_bool(raw.get("transferOutEnabled")),
        account_type=str(raw.get("accountType") or "cross"),
        margin_level=_optional_positive_float(raw.get("marginLevel")),
        collateral_margin_level=_optional_positive_float(
            raw.get("collateralMarginLevel")
        ),
        total_asset_btc=_non_negative_float(raw.get("totalAssetOfBtc")),
        total_liability_btc=_non_negative_float(raw.get("totalLiabilityOfBtc")),
        total_net_asset_btc=_non_negative_float(raw.get("totalNetAssetOfBtc")),
        total_collateral_usdt=_non_negative_float(
            raw.get("TotalCollateralValueInUSDT")
            if raw.get("TotalCollateralValueInUSDT") is not None
            else raw.get("totalCollateralValueInUSDT")
        ),
        total_open_order_loss_usdt=_non_negative_float(
            raw.get("totalOpenOrderLossInUSDT")
        ),
        assets=assets,
    )


def parse_isolated_margin_snapshots(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
) -> tuple[IsolatedMarginSnapshot, ...]:
    """Parse Binance's isolated-margin account response."""
    if isinstance(raw, Mapping):
        assets_raw = raw.get("assets", ())
    else:
        assets_raw = raw
    if not isinstance(assets_raw, Sequence) or isinstance(assets_raw, (str, bytes)):
        raise MarginModelError("isolated margin account response did not include assets")
    snapshots: list[IsolatedMarginSnapshot] = []
    for item in assets_raw:
        if not isinstance(item, Mapping):
            continue
        symbol = str(item.get("symbol") or "").strip().upper()
        if not symbol:
            continue
        base_raw = item.get("baseAsset") if isinstance(item.get("baseAsset"), Mapping) else {}
        quote_raw = (
            item.get("quoteAsset") if isinstance(item.get("quoteAsset"), Mapping) else {}
        )
        snapshots.append(
            IsolatedMarginSnapshot(
                symbol=symbol,
                isolated_created=_optional_bool(item.get("isolatedCreated")),
                enabled=_optional_bool(item.get("enabled")),
                trade_enabled=_optional_bool(item.get("tradeEnabled")),
                margin_level=_optional_positive_float(item.get("marginLevel")),
                margin_level_status=str(item.get("marginLevelStatus") or "unknown"),
                margin_ratio=_optional_positive_float(item.get("marginRatio")),
                index_price=_optional_positive_float(item.get("indexPrice")),
                liquidate_price=_optional_positive_float(item.get("liquidatePrice")),
                liquidate_rate=_optional_positive_float(item.get("liquidateRate")),
                base_asset=_parse_asset_balance(base_raw),
                quote_asset=_parse_asset_balance(quote_raw),
            )
        )
    return tuple(snapshots)


def assert_margin_live_disabled(*, confirm_live_margin: bool = False) -> None:
    """Fail closed for any attempted live margin activation."""
    if confirm_live_margin:
        raise MarginModelError(
            "live margin activation is not implemented; Phase 19 is dry-run only"
        )


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run read-only margin discovery and local risk modeling."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="required; confirms no live margin write action will be attempted",
    )
    parser.add_argument(
        "--confirm-live-margin",
        action="store_true",
        help="always fails; live margin activation is not implemented",
    )
    parser.add_argument(
        "--account-type",
        choices=(ACCOUNT_CROSS, ACCOUNT_ISOLATED),
        default=ACCOUNT_CROSS,
    )
    parser.add_argument("--isolated-symbol", help="isolated margin symbol")
    parser.add_argument(
        "--include-isolated",
        action="store_true",
        help="also query isolated margin account info",
    )
    parser.add_argument(
        "--asset",
        default="USDT",
        help="asset to model for borrow or repay",
    )
    parser.add_argument("--borrow-amount", type=float, help="borrow amount to model")
    parser.add_argument("--repay-amount", type=float, help="repay amount to model")
    parser.add_argument(
        "--asset-usdt-price",
        type=float,
        help="USDT price for non-stable assets when applying liability caps",
    )
    parser.add_argument(
        "--min-margin-level",
        type=float,
        default=MarginRiskLimits.min_margin_level,
    )
    parser.add_argument(
        "--liquidation-margin-level",
        type=float,
        default=MarginRiskLimits.liquidation_margin_level,
    )
    parser.add_argument(
        "--min-liquidation-buffer-percent",
        type=float,
        default=MarginRiskLimits.min_liquidation_buffer_percent,
    )
    parser.add_argument(
        "--max-borrow-utilization-percent",
        type=float,
        default=MarginRiskLimits.max_borrow_utilization_percent,
    )
    parser.add_argument(
        "--max-hourly-interest-usdt",
        type=float,
        default=MarginRiskLimits.max_hourly_interest_usdt,
    )
    parser.add_argument(
        "--max-total-liability-usdt",
        type=float,
        default=MarginRiskLimits.max_total_liability_usdt,
    )
    parser.add_argument("--json", action="store_true", help="print JSON output")
    args = parser.parse_args(argv)

    if not args.dry_run:
        print("margin check failed: --dry-run is required; live margin is disabled")
        return 1
    try:
        assert_margin_live_disabled(confirm_live_margin=args.confirm_live_margin)
        settings = Settings.from_env()
        limits = MarginRiskLimits(
            min_margin_level=args.min_margin_level,
            liquidation_margin_level=args.liquidation_margin_level,
            min_liquidation_buffer_percent=args.min_liquidation_buffer_percent,
            max_borrow_utilization_percent=args.max_borrow_utilization_percent,
            max_hourly_interest_usdt=args.max_hourly_interest_usdt,
            max_total_liability_usdt=args.max_total_liability_usdt,
        )
        include_isolated = args.include_isolated or args.account_type == ACCOUNT_ISOLATED
        isolated_symbols = (
            (args.isolated_symbol,)
            if args.isolated_symbol
            else (settings.trading_symbol,) if include_isolated else ()
        )
        borrow_request = (
            MarginBorrowRequest(
                account_type=args.account_type,
                isolated_symbol=args.isolated_symbol,
                asset=args.asset,
                requested_amount=args.borrow_amount,
                asset_usdt_price=args.asset_usdt_price,
            )
            if args.borrow_amount is not None
            else None
        )
        repay_request = (
            MarginRepayRequest(
                account_type=args.account_type,
                isolated_symbol=args.isolated_symbol,
                asset=args.asset,
                requested_amount=args.repay_amount,
            )
            if args.repay_amount is not None
            else None
        )
        report = asyncio.run(
            _run_margin_dry_run_report(
                settings,
                include_isolated=include_isolated,
                isolated_symbols=isolated_symbols,
                borrow_request=borrow_request,
                repay_request=repay_request,
                limits=limits,
            )
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_margin_report(report))
    except Exception as exc:
        print(f"margin check failed: {exc}")
        return 1
    return 0


def render_margin_report(report: MarginDryRunReport) -> str:
    """Render a concise human-readable margin dry-run report."""
    discovery = report.discovery
    cross = discovery.get("cross", {}) if isinstance(discovery, Mapping) else {}
    isolated = discovery.get("isolated", ()) if isinstance(discovery, Mapping) else ()
    lines = [
        f"Mode: {report.mode}",
        "Live margin trading: disabled",
        "Write endpoints used: none",
        f"Cross account type: {cross.get('account_type', 'unknown')}",
        f"Cross margin level: {cross.get('margin_level', 'unknown')}",
        f"Cross total liability BTC: {cross.get('total_liability_btc', 0.0)}",
        f"Isolated snapshots: {len(isolated) if isinstance(isolated, Sequence) else 0}",
    ]
    if report.borrow_plan:
        borrow = report.borrow_plan
        lines.extend(
            [
                "Borrow plan:",
                f"  - asset: {borrow['asset']}",
                f"  - planned amount: {borrow['planned_borrow_amount']}",
                f"  - allowed: {_yes_no(bool(borrow['allowed']))}",
                f"  - estimated margin level: {borrow['margin_level_after_estimate']}",
            ]
        )
        if borrow["rejection_reasons"]:
            lines.append("  - rejection reasons: " + ", ".join(borrow["rejection_reasons"]))
        if borrow["warnings"]:
            lines.append("  - warnings: " + ", ".join(borrow["warnings"]))
    if report.repay_plan:
        repay = report.repay_plan
        lines.extend(
            [
                "Repay plan:",
                f"  - asset: {repay['asset']}",
                f"  - planned amount: {repay['planned_repay_amount']}",
                f"  - remaining liability: {repay['remaining_liability']}",
                f"  - allowed: {_yes_no(bool(repay['allowed']))}",
            ]
        )
        if repay["rejection_reasons"]:
            lines.append("  - rejection reasons: " + ", ".join(repay["rejection_reasons"]))
    return "\n".join(lines)


async def _run_margin_dry_run_report(
    settings: Settings,
    *,
    include_isolated: bool,
    isolated_symbols: Sequence[str],
    borrow_request: MarginBorrowRequest | None,
    repay_request: MarginRepayRequest | None,
    limits: MarginRiskLimits,
) -> MarginDryRunReport:
    client = BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    try:
        return await build_margin_dry_run_report(
            settings,
            client=client,
            include_isolated=include_isolated,
            isolated_symbols=isolated_symbols,
            borrow_request=borrow_request,
            repay_request=repay_request,
            limits=limits,
        )
    finally:
        await client.close()


async def _query_max_borrowable(
    client: Any,
    *,
    account_type: str,
    asset: str,
    isolated_symbol: str | None,
) -> Mapping[str, Any]:
    params = {"asset": asset}
    if account_type == ACCOUNT_ISOLATED:
        params["isolatedSymbol"] = _exchange_symbol(isolated_symbol or "")
    payload = await client.request(
        "sapiGetMarginMaxBorrowable",
        params,
        request_weight=750,
    )
    if not isinstance(payload, Mapping):
        raise MarginModelError("max borrow response was not a mapping")
    return payload


async def _query_next_hourly_interest_rate(
    client: Any,
    *,
    account_type: str,
    asset: str,
) -> float:
    payload = await client.request(
        "sapiGetMarginNextHourlyInterestRate",
        {
            "assets": asset,
            "isIsolated": "TRUE" if account_type == ACCOUNT_ISOLATED else "FALSE",
        },
        request_weight=100,
    )
    if isinstance(payload, Mapping):
        values: Sequence[Any] = (payload,)
    elif isinstance(payload, Sequence) and not isinstance(payload, (str, bytes)):
        values = payload
    else:
        raise MarginModelError("next hourly interest response was not usable")
    for item in values:
        if not isinstance(item, Mapping):
            continue
        if _asset(item.get("asset") or asset) == asset:
            return _non_negative_float(item.get("nextHourlyInterestRate"))
    return 0.0


def _parse_asset_balance(raw: Mapping[str, Any]) -> MarginAssetBalance:
    return MarginAssetBalance(
        asset=_asset(raw.get("asset") or "UNKNOWN"),
        free=_non_negative_float(raw.get("free")),
        locked=_non_negative_float(raw.get("locked")),
        borrowed=_non_negative_float(raw.get("borrowed")),
        interest=_non_negative_float(raw.get("interest")),
        net_asset=_float(raw.get("netAsset")),
        total_asset=_non_negative_float(raw.get("totalAsset")),
        net_asset_btc=_non_negative_float(raw.get("netAssetOfBtc")),
        borrow_enabled=_optional_bool(raw.get("borrowEnabled")),
        repay_enabled=_optional_bool(raw.get("repayEnabled")),
    )


def _account_snapshot(
    discovery: MarginDiscovery,
    account_type: str,
    isolated_symbol: str | None,
) -> CrossMarginSnapshot | IsolatedMarginSnapshot:
    if account_type == ACCOUNT_CROSS:
        return discovery.cross
    if not isolated_symbol:
        raise MarginModelError("isolated_symbol is required for isolated margin")
    snapshot = discovery.isolated_snapshot(isolated_symbol)
    if snapshot is None:
        raise MarginModelError(f"isolated margin snapshot not found for {isolated_symbol}")
    return snapshot


def _asset_balance(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
    asset: str,
) -> MarginAssetBalance | None:
    return snapshot.asset_balance(asset)


def _asset_liability(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
    asset: str,
) -> float:
    return snapshot.asset_liability(asset)


def _borrow_enabled(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
    asset: str,
) -> bool:
    balance = _asset_balance(snapshot, asset)
    if isinstance(snapshot, CrossMarginSnapshot):
        if snapshot.created is False or snapshot.borrow_enabled is False:
            return False
        return True
    if snapshot.enabled is False:
        return False
    if balance is not None and balance.borrow_enabled is False:
        return False
    return True


def _repay_enabled(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
    asset: str,
) -> bool:
    balance = _asset_balance(snapshot, asset)
    if isinstance(snapshot, CrossMarginSnapshot):
        return snapshot.created is not False
    if snapshot.enabled is False:
        return False
    if balance is not None and balance.repay_enabled is False:
        return False
    return True


def _snapshot_margin_level(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
) -> float | None:
    return snapshot.margin_level


def _estimate_margin_level_after(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
    asset: str,
    new_liability_amount: float,
    asset_usdt_price: float | None,
) -> float | None:
    if asset_usdt_price is None:
        return _snapshot_margin_level(snapshot)
    asset = _asset(asset)
    additional_liability_usdt = new_liability_amount * asset_usdt_price
    if isinstance(snapshot, CrossMarginSnapshot):
        collateral_usdt = snapshot.total_collateral_usdt
        if collateral_usdt > 0:
            liability_usdt = (
                snapshot.asset_liability(asset) * asset_usdt_price
                + additional_liability_usdt
            )
            return (collateral_usdt / liability_usdt) if liability_usdt > 0 else None
        before = snapshot.margin_level
        before_liability = snapshot.asset_liability(asset) * asset_usdt_price
        if before is not None and before > 0 and before_liability > 0:
            asset_value = before * before_liability
            return asset_value / (before_liability + additional_liability_usdt)
        return before

    asset_value_usdt, liability_usdt = _isolated_usdt_values(snapshot)
    if asset_value_usdt is not None and liability_usdt is not None:
        return asset_value_usdt / (liability_usdt + additional_liability_usdt)
    before = snapshot.margin_level
    before_liability = snapshot.asset_liability(asset) * asset_usdt_price
    if before is not None and before > 0 and before_liability > 0:
        asset_value = before * before_liability
        return asset_value / (before_liability + additional_liability_usdt)
    return before


def _projected_liability_usdt(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
    asset: str,
    new_liability_amount: float,
    asset_usdt_price: float | None,
) -> float | None:
    if asset_usdt_price is None:
        return None
    asset = _asset(asset)
    additional = new_liability_amount * asset_usdt_price
    if isinstance(snapshot, IsolatedMarginSnapshot):
        _, existing = _isolated_usdt_values(snapshot)
        return (existing or 0.0) + additional
    return snapshot.asset_liability(asset) * asset_usdt_price + additional


def _isolated_usdt_values(
    snapshot: IsolatedMarginSnapshot,
) -> tuple[float | None, float | None]:
    quote_price = 1.0 if _is_stable_asset(snapshot.quote_asset.asset) else None
    if snapshot.index_price is None or quote_price is None:
        return None, None
    asset_value = (
        snapshot.base_asset.total_asset * snapshot.index_price
        + snapshot.quote_asset.total_asset * quote_price
    )
    liability = (
        snapshot.base_asset.liability * snapshot.index_price
        + snapshot.quote_asset.liability * quote_price
    )
    return asset_value, liability


def _liquidation_buffer_percent(
    margin_level: float | None,
    liquidation_margin_level: float,
) -> float | None:
    if margin_level is None:
        return None
    if liquidation_margin_level <= 0:
        return None
    return ((margin_level - liquidation_margin_level) / liquidation_margin_level) * 100.0


def _asset_usdt_price(asset: str, value: float | None) -> float | None:
    if _is_stable_asset(asset):
        return 1.0
    if value is None:
        return None
    return _positive_number(value, "asset_usdt_price")


def _infer_asset_usdt_price(
    snapshot: CrossMarginSnapshot | IsolatedMarginSnapshot,
    asset: str,
) -> float | None:
    asset = _asset(asset)
    if _is_stable_asset(asset):
        return 1.0
    if (
        isinstance(snapshot, IsolatedMarginSnapshot)
        and snapshot.base_asset.asset == asset
        and _is_stable_asset(snapshot.quote_asset.asset)
        and snapshot.index_price is not None
    ):
        return snapshot.index_price
    return None


def _is_stable_asset(asset: str) -> bool:
    return _asset(asset) in {"USDT", "USDC", "FDUSD", "BUSD", "TUSD", "DAI"}


def _validate_limits(limits: MarginRiskLimits) -> None:
    _positive_number(limits.min_margin_level, "min_margin_level")
    _positive_number(limits.liquidation_margin_level, "liquidation_margin_level")
    _positive_number(
        limits.min_liquidation_buffer_percent,
        "min_liquidation_buffer_percent",
    )
    utilization = _positive_number(
        limits.max_borrow_utilization_percent,
        "max_borrow_utilization_percent",
    )
    if utilization > 100:
        raise MarginModelError("max_borrow_utilization_percent must be <= 100")
    _positive_number(limits.max_hourly_interest_usdt, "max_hourly_interest_usdt")
    _positive_number(limits.max_total_liability_usdt, "max_total_liability_usdt")


def _required_isolated_symbol(account_type: str, symbol: str | None) -> str | None:
    if account_type != ACCOUNT_ISOLATED:
        return None
    if not symbol or not symbol.strip():
        raise MarginModelError("isolated_symbol is required for isolated margin")
    return normalize_symbol(symbol)


def _account_type(value: str) -> str:
    normalized = value.strip().lower().replace("-", "_")
    if normalized not in {ACCOUNT_CROSS, ACCOUNT_ISOLATED}:
        raise MarginModelError("account_type must be cross or isolated")
    return normalized


def _asset(value: object) -> str:
    text = str(value or "").strip().upper()
    if not text:
        raise MarginModelError("asset must not be blank")
    return text


def _exchange_symbol(symbol: str) -> str:
    text = str(symbol or "").strip()
    if not text:
        raise MarginModelError("symbol must not be blank")
    return re.sub(r"[^A-Za-z0-9]", "", text).upper()


def _positive_number(value: float, field_name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise MarginModelError(f"{field_name} must be a number") from exc
    if not math.isfinite(number) or number <= 0:
        raise MarginModelError(f"{field_name} must be greater than zero")
    return number


def _non_negative_float(value: object) -> float:
    number = _float(value)
    return number if number > 0 else 0.0


def _optional_positive_float(value: object) -> float | None:
    number = _float(value)
    return number if number > 0 else None


def _float(value: object) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return 0.0
    return number if math.isfinite(number) else 0.0


def _optional_bool(value: object) -> bool | None:
    return value if isinstance(value, bool) else None


def _mode(settings: Settings) -> str:
    if settings.dry_run:
        return "DRY_RUN"
    if settings.use_testnet:
        return "testnet"
    return "LIVE"


def _yes_no(value: bool) -> str:
    return "yes" if value else "no"


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


if __name__ == "__main__":
    raise SystemExit(run())

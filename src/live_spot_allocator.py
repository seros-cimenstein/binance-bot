"""Guarded foreground live spot allocator for controlled pool execution."""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from decimal import Decimal, ROUND_DOWN
from pathlib import Path
from typing import Any, Mapping

from src.api_permissions import (
    ApiPermissionError,
    ensure_feature_allowed,
    ensure_spot_order_test_allowed,
    inspect_api_permissions,
)
from src.config import Settings
from src.exchange.client import BinanceClient
from src.pool.fees import CommissionService
from src.scope_audit import (
    ScopeAuditError,
    build_scope_audit_report,
    ensure_scope_allowed,
)
from src.telegram_alerts import TelegramAlerter


class LiveSpotAllocatorError(RuntimeError):
    """Raised when the guarded live spot allocator cannot proceed."""


@dataclass(frozen=True, slots=True)
class LiveSpotLimits:
    """Hard risk limits for one live spot allocation cycle."""

    max_order_notional_usdt: float = 6.0
    max_daily_notional_usdt: float = 12.0
    max_daily_orders: int = 2
    max_spread_bps: float = 50.0
    slippage_buffer_bps: float = 25.0
    max_turnover_percent: float = 100.0
    max_asset_exposure_percent: float = 100.0
    max_total_exposure_usdt: float = 0.0
    daily_loss_limit_usdt: float = 0.0
    max_drawdown_percent: float = 0.0
    min_bnb_reserve: float = 0.00005
    max_route_legs: int = 1
    audit_path: str = "data/live-spot-allocations.jsonl"
    emergency_stop_path: str = "data/live-spot.stop"


@dataclass(frozen=True, slots=True)
class LiveSpotIntent:
    """Requested live spot allocation intent."""

    source_asset: str
    destination_asset: str
    requested_source_amount: float | None = None
    reason: str = "manual foreground live spot allocation"


@dataclass(frozen=True, slots=True)
class LiveSpotPlan:
    """Auditable live spot allocation plan."""

    timestamp: str
    intent: dict[str, Any]
    symbol: str
    side: str
    order_amount: float
    source_spend_amount: float
    estimated_price: float
    estimated_notional_usdt: float
    expected_destination_amount: float
    fee_rate: float
    estimated_fee_amount: float
    min_amount: float
    min_notional_usdt: float
    spread_bps: float
    route_quote: dict[str, Any]
    portfolio_value_usdt_before: float
    portfolio_value_usdt_after: float
    destination_value_usdt_after: float
    benchmark_values_after: dict[str, float]
    risk: dict[str, Any]
    commission: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class LiveSpotResult:
    """Plan or execution result for one guarded allocation cycle."""

    timestamp: str
    mode: str
    status: str
    plan: dict[str, Any]
    submission_attempted: bool
    order: dict[str, Any] = field(default_factory=dict)
    final_order: dict[str, Any] = field(default_factory=dict)
    balance_before: dict[str, Any] = field(default_factory=dict)
    balance_after: dict[str, Any] = field(default_factory=dict)
    open_orders_after: list[dict[str, Any]] = field(default_factory=list)
    reconciliation: dict[str, Any] = field(default_factory=dict)
    error: str = ""
    audit_path: str = ""
    telegram_sent: bool = False

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def run_live_spot_allocator(
    settings: Settings,
    *,
    submit_order: bool,
    confirm_live_allocator: bool = False,
    confirm_canary_reviewed: bool = False,
    confirm_replay_passed: bool = False,
    confirm_paper_warmup_reviewed: bool = False,
    confirm_permission_diagnostics: bool = False,
    intent: LiveSpotIntent,
    limits: LiveSpotLimits | None = None,
    client: Any | None = None,
    alerter: Any | None = None,
) -> LiveSpotResult:
    """Plan or submit one guarded live spot allocation cycle."""
    limits = limits or LiveSpotLimits()
    _validate_limits(limits)
    _preflight(
        settings,
        submit_order=submit_order,
        confirm_live_allocator=confirm_live_allocator,
        confirm_canary_reviewed=confirm_canary_reviewed,
        confirm_replay_passed=confirm_replay_passed,
        confirm_paper_warmup_reviewed=confirm_paper_warmup_reviewed,
        confirm_permission_diagnostics=confirm_permission_diagnostics,
    )
    owns_client = client is None
    client = client or BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    alerter = alerter or TelegramAlerter(
        settings.telegram_bot_token,
        settings.telegram_chat_id,
    )
    audit_path = Path(limits.audit_path)
    try:
        plan, balance_before = await build_live_spot_plan(
            client,
            intent=intent,
            limits=limits,
        )
        try:
            ensure_scope_allowed(
                build_scope_audit_report(
                    settings,
                    canary_symbols=(plan.symbol,),
                )
            )
        except ScopeAuditError as exc:
            raise LiveSpotAllocatorError(str(exc)) from exc
        if not submit_order:
            result = LiveSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="planned",
                plan=plan.as_dict(),
                submission_attempted=False,
                balance_before=balance_before,
            )
            audit_file = _write_audit(audit_path, result)
            return LiveSpotResult(
                **{**result.as_dict(), "audit_path": str(audit_file)}
            )

        try:
            report = await inspect_api_permissions(
                settings,
                client=client,
                spot_test_symbol=plan.symbol,
                feature_names=("spot_live_pool",),
            )
            ensure_feature_allowed(report, "spot_live_pool")
            await ensure_spot_order_test_allowed(
                client,
                symbol=plan.symbol,
                side=plan.side,
                order_type="market",
                quantity=plan.order_amount,
            )
        except ApiPermissionError as exc:
            result = LiveSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="blocked",
                plan=plan.as_dict(),
                submission_attempted=False,
                balance_before=balance_before,
                error=str(exc),
            )
            audit_file = _write_audit(audit_path, result)
            await _send_allocator_alert(alerter, result, audit_file)
            raise LiveSpotAllocatorError(str(exc)) from exc

        try:
            order = await client.create_order(
                plan.symbol,
                "market",
                plan.side,
                plan.order_amount,
            )
        except Exception as exc:
            result = LiveSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="submission_error",
                plan=plan.as_dict(),
                submission_attempted=True,
                balance_before=balance_before,
                error=str(exc),
            )
            audit_file = _write_audit(audit_path, result)
            await _send_allocator_alert(alerter, result, audit_file)
            raise LiveSpotAllocatorError(
                "live spot allocation order failed; no retry was attempted"
            ) from exc

        final_order = await _final_order_status(client, plan.symbol, order)
        balance_after_raw = await client.get_balance()
        balance_after = _sanitize_balance(balance_after_raw)
        open_orders = await _open_orders_after(client, plan.symbol)
        reconciliation = _reconcile_balances(
            plan,
            balance_before,
            balance_after,
        )
        result = LiveSpotResult(
            timestamp=_utc_timestamp(),
            mode=_mode(settings),
            status="submitted",
            plan=plan.as_dict(),
            submission_attempted=True,
            order=_sanitize_order(order),
            final_order=_sanitize_order(final_order),
            balance_before=balance_before,
            balance_after=balance_after,
            open_orders_after=open_orders,
            reconciliation=reconciliation,
        )
        audit_file = _write_audit(audit_path, result)
        telegram_sent = await _send_allocator_alert(alerter, result, audit_file)
        return LiveSpotResult(
            **{
                **result.as_dict(),
                "audit_path": str(audit_file),
                "telegram_sent": telegram_sent,
            }
        )
    finally:
        if owns_client:
            await client.close()


async def build_live_spot_plan(
    client: Any,
    *,
    intent: LiveSpotIntent,
    limits: LiveSpotLimits,
) -> tuple[LiveSpotPlan, dict[str, Any]]:
    """Build a guarded direct-market allocation plan from live data."""
    _validate_limits(limits)
    _ensure_no_emergency_stop(limits.emergency_stop_path)
    source = intent.source_asset.strip().upper()
    destination = intent.destination_asset.strip().upper()
    if not source or not destination:
        raise LiveSpotAllocatorError("source and destination assets are required")
    if source == destination:
        raise LiveSpotAllocatorError("source and destination assets must differ")
    if limits.max_route_legs != 1:
        raise LiveSpotAllocatorError("live spot allocator v1 supports direct routes only")

    usage = _daily_usage(Path(limits.audit_path))
    if usage["orders"] >= limits.max_daily_orders:
        raise LiveSpotAllocatorError("live spot daily order limit is exhausted")
    if (
        limits.daily_loss_limit_usdt > 0
        and usage["realized_loss_usdt"] >= limits.daily_loss_limit_usdt
    ):
        raise LiveSpotAllocatorError("live spot daily loss limit is exhausted")
    remaining_daily_notional = limits.max_daily_notional_usdt - usage["notional"]
    if remaining_daily_notional <= 0:
        raise LiveSpotAllocatorError("live spot daily notional limit is exhausted")

    balance = await client.get_balance()
    balance_snapshot = _sanitize_balance(balance)
    free_balances = balance.get("free", {}) if isinstance(balance, Mapping) else {}
    free_source = _positive_float(
        free_balances.get(source) if isinstance(free_balances, Mapping) else None
    )
    if free_source is None:
        raise LiveSpotAllocatorError(f"no free {source} balance is available")

    markets = await client.request("load_markets", request_weight=10)
    if not isinstance(markets, Mapping):
        raise LiveSpotAllocatorError("load_markets did not return a market mapping")
    market, symbol, side = _direct_market(markets, source, destination)
    ticker = await client.fetch_ticker(symbol)
    price, spread_bps = _execution_price_and_spread(ticker, side)
    if price is None:
        raise LiveSpotAllocatorError(f"{symbol} ticker did not include a price")
    if spread_bps is None:
        raise LiveSpotAllocatorError(f"{symbol} ticker did not include bid/ask spread")
    if spread_bps > limits.max_spread_bps:
        raise LiveSpotAllocatorError("live spot spread exceeds configured limit")

    min_amount = _market_min_amount(market)
    min_notional = _market_min_notional(market)
    precision = _market_amount_precision(market)
    spend_cap = min(limits.max_order_notional_usdt, remaining_daily_notional)
    if intent.requested_source_amount is not None:
        requested = _positive_float(intent.requested_source_amount)
        if requested is None:
            raise LiveSpotAllocatorError("requested_source_amount must be positive")
        if side == "sell":
            spend_cap = min(spend_cap, requested * price)
        else:
            spend_cap = min(spend_cap, requested)

    order_amount, source_spend, notional = _sized_order(
        side=side,
        free_source=free_source,
        price=price,
        spend_cap_usdt=spend_cap,
        min_amount=min_amount,
        min_notional=min_notional,
        amount_precision=precision,
    )
    _check_bnb_reserve(source, free_source, source_spend, limits.min_bnb_reserve)

    commission = await CommissionService(client).get(symbol)
    fee_rate = commission.taker_rate(side)
    gross_destination = notional if side == "sell" else order_amount
    expected_destination, fee_amount = _expected_destination(
        gross_destination=gross_destination,
        fee_rate=fee_rate,
    )
    valuations = await _portfolio_valuations(
        client,
        markets,
        balance_snapshot,
        source_asset=source,
        destination_asset=destination,
        source_delta=-source_spend,
        destination_delta=expected_destination,
    )
    _risk_check(
        limits,
        usage,
        notional=notional,
        portfolio_value_before=valuations["total_before"],
        portfolio_value_after=valuations["total_after"],
        destination_value_after=valuations["destination_value_after"],
    )
    route_quote = {
        "source_asset": source,
        "destination_asset": destination,
        "input_amount": source_spend,
        "output_amount": expected_destination,
        "slippage_bps": spread_bps + limits.slippage_buffer_bps,
        "legs": [
            {
                "symbol": symbol,
                "side": side,
                "source_asset": source,
                "destination_asset": destination,
                "input_amount": source_spend,
                "spent_amount": source_spend,
                "gross_output_amount": (
                    notional if side == "sell" else order_amount
                ),
                "fee_rate": fee_rate,
                "fee_output_amount": fee_amount,
                "net_output_amount": expected_destination,
                "commission_discount_asset": (
                    commission.discount_asset if commission.discount_enabled else ""
                ),
            }
        ],
    }
    risk = {
        "daily_orders_used": usage["orders"],
        "daily_notional_used_usdt": usage["notional"],
        "daily_realized_loss_usdt": usage["realized_loss_usdt"],
        "max_order_notional_usdt": limits.max_order_notional_usdt,
        "max_daily_notional_usdt": limits.max_daily_notional_usdt,
        "max_daily_orders": limits.max_daily_orders,
        "max_turnover_percent": limits.max_turnover_percent,
        "max_asset_exposure_percent": limits.max_asset_exposure_percent,
        "max_total_exposure_usdt": limits.max_total_exposure_usdt,
        "daily_loss_limit_usdt": limits.daily_loss_limit_usdt,
        "max_drawdown_percent": limits.max_drawdown_percent,
        "min_bnb_reserve": limits.min_bnb_reserve,
        "max_route_legs": limits.max_route_legs,
    }
    return (
        LiveSpotPlan(
            timestamp=_utc_timestamp(),
            intent=asdict(intent),
            symbol=symbol,
            side=side,
            order_amount=order_amount,
            source_spend_amount=source_spend,
            estimated_price=price,
            estimated_notional_usdt=notional,
            expected_destination_amount=expected_destination,
            fee_rate=fee_rate,
            estimated_fee_amount=fee_amount,
            min_amount=min_amount,
            min_notional_usdt=min_notional,
            spread_bps=spread_bps,
            route_quote=route_quote,
            portfolio_value_usdt_before=valuations["total_before"],
            portfolio_value_usdt_after=valuations["total_after"],
            destination_value_usdt_after=valuations["destination_value_after"],
            benchmark_values_after=valuations["benchmarks_after"],
            risk=risk,
            commission=_commission_dict(commission, side),
        ),
        balance_snapshot,
    )


def _preflight(
    settings: Settings,
    *,
    submit_order: bool,
    confirm_live_allocator: bool,
    confirm_canary_reviewed: bool,
    confirm_replay_passed: bool,
    confirm_paper_warmup_reviewed: bool,
    confirm_permission_diagnostics: bool,
) -> None:
    if not submit_order:
        return
    if not confirm_live_allocator:
        raise LiveSpotAllocatorError(
            "live spot allocator requires --confirm-live-allocator"
        )
    if not confirm_canary_reviewed:
        raise LiveSpotAllocatorError(
            "live spot allocator requires --confirm-canary-reviewed"
        )
    if not confirm_replay_passed:
        raise LiveSpotAllocatorError(
            "live spot allocator requires --confirm-replay-passed"
        )
    if not confirm_paper_warmup_reviewed:
        raise LiveSpotAllocatorError(
            "live spot allocator requires --confirm-paper-warmup-reviewed"
        )
    if not confirm_permission_diagnostics:
        raise LiveSpotAllocatorError(
            "live spot allocator requires --confirm-permission-diagnostics"
        )
    if settings.use_testnet:
        raise LiveSpotAllocatorError("live spot allocator requires USE_TESTNET=false")
    if settings.dry_run:
        raise LiveSpotAllocatorError("live spot allocator requires DRY_RUN=false")
    if settings.confirm_live_trading != "yes":
        raise LiveSpotAllocatorError(
            "live spot allocator requires CONFIRM_LIVE_TRADING=yes"
        )


def _direct_market(
    markets: Mapping[str, Any],
    source: str,
    destination: str,
) -> tuple[Mapping[str, Any], str, str]:
    sell_symbol = f"{source}/{destination}"
    sell_market = markets.get(sell_symbol)
    if _is_active_spot_market(sell_market):
        return sell_market, sell_symbol, "sell"
    buy_symbol = f"{destination}/{source}"
    buy_market = markets.get(buy_symbol)
    if _is_active_spot_market(buy_market):
        return buy_market, buy_symbol, "buy"
    raise LiveSpotAllocatorError(
        f"no active direct spot market for {source}->{destination}"
    )


def _sized_order(
    *,
    side: str,
    free_source: float,
    price: float,
    spend_cap_usdt: float,
    min_amount: float,
    min_notional: float,
    amount_precision: int | float | None,
) -> tuple[float, float, float]:
    if side == "sell":
        raw_base = min(free_source, spend_cap_usdt / price)
        order_amount = _round_down(raw_base, amount_precision)
        source_spend = order_amount
        notional = order_amount * price
    else:
        quote_spend = min(free_source, spend_cap_usdt)
        order_amount = _round_down(quote_spend / price, amount_precision)
        source_spend = order_amount * price
        notional = source_spend
    if order_amount <= 0:
        raise LiveSpotAllocatorError("live spot order amount rounded to zero")
    if order_amount < min_amount or notional < min_notional:
        raise LiveSpotAllocatorError(
            "live spot order is below Binance minimum amount or notional"
        )
    return order_amount, source_spend, notional


def _expected_destination(
    *,
    gross_destination: float,
    fee_rate: float,
) -> tuple[float, float]:
    fee = gross_destination * fee_rate
    return gross_destination - fee, fee


def _risk_check(
    limits: LiveSpotLimits,
    usage: Mapping[str, float | int],
    *,
    notional: float,
    portfolio_value_before: float,
    portfolio_value_after: float,
    destination_value_after: float,
) -> None:
    if notional > limits.max_order_notional_usdt + 1e-9:
        raise LiveSpotAllocatorError("order notional exceeds max order limit")
    if usage["notional"] + notional > limits.max_daily_notional_usdt + 1e-9:
        raise LiveSpotAllocatorError("order notional exceeds daily notional limit")
    if portfolio_value_before > 0:
        turnover = notional / portfolio_value_before * 100
        if turnover > limits.max_turnover_percent:
            raise LiveSpotAllocatorError("turnover exceeds configured limit")
    if portfolio_value_after > 0:
        exposure = destination_value_after / portfolio_value_after * 100
        if exposure > limits.max_asset_exposure_percent:
            raise LiveSpotAllocatorError("destination exposure exceeds limit")
    if (
        limits.max_total_exposure_usdt > 0
        and portfolio_value_after > limits.max_total_exposure_usdt
    ):
        raise LiveSpotAllocatorError("total exposure exceeds configured limit")
    if (
        limits.daily_loss_limit_usdt > 0
        and usage["realized_loss_usdt"] >= limits.daily_loss_limit_usdt
    ):
        raise LiveSpotAllocatorError("daily realized loss limit is active")
    equity_high = float(usage.get("equity_high_usdt") or 0.0)
    if equity_high > 0 and limits.max_drawdown_percent > 0:
        drawdown = max((equity_high - portfolio_value_before) / equity_high, 0.0)
        if drawdown * 100 > limits.max_drawdown_percent:
            raise LiveSpotAllocatorError("drawdown limit is active")


async def _portfolio_valuations(
    client: Any,
    markets: Mapping[str, Any],
    balance: Mapping[str, Any],
    *,
    source_asset: str,
    destination_asset: str,
    source_delta: float,
    destination_delta: float,
) -> dict[str, Any]:
    totals = balance.get("total") if isinstance(balance.get("total"), Mapping) else {}
    balances = {
        str(asset).upper(): float(amount)
        for asset, amount in totals.items()
        if _positive_float(amount) is not None
    }
    before = await _value_balances_usdt(client, markets, balances)
    after_balances = dict(balances)
    after_balances[source_asset] = max(
        after_balances.get(source_asset, 0.0) + source_delta,
        0.0,
    )
    after_balances[destination_asset] = (
        after_balances.get(destination_asset, 0.0) + destination_delta
    )
    after = await _value_balances_usdt(client, markets, after_balances)
    destination_value = await _asset_value_usdt(
        client,
        markets,
        destination_asset,
        after_balances.get(destination_asset, 0.0),
    )
    benchmarks = await _benchmark_values(client, markets, after)
    return {
        "total_before": before,
        "total_after": after,
        "destination_value_after": destination_value,
        "benchmarks_after": benchmarks,
    }


async def _value_balances_usdt(
    client: Any,
    markets: Mapping[str, Any],
    balances: Mapping[str, float],
) -> float:
    total = 0.0
    for asset, amount in balances.items():
        total += await _asset_value_usdt(client, markets, asset, amount)
    return total


async def _asset_value_usdt(
    client: Any,
    markets: Mapping[str, Any],
    asset: str,
    amount: float,
) -> float:
    if amount <= 0:
        return 0.0
    if asset == "USDT":
        return amount
    sell_symbol = f"{asset}/USDT"
    if _is_active_spot_market(markets.get(sell_symbol)):
        ticker = await client.fetch_ticker(sell_symbol)
        price = _positive_float(ticker.get("bid")) or _positive_float(ticker.get("last"))
        return amount * price if price else 0.0
    buy_symbol = f"USDT/{asset}"
    if _is_active_spot_market(markets.get(buy_symbol)):
        ticker = await client.fetch_ticker(buy_symbol)
        price = _positive_float(ticker.get("ask")) or _positive_float(ticker.get("last"))
        return amount / price if price else 0.0
    return 0.0


async def _benchmark_values(
    client: Any,
    markets: Mapping[str, Any],
    total_usdt: float,
) -> dict[str, float]:
    benchmarks = {"USDT": total_usdt}
    for asset in ("BTC", "PAXG", "XAUT"):
        symbol = f"{asset}/USDT"
        if not _is_active_spot_market(markets.get(symbol)):
            continue
        ticker = await client.fetch_ticker(symbol)
        price = _positive_float(ticker.get("ask")) or _positive_float(ticker.get("last"))
        if price:
            benchmarks[asset] = total_usdt / price
    return benchmarks


def _daily_usage(path: Path) -> dict[str, float | int]:
    today = datetime.now(timezone.utc).date().isoformat()
    orders = 0
    notional = 0.0
    realized_loss = 0.0
    equity_high = 0.0
    if not path.exists():
        return _usage(orders, notional, realized_loss, equity_high)
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        timestamp = str(payload.get("timestamp") or "")
        if not timestamp.startswith(today):
            continue
        plan = payload.get("plan") if isinstance(payload.get("plan"), Mapping) else {}
        reconciliation = (
            payload.get("reconciliation")
            if isinstance(payload.get("reconciliation"), Mapping)
            else {}
        )
        equity_values = [
            _positive_float(plan.get("portfolio_value_usdt_before")),
            _positive_float(plan.get("portfolio_value_usdt_after")),
            _positive_float(reconciliation.get("portfolio_value_usdt_after")),
        ]
        equity_high = max(equity_high, *(value or 0.0 for value in equity_values))
        if not payload.get("submission_attempted"):
            continue
        orders += 1
        notional += _positive_float(plan.get("estimated_notional_usdt")) or 0.0
        realized_loss += _positive_float(payload.get("realized_loss_usdt")) or 0.0
    return _usage(orders, notional, realized_loss, equity_high)


def _usage(
    orders: int,
    notional: float,
    realized_loss: float,
    equity_high: float,
) -> dict[str, float | int]:
    return {
        "orders": orders,
        "notional": notional,
        "realized_loss_usdt": realized_loss,
        "equity_high_usdt": equity_high,
    }


def _validate_limits(limits: LiveSpotLimits) -> None:
    for value, name in (
        (limits.max_order_notional_usdt, "max_order_notional_usdt"),
        (limits.max_daily_notional_usdt, "max_daily_notional_usdt"),
        (limits.max_spread_bps, "max_spread_bps"),
        (limits.max_turnover_percent, "max_turnover_percent"),
        (limits.max_asset_exposure_percent, "max_asset_exposure_percent"),
    ):
        _positive_limit(value, name)
    for value, name in (
        (limits.slippage_buffer_bps, "slippage_buffer_bps"),
        (limits.max_total_exposure_usdt, "max_total_exposure_usdt"),
        (limits.daily_loss_limit_usdt, "daily_loss_limit_usdt"),
        (limits.max_drawdown_percent, "max_drawdown_percent"),
        (limits.min_bnb_reserve, "min_bnb_reserve"),
    ):
        _non_negative_limit(value, name)
    if limits.max_daily_orders <= 0:
        raise LiveSpotAllocatorError("max_daily_orders must be greater than zero")
    if limits.max_daily_notional_usdt < limits.max_order_notional_usdt:
        raise LiveSpotAllocatorError(
            "max_daily_notional_usdt must be at least max_order_notional_usdt"
        )
    if limits.max_route_legs != 1:
        raise LiveSpotAllocatorError("max_route_legs must be 1 for allocator v1")
    if not limits.audit_path.strip():
        raise LiveSpotAllocatorError("audit_path must not be blank")
    if not limits.emergency_stop_path.strip():
        raise LiveSpotAllocatorError("emergency_stop_path must not be blank")


def _positive_limit(value: float, name: str) -> None:
    if not math.isfinite(value) or value <= 0:
        raise LiveSpotAllocatorError(f"{name} must be a positive finite value")


def _non_negative_limit(value: float, name: str) -> None:
    if not math.isfinite(value) or value < 0:
        raise LiveSpotAllocatorError(f"{name} must be a finite non-negative value")


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise LiveSpotAllocatorError(f"live spot emergency stop is active: {path}")


def _check_bnb_reserve(
    source: str,
    free_source: float,
    source_spend: float,
    min_bnb_reserve: float,
) -> None:
    if source == "BNB" and free_source - source_spend < min_bnb_reserve:
        raise LiveSpotAllocatorError("BNB reserve would be depleted")


async def _final_order_status(
    client: Any,
    symbol: str,
    order: Mapping[str, Any],
) -> Mapping[str, Any]:
    status = str(order.get("status") or "").lower()
    order_id = _order_id(order)
    if status in {"closed", "canceled", "rejected", "expired"} or not order_id:
        return order
    for _ in range(3):
        await asyncio.sleep(1)
        fetched = await client.request("fetch_order", order_id, symbol, request_weight=1)
        fetched_status = str(fetched.get("status") or "").lower()
        if fetched_status in {"closed", "canceled", "rejected", "expired"}:
            return fetched
    return fetched


async def _open_orders_after(client: Any, symbol: str) -> list[dict[str, Any]]:
    try:
        orders = await client.request("fetch_open_orders", symbol, request_weight=3)
    except Exception as exc:
        return [{"error": str(exc)}]
    if not isinstance(orders, list):
        return []
    return [_sanitize_order(order) for order in orders if isinstance(order, Mapping)]


def _reconcile_balances(
    plan: LiveSpotPlan,
    balance_before: Mapping[str, Any],
    balance_after: Mapping[str, Any],
) -> dict[str, Any]:
    before_total = (
        balance_before.get("total")
        if isinstance(balance_before.get("total"), Mapping)
        else {}
    )
    after_total = (
        balance_after.get("total")
        if isinstance(balance_after.get("total"), Mapping)
        else {}
    )
    source = plan.intent["source_asset"].upper()
    destination = plan.intent["destination_asset"].upper()
    source_delta = _balance_amount(after_total, source) - _balance_amount(
        before_total,
        source,
    )
    destination_delta = _balance_amount(after_total, destination) - _balance_amount(
        before_total,
        destination,
    )
    return {
        "source_asset": source,
        "destination_asset": destination,
        "expected_source_delta": -plan.source_spend_amount,
        "actual_source_delta": source_delta,
        "expected_destination_delta": plan.expected_destination_amount,
        "actual_destination_delta": destination_delta,
        "portfolio_value_usdt_after": plan.portfolio_value_usdt_after,
    }


async def _send_allocator_alert(
    alerter: Any,
    result: LiveSpotResult,
    audit_path: Path,
) -> bool:
    send = getattr(alerter, "send", None)
    if send is None or not callable(send):
        return False
    plan = result.plan
    return bool(
        await send(
            "<b>LIVE SPOT ALLOCATOR</b>\n"
            f"Status: <code>{result.status}</code>\n"
            f"Side: <code>{str(plan.get('side', '')).upper()}</code>\n"
            f"Symbol: <code>{plan.get('symbol', '')}</code>\n"
            f"Amount: <code>{float(plan.get('order_amount', 0.0)):g}</code>\n"
            f"Audit: <code>{audit_path}</code>"
        )
    )


def _write_audit(path: Path, result: LiveSpotResult) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(result.as_dict(), sort_keys=True) + "\n")
    return path


def _sanitize_order(order: Mapping[str, Any]) -> dict[str, Any]:
    allowed = {
        "id",
        "clientOrderId",
        "orderId",
        "symbol",
        "type",
        "side",
        "status",
        "amount",
        "filled",
        "remaining",
        "average",
        "price",
        "cost",
        "fee",
        "fees",
        "timestamp",
        "datetime",
    }
    return {key: order[key] for key in allowed if key in order}


def _sanitize_balance(balance: object) -> dict[str, Any]:
    if not isinstance(balance, Mapping):
        return {}
    sanitized: dict[str, Any] = {}
    for group_name in ("free", "used", "total"):
        group = balance.get(group_name)
        if not isinstance(group, Mapping):
            continue
        values = {
            str(asset).upper(): amount
            for asset, amount in group.items()
            if _positive_float(amount) is not None
        }
        if values:
            sanitized[group_name] = values
    return sanitized


def _commission_dict(commission: Any, side: str) -> dict[str, Any]:
    return {
        "symbol": commission.symbol,
        "taker_rate": commission.taker_rate(side),
        "discount_enabled": commission.discount_enabled,
        "discount_asset": commission.discount_asset,
        "retrieved_at": commission.retrieved_at.isoformat(),
    }


def _execution_price_and_spread(
    ticker: Mapping[str, Any],
    side: str,
) -> tuple[float | None, float | None]:
    bid = _positive_float(ticker.get("bid"))
    ask = _positive_float(ticker.get("ask"))
    last = _positive_float(ticker.get("last"))
    price = bid if side == "sell" else ask
    if price is None:
        price = last
    if bid is None or ask is None or ask < bid:
        return price, None
    spread = (ask - bid) / ((ask + bid) / 2) * 10_000
    return price, spread


def _is_active_spot_market(market: object) -> bool:
    return (
        isinstance(market, Mapping)
        and bool(market.get("spot", True))
        and market.get("active", True) is True
    )


def _market_min_amount(market: Mapping[str, Any]) -> float:
    limits = market.get("limits") if isinstance(market.get("limits"), Mapping) else {}
    amount = limits.get("amount") if isinstance(limits.get("amount"), Mapping) else {}
    return _positive_float(amount.get("min")) or 0.0


def _market_min_notional(market: Mapping[str, Any]) -> float:
    limits = market.get("limits") if isinstance(market.get("limits"), Mapping) else {}
    cost = limits.get("cost") if isinstance(limits.get("cost"), Mapping) else {}
    return _positive_float(cost.get("min")) or 0.0


def _market_amount_precision(market: Mapping[str, Any]) -> int | float | None:
    precision = market.get("precision") if isinstance(market.get("precision"), Mapping) else {}
    value = precision.get("amount")
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(number) or number < 0:
        return None
    return int(number) if number.is_integer() else number


def _round_down(amount: float, precision: int | float | None) -> float:
    quantum = _quantum(precision)
    if quantum is None:
        return amount
    decimal_amount = Decimal(str(amount))
    return float(
        (decimal_amount / quantum).to_integral_value(rounding=ROUND_DOWN)
        * quantum
    )


def _quantum(precision: int | float | None) -> Decimal | None:
    if precision is None:
        return None
    if isinstance(precision, int):
        return Decimal("1").scaleb(-precision)
    quantum = Decimal(str(precision))
    return quantum if quantum > 0 else None


def _positive_float(value: object) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) and number > 0 else None


def _balance_amount(group: Mapping[str, Any], asset: str) -> float:
    return float(group.get(asset, 0.0) or 0.0)


def _order_id(order: Mapping[str, Any]) -> str | None:
    value = order.get("id") or order.get("orderId")
    return str(value) if value is not None and str(value).strip() else None


def _mode(settings: Settings) -> str:
    if settings.dry_run:
        return "DRY_RUN"
    if settings.use_testnet:
        return "testnet"
    return "LIVE"


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Plan or submit one guarded live spot allocation."
    )
    parser.add_argument("--source-asset", default="BNB")
    parser.add_argument("--destination-asset", default="USDT")
    parser.add_argument("--requested-source-amount", type=float)
    parser.add_argument("--reason", default="manual foreground live spot allocation")
    parser.add_argument("--max-order-notional-usdt", type=float, default=6.0)
    parser.add_argument("--max-daily-notional-usdt", type=float, default=12.0)
    parser.add_argument("--max-daily-orders", type=int, default=2)
    parser.add_argument("--max-spread-bps", type=float, default=50.0)
    parser.add_argument("--slippage-buffer-bps", type=float, default=25.0)
    parser.add_argument("--max-turnover-percent", type=float, default=100.0)
    parser.add_argument("--max-asset-exposure-percent", type=float, default=100.0)
    parser.add_argument("--max-total-exposure-usdt", type=float, default=0.0)
    parser.add_argument("--daily-loss-limit-usdt", type=float, default=0.0)
    parser.add_argument("--max-drawdown-percent", type=float, default=0.0)
    parser.add_argument("--min-bnb-reserve", type=float, default=0.00005)
    parser.add_argument(
        "--audit-path",
        default=os.getenv(
            "LIVE_SPOT_ALLOCATOR_AUDIT_PATH",
            "data/live-spot-allocations.jsonl",
        ),
    )
    parser.add_argument(
        "--emergency-stop-path",
        default=os.getenv("LIVE_SPOT_ALLOCATOR_STOP_PATH", "data/live-spot.stop"),
    )
    parser.add_argument(
        "--plan-only",
        action="store_true",
        help="write a proposed allocation audit record without submitting an order",
    )
    parser.add_argument("--confirm-live-allocator", action="store_true")
    parser.add_argument("--confirm-canary-reviewed", action="store_true")
    parser.add_argument("--confirm-replay-passed", action="store_true")
    parser.add_argument("--confirm-paper-warmup-reviewed", action="store_true")
    parser.add_argument("--confirm-permission-diagnostics", action="store_true")
    args = parser.parse_args(argv)
    submit_order = not args.plan_only
    if submit_order and not args.confirm_live_allocator:
        print(
            "Live spot allocator failed: live spot allocator requires "
            "--confirm-live-allocator"
        )
        return 1
    limits = LiveSpotLimits(
        max_order_notional_usdt=args.max_order_notional_usdt,
        max_daily_notional_usdt=args.max_daily_notional_usdt,
        max_daily_orders=args.max_daily_orders,
        max_spread_bps=args.max_spread_bps,
        slippage_buffer_bps=args.slippage_buffer_bps,
        max_turnover_percent=args.max_turnover_percent,
        max_asset_exposure_percent=args.max_asset_exposure_percent,
        max_total_exposure_usdt=args.max_total_exposure_usdt,
        daily_loss_limit_usdt=args.daily_loss_limit_usdt,
        max_drawdown_percent=args.max_drawdown_percent,
        min_bnb_reserve=args.min_bnb_reserve,
        audit_path=args.audit_path,
        emergency_stop_path=args.emergency_stop_path,
    )
    intent = LiveSpotIntent(
        source_asset=args.source_asset,
        destination_asset=args.destination_asset,
        requested_source_amount=args.requested_source_amount,
        reason=args.reason,
    )
    try:
        result = asyncio.run(
            run_live_spot_allocator(
                Settings.from_env(),
                submit_order=submit_order,
                confirm_live_allocator=args.confirm_live_allocator,
                confirm_canary_reviewed=args.confirm_canary_reviewed,
                confirm_replay_passed=args.confirm_replay_passed,
                confirm_paper_warmup_reviewed=args.confirm_paper_warmup_reviewed,
                confirm_permission_diagnostics=args.confirm_permission_diagnostics,
                intent=intent,
                limits=limits,
            )
        )
    except Exception as exc:
        print(f"Live spot allocator failed: {exc}")
        return 1
    print(json.dumps(result.as_dict(), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

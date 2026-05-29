"""Foreground spot live canary with strict gates and audit logging."""

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


class SpotCanaryError(RuntimeError):
    """Raised when the spot canary cannot run safely."""


@dataclass(frozen=True, slots=True)
class SpotCanaryLimits:
    """Configured hard limits for one canary run."""

    max_order_notional_usdt: float = 6.0
    max_daily_notional_usdt: float = 6.0
    max_daily_orders: int = 1
    max_spread_bps: float = 50.0
    audit_path: str = "data/spot-canary.jsonl"
    emergency_stop_path: str = "data/spot-canary.stop"


@dataclass(frozen=True, slots=True)
class SpotCanaryPlan:
    """One direct spot canary conversion plan."""

    timestamp: str
    source_asset: str
    destination_asset: str
    symbol: str
    side: str
    amount: float
    free_source_amount: float
    estimated_price: float
    estimated_notional_usdt: float
    expected_destination_amount: float
    fee_rate: float
    estimated_fee_destination: float
    min_amount: float
    min_notional_usdt: float
    spread_bps: float
    max_spread_bps: float
    max_order_notional_usdt: float
    max_daily_notional_usdt: float
    max_daily_orders: int
    daily_orders_used: int
    daily_notional_used_usdt: float
    route_quote: dict[str, Any]
    commission: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class SpotCanaryResult:
    """Auditable outcome of a spot canary plan or submission."""

    timestamp: str
    mode: str
    status: str
    plan: dict[str, Any]
    submission_attempted: bool
    order: dict[str, Any] = field(default_factory=dict)
    final_order: dict[str, Any] = field(default_factory=dict)
    balance_before: dict[str, Any] = field(default_factory=dict)
    balance_after: dict[str, Any] = field(default_factory=dict)
    error: str = ""
    audit_path: str = ""
    telegram_sent: bool = False

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def run_spot_canary(
    settings: Settings,
    *,
    submit_order: bool,
    confirm_live_canary: bool = False,
    confirm_replay_passed: bool = False,
    confirm_paper_warmup_reviewed: bool = False,
    source_asset: str = "BNB",
    destination_asset: str = "USDT",
    limits: SpotCanaryLimits | None = None,
    client: Any | None = None,
    alerter: Any | None = None,
) -> SpotCanaryResult:
    """Plan or submit one strictly gated direct spot canary order."""
    limits = limits or SpotCanaryLimits()
    _validate_limits(limits)
    _preflight(
        settings,
        submit_order=submit_order,
        confirm_live_canary=confirm_live_canary,
        confirm_replay_passed=confirm_replay_passed,
        confirm_paper_warmup_reviewed=confirm_paper_warmup_reviewed,
    )
    try:
        ensure_scope_allowed(
            build_scope_audit_report(
                settings,
                canary_symbols=(f"{source_asset}/{destination_asset}",),
            )
        )
    except ScopeAuditError as exc:
        raise SpotCanaryError(str(exc)) from exc
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
        plan, balance_before = await build_spot_canary_plan(
            client,
            source_asset=source_asset,
            destination_asset=destination_asset,
            limits=limits,
        )
        if not submit_order:
            result = SpotCanaryResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="planned",
                plan=plan.as_dict(),
                submission_attempted=False,
                balance_before=balance_before,
            )
            audit_file = _write_audit(audit_path, result)
            return SpotCanaryResult(
                **{
                    **result.as_dict(),
                    "audit_path": str(audit_file),
                }
            )

        try:
            report = await inspect_api_permissions(
                settings,
                client=client,
                spot_test_symbol=plan.symbol,
                feature_names=("spot_live_canary",),
            )
            ensure_feature_allowed(report, "spot_live_canary")
            await ensure_spot_order_test_allowed(
                client,
                symbol=plan.symbol,
                side=plan.side,
                order_type="market",
                quantity=plan.amount,
            )
        except ApiPermissionError as exc:
            result = SpotCanaryResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="blocked",
                plan=plan.as_dict(),
                submission_attempted=False,
                balance_before=balance_before,
                error=str(exc),
            )
            audit_file = _write_audit(audit_path, result)
            telegram_sent = await _send_canary_alert(alerter, result, audit_file)
            raise SpotCanaryError(str(exc)) from exc

        try:
            order = await client.create_order(
                plan.symbol,
                "market",
                plan.side,
                plan.amount,
            )
        except Exception as exc:
            result = SpotCanaryResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="submission_error",
                plan=plan.as_dict(),
                submission_attempted=True,
                balance_before=balance_before,
                error=str(exc),
            )
            audit_file = _write_audit(audit_path, result)
            await _send_canary_alert(alerter, result, audit_file)
            raise SpotCanaryError(
                "spot canary order submission failed; no retry was attempted"
            ) from exc

        final_order = await _final_order_status(client, plan.symbol, order)
        balance_after = await client.get_balance()
        result = SpotCanaryResult(
            timestamp=_utc_timestamp(),
            mode=_mode(settings),
            status="submitted",
            plan=plan.as_dict(),
            submission_attempted=True,
            order=_sanitize_order(order),
            final_order=_sanitize_order(final_order),
            balance_before=balance_before,
            balance_after=_sanitize_balance(balance_after),
        )
        audit_file = _write_audit(audit_path, result)
        telegram_sent = await _send_canary_alert(alerter, result, audit_file)
        return SpotCanaryResult(
            **{
                **result.as_dict(),
                "audit_path": str(audit_file),
                "telegram_sent": telegram_sent,
            }
        )
    finally:
        if owns_client:
            await client.close()


async def build_spot_canary_plan(
    client: Any,
    *,
    source_asset: str,
    destination_asset: str,
    limits: SpotCanaryLimits,
) -> tuple[SpotCanaryPlan, dict[str, Any]]:
    """Build a direct sell-to-USDT canary plan from current Binance data."""
    _validate_limits(limits)
    _ensure_no_emergency_stop(limits.emergency_stop_path)
    source = source_asset.strip().upper()
    destination = destination_asset.strip().upper()
    if not source:
        raise SpotCanaryError("source_asset must not be blank")
    if destination != "USDT":
        raise SpotCanaryError("spot canary v1 only supports destination_asset=USDT")

    usage = _daily_usage(Path(limits.audit_path))
    if usage["orders"] >= limits.max_daily_orders:
        raise SpotCanaryError("spot canary daily order limit is exhausted")
    remaining_daily_notional = limits.max_daily_notional_usdt - usage["notional"]
    if remaining_daily_notional <= 0:
        raise SpotCanaryError("spot canary daily notional limit is exhausted")

    balance = await client.get_balance()
    balance_snapshot = _sanitize_balance(balance)
    free_balances = balance.get("free", {}) if isinstance(balance, Mapping) else {}
    free_source = _positive_float(
        free_balances.get(source) if isinstance(free_balances, Mapping) else None
    )
    if free_source is None:
        raise SpotCanaryError(f"no free {source} balance is available")

    markets = await client.request("load_markets", request_weight=10)
    if not isinstance(markets, Mapping):
        raise SpotCanaryError("load_markets did not return a market mapping")
    symbol = f"{source}/{destination}"
    market = markets.get(symbol)
    if not _is_active_spot_market(market):
        raise SpotCanaryError(f"{symbol} is not an active direct spot market")

    ticker = await client.fetch_ticker(symbol)
    price = _sell_price(ticker)
    if price is None:
        raise SpotCanaryError(f"{symbol} ticker did not include a sell price")
    spread_bps = _spread_bps(ticker)
    if spread_bps is None:
        raise SpotCanaryError(f"{symbol} ticker did not include bid/ask spread")
    if spread_bps > limits.max_spread_bps:
        raise SpotCanaryError("spot canary spread exceeds configured limit")

    min_amount = _market_min_amount(market)
    min_notional = _market_min_notional(market)
    precision = _market_amount_precision(market)
    order_notional_cap = min(
        limits.max_order_notional_usdt,
        remaining_daily_notional,
    )
    raw_amount = min(free_source, order_notional_cap / price)
    amount = _round_down(raw_amount, precision)
    notional = amount * price
    if amount <= 0:
        raise SpotCanaryError("spot canary amount rounded to zero")
    if amount < min_amount or notional < min_notional:
        raise SpotCanaryError(
            "spot canary order would be below Binance minimum amount or notional"
        )

    commission = await CommissionService(client).get(symbol)
    fee_rate = commission.taker_rate("sell")
    fee_amount = notional * fee_rate
    expected_destination = notional - fee_amount
    route_quote = {
        "source_asset": source,
        "destination_asset": destination,
        "input_amount": amount,
        "output_amount": expected_destination,
        "slippage_bps": spread_bps,
        "legs": [
            {
                "symbol": symbol,
                "side": "sell",
                "source_asset": source,
                "destination_asset": destination,
                "input_amount": amount,
                "spent_amount": amount,
                "gross_output_amount": notional,
                "fee_rate": fee_rate,
                "fee_output_amount": fee_amount,
                "net_output_amount": expected_destination,
                "commission_discount_asset": (
                    commission.discount_asset if commission.discount_enabled else ""
                ),
            }
        ],
    }
    return (
        SpotCanaryPlan(
            timestamp=_utc_timestamp(),
            source_asset=source,
            destination_asset=destination,
            symbol=symbol,
            side="sell",
            amount=amount,
            free_source_amount=free_source,
            estimated_price=price,
            estimated_notional_usdt=notional,
            expected_destination_amount=expected_destination,
            fee_rate=fee_rate,
            estimated_fee_destination=fee_amount,
            min_amount=min_amount,
            min_notional_usdt=min_notional,
            spread_bps=spread_bps,
            max_spread_bps=limits.max_spread_bps,
            max_order_notional_usdt=limits.max_order_notional_usdt,
            max_daily_notional_usdt=limits.max_daily_notional_usdt,
            max_daily_orders=limits.max_daily_orders,
            daily_orders_used=int(usage["orders"]),
            daily_notional_used_usdt=float(usage["notional"]),
            route_quote=route_quote,
            commission=_commission_dict(commission),
        ),
        balance_snapshot,
    )


def _preflight(
    settings: Settings,
    *,
    submit_order: bool,
    confirm_live_canary: bool,
    confirm_replay_passed: bool,
    confirm_paper_warmup_reviewed: bool,
) -> None:
    if not submit_order:
        return
    if not confirm_live_canary:
        raise SpotCanaryError("spot canary requires --confirm-live-canary")
    if not confirm_replay_passed:
        raise SpotCanaryError("spot canary requires --confirm-replay-passed")
    if not confirm_paper_warmup_reviewed:
        raise SpotCanaryError(
            "spot canary requires --confirm-paper-warmup-reviewed"
        )
    if settings.use_testnet:
        raise SpotCanaryError("spot canary requires USE_TESTNET=false")
    if settings.dry_run:
        raise SpotCanaryError("spot canary requires DRY_RUN=false")
    if settings.confirm_live_trading != "yes":
        raise SpotCanaryError("spot canary requires CONFIRM_LIVE_TRADING=yes")


def _validate_limits(limits: SpotCanaryLimits) -> None:
    _positive_limit(limits.max_order_notional_usdt, "max_order_notional_usdt")
    _positive_limit(limits.max_daily_notional_usdt, "max_daily_notional_usdt")
    _positive_limit(limits.max_spread_bps, "max_spread_bps")
    if limits.max_daily_orders <= 0:
        raise SpotCanaryError("max_daily_orders must be greater than zero")
    if limits.max_daily_notional_usdt < limits.max_order_notional_usdt:
        raise SpotCanaryError(
            "max_daily_notional_usdt must be at least max_order_notional_usdt"
        )
    if not limits.audit_path.strip():
        raise SpotCanaryError("audit_path must not be blank")
    if not limits.emergency_stop_path.strip():
        raise SpotCanaryError("emergency_stop_path must not be blank")


def _positive_limit(value: float, name: str) -> None:
    if not math.isfinite(value) or value <= 0:
        raise SpotCanaryError(f"{name} must be a positive finite value")


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise SpotCanaryError(f"spot canary emergency stop is active: {path}")


def _daily_usage(path: Path) -> dict[str, float | int]:
    today = datetime.now(timezone.utc).date().isoformat()
    orders = 0
    notional = 0.0
    if not path.exists():
        return {"orders": orders, "notional": notional}
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
        if not payload.get("submission_attempted"):
            continue
        orders += 1
        plan = payload.get("plan") if isinstance(payload.get("plan"), Mapping) else {}
        notional += _positive_float(plan.get("estimated_notional_usdt")) or 0.0
    return {"orders": orders, "notional": notional}


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


async def _send_canary_alert(
    alerter: Any,
    result: SpotCanaryResult,
    audit_path: Path,
) -> bool:
    send = getattr(alerter, "send", None)
    if send is None or not callable(send):
        return False
    plan = result.plan
    return bool(
        await send(
            "<b>SPOT LIVE CANARY</b>\n"
            f"Status: <code>{result.status}</code>\n"
            f"Side: <code>{str(plan.get('side', '')).upper()}</code>\n"
            f"Symbol: <code>{plan.get('symbol', '')}</code>\n"
            f"Amount: <code>{float(plan.get('amount', 0.0)):g}</code>\n"
            f"Audit: <code>{audit_path}</code>"
        )
    )


def _write_audit(path: Path, result: SpotCanaryResult) -> Path:
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


def _commission_dict(commission: Any) -> dict[str, Any]:
    return {
        "symbol": commission.symbol,
        "taker_rate_sell": commission.taker_rate("sell"),
        "discount_enabled": commission.discount_enabled,
        "discount_asset": commission.discount_asset,
        "retrieved_at": commission.retrieved_at.isoformat(),
    }


def _is_active_spot_market(market: object) -> bool:
    return (
        isinstance(market, Mapping)
        and bool(market.get("spot", True))
        and market.get("active", True) is True
    )


def _sell_price(ticker: Mapping[str, Any]) -> float | None:
    return _positive_float(ticker.get("bid")) or _positive_float(ticker.get("last"))


def _spread_bps(ticker: Mapping[str, Any]) -> float | None:
    bid = _positive_float(ticker.get("bid"))
    ask = _positive_float(ticker.get("ask"))
    if bid is None or ask is None or ask < bid:
        return None
    return (ask - bid) / ((ask + bid) / 2) * 10_000


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
    return float((decimal_amount / quantum).to_integral_value(rounding=ROUND_DOWN) * quantum)


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
        description="Plan or submit one tightly gated spot live canary order."
    )
    parser.add_argument("--source-asset", default="BNB")
    parser.add_argument("--destination-asset", default="USDT")
    parser.add_argument("--max-order-notional-usdt", type=float, default=6.0)
    parser.add_argument("--max-daily-notional-usdt", type=float, default=6.0)
    parser.add_argument("--max-daily-orders", type=int, default=1)
    parser.add_argument("--max-spread-bps", type=float, default=50.0)
    parser.add_argument(
        "--audit-path",
        default=os.getenv("SPOT_CANARY_AUDIT_PATH", "data/spot-canary.jsonl"),
    )
    parser.add_argument(
        "--emergency-stop-path",
        default=os.getenv("SPOT_CANARY_STOP_PATH", "data/spot-canary.stop"),
    )
    parser.add_argument(
        "--plan-only",
        action="store_true",
        help="write a proposed canary audit record without submitting an order",
    )
    parser.add_argument(
        "--confirm-live-canary",
        action="store_true",
        help="required unless --plan-only is used",
    )
    parser.add_argument(
        "--confirm-replay-passed",
        action="store_true",
        help="required unless --plan-only is used",
    )
    parser.add_argument(
        "--confirm-paper-warmup-reviewed",
        action="store_true",
        help="required unless --plan-only is used",
    )
    args = parser.parse_args(argv)
    submit_order = not args.plan_only
    if submit_order and not args.confirm_live_canary:
        print("Spot canary failed: spot canary requires --confirm-live-canary")
        return 1
    limits = SpotCanaryLimits(
        max_order_notional_usdt=args.max_order_notional_usdt,
        max_daily_notional_usdt=args.max_daily_notional_usdt,
        max_daily_orders=args.max_daily_orders,
        max_spread_bps=args.max_spread_bps,
        audit_path=args.audit_path,
        emergency_stop_path=args.emergency_stop_path,
    )
    try:
        result = asyncio.run(
            run_spot_canary(
                Settings.from_env(),
                submit_order=submit_order,
                confirm_live_canary=args.confirm_live_canary,
                confirm_replay_passed=args.confirm_replay_passed,
                confirm_paper_warmup_reviewed=args.confirm_paper_warmup_reviewed,
                source_asset=args.source_asset,
                destination_asset=args.destination_asset,
                limits=limits,
            )
        )
    except Exception as exc:
        print(f"Spot canary failed: {exc}")
        return 1
    print(json.dumps(result.as_dict(), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

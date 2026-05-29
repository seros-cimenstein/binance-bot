"""Manual one-shot real-order probe with hard live-money gates."""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from decimal import Decimal, ROUND_CEILING
from pathlib import Path
from typing import Any, Mapping

from src.api_permissions import ApiPermissionError, ensure_spot_order_test_allowed
from src.config import Settings
from src.exchange.client import BinanceClient
from src.scope_audit import (
    ScopeAuditError,
    build_scope_audit_report,
    ensure_scope_allowed,
)
from src.telegram_alerts import TelegramAlerter


class LiveProbeError(RuntimeError):
    """Raised when a live probe cannot be run safely."""


@dataclass(frozen=True, slots=True)
class LiveProbeCandidate:
    """One direct-to-USDT live sell candidate."""

    symbol: str
    asset: str
    free_amount: float
    sell_amount: float
    estimated_price: float
    estimated_notional_usdt: float
    min_notional_usdt: float
    min_amount: float
    cap_usdt: float
    cap_exceeded_to_satisfy_minimum: bool = False


@dataclass(frozen=True, slots=True)
class LiveProbeResult:
    """Auditable outcome of one submitted probe order."""

    timestamp: str
    mode: str
    symbol: str
    side: str
    amount: float
    estimated_notional_usdt: float
    order_id: str | None
    order_status: str
    filled_amount: float | None
    average_price: float | None
    fee_summary: list[dict[str, Any]]
    commission: dict[str, Any] = field(default_factory=dict)
    audit_path: str = ""
    telegram_sent: bool = False

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def run_live_probe(
    settings: Settings,
    *,
    confirm_real_order: bool,
    max_notional_usdt: float = 6.0,
    audit_path: str | Path = "data/live-probes.jsonl",
    client: Any | None = None,
    alerter: Any | None = None,
) -> LiveProbeResult:
    """Submit exactly one direct-to-USDT market sell after live preflight."""
    _preflight(settings, confirm_real_order=confirm_real_order)
    if max_notional_usdt <= 0 or not math.isfinite(max_notional_usdt):
        raise LiveProbeError("max_notional_usdt must be a positive finite value")

    owns_client = client is None
    client = client or BinanceClient(
        settings.api_key,
        settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    alerter = alerter or TelegramAlerter(
        settings.telegram_bot_token,
        settings.telegram_chat_id,
    )
    try:
        balances = await client.get_balance()
        free_balances = _positive_balances(balances.get("free", {}))
        markets = await client.request("load_markets", request_weight=10)
        candidate = await select_live_sell_candidate(
            client,
            free_balances,
            markets,
            max_notional_usdt=max_notional_usdt,
        )
        try:
            ensure_scope_allowed(
                build_scope_audit_report(
                    settings,
                    canary_symbols=(candidate.symbol,),
                )
            )
        except ScopeAuditError as exc:
            raise LiveProbeError(str(exc)) from exc
        try:
            await ensure_spot_order_test_allowed(
                client,
                symbol=candidate.symbol,
                side="sell",
                order_type="market",
                quantity=candidate.sell_amount,
            )
        except ApiPermissionError as exc:
            raise LiveProbeError(str(exc)) from exc
        commission = await _read_commission(client, candidate.symbol)
        order = await client.create_order(
            candidate.symbol,
            "market",
            "sell",
            candidate.sell_amount,
        )
        final_order = await _final_order_status(client, candidate.symbol, order)
        result = LiveProbeResult(
            timestamp=datetime.now(timezone.utc).isoformat(),
            mode="LIVE",
            symbol=candidate.symbol,
            side="sell",
            amount=candidate.sell_amount,
            estimated_notional_usdt=candidate.estimated_notional_usdt,
            order_id=_order_id(final_order) or _order_id(order),
            order_status=str(final_order.get("status") or order.get("status") or "unknown"),
            filled_amount=_optional_float(final_order.get("filled") or order.get("filled")),
            average_price=_optional_float(final_order.get("average") or order.get("average")),
            fee_summary=_fee_summary(final_order) or _fee_summary(order),
            commission=_sanitize_commission(commission),
        )
        audit_file = _write_audit(audit_path, result)
        telegram_sent = await _send_live_probe_alert(alerter, result, audit_file)
        return LiveProbeResult(
            **{
                **result.as_dict(),
                "audit_path": str(audit_file),
                "telegram_sent": telegram_sent,
            }
        )
    finally:
        if owns_client:
            await client.close()


async def select_live_sell_candidate(
    client: Any,
    free_balances: Mapping[str, float],
    markets: Mapping[str, Any],
    *,
    max_notional_usdt: float,
) -> LiveProbeCandidate:
    """Choose the smallest direct USDT market sell that Binance rules allow."""
    for asset in _ordered_assets(free_balances):
        free_amount = _positive_float(free_balances.get(asset))
        if free_amount is None:
            continue
        symbol = f"{asset}/USDT"
        market = markets.get(symbol)
        if not _is_active_spot_market(market):
            continue
        ticker = await client.fetch_ticker(symbol)
        price = _sell_price(ticker)
        if price is None:
            continue
        min_amount = _market_min_amount(market)
        min_notional = _market_min_notional(market)
        precision = _market_amount_precision(market)
        required_amount = max(min_amount, min_notional / price)
        sell_amount = _round_up(required_amount, precision)
        if sell_amount > free_amount:
            full_balance_amount = _round_down(free_amount, precision)
            if _valid_order_amount(full_balance_amount, price, min_amount, min_notional):
                sell_amount = full_balance_amount
            else:
                continue
        notional = sell_amount * price
        cap_exceeded = notional > max_notional_usdt
        return LiveProbeCandidate(
            symbol=symbol,
            asset=asset,
            free_amount=free_amount,
            sell_amount=sell_amount,
            estimated_price=price,
            estimated_notional_usdt=notional,
            min_notional_usdt=min_notional,
            min_amount=min_amount,
            cap_usdt=max_notional_usdt,
            cap_exceeded_to_satisfy_minimum=cap_exceeded,
        )
    raise LiveProbeError("no free asset has a direct USDT market satisfying Binance minimums")


def _preflight(settings: Settings, *, confirm_real_order: bool) -> None:
    if not confirm_real_order:
        raise LiveProbeError("live probe requires --confirm-real-order")
    if settings.use_testnet:
        raise LiveProbeError("live probe requires USE_TESTNET=false")
    if settings.dry_run:
        raise LiveProbeError("live probe requires DRY_RUN=false")
    if settings.confirm_live_trading != "yes":
        raise LiveProbeError("live probe requires CONFIRM_LIVE_TRADING=yes")


def _ordered_assets(free_balances: Mapping[str, float]) -> list[str]:
    assets = [str(asset).upper() for asset, amount in free_balances.items() if _positive_float(amount)]
    return sorted(assets, key=lambda asset: (asset != "BNB", asset))


def _positive_balances(raw: object) -> dict[str, float]:
    if not isinstance(raw, Mapping):
        return {}
    balances: dict[str, float] = {}
    for asset, amount in raw.items():
        number = _positive_float(amount)
        if number is not None:
            balances[str(asset).upper()] = number
    return balances


def _is_active_spot_market(market: object) -> bool:
    return (
        isinstance(market, Mapping)
        and bool(market.get("spot", True))
        and market.get("active", True) is True
    )


def _sell_price(ticker: Mapping[str, Any]) -> float | None:
    return _positive_float(ticker.get("bid")) or _positive_float(ticker.get("last"))


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


def _round_up(amount: float, precision: int | float | None) -> float:
    quantum = _quantum(precision)
    if quantum is None:
        return amount
    decimal_amount = Decimal(str(amount))
    return float((decimal_amount / quantum).to_integral_value(rounding=ROUND_CEILING) * quantum)


def _round_down(amount: float, precision: int | float | None) -> float:
    quantum = _quantum(precision)
    if quantum is None:
        return amount
    decimal_amount = Decimal(str(amount))
    return float((decimal_amount // quantum) * quantum)


def _quantum(precision: int | float | None) -> Decimal | None:
    if precision is None:
        return None
    if isinstance(precision, int):
        return Decimal("1").scaleb(-precision)
    quantum = Decimal(str(precision))
    return quantum if quantum > 0 else None


def _valid_order_amount(
    amount: float,
    price: float,
    min_amount: float,
    min_notional: float,
) -> bool:
    return amount > 0 and amount >= min_amount and amount * price >= min_notional


async def _read_commission(client: Any, symbol: str) -> dict[str, Any]:
    get_commission_rates = getattr(client, "get_commission_rates", None)
    if get_commission_rates is None or not callable(get_commission_rates):
        return {}
    return await get_commission_rates(symbol)


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


def _order_id(order: Mapping[str, Any]) -> str | None:
    value = order.get("id") or order.get("orderId")
    return str(value) if value is not None and str(value).strip() else None


def _fee_summary(order: Mapping[str, Any]) -> list[dict[str, Any]]:
    fees = order.get("fees")
    if isinstance(fees, list):
        return [dict(fee) for fee in fees if isinstance(fee, Mapping)]
    fee = order.get("fee")
    if isinstance(fee, Mapping):
        return [dict(fee)]
    return []


def _sanitize_commission(payload: Mapping[str, Any]) -> dict[str, Any]:
    allowed = {"symbol", "standardCommission", "taxCommission", "specialCommission", "discount"}
    return {key: payload[key] for key in allowed if key in payload}


def _write_audit(path: str | Path, result: LiveProbeResult) -> Path:
    audit_path = Path(path)
    audit_path.parent.mkdir(parents=True, exist_ok=True)
    with audit_path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(result.as_dict(), sort_keys=True) + "\n")
    return audit_path


async def _send_live_probe_alert(
    alerter: Any,
    result: LiveProbeResult,
    audit_path: Path,
) -> bool:
    send = getattr(alerter, "send", None)
    if send is None or not callable(send):
        return False
    return bool(
        await send(
            "\u26a0\ufe0f <b>LIVE PROBE</b>\n"
            f"Side: <code>{result.side.upper()}</code>\n"
            f"Symbol: <code>{result.symbol}</code>\n"
            f"Amount: <code>{result.amount:g}</code>\n"
            f"Status: <code>{result.order_status}</code>\n"
            f"Audit: <code>{audit_path}</code>"
        )
    )


def _positive_float(value: object) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) and number > 0 else None


def _optional_float(value: object) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) else None


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Submit one tightly gated real-money live probe order."
    )
    parser.add_argument(
        "--confirm-real-order",
        action="store_true",
        help="required; confirms this command may submit one real market sell",
    )
    parser.add_argument(
        "--max-notional-usdt",
        type=float,
        default=6.0,
        help="preferred USDT cap; Binance minimums may exceed it for tiny-balance probes",
    )
    parser.add_argument(
        "--audit-path",
        default=os.getenv("LIVE_PROBE_AUDIT_PATH", "data/live-probes.jsonl"),
        help="JSONL audit path for submitted probe orders",
    )
    args = parser.parse_args(argv)
    if not args.confirm_real_order:
        print("Live probe failed: live probe requires --confirm-real-order")
        return 1
    try:
        result = asyncio.run(
            run_live_probe(
                Settings.from_env(),
                confirm_real_order=args.confirm_real_order,
                max_notional_usdt=args.max_notional_usdt,
                audit_path=args.audit_path,
            )
        )
    except Exception as exc:
        print(f"Live probe failed: {exc}")
        return 1
    print(json.dumps(result.as_dict(), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

"""Binance API permission diagnostics and feature capability registry."""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import re
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from decimal import Decimal, ROUND_CEILING, ROUND_FLOOR
from typing import Any, Mapping, Sequence

from src.config import Settings
from src.data.feeds import normalize_symbol
from src.exchange.client import BinanceClient

STATUS_OK = "ok"
STATUS_MISSING = "missing"
STATUS_UNKNOWN = "unknown"
STATUS_POLICY_BLOCKED = "policy_blocked"
STATUS_SKIPPED = "skipped"

READING = "reading"
SPOT_TRADING = "spot_trading"
MARGIN = "margin"
FUTURES = "futures"
UNIVERSAL_TRANSFER = "universal_transfer"
WITHDRAWALS = "withdrawals"
PREDICTION = "prediction"
SYMBOL_WHITELIST = "symbol_whitelist"
IP_RESTRICTION = "ip_restriction"

CAPABILITY_ORDER = (
    READING,
    SPOT_TRADING,
    MARGIN,
    FUTURES,
    UNIVERSAL_TRANSFER,
    WITHDRAWALS,
    PREDICTION,
    SYMBOL_WHITELIST,
    IP_RESTRICTION,
)

FEATURES: dict[str, tuple[tuple[str, ...], bool, str]] = {
    "operations_check": ((READING,), True, "read-only operational diagnostics"),
    "paper_pool": ((READING,), True, "paper-only pool evaluation"),
    "manual_live_probe": (
        (READING, SPOT_TRADING),
        True,
        "foreground one-shot live probe",
    ),
    "spot_live_canary": (
        (READING, SPOT_TRADING),
        True,
        "foreground tiny spot live canary",
    ),
    "spot_live_pool": (
        (READING, SPOT_TRADING),
        True,
        "foreground guarded live spot allocator",
    ),
    "claude_spot_autonomous": (
        (READING, SPOT_TRADING),
        True,
        "Claude-controlled autonomous Spot allocator",
    ),
    "margin_dry_run": (
        (READING,),
        True,
        "read-only margin discovery and local risk modeling",
    ),
    "margin_canary_plan": (
        (READING,),
        True,
        "foreground margin canary planning with live mutation blocked",
    ),
    "margin_trading": (
        (READING, MARGIN),
        False,
        "live margin activation is not implemented",
    ),
    "futures_dry_run": (
        (READING,),
        True,
        "read-only USD-M futures discovery and local risk modeling",
    ),
    "futures_canary_plan": (
        (READING,),
        True,
        "foreground USD-M futures canary planning with live mutation blocked",
    ),
    "futures_trading": (
        (READING, FUTURES),
        False,
        "live futures activation is not implemented",
    ),
    "transfer_dry_run": (
        (READING,),
        True,
        "read-only wallet discovery and local transfer planning",
    ),
    "transfer_canary_plan": (
        (READING,),
        True,
        "foreground transfer canary planning with live mutation blocked",
    ),
    "unified_pool_dry_run": (
        (READING,),
        True,
        "read-only unified account-type valuation and constraint modeling",
    ),
    "symbol_scope_audit": (
        (READING,),
        True,
        "local symbol and asset whitelist diagnostics",
    ),
    "withdrawal_dry_run": (
        (READING,),
        True,
        "local withdrawal safety planning with live withdrawal blocked",
    ),
    "prediction_dry_run": (
        (READING,),
        True,
        "local prediction risk modeling with live prediction trading blocked",
    ),
    "universal_transfer": (
        (READING, UNIVERSAL_TRANSFER),
        False,
        "live transfer activation is not implemented",
    ),
    "withdrawals": ((READING, WITHDRAWALS), False, "hard-blocked by policy"),
    "prediction_trading": (
        (READING, PREDICTION),
        False,
        "phase 23 roadmap item",
    ),
    "future_product_module": (
        (READING,),
        False,
        "phase 23 placeholder for future Binance product modules",
    ),
}

_ERROR_CODE_RE = re.compile(r"""["']code["']\s*:\s*["']?(-?\d+)""")
_HTTP_STATUS_RE = re.compile(r"\b([45]\d{2})\b")


class ApiPermissionError(RuntimeError):
    """Raised when a feature is not allowed by permission diagnostics."""


@dataclass(frozen=True, slots=True)
class CapabilityCheck:
    """One Binance API capability diagnostic result."""

    capability: str
    status: str
    source: str
    detail: str
    error_code: str | int | None = None
    http_status: int | None = None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FeatureCheck:
    """Whether one bot feature is allowed by current capability diagnostics."""

    feature: str
    allowed: bool
    implemented: bool
    required_capabilities: tuple[str, ...]
    missing_capabilities: tuple[str, ...]
    unknown_capabilities: tuple[str, ...]
    policy_blocked_capabilities: tuple[str, ...]
    detail: str

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class SpotOrderTest:
    """Safe spot order-test payload for Binance's non-matching endpoint."""

    symbol: str
    side: str
    order_type: str
    quantity: float
    price: float | None = None

    def as_binance_params(self) -> dict[str, str]:
        params = {
            "symbol": _exchange_symbol(self.symbol),
            "side": self.side.upper(),
            "type": self.order_type.upper(),
            "quantity": _format_decimal(self.quantity),
        }
        if self.order_type.lower() == "limit":
            if self.price is None:
                raise ApiPermissionError("limit order test requires a price")
            params["timeInForce"] = "GTC"
            params["price"] = _format_decimal(self.price)
        return params


@dataclass(frozen=True, slots=True)
class PermissionReport:
    """Full permission diagnostic report."""

    timestamp: str
    mode: str
    account_type: str | None
    account_permissions: tuple[str, ...]
    can_trade: bool | None
    can_withdraw: bool | None
    spot_test_symbol: str | None
    capabilities: tuple[CapabilityCheck, ...]
    features: tuple[FeatureCheck, ...]

    def capability(self, name: str) -> CapabilityCheck:
        normalized = _normalize_capability_name(name)
        for capability in self.capabilities:
            if capability.capability == normalized:
                return capability
        raise KeyError(normalized)

    def feature(self, name: str) -> FeatureCheck:
        normalized = _normalize_feature_name(name)
        for feature in self.features:
            if feature.feature == normalized:
                return feature
        raise KeyError(normalized)

    def as_dict(self) -> dict[str, Any]:
        return {
            "timestamp": self.timestamp,
            "mode": self.mode,
            "account_type": self.account_type,
            "account_permissions": list(self.account_permissions),
            "can_trade": self.can_trade,
            "can_withdraw": self.can_withdraw,
            "spot_test_symbol": self.spot_test_symbol,
            "capabilities": [capability.as_dict() for capability in self.capabilities],
            "features": [feature.as_dict() for feature in self.features],
        }


async def inspect_api_permissions(
    settings: Settings,
    *,
    client: Any | None = None,
    spot_test_symbol: str | None = None,
    include_spot_order_test: bool = True,
    feature_names: Sequence[str] | None = None,
) -> PermissionReport:
    """Inspect API capabilities without submitting real orders."""
    owns_client = client is None
    client = client or BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    capabilities: dict[str, CapabilityCheck] = {}
    account: Mapping[str, Any] = {}
    normalized_symbol = normalize_symbol(spot_test_symbol or settings.trading_symbol)

    try:
        account_check, account = await _probe_account_info(client)
        capabilities[READING] = account_check

        if include_spot_order_test and account_check.status == STATUS_OK:
            order_test = await build_dynamic_spot_order_test(client, normalized_symbol)
            capabilities[SPOT_TRADING] = await probe_spot_order_test(
                client,
                order_test,
            )
        elif not include_spot_order_test:
            capabilities[SPOT_TRADING] = CapabilityCheck(
                SPOT_TRADING,
                STATUS_SKIPPED,
                "operator",
                "spot order-test probe was skipped",
            )
        else:
            capabilities[SPOT_TRADING] = CapabilityCheck(
                SPOT_TRADING,
                STATUS_UNKNOWN,
                "account",
                "spot trading was not checked because account reading failed",
            )

        capabilities.update(_policy_capabilities(capabilities))
        features = tuple(
            evaluate_feature(capabilities, feature_name)
            for feature_name in _selected_features(feature_names)
        )
        return PermissionReport(
            timestamp=datetime.now(timezone.utc).isoformat(),
            mode=_mode(settings),
            account_type=_optional_text(account.get("accountType")),
            account_permissions=_account_permissions(account),
            can_trade=_optional_bool(account.get("canTrade")),
            can_withdraw=_optional_bool(account.get("canWithdraw")),
            spot_test_symbol=normalized_symbol if include_spot_order_test else None,
            capabilities=tuple(capabilities[name] for name in CAPABILITY_ORDER),
            features=features,
        )
    finally:
        if owns_client:
            await client.close()


async def build_dynamic_spot_order_test(
    client: Any,
    symbol: str,
) -> SpotOrderTest:
    """Build a valid-looking order-test payload from current market metadata."""
    normalized_symbol = normalize_symbol(symbol)
    markets = await client.request("load_markets", request_weight=10)
    if not isinstance(markets, Mapping):
        raise ApiPermissionError("load_markets did not return a market mapping")
    market = markets.get(normalized_symbol)
    if not _is_active_spot_market(market):
        raise ApiPermissionError(
            f"{normalized_symbol} is not an active spot market for order testing"
        )

    ticker = await client.fetch_ticker(normalized_symbol)
    price = _ticker_price(ticker)
    if price is None:
        raise ApiPermissionError(
            f"{normalized_symbol} ticker did not include a usable price"
        )

    price = _round_down(price, _market_precision(market, "price")) or price
    min_amount = _market_min_amount(market)
    min_notional = _market_min_notional(market) or 5.0
    required_quantity = max(min_amount, (min_notional * 1.02) / price)
    quantity = _round_up(required_quantity, _market_precision(market, "amount"))
    if quantity <= 0:
        raise ApiPermissionError(
            f"{normalized_symbol} market metadata produced a zero test quantity"
        )
    return SpotOrderTest(
        symbol=normalized_symbol,
        side="BUY",
        order_type="LIMIT",
        quantity=quantity,
        price=price,
    )


async def probe_spot_order_test(
    client: Any,
    order_test: SpotOrderTest,
) -> CapabilityCheck:
    """Run Binance's safe spot order-test endpoint."""
    try:
        await client.request(
            "privatePostOrderTest",
            order_test.as_binance_params(),
            request_weight=1,
        )
    except Exception as exc:
        return _capability_from_error(
            SPOT_TRADING,
            "POST /api/v3/order/test",
            exc,
            permission_detail=(
                "Binance rejected the safe order-test endpoint. Enable "
                "'Spot & Margin Trading' for this API key and verify the "
                "current server IP is allowed in API Management."
            ),
            unknown_detail=(
                "Binance reached the safe order-test endpoint but rejected the "
                "test payload; inspect market rules, balances, and the raw "
                "exchange error before enabling live orders."
            ),
        )
    return CapabilityCheck(
        SPOT_TRADING,
        STATUS_OK,
        "POST /api/v3/order/test",
        f"safe spot order-test succeeded for {order_test.symbol}",
    )


async def ensure_spot_order_test_allowed(
    client: Any,
    *,
    symbol: str,
    side: str,
    order_type: str,
    quantity: float,
    price: float | None = None,
) -> None:
    """Fail closed unless Binance accepts a safe order-test payload."""
    check = await probe_spot_order_test(
        client,
        SpotOrderTest(
            symbol=normalize_symbol(symbol),
            side=side,
            order_type=order_type,
            quantity=quantity,
            price=price,
        ),
    )
    if check.status != STATUS_OK:
        raise ApiPermissionError(_capability_failure_message(check))


def evaluate_feature(
    capabilities: Mapping[str, CapabilityCheck],
    feature_name: str,
) -> FeatureCheck:
    """Evaluate whether a bot feature may run under current diagnostics."""
    normalized = _normalize_feature_name(feature_name)
    required, implemented, feature_detail = FEATURES[normalized]
    missing: list[str] = []
    unknown: list[str] = []
    policy_blocked: list[str] = []
    for capability_name in required:
        capability = capabilities.get(capability_name)
        status = capability.status if capability is not None else STATUS_UNKNOWN
        if status == STATUS_MISSING:
            missing.append(capability_name)
        elif status == STATUS_POLICY_BLOCKED:
            policy_blocked.append(capability_name)
        elif status != STATUS_OK:
            unknown.append(capability_name)

    allowed = implemented and not missing and not unknown and not policy_blocked
    if not implemented:
        detail = f"{feature_detail}; feature is not implemented yet"
    elif allowed:
        detail = "all required capabilities passed"
    else:
        detail = "feature fails closed until required capabilities pass"

    return FeatureCheck(
        feature=normalized,
        allowed=allowed,
        implemented=implemented,
        required_capabilities=required,
        missing_capabilities=tuple(missing),
        unknown_capabilities=tuple(unknown),
        policy_blocked_capabilities=tuple(policy_blocked),
        detail=detail,
    )


def ensure_feature_allowed(report: PermissionReport, feature_name: str) -> FeatureCheck:
    """Raise if a feature is not allowed by the report."""
    feature = report.feature(feature_name)
    if not feature.allowed:
        raise ApiPermissionError(_feature_failure_message(feature, report))
    return feature


def render_report(report: PermissionReport) -> str:
    """Render a concise human-readable report."""
    lines = [
        f"Mode: {report.mode}",
        f"Account type: {report.account_type or 'unknown'}",
        "Account permissions: "
        + (", ".join(report.account_permissions) if report.account_permissions else "unknown"),
        f"canTrade: {_render_optional(report.can_trade)}",
        f"canWithdraw: {_render_optional(report.can_withdraw)}",
        "Capabilities:",
    ]
    for capability in report.capabilities:
        suffix = ""
        if capability.error_code is not None:
            suffix += f" error_code={capability.error_code}"
        if capability.http_status is not None:
            suffix += f" http_status={capability.http_status}"
        lines.append(
            f"  - {capability.capability}: {capability.status} "
            f"({capability.source}) - {capability.detail}{suffix}"
        )

    lines.append("Features:")
    for feature in report.features:
        verdict = "allowed" if feature.allowed else "blocked"
        lines.append(f"  - {feature.feature}: {verdict} - {feature.detail}")
        if feature.missing_capabilities:
            lines.append(
                "    missing: " + ", ".join(feature.missing_capabilities)
            )
        if feature.unknown_capabilities:
            lines.append(
                "    unknown: " + ", ".join(feature.unknown_capabilities)
            )
        if feature.policy_blocked_capabilities:
            lines.append(
                "    policy_blocked: "
                + ", ".join(feature.policy_blocked_capabilities)
            )
    return "\n".join(lines)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Inspect Binance API permissions without placing orders."
    )
    parser.add_argument(
        "--feature",
        action="append",
        choices=sorted(FEATURES),
        help="feature to evaluate; may be passed more than once",
    )
    parser.add_argument(
        "--spot-test-symbol",
        help="spot symbol for the safe /api/v3/order/test probe",
    )
    parser.add_argument(
        "--skip-spot-order-test",
        action="store_true",
        help="skip the safe TRADE endpoint probe and report spot trading as skipped",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print the diagnostic report as JSON",
    )
    args = parser.parse_args(argv)
    try:
        report = asyncio.run(
            inspect_api_permissions(
                Settings.from_env(),
                spot_test_symbol=args.spot_test_symbol,
                include_spot_order_test=not args.skip_spot_order_test,
                feature_names=args.feature,
            )
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_report(report))
        if args.feature:
            for feature_name in args.feature:
                ensure_feature_allowed(report, feature_name)
    except Exception as exc:
        print(f"API permission check failed: {exc}")
        return 1
    return 0


async def _probe_account_info(
    client: Any,
) -> tuple[CapabilityCheck, Mapping[str, Any]]:
    try:
        account = await client.request("privateGetAccount", request_weight=20)
    except Exception as exc:
        return (
            _capability_from_error(
                READING,
                "GET /api/v3/account",
                exc,
                permission_detail=(
                    "Binance rejected account information access. Check the API "
                    "key/secret, Enable Reading, and any IP restriction."
                ),
                unknown_detail=(
                    "Binance account information request failed before "
                    "permissions could be confirmed."
                ),
            ),
            {},
        )
    if not isinstance(account, Mapping):
        return (
            CapabilityCheck(
                READING,
                STATUS_UNKNOWN,
                "GET /api/v3/account",
                "account information response was not a mapping",
            ),
            {},
        )
    return (
        CapabilityCheck(
            READING,
            STATUS_OK,
            "GET /api/v3/account",
            "account information request succeeded",
        ),
        account,
    )


def _policy_capabilities(
    capabilities: Mapping[str, CapabilityCheck],
) -> dict[str, CapabilityCheck]:
    permission_error_seen = any(
        _is_permission_error_code(capability.error_code)
        for capability in capabilities.values()
    )
    ip_detail = (
        "Binance does not expose the API-key IP whitelist in this safe "
        "diagnostic. Because a -2015-style error was observed, verify the "
        "current server IP in API Management."
        if permission_error_seen
        else "Binance does not expose the API-key IP whitelist in this safe diagnostic."
    )
    return {
        MARGIN: CapabilityCheck(
            MARGIN,
            STATUS_POLICY_BLOCKED,
            "policy",
            "live margin trading remains disabled; Phase 19 only permits dry-run discovery and local modeling",
        ),
        FUTURES: CapabilityCheck(
            FUTURES,
            STATUS_POLICY_BLOCKED,
            "policy",
            "live futures trading remains disabled; Phase 20 only permits dry-run discovery and local modeling",
        ),
        UNIVERSAL_TRANSFER: CapabilityCheck(
            UNIVERSAL_TRANSFER,
            STATUS_POLICY_BLOCKED,
            "policy",
            "live transfer automation remains disabled; Phase 21 only permits dry-run planning and local audit",
        ),
        WITHDRAWALS: CapabilityCheck(
            WITHDRAWALS,
            STATUS_POLICY_BLOCKED,
            "policy",
            "withdrawals are hard-blocked unless a future withdrawal-safety phase approves them",
        ),
        PREDICTION: CapabilityCheck(
            PREDICTION,
            STATUS_POLICY_BLOCKED,
            "policy",
            "future phase 23 module; prediction trading is not implemented",
        ),
        SYMBOL_WHITELIST: CapabilityCheck(
            SYMBOL_WHITELIST,
            STATUS_UNKNOWN,
            "api-management",
            "Binance does not expose the full symbol whitelist in this safe diagnostic",
        ),
        IP_RESTRICTION: CapabilityCheck(
            IP_RESTRICTION,
            STATUS_UNKNOWN,
            "api-management",
            ip_detail,
        ),
    }


def _capability_from_error(
    capability: str,
    source: str,
    exc: Exception,
    *,
    permission_detail: str,
    unknown_detail: str,
) -> CapabilityCheck:
    error_code = _extract_error_code(exc)
    http_status = _extract_http_status(exc)
    if _is_permission_or_ip_error(exc):
        return CapabilityCheck(
            capability,
            STATUS_MISSING,
            source,
            permission_detail,
            error_code=error_code,
            http_status=http_status,
        )
    return CapabilityCheck(
        capability,
        STATUS_UNKNOWN,
        source,
        f"{unknown_detail} Exchange said: {exc}",
        error_code=error_code,
        http_status=http_status,
    )


def _capability_failure_message(check: CapabilityCheck) -> str:
    suffix = ""
    if check.error_code is not None:
        suffix += f" error_code={check.error_code}"
    if check.http_status is not None:
        suffix += f" http_status={check.http_status}"
    return (
        f"{check.capability} permission check failed: {check.status}; "
        f"{check.detail}{suffix}"
    )


def _feature_failure_message(
    feature: FeatureCheck,
    report: PermissionReport,
) -> str:
    parts = [f"{feature.feature} is not allowed: {feature.detail}"]
    for capability_name in (
        feature.missing_capabilities
        + feature.unknown_capabilities
        + feature.policy_blocked_capabilities
    ):
        capability = report.capability(capability_name)
        parts.append(
            f"{capability.capability}={capability.status} ({capability.detail})"
        )
    return "; ".join(parts)


def _selected_features(feature_names: Sequence[str] | None) -> tuple[str, ...]:
    if not feature_names:
        return tuple(FEATURES)
    return tuple(_normalize_feature_name(feature) for feature in feature_names)


def _normalize_feature_name(name: str) -> str:
    normalized = name.strip().lower().replace("-", "_")
    if normalized not in FEATURES:
        raise ApiPermissionError(f"unknown feature: {name}")
    return normalized


def _normalize_capability_name(name: str) -> str:
    normalized = name.strip().lower().replace("-", "_")
    if normalized not in CAPABILITY_ORDER:
        raise ApiPermissionError(f"unknown capability: {name}")
    return normalized


def _mode(settings: Settings) -> str:
    if settings.dry_run:
        return "DRY_RUN"
    if settings.use_testnet:
        return "testnet"
    return "LIVE"


def _account_permissions(account: Mapping[str, Any]) -> tuple[str, ...]:
    permissions = account.get("permissions")
    if not isinstance(permissions, Sequence) or isinstance(permissions, (str, bytes)):
        return ()
    return tuple(str(permission).upper() for permission in permissions)


def _is_active_spot_market(market: object) -> bool:
    return (
        isinstance(market, Mapping)
        and bool(market.get("spot", True))
        and market.get("active", True) is True
    )


def _ticker_price(ticker: Mapping[str, Any]) -> float | None:
    for key in ("last", "close", "ask", "bid"):
        value = _positive_float(ticker.get(key))
        if value is not None:
            return value
    return None


def _market_min_amount(market: Mapping[str, Any]) -> float:
    limits = market.get("limits") if isinstance(market.get("limits"), Mapping) else {}
    amount = limits.get("amount") if isinstance(limits.get("amount"), Mapping) else {}
    return _positive_float(amount.get("min")) or 0.0


def _market_min_notional(market: Mapping[str, Any]) -> float:
    limits = market.get("limits") if isinstance(market.get("limits"), Mapping) else {}
    cost = limits.get("cost") if isinstance(limits.get("cost"), Mapping) else {}
    return _positive_float(cost.get("min")) or 0.0


def _market_precision(market: Mapping[str, Any], key: str) -> int | float | None:
    precision = market.get("precision") if isinstance(market.get("precision"), Mapping) else {}
    value = precision.get(key)
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(number) or number < 0:
        return None
    return int(number) if number.is_integer() else number


def _round_up(amount: float, precision: int | float | None) -> float:
    return _round(amount, precision, ROUND_CEILING)


def _round_down(amount: float, precision: int | float | None) -> float:
    return _round(amount, precision, ROUND_FLOOR)


def _round(amount: float, precision: int | float | None, rounding: str) -> float:
    quantum = _quantum(precision)
    if quantum is None:
        return amount
    decimal_amount = Decimal(str(amount))
    return float((decimal_amount / quantum).to_integral_value(rounding=rounding) * quantum)


def _quantum(precision: int | float | None) -> Decimal | None:
    if precision is None:
        return None
    if isinstance(precision, int):
        return Decimal("1").scaleb(-precision)
    quantum = Decimal(str(precision))
    return quantum if quantum > 0 else None


def _format_decimal(value: float) -> str:
    if not math.isfinite(value) or value <= 0:
        raise ApiPermissionError("order-test numeric values must be positive")
    text = format(Decimal(str(value)).normalize(), "f")
    return text.rstrip("0").rstrip(".") if "." in text else text


def _exchange_symbol(symbol: str) -> str:
    return re.sub(r"[^A-Za-z0-9]", "", normalize_symbol(symbol)).upper()


def _positive_float(value: object) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) and number > 0 else None


def _optional_bool(value: object) -> bool | None:
    return value if isinstance(value, bool) else None


def _optional_text(value: object) -> str | None:
    if value is None:
        return None
    text = str(value).strip()
    return text or None


def _extract_error_code(exc: Exception) -> str | int | None:
    code = getattr(exc, "error_code", None)
    if code is None:
        code = getattr(exc, "code", None)
    if code is not None:
        return code
    match = _ERROR_CODE_RE.search(str(exc))
    return match.group(1) if match else None


def _extract_http_status(exc: Exception) -> int | None:
    for attr_name in ("http_status", "http_status_code", "status_code"):
        status = getattr(exc, attr_name, None)
        if isinstance(status, int):
            return status
    match = _HTTP_STATUS_RE.search(str(exc))
    return int(match.group(1)) if match else None


def _is_permission_or_ip_error(exc: Exception) -> bool:
    if _is_permission_error_code(_extract_error_code(exc)):
        return True
    return "invalid api-key, ip, or permissions" in str(exc).lower()


def _is_permission_error_code(error_code: str | int | None) -> bool:
    return str(error_code) == "-2015"


def _render_optional(value: bool | None) -> str:
    if value is None:
        return "unknown"
    return "true" if value else "false"


if __name__ == "__main__":
    raise SystemExit(run())

"""Local API symbol/asset whitelist diagnostics.

Binance does not expose the full API-key symbol whitelist through the safe
permission diagnostic. This module uses operator-owned local configuration to
fail closed before live-capable planning touches a symbol or asset outside the
declared scope.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import asdict, dataclass
from typing import Any, Sequence

from src.config import Settings
from src.data.feeds import normalize_symbol

ITEM_SYMBOL = "symbol"
ITEM_ASSET = "asset"
SOURCE_TRADING_SYMBOL = "trading_symbol"
SOURCE_POOL_ROUTE = "pool_route"
SOURCE_CANARY = "canary"
SOURCE_MARGIN = "margin"
SOURCE_FUTURES = "futures"
SOURCE_TRANSFER = "transfer"
SOURCE_WITHDRAWAL = "withdrawal"
SOURCE_PREDICTION = "prediction"


class ScopeAuditError(RuntimeError):
    """Raised when local scope diagnostics cannot be evaluated."""


@dataclass(frozen=True, slots=True)
class ScopeItem:
    """One symbol or asset requiring local API scope approval."""

    kind: str
    value: str
    source: str
    feature: str
    live_relevant: bool = True

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ScopeCheck:
    """Evaluation result for one scope item."""

    item: ScopeItem
    allowed: bool
    reason: str

    def as_dict(self) -> dict[str, Any]:
        return {
            "item": self.item.as_dict(),
            "allowed": self.allowed,
            "reason": self.reason,
        }


@dataclass(frozen=True, slots=True)
class ScopeAuditReport:
    """Full local API scope report."""

    configured_symbols: tuple[str, ...]
    configured_assets: tuple[str, ...]
    checks: tuple[ScopeCheck, ...]
    binance_symbol_whitelist_visible: bool = False

    @property
    def allowed(self) -> bool:
        return all(check.allowed for check in self.checks)

    @property
    def live_ready(self) -> bool:
        return self.allowed and bool(self.configured_symbols)

    @property
    def blocked_checks(self) -> tuple[ScopeCheck, ...]:
        return tuple(check for check in self.checks if not check.allowed)

    def as_dict(self) -> dict[str, Any]:
        return {
            "configured_symbols": list(self.configured_symbols),
            "configured_assets": list(self.configured_assets),
            "binance_symbol_whitelist_visible": self.binance_symbol_whitelist_visible,
            "allowed": self.allowed,
            "live_ready": self.live_ready,
            "checks": [check.as_dict() for check in self.checks],
        }


def build_scope_items(
    settings: Settings,
    *,
    pool_route_symbols: Sequence[str] = (),
    canary_symbols: Sequence[str] = (),
    margin_symbols: Sequence[str] = (),
    futures_symbols: Sequence[str] = (),
    transfer_assets: Sequence[str] = (),
    withdrawal_assets: Sequence[str] = (),
    prediction_markets: Sequence[str] = (),
) -> tuple[ScopeItem, ...]:
    """Build scope items from runtime settings and optional module inputs."""
    items: list[ScopeItem] = []
    for symbol in settings.trading_symbols:
        items.append(
            ScopeItem(
                kind=ITEM_SYMBOL,
                value=normalize_symbol(symbol),
                source=SOURCE_TRADING_SYMBOL,
                feature="spot_trading",
            )
        )
    items.extend(_symbol_items(pool_route_symbols, SOURCE_POOL_ROUTE, "paper_pool"))
    items.extend(_symbol_items(canary_symbols, SOURCE_CANARY, "spot_live_canary"))
    items.extend(_symbol_items(margin_symbols, SOURCE_MARGIN, "margin_trading"))
    items.extend(_symbol_items(futures_symbols, SOURCE_FUTURES, "futures_trading"))
    items.extend(_asset_items(transfer_assets, SOURCE_TRANSFER, "universal_transfer"))
    items.extend(_asset_items(withdrawal_assets, SOURCE_WITHDRAWAL, "withdrawals"))
    items.extend(_symbol_items(prediction_markets, SOURCE_PREDICTION, "prediction_trading"))
    return _dedupe_items(items)


def build_scope_audit_report(
    settings: Settings,
    *,
    items: Sequence[ScopeItem] | None = None,
    pool_route_symbols: Sequence[str] = (),
    canary_symbols: Sequence[str] = (),
    margin_symbols: Sequence[str] = (),
    futures_symbols: Sequence[str] = (),
    transfer_assets: Sequence[str] = (),
    withdrawal_assets: Sequence[str] = (),
    prediction_markets: Sequence[str] = (),
) -> ScopeAuditReport:
    """Evaluate local symbol/asset scope without contacting Binance."""
    scope_items = tuple(items) if items is not None else build_scope_items(
        settings,
        pool_route_symbols=pool_route_symbols,
        canary_symbols=canary_symbols,
        margin_symbols=margin_symbols,
        futures_symbols=futures_symbols,
        transfer_assets=transfer_assets,
        withdrawal_assets=withdrawal_assets,
        prediction_markets=prediction_markets,
    )
    checks = tuple(_evaluate_item(settings, item) for item in scope_items)
    return ScopeAuditReport(
        configured_symbols=settings.api_symbol_whitelist,
        configured_assets=settings.api_asset_whitelist,
        checks=checks,
    )


def ensure_scope_allowed(report: ScopeAuditReport) -> ScopeAuditReport:
    """Fail closed unless every requested symbol and asset is locally allowed."""
    if report.blocked_checks:
        details = "; ".join(
            f"{check.item.kind} {check.item.value} from {check.item.source}: {check.reason}"
            for check in report.blocked_checks
        )
        raise ScopeAuditError(f"API scope audit failed: {details}")
    return report


def render_scope_report(report: ScopeAuditReport) -> str:
    """Render a concise local scope report."""
    lines = [
        "API scope audit: local/operator whitelist",
        "Binance symbol whitelist visible: no",
        "Configured symbols: "
        + (", ".join(report.configured_symbols) if report.configured_symbols else "none"),
        "Configured assets: "
        + (", ".join(report.configured_assets) if report.configured_assets else "none"),
        f"Live scope ready: {'yes' if report.live_ready else 'no'}",
        "Checks:",
    ]
    if not report.checks:
        lines.append("  - none")
    for check in report.checks:
        verdict = "allowed" if check.allowed else "blocked"
        lines.append(
            f"  - {check.item.kind} {check.item.value} "
            f"({check.item.source}/{check.item.feature}): {verdict} - {check.reason}"
        )
    return "\n".join(lines)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Audit local API symbol/asset whitelist scope."
    )
    parser.add_argument("--symbol", action="append", help="symbol to check")
    parser.add_argument("--asset", action="append", help="asset to check")
    parser.add_argument(
        "--fail-on-blocked",
        action="store_true",
        help="return a non-zero exit code if any requested scope is blocked",
    )
    parser.add_argument("--json", action="store_true", help="print JSON output")
    args = parser.parse_args(argv)
    try:
        settings = Settings.from_env()
        items = list(build_scope_items(settings))
        items.extend(_symbol_items(args.symbol or (), "cli", "manual"))
        items.extend(_asset_items(args.asset or (), "cli", "manual"))
        report = build_scope_audit_report(settings, items=_dedupe_items(items))
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_scope_report(report))
        if args.fail_on_blocked:
            ensure_scope_allowed(report)
    except Exception as exc:
        print(f"API scope audit failed: {exc}")
        return 1
    return 0


def _evaluate_item(settings: Settings, item: ScopeItem) -> ScopeCheck:
    if item.kind == ITEM_SYMBOL:
        if not settings.api_symbol_whitelist:
            return ScopeCheck(item, False, "no local API_SYMBOL_WHITELIST configured")
        if item.value in settings.api_symbol_whitelist:
            return ScopeCheck(item, True, "symbol is in local whitelist")
        return ScopeCheck(item, False, "symbol is not in local whitelist")
    if item.kind == ITEM_ASSET:
        if not settings.api_asset_whitelist:
            return ScopeCheck(item, False, "no local API_ASSET_WHITELIST configured")
        if item.value in settings.api_asset_whitelist:
            return ScopeCheck(item, True, "asset is in local whitelist")
        return ScopeCheck(item, False, "asset is not in local whitelist")
    raise ScopeAuditError(f"unknown scope item kind: {item.kind}")


def _symbol_items(
    symbols: Sequence[str],
    source: str,
    feature: str,
) -> tuple[ScopeItem, ...]:
    return tuple(
        ScopeItem(
            kind=ITEM_SYMBOL,
            value=normalize_symbol(symbol),
            source=source,
            feature=feature,
        )
        for symbol in symbols
        if str(symbol).strip()
    )


def _asset_items(
    assets: Sequence[str],
    source: str,
    feature: str,
) -> tuple[ScopeItem, ...]:
    return tuple(
        ScopeItem(
            kind=ITEM_ASSET,
            value=_asset_symbol(asset),
            source=source,
            feature=feature,
        )
        for asset in assets
        if str(asset).strip()
    )


def _dedupe_items(items: Sequence[ScopeItem]) -> tuple[ScopeItem, ...]:
    seen: set[tuple[str, str, str, str]] = set()
    deduped: list[ScopeItem] = []
    for item in items:
        key = (item.kind, item.value, item.source, item.feature)
        if key in seen:
            continue
        seen.add(key)
        deduped.append(item)
    return tuple(deduped)


def _asset_symbol(value: object) -> str:
    asset = str(value).strip().upper()
    if not asset or not asset.replace("-", "").replace("_", "").isalnum():
        raise ScopeAuditError(f"invalid asset symbol: {value}")
    return asset


if __name__ == "__main__":
    raise SystemExit(run())

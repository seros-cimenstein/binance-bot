"""Dry-run capital transfer planning across Binance account wallets.

Phase 21 is planning only. This module may read wallet, spot, funding, margin,
and USD-M futures balances, then model internal transfer effects. It has no
live universal-transfer submission path.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import re
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

from src.config import Settings
from src.data.feeds import normalize_symbol
from src.exchange.client import BinanceClient

WALLET_SPOT = "spot"
WALLET_FUNDING = "funding"
WALLET_CROSS_MARGIN = "cross_margin"
WALLET_ISOLATED_MARGIN = "isolated_margin"
WALLET_USD_M_FUTURES = "usd_m_futures"

TRANSFER_ROUTES: dict[tuple[str, str], str] = {
    (WALLET_SPOT, WALLET_USD_M_FUTURES): "MAIN_UMFUTURE",
    (WALLET_USD_M_FUTURES, WALLET_SPOT): "UMFUTURE_MAIN",
    (WALLET_SPOT, WALLET_CROSS_MARGIN): "MAIN_MARGIN",
    (WALLET_CROSS_MARGIN, WALLET_SPOT): "MARGIN_MAIN",
    (WALLET_CROSS_MARGIN, WALLET_USD_M_FUTURES): "MARGIN_UMFUTURE",
    (WALLET_USD_M_FUTURES, WALLET_CROSS_MARGIN): "UMFUTURE_MARGIN",
    (WALLET_SPOT, WALLET_FUNDING): "MAIN_FUNDING",
    (WALLET_FUNDING, WALLET_SPOT): "FUNDING_MAIN",
    (WALLET_FUNDING, WALLET_USD_M_FUTURES): "FUNDING_UMFUTURE",
    (WALLET_USD_M_FUTURES, WALLET_FUNDING): "UMFUTURE_FUNDING",
    (WALLET_CROSS_MARGIN, WALLET_FUNDING): "MARGIN_FUNDING",
    (WALLET_FUNDING, WALLET_CROSS_MARGIN): "FUNDING_MARGIN",
    (WALLET_ISOLATED_MARGIN, WALLET_CROSS_MARGIN): "ISOLATEDMARGIN_MARGIN",
    (WALLET_CROSS_MARGIN, WALLET_ISOLATED_MARGIN): "MARGIN_ISOLATEDMARGIN",
    (WALLET_ISOLATED_MARGIN, WALLET_ISOLATED_MARGIN): (
        "ISOLATEDMARGIN_ISOLATEDMARGIN"
    ),
}
WRITE_ENDPOINTS_BLOCKED = ("sapiPostAssetTransfer",)
ALL_WALLETS = (
    WALLET_SPOT,
    WALLET_FUNDING,
    WALLET_CROSS_MARGIN,
    WALLET_ISOLATED_MARGIN,
    WALLET_USD_M_FUTURES,
)


class TransferPlanError(RuntimeError):
    """Raised when dry-run transfer planning cannot proceed safely."""


@dataclass(frozen=True, slots=True)
class WalletActivation:
    """Wallet summary row from Binance's wallet-balance endpoint."""

    wallet: str
    wallet_name: str
    active: bool | None
    quote_balance: float

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class WalletAssetBalance:
    """Asset balance for one wallet used by transfer planning."""

    wallet: str
    asset: str
    free: float = 0.0
    locked: float = 0.0
    borrowed: float = 0.0
    interest: float = 0.0
    available: float = 0.0
    wallet_balance: float = 0.0
    btc_valuation: float = 0.0

    @property
    def liability(self) -> float:
        return self.borrowed + self.interest

    @property
    def transferable_free(self) -> float:
        if self.available > 0:
            return self.available
        return self.free

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class TransferHistoryRecord:
    """Read-only universal-transfer history row."""

    transfer_type: str
    asset: str
    amount: float
    status: str
    transaction_id: str
    timestamp_ms: int | None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class TransferDiscovery:
    """Combined read-only wallet state for transfer planning."""

    timestamp: str
    asset: str
    quote_asset: str
    wallet_activations: tuple[WalletActivation, ...]
    balances: tuple[WalletAssetBalance, ...]
    recent_transfers: tuple[TransferHistoryRecord, ...] = ()
    live_transfers_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def activation(self, wallet: str) -> WalletActivation | None:
        normalized = normalize_wallet(wallet)
        for activation in self.wallet_activations:
            if activation.wallet == normalized:
                return activation
        return None

    def balance(self, wallet: str, asset: str | None = None) -> WalletAssetBalance:
        normalized_wallet = normalize_wallet(wallet)
        normalized_asset = _asset(asset or self.asset)
        for balance in self.balances:
            if balance.wallet == normalized_wallet and balance.asset == normalized_asset:
                return balance
        return WalletAssetBalance(wallet=normalized_wallet, asset=normalized_asset)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class TransferRiskLimits:
    """Conservative local limits for one dry-run transfer plan."""

    max_transfer_amount: float = 10.0
    max_transfer_value_usdt: float = 25.0
    min_source_free_after: float = 0.0
    require_active_wallets: bool = True
    block_margin_debt_transfer_out: bool = True
    allowed_wallets: tuple[str, ...] = ALL_WALLETS

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class TransferRequest:
    """Requested internal transfer modeling inputs."""

    source_wallet: str
    destination_wallet: str
    asset: str
    amount: float
    reason: str = "manual dry-run transfer plan"
    asset_usdt_price: float | None = None
    from_symbol: str | None = None
    to_symbol: str | None = None


@dataclass(frozen=True, slots=True)
class TransferPlan:
    """Dry-run transfer plan and resulting allocation state."""

    timestamp: str
    source_wallet: str
    destination_wallet: str
    transfer_type: str | None
    asset: str
    amount: float
    asset_usdt_price: float | None
    transfer_value_usdt: float | None
    reason: str
    from_symbol: str | None
    to_symbol: str | None
    source_free_before: float
    source_free_after: float
    destination_free_before: float
    destination_free_after: float
    source_active: bool | None
    destination_active: bool | None
    submission_attempted: bool
    submitted: bool
    allowed: bool
    rejection_reasons: tuple[str, ...] = ()
    warnings: tuple[str, ...] = ()
    resulting_allocation: dict[str, Any] = field(default_factory=dict)
    risk_limits: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class TransferDryRunReport:
    """Full dry-run transfer report."""

    timestamp: str
    mode: str
    discovery: dict[str, Any]
    plan: dict[str, Any] | None = None
    audit_path: str = ""
    live_transfers_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def discover_transfer_wallets(
    client: Any,
    *,
    asset: str,
    quote_asset: str = "USDT",
    history_type: str | None = None,
    include_margin: bool = True,
    include_futures: bool = True,
) -> TransferDiscovery:
    """Discover transfer-relevant wallet balances with read-only endpoints."""
    normalized_asset = _asset(asset)
    normalized_quote = _asset(quote_asset)
    wallet_summary_raw = await client.request(
        "sapiGetAssetWalletBalance",
        {"quoteAsset": normalized_quote},
        request_weight=60,
    )
    spot_raw = await client.request(
        "sapiV3PostAssetGetUserAsset",
        {"asset": normalized_asset, "needBtcValuation": True},
        request_weight=5,
    )
    funding_raw = await client.request(
        "sapiPostAssetGetFundingAsset",
        {"asset": normalized_asset, "needBtcValuation": "true"},
        request_weight=1,
    )
    balances = [
        _parse_spot_balance(spot_raw, normalized_asset),
        _parse_funding_balance(funding_raw, normalized_asset),
    ]
    if include_margin:
        margin_raw = await client.request("sapiGetMarginAccount", request_weight=10)
        balances.append(_parse_cross_margin_balance(margin_raw, normalized_asset))
    if include_futures:
        futures_raw = await client.request("fapiPrivateV3GetAccount", request_weight=5)
        balances.append(_parse_usdm_futures_balance(futures_raw, normalized_asset))

    recent_transfers: tuple[TransferHistoryRecord, ...] = ()
    if history_type:
        history_raw = await client.request(
            "sapiGetAssetTransfer",
            {"type": history_type, "size": 10},
            request_weight=1,
        )
        recent_transfers = parse_transfer_history(history_raw)

    return TransferDiscovery(
        timestamp=_utc_timestamp(),
        asset=normalized_asset,
        quote_asset=normalized_quote,
        wallet_activations=parse_wallet_activations(wallet_summary_raw),
        balances=tuple(balances),
        recent_transfers=recent_transfers,
    )


def build_transfer_plan(
    discovery: TransferDiscovery,
    request: TransferRequest,
    *,
    limits: TransferRiskLimits | None = None,
) -> TransferPlan:
    """Build a dry-run internal transfer plan without submitting it."""
    limits = limits or TransferRiskLimits()
    _validate_limits(limits)
    source_wallet = normalize_wallet(request.source_wallet)
    destination_wallet = normalize_wallet(request.destination_wallet)
    asset = _asset(request.asset)
    amount = _positive_number(request.amount, "amount")
    transfer_type = TRANSFER_ROUTES.get((source_wallet, destination_wallet))
    source_balance = discovery.balance(source_wallet, asset)
    destination_balance = discovery.balance(destination_wallet, asset)
    source_activation = discovery.activation(source_wallet)
    destination_activation = discovery.activation(destination_wallet)
    source_active = source_activation.active if source_activation is not None else None
    destination_active = (
        destination_activation.active if destination_activation is not None else None
    )
    asset_usdt_price = _asset_usdt_price(asset, request.asset_usdt_price)
    transfer_value_usdt = (
        amount * asset_usdt_price if asset_usdt_price is not None else None
    )
    source_free_before = source_balance.transferable_free
    destination_free_before = destination_balance.transferable_free
    source_free_after = source_free_before - amount
    destination_free_after = destination_free_before + amount

    rejections: list[str] = []
    warnings: list[str] = []
    if source_wallet == destination_wallet and source_wallet != WALLET_ISOLATED_MARGIN:
        rejections.append("source and destination wallets must differ")
    if source_wallet not in limits.allowed_wallets:
        rejections.append("source wallet is not allowed by risk limits")
    if destination_wallet not in limits.allowed_wallets:
        rejections.append("destination wallet is not allowed by risk limits")
    if transfer_type is None:
        rejections.append("wallet route is not supported by Binance universal transfer")
    if limits.require_active_wallets:
        if source_active is False:
            rejections.append("source wallet is not active")
        if destination_active is False:
            rejections.append("destination wallet is not active")
    if amount > limits.max_transfer_amount:
        rejections.append("amount exceeds max_transfer_amount")
    if transfer_value_usdt is None:
        rejections.append("asset_usdt_price is required for non-stable transfer caps")
    elif transfer_value_usdt > limits.max_transfer_value_usdt:
        rejections.append("transfer value exceeds max_transfer_value_usdt")
    if amount > source_free_before:
        rejections.append("amount exceeds source transferable balance")
    if source_free_after < limits.min_source_free_after:
        rejections.append("source free balance after transfer is below minimum")
    if (
        limits.block_margin_debt_transfer_out
        and source_wallet in {WALLET_CROSS_MARGIN, WALLET_ISOLATED_MARGIN}
        and source_balance.liability > 0
    ):
        rejections.append("source margin wallet has outstanding debt or interest")
    if transfer_type in {"ISOLATEDMARGIN_MARGIN", "ISOLATEDMARGIN_ISOLATEDMARGIN"}:
        if not request.from_symbol:
            rejections.append("from_symbol is required for isolated-margin source")
    if transfer_type in {"MARGIN_ISOLATEDMARGIN", "ISOLATEDMARGIN_ISOLATEDMARGIN"}:
        if not request.to_symbol:
            rejections.append("to_symbol is required for isolated-margin destination")
    if source_active is None:
        warnings.append("source wallet activation status was not available")
    if destination_active is None:
        warnings.append("destination wallet activation status was not available")

    resulting_allocation = {
        source_wallet: {
            "asset": asset,
            "free_before": source_free_before,
            "free_after": source_free_after,
        },
        destination_wallet: {
            "asset": asset,
            "free_before": destination_free_before,
            "free_after": destination_free_after,
        },
    }

    return TransferPlan(
        timestamp=_utc_timestamp(),
        source_wallet=source_wallet,
        destination_wallet=destination_wallet,
        transfer_type=transfer_type,
        asset=asset,
        amount=amount,
        asset_usdt_price=asset_usdt_price,
        transfer_value_usdt=transfer_value_usdt,
        reason=request.reason,
        from_symbol=_optional_exchange_symbol(request.from_symbol),
        to_symbol=_optional_exchange_symbol(request.to_symbol),
        source_free_before=source_free_before,
        source_free_after=source_free_after,
        destination_free_before=destination_free_before,
        destination_free_after=destination_free_after,
        source_active=source_active,
        destination_active=destination_active,
        submission_attempted=False,
        submitted=False,
        allowed=not rejections,
        rejection_reasons=tuple(rejections),
        warnings=tuple(warnings),
        resulting_allocation=resulting_allocation,
        risk_limits=limits.as_dict(),
    )


async def build_transfer_dry_run_report(
    settings: Settings,
    *,
    client: Any,
    request: TransferRequest,
    limits: TransferRiskLimits | None = None,
    audit_path: str | None = None,
) -> TransferDryRunReport:
    """Build and optionally audit a dry-run internal transfer report."""
    source_wallet = normalize_wallet(request.source_wallet)
    destination_wallet = normalize_wallet(request.destination_wallet)
    history_type = TRANSFER_ROUTES.get((source_wallet, destination_wallet))
    discovery = await discover_transfer_wallets(
        client,
        asset=request.asset,
        history_type=history_type,
        include_margin=(
            source_wallet in {WALLET_CROSS_MARGIN, WALLET_ISOLATED_MARGIN}
            or destination_wallet in {WALLET_CROSS_MARGIN, WALLET_ISOLATED_MARGIN}
        ),
        include_futures=(
            source_wallet == WALLET_USD_M_FUTURES
            or destination_wallet == WALLET_USD_M_FUTURES
        ),
    )
    plan = build_transfer_plan(discovery, request, limits=limits)
    report = TransferDryRunReport(
        timestamp=_utc_timestamp(),
        mode=_mode(settings),
        discovery=discovery.as_dict(),
        plan=plan.as_dict(),
    )
    if audit_path:
        written = write_transfer_audit(Path(audit_path), plan)
        return TransferDryRunReport(
            **{**report.as_dict(), "audit_path": str(written)}
        )
    return report


def write_transfer_audit(path: Path, plan: TransferPlan) -> Path:
    """Append one proposed transfer plan to a JSONL audit file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(plan.as_dict(), sort_keys=True) + "\n")
    return path


def parse_wallet_activations(raw: Sequence[Mapping[str, Any]]) -> tuple[WalletActivation, ...]:
    """Parse Binance wallet-balance rows."""
    if not isinstance(raw, Sequence) or isinstance(raw, (str, bytes)):
        return ()
    activations: list[WalletActivation] = []
    for item in raw:
        if not isinstance(item, Mapping):
            continue
        wallet_name = str(item.get("walletName") or "").strip()
        wallet = _wallet_from_name(wallet_name)
        if wallet is None:
            continue
        activations.append(
            WalletActivation(
                wallet=wallet,
                wallet_name=wallet_name,
                active=_optional_bool(item.get("activate")),
                quote_balance=_float(item.get("balance")),
            )
        )
    return tuple(activations)


def parse_transfer_history(raw: Mapping[str, Any] | Sequence[Mapping[str, Any]]) -> tuple[TransferHistoryRecord, ...]:
    """Parse universal-transfer history rows."""
    if isinstance(raw, Mapping):
        rows = raw.get("rows", ())
    else:
        rows = raw
    if not isinstance(rows, Sequence) or isinstance(rows, (str, bytes)):
        return ()
    records: list[TransferHistoryRecord] = []
    for item in rows:
        if not isinstance(item, Mapping):
            continue
        records.append(
            TransferHistoryRecord(
                transfer_type=str(item.get("type") or "").strip().upper(),
                asset=_asset(item.get("asset") or ""),
                amount=_float(item.get("amount")),
                status=str(item.get("status") or "unknown").strip().upper(),
                transaction_id=str(item.get("tranId") or ""),
                timestamp_ms=_optional_int(item.get("timestamp")),
            )
        )
    return tuple(records)


def assert_transfer_live_disabled(*, confirm_live_transfer: bool = False) -> None:
    """Fail closed for any attempted live transfer activation."""
    if confirm_live_transfer:
        raise TransferPlanError(
            "live transfer activation is not implemented; Phase 21 is dry-run only"
        )


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Plan Binance internal transfers without submitting them."
    )
    parser.add_argument("--dry-run", action="store_true", help="required")
    parser.add_argument(
        "--confirm-live-transfer",
        action="store_true",
        help="always fails; live transfer activation is not implemented",
    )
    parser.add_argument("--source-wallet", default=WALLET_SPOT)
    parser.add_argument("--destination-wallet", default=WALLET_FUNDING)
    parser.add_argument("--asset", default="USDT")
    parser.add_argument("--amount", type=float, required=False)
    parser.add_argument("--asset-usdt-price", type=float)
    parser.add_argument("--from-symbol")
    parser.add_argument("--to-symbol")
    parser.add_argument("--reason", default="manual dry-run transfer plan")
    parser.add_argument("--max-transfer-amount", type=float, default=10.0)
    parser.add_argument("--max-transfer-value-usdt", type=float, default=25.0)
    parser.add_argument("--min-source-free-after", type=float, default=0.0)
    parser.add_argument("--audit-path", default="data/transfer-plans.jsonl")
    parser.add_argument("--no-audit", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    if not args.dry_run:
        print("transfer check failed: --dry-run is required; live transfer is disabled")
        return 1
    try:
        assert_transfer_live_disabled(confirm_live_transfer=args.confirm_live_transfer)
        if args.amount is None:
            raise TransferPlanError("--amount is required for transfer planning")
        settings = Settings.from_env()
        request = TransferRequest(
            source_wallet=args.source_wallet,
            destination_wallet=args.destination_wallet,
            asset=args.asset,
            amount=args.amount,
            reason=args.reason,
            asset_usdt_price=args.asset_usdt_price,
            from_symbol=args.from_symbol,
            to_symbol=args.to_symbol,
        )
        limits = TransferRiskLimits(
            max_transfer_amount=args.max_transfer_amount,
            max_transfer_value_usdt=args.max_transfer_value_usdt,
            min_source_free_after=args.min_source_free_after,
        )
        report = asyncio.run(
            _run_transfer_dry_run_report(
                settings,
                request=request,
                limits=limits,
                audit_path=None if args.no_audit else args.audit_path,
            )
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_transfer_report(report))
    except Exception as exc:
        print(f"transfer check failed: {exc}")
        return 1
    return 0


def render_transfer_report(report: TransferDryRunReport) -> str:
    """Render a concise human-readable dry-run transfer report."""
    lines = [
        f"Mode: {report.mode}",
        "Live transfers: disabled",
        "Write endpoints used: none",
    ]
    if report.plan:
        plan = report.plan
        lines.extend(
            [
                "Transfer plan:",
                f"  - route: {plan['source_wallet']} -> {plan['destination_wallet']}",
                f"  - type: {plan['transfer_type']}",
                f"  - asset: {plan['asset']}",
                f"  - amount: {plan['amount']}",
                f"  - source after: {plan['source_free_after']}",
                f"  - destination after: {plan['destination_free_after']}",
                f"  - allowed: {_yes_no(bool(plan['allowed']))}",
            ]
        )
        if plan["rejection_reasons"]:
            lines.append("  - rejection reasons: " + ", ".join(plan["rejection_reasons"]))
        if plan["warnings"]:
            lines.append("  - warnings: " + ", ".join(plan["warnings"]))
    if report.audit_path:
        lines.append(f"Audit path: {report.audit_path}")
    return "\n".join(lines)


async def _run_transfer_dry_run_report(
    settings: Settings,
    *,
    request: TransferRequest,
    limits: TransferRiskLimits,
    audit_path: str | None,
) -> TransferDryRunReport:
    client = BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    try:
        return await build_transfer_dry_run_report(
            settings,
            client=client,
            request=request,
            limits=limits,
            audit_path=audit_path,
        )
    finally:
        await client.close()


def _parse_spot_balance(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
    asset: str,
) -> WalletAssetBalance:
    item = _first_asset_row(raw, asset)
    return WalletAssetBalance(
        wallet=WALLET_SPOT,
        asset=asset,
        free=_float(item.get("free")),
        locked=_float(item.get("locked")) + _float(item.get("freeze")),
        btc_valuation=_float(item.get("btcValuation")),
    )


def _parse_funding_balance(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
    asset: str,
) -> WalletAssetBalance:
    item = _first_asset_row(raw, asset)
    return WalletAssetBalance(
        wallet=WALLET_FUNDING,
        asset=asset,
        free=_float(item.get("free")),
        locked=(
            _float(item.get("locked"))
            + _float(item.get("freeze"))
            + _float(item.get("withdrawing"))
        ),
        btc_valuation=_float(item.get("btcValuation")),
    )


def _parse_cross_margin_balance(raw: Mapping[str, Any], asset: str) -> WalletAssetBalance:
    user_assets = raw.get("userAssets") if isinstance(raw, Mapping) else ()
    item = _first_asset_row(user_assets, asset)
    return WalletAssetBalance(
        wallet=WALLET_CROSS_MARGIN,
        asset=asset,
        free=_float(item.get("free")),
        locked=_float(item.get("locked")),
        borrowed=_float(item.get("borrowed")),
        interest=_float(item.get("interest")),
    )


def _parse_usdm_futures_balance(raw: Mapping[str, Any], asset: str) -> WalletAssetBalance:
    assets = raw.get("assets") if isinstance(raw, Mapping) else ()
    item = _first_asset_row(assets, asset)
    return WalletAssetBalance(
        wallet=WALLET_USD_M_FUTURES,
        asset=asset,
        free=_float(item.get("availableBalance")),
        available=_float(item.get("availableBalance")),
        wallet_balance=_float(item.get("walletBalance")),
        locked=_float(item.get("initialMargin")) + _float(item.get("maintMargin")),
    )


def _first_asset_row(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]] | object,
    asset: str,
) -> Mapping[str, Any]:
    if isinstance(raw, Mapping):
        if _asset(raw.get("asset") or asset) == asset:
            return raw
        return {}
    if isinstance(raw, Sequence) and not isinstance(raw, (str, bytes)):
        for item in raw:
            if isinstance(item, Mapping) and _asset(item.get("asset") or "") == asset:
                return item
    return {}


def normalize_wallet(value: str) -> str:
    normalized = value.strip().lower().replace("-", "_").replace(" ", "_")
    aliases = {
        "main": WALLET_SPOT,
        "spot": WALLET_SPOT,
        "fund": WALLET_FUNDING,
        "funding": WALLET_FUNDING,
        "margin": WALLET_CROSS_MARGIN,
        "cross_margin": WALLET_CROSS_MARGIN,
        "isolated": WALLET_ISOLATED_MARGIN,
        "isolated_margin": WALLET_ISOLATED_MARGIN,
        "umfuture": WALLET_USD_M_FUTURES,
        "um_futures": WALLET_USD_M_FUTURES,
        "usd_m": WALLET_USD_M_FUTURES,
        "usd_m_futures": WALLET_USD_M_FUTURES,
        "usdm": WALLET_USD_M_FUTURES,
    }
    if normalized not in aliases:
        raise TransferPlanError(f"unsupported wallet: {value}")
    return aliases[normalized]


def _wallet_from_name(wallet_name: str) -> str | None:
    normalized = wallet_name.strip().lower()
    if normalized == "spot":
        return WALLET_SPOT
    if normalized == "funding":
        return WALLET_FUNDING
    if normalized == "cross margin":
        return WALLET_CROSS_MARGIN
    if normalized == "isolated margin":
        return WALLET_ISOLATED_MARGIN
    if "usd" in normalized and "futures" in normalized:
        return WALLET_USD_M_FUTURES
    return None


def _optional_exchange_symbol(symbol: str | None) -> str | None:
    if symbol is None or not symbol.strip():
        return None
    return re.sub(r"[^A-Za-z0-9]", "", normalize_symbol(symbol)).upper()


def _asset_usdt_price(asset: str, value: float | None) -> float | None:
    if _asset(asset) in {"USDT", "USDC", "FDUSD", "BUSD", "TUSD", "DAI"}:
        return 1.0
    if value is None:
        return None
    return _positive_number(value, "asset_usdt_price")


def _validate_limits(limits: TransferRiskLimits) -> None:
    _positive_number(limits.max_transfer_amount, "max_transfer_amount")
    _positive_number(limits.max_transfer_value_usdt, "max_transfer_value_usdt")
    _non_negative_number(limits.min_source_free_after, "min_source_free_after")
    for wallet in limits.allowed_wallets:
        normalize_wallet(wallet)


def _asset(value: object) -> str:
    text = str(value or "").strip().upper()
    if not text:
        raise TransferPlanError("asset must not be blank")
    return text


def _positive_number(value: float, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number <= 0:
        raise TransferPlanError(f"{field_name} must be greater than zero")
    return number


def _non_negative_number(value: float, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number < 0:
        raise TransferPlanError(f"{field_name} must be at least zero")
    return number


def _coerce_float(value: object, field_name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise TransferPlanError(f"{field_name} must be a number") from exc
    if not math.isfinite(number):
        raise TransferPlanError(f"{field_name} must be finite")
    return number


def _float(value: object) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return 0.0
    return number if math.isfinite(number) else 0.0


def _optional_int(value: object) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


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

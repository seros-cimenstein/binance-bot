"""Dry-run USD-M futures discovery and risk modeling.

Phase 20 is modeling only. This module may read USD-M futures account,
position, market, funding, symbol-config, and leverage-bracket data, but it has
no live futures order, leverage-change, margin-mode, or position-mode write
path.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import re
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from decimal import Decimal
from typing import Any, Mapping, Sequence

from src.config import Settings
from src.data.feeds import normalize_symbol
from src.exchange.client import BinanceClient

PRODUCT_USDM = "USD-M"
POSITION_BOTH = "BOTH"
POSITION_LONG = "LONG"
POSITION_SHORT = "SHORT"
SIDE_BUY = "BUY"
SIDE_SELL = "SELL"
ORDER_MARKET = "MARKET"
ORDER_LIMIT = "LIMIT"
WRITE_ENDPOINTS_BLOCKED = (
    "fapiPrivatePostOrder",
    "fapiPrivatePostBatchOrders",
    "fapiPrivatePostLeverage",
    "fapiPrivatePostMarginType",
    "fapiPrivatePostPositionSideDual",
    "fapiPrivatePostPositionMargin",
)


class FuturesModelError(RuntimeError):
    """Raised when dry-run futures modeling cannot proceed safely."""


@dataclass(frozen=True, slots=True)
class FuturesAssetBalance:
    """One USD-M futures asset balance."""

    asset: str
    wallet_balance: float = 0.0
    unrealized_profit: float = 0.0
    margin_balance: float = 0.0
    maint_margin: float = 0.0
    initial_margin: float = 0.0
    position_initial_margin: float = 0.0
    open_order_initial_margin: float = 0.0
    cross_wallet_balance: float = 0.0
    cross_unrealized_pnl: float = 0.0
    available_balance: float = 0.0
    max_withdraw_amount: float = 0.0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesAccountSnapshot:
    """Read-only USD-M account snapshot."""

    total_wallet_balance: float
    total_unrealized_profit: float
    total_margin_balance: float
    total_initial_margin: float
    total_maint_margin: float
    total_position_initial_margin: float
    total_open_order_initial_margin: float
    total_cross_wallet_balance: float
    total_cross_unrealized_pnl: float
    available_balance: float
    max_withdraw_amount: float
    assets: tuple[FuturesAssetBalance, ...] = ()

    def maintenance_buffer_percent(self) -> float | None:
        if self.total_maint_margin <= 0:
            return None
        return (
            (self.total_margin_balance - self.total_maint_margin)
            / self.total_maint_margin
        ) * 100.0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesPosition:
    """Read-only USD-M position risk row."""

    symbol: str
    position_side: str
    position_amt: float
    entry_price: float
    break_even_price: float
    mark_price: float
    unrealized_profit: float
    liquidation_price: float | None
    notional: float
    margin_asset: str
    isolated_margin: float
    isolated_wallet: float
    initial_margin: float
    maint_margin: float
    position_initial_margin: float
    open_order_initial_margin: float
    leverage: float | None
    margin_type: str
    adl: int | None
    update_time: int | None

    @property
    def abs_notional(self) -> float:
        return abs(self.notional)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesSymbolRules:
    """Market rules and filters for one USD-M symbol."""

    symbol: str
    base_asset: str
    quote_asset: str
    margin_asset: str
    contract_type: str
    status: str
    quantity_step: float
    min_qty: float
    max_qty: float
    tick_size: float
    min_price: float
    max_price: float
    min_notional: float
    trigger_protect: float

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesSymbolConfig:
    """Account-specific USD-M symbol configuration."""

    symbol: str
    margin_type: str
    leverage: float
    max_notional_value: float
    auto_add_margin: bool | None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesLeverageBracket:
    """One leverage bracket for a USD-M symbol."""

    symbol: str
    initial_leverage: float
    notional_floor: float
    notional_cap: float
    maint_margin_ratio: float
    cum: float

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesMarkPrice:
    """Mark/index price and current funding information."""

    symbol: str
    mark_price: float
    index_price: float
    estimated_settle_price: float
    last_funding_rate: float
    interest_rate: float
    next_funding_time: int | None
    time: int | None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesDiscovery:
    """Combined read-only USD-M futures discovery result."""

    timestamp: str
    product: str
    account: FuturesAccountSnapshot
    positions: tuple[FuturesPosition, ...]
    symbols: tuple[FuturesSymbolRules, ...]
    symbol_configs: tuple[FuturesSymbolConfig, ...]
    leverage_brackets: tuple[FuturesLeverageBracket, ...]
    mark_prices: tuple[FuturesMarkPrice, ...]
    live_trading_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def symbol_rules(self, symbol: str) -> FuturesSymbolRules | None:
        exchange_symbol = _exchange_symbol(symbol)
        for rules in self.symbols:
            if rules.symbol == exchange_symbol:
                return rules
        return None

    def symbol_config(self, symbol: str) -> FuturesSymbolConfig | None:
        exchange_symbol = _exchange_symbol(symbol)
        for config in self.symbol_configs:
            if config.symbol == exchange_symbol:
                return config
        return None

    def leverage_bracket(self, symbol: str, notional: float) -> FuturesLeverageBracket | None:
        exchange_symbol = _exchange_symbol(symbol)
        candidates = [
            bracket
            for bracket in self.leverage_brackets
            if bracket.symbol == exchange_symbol
        ]
        for bracket in candidates:
            if bracket.notional_floor <= notional <= bracket.notional_cap:
                return bracket
        return candidates[0] if candidates else None

    def mark_price(self, symbol: str) -> FuturesMarkPrice | None:
        exchange_symbol = _exchange_symbol(symbol)
        for mark in self.mark_prices:
            if mark.symbol == exchange_symbol:
                return mark
        return None

    def position(self, symbol: str, position_side: str = POSITION_BOTH) -> FuturesPosition | None:
        exchange_symbol = _exchange_symbol(symbol)
        normalized_side = _position_side(position_side)
        for position in self.positions:
            if position.symbol == exchange_symbol and position.position_side == normalized_side:
                return position
        return None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesRiskLimits:
    """Conservative local risk limits for USD-M dry-run plans."""

    max_leverage: float = 3.0
    max_notional_usdt: float = 25.0
    max_account_notional_percent: float = 25.0
    max_initial_margin_usdt: float = 10.0
    min_free_balance_after_initial_margin_usdt: float = 5.0
    min_liquidation_distance_percent: float = 20.0
    max_abs_funding_rate: float = 0.001
    max_abs_funding_payment_usdt: float = 0.05
    min_maintenance_buffer_percent: float = 50.0
    allowed_margin_types: tuple[str, ...] = ("CROSSED", "ISOLATED")

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesOrderIntent:
    """Requested USD-M futures order modeling inputs."""

    symbol: str
    side: str
    quantity: float
    order_type: str = ORDER_MARKET
    price: float | None = None
    position_side: str = POSITION_BOTH
    reduce_only: bool = False
    requested_leverage: float | None = None


@dataclass(frozen=True, slots=True)
class FuturesOrderPlan:
    """Dry-run USD-M order risk result."""

    timestamp: str
    product: str
    symbol: str
    side: str
    position_side: str
    order_type: str
    quantity: float
    price: float | None
    mark_price: float
    notional_usdt: float
    requested_leverage: float | None
    configured_leverage: float
    effective_leverage: float
    margin_type: str
    initial_margin_required_usdt: float
    maintenance_margin_estimate_usdt: float
    liquidation_price_estimate: float | None
    liquidation_distance_percent: float | None
    funding_rate: float
    projected_funding_payment_usdt: float
    reduce_only: bool
    opening_position: bool
    closing_position: bool
    maintenance_buffer_percent: float | None
    symbol_rules: dict[str, Any]
    leverage_bracket: dict[str, Any]
    allowed: bool
    rejection_reasons: tuple[str, ...] = ()
    warnings: tuple[str, ...] = ()
    risk_limits: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesReconciliation:
    """Dry-run reconciliation of current USD-M account state."""

    timestamp: str
    total_positions: int
    nonzero_positions: int
    total_abs_notional_usdt: float
    total_unrealized_pnl_usdt: float
    margin_balance_usdt: float
    maint_margin_usdt: float
    maintenance_buffer_percent: float | None
    allowed: bool
    rejection_reasons: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class FuturesDryRunReport:
    """Full USD-M futures dry-run report."""

    timestamp: str
    mode: str
    discovery: dict[str, Any]
    order_plan: dict[str, Any] | None = None
    reconciliation: dict[str, Any] | None = None
    live_trading_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def discover_usdm_futures(
    client: Any,
    *,
    symbols: Sequence[str] = (),
) -> FuturesDiscovery:
    """Discover USD-M futures account state with read-only calls."""
    exchange_symbols = tuple(_exchange_symbol(symbol) for symbol in symbols)
    account_raw = await client.request("fapiPrivateV3GetAccount", request_weight=5)
    position_params = _single_symbol_params(exchange_symbols)
    positions_raw = await client.request(
        "fapiPrivateV3GetPositionRisk",
        position_params,
        request_weight=5,
    )
    exchange_info_raw = await client.request(
        "fapiPublicGetExchangeInfo",
        request_weight=1,
    )
    symbol_config_raw = await client.request(
        "fapiPrivateGetSymbolConfig",
        position_params,
        request_weight=5,
    )
    leverage_brackets_raw = await client.request(
        "fapiPrivateGetLeverageBracket",
        position_params,
        request_weight=1,
    )
    mark_prices_raw = await _fetch_mark_prices(client, exchange_symbols)

    symbols_parsed = parse_futures_symbol_rules(exchange_info_raw, exchange_symbols)
    return FuturesDiscovery(
        timestamp=_utc_timestamp(),
        product=PRODUCT_USDM,
        account=parse_futures_account_snapshot(account_raw),
        positions=parse_futures_positions(positions_raw),
        symbols=symbols_parsed,
        symbol_configs=parse_futures_symbol_configs(symbol_config_raw),
        leverage_brackets=parse_futures_leverage_brackets(leverage_brackets_raw),
        mark_prices=parse_futures_mark_prices(mark_prices_raw),
    )


def build_futures_order_plan(
    discovery: FuturesDiscovery,
    intent: FuturesOrderIntent,
    *,
    limits: FuturesRiskLimits | None = None,
) -> FuturesOrderPlan:
    """Build a dry-run USD-M order plan without submitting an order."""
    limits = limits or FuturesRiskLimits()
    _validate_limits(limits)
    symbol = _exchange_symbol(intent.symbol)
    side = _side(intent.side)
    position_side = _position_side(intent.position_side)
    order_type = _order_type(intent.order_type)
    quantity = _positive_number(intent.quantity, "quantity")
    rules = _required(discovery.symbol_rules(symbol), f"symbol rules not found for {symbol}")
    config = _required(
        discovery.symbol_config(symbol),
        f"symbol config not found for {symbol}",
    )
    mark = _required(discovery.mark_price(symbol), f"mark price not found for {symbol}")
    price = _execution_price(order_type, intent.price, mark)
    notional = quantity * price
    bracket = _required(
        discovery.leverage_bracket(symbol, notional),
        f"leverage bracket not found for {symbol}",
    )
    configured_leverage = (
        _positive_number(intent.requested_leverage, "requested_leverage")
        if intent.requested_leverage is not None
        else config.leverage
    )
    effective_leverage = configured_leverage
    initial_margin = notional / effective_leverage if effective_leverage > 0 else math.inf
    maintenance_margin = notional * bracket.maint_margin_ratio + bracket.cum
    liquidation_price = _estimate_liquidation_price(
        side=side,
        price=price,
        leverage=effective_leverage,
        maint_margin_ratio=bracket.maint_margin_ratio,
    )
    liquidation_distance = _liquidation_distance_percent(
        side=side,
        price=price,
        liquidation_price=liquidation_price,
    )
    funding_payment = notional * mark.last_funding_rate
    if _opens_short(side, position_side):
        funding_payment *= -1
    position = discovery.position(symbol, position_side)
    closing_position = _is_closing_position(position, side, quantity, position_side)
    opening_position = not closing_position
    maintenance_buffer = discovery.account.maintenance_buffer_percent()

    rejections: list[str] = []
    warnings: list[str] = []
    if rules.status != "TRADING":
        rejections.append("symbol is not trading")
    if rules.contract_type != "PERPETUAL":
        rejections.append("only USD-M perpetual contracts are modeled")
    if quantity < rules.min_qty:
        rejections.append("quantity is below the symbol minimum")
    if rules.max_qty > 0 and quantity > rules.max_qty:
        rejections.append("quantity is above the symbol maximum")
    if rules.quantity_step > 0 and not _is_multiple(quantity, rules.quantity_step):
        rejections.append("quantity does not align with the symbol step size")
    if order_type == ORDER_LIMIT and intent.price is not None:
        if rules.tick_size > 0 and not _is_multiple(intent.price, rules.tick_size):
            rejections.append("limit price does not align with the symbol tick size")
    if notional < rules.min_notional:
        rejections.append("notional is below the symbol minimum notional")
    if configured_leverage > limits.max_leverage:
        rejections.append("configured leverage exceeds the risk limit")
    if configured_leverage > bracket.initial_leverage:
        rejections.append("configured leverage exceeds the leverage bracket")
    if config.margin_type not in set(limits.allowed_margin_types):
        rejections.append("margin type is not allowed by risk limits")
    if notional > limits.max_notional_usdt:
        rejections.append("notional exceeds max_notional_usdt")
    if (
        discovery.account.total_margin_balance > 0
        and notional
        > discovery.account.total_margin_balance
        * (limits.max_account_notional_percent / 100.0)
    ):
        rejections.append("notional exceeds account exposure percent limit")
    if initial_margin > limits.max_initial_margin_usdt:
        rejections.append("initial margin exceeds max_initial_margin_usdt")
    if initial_margin > discovery.account.available_balance:
        rejections.append("initial margin exceeds available futures balance")
    if (
        discovery.account.available_balance - initial_margin
        < limits.min_free_balance_after_initial_margin_usdt
    ):
        rejections.append("free balance after initial margin would be too low")
    if (
        liquidation_distance is not None
        and liquidation_distance < limits.min_liquidation_distance_percent
    ):
        rejections.append("estimated liquidation distance is below minimum")
    if abs(mark.last_funding_rate) > limits.max_abs_funding_rate:
        rejections.append("absolute funding rate exceeds configured cap")
    if abs(funding_payment) > limits.max_abs_funding_payment_usdt:
        rejections.append("projected funding payment exceeds configured cap")
    if (
        maintenance_buffer is not None
        and maintenance_buffer < limits.min_maintenance_buffer_percent
    ):
        rejections.append("account maintenance buffer is below minimum")
    if intent.reduce_only and not closing_position:
        rejections.append("reduce-only order would not reduce an existing position")
    if intent.reduce_only and position is not None and quantity > abs(position.position_amt):
        rejections.append("reduce-only quantity exceeds the existing position size")
    if not intent.reduce_only and closing_position:
        warnings.append("order would reduce an existing position but is not reduce-only")

    return FuturesOrderPlan(
        timestamp=_utc_timestamp(),
        product=PRODUCT_USDM,
        symbol=symbol,
        side=side,
        position_side=position_side,
        order_type=order_type,
        quantity=quantity,
        price=intent.price if order_type == ORDER_LIMIT else None,
        mark_price=mark.mark_price,
        notional_usdt=notional,
        requested_leverage=intent.requested_leverage,
        configured_leverage=configured_leverage,
        effective_leverage=effective_leverage,
        margin_type=config.margin_type,
        initial_margin_required_usdt=initial_margin,
        maintenance_margin_estimate_usdt=maintenance_margin,
        liquidation_price_estimate=liquidation_price,
        liquidation_distance_percent=liquidation_distance,
        funding_rate=mark.last_funding_rate,
        projected_funding_payment_usdt=funding_payment,
        reduce_only=intent.reduce_only,
        opening_position=opening_position,
        closing_position=closing_position,
        maintenance_buffer_percent=maintenance_buffer,
        symbol_rules=rules.as_dict(),
        leverage_bracket=bracket.as_dict(),
        allowed=not rejections,
        rejection_reasons=tuple(rejections),
        warnings=tuple(warnings),
        risk_limits=limits.as_dict(),
    )


def reconcile_futures_state(
    discovery: FuturesDiscovery,
    *,
    limits: FuturesRiskLimits | None = None,
) -> FuturesReconciliation:
    """Summarize current USD-M futures positions and account buffer."""
    limits = limits or FuturesRiskLimits()
    _validate_limits(limits)
    nonzero = [
        position
        for position in discovery.positions
        if abs(position.position_amt) > 0 or abs(position.notional) > 0
    ]
    total_abs_notional = sum(position.abs_notional for position in nonzero)
    total_unrealized = sum(position.unrealized_profit for position in nonzero)
    buffer_percent = discovery.account.maintenance_buffer_percent()
    rejections: list[str] = []
    if (
        buffer_percent is not None
        and buffer_percent < limits.min_maintenance_buffer_percent
    ):
        rejections.append("account maintenance buffer is below minimum")
    if total_abs_notional > limits.max_notional_usdt:
        rejections.append("open position notional exceeds max_notional_usdt")
    return FuturesReconciliation(
        timestamp=_utc_timestamp(),
        total_positions=len(discovery.positions),
        nonzero_positions=len(nonzero),
        total_abs_notional_usdt=total_abs_notional,
        total_unrealized_pnl_usdt=total_unrealized,
        margin_balance_usdt=discovery.account.total_margin_balance,
        maint_margin_usdt=discovery.account.total_maint_margin,
        maintenance_buffer_percent=buffer_percent,
        allowed=not rejections,
        rejection_reasons=tuple(rejections),
    )


async def build_futures_dry_run_report(
    settings: Settings,
    *,
    client: Any,
    symbols: Sequence[str],
    intent: FuturesOrderIntent | None = None,
    limits: FuturesRiskLimits | None = None,
) -> FuturesDryRunReport:
    """Build a full USD-M futures dry-run report from read-only endpoints."""
    discovery = await discover_usdm_futures(client, symbols=symbols)
    reconciliation = reconcile_futures_state(discovery, limits=limits)
    order_plan = (
        build_futures_order_plan(discovery, intent, limits=limits)
        if intent is not None
        else None
    )
    return FuturesDryRunReport(
        timestamp=_utc_timestamp(),
        mode=_mode(settings),
        discovery=discovery.as_dict(),
        order_plan=order_plan.as_dict() if order_plan is not None else None,
        reconciliation=reconciliation.as_dict(),
    )


def parse_futures_account_snapshot(raw: Mapping[str, Any]) -> FuturesAccountSnapshot:
    """Parse Binance's USD-M futures account response."""
    if not isinstance(raw, Mapping):
        raise FuturesModelError("futures account response was not a mapping")
    assets_raw = raw.get("assets")
    assets = (
        tuple(_parse_asset_balance(asset_raw) for asset_raw in assets_raw)
        if isinstance(assets_raw, Sequence)
        and not isinstance(assets_raw, (str, bytes))
        else ()
    )
    return FuturesAccountSnapshot(
        total_wallet_balance=_float(raw.get("totalWalletBalance")),
        total_unrealized_profit=_float(raw.get("totalUnrealizedProfit")),
        total_margin_balance=_float(raw.get("totalMarginBalance")),
        total_initial_margin=_float(raw.get("totalInitialMargin")),
        total_maint_margin=_float(raw.get("totalMaintMargin")),
        total_position_initial_margin=_float(raw.get("totalPositionInitialMargin")),
        total_open_order_initial_margin=_float(raw.get("totalOpenOrderInitialMargin")),
        total_cross_wallet_balance=_float(raw.get("totalCrossWalletBalance")),
        total_cross_unrealized_pnl=_float(raw.get("totalCrossUnPnl")),
        available_balance=_float(raw.get("availableBalance")),
        max_withdraw_amount=_float(raw.get("maxWithdrawAmount")),
        assets=assets,
    )


def parse_futures_positions(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
) -> tuple[FuturesPosition, ...]:
    """Parse Binance's USD-M position-risk response."""
    items = _payload_items(raw, item_key="positions")
    positions: list[FuturesPosition] = []
    for item in items:
        symbol = str(item.get("symbol") or "").strip().upper()
        if not symbol:
            continue
        positions.append(
            FuturesPosition(
                symbol=symbol,
                position_side=_position_side(str(item.get("positionSide") or POSITION_BOTH)),
                position_amt=_float(item.get("positionAmt")),
                entry_price=_float(item.get("entryPrice")),
                break_even_price=_float(item.get("breakEvenPrice")),
                mark_price=_float(item.get("markPrice")),
                unrealized_profit=_float(item.get("unRealizedProfit")),
                liquidation_price=_optional_positive_float(item.get("liquidationPrice")),
                notional=_float(item.get("notional")),
                margin_asset=_asset(item.get("marginAsset") or "USDT"),
                isolated_margin=_float(item.get("isolatedMargin")),
                isolated_wallet=_float(item.get("isolatedWallet")),
                initial_margin=_float(item.get("initialMargin")),
                maint_margin=_float(item.get("maintMargin")),
                position_initial_margin=_float(item.get("positionInitialMargin")),
                open_order_initial_margin=_float(item.get("openOrderInitialMargin")),
                leverage=_optional_positive_float(item.get("leverage")),
                margin_type=_margin_type(item.get("marginType") or "CROSSED"),
                adl=_optional_int(item.get("adl")),
                update_time=_optional_int(item.get("updateTime")),
            )
        )
    return tuple(positions)


def parse_futures_symbol_rules(
    raw: Mapping[str, Any],
    exchange_symbols: Sequence[str] = (),
) -> tuple[FuturesSymbolRules, ...]:
    """Parse USD-M futures exchangeInfo symbol filters."""
    if not isinstance(raw, Mapping):
        raise FuturesModelError("futures exchangeInfo response was not a mapping")
    symbols_raw = raw.get("symbols")
    if not isinstance(symbols_raw, Sequence) or isinstance(symbols_raw, (str, bytes)):
        raise FuturesModelError("futures exchangeInfo response did not include symbols")
    wanted = set(exchange_symbols)
    rules: list[FuturesSymbolRules] = []
    for item in symbols_raw:
        if not isinstance(item, Mapping):
            continue
        symbol = str(item.get("symbol") or "").strip().upper()
        if not symbol or (wanted and symbol not in wanted):
            continue
        filters = _filters_by_type(item.get("filters"))
        price_filter = filters.get("PRICE_FILTER", {})
        lot_filter = filters.get("LOT_SIZE", {})
        min_notional_filter = filters.get("MIN_NOTIONAL", {})
        rules.append(
            FuturesSymbolRules(
                symbol=symbol,
                base_asset=_asset(item.get("baseAsset") or ""),
                quote_asset=_asset(item.get("quoteAsset") or ""),
                margin_asset=_asset(item.get("marginAsset") or "USDT"),
                contract_type=str(item.get("contractType") or "").strip().upper(),
                status=str(item.get("status") or "").strip().upper(),
                quantity_step=_float(lot_filter.get("stepSize")),
                min_qty=_float(lot_filter.get("minQty")),
                max_qty=_float(lot_filter.get("maxQty")),
                tick_size=_float(price_filter.get("tickSize")),
                min_price=_float(price_filter.get("minPrice")),
                max_price=_float(price_filter.get("maxPrice")),
                min_notional=_float(
                    min_notional_filter.get("notional")
                    if min_notional_filter.get("notional") is not None
                    else min_notional_filter.get("minNotional")
                ),
                trigger_protect=_float(item.get("triggerProtect")),
            )
        )
    return tuple(rules)


def parse_futures_symbol_configs(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
) -> tuple[FuturesSymbolConfig, ...]:
    """Parse account-specific USD-M symbol configuration."""
    configs: list[FuturesSymbolConfig] = []
    for item in _payload_items(raw):
        symbol = str(item.get("symbol") or "").strip().upper()
        if not symbol:
            continue
        configs.append(
            FuturesSymbolConfig(
                symbol=symbol,
                margin_type=_margin_type(item.get("marginType") or "CROSSED"),
                leverage=_float(item.get("leverage")),
                max_notional_value=_float(item.get("maxNotionalValue")),
                auto_add_margin=_optional_boolish(item.get("isAutoAddMargin")),
            )
        )
    return tuple(configs)


def parse_futures_leverage_brackets(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
) -> tuple[FuturesLeverageBracket, ...]:
    """Parse USD-M leverage-bracket response."""
    brackets: list[FuturesLeverageBracket] = []
    for item in _payload_items(raw):
        symbol = str(item.get("symbol") or "").strip().upper()
        raw_brackets = item.get("brackets")
        if not symbol or not isinstance(raw_brackets, Sequence):
            continue
        for bracket in raw_brackets:
            if not isinstance(bracket, Mapping):
                continue
            brackets.append(
                FuturesLeverageBracket(
                    symbol=symbol,
                    initial_leverage=_float(bracket.get("initialLeverage")),
                    notional_floor=_float(bracket.get("notionalFloor")),
                    notional_cap=_float(bracket.get("notionalCap")),
                    maint_margin_ratio=_float(bracket.get("maintMarginRatio")),
                    cum=_float(bracket.get("cum")),
                )
            )
    return tuple(brackets)


def parse_futures_mark_prices(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
) -> tuple[FuturesMarkPrice, ...]:
    """Parse USD-M mark price and funding data."""
    marks: list[FuturesMarkPrice] = []
    for item in _payload_items(raw):
        symbol = str(item.get("symbol") or "").strip().upper()
        if not symbol:
            continue
        marks.append(
            FuturesMarkPrice(
                symbol=symbol,
                mark_price=_float(item.get("markPrice")),
                index_price=_float(item.get("indexPrice")),
                estimated_settle_price=_float(item.get("estimatedSettlePrice")),
                last_funding_rate=_float(item.get("lastFundingRate")),
                interest_rate=_float(item.get("interestRate")),
                next_funding_time=_optional_int(item.get("nextFundingTime")),
                time=_optional_int(item.get("time")),
            )
        )
    return tuple(marks)


def assert_futures_live_disabled(*, confirm_live_futures: bool = False) -> None:
    """Fail closed for any attempted live futures activation."""
    if confirm_live_futures:
        raise FuturesModelError(
            "live futures activation is not implemented; Phase 20 is dry-run only"
        )


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run read-only USD-M futures discovery and risk modeling."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="required; confirms no live futures write action will be attempted",
    )
    parser.add_argument(
        "--confirm-live-futures",
        action="store_true",
        help="always fails; live futures activation is not implemented",
    )
    parser.add_argument("--symbol", help="USD-M futures symbol, for example BTC/USDT")
    parser.add_argument("--side", choices=(SIDE_BUY, SIDE_SELL), default=SIDE_BUY)
    parser.add_argument("--quantity", type=float, help="quantity to model")
    parser.add_argument("--price", type=float, help="limit price to model")
    parser.add_argument(
        "--order-type",
        choices=(ORDER_MARKET, ORDER_LIMIT),
        default=ORDER_MARKET,
    )
    parser.add_argument(
        "--position-side",
        choices=(POSITION_BOTH, POSITION_LONG, POSITION_SHORT),
        default=POSITION_BOTH,
    )
    parser.add_argument("--reduce-only", action="store_true")
    parser.add_argument("--requested-leverage", type=float)
    parser.add_argument("--max-leverage", type=float, default=FuturesRiskLimits.max_leverage)
    parser.add_argument(
        "--max-notional-usdt",
        type=float,
        default=FuturesRiskLimits.max_notional_usdt,
    )
    parser.add_argument(
        "--max-account-notional-percent",
        type=float,
        default=FuturesRiskLimits.max_account_notional_percent,
    )
    parser.add_argument(
        "--max-initial-margin-usdt",
        type=float,
        default=FuturesRiskLimits.max_initial_margin_usdt,
    )
    parser.add_argument(
        "--min-free-balance-after-initial-margin-usdt",
        type=float,
        default=FuturesRiskLimits.min_free_balance_after_initial_margin_usdt,
    )
    parser.add_argument(
        "--min-liquidation-distance-percent",
        type=float,
        default=FuturesRiskLimits.min_liquidation_distance_percent,
    )
    parser.add_argument(
        "--max-abs-funding-rate",
        type=float,
        default=FuturesRiskLimits.max_abs_funding_rate,
    )
    parser.add_argument(
        "--max-abs-funding-payment-usdt",
        type=float,
        default=FuturesRiskLimits.max_abs_funding_payment_usdt,
    )
    parser.add_argument(
        "--min-maintenance-buffer-percent",
        type=float,
        default=FuturesRiskLimits.min_maintenance_buffer_percent,
    )
    parser.add_argument("--json", action="store_true", help="print JSON output")
    args = parser.parse_args(argv)

    if not args.dry_run:
        print("futures check failed: --dry-run is required; live futures is disabled")
        return 1
    try:
        assert_futures_live_disabled(confirm_live_futures=args.confirm_live_futures)
        settings = Settings.from_env()
        symbol = normalize_symbol(args.symbol or settings.trading_symbol)
        limits = FuturesRiskLimits(
            max_leverage=args.max_leverage,
            max_notional_usdt=args.max_notional_usdt,
            max_account_notional_percent=args.max_account_notional_percent,
            max_initial_margin_usdt=args.max_initial_margin_usdt,
            min_free_balance_after_initial_margin_usdt=(
                args.min_free_balance_after_initial_margin_usdt
            ),
            min_liquidation_distance_percent=args.min_liquidation_distance_percent,
            max_abs_funding_rate=args.max_abs_funding_rate,
            max_abs_funding_payment_usdt=args.max_abs_funding_payment_usdt,
            min_maintenance_buffer_percent=args.min_maintenance_buffer_percent,
        )
        intent = (
            FuturesOrderIntent(
                symbol=symbol,
                side=args.side,
                quantity=args.quantity,
                order_type=args.order_type,
                price=args.price,
                position_side=args.position_side,
                reduce_only=args.reduce_only,
                requested_leverage=args.requested_leverage,
            )
            if args.quantity is not None
            else None
        )
        report = asyncio.run(
            _run_futures_dry_run_report(
                settings,
                symbols=(symbol,),
                intent=intent,
                limits=limits,
            )
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_futures_report(report))
    except Exception as exc:
        print(f"futures check failed: {exc}")
        return 1
    return 0


def render_futures_report(report: FuturesDryRunReport) -> str:
    """Render a concise human-readable USD-M dry-run report."""
    discovery = report.discovery
    account = discovery.get("account", {}) if isinstance(discovery, Mapping) else {}
    positions = discovery.get("positions", ()) if isinstance(discovery, Mapping) else ()
    lines = [
        f"Mode: {report.mode}",
        "Live futures trading: disabled",
        "Write endpoints used: none",
        f"Product: {discovery.get('product', PRODUCT_USDM) if isinstance(discovery, Mapping) else PRODUCT_USDM}",
        f"Margin balance USDT: {account.get('total_margin_balance', 0.0)}",
        f"Available balance USDT: {account.get('available_balance', 0.0)}",
        f"Positions discovered: {len(positions) if isinstance(positions, Sequence) else 0}",
    ]
    if report.reconciliation:
        reconciliation = report.reconciliation
        lines.extend(
            [
                "Reconciliation:",
                f"  - nonzero positions: {reconciliation['nonzero_positions']}",
                f"  - total notional: {reconciliation['total_abs_notional_usdt']}",
                f"  - allowed: {_yes_no(bool(reconciliation['allowed']))}",
            ]
        )
        if reconciliation["rejection_reasons"]:
            lines.append(
                "  - rejection reasons: "
                + ", ".join(reconciliation["rejection_reasons"])
            )
    if report.order_plan:
        plan = report.order_plan
        lines.extend(
            [
                "Order plan:",
                f"  - symbol: {plan['symbol']}",
                f"  - side: {plan['side']}",
                f"  - quantity: {plan['quantity']}",
                f"  - notional: {plan['notional_usdt']}",
                f"  - leverage: {plan['configured_leverage']}",
                f"  - liquidation distance: {plan['liquidation_distance_percent']}",
                f"  - allowed: {_yes_no(bool(plan['allowed']))}",
            ]
        )
        if plan["rejection_reasons"]:
            lines.append("  - rejection reasons: " + ", ".join(plan["rejection_reasons"]))
        if plan["warnings"]:
            lines.append("  - warnings: " + ", ".join(plan["warnings"]))
    return "\n".join(lines)


async def _run_futures_dry_run_report(
    settings: Settings,
    *,
    symbols: Sequence[str],
    intent: FuturesOrderIntent | None,
    limits: FuturesRiskLimits,
) -> FuturesDryRunReport:
    client = BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    try:
        return await build_futures_dry_run_report(
            settings,
            client=client,
            symbols=symbols,
            intent=intent,
            limits=limits,
        )
    finally:
        await client.close()


async def _fetch_mark_prices(
    client: Any,
    exchange_symbols: Sequence[str],
) -> Mapping[str, Any] | Sequence[Mapping[str, Any]]:
    if len(exchange_symbols) == 1:
        return await client.request(
            "fapiPublicGetPremiumIndex",
            {"symbol": exchange_symbols[0]},
            request_weight=1,
        )
    return await client.request("fapiPublicGetPremiumIndex", request_weight=1)


def _parse_asset_balance(raw: Mapping[str, Any]) -> FuturesAssetBalance:
    return FuturesAssetBalance(
        asset=_asset(raw.get("asset") or ""),
        wallet_balance=_float(raw.get("walletBalance")),
        unrealized_profit=_float(raw.get("unrealizedProfit")),
        margin_balance=_float(raw.get("marginBalance")),
        maint_margin=_float(raw.get("maintMargin")),
        initial_margin=_float(raw.get("initialMargin")),
        position_initial_margin=_float(raw.get("positionInitialMargin")),
        open_order_initial_margin=_float(raw.get("openOrderInitialMargin")),
        cross_wallet_balance=_float(raw.get("crossWalletBalance")),
        cross_unrealized_pnl=_float(raw.get("crossUnPnl")),
        available_balance=_float(raw.get("availableBalance")),
        max_withdraw_amount=_float(raw.get("maxWithdrawAmount")),
    )


def _payload_items(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
    *,
    item_key: str | None = None,
) -> tuple[Mapping[str, Any], ...]:
    if isinstance(raw, Mapping):
        if item_key and isinstance(raw.get(item_key), Sequence):
            nested = raw[item_key]
            return tuple(
                item for item in nested if isinstance(item, Mapping)
            )
        return (raw,)
    if isinstance(raw, Sequence) and not isinstance(raw, (str, bytes)):
        return tuple(item for item in raw if isinstance(item, Mapping))
    return ()


def _filters_by_type(raw: object) -> dict[str, Mapping[str, Any]]:
    if not isinstance(raw, Sequence) or isinstance(raw, (str, bytes)):
        return {}
    filters: dict[str, Mapping[str, Any]] = {}
    for item in raw:
        if not isinstance(item, Mapping):
            continue
        filter_type = str(item.get("filterType") or "").strip().upper()
        if filter_type:
            filters[filter_type] = item
    return filters


def _single_symbol_params(exchange_symbols: Sequence[str]) -> dict[str, str]:
    return {"symbol": exchange_symbols[0]} if len(exchange_symbols) == 1 else {}


def _execution_price(
    order_type: str,
    requested_price: float | None,
    mark: FuturesMarkPrice,
) -> float:
    if order_type == ORDER_LIMIT:
        if requested_price is None:
            raise FuturesModelError("limit futures plan requires --price")
        return _positive_number(requested_price, "price")
    if mark.mark_price <= 0:
        raise FuturesModelError("mark price must be positive for market order modeling")
    return mark.mark_price


def _estimate_liquidation_price(
    *,
    side: str,
    price: float,
    leverage: float,
    maint_margin_ratio: float,
) -> float | None:
    if price <= 0 or leverage <= 0:
        return None
    margin_fraction = (1.0 / leverage) - maint_margin_ratio
    if margin_fraction <= 0:
        return price
    if side == SIDE_BUY:
        return price * (1.0 - margin_fraction)
    return price * (1.0 + margin_fraction)


def _liquidation_distance_percent(
    *,
    side: str,
    price: float,
    liquidation_price: float | None,
) -> float | None:
    if liquidation_price is None or price <= 0:
        return None
    if side == SIDE_BUY:
        return ((price - liquidation_price) / price) * 100.0
    return ((liquidation_price - price) / price) * 100.0


def _is_closing_position(
    position: FuturesPosition | None,
    side: str,
    quantity: float,
    position_side: str,
) -> bool:
    if position is None or quantity <= 0:
        return False
    amount = position.position_amt
    if position_side == POSITION_LONG:
        return side == SIDE_SELL and amount > 0
    if position_side == POSITION_SHORT:
        return side == SIDE_BUY and amount < 0
    if amount > 0:
        return side == SIDE_SELL
    if amount < 0:
        return side == SIDE_BUY
    return False


def _opens_short(side: str, position_side: str) -> bool:
    if position_side == POSITION_SHORT:
        return True
    return position_side == POSITION_BOTH and side == SIDE_SELL


def _validate_limits(limits: FuturesRiskLimits) -> None:
    _positive_number(limits.max_leverage, "max_leverage")
    _positive_number(limits.max_notional_usdt, "max_notional_usdt")
    percent = _positive_number(
        limits.max_account_notional_percent,
        "max_account_notional_percent",
    )
    if percent > 100:
        raise FuturesModelError("max_account_notional_percent must be <= 100")
    _positive_number(limits.max_initial_margin_usdt, "max_initial_margin_usdt")
    _non_negative_number(
        limits.min_free_balance_after_initial_margin_usdt,
        "min_free_balance_after_initial_margin_usdt",
    )
    _positive_number(
        limits.min_liquidation_distance_percent,
        "min_liquidation_distance_percent",
    )
    _non_negative_number(limits.max_abs_funding_rate, "max_abs_funding_rate")
    _non_negative_number(
        limits.max_abs_funding_payment_usdt,
        "max_abs_funding_payment_usdt",
    )
    _positive_number(
        limits.min_maintenance_buffer_percent,
        "min_maintenance_buffer_percent",
    )
    if not limits.allowed_margin_types:
        raise FuturesModelError("allowed_margin_types must not be empty")


def _side(value: str) -> str:
    normalized = value.strip().upper()
    if normalized not in {SIDE_BUY, SIDE_SELL}:
        raise FuturesModelError("side must be BUY or SELL")
    return normalized


def _position_side(value: str) -> str:
    normalized = value.strip().upper()
    if normalized not in {POSITION_BOTH, POSITION_LONG, POSITION_SHORT}:
        raise FuturesModelError("position_side must be BOTH, LONG, or SHORT")
    return normalized


def _order_type(value: str) -> str:
    normalized = value.strip().upper()
    if normalized not in {ORDER_MARKET, ORDER_LIMIT}:
        raise FuturesModelError("order_type must be MARKET or LIMIT")
    return normalized


def _margin_type(value: object) -> str:
    normalized = str(value or "").strip().upper()
    if normalized in {"CROSS", "CROSSED"}:
        return "CROSSED"
    if normalized == "ISOLATED":
        return "ISOLATED"
    return normalized or "UNKNOWN"


def _asset(value: object) -> str:
    text = str(value or "").strip().upper()
    if not text:
        raise FuturesModelError("asset must not be blank")
    return text


def _exchange_symbol(symbol: str) -> str:
    text = str(symbol or "").strip()
    if not text:
        raise FuturesModelError("symbol must not be blank")
    try:
        text = normalize_symbol(text)
    except ValueError:
        pass
    return re.sub(r"[^A-Za-z0-9]", "", text).upper()


def _is_multiple(value: float, step: float) -> bool:
    if step <= 0:
        return True
    decimal_value = Decimal(str(value))
    decimal_step = Decimal(str(step))
    remainder = decimal_value % decimal_step
    return remainder == 0


def _required(value: Any | None, message: str) -> Any:
    if value is None:
        raise FuturesModelError(message)
    return value


def _positive_number(value: float, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number <= 0:
        raise FuturesModelError(f"{field_name} must be greater than zero")
    return number


def _non_negative_number(value: float, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number < 0:
        raise FuturesModelError(f"{field_name} must be at least zero")
    return number


def _coerce_float(value: object, field_name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise FuturesModelError(f"{field_name} must be a number") from exc
    if not math.isfinite(number):
        raise FuturesModelError(f"{field_name} must be finite")
    return number


def _float(value: object) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return 0.0
    return number if math.isfinite(number) else 0.0


def _optional_positive_float(value: object) -> float | None:
    number = _float(value)
    return number if number > 0 else None


def _optional_int(value: object) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _optional_boolish(value: object) -> bool | None:
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in {"true", "1", "yes"}:
            return True
        if normalized in {"false", "0", "no"}:
            return False
    return None


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

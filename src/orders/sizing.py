"""Dynamic order sizing using portfolio state and exchange market rules."""

from __future__ import annotations

import math
from collections.abc import Mapping
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation, ROUND_DOWN
from typing import Any

from src.portfolio import PortfolioSnapshot


ORDER_SIZE_MODES = frozenset(
    {"fixed", "quote_percent", "equity_percent", "risk_percent"}
)


class OrderSizingError(ValueError):
    """Raised when an order cannot be sized safely."""


@dataclass(frozen=True, slots=True)
class OrderSizingConfig:
    """Runtime configuration used by :class:`OrderSizer`."""

    mode: str = "fixed"
    order_size_percent: float = 1.0
    min_order_notional_usd: float = 0.0
    max_order_notional_usd: float = 0.0
    min_balance_usdt: float = 0.0
    max_position_size_usd: float = 500.0

    @classmethod
    def from_settings(cls, settings: Any) -> "OrderSizingConfig":
        """Build a sizing config from the central Settings object."""
        return cls(
            mode=settings.order_size_mode,
            order_size_percent=settings.order_size_percent,
            min_order_notional_usd=settings.min_order_notional_usd,
            max_order_notional_usd=settings.max_order_notional_usd,
            min_balance_usdt=settings.min_balance_usdt,
            max_position_size_usd=settings.max_position_size_usd,
        )

    def __post_init__(self) -> None:
        mode = self.mode.strip().lower()
        object.__setattr__(self, "mode", mode)
        if mode not in ORDER_SIZE_MODES:
            raise OrderSizingError(
                "ORDER_SIZE_MODE must be one of: "
                + ", ".join(sorted(ORDER_SIZE_MODES))
            )
        order_size_percent = _finite_float(
            self.order_size_percent,
            "ORDER_SIZE_PERCENT",
            minimum=0.0,
        )
        object.__setattr__(self, "order_size_percent", order_size_percent)
        if mode != "fixed" and order_size_percent <= 0:
            raise OrderSizingError("ORDER_SIZE_PERCENT must be greater than zero")
        min_order_notional_usd = _finite_float(
            self.min_order_notional_usd,
            "MIN_ORDER_NOTIONAL_USD",
            minimum=0.0,
        )
        object.__setattr__(
            self,
            "min_order_notional_usd",
            min_order_notional_usd,
        )
        max_order_notional_usd = _finite_float(
            self.max_order_notional_usd,
            "MAX_ORDER_NOTIONAL_USD",
            minimum=0.0,
        )
        object.__setattr__(
            self,
            "max_order_notional_usd",
            max_order_notional_usd,
        )
        min_balance_usdt = _finite_float(
            self.min_balance_usdt,
            "MIN_BALANCE_USDT",
            minimum=0.0,
        )
        object.__setattr__(self, "min_balance_usdt", min_balance_usdt)
        max_position_size_usd = _finite_float(
            self.max_position_size_usd,
            "MAX_POSITION_SIZE_USD",
            minimum=0.0,
            exclusive_minimum=True,
        )
        object.__setattr__(self, "max_position_size_usd", max_position_size_usd)
        if (
            max_order_notional_usd > 0
            and max_order_notional_usd < min_order_notional_usd
        ):
            raise OrderSizingError(
                "MAX_ORDER_NOTIONAL_USD must be greater than or equal to "
                "MIN_ORDER_NOTIONAL_USD"
            )


@dataclass(frozen=True, slots=True)
class OrderSizingRequest:
    """Inputs needed to resolve a concrete base-asset order amount."""

    symbol: str
    side: str
    requested_amount: float
    price: float | None
    order_type: str


@dataclass(frozen=True, slots=True)
class SizedOrder:
    """Resolved order amount and supporting sizing context."""

    amount: float
    notional_usdt: float | None
    price: float | None
    mode: str
    original_amount: float
    portfolio: PortfolioSnapshot | None = None


@dataclass(frozen=True, slots=True)
class MarketRules:
    """Exchange precision and minimum-size constraints for one market."""

    min_amount: float = 0.0
    min_notional: float = 0.0
    amount_precision: int | float | None = None


class OrderSizer:
    """Resolve configured sizing modes into Binance-ready base quantities."""

    async def size_order(
        self,
        request: OrderSizingRequest,
        config: OrderSizingConfig,
        *,
        portfolio_service: Any | None = None,
        exchange: Any | None = None,
    ) -> SizedOrder:
        """Return a sized order amount after clamps, rounding, and checks."""
        symbol = _non_blank(request.symbol, "symbol")
        side = _normalize_side(request.side)
        requested_amount = _finite_float(
            request.requested_amount,
            "amount",
            minimum=0.0,
            exclusive_minimum=True,
        )
        price = _optional_positive_price(request.price)
        market_rules = _market_rules(exchange, symbol)
        portfolio: PortfolioSnapshot | None = None

        if config.mode != "fixed" and price is None:
            raise OrderSizingError(
                f"ORDER_SIZE_MODE={config.mode} requires a current market price"
            )

        if self._requires_portfolio(config, side=side):
            if price is None:
                raise OrderSizingError("balance-aware sizing requires a market price")
            portfolio = await self._snapshot(
                portfolio_service,
                symbol=symbol,
                price=price,
            )

        if config.mode == "fixed":
            amount = requested_amount
            target_notional = amount * price if price is not None else None
        else:
            assert price is not None
            assert portfolio is not None
            target_notional = self._target_notional(config, portfolio)
            if side == "buy":
                target_notional = min(
                    target_notional,
                    self._spendable_quote_balance(config, portfolio),
                )
            amount = target_notional / price

        if price is not None and config.max_order_notional_usd > 0:
            notional = amount * price
            if notional > config.max_order_notional_usd:
                amount = config.max_order_notional_usd / price

        amount = _round_amount_down(
            exchange,
            symbol=symbol,
            amount=amount,
            precision=market_rules.amount_precision,
        )
        _validate_min_amount(amount, market_rules)

        notional_usdt = amount * price if price is not None else None
        _validate_notional(notional_usdt, config, market_rules)
        if side == "buy" and portfolio is not None and notional_usdt is not None:
            spendable = self._spendable_quote_balance(config, portfolio)
            if notional_usdt > spendable + 1e-12:
                raise OrderSizingError(
                    "insufficient available balance: "
                    f"{notional_usdt:.8f} USDT exceeds spendable "
                    f"{spendable:.8f} USDT"
                )

        return SizedOrder(
            amount=amount,
            notional_usdt=notional_usdt,
            price=price,
            mode=config.mode,
            original_amount=requested_amount,
            portfolio=portfolio,
        )

    def _requires_portfolio(self, config: OrderSizingConfig, *, side: str) -> bool:
        return (
            config.mode != "fixed"
            or (side == "buy" and config.min_balance_usdt > 0)
        )

    async def _snapshot(
        self,
        portfolio_service: Any | None,
        *,
        symbol: str,
        price: float,
    ) -> PortfolioSnapshot:
        if portfolio_service is None:
            raise OrderSizingError("portfolio snapshot service is required")
        snapshot = getattr(portfolio_service, "snapshot", None)
        if snapshot is None or not callable(snapshot):
            raise OrderSizingError("portfolio snapshot service is invalid")
        return await snapshot(mark_prices={symbol: price})

    def _target_notional(
        self,
        config: OrderSizingConfig,
        portfolio: PortfolioSnapshot,
    ) -> float:
        percent = config.order_size_percent / 100.0
        if config.mode == "quote_percent":
            return portfolio.available_balance_usdt * percent
        if config.mode == "equity_percent":
            return portfolio.total_equity_usdt * percent
        if config.mode == "risk_percent":
            return config.max_position_size_usd * percent
        raise OrderSizingError(f"unsupported order size mode: {config.mode}")

    def _spendable_quote_balance(
        self,
        config: OrderSizingConfig,
        portfolio: PortfolioSnapshot,
    ) -> float:
        spendable = portfolio.available_balance_usdt - config.min_balance_usdt
        if spendable <= 0:
            raise OrderSizingError(
                "insufficient available balance: "
                f"{portfolio.available_balance_usdt:.8f} USDT is at or below "
                f"MIN_BALANCE_USDT={config.min_balance_usdt:.8f}"
            )
        return spendable


def _market_rules(exchange: Any | None, symbol: str) -> MarketRules:
    market = _find_market(exchange, symbol)
    if not isinstance(market, Mapping):
        return MarketRules()

    limits = market.get("limits") if isinstance(market.get("limits"), Mapping) else {}
    amount_limits = (
        limits.get("amount") if isinstance(limits.get("amount"), Mapping) else {}
    )
    cost_limits = limits.get("cost") if isinstance(limits.get("cost"), Mapping) else {}
    precision = (
        market.get("precision") if isinstance(market.get("precision"), Mapping) else {}
    )
    return MarketRules(
        min_amount=_optional_positive_float(amount_limits.get("min")) or 0.0,
        min_notional=_optional_positive_float(cost_limits.get("min")) or 0.0,
        amount_precision=_precision_value(precision.get("amount")),
    )


def _find_market(exchange: Any | None, symbol: str) -> Mapping[str, Any] | None:
    if exchange is None:
        return None
    market_method = getattr(exchange, "market", None)
    if market_method is not None and callable(market_method):
        try:
            market = market_method(symbol)
        except Exception:
            market = None
        if isinstance(market, Mapping):
            return market

    markets = getattr(exchange, "markets", None)
    if not isinstance(markets, Mapping):
        return None
    if isinstance(markets.get(symbol), Mapping):
        return markets[symbol]
    compact_symbol = _compact_symbol(symbol)
    for market in markets.values():
        if not isinstance(market, Mapping):
            continue
        market_symbol = str(market.get("symbol", "")).strip()
        if _compact_symbol(market_symbol) == compact_symbol:
            return market
    return None


def _round_amount_down(
    exchange: Any | None,
    *,
    symbol: str,
    amount: float,
    precision: int | float | None,
) -> float:
    if amount <= 0 or not math.isfinite(amount):
        raise OrderSizingError("sized amount must be greater than zero")

    amount_to_precision = getattr(exchange, "amount_to_precision", None)
    if amount_to_precision is not None and callable(amount_to_precision):
        try:
            rounded = float(amount_to_precision(symbol, amount))
        except Exception:
            rounded = None
        if rounded is not None and math.isfinite(rounded):
            return rounded

    if precision is None:
        return amount
    if isinstance(precision, bool):
        return amount
    if isinstance(precision, int):
        return _round_decimal_places(amount, precision)

    step = _optional_positive_float(precision)
    if step is None:
        return amount
    if step >= 1 and float(step).is_integer():
        return _round_decimal_places(amount, int(step))
    return math.floor(amount / step) * step


def _round_decimal_places(amount: float, places: int) -> float:
    if places < 0:
        return amount
    quantizer = Decimal("1").scaleb(-places)
    try:
        decimal_amount = Decimal(str(amount))
    except InvalidOperation as exc:
        raise OrderSizingError("sized amount must be numeric") from exc
    return float(decimal_amount.quantize(quantizer, rounding=ROUND_DOWN))


def _validate_min_amount(amount: float, market_rules: MarketRules) -> None:
    if amount <= 0 or not math.isfinite(amount):
        raise OrderSizingError("sized amount must be greater than zero")
    if market_rules.min_amount > 0 and amount + 1e-12 < market_rules.min_amount:
        raise OrderSizingError(
            "order amount below exchange lot size: "
            f"{amount:.12g} < {market_rules.min_amount:.12g}"
        )


def _validate_notional(
    notional: float | None,
    config: OrderSizingConfig,
    market_rules: MarketRules,
) -> None:
    if notional is None:
        return
    min_notional = max(config.min_order_notional_usd, market_rules.min_notional)
    if min_notional > 0 and notional + 1e-12 < min_notional:
        raise OrderSizingError(
            "order notional below minimum: "
            f"{notional:.8f} USDT < {min_notional:.8f} USDT"
        )
    if config.max_order_notional_usd > 0 and notional > (
        config.max_order_notional_usd + 1e-12
    ):
        raise OrderSizingError(
            "order notional above maximum: "
            f"{notional:.8f} USDT > {config.max_order_notional_usd:.8f} USDT"
        )


def _normalize_side(side: str) -> str:
    side_normalized = _non_blank(side, "side").lower()
    if side_normalized not in {"buy", "sell"}:
        raise OrderSizingError("side must be 'buy' or 'sell'")
    return side_normalized


def _optional_positive_price(price: float | None) -> float | None:
    if price is None:
        return None
    return _finite_float(price, "price", minimum=0.0, exclusive_minimum=True)


def _finite_float(
    value: Any,
    field_name: str,
    *,
    minimum: float,
    exclusive_minimum: bool = False,
) -> float:
    try:
        result = float(value)
    except (TypeError, ValueError) as exc:
        raise OrderSizingError(f"{field_name} must be a finite number") from exc
    if not math.isfinite(result):
        raise OrderSizingError(f"{field_name} must be a finite number")
    if exclusive_minimum and result <= minimum:
        raise OrderSizingError(f"{field_name} must be greater than {minimum:g}")
    if not exclusive_minimum and result < minimum:
        raise OrderSizingError(f"{field_name} must be at least {minimum:g}")
    return result


def _optional_positive_float(value: Any) -> float | None:
    try:
        result = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(result) or result <= 0:
        return None
    return result


def _precision_value(value: Any) -> int | float | None:
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    try:
        result = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(result) or result < 0:
        return None
    if result.is_integer():
        return int(result)
    return result


def _non_blank(value: Any, field_name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise OrderSizingError(f"{field_name} must not be blank")
    return value.strip()


def _compact_symbol(symbol: str) -> str:
    return "".join(character for character in symbol.upper() if character.isalnum())

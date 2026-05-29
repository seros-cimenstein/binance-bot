"""Dry-run Binance Prediction market planning and risk modeling.

Phase 28 is modeling only. This module parses local or injected read-only
market data and builds local plans; it intentionally has no live prediction
order, stake, claim, or settlement write path.
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

DIRECTION_UP = "UP"
DIRECTION_DOWN = "DOWN"
AVAILABLE_STATUSES = ("OPEN", "TRADING", "LIVE")
WRITE_ENDPOINTS_BLOCKED = (
    "predictionPostOrder",
    "predictionPostBet",
    "predictionPostClaim",
    "predictionPostSettle",
    "bapiPredictionPostOrder",
    "bapiPredictionPostBet",
)


class PredictionModelError(RuntimeError):
    """Raised when dry-run prediction modeling cannot proceed safely."""


@dataclass(frozen=True, slots=True)
class PredictionMarket:
    """One parsed prediction market or round."""

    market_id: str
    symbol: str
    underlying: str
    quote_asset: str
    status: str
    round_id: str
    lock_time: int | None
    close_time: int | None
    settlement_time: int | None
    lock_price: float | None
    mark_price: float | None
    pool_up_usdt: float
    pool_down_usdt: float
    total_pool_usdt: float
    payout_up: float | None
    payout_down: float | None
    source: str = "payload"

    def seconds_to_lock(self, now: int) -> int | None:
        return self.lock_time - now if self.lock_time is not None else None

    def seconds_to_settlement(self, now: int) -> int | None:
        target = self.settlement_time if self.settlement_time is not None else self.close_time
        return target - now if target is not None else None

    def is_available(self, now: int) -> bool:
        seconds_to_lock = self.seconds_to_lock(now)
        return self.status in AVAILABLE_STATUSES and (
            seconds_to_lock is None or seconds_to_lock > 0
        )

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class PredictionDiscovery:
    """Read-only prediction market discovery result."""

    timestamp: str
    markets: tuple[PredictionMarket, ...]
    live_trading_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def market(self, market_id_or_symbol: str) -> PredictionMarket | None:
        normalized = _market_key(market_id_or_symbol)
        symbol = _exchange_symbol(market_id_or_symbol)
        for market in self.markets:
            if _market_key(market.market_id) == normalized:
                return market
            if _market_key(market.round_id) == normalized:
                return market
            if market.symbol == symbol:
                return market
        return None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class PredictionRiskLimits:
    """Conservative local limits for prediction dry-run plans."""

    max_stake_usdt: float = 5.0
    max_total_stake_usdt: float = 10.0
    max_stake_bankroll_percent: float = 5.0
    min_seconds_to_lock: int = 60
    max_seconds_to_settlement: int = 3600
    min_total_pool_usdt: float = 25.0
    max_pool_imbalance_percent: float = 80.0
    max_settlement_risk_score: float = 50.0
    max_drawdown_percent: float = 10.0
    require_available_market: bool = True
    emergency_stop: bool = False

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class PredictionIntent:
    """Requested prediction stake modeling inputs."""

    market: str
    direction: str
    stake_usdt: float
    bankroll_usdt: float
    current_open_stake_usdt: float = 0.0
    current_drawdown_percent: float = 0.0
    emergency_stop: bool = False
    now: int | None = None


@dataclass(frozen=True, slots=True)
class PredictionPlan:
    """Dry-run prediction stake and risk result."""

    timestamp: str
    market_id: str
    symbol: str
    round_id: str
    direction: str
    stake_usdt: float
    bankroll_usdt: float
    current_open_stake_usdt: float
    projected_open_stake_usdt: float
    stake_bankroll_percent: float
    status: str
    seconds_to_lock: int | None
    seconds_to_settlement: int | None
    total_pool_usdt: float
    pool_imbalance_percent: float | None
    payout_estimate: float | None
    settlement_risk_score: float
    emergency_stop_active: bool
    market_available: bool
    allowed: bool
    rejection_reasons: tuple[str, ...] = ()
    warnings: tuple[str, ...] = ()
    market: dict[str, Any] = field(default_factory=dict)
    risk_limits: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class PredictionDryRunReport:
    """Full prediction dry-run report."""

    timestamp: str
    mode: str
    discovery: dict[str, Any]
    plan: dict[str, Any] | None = None
    audit: dict[str, Any] = field(default_factory=dict)
    live_trading_enabled: bool = False
    write_endpoints_used: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


async def discover_prediction_markets(
    client: Any | None = None,
    *,
    payload: Mapping[str, Any] | Sequence[Mapping[str, Any]] | None = None,
    symbols: Sequence[str] = (),
) -> PredictionDiscovery:
    """Discover prediction markets from injected client data or a local payload."""
    if payload is None:
        if client is None:
            raise PredictionModelError("prediction market data is required")
        payload = await client.request("predictionGetMarkets", request_weight=1)
    markets = parse_prediction_markets(payload, symbols=symbols)
    if not markets:
        raise PredictionModelError("prediction market data did not include any markets")
    return PredictionDiscovery(timestamp=_utc_timestamp(), markets=markets)


def parse_prediction_markets(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
    *,
    symbols: Sequence[str] = (),
) -> tuple[PredictionMarket, ...]:
    """Parse prediction market data from Binance-like or local JSON payloads."""
    wanted = {_exchange_symbol(symbol) for symbol in symbols}
    markets: list[PredictionMarket] = []
    for item in _payload_items(raw):
        symbol_value = _first_present(item, "symbol", "pair", "underlyingSymbol", "asset")
        if symbol_value is None:
            continue
        symbol = _exchange_symbol(symbol_value)
        if wanted and symbol not in wanted:
            continue
        market_id = str(
            _first_present(item, "marketId", "id", "roundId", "epoch") or symbol
        ).strip()
        round_id = str(_first_present(item, "roundId", "epoch", "round") or market_id).strip()
        lock_time = _timestamp(
            _first_present(item, "lockTime", "lockTimestamp", "lockDate", "startTime")
        )
        close_time = _timestamp(
            _first_present(item, "closeTime", "closeTimestamp", "endTime")
        )
        settlement_time = _timestamp(
            _first_present(item, "settlementTime", "settleTime", "settleTimestamp")
        )
        pool_up = _float(_first_present(item, "poolUp", "bullAmount", "upPool", "longPool"))
        pool_down = _float(
            _first_present(item, "poolDown", "bearAmount", "downPool", "shortPool")
        )
        total_pool = _float(_first_present(item, "totalPool", "totalAmount", "pool"))
        if total_pool <= 0:
            total_pool = pool_up + pool_down
        markets.append(
            PredictionMarket(
                market_id=market_id,
                symbol=symbol,
                underlying=_asset(_first_present(item, "underlying", "baseAsset") or symbol),
                quote_asset=_asset(_first_present(item, "quoteAsset", "marginAsset") or "USDT"),
                status=str(_first_present(item, "status", "state") or "UNKNOWN").upper(),
                round_id=round_id,
                lock_time=lock_time,
                close_time=close_time,
                settlement_time=settlement_time,
                lock_price=_optional_positive_float(
                    _first_present(item, "lockPrice", "lockedPrice")
                ),
                mark_price=_optional_positive_float(
                    _first_present(item, "markPrice", "currentPrice", "price")
                ),
                pool_up_usdt=pool_up,
                pool_down_usdt=pool_down,
                total_pool_usdt=total_pool,
                payout_up=_optional_positive_float(
                    _first_present(item, "payoutUp", "bullPayout", "upPayout")
                ),
                payout_down=_optional_positive_float(
                    _first_present(item, "payoutDown", "bearPayout", "downPayout")
                ),
                source=str(_first_present(item, "source") or "payload"),
            )
        )
    return tuple(markets)


def build_prediction_plan(
    discovery: PredictionDiscovery,
    intent: PredictionIntent,
    *,
    limits: PredictionRiskLimits | None = None,
) -> PredictionPlan:
    """Build a dry-run prediction stake plan without submitting anything."""
    limits = limits or PredictionRiskLimits()
    _validate_limits(limits)
    now = intent.now if intent.now is not None else _unix_timestamp()
    market = _required(
        discovery.market(intent.market),
        f"prediction market not found for {intent.market}",
    )
    direction = _direction(intent.direction)
    stake = _positive_number(intent.stake_usdt, "stake_usdt")
    bankroll = _positive_number(intent.bankroll_usdt, "bankroll_usdt")
    open_stake = _non_negative_number(
        intent.current_open_stake_usdt,
        "current_open_stake_usdt",
    )
    drawdown = _non_negative_number(
        intent.current_drawdown_percent,
        "current_drawdown_percent",
    )
    projected_open_stake = open_stake + stake
    stake_bankroll_percent = (stake / bankroll) * 100.0
    seconds_to_lock = market.seconds_to_lock(now)
    seconds_to_settlement = market.seconds_to_settlement(now)
    pool_imbalance = _pool_imbalance_percent(market)
    payout = market.payout_up if direction == DIRECTION_UP else market.payout_down
    settlement_risk = _settlement_risk_score(
        market,
        seconds_to_lock=seconds_to_lock,
        seconds_to_settlement=seconds_to_settlement,
        pool_imbalance_percent=pool_imbalance,
    )
    emergency_stop_active = intent.emergency_stop or limits.emergency_stop
    market_available = market.is_available(now)

    rejections: list[str] = []
    warnings: list[str] = []
    if emergency_stop_active:
        rejections.append("emergency stop is active")
    if limits.require_available_market and not market_available:
        rejections.append("prediction market is not available")
    if stake > limits.max_stake_usdt:
        rejections.append("stake exceeds max_stake_usdt")
    if projected_open_stake > limits.max_total_stake_usdt:
        rejections.append("projected open stake exceeds max_total_stake_usdt")
    if stake_bankroll_percent > limits.max_stake_bankroll_percent:
        rejections.append("stake exceeds bankroll percent limit")
    if drawdown > limits.max_drawdown_percent:
        rejections.append("current drawdown exceeds configured maximum")
    if seconds_to_lock is None:
        rejections.append("market lock time is unavailable")
    elif seconds_to_lock < limits.min_seconds_to_lock:
        rejections.append("market is too close to lock")
    if seconds_to_settlement is None:
        rejections.append("market settlement time is unavailable")
    elif seconds_to_settlement <= 0:
        rejections.append("market settlement window has elapsed")
    elif seconds_to_settlement > limits.max_seconds_to_settlement:
        rejections.append("settlement window exceeds configured maximum")
    if market.total_pool_usdt < limits.min_total_pool_usdt:
        rejections.append("market liquidity is below minimum")
    if (
        pool_imbalance is not None
        and pool_imbalance > limits.max_pool_imbalance_percent
    ):
        rejections.append("market pool imbalance exceeds configured maximum")
    if settlement_risk > limits.max_settlement_risk_score:
        rejections.append("settlement risk exceeds configured maximum")
    if payout is None:
        warnings.append("payout estimate is unavailable")

    return PredictionPlan(
        timestamp=_utc_timestamp(),
        market_id=market.market_id,
        symbol=market.symbol,
        round_id=market.round_id,
        direction=direction,
        stake_usdt=stake,
        bankroll_usdt=bankroll,
        current_open_stake_usdt=open_stake,
        projected_open_stake_usdt=projected_open_stake,
        stake_bankroll_percent=stake_bankroll_percent,
        status=market.status,
        seconds_to_lock=seconds_to_lock,
        seconds_to_settlement=seconds_to_settlement,
        total_pool_usdt=market.total_pool_usdt,
        pool_imbalance_percent=pool_imbalance,
        payout_estimate=payout,
        settlement_risk_score=settlement_risk,
        emergency_stop_active=emergency_stop_active,
        market_available=market_available,
        allowed=not rejections,
        rejection_reasons=tuple(rejections),
        warnings=tuple(warnings),
        market=market.as_dict(),
        risk_limits=limits.as_dict(),
    )


async def build_prediction_dry_run_report(
    settings: Settings,
    *,
    client: Any | None = None,
    payload: Mapping[str, Any] | Sequence[Mapping[str, Any]] | None = None,
    symbols: Sequence[str] = (),
    intent: PredictionIntent | None = None,
    limits: PredictionRiskLimits | None = None,
) -> PredictionDryRunReport:
    """Build a full prediction dry-run report from read-only local data."""
    discovery = await discover_prediction_markets(client, payload=payload, symbols=symbols)
    plan = build_prediction_plan(discovery, intent, limits=limits) if intent else None
    audit = _audit(discovery, plan)
    return PredictionDryRunReport(
        timestamp=_utc_timestamp(),
        mode=_mode(settings),
        discovery=discovery.as_dict(),
        plan=plan.as_dict() if plan else None,
        audit=audit,
    )


def assert_prediction_live_disabled(*, confirm_live_prediction: bool = False) -> None:
    """Fail closed for any attempted live prediction activation."""
    if confirm_live_prediction:
        raise PredictionModelError(
            "live prediction activation is not implemented; Phase 28 is dry-run only"
        )


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run read-only prediction market parsing and local risk modeling."
    )
    parser.add_argument("--dry-run", action="store_true", help="required")
    parser.add_argument(
        "--confirm-live-prediction",
        action="store_true",
        help="always fails; live prediction activation is not implemented",
    )
    parser.add_argument("--market-data-file", help="local prediction market JSON payload")
    parser.add_argument("--symbol", help="prediction symbol, for example BTC/USDT")
    parser.add_argument("--market", help="market id, round id, or symbol to model")
    parser.add_argument("--direction", choices=(DIRECTION_UP, DIRECTION_DOWN))
    parser.add_argument("--stake-usdt", type=float)
    parser.add_argument("--bankroll-usdt", type=float)
    parser.add_argument("--current-open-stake-usdt", type=float, default=0.0)
    parser.add_argument("--current-drawdown-percent", type=float, default=0.0)
    parser.add_argument("--emergency-stop", action="store_true")
    parser.add_argument("--max-stake-usdt", type=float, default=PredictionRiskLimits.max_stake_usdt)
    parser.add_argument(
        "--max-total-stake-usdt",
        type=float,
        default=PredictionRiskLimits.max_total_stake_usdt,
    )
    parser.add_argument(
        "--max-stake-bankroll-percent",
        type=float,
        default=PredictionRiskLimits.max_stake_bankroll_percent,
    )
    parser.add_argument(
        "--min-seconds-to-lock",
        type=int,
        default=PredictionRiskLimits.min_seconds_to_lock,
    )
    parser.add_argument(
        "--max-seconds-to-settlement",
        type=int,
        default=PredictionRiskLimits.max_seconds_to_settlement,
    )
    parser.add_argument(
        "--min-total-pool-usdt",
        type=float,
        default=PredictionRiskLimits.min_total_pool_usdt,
    )
    parser.add_argument(
        "--max-pool-imbalance-percent",
        type=float,
        default=PredictionRiskLimits.max_pool_imbalance_percent,
    )
    parser.add_argument(
        "--max-settlement-risk-score",
        type=float,
        default=PredictionRiskLimits.max_settlement_risk_score,
    )
    parser.add_argument(
        "--max-drawdown-percent",
        type=float,
        default=PredictionRiskLimits.max_drawdown_percent,
    )
    parser.add_argument("--json", action="store_true", help="print JSON output")
    args = parser.parse_args(argv)

    if not args.dry_run:
        print("prediction check failed: --dry-run is required; live prediction is disabled")
        return 1
    try:
        assert_prediction_live_disabled(
            confirm_live_prediction=args.confirm_live_prediction
        )
        payload = _load_json_payload(args.market_data_file)
        settings = Settings.from_env()
        symbol = normalize_symbol(args.symbol or settings.trading_symbol)
        limits = PredictionRiskLimits(
            max_stake_usdt=args.max_stake_usdt,
            max_total_stake_usdt=args.max_total_stake_usdt,
            max_stake_bankroll_percent=args.max_stake_bankroll_percent,
            min_seconds_to_lock=args.min_seconds_to_lock,
            max_seconds_to_settlement=args.max_seconds_to_settlement,
            min_total_pool_usdt=args.min_total_pool_usdt,
            max_pool_imbalance_percent=args.max_pool_imbalance_percent,
            max_settlement_risk_score=args.max_settlement_risk_score,
            max_drawdown_percent=args.max_drawdown_percent,
        )
        intent = (
            PredictionIntent(
                market=args.market or symbol,
                direction=args.direction,
                stake_usdt=args.stake_usdt,
                bankroll_usdt=args.bankroll_usdt,
                current_open_stake_usdt=args.current_open_stake_usdt,
                current_drawdown_percent=args.current_drawdown_percent,
                emergency_stop=args.emergency_stop,
            )
            if args.direction is not None
            or args.stake_usdt is not None
            or args.bankroll_usdt is not None
            else None
        )
        report = asyncio.run(
            build_prediction_dry_run_report(
                settings,
                payload=payload,
                symbols=(symbol,),
                intent=intent,
                limits=limits,
            )
        )
        if args.json:
            print(json.dumps(report.as_dict(), indent=2, sort_keys=True))
        else:
            print(render_prediction_report(report))
    except Exception as exc:
        print(f"prediction check failed: {exc}")
        return 1
    return 0


def render_prediction_report(report: PredictionDryRunReport) -> str:
    """Render a concise human-readable prediction dry-run report."""
    discovery = report.discovery
    markets = discovery.get("markets", ()) if isinstance(discovery, Mapping) else ()
    lines = [
        f"Mode: {report.mode}",
        "Live prediction trading: disabled",
        "Write endpoints used: none",
        f"Markets discovered: {len(markets) if isinstance(markets, Sequence) else 0}",
    ]
    if report.audit:
        lines.extend(
            [
                "Audit:",
                f"  - market data available: {_yes_no(bool(report.audit['market_data_available']))}",
                f"  - allowed: {_yes_no(bool(report.audit['allowed']))}",
            ]
        )
        if report.audit["rejection_reasons"]:
            lines.append(
                "  - rejection reasons: "
                + ", ".join(report.audit["rejection_reasons"])
            )
    if report.plan:
        plan = report.plan
        lines.extend(
            [
                "Prediction plan:",
                f"  - market: {plan['market_id']}",
                f"  - symbol: {plan['symbol']}",
                f"  - direction: {plan['direction']}",
                f"  - stake USDT: {plan['stake_usdt']}",
                f"  - seconds to lock: {plan['seconds_to_lock']}",
                f"  - settlement risk score: {plan['settlement_risk_score']}",
                f"  - emergency stop: {_yes_no(bool(plan['emergency_stop_active']))}",
                f"  - allowed: {_yes_no(bool(plan['allowed']))}",
            ]
        )
        if plan["rejection_reasons"]:
            lines.append("  - rejection reasons: " + ", ".join(plan["rejection_reasons"]))
        if plan["warnings"]:
            lines.append("  - warnings: " + ", ".join(plan["warnings"]))
    return "\n".join(lines)


def _audit(
    discovery: PredictionDiscovery,
    plan: PredictionPlan | None,
) -> dict[str, Any]:
    rejections = tuple(plan.rejection_reasons) if plan else ()
    return {
        "timestamp": _utc_timestamp(),
        "market_data_available": bool(discovery.markets),
        "markets_discovered": len(discovery.markets),
        "live_trading_enabled": False,
        "write_endpoints_used": (),
        "allowed": bool(plan.allowed) if plan else bool(discovery.markets),
        "rejection_reasons": rejections,
    }


def _payload_items(
    raw: Mapping[str, Any] | Sequence[Mapping[str, Any]],
) -> tuple[Mapping[str, Any], ...]:
    if isinstance(raw, Mapping):
        for key in ("markets", "data", "items", "rows", "list"):
            nested = raw.get(key)
            if isinstance(nested, Sequence) and not isinstance(nested, (str, bytes)):
                return tuple(item for item in nested if isinstance(item, Mapping))
        return (raw,)
    if isinstance(raw, Sequence) and not isinstance(raw, (str, bytes)):
        return tuple(item for item in raw if isinstance(item, Mapping))
    return ()


def _first_present(item: Mapping[str, Any], *keys: str) -> Any:
    for key in keys:
        value = item.get(key)
        if value is not None and value != "":
            return value
    return None


def _settlement_risk_score(
    market: PredictionMarket,
    *,
    seconds_to_lock: int | None,
    seconds_to_settlement: int | None,
    pool_imbalance_percent: float | None,
) -> float:
    score = 0.0
    if seconds_to_lock is None:
        score += 30.0
    elif seconds_to_lock < 60:
        score += 25.0
    elif seconds_to_lock < 300:
        score += 10.0
    if seconds_to_settlement is None:
        score += 30.0
    elif seconds_to_settlement > 3600:
        score += 15.0
    elif seconds_to_settlement <= 0:
        score += 50.0
    if market.lock_price is None or market.mark_price is None:
        score += 10.0
    if market.total_pool_usdt <= 0:
        score += 25.0
    if pool_imbalance_percent is not None:
        score += min(25.0, pool_imbalance_percent / 4.0)
    return min(100.0, score)


def _pool_imbalance_percent(market: PredictionMarket) -> float | None:
    total = market.total_pool_usdt
    if total <= 0:
        return None
    return (abs(market.pool_up_usdt - market.pool_down_usdt) / total) * 100.0


def _validate_limits(limits: PredictionRiskLimits) -> None:
    _positive_number(limits.max_stake_usdt, "max_stake_usdt")
    _positive_number(limits.max_total_stake_usdt, "max_total_stake_usdt")
    _positive_number(limits.max_stake_bankroll_percent, "max_stake_bankroll_percent")
    _non_negative_number(limits.min_seconds_to_lock, "min_seconds_to_lock")
    _positive_number(limits.max_seconds_to_settlement, "max_seconds_to_settlement")
    _non_negative_number(limits.min_total_pool_usdt, "min_total_pool_usdt")
    _positive_number(limits.max_pool_imbalance_percent, "max_pool_imbalance_percent")
    _positive_number(limits.max_settlement_risk_score, "max_settlement_risk_score")
    _non_negative_number(limits.max_drawdown_percent, "max_drawdown_percent")


def _load_json_payload(path: str | None) -> Mapping[str, Any] | Sequence[Mapping[str, Any]]:
    if not path:
        raise PredictionModelError("local --market-data-file is required")
    with Path(path).open("r", encoding="utf-8") as handle:
        payload = json.load(handle)
    if not isinstance(payload, Mapping) and not (
        isinstance(payload, Sequence) and not isinstance(payload, (str, bytes))
    ):
        raise PredictionModelError("prediction market JSON must be an object or array")
    return payload


def _direction(value: str | None) -> str:
    normalized = str(value or "").strip().upper()
    if normalized not in {DIRECTION_UP, DIRECTION_DOWN}:
        raise PredictionModelError("direction must be UP or DOWN")
    return normalized


def _market_key(value: object) -> str:
    return re.sub(r"[^A-Za-z0-9]", "", str(value or "")).upper()


def _exchange_symbol(symbol: object) -> str:
    text = str(symbol or "").strip()
    if not text:
        raise PredictionModelError("symbol must not be blank")
    try:
        text = normalize_symbol(text)
    except ValueError:
        pass
    return re.sub(r"[^A-Za-z0-9]", "", text).upper()


def _asset(value: object) -> str:
    text = str(value or "").strip().upper()
    if not text:
        raise PredictionModelError("asset must not be blank")
    return text


def _positive_number(value: object, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number <= 0:
        raise PredictionModelError(f"{field_name} must be greater than zero")
    return number


def _non_negative_number(value: object, field_name: str) -> float:
    number = _coerce_float(value, field_name)
    if number < 0:
        raise PredictionModelError(f"{field_name} must be at least zero")
    return number


def _coerce_float(value: object, field_name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise PredictionModelError(f"{field_name} must be a number") from exc
    if not math.isfinite(number):
        raise PredictionModelError(f"{field_name} must be finite")
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


def _timestamp(value: object) -> int | None:
    if value is None or value == "":
        return None
    if isinstance(value, str) and not value.strip().replace(".", "", 1).isdigit():
        try:
            parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            return None
        return int(parsed.timestamp())
    number = _float(value)
    if number <= 0:
        return None
    if number > 10_000_000_000:
        number = number / 1000.0
    return int(number)


def _required(value: Any | None, message: str) -> Any:
    if value is None:
        raise PredictionModelError(message)
    return value


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


def _unix_timestamp() -> int:
    return int(datetime.now(timezone.utc).timestamp())


if __name__ == "__main__":
    raise SystemExit(run())

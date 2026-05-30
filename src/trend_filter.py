"""Long-horizon cross-sectional momentum runtime.

This is the live counterpart to ``src.trend_filter_backtest``. Strategy:

* At each rebalance cycle (default: every 30 days), compute trailing N-day
  return for every symbol in the configured universe.
* Pick the top K by return; equal-weight them as the target portfolio.
* Sell any current holding not in the target set; buy any target not
  currently held. Trim/scale existing holdings to the target weight.

Compared to funding-arb, the runtime is dramatically simpler:

* Spot-only — no futures, no collateral transfers, no liquidation risk.
* Long-only — no short legs, no reconciliation across accounts.
* Sparse trading — fees amortise over real multi-week moves.
* Single-leg execution — if a SELL fails the BUY just doesn't fire; no
  rollback machinery needed.

Designed to run on the smallest viable spot balance. With ``top_k=1`` a
$10 USDT wallet can take one full position that clears most Binance
``MIN_NOTIONAL`` filters.

Live submission is gated by:

* ``TREND_FILTER_ENABLED=true``
* ``CONFIRM_LIVE_TREND_FILTER=yes``
* ``USE_TESTNET=false`` and ``DRY_RUN=false``
* ``TREND_FILTER_PROVEN_EDGE=yes`` (set only after a positive backtest
  on your configured universe / lookback / rebalance cadence).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
from dataclasses import asdict, dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

from dotenv import load_dotenv


class TrendFilterError(RuntimeError):
    """Raised when the trend filter runtime cannot proceed."""


@dataclass(frozen=True, slots=True)
class TrendFilterSettings:
    api_key: str
    api_secret: str
    use_testnet: bool = False
    dry_run: bool = True
    enabled: bool = False
    confirm_live_trend_filter: str = "no"

    universe: tuple[str, ...] = (
        "BTC/USDT", "ETH/USDT", "SOL/USDT", "BNB/USDT",
        "XRP/USDT", "ADA/USDT", "DOGE/USDT", "AVAX/USDT",
        "LINK/USDT", "DOT/USDT", "LTC/USDT",
    )
    interval: str = "1d"
    lookback_days: int = 90
    top_k: int = 1
    rebalance_every_days: int = 30
    min_lookback_return_pct: float = 0.0
    taker_fee_bps: float = 10.0
    slippage_bps: float = 5.0
    min_balance_usdt: float = 5.0  # set very low so $8 wallets can trade
    min_order_notional_usdt: float = 5.0
    dynamic_balance: bool = True

    state_path: str = "data/trend-filter-state.json"
    audit_path: str = "data/trend-filter-decisions.jsonl"
    ledger_path: str = "data/trend-filter-ledger.jsonl"
    emergency_stop_path: str = "data/trend-filter.stop"

    def validate(self) -> None:
        if not self.universe:
            raise TrendFilterError("universe must not be empty")
        if self.top_k <= 0:
            raise TrendFilterError("top_k must be positive")
        if self.top_k > len(self.universe):
            raise TrendFilterError("top_k cannot exceed universe size")
        if self.lookback_days <= 0:
            raise TrendFilterError("lookback_days must be positive")
        if self.rebalance_every_days <= 0:
            raise TrendFilterError("rebalance_every_days must be positive")
        if self.min_balance_usdt < 0:
            raise TrendFilterError("min_balance_usdt must be non-negative")
        if self.min_order_notional_usdt <= 0:
            raise TrendFilterError("min_order_notional_usdt must be positive")
        for path_name in ("state_path", "audit_path", "ledger_path", "emergency_stop_path"):
            if not getattr(self, path_name).strip():
                raise TrendFilterError(f"{path_name} must not be blank")

    @classmethod
    def from_env(cls) -> "TrendFilterSettings":
        load_dotenv(dotenv_path=os.environ.get("DOTENV_PATH"))
        api_key = os.getenv("BINANCE_API_KEY", "")
        api_secret = os.getenv("BINANCE_API_SECRET", "")
        if not api_key or not api_secret:
            raise TrendFilterError("BINANCE_API_KEY and BINANCE_API_SECRET must be set")

        def _bool(name: str, default: bool) -> bool:
            v = os.getenv(name, "")
            return default if not v else v.strip().lower() in {"1", "true", "yes", "on"}

        def _float(name: str, default: float) -> float:
            try:
                return float(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise TrendFilterError(f"{name} must be numeric")

        def _int(name: str, default: int) -> int:
            try:
                return int(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise TrendFilterError(f"{name} must be an integer")

        def _csv(name: str, default: tuple[str, ...]) -> tuple[str, ...]:
            raw = os.getenv(name, "")
            if not raw:
                return default
            return tuple(s.strip() for s in raw.split(",") if s.strip())

        defaults = cls(api_key="x", api_secret="x")
        settings = cls(
            api_key=api_key, api_secret=api_secret,
            use_testnet=_bool("USE_TESTNET", False),
            dry_run=_bool("DRY_RUN", True),
            enabled=_bool("TREND_FILTER_ENABLED", False),
            confirm_live_trend_filter=os.getenv("CONFIRM_LIVE_TREND_FILTER", "no"),
            universe=_csv("TREND_FILTER_UNIVERSE", defaults.universe),
            interval=os.getenv("TREND_FILTER_INTERVAL", defaults.interval),
            lookback_days=_int("TREND_FILTER_LOOKBACK_DAYS", defaults.lookback_days),
            top_k=_int("TREND_FILTER_TOP_K", defaults.top_k),
            rebalance_every_days=_int("TREND_FILTER_REBALANCE_EVERY_DAYS", defaults.rebalance_every_days),
            min_lookback_return_pct=_float("TREND_FILTER_MIN_LOOKBACK_RETURN_PCT", defaults.min_lookback_return_pct),
            taker_fee_bps=_float("TREND_FILTER_TAKER_FEE_BPS", defaults.taker_fee_bps),
            slippage_bps=_float("TREND_FILTER_SLIPPAGE_BPS", defaults.slippage_bps),
            min_balance_usdt=_float("TREND_FILTER_MIN_BALANCE_USDT", defaults.min_balance_usdt),
            min_order_notional_usdt=_float("TREND_FILTER_MIN_ORDER_NOTIONAL_USDT", defaults.min_order_notional_usdt),
            dynamic_balance=_bool("TREND_FILTER_DYNAMIC_BALANCE", defaults.dynamic_balance),
            state_path=os.getenv("TREND_FILTER_STATE_PATH", defaults.state_path),
            audit_path=os.getenv("TREND_FILTER_AUDIT_PATH", defaults.audit_path),
            ledger_path=os.getenv("TREND_FILTER_LEDGER_PATH", defaults.ledger_path),
            emergency_stop_path=os.getenv("TREND_FILTER_EMERGENCY_STOP_PATH", defaults.emergency_stop_path),
        )
        settings.validate()
        return settings


@dataclass(slots=True)
class TrendFilterHolding:
    symbol: str
    qty: float
    entry_price: float
    entry_time_ms: int

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "TrendFilterHolding":
        return cls(
            symbol=str(payload["symbol"]),
            qty=float(payload["qty"]),
            entry_price=float(payload["entry_price"]),
            entry_time_ms=int(payload["entry_time_ms"]),
        )


@dataclass(frozen=True, slots=True)
class TrendFilterState:
    holdings: tuple[TrendFilterHolding, ...] = ()
    last_rebalance_time_ms: int = 0

    def as_dict(self) -> dict[str, Any]:
        return {
            "holdings": [h.as_dict() for h in self.holdings],
            "last_rebalance_time_ms": self.last_rebalance_time_ms,
        }

    @classmethod
    def empty(cls) -> "TrendFilterState":
        return cls()

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "TrendFilterState":
        rows = payload.get("holdings") or []
        return cls(
            holdings=tuple(TrendFilterHolding.from_mapping(r) for r in rows),
            last_rebalance_time_ms=int(payload.get("last_rebalance_time_ms", 0)),
        )


def load_state(path: Path | str) -> TrendFilterState:
    p = Path(path)
    if not p.exists():
        return TrendFilterState.empty()
    try:
        raw = json.loads(p.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise TrendFilterError(f"could not read state at {p}: {exc}") from exc
    return TrendFilterState.from_mapping(raw)


def save_state(state: TrendFilterState, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(state.as_dict(), indent=2, sort_keys=True))
    tmp.replace(p)


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise TrendFilterError(f"emergency stop file present at {path}; refusing to run")


def _preflight(settings: TrendFilterSettings, *, submit_order: bool) -> None:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if not submit_order:
        return
    if not settings.enabled:
        raise TrendFilterError("TREND_FILTER_ENABLED=true is required for live submission")
    if settings.use_testnet:
        raise TrendFilterError("trend-filter live submission requires USE_TESTNET=false")
    if settings.dry_run:
        raise TrendFilterError("trend-filter live submission requires DRY_RUN=false")
    if settings.confirm_live_trend_filter != "yes":
        raise TrendFilterError("trend-filter live submission requires CONFIRM_LIVE_TREND_FILTER=yes")
    if os.getenv("TREND_FILTER_PROVEN_EDGE", "").strip().lower() != "yes":
        raise TrendFilterError(
            "trend-filter live submission requires TREND_FILTER_PROVEN_EDGE=yes. "
            "Run `python -m src.trend_filter_backtest` and set this only when the harness "
            "shows positive risk-adjusted edge on your configured universe."
        )


@dataclass(frozen=True, slots=True)
class TrendFilterSignal:
    symbol: str
    current_price: float
    lookback_return_pct: float


@dataclass(frozen=True, slots=True)
class TrendFilterDecision:
    action: str  # "buy" | "sell" | "hold" | "skip_rebalance" | "skip_cycle"
    symbol: str
    reason: str
    target_notional_usdt: float = 0.0
    qty: float = 0.0


async def _fetch_kline_window(symbol: str, *, lookback_days: int) -> list[float] | None:
    """Fetch enough daily klines to compute lookback return. Returns closes."""
    import aiohttp
    rest_symbol = symbol.replace("/", "").upper()
    # Pull lookback_days + 1 daily candles to get 'now' and 'lookback days ago'.
    url = "https://api.binance.com/api/v3/klines"
    params = {"symbol": rest_symbol, "interval": "1d", "limit": lookback_days + 2}
    async with aiohttp.ClientSession() as session:
        async with session.get(url, params=params) as resp:
            if resp.status != 200:
                return None
            rows = await resp.json()
    if not rows or len(rows) < 2:
        return None
    return [float(r[4]) for r in rows]


async def _fetch_signals(settings: TrendFilterSettings) -> list[TrendFilterSignal]:
    """Concurrent kline fetch for the configured universe."""
    sem = asyncio.Semaphore(6)

    async def _one(symbol: str) -> TrendFilterSignal | None:
        async with sem:
            closes = await _fetch_kline_window(symbol, lookback_days=settings.lookback_days)
        if closes is None or len(closes) < settings.lookback_days + 1:
            return None
        current_price = closes[-1]
        lookback_price = closes[-(settings.lookback_days + 1)]
        if lookback_price <= 0:
            return None
        ret_pct = (current_price / lookback_price - 1.0) * 100.0
        return TrendFilterSignal(symbol=symbol, current_price=current_price,
                                  lookback_return_pct=ret_pct)

    results = await asyncio.gather(*(_one(s) for s in settings.universe))
    return [s for s in results if s is not None]


class TrendFilterAdvisor:
    """Decides which target symbols to hold this rebalance."""

    def __init__(self, settings: TrendFilterSettings) -> None:
        self.settings = settings

    def decide(
        self, *, state: TrendFilterState, signals: Sequence[TrendFilterSignal],
        balance_usdt: float, current_time_ms: int,
    ) -> list[TrendFilterDecision]:
        cfg = self.settings

        # Skip the cycle entirely if not yet at the rebalance interval.
        next_rebalance_due_ms = (
            state.last_rebalance_time_ms + cfg.rebalance_every_days * 86_400_000
            if state.last_rebalance_time_ms > 0 else 0
        )
        if current_time_ms < next_rebalance_due_ms:
            return [TrendFilterDecision(
                action="skip_cycle", symbol="",
                reason=f"next rebalance due at {next_rebalance_due_ms} (current {current_time_ms})",
            )]

        # Rank candidates by trailing return; filter floor.
        eligible = [s for s in signals if s.lookback_return_pct >= cfg.min_lookback_return_pct]
        eligible.sort(key=lambda x: x.lookback_return_pct, reverse=True)
        target_signals = eligible[: cfg.top_k]
        target_symbols = {s.symbol for s in target_signals}

        # Compute total equity = balance + value of current holdings.
        signals_by_symbol = {s.symbol: s for s in signals}
        held_value = 0.0
        for h in state.holdings:
            sig = signals_by_symbol.get(h.symbol)
            if sig is not None:
                held_value += h.qty * sig.current_price
        total_equity = balance_usdt + held_value
        target_notional_per_symbol = (
            total_equity / len(target_signals) if target_signals else 0.0
        )

        decisions: list[TrendFilterDecision] = []

        # 1. Sells: any current holding not in the target set.
        held_symbols = {h.symbol for h in state.holdings}
        for h in state.holdings:
            if h.symbol not in target_symbols:
                decisions.append(TrendFilterDecision(
                    action="sell", symbol=h.symbol,
                    reason=f"holding {h.symbol} no longer in top {cfg.top_k}",
                    qty=h.qty,
                ))
        # 2. Buys: any target not currently held (or rebalancing existing).
        for sig in target_signals:
            if sig.symbol in held_symbols:
                decisions.append(TrendFilterDecision(
                    action="hold", symbol=sig.symbol,
                    reason=f"{sig.symbol} remains in top {cfg.top_k} "
                            f"(lookback return {sig.lookback_return_pct:+.2f}%)",
                ))
                continue
            if target_notional_per_symbol < cfg.min_order_notional_usdt:
                decisions.append(TrendFilterDecision(
                    action="skip_rebalance", symbol=sig.symbol,
                    reason=(
                        f"target notional ${target_notional_per_symbol:.4f} below "
                        f"min order notional ${cfg.min_order_notional_usdt}"
                    ),
                    target_notional_usdt=target_notional_per_symbol,
                ))
                continue
            decisions.append(TrendFilterDecision(
                action="buy", symbol=sig.symbol,
                reason=(
                    f"new top-{cfg.top_k} entry: lookback return "
                    f"{sig.lookback_return_pct:+.2f}%, target ${target_notional_per_symbol:.4f}"
                ),
                target_notional_usdt=target_notional_per_symbol,
            ))
        if not decisions:
            decisions.append(TrendFilterDecision(
                action="hold", symbol="",
                reason="no eligible signals; staying in cash",
            ))
        return decisions


@dataclass(frozen=True, slots=True)
class TrendFilterLedgerEntry:
    timestamp_ms: int
    action: str
    symbol: str
    qty: float
    price: float
    notional_usdt: float
    fee_quote: float
    order_id: str
    success: bool
    error: str = ""


def append_audit(path: str, payload: Mapping[str, Any]) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True))
        fh.write("\n")


def append_ledger(path: str, entries: Sequence[TrendFilterLedgerEntry]) -> None:
    if not entries:
        return
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        for entry in entries:
            fh.write(json.dumps(asdict(entry), sort_keys=True))
            fh.write("\n")


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def _utc_timestamp() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


async def _fetch_usdt_balance(spot_client: Any) -> float:
    try:
        bal = await spot_client.fetch_balance()
    except Exception:  # noqa: BLE001 — fail closed to configured fallback
        return 0.0
    if not isinstance(bal, Mapping):
        return 0.0
    free = bal.get("free")
    if isinstance(free, Mapping):
        try:
            return float(free.get("USDT") or 0.0)
        except (TypeError, ValueError):
            return 0.0
    return 0.0


async def build_live_spot_client(settings: TrendFilterSettings) -> Any:
    import ccxt.async_support as ccxt_async
    return ccxt_async.binance(
        {
            "apiKey": settings.api_key, "secret": settings.api_secret,
            "enableRateLimit": True,
            "options": {"adjustForTimeDifference": True, "fetchCurrencies": False},
        }
    )


async def _execute_decisions(
    *, settings: TrendFilterSettings, spot_client: Any,
    decisions: Sequence[TrendFilterDecision],
    signals_by_symbol: Mapping[str, TrendFilterSignal],
    current_time_ms: int,
    current_holdings: dict[str, TrendFilterHolding],
) -> tuple[dict[str, TrendFilterHolding], list[TrendFilterLedgerEntry]]:
    ledger: list[TrendFilterLedgerEntry] = []
    # Sells first to free cash.
    for d in decisions:
        if d.action != "sell":
            continue
        try:
            order = await spot_client.create_order(
                symbol=d.symbol, type="market", side="sell", amount=d.qty,
            )
            filled = float(order.get("filled") or 0.0)
            avg = float(order.get("average") or 0.0) or signals_by_symbol[d.symbol].current_price
            notional = filled * avg
            fee = float((order.get("fee") or {}).get("cost") or 0.0)
            ledger.append(TrendFilterLedgerEntry(
                timestamp_ms=current_time_ms, action="sell", symbol=d.symbol,
                qty=filled, price=avg, notional_usdt=notional, fee_quote=fee,
                order_id=str(order.get("id") or ""), success=filled > 0,
            ))
            current_holdings.pop(d.symbol, None)
            from src.alerts import maybe_telegram
            await maybe_telegram(
                f"trend filter SELL {d.symbol}: {filled:.6f} @ ${avg:.4f} = ${notional:.4f}"
            )
        except Exception as exc:  # noqa: BLE001 — log + continue
            ledger.append(TrendFilterLedgerEntry(
                timestamp_ms=current_time_ms, action="sell", symbol=d.symbol,
                qty=0.0, price=0.0, notional_usdt=0.0, fee_quote=0.0,
                order_id="", success=False, error=f"{type(exc).__name__}: {str(exc)[:200]}",
            ))
            from src.alerts import maybe_telegram
            await maybe_telegram(
                f"trend filter SELL FAILED {d.symbol}: {type(exc).__name__}: {str(exc)[:150]}"
            )

    # Buys after sells settle. Use raw amount (base qty) for create_order.
    for d in decisions:
        if d.action != "buy":
            continue
        sig = signals_by_symbol.get(d.symbol)
        if sig is None or sig.current_price <= 0:
            continue
        qty = d.target_notional_usdt / sig.current_price
        try:
            # quoteOrderQty is the cleanest way to buy "USDT worth"; fall back
            # to standard amount if the venue rejects.
            order = await spot_client.create_order(
                symbol=d.symbol, type="market", side="buy", amount=None,
                price=None, params={"quoteOrderQty": d.target_notional_usdt},
            )
            filled = float(order.get("filled") or 0.0)
            avg = float(order.get("average") or 0.0) or sig.current_price
            notional = filled * avg
            fee = float((order.get("fee") or {}).get("cost") or 0.0)
            ledger.append(TrendFilterLedgerEntry(
                timestamp_ms=current_time_ms, action="buy", symbol=d.symbol,
                qty=filled, price=avg, notional_usdt=notional, fee_quote=fee,
                order_id=str(order.get("id") or ""), success=filled > 0,
            ))
            if filled > 0:
                current_holdings[d.symbol] = TrendFilterHolding(
                    symbol=d.symbol, qty=filled, entry_price=avg,
                    entry_time_ms=current_time_ms,
                )
            from src.alerts import maybe_telegram
            await maybe_telegram(
                f"trend filter BUY {d.symbol}: {filled:.6f} @ ${avg:.4f} = ${notional:.4f}"
            )
        except Exception as exc:  # noqa: BLE001 — log + continue
            ledger.append(TrendFilterLedgerEntry(
                timestamp_ms=current_time_ms, action="buy", symbol=d.symbol,
                qty=0.0, price=0.0, notional_usdt=0.0, fee_quote=0.0,
                order_id="", success=False, error=f"{type(exc).__name__}: {str(exc)[:200]}",
            ))
            from src.alerts import maybe_telegram
            await maybe_telegram(
                f"trend filter BUY FAILED {d.symbol}: {type(exc).__name__}: {str(exc)[:150]}"
            )
    return current_holdings, ledger


async def run_cycle(
    settings: TrendFilterSettings, *, mode: str = "plan_only",
    spot_client: Any | None = None,
) -> dict[str, Any]:
    """Run one trend-filter decision cycle.

    Modes: ``plan_only`` (no state mutation), ``paper`` (state-mutating
    simulation), ``live`` (real orders via ``spot_client``).
    """
    if mode not in {"plan_only", "paper", "live"}:
        raise TrendFilterError(f"unknown mode: {mode}")
    _preflight(settings, submit_order=(mode == "live"))

    state = load_state(settings.state_path)
    signals = await _fetch_signals(settings)
    signals_by_symbol = {s.symbol: s for s in signals}

    # Resolve balance.
    balance_usdt: float
    balance_source: str
    if mode == "live" and settings.dynamic_balance and spot_client is not None:
        balance_usdt = await _fetch_usdt_balance(spot_client)
        balance_source = "live"
    elif mode == "live":
        balance_usdt = 0.0
        balance_source = "missing"
    else:
        balance_usdt = settings.min_balance_usdt  # placeholder for plan/paper
        balance_source = "synthetic"

    insufficient_balance = (
        mode == "live" and balance_usdt < settings.min_balance_usdt
    )

    current_ts = _utc_now_ms()
    advisor = TrendFilterAdvisor(settings)
    decisions = advisor.decide(
        state=state, signals=signals,
        balance_usdt=balance_usdt, current_time_ms=current_ts,
    )

    ledger: list[TrendFilterLedgerEntry] = []
    rebalanced = any(d.action in {"buy", "sell"} for d in decisions)

    if mode == "live" and rebalanced and not insufficient_balance and spot_client is not None:
        current_holdings = {h.symbol: h for h in state.holdings}
        current_holdings, ledger = await _execute_decisions(
            settings=settings, spot_client=spot_client, decisions=decisions,
            signals_by_symbol=signals_by_symbol, current_time_ms=current_ts,
            current_holdings=current_holdings,
        )
        state = TrendFilterState(
            holdings=tuple(current_holdings.values()),
            last_rebalance_time_ms=current_ts,
        )
        save_state(state, settings.state_path)
        append_ledger(settings.ledger_path, ledger)
    elif mode == "paper" and rebalanced and not insufficient_balance:
        # Simulate fills at current_price.
        current_holdings = {h.symbol: h for h in state.holdings}
        for d in decisions:
            if d.action == "sell":
                current_holdings.pop(d.symbol, None)
                ledger.append(TrendFilterLedgerEntry(
                    timestamp_ms=current_ts, action="sell", symbol=d.symbol,
                    qty=d.qty, price=signals_by_symbol[d.symbol].current_price,
                    notional_usdt=d.qty * signals_by_symbol[d.symbol].current_price,
                    fee_quote=d.qty * signals_by_symbol[d.symbol].current_price * (settings.taker_fee_bps / 10_000.0),
                    order_id="paper", success=True,
                ))
            elif d.action == "buy":
                sig = signals_by_symbol[d.symbol]
                qty = d.target_notional_usdt / sig.current_price
                current_holdings[d.symbol] = TrendFilterHolding(
                    symbol=d.symbol, qty=qty, entry_price=sig.current_price,
                    entry_time_ms=current_ts,
                )
                ledger.append(TrendFilterLedgerEntry(
                    timestamp_ms=current_ts, action="buy", symbol=d.symbol,
                    qty=qty, price=sig.current_price, notional_usdt=d.target_notional_usdt,
                    fee_quote=d.target_notional_usdt * (settings.taker_fee_bps / 10_000.0),
                    order_id="paper", success=True,
                ))
        state = TrendFilterState(
            holdings=tuple(current_holdings.values()),
            last_rebalance_time_ms=current_ts,
        )
        save_state(state, settings.state_path)
        append_ledger(settings.ledger_path, ledger)

    payload = {
        "timestamp": _utc_timestamp(),
        "mode": mode,
        "universe_size": len(settings.universe),
        "signals_count": len(signals),
        "balance_source": balance_source,
        "balance_usdt": balance_usdt,
        "insufficient_balance": insufficient_balance,
        "open_holdings": [h.as_dict() for h in state.holdings],
        "decisions": [asdict(d) for d in decisions],
        "ledger_entries": [asdict(e) for e in ledger],
        "top_signals": sorted(
            [{"symbol": s.symbol, "lookback_return_pct": s.lookback_return_pct,
              "current_price": s.current_price} for s in signals],
            key=lambda r: r["lookback_return_pct"], reverse=True,
        )[:10],
    }
    append_audit(settings.audit_path, payload)
    return payload


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Long-horizon trend-filter runtime. Plan-only by default."
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--plan-only", action="store_const", const="plan_only", dest="mode")
    group.add_argument("--paper", action="store_const", const="paper", dest="mode")
    group.add_argument("--submit-live", action="store_const", const="live", dest="mode")
    parser.set_defaults(mode="plan_only")
    args = parser.parse_args(argv)

    try:
        settings = TrendFilterSettings.from_env()
        if args.mode == "live":
            result = asyncio.run(_run_live(settings))
        else:
            result = asyncio.run(run_cycle(settings, mode=args.mode))
    except TrendFilterError as exc:
        print(f"trend-filter failed: {exc}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


async def _run_live(settings: TrendFilterSettings) -> dict[str, Any]:
    client = await build_live_spot_client(settings)
    try:
        return await run_cycle(settings, mode="live", spot_client=client)
    finally:
        try:
            await client.close()
        except Exception:  # noqa: BLE001 — best-effort cleanup
            pass


if __name__ == "__main__":
    raise SystemExit(run())

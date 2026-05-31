"""Live mean-reversion runtime (paper-first, capital-gated for live).

Companion to src.mean_reversion_backtest. Polls Binance REST for 1m
klines on a configured universe, tracks rolling 15-minute returns, and
fires the same decision rule the backtest validated:

  enter long when trailing N-min return <= dump_threshold_pct
  exit on retrace target / stop loss / max hold

Three modes:

  --plan-only     read-only: compute decisions, audit them, no state mutation
  --paper         materialise decisions in local state, simulate fills at
                  current close, write closed trades to ledger
  --submit-live   real spot BUY/SELL via ccxt; gated by MEAN_REV_ENABLED,
                  CONFIRM_LIVE_MEAN_REV, !DRY_RUN/!USE_TESTNET, and
                  MEAN_REV_PROVEN_EDGE (set only after the backtest verdict
                  is reproduced on the live ledger).

Designed to run as a long-lived systemd daemon (Type=simple). Loops
internally on ``cycle_interval_seconds`` so a single process can keep its
in-memory rolling-return state hot across many minutes.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from dataclasses import asdict, dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

import aiohttp
from dotenv import load_dotenv


class MeanReversionError(RuntimeError):
    """Raised when the mean-reversion runtime cannot proceed."""


# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class MeanReversionSettings:
    api_key: str
    api_secret: str
    use_testnet: bool = False
    dry_run: bool = True
    enabled: bool = False
    confirm_live_mean_rev: str = "no"

    universe: tuple[str, ...] = (
        "BTC/USDT", "ETH/USDT", "SOL/USDT", "BNB/USDT",
        "XRP/USDT", "ADA/USDT", "DOGE/USDT", "AVAX/USDT",
    )
    dump_lookback_bars: int = 15
    dump_threshold_pct: float = -5.0
    retrace_target_pct: float = 0.5
    stop_loss_pct: float = -3.0
    max_hold_bars: int = 60

    taker_fee_bps: float = 10.0
    slippage_bps: float = 5.0

    notional_per_position_usdt: float = 50.0
    max_concurrent_positions: int = 5
    min_balance_usdt: float = 50.0
    dynamic_balance: bool = True

    cycle_interval_seconds: float = 60.0
    fetch_concurrency: int = 4

    state_path: str = "data/mean-reversion-state.json"
    audit_path: str = "data/mean-reversion-decisions.jsonl"
    ledger_path: str = "data/mean-reversion-ledger.jsonl"
    emergency_stop_path: str = "data/mean-reversion.stop"

    def validate(self) -> None:
        if not self.universe:
            raise MeanReversionError("universe must not be empty")
        if self.dump_lookback_bars <= 0:
            raise MeanReversionError("dump_lookback_bars must be positive")
        if self.dump_threshold_pct >= 0:
            raise MeanReversionError("dump_threshold_pct must be negative")
        if self.max_hold_bars <= 0:
            raise MeanReversionError("max_hold_bars must be positive")
        if self.max_concurrent_positions <= 0:
            raise MeanReversionError("max_concurrent_positions must be positive")
        if self.notional_per_position_usdt <= 0:
            raise MeanReversionError("notional_per_position_usdt must be positive")
        if self.min_balance_usdt < 0:
            raise MeanReversionError("min_balance_usdt must be non-negative")
        if self.cycle_interval_seconds < 1.0:
            raise MeanReversionError("cycle_interval_seconds must be >= 1.0")
        for path_name in ("state_path", "audit_path", "ledger_path", "emergency_stop_path"):
            if not getattr(self, path_name).strip():
                raise MeanReversionError(f"{path_name} must not be blank")

    @classmethod
    def from_env(cls) -> "MeanReversionSettings":
        load_dotenv(dotenv_path=os.environ.get("DOTENV_PATH"))
        api_key = os.getenv("BINANCE_API_KEY", "")
        api_secret = os.getenv("BINANCE_API_SECRET", "")
        if not api_key or not api_secret:
            raise MeanReversionError("BINANCE_API_KEY and BINANCE_API_SECRET must be set")

        def _bool(name: str, default: bool) -> bool:
            v = os.getenv(name, "")
            return default if not v else v.strip().lower() in {"1", "true", "yes", "on"}

        def _float(name: str, default: float) -> float:
            try:
                return float(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise MeanReversionError(f"{name} must be numeric")

        def _int(name: str, default: int) -> int:
            try:
                return int(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise MeanReversionError(f"{name} must be an integer")

        def _csv(name: str, default: tuple[str, ...]) -> tuple[str, ...]:
            raw = os.getenv(name, "")
            return tuple(s.strip() for s in raw.split(",") if s.strip()) or default

        defaults = cls(api_key="x", api_secret="x")
        settings = cls(
            api_key=api_key, api_secret=api_secret,
            use_testnet=_bool("USE_TESTNET", False),
            dry_run=_bool("DRY_RUN", True),
            enabled=_bool("MEAN_REV_ENABLED", False),
            confirm_live_mean_rev=os.getenv("CONFIRM_LIVE_MEAN_REV", "no"),
            universe=_csv("MEAN_REV_UNIVERSE", defaults.universe),
            dump_lookback_bars=_int("MEAN_REV_DUMP_LOOKBACK_BARS", defaults.dump_lookback_bars),
            dump_threshold_pct=_float("MEAN_REV_DUMP_THRESHOLD_PCT", defaults.dump_threshold_pct),
            retrace_target_pct=_float("MEAN_REV_RETRACE_TARGET_PCT", defaults.retrace_target_pct),
            stop_loss_pct=_float("MEAN_REV_STOP_LOSS_PCT", defaults.stop_loss_pct),
            max_hold_bars=_int("MEAN_REV_MAX_HOLD_BARS", defaults.max_hold_bars),
            taker_fee_bps=_float("MEAN_REV_TAKER_FEE_BPS", defaults.taker_fee_bps),
            slippage_bps=_float("MEAN_REV_SLIPPAGE_BPS", defaults.slippage_bps),
            notional_per_position_usdt=_float(
                "MEAN_REV_NOTIONAL_PER_POSITION_USDT",
                defaults.notional_per_position_usdt,
            ),
            max_concurrent_positions=_int(
                "MEAN_REV_MAX_CONCURRENT_POSITIONS", defaults.max_concurrent_positions,
            ),
            min_balance_usdt=_float("MEAN_REV_MIN_BALANCE_USDT", defaults.min_balance_usdt),
            dynamic_balance=_bool("MEAN_REV_DYNAMIC_BALANCE", defaults.dynamic_balance),
            cycle_interval_seconds=_float(
                "MEAN_REV_CYCLE_INTERVAL_SECONDS", defaults.cycle_interval_seconds,
            ),
            fetch_concurrency=_int("MEAN_REV_FETCH_CONCURRENCY", defaults.fetch_concurrency),
            state_path=os.getenv("MEAN_REV_STATE_PATH", defaults.state_path),
            audit_path=os.getenv("MEAN_REV_AUDIT_PATH", defaults.audit_path),
            ledger_path=os.getenv("MEAN_REV_LEDGER_PATH", defaults.ledger_path),
            emergency_stop_path=os.getenv(
                "MEAN_REV_EMERGENCY_STOP_PATH", defaults.emergency_stop_path,
            ),
        )
        settings.validate()
        return settings


# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class MeanReversionPosition:
    symbol: str
    open_time_ms: int
    entry_price: float
    qty: float
    notional_usdt: float
    dump_pct_at_entry: float
    bars_held: int = 0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "MeanReversionPosition":
        return cls(
            symbol=str(payload["symbol"]),
            open_time_ms=int(payload["open_time_ms"]),
            entry_price=float(payload["entry_price"]),
            qty=float(payload["qty"]),
            notional_usdt=float(payload["notional_usdt"]),
            dump_pct_at_entry=float(payload["dump_pct_at_entry"]),
            bars_held=int(payload.get("bars_held") or 0),
        )


@dataclass(frozen=True, slots=True)
class MeanReversionState:
    positions: tuple[MeanReversionPosition, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return {"positions": [p.as_dict() for p in self.positions]}

    @classmethod
    def empty(cls) -> "MeanReversionState":
        return cls()

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "MeanReversionState":
        rows = payload.get("positions") or []
        return cls(positions=tuple(MeanReversionPosition.from_mapping(r) for r in rows))


def load_state(path: Path | str) -> MeanReversionState:
    p = Path(path)
    if not p.exists():
        return MeanReversionState.empty()
    try:
        return MeanReversionState.from_mapping(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise MeanReversionError(f"could not read state at {p}: {exc}") from exc


def save_state(state: MeanReversionState, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(state.as_dict(), indent=2, sort_keys=True))
    tmp.replace(p)


# ---------------------------------------------------------------------------
# Signals + decisions
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class SymbolSignal:
    """Latest snapshot a symbol contributes to the decision engine."""

    symbol: str
    current_price: float
    bar_high: float
    bar_low: float
    lookback_return_pct: float


@dataclass(frozen=True, slots=True)
class MeanReversionDecision:
    """One action a cycle would take (open, close, hold, skip)."""

    action: str  # "open" | "close" | "hold" | "skip_open"
    symbol: str
    reason: str
    notional_usdt: float = 0.0
    qty: float = 0.0
    entry_price: float | None = None
    exit_price: float | None = None


def compute_decisions(
    settings: MeanReversionSettings,
    state: MeanReversionState,
    signals: Sequence[SymbolSignal],
) -> list[MeanReversionDecision]:
    """Pure decision rule — mirrors the backtest's per-bar logic."""
    signals_by_symbol = {s.symbol: s for s in signals}
    decisions: list[MeanReversionDecision] = []

    held_symbols = {p.symbol for p in state.positions}
    close_symbols: set[str] = set()

    # 1) Exits on currently-held positions.
    for position in state.positions:
        signal = signals_by_symbol.get(position.symbol)
        if signal is None:
            decisions.append(MeanReversionDecision(
                action="hold", symbol=position.symbol,
                reason="no signal this cycle; held position retained",
            ))
            continue
        entry = position.entry_price
        dump = abs(position.dump_pct_at_entry)
        target_price = entry * (1.0 + dump * settings.retrace_target_pct / 100.0)
        stop_price = entry * (1.0 + settings.stop_loss_pct / 100.0)
        reason = ""
        exit_price = entry
        if signal.bar_low <= stop_price:
            reason = (
                f"stop loss: bar_low={signal.bar_low:.4f} "
                f"<= stop_price={stop_price:.4f}"
            )
            exit_price = stop_price
        elif signal.bar_high >= target_price:
            reason = (
                f"take profit: bar_high={signal.bar_high:.4f} "
                f">= target={target_price:.4f}"
            )
            exit_price = target_price
        elif position.bars_held >= settings.max_hold_bars:
            reason = f"max hold ({settings.max_hold_bars} bars) reached"
            exit_price = signal.current_price
        if reason:
            decisions.append(MeanReversionDecision(
                action="close", symbol=position.symbol, reason=reason,
                qty=position.qty, exit_price=exit_price,
            ))
            close_symbols.add(position.symbol)
        else:
            decisions.append(MeanReversionDecision(
                action="hold", symbol=position.symbol,
                reason=(
                    f"no exit: bar [{signal.bar_low:.4f}, {signal.bar_high:.4f}] "
                    f"inside [{stop_price:.4f}, {target_price:.4f}]; bars_held="
                    f"{position.bars_held}"
                ),
            ))

    # 2) Entries (capacity = max_concurrent - currently_open + queued_closes).
    free_slots = (
        settings.max_concurrent_positions - len(held_symbols) + len(close_symbols)
    )
    if free_slots > 0:
        # Rank candidates by most-negative lookback return (deepest dump).
        candidates: list[tuple[float, SymbolSignal]] = []
        for signal in signals:
            if signal.symbol in held_symbols and signal.symbol not in close_symbols:
                continue
            if signal.lookback_return_pct > settings.dump_threshold_pct:
                continue
            candidates.append((signal.lookback_return_pct, signal))
        candidates.sort(key=lambda item: item[0])  # most-negative first
        for ret_pct, signal in candidates[:max(free_slots, 0)]:
            entry_price = signal.current_price
            if entry_price <= 0:
                continue
            qty = settings.notional_per_position_usdt / entry_price
            decisions.append(MeanReversionDecision(
                action="open", symbol=signal.symbol,
                reason=(
                    f"15m return {ret_pct:.2f}% <= threshold "
                    f"{settings.dump_threshold_pct:.2f}%; "
                    f"entry {entry_price:.4f}"
                ),
                notional_usdt=settings.notional_per_position_usdt,
                qty=qty, entry_price=entry_price,
            ))
    return decisions


# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise MeanReversionError(
            f"emergency stop file present at {path}; refusing to run"
        )


def _preflight(settings: MeanReversionSettings, *, submit_order: bool) -> None:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if not submit_order:
        return
    if not settings.enabled:
        raise MeanReversionError("MEAN_REV_ENABLED=true is required for live submission")
    if settings.use_testnet:
        raise MeanReversionError("mean-rev live submission requires USE_TESTNET=false")
    if settings.dry_run:
        raise MeanReversionError("mean-rev live submission requires DRY_RUN=false")
    if settings.confirm_live_mean_rev != "yes":
        raise MeanReversionError(
            "mean-rev live submission requires CONFIRM_LIVE_MEAN_REV=yes"
        )
    if os.getenv("MEAN_REV_PROVEN_EDGE", "").strip().lower() != "yes":
        raise MeanReversionError(
            "mean-rev live submission requires MEAN_REV_PROVEN_EDGE=yes. "
            "Run --paper for at least a few hundred completed round-trips; "
            "set this only when paper expectancy reproduces the backtest verdict."
        )


# ---------------------------------------------------------------------------
# Klines + signals
# ---------------------------------------------------------------------------


KLINES_REST_URL = "https://api.binance.com/api/v3/klines"


async def _fetch_kline_window(
    *, session: aiohttp.ClientSession, symbol: str, lookback_bars: int,
) -> list[list[Any]] | None:
    """Pull just enough 1-minute klines to compute the trailing return."""
    rest_symbol = symbol.replace("/", "").upper()
    params = {"symbol": rest_symbol, "interval": "1m", "limit": lookback_bars + 2}
    try:
        async with session.get(KLINES_REST_URL, params=params, timeout=10) as resp:
            if resp.status != 200:
                return None
            payload = await resp.json()
    except (aiohttp.ClientError, asyncio.TimeoutError):
        return None
    if not isinstance(payload, list) or len(payload) < lookback_bars + 1:
        return None
    return payload


async def fetch_signals(
    *, session: aiohttp.ClientSession, settings: MeanReversionSettings,
) -> list[SymbolSignal]:
    sem = asyncio.Semaphore(max(settings.fetch_concurrency, 1))

    async def _one(symbol: str) -> SymbolSignal | None:
        async with sem:
            klines = await _fetch_kline_window(
                session=session, symbol=symbol,
                lookback_bars=settings.dump_lookback_bars,
            )
        if klines is None:
            return None
        try:
            current_close = float(klines[-1][4])
            current_high = float(klines[-1][2])
            current_low = float(klines[-1][3])
            lookback_close = float(klines[-(settings.dump_lookback_bars + 1)][4])
        except (TypeError, ValueError, IndexError):
            return None
        if lookback_close <= 0:
            return None
        ret_pct = (current_close / lookback_close - 1.0) * 100.0
        return SymbolSignal(
            symbol=symbol, current_price=current_close,
            bar_high=current_high, bar_low=current_low,
            lookback_return_pct=ret_pct,
        )

    results = await asyncio.gather(*(_one(s) for s in settings.universe))
    return [s for s in results if s is not None]


# ---------------------------------------------------------------------------
# Ledger + audit
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class MeanReversionLedgerEntry:
    timestamp_ms: int
    action: str         # "open" | "close"
    symbol: str
    qty: float
    price: float
    notional_usdt: float
    fee_usdt: float
    pnl_usdt: float
    exit_reason: str
    mode: str           # "paper" | "live"


def _append_ledger(path: str, entry: MeanReversionLedgerEntry) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(asdict(entry), sort_keys=True))
        fh.write("\n")


def _append_audit(path: str, payload: Mapping[str, Any]) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True))
        fh.write("\n")


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def _utc_timestamp() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


# ---------------------------------------------------------------------------
# Apply decisions (paper)
# ---------------------------------------------------------------------------


def apply_paper_decisions(
    *,
    settings: MeanReversionSettings,
    state: MeanReversionState,
    signals: Sequence[SymbolSignal],
    decisions: Sequence[MeanReversionDecision],
    cycle_ts_ms: int,
) -> tuple[MeanReversionState, list[MeanReversionLedgerEntry]]:
    fee_rate = settings.taker_fee_bps / 10_000.0
    slip_rate = settings.slippage_bps / 10_000.0
    one_side_cost_rate = fee_rate + slip_rate
    signals_by_symbol = {s.symbol: s for s in signals}

    closed_symbols = {d.symbol for d in decisions if d.action == "close"}
    new_positions: list[MeanReversionPosition] = []
    ledger: list[MeanReversionLedgerEntry] = []

    # 1. Carry forward held positions that aren't being closed; tick bars_held.
    for position in state.positions:
        if position.symbol in closed_symbols:
            continue
        position.bars_held += 1
        new_positions.append(position)

    # 2. Record close ledger rows.
    for decision in decisions:
        if decision.action != "close" or decision.exit_price is None:
            continue
        held = next((p for p in state.positions if p.symbol == decision.symbol), None)
        if held is None:
            continue
        exit_price = decision.exit_price
        proceeds = held.qty * exit_price * (1.0 - one_side_cost_rate)
        # The entry side already absorbed its fees at open time (recorded then).
        # PnL net of *exit-side* costs vs. notional invested at open.
        pnl = proceeds - held.notional_usdt
        ledger.append(MeanReversionLedgerEntry(
            timestamp_ms=cycle_ts_ms, action="close", symbol=held.symbol,
            qty=held.qty, price=exit_price,
            notional_usdt=held.qty * exit_price,
            fee_usdt=held.qty * exit_price * one_side_cost_rate,
            pnl_usdt=pnl, exit_reason=decision.reason, mode="paper",
        ))

    # 3. Open new positions.
    for decision in decisions:
        if decision.action != "open" or decision.entry_price is None:
            continue
        entry_price = decision.entry_price
        qty = decision.qty
        notional = decision.notional_usdt
        entry_fee = notional * one_side_cost_rate
        # P&L at entry is the entry fee (negative). True P&L resolves at close.
        ledger.append(MeanReversionLedgerEntry(
            timestamp_ms=cycle_ts_ms, action="open", symbol=decision.symbol,
            qty=qty, price=entry_price, notional_usdt=notional,
            fee_usdt=entry_fee, pnl_usdt=-entry_fee, exit_reason="",
            mode="paper",
        ))
        new_positions.append(MeanReversionPosition(
            symbol=decision.symbol, open_time_ms=cycle_ts_ms,
            entry_price=entry_price, qty=qty, notional_usdt=notional,
            dump_pct_at_entry=signals_by_symbol[decision.symbol].lookback_return_pct,
            bars_held=0,
        ))

    return MeanReversionState(positions=tuple(new_positions)), ledger


# ---------------------------------------------------------------------------
# Cycle driver
# ---------------------------------------------------------------------------


async def run_cycle(
    settings: MeanReversionSettings, *,
    mode: str = "plan_only",
    session: aiohttp.ClientSession | None = None,
) -> dict[str, Any]:
    """One pass: fetch signals, compute decisions, optionally execute paper-mode."""
    if mode not in {"plan_only", "paper", "live"}:
        raise MeanReversionError(f"unknown mode: {mode}")
    _preflight(settings, submit_order=(mode == "live"))
    if mode == "live":
        raise MeanReversionError(
            "live execution path is not yet implemented; only plan_only and paper are supported"
        )

    owns_session = session is None
    if session is None:
        session = aiohttp.ClientSession()
    try:
        signals = await fetch_signals(session=session, settings=settings)
    finally:
        if owns_session:
            await session.close()

    state = load_state(settings.state_path)
    decisions = compute_decisions(settings, state, signals)

    ledger: list[MeanReversionLedgerEntry] = []
    if mode == "paper":
        cycle_ts_ms = _utc_now_ms()
        state, ledger = apply_paper_decisions(
            settings=settings, state=state, signals=signals,
            decisions=decisions, cycle_ts_ms=cycle_ts_ms,
        )
        save_state(state, settings.state_path)
        for entry in ledger:
            _append_ledger(settings.ledger_path, entry)

    payload = {
        "timestamp": _utc_timestamp(),
        "mode": mode,
        "signals_count": len(signals),
        "open_positions": len(state.positions),
        "decisions": [asdict(d) for d in decisions],
        "ledger_entries_this_cycle": [asdict(e) for e in ledger],
        "top_signals": sorted(
            [{"symbol": s.symbol, "lookback_return_pct": s.lookback_return_pct,
              "current_price": s.current_price} for s in signals],
            key=lambda r: r["lookback_return_pct"],
        )[:10],
    }
    _append_audit(settings.audit_path, payload)
    return payload


async def run_daemon(settings: MeanReversionSettings, *, mode: str) -> None:
    """Long-running loop for paper / future-live deployment."""
    async with aiohttp.ClientSession() as session:
        while True:
            try:
                await run_cycle(settings, mode=mode, session=session)
            except MeanReversionError as exc:
                _append_audit(settings.audit_path, {
                    "timestamp": _utc_timestamp(), "error": str(exc),
                })
            except Exception as exc:  # noqa: BLE001 — never abort the daemon
                _append_audit(settings.audit_path, {
                    "timestamp": _utc_timestamp(),
                    "error": f"{type(exc).__name__}: {str(exc)[:200]}",
                })
            await asyncio.sleep(settings.cycle_interval_seconds)


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Mean-reversion runtime: dump-buy with retrace/stop/max-hold exits."
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--plan-only", action="store_const", const="plan_only", dest="mode",
    )
    group.add_argument(
        "--paper", action="store_const", const="paper", dest="mode",
    )
    group.add_argument(
        "--submit-live", action="store_const", const="live", dest="mode",
    )
    parser.add_argument(
        "--daemon", action="store_true",
        help="loop forever at cycle_interval_seconds (default: one cycle then exit)",
    )
    parser.set_defaults(mode="plan_only")
    args = parser.parse_args(argv)

    try:
        settings = MeanReversionSettings.from_env()
        if args.daemon:
            asyncio.run(run_daemon(settings, mode=args.mode))
            return 0
        result = asyncio.run(run_cycle(settings, mode=args.mode))
    except MeanReversionError as exc:
        print(f"mean-reversion failed: {exc}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

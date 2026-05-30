"""Delta-neutral funding-rate harvest runtime.

This module is the live counterpart to ``src.funding_arb_backtest``. It owns
the persistent positions and submits the spot-buy / perp-short legs (and
their close legs) under a layered set of preflight gates.

Decision shape (deterministic, no LLM):

* For each candidate USD-M perp:
    * If we don't already hold a position: open when the *current* funding
      rate clears ``entry_funding_bps``.
    * If we do hold a position: close when the funding rate falls to or
      below ``exit_funding_bps``, or when ``max_hold_funding_events`` has
      elapsed, or when the spot/perp basis blows out.
* Capacity is bounded by ``max_concurrent_positions``.
* Per-position notional is ``per_position_notional_pct`` of starting
  balance.

Live submission is gated by:

* ``FUNDING_ARB_ENABLED=true``
* ``CONFIRM_LIVE_FUNDING_ARB=yes``
* ``USE_TESTNET=false`` and ``DRY_RUN=false``
* ``FUNDING_ARB_PROVEN_EDGE=yes`` (the dedicated gate; set only after
  the backtest harness reports ``edge_likely`` under conservative cost
  assumptions, and you accept the unmodeled risks documented in
  OPERATIONS.md).

Until live is intentionally enabled, the runtime operates in *plan-only*
mode (no orders submitted, decisions printed and audited).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

from dotenv import load_dotenv


class FundingArbError(RuntimeError):
    """Raised when the funding-arb runtime cannot proceed."""


@dataclass(frozen=True, slots=True)
class FundingArbSettings:
    api_key: str
    api_secret: str
    use_testnet: bool = False
    dry_run: bool = True
    enabled: bool = False
    confirm_live_funding_arb: str = "no"

    universe: tuple[str, ...] = ()  # explicit allowlist; empty = use dynamic scope
    dynamic_universe_size: int = 50
    min_quote_volume_usdt: float = 50_000_000.0

    entry_funding_bps: float = 3.0
    exit_funding_bps: float = -1.0
    max_hold_funding_events: int = 30
    max_concurrent_positions: int = 5
    per_position_notional_pct: float = 20.0
    starting_balance_usdt: float = 10_000.0

    taker_fee_spot_bps: float = 10.0
    taker_fee_futures_bps: float = 5.0
    slippage_per_leg_bps: float = 5.0

    basis_circuit_breaker_bps: float = 200.0

    state_path: str = "data/funding-arb-state.json"
    audit_path: str = "data/funding-arb-decisions.jsonl"
    ledger_path: str = "data/funding-arb-ledger.jsonl"
    emergency_stop_path: str = "data/funding-arb.stop"

    def validate(self) -> None:
        if self.starting_balance_usdt <= 0:
            raise FundingArbError("starting_balance_usdt must be positive")
        if not (0 < self.per_position_notional_pct <= 100):
            raise FundingArbError("per_position_notional_pct must be in (0, 100]")
        if self.max_concurrent_positions <= 0:
            raise FundingArbError("max_concurrent_positions must be positive")
        if self.max_hold_funding_events <= 0:
            raise FundingArbError("max_hold_funding_events must be positive")
        if self.basis_circuit_breaker_bps <= 0:
            raise FundingArbError("basis_circuit_breaker_bps must be positive")
        if self.entry_funding_bps <= self.exit_funding_bps:
            raise FundingArbError(
                "entry_funding_bps must be strictly above exit_funding_bps"
            )
        for path_name in ("state_path", "audit_path", "ledger_path", "emergency_stop_path"):
            if not getattr(self, path_name).strip():
                raise FundingArbError(f"{path_name} must not be blank")

    @classmethod
    def from_env(cls) -> "FundingArbSettings":
        load_dotenv()
        api_key = os.getenv("BINANCE_API_KEY", "")
        api_secret = os.getenv("BINANCE_API_SECRET", "")
        if not api_key or not api_secret:
            raise FundingArbError(
                "BINANCE_API_KEY and BINANCE_API_SECRET must be set"
            )

        def _bool(name: str, default: bool) -> bool:
            value = os.getenv(name, "")
            if not value:
                return default
            return value.strip().lower() in {"1", "true", "yes", "on"}

        def _float(name: str, default: float) -> float:
            try:
                return float(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise FundingArbError(f"{name} must be numeric")

        def _int(name: str, default: int) -> int:
            try:
                return int(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise FundingArbError(f"{name} must be an integer")

        def _csv(name: str) -> tuple[str, ...]:
            raw = os.getenv(name, "")
            return tuple(s.strip() for s in raw.split(",") if s.strip())

        settings = cls(
            api_key=api_key,
            api_secret=api_secret,
            use_testnet=_bool("USE_TESTNET", False),
            dry_run=_bool("DRY_RUN", True),
            enabled=_bool("FUNDING_ARB_ENABLED", False),
            confirm_live_funding_arb=os.getenv("CONFIRM_LIVE_FUNDING_ARB", "no"),
            universe=_csv("FUNDING_ARB_UNIVERSE"),
            dynamic_universe_size=_int("FUNDING_ARB_DYNAMIC_UNIVERSE_SIZE", 50),
            min_quote_volume_usdt=_float("FUNDING_ARB_MIN_QUOTE_VOLUME_USDT", 50_000_000.0),
            entry_funding_bps=_float("FUNDING_ARB_ENTRY_FUNDING_BPS", 3.0),
            exit_funding_bps=_float("FUNDING_ARB_EXIT_FUNDING_BPS", -1.0),
            max_hold_funding_events=_int("FUNDING_ARB_MAX_HOLD_FUNDING_EVENTS", 30),
            max_concurrent_positions=_int("FUNDING_ARB_MAX_CONCURRENT_POSITIONS", 5),
            per_position_notional_pct=_float("FUNDING_ARB_PER_POSITION_NOTIONAL_PCT", 20.0),
            starting_balance_usdt=_float("FUNDING_ARB_STARTING_BALANCE_USDT", 10_000.0),
            taker_fee_spot_bps=_float("FUNDING_ARB_TAKER_FEE_SPOT_BPS", 10.0),
            taker_fee_futures_bps=_float("FUNDING_ARB_TAKER_FEE_FUTURES_BPS", 5.0),
            slippage_per_leg_bps=_float("FUNDING_ARB_SLIPPAGE_PER_LEG_BPS", 5.0),
            basis_circuit_breaker_bps=_float("FUNDING_ARB_BASIS_CIRCUIT_BREAKER_BPS", 200.0),
            state_path=os.getenv("FUNDING_ARB_STATE_PATH", "data/funding-arb-state.json"),
            audit_path=os.getenv("FUNDING_ARB_AUDIT_PATH", "data/funding-arb-decisions.jsonl"),
            ledger_path=os.getenv("FUNDING_ARB_LEDGER_PATH", "data/funding-arb-ledger.jsonl"),
            emergency_stop_path=os.getenv(
                "FUNDING_ARB_EMERGENCY_STOP_PATH", "data/funding-arb.stop"
            ),
        )
        settings.validate()
        return settings


@dataclass(slots=True)
class FundingArbPosition:
    """A live delta-neutral position: long spot + short perp."""

    symbol: str  # CCXT-style, e.g. "SOL/USDT"
    open_time_ms: int
    entry_funding_bps: float
    notional_usdt: float
    spot_qty: float
    perp_qty: float
    funding_received_bps: float = 0.0
    funding_received_usdt: float = 0.0
    events_held: int = 0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "FundingArbPosition":
        return cls(
            symbol=str(payload["symbol"]),
            open_time_ms=int(payload["open_time_ms"]),
            entry_funding_bps=float(payload["entry_funding_bps"]),
            notional_usdt=float(payload["notional_usdt"]),
            spot_qty=float(payload["spot_qty"]),
            perp_qty=float(payload["perp_qty"]),
            funding_received_bps=float(payload.get("funding_received_bps", 0.0)),
            funding_received_usdt=float(payload.get("funding_received_usdt", 0.0)),
            events_held=int(payload.get("events_held", 0)),
        )


@dataclass(frozen=True, slots=True)
class FundingArbDecision:
    action: str  # "open" | "close" | "hold"
    symbol: str
    reason: str
    current_funding_bps: float
    notional_usdt: float = 0.0


@dataclass(frozen=True, slots=True)
class FundingArbState:
    positions: tuple[FundingArbPosition, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return {"positions": [p.as_dict() for p in self.positions]}

    @classmethod
    def empty(cls) -> "FundingArbState":
        return cls()

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "FundingArbState":
        rows = payload.get("positions") or []
        return cls(positions=tuple(FundingArbPosition.from_mapping(r) for r in rows))


def load_state(path: Path | str) -> FundingArbState:
    p = Path(path)
    if not p.exists():
        return FundingArbState.empty()
    try:
        raw = json.loads(p.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise FundingArbError(f"could not read funding-arb state at {p}: {exc}") from exc
    return FundingArbState.from_mapping(raw)


def save_state(state: FundingArbState, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(state.as_dict(), indent=2, sort_keys=True))
    tmp.replace(p)


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise FundingArbError(f"emergency stop file present at {path}; refusing to run")


def _preflight(settings: FundingArbSettings, *, submit_order: bool) -> None:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if not submit_order:
        return
    if not settings.enabled:
        raise FundingArbError("FUNDING_ARB_ENABLED=true is required for live submission")
    if settings.use_testnet:
        raise FundingArbError("funding-arb live submission requires USE_TESTNET=false")
    if settings.dry_run:
        raise FundingArbError("funding-arb live submission requires DRY_RUN=false")
    if settings.confirm_live_funding_arb != "yes":
        raise FundingArbError(
            "funding-arb live submission requires CONFIRM_LIVE_FUNDING_ARB=yes"
        )
    if os.getenv("FUNDING_ARB_PROVEN_EDGE", "").strip().lower() != "yes":
        raise FundingArbError(
            "funding-arb live submission requires FUNDING_ARB_PROVEN_EDGE=yes. "
            "Run `python -m src.funding_arb_backtest` and only set this when the "
            "harness reports verdict=edge_likely under conservative cost "
            "assumptions. Until then, plan-only is the only available mode."
        )


@dataclass(frozen=True, slots=True)
class CandidateSignal:
    """Per-symbol snapshot used to drive a decision."""

    symbol: str
    funding_bps: float
    mark_price: float
    spot_price: float

    def basis_bps(self) -> float:
        if self.spot_price <= 0:
            return 0.0
        return (self.mark_price - self.spot_price) / self.spot_price * 10_000.0


class FundingArbAdvisor:
    """Deterministic open/close/hold decider for the held + candidate universe."""

    def __init__(self, settings: FundingArbSettings) -> None:
        self.settings = settings

    def decide(
        self,
        *,
        state: FundingArbState,
        signals: Sequence[CandidateSignal],
    ) -> list[FundingArbDecision]:
        decisions: list[FundingArbDecision] = []
        signals_by_symbol = {s.symbol: s for s in signals}
        held_symbols = {p.symbol for p in state.positions}
        cfg = self.settings

        # 1. Per-held-position: close or hold.
        for position in state.positions:
            signal = signals_by_symbol.get(position.symbol)
            if signal is None:
                decisions.append(
                    FundingArbDecision(
                        action="hold",
                        symbol=position.symbol,
                        reason="no current signal for held position; holding pending data",
                        current_funding_bps=0.0,
                    )
                )
                continue
            basis_bps = signal.basis_bps()
            if abs(basis_bps) >= cfg.basis_circuit_breaker_bps:
                decisions.append(
                    FundingArbDecision(
                        action="close",
                        symbol=position.symbol,
                        reason=(
                            f"basis circuit breaker: {basis_bps:.1f}bps "
                            f"exceeds {cfg.basis_circuit_breaker_bps:.1f}bps cap"
                        ),
                        current_funding_bps=signal.funding_bps,
                    )
                )
                continue
            if position.events_held >= cfg.max_hold_funding_events:
                decisions.append(
                    FundingArbDecision(
                        action="close",
                        symbol=position.symbol,
                        reason=f"max hold ({cfg.max_hold_funding_events} events) reached",
                        current_funding_bps=signal.funding_bps,
                    )
                )
                continue
            if signal.funding_bps <= cfg.exit_funding_bps:
                decisions.append(
                    FundingArbDecision(
                        action="close",
                        symbol=position.symbol,
                        reason=(
                            f"funding {signal.funding_bps:.2f}bps fell to or below "
                            f"exit threshold {cfg.exit_funding_bps:.2f}bps"
                        ),
                        current_funding_bps=signal.funding_bps,
                    )
                )
                continue
            decisions.append(
                FundingArbDecision(
                    action="hold",
                    symbol=position.symbol,
                    reason=(
                        f"funding {signal.funding_bps:.2f}bps still above exit "
                        f"({cfg.exit_funding_bps:.2f}bps); held {position.events_held} events"
                    ),
                    current_funding_bps=signal.funding_bps,
                )
            )

        # 2. Pending closes free capacity in the same cycle.
        pending_closes = {d.symbol for d in decisions if d.action == "close"}
        free_slots = (
            cfg.max_concurrent_positions
            - len(held_symbols)
            + len(pending_closes)
        )

        # 3. Score candidates for opening.
        opens: list[tuple[float, CandidateSignal]] = []
        for signal in signals:
            if signal.symbol in held_symbols and signal.symbol not in pending_closes:
                continue
            if signal.funding_bps < cfg.entry_funding_bps:
                continue
            basis_bps = signal.basis_bps()
            if abs(basis_bps) >= cfg.basis_circuit_breaker_bps:
                continue
            opens.append((signal.funding_bps, signal))
        opens.sort(reverse=True, key=lambda item: item[0])
        notional_per_position = cfg.starting_balance_usdt * (cfg.per_position_notional_pct / 100.0)
        for funding_bps, signal in opens[: max(free_slots, 0)]:
            decisions.append(
                FundingArbDecision(
                    action="open",
                    symbol=signal.symbol,
                    reason=(
                        f"funding {funding_bps:.2f}bps clears entry "
                        f"({cfg.entry_funding_bps:.2f}bps); basis "
                        f"{signal.basis_bps():.1f}bps within breaker"
                    ),
                    current_funding_bps=funding_bps,
                    notional_usdt=notional_per_position,
                )
            )
        return decisions


def append_audit(audit_path: str, payload: Mapping[str, Any]) -> None:
    p = Path(audit_path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True))
        fh.write("\n")


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


@dataclass(frozen=True, slots=True)
class PaperLedgerEntry:
    """One closed paper trade recorded for P&L attribution."""

    symbol: str
    open_time_ms: int
    close_time_ms: int
    entry_funding_bps: float
    funding_received_bps: float
    funding_received_usdt: float
    events_held: int
    round_trip_cost_bps: float
    round_trip_cost_usdt: float
    net_pnl_bps: float
    net_pnl_usdt: float


def apply_paper_decisions(
    *,
    settings: FundingArbSettings,
    state: FundingArbState,
    signals: Sequence[CandidateSignal],
    decisions: Sequence[FundingArbDecision],
    cycle_ts_ms: int,
) -> tuple[FundingArbState, list[PaperLedgerEntry]]:
    """Apply open/close/hold decisions to in-memory state for paper trading.

    Funding is credited once per cycle, per open position, at the current
    funding rate. Closes incur the modelled round-trip cost (which is split
    across open + close; opens pay nothing in paper mode, closes pay the
    full round-trip — equivalent total cost to the backtest model).
    """
    signals_by_symbol = {s.symbol: s for s in signals}
    close_symbols = {d.symbol for d in decisions if d.action == "close"}
    open_decisions = [d for d in decisions if d.action == "open"]

    one_side_cost_bps = (
        settings.taker_fee_spot_bps
        + settings.taker_fee_futures_bps
        + 2.0 * settings.slippage_per_leg_bps
    )
    round_trip_cost_bps = 2.0 * one_side_cost_bps

    updated: list[FundingArbPosition] = []
    ledger: list[PaperLedgerEntry] = []

    # 1. Accrue funding on currently open positions, queue closes.
    for position in state.positions:
        signal = signals_by_symbol.get(position.symbol)
        if signal is not None:
            funding_increment_bps = signal.funding_bps
            funding_increment_usdt = position.notional_usdt * (signal.funding_bps / 10_000.0)
            position.funding_received_bps += funding_increment_bps
            position.funding_received_usdt += funding_increment_usdt
            position.events_held += 1

        if position.symbol in close_symbols:
            close_cost_usdt = position.notional_usdt * (round_trip_cost_bps / 10_000.0)
            net_pnl_usdt = position.funding_received_usdt - close_cost_usdt
            net_pnl_bps = position.funding_received_bps - round_trip_cost_bps
            ledger.append(
                PaperLedgerEntry(
                    symbol=position.symbol,
                    open_time_ms=position.open_time_ms,
                    close_time_ms=cycle_ts_ms,
                    entry_funding_bps=position.entry_funding_bps,
                    funding_received_bps=position.funding_received_bps,
                    funding_received_usdt=position.funding_received_usdt,
                    events_held=position.events_held,
                    round_trip_cost_bps=round_trip_cost_bps,
                    round_trip_cost_usdt=close_cost_usdt,
                    net_pnl_bps=net_pnl_bps,
                    net_pnl_usdt=net_pnl_usdt,
                )
            )
            continue
        updated.append(position)

    # 2. Materialize newly opened positions.
    for decision in open_decisions:
        signal = signals_by_symbol.get(decision.symbol)
        if signal is None or signal.spot_price <= 0:
            continue
        spot_qty = decision.notional_usdt / signal.spot_price
        updated.append(
            FundingArbPosition(
                symbol=decision.symbol,
                open_time_ms=cycle_ts_ms,
                entry_funding_bps=decision.current_funding_bps,
                notional_usdt=decision.notional_usdt,
                spot_qty=spot_qty,
                perp_qty=-spot_qty,
            )
        )
    return FundingArbState(positions=tuple(updated)), ledger


def append_ledger(ledger_path: str, entries: Sequence[PaperLedgerEntry]) -> None:
    if not entries:
        return
    p = Path(ledger_path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        for entry in entries:
            fh.write(json.dumps(asdict(entry), sort_keys=True))
            fh.write("\n")


def _utc_timestamp() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


async def build_live_executor(settings: FundingArbSettings) -> Any:
    """Construct a FundingArbExecutor wired to real Binance Spot + USD-M Futures.

    Kept out of ``run_cycle`` so tests can inject a mock executor and so the
    ccxt import is lazy.
    """
    import ccxt.async_support as ccxt_async

    from src.funding_arb_executor import FundingArbExecutor

    spot = ccxt_async.binance(
        {
            "apiKey": settings.api_key,
            "secret": settings.api_secret,
            "enableRateLimit": True,
            "options": {"adjustForTimeDifference": True, "fetchCurrencies": False},
        }
    )
    futures = ccxt_async.binanceusdm(
        {
            "apiKey": settings.api_key,
            "secret": settings.api_secret,
            "enableRateLimit": True,
            "options": {"adjustForTimeDifference": True, "fetchCurrencies": False},
        }
    )

    class _CcxtCollateral:
        async def transfer_spot_to_futures(self, amount_usdt: float):
            # Binance universal transfer: MAIN_UMFUTURE
            return await spot.sapi_post_asset_transfer(
                {"type": "MAIN_UMFUTURE", "asset": "USDT", "amount": amount_usdt}
            )

        async def transfer_futures_to_spot(self, amount_usdt: float):
            return await spot.sapi_post_asset_transfer(
                {"type": "UMFUTURE_MAIN", "asset": "USDT", "amount": amount_usdt}
            )

    return FundingArbExecutor(spot=spot, futures=futures, collateral=_CcxtCollateral())


async def _resolve_universe(settings: FundingArbSettings) -> list[str]:
    if settings.universe:
        return list(settings.universe)
    from src.funding_arb_backtest import _fetch_perp_universe

    return await _fetch_perp_universe(
        max_symbols=settings.dynamic_universe_size,
        min_quote_volume_usdt=settings.min_quote_volume_usdt,
    )


async def _fetch_current_signals(symbols: Sequence[str]) -> list[CandidateSignal]:
    """Pull current funding rate + mark price + spot price for each symbol."""
    import aiohttp

    rest_symbols = [s.replace("/", "").upper() for s in symbols]
    out: list[CandidateSignal] = []
    async with aiohttp.ClientSession() as session:
        async with session.get("https://fapi.binance.com/fapi/v1/premiumIndex") as resp:
            resp.raise_for_status()
            mark_rows = await resp.json()
        async with session.get("https://api.binance.com/api/v3/ticker/price") as resp:
            resp.raise_for_status()
            spot_rows = await resp.json()

    spot_by_symbol = {row["symbol"]: float(row["price"]) for row in spot_rows}
    mark_by_symbol = {row["symbol"]: row for row in mark_rows}

    for ccxt_symbol, rest_symbol in zip(symbols, rest_symbols):
        mark_row = mark_by_symbol.get(rest_symbol)
        if mark_row is None:
            continue
        try:
            funding_bps = float(mark_row["lastFundingRate"]) * 10_000.0
            mark_price = float(mark_row["markPrice"])
        except (KeyError, TypeError, ValueError):
            continue
        spot_price = spot_by_symbol.get(rest_symbol, 0.0)
        if spot_price <= 0:
            continue
        out.append(
            CandidateSignal(
                symbol=ccxt_symbol,
                funding_bps=funding_bps,
                mark_price=mark_price,
                spot_price=spot_price,
            )
        )
    return out


async def run_cycle(
    settings: FundingArbSettings,
    *,
    mode: str = "plan_only",
    executor: Any | None = None,
) -> dict[str, Any]:
    """Run one decision cycle. Modes:

    * ``plan_only`` — compute decisions; do not mutate state or submit orders.
    * ``paper`` — compute decisions, materialize them in local state,
      simulate funding accrual and round-trip costs; no real orders.
    * ``live`` — execute decisions against the real exchange using the
      provided ``executor`` (a ``FundingArbExecutor``). Reconciles
      on-exchange state with local state first and halts on mismatch.

    Returns the audit payload, which is also appended to the audit JSONL.
    """
    if mode not in {"plan_only", "paper", "live"}:
        raise FundingArbError(f"unknown funding-arb mode: {mode}")
    _preflight(settings, submit_order=(mode == "live"))

    state = load_state(settings.state_path)
    symbols = await _resolve_universe(settings)
    signals = await _fetch_current_signals(symbols)
    advisor = FundingArbAdvisor(settings)
    decisions = advisor.decide(state=state, signals=signals)

    ledger: list[PaperLedgerEntry] = []
    live_results: list[dict[str, Any]] = []
    halt_reason: str | None = None

    if mode == "paper":
        cycle_ts_ms = _utc_now_ms()
        state, ledger = apply_paper_decisions(
            settings=settings,
            state=state,
            signals=signals,
            decisions=decisions,
            cycle_ts_ms=cycle_ts_ms,
        )
        save_state(state, settings.state_path)
        append_ledger(settings.ledger_path, ledger)

    if mode == "live":
        if executor is None:
            raise FundingArbError("live mode requires an executor instance")
        # Reconcile first; halt on mismatch.
        report = await executor.reconcile({p.symbol: p for p in state.positions})
        if not report.ok:
            halt_reason = f"reconciliation mismatch: {[m.detail for m in report.mismatches]}"
        else:
            state, ledger, live_results = await _execute_live_decisions(
                settings=settings,
                state=state,
                signals=signals,
                decisions=decisions,
                executor=executor,
            )
            save_state(state, settings.state_path)
            append_ledger(settings.ledger_path, ledger)

    payload = {
        "timestamp": _utc_timestamp(),
        "mode": mode,
        "universe_size": len(symbols),
        "signals_count": len(signals),
        "open_positions": len(state.positions),
        "decisions": [asdict(d) for d in decisions],
        "ledger_entries_this_cycle": [asdict(e) for e in ledger],
        "live_results": live_results,
        "halt_reason": halt_reason,
        "top_signals": sorted(
            [
                {"symbol": s.symbol, "funding_bps": s.funding_bps, "basis_bps": s.basis_bps()}
                for s in signals
            ],
            key=lambda r: r["funding_bps"],
            reverse=True,
        )[:10],
    }
    append_audit(settings.audit_path, payload)
    if halt_reason is not None:
        raise FundingArbError(halt_reason)
    return payload


async def _execute_live_decisions(
    *,
    settings: FundingArbSettings,
    state: FundingArbState,
    signals: Sequence[CandidateSignal],
    decisions: Sequence[FundingArbDecision],
    executor: Any,
) -> tuple[FundingArbState, list[PaperLedgerEntry], list[dict[str, Any]]]:
    """Run live closes first (free capacity), then opens. Update state from results."""
    cycle_ts_ms = _utc_now_ms()
    signals_by_symbol = {s.symbol: s for s in signals}
    positions_by_symbol = {p.symbol: p for p in state.positions}
    close_symbols = [d.symbol for d in decisions if d.action == "close"]
    open_decisions = [d for d in decisions if d.action == "open"]

    one_side_cost_bps = (
        settings.taker_fee_spot_bps
        + settings.taker_fee_futures_bps
        + 2.0 * settings.slippage_per_leg_bps
    )
    round_trip_cost_bps = 2.0 * one_side_cost_bps

    ledger: list[PaperLedgerEntry] = []
    live_results: list[dict[str, Any]] = []

    # 1. Accrue current funding on still-held positions (those NOT being closed).
    held_after: dict[str, FundingArbPosition] = {}
    for symbol, position in positions_by_symbol.items():
        signal = signals_by_symbol.get(symbol)
        if signal is not None and symbol not in close_symbols:
            funding_increment_usdt = position.notional_usdt * (signal.funding_bps / 10_000.0)
            position.funding_received_bps += signal.funding_bps
            position.funding_received_usdt += funding_increment_usdt
            position.events_held += 1
        held_after[symbol] = position

    # 2. Execute closes.
    for symbol in close_symbols:
        position = positions_by_symbol.get(symbol)
        if position is None:
            continue
        # Accrue the final funding event before closing too.
        signal = signals_by_symbol.get(symbol)
        if signal is not None:
            funding_increment_usdt = position.notional_usdt * (signal.funding_bps / 10_000.0)
            position.funding_received_bps += signal.funding_bps
            position.funding_received_usdt += funding_increment_usdt
            position.events_held += 1
        try:
            close_result = await executor.close_position(position)
        except FundingArbError as exc:
            live_results.append(
                {"action": "close", "symbol": symbol, "ok": False, "error": str(exc)}
            )
            # Keep the position in state if close failed; manual intervention.
            continue
        ledger.append(
            PaperLedgerEntry(
                symbol=symbol,
                open_time_ms=position.open_time_ms,
                close_time_ms=cycle_ts_ms,
                entry_funding_bps=position.entry_funding_bps,
                funding_received_bps=position.funding_received_bps,
                funding_received_usdt=position.funding_received_usdt,
                events_held=position.events_held,
                round_trip_cost_bps=round_trip_cost_bps,
                round_trip_cost_usdt=position.notional_usdt * (round_trip_cost_bps / 10_000.0),
                net_pnl_bps=position.funding_received_bps - round_trip_cost_bps,
                net_pnl_usdt=close_result.realized_pnl_usdt - position.notional_usdt * (round_trip_cost_bps / 10_000.0),
            )
        )
        live_results.append(
            {
                "action": "close",
                "symbol": symbol,
                "ok": True,
                "realized_pnl_usdt": close_result.realized_pnl_usdt,
                "spot_order_id": close_result.spot_leg.order_id,
                "perp_order_id": close_result.perp_leg.order_id,
            }
        )
        held_after.pop(symbol, None)

    # 3. Execute opens.
    for decision in open_decisions:
        try:
            open_result = await executor.open_position(
                decision.symbol,
                notional_usdt=decision.notional_usdt,
                entry_funding_bps=decision.current_funding_bps,
                open_time_ms=cycle_ts_ms,
            )
        except FundingArbError as exc:
            live_results.append(
                {"action": "open", "symbol": decision.symbol, "ok": False, "error": str(exc)}
            )
            continue
        held_after[decision.symbol] = open_result.position
        live_results.append(
            {
                "action": "open",
                "symbol": decision.symbol,
                "ok": True,
                "spot_order_id": open_result.spot_leg.order_id,
                "perp_order_id": open_result.perp_leg.order_id,
                "filled_qty": open_result.spot_leg.filled_qty,
            }
        )

    new_state = FundingArbState(positions=tuple(held_after.values()))
    return new_state, ledger, live_results


async def plan_cycle(settings: FundingArbSettings) -> dict[str, Any]:
    """Backwards-compatible thin wrapper that runs a plan_only cycle."""
    return await run_cycle(settings, mode="plan_only")


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Funding-arb runtime. Plan-only by default; paper and live are opt-in."
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--plan-only",
        action="store_const",
        const="plan_only",
        dest="mode",
        help="compute decisions without mutating state (default)",
    )
    group.add_argument(
        "--paper",
        action="store_const",
        const="paper",
        dest="mode",
        help="materialize decisions in local state with simulated funding/fees, no real orders",
    )
    group.add_argument(
        "--submit-live",
        action="store_const",
        const="live",
        dest="mode",
        help="reserved for layer C live execution; currently raises after preflight gating",
    )
    parser.set_defaults(mode="plan_only")
    args = parser.parse_args(argv)

    try:
        settings = FundingArbSettings.from_env()
        if args.mode == "live":
            result = asyncio.run(_run_live(settings))
        else:
            result = asyncio.run(run_cycle(settings, mode=args.mode))
    except FundingArbError as exc:
        print(f"funding-arb failed: {exc}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


async def _run_live(settings: FundingArbSettings) -> dict[str, Any]:
    executor = await build_live_executor(settings)
    try:
        return await run_cycle(settings, mode="live", executor=executor)
    finally:
        # Close any underlying aiohttp sessions ccxt owns.
        try:
            await executor.spot.close()
        except Exception:  # noqa: BLE001 — best-effort cleanup
            pass
        try:
            await executor.futures.close()
        except Exception:  # noqa: BLE001 — best-effort cleanup
            pass


if __name__ == "__main__":
    raise SystemExit(run())

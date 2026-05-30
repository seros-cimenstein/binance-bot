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
        for path_name in ("state_path", "audit_path", "emergency_stop_path"):
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


def _utc_timestamp() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


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


async def plan_cycle(settings: FundingArbSettings) -> dict[str, Any]:
    """Run one decision cycle without submitting orders. Returns the audit payload."""
    _preflight(settings, submit_order=False)
    state = load_state(settings.state_path)
    symbols = await _resolve_universe(settings)
    signals = await _fetch_current_signals(symbols)
    advisor = FundingArbAdvisor(settings)
    decisions = advisor.decide(state=state, signals=signals)
    payload = {
        "timestamp": _utc_timestamp(),
        "mode": "plan_only",
        "universe_size": len(symbols),
        "signals_count": len(signals),
        "open_positions": len(state.positions),
        "decisions": [asdict(d) for d in decisions],
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
    return payload


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Funding-arb runtime. Plan-only by default; live submission gated."
    )
    parser.add_argument("--plan-only", action="store_true", default=True, help="(default)")
    parser.add_argument(
        "--submit-live",
        action="store_true",
        help="reserved for future live-execution phase; currently raises",
    )
    args = parser.parse_args(argv)

    try:
        settings = FundingArbSettings.from_env()
        if args.submit_live:
            # Force preflight to surface the gating message.
            _preflight(settings, submit_order=True)
            raise FundingArbError(
                "live execution path is not yet implemented; only plan-only is supported"
            )
        result = asyncio.run(plan_cycle(settings))
    except FundingArbError as exc:
        print(f"funding-arb failed: {exc}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

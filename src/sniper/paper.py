"""Forward-paper executor for the Solana memecoin sniper.

Consumes the ``TradeCandidate`` stream from ``src.sniper.pipeline`` and,
for each tradeable candidate:

  1. Fetches a Jupiter quote: SOL → token (entry price)
  2. Records a simulated buy at that quote
  3. Spawns a background task that sleeps ``hold_seconds`` then fetches
     SOL ← token (exit price) and writes the realised P&L
  4. Bounded concurrency via a semaphore so we don't flood Jupiter / RPC

No real signing, no real swaps, no SOL spent. Output is a JSONL ledger
of simulated round-trips with entry / exit prices and net P&L in SOL +
USD. After accumulating real ledger data we can decide whether to
build the Phase 5 live executor.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import time
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import aiohttp

from src.sniper.jupiter import (
    DEFAULT_QUOTE_URL,
    JupiterQuote,
    JupiterQuoteError,
    SOL_MINT,
    get_quote,
)
from src.sniper.pipeline import TradeCandidate, candidate_stream


@dataclass(frozen=True, slots=True)
class ExecutionFriction:
    """Models the gap between Jupiter-quote paper fills and live Solana execution.

    Three sources of friction the existing paper executor ignored:

      * **Latency** — between candidate detection and our buy hitting the chain,
        faster snipers move the price. Modeled as a pre-quote sleep so Jupiter
        gives us the post-front-running price.
      * **Fill probability** — not every submitted tx lands. Lost slot races,
        compute-unit caps, expired blockhashes. ``entry_fill_prob`` < 1.0
        means a fraction of attempts burn gas without taking a position;
        ``exit_fill_prob`` < 1.0 means we can be stuck holding (treated as a
        full loss of position, conservative-but-defensible — a tx that
        won't land is the same as a slow rug from our P&L's perspective).
      * **Gas** — base fee burns on every submitted tx (landed or not);
        priority fee only on landed.

    Pass ``ExecutionFriction.zero()`` for the historic idealized paper behavior.
    """

    profile_id: str = "pi_realism_v1"
    entry_latency_ms: int = 800
    entry_latency_jitter_ms: int = 600
    entry_fill_prob: float = 0.70
    exit_fill_prob: float = 0.95
    base_fee_lamports: int = 5_000
    priority_fee_lamports: int = 20_000

    @classmethod
    def zero(cls) -> "ExecutionFriction":
        return cls(
            profile_id="zero",
            entry_latency_ms=0, entry_latency_jitter_ms=0,
            entry_fill_prob=1.0, exit_fill_prob=1.0,
            base_fee_lamports=0, priority_fee_lamports=0,
        )

    def sampled_entry_latency_ms(self, rng: random.Random) -> int:
        if self.entry_latency_jitter_ms <= 0:
            return self.entry_latency_ms
        return self.entry_latency_ms + rng.randint(0, self.entry_latency_jitter_ms)


DEFAULT_FRICTION = ExecutionFriction()


@dataclass(frozen=True, slots=True)
class PaperSnipeRecord:
    """One simulated round-trip on a sniped token."""

    mint: str
    symbol: str
    name: str
    entry_time_ms: int
    exit_time_ms: int
    hold_seconds: int
    entry_in_sol_atoms: int   # what we "spent" on the buy (0 if entry never landed)
    entry_out_token_atoms: int  # what we "received"
    exit_out_sol_atoms: int   # what we'd get back at exit
    sol_pnl_atoms: int        # exit_out - entry_in - gas_paid_lamports
    sol_pnl_pct: float
    entry_usd_value: float
    exit_usd_value: float
    usd_pnl: float
    entry_route_labels: tuple[str, ...]
    exit_route_labels: tuple[str, ...]
    # "round_trip" | "no_exit_route" | "exit_error"
    # | "entry_failed_tx" | "exit_failed_tx"
    exit_status: str
    error: str = ""
    # Friction-layer fields (default to zero-friction values so old ledger
    # rows deserialized without these still validate)
    friction_profile_id: str = "zero"
    entry_landed: bool = True
    exit_landed: bool = True
    gas_paid_lamports: int = 0
    entry_latency_ms_applied: int = 0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def _utc_timestamp() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


def _append_ledger(path: str, record: PaperSnipeRecord) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(record.as_dict(), sort_keys=True))
        fh.write("\n")


def _usd_per_sol_atom(quote: JupiterQuote) -> float:
    """Estimate SOL→USD rate from a quote's reported swap dollar value.

    Used to convert gas (lamports) into USD without doing another quote.
    Falls back to 0.0 if the swap value or amount is missing.
    """
    if quote.in_amount <= 0 or quote.swap_usd_value <= 0:
        return 0.0
    # quote may have been SOL→token (in_amount in lamports) or token→SOL.
    if quote.input_mint == SOL_MINT:
        return quote.swap_usd_value / quote.in_amount
    if quote.output_mint == SOL_MINT and quote.out_amount > 0:
        return quote.swap_usd_value / quote.out_amount
    return 0.0


async def simulate_snipe(
    *,
    candidate: TradeCandidate,
    entry_sol_atoms: int,
    hold_seconds: int,
    slippage_bps: int,
    quote_url: str,
    session: aiohttp.ClientSession,
    ledger_path: str,
    friction: ExecutionFriction = DEFAULT_FRICTION,
    rng: random.Random | None = None,
) -> PaperSnipeRecord | None:
    """Simulate one entry → hold → exit round-trip on a candidate.

    Returns the record on success, ``None`` if the entry quote itself
    failed (no route available right at launch). Applies ``friction``
    to model latency, fill probability, and gas — pass
    ``ExecutionFriction.zero()`` for the historic idealized behavior.
    """
    mint = candidate.create_event.mint
    symbol = candidate.create_event.symbol
    name = candidate.create_event.name
    if rng is None:
        rng = random.Random()

    # Latency: sleep before quoting so Jupiter prices the post-front-running
    # state. Faster snipers have already moved the pool by the time we ask.
    entry_latency_applied = friction.sampled_entry_latency_ms(rng)
    if entry_latency_applied > 0:
        await asyncio.sleep(entry_latency_applied / 1000.0)

    # Entry quote: SOL → token
    try:
        buy = await get_quote(
            input_mint=SOL_MINT, output_mint=mint,
            amount_atoms=entry_sol_atoms, slippage_bps=slippage_bps,
            quote_url=quote_url, session=session,
        )
    except JupiterQuoteError:
        return None
    entry_time_ms = _utc_now_ms()
    token_atoms_received = buy.out_amount
    usd_per_lamport = _usd_per_sol_atom(buy)

    # Entry fill roll: did our submitted tx actually land?
    entry_landed = rng.random() < friction.entry_fill_prob
    if not entry_landed:
        gas_paid = friction.base_fee_lamports
        gas_usd = gas_paid * usd_per_lamport
        sol_pnl_atoms = -gas_paid
        sol_pnl_pct = (sol_pnl_atoms / entry_sol_atoms * 100.0
                       if entry_sol_atoms > 0 else 0.0)
        record = PaperSnipeRecord(
            mint=mint, symbol=symbol, name=name,
            entry_time_ms=entry_time_ms, exit_time_ms=entry_time_ms,
            hold_seconds=hold_seconds,
            entry_in_sol_atoms=0,
            entry_out_token_atoms=0,
            exit_out_sol_atoms=0,
            sol_pnl_atoms=sol_pnl_atoms, sol_pnl_pct=sol_pnl_pct,
            entry_usd_value=0.0, exit_usd_value=0.0,
            usd_pnl=-gas_usd,
            entry_route_labels=buy.route_labels,
            exit_route_labels=(),
            exit_status="entry_failed_tx",
            error="entry tx did not land",
            friction_profile_id=friction.profile_id,
            entry_landed=False, exit_landed=False,
            gas_paid_lamports=gas_paid,
            entry_latency_ms_applied=entry_latency_applied,
        )
        _append_ledger(ledger_path, record)
        return record

    # Entry landed: base + priority fee
    gas_paid = friction.base_fee_lamports + friction.priority_fee_lamports

    await asyncio.sleep(hold_seconds)

    # Exit quote: token → SOL
    exit_status = "round_trip"
    error = ""
    sell: JupiterQuote | None = None
    try:
        sell = await get_quote(
            input_mint=mint, output_mint=SOL_MINT,
            amount_atoms=token_atoms_received, slippage_bps=slippage_bps,
            quote_url=quote_url, session=session,
        )
    except JupiterQuoteError as exc:
        # No liquidity on the other side — tx never submitted, no extra gas.
        exit_status = "no_exit_route"
        error = str(exc)[:200]
    except Exception as exc:  # noqa: BLE001 — should not abort other positions
        exit_status = "exit_error"
        error = f"{type(exc).__name__}: {str(exc)[:200]}"

    exit_landed = True
    if sell is not None:
        # Exit fill roll: did the sell tx land?
        exit_landed = rng.random() < friction.exit_fill_prob
        if not exit_landed:
            exit_status = "exit_failed_tx"
            error = "exit tx did not land — position lost"
            # Base fee burned on the failed exit attempt; treat position as lost.
            gas_paid += friction.base_fee_lamports
            sell = None  # exit_out = 0 below
        else:
            gas_paid += friction.base_fee_lamports + friction.priority_fee_lamports

    exit_out = sell.out_amount if sell is not None else 0
    sol_pnl_atoms = exit_out - entry_sol_atoms - gas_paid
    sol_pnl_pct = (
        (sol_pnl_atoms / entry_sol_atoms) * 100.0
        if entry_sol_atoms > 0 else 0.0
    )
    entry_usd = buy.swap_usd_value
    exit_usd = sell.swap_usd_value if sell is not None else 0.0
    gas_usd = gas_paid * usd_per_lamport
    usd_pnl = exit_usd - entry_usd - gas_usd

    record = PaperSnipeRecord(
        mint=mint, symbol=symbol, name=name,
        entry_time_ms=entry_time_ms, exit_time_ms=_utc_now_ms(),
        hold_seconds=hold_seconds,
        entry_in_sol_atoms=entry_sol_atoms,
        entry_out_token_atoms=token_atoms_received,
        exit_out_sol_atoms=exit_out,
        sol_pnl_atoms=sol_pnl_atoms, sol_pnl_pct=sol_pnl_pct,
        entry_usd_value=entry_usd, exit_usd_value=exit_usd,
        usd_pnl=usd_pnl,
        entry_route_labels=buy.route_labels,
        exit_route_labels=sell.route_labels if sell is not None else (),
        exit_status=exit_status, error=error,
        friction_profile_id=friction.profile_id,
        entry_landed=True, exit_landed=exit_landed,
        gas_paid_lamports=gas_paid,
        entry_latency_ms_applied=entry_latency_applied,
    )
    _append_ledger(ledger_path, record)
    return record


async def run_paper_executor(
    *,
    entry_sol_atoms: int = 10_000_000,    # 0.01 SOL
    hold_seconds: int = 300,              # 5 minutes
    slippage_bps: int = 500,
    max_concurrent: int = 5,
    quote_url: str = DEFAULT_QUOTE_URL,
    ledger_path: str = "data/sniper-paper-ledger.jsonl",
    summary_path: str = "data/sniper-paper-summary.json",
    max_candidates: int | None = None,
    duration_seconds: float | None = None,
    friction: ExecutionFriction = DEFAULT_FRICTION,
) -> dict[str, Any]:
    """Drive the candidate stream + simulate each pick.

    Returns a summary dict with counts, win rate, total simulated P&L.
    """
    started_at_ms = _utc_now_ms()
    queued = 0
    skipped_at_capacity = 0
    in_flight: set[asyncio.Task[PaperSnipeRecord | None]] = set()

    async def _simulate(candidate: TradeCandidate, session: aiohttp.ClientSession) -> PaperSnipeRecord | None:
        return await simulate_snipe(
            candidate=candidate,
            entry_sol_atoms=entry_sol_atoms,
            hold_seconds=hold_seconds,
            slippage_bps=slippage_bps,
            quote_url=quote_url, session=session,
            ledger_path=ledger_path,
            friction=friction,
        )

    async def _drive() -> None:
        nonlocal queued, skipped_at_capacity
        async with aiohttp.ClientSession() as session:
            async for candidate in candidate_stream(http_session=session):
                # Drop fully-completed tasks first so the count is accurate.
                in_flight.difference_update({t for t in in_flight if t.done()})
                if len(in_flight) >= max(1, max_concurrent):
                    skipped_at_capacity += 1
                    continue
                queued += 1
                task = asyncio.create_task(_simulate(candidate, session))
                in_flight.add(task)
                task.add_done_callback(in_flight.discard)
                if max_candidates is not None and queued >= max_candidates:
                    break
            if in_flight:
                await asyncio.gather(*in_flight, return_exceptions=True)

    try:
        if duration_seconds is not None:
            try:
                await asyncio.wait_for(_drive(), timeout=duration_seconds)
            except asyncio.TimeoutError:
                # Let in-flight simulations finish so we get their exits.
                if in_flight:
                    await asyncio.gather(*in_flight, return_exceptions=True)
        else:
            await _drive()
    finally:
        pass

    # Build summary from ledger file (covers everything written to disk).
    records: list[dict[str, Any]] = []
    if Path(ledger_path).exists():
        with Path(ledger_path).open() as fh:
            for line in fh:
                if not line.strip():
                    continue
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    relevant = [r for r in records if int(r.get("entry_time_ms", 0)) >= started_at_ms]
    completed = [r for r in relevant if r.get("exit_status") == "round_trip"]
    entry_failed = [r for r in relevant if r.get("exit_status") == "entry_failed_tx"]
    exit_failed = [r for r in relevant if r.get("exit_status") == "exit_failed_tx"]
    wins = sum(1 for r in completed if r.get("sol_pnl_atoms", 0) > 0)
    losses = sum(1 for r in completed if r.get("sol_pnl_atoms", 0) < 0)
    # Round-trip-only P&L (the historic, idealized "did the strategy pick well"
    # number) and net-of-everything P&L (the realistic number including gas
    # burned on failed entries and positions lost to failed exits).
    total_sol_pnl_round_trips = sum(int(r.get("sol_pnl_atoms", 0)) for r in completed)
    total_usd_pnl_round_trips = sum(float(r.get("usd_pnl", 0)) for r in completed)
    net_sol_pnl_atoms = sum(int(r.get("sol_pnl_atoms", 0)) for r in relevant)
    net_usd_pnl = sum(float(r.get("usd_pnl", 0)) for r in relevant)
    total_gas_lamports = sum(int(r.get("gas_paid_lamports", 0)) for r in relevant)
    summary = {
        "started_at": datetime.fromtimestamp(started_at_ms / 1000, tz=timezone.utc).isoformat(),
        "finished_at": _utc_timestamp(),
        "candidates_queued": queued,
        "candidates_skipped_at_capacity": skipped_at_capacity,
        "simulations_recorded": len(relevant),
        "round_trips_completed": len(completed),
        "entry_failed_tx_count": len(entry_failed),
        "exit_failed_tx_count": len(exit_failed),
        "wins": wins, "losses": losses,
        "win_rate_pct": (wins / max(wins + losses, 1)) * 100.0,
        "round_trip_sol_pnl_atoms": total_sol_pnl_round_trips,
        "round_trip_sol_pnl": total_sol_pnl_round_trips / 1_000_000_000.0,
        "round_trip_usd_pnl": round(total_usd_pnl_round_trips, 6),
        "net_sol_pnl_atoms": net_sol_pnl_atoms,
        "net_sol_pnl": net_sol_pnl_atoms / 1_000_000_000.0,
        "net_usd_pnl": round(net_usd_pnl, 6),
        "total_gas_lamports": total_gas_lamports,
        "total_gas_sol": total_gas_lamports / 1_000_000_000.0,
        "entry_sol_atoms": entry_sol_atoms,
        "hold_seconds": hold_seconds,
        "friction_profile_id": friction.profile_id,
    }
    Path(summary_path).parent.mkdir(parents=True, exist_ok=True)
    Path(summary_path).write_text(json.dumps(summary, indent=2, sort_keys=True))
    return summary


def _friction_from_args(args: argparse.Namespace) -> ExecutionFriction:
    """Build an ExecutionFriction from CLI flags, env vars, and the preset.

    Precedence (highest first): CLI flag > env var > preset default.
    """
    if args.friction_profile == "zero":
        base = ExecutionFriction.zero()
    else:
        base = ExecutionFriction()  # pi_realism_v1

    def _resolve_int(cli_val: int | None, env_key: str, default: int) -> int:
        if cli_val is not None:
            return cli_val
        env_val = os.getenv(env_key)
        return int(env_val) if env_val is not None else default

    def _resolve_float(cli_val: float | None, env_key: str, default: float) -> float:
        if cli_val is not None:
            return cli_val
        env_val = os.getenv(env_key)
        return float(env_val) if env_val is not None else default

    return ExecutionFriction(
        profile_id=args.friction_profile,
        entry_latency_ms=_resolve_int(args.entry_latency_ms,
                                      "SNIPER_ENTRY_LATENCY_MS",
                                      base.entry_latency_ms),
        entry_latency_jitter_ms=_resolve_int(args.entry_latency_jitter_ms,
                                             "SNIPER_ENTRY_LATENCY_JITTER_MS",
                                             base.entry_latency_jitter_ms),
        entry_fill_prob=_resolve_float(args.entry_fill_prob,
                                       "SNIPER_ENTRY_FILL_PROB",
                                       base.entry_fill_prob),
        exit_fill_prob=_resolve_float(args.exit_fill_prob,
                                      "SNIPER_EXIT_FILL_PROB",
                                      base.exit_fill_prob),
        base_fee_lamports=_resolve_int(args.base_fee_lamports,
                                       "SNIPER_BASE_FEE_LAMPORTS",
                                       base.base_fee_lamports),
        priority_fee_lamports=_resolve_int(args.priority_fee_lamports,
                                           "SNIPER_PRIORITY_FEE_LAMPORTS",
                                           base.priority_fee_lamports),
    )


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run the Solana sniper in forward-paper mode: detect, "
                    "decode, score, quote on Jupiter, simulate hold + exit, "
                    "record realised P&L to a JSONL ledger."
    )
    parser.add_argument("--entry-sol", type=float, default=0.01,
                        help="SOL per simulated buy (default 0.01)")
    parser.add_argument("--hold-seconds", type=int, default=300)
    parser.add_argument("--slippage-bps", type=int, default=500)
    parser.add_argument("--max-concurrent", type=int, default=5)
    parser.add_argument("--quote-url", default=DEFAULT_QUOTE_URL)
    parser.add_argument("--ledger-path",
                        default=os.getenv("SNIPER_PAPER_LEDGER",
                                          "data/sniper-paper-ledger.jsonl"))
    parser.add_argument("--summary-path",
                        default=os.getenv("SNIPER_PAPER_SUMMARY",
                                          "data/sniper-paper-summary.json"))
    parser.add_argument("--max-candidates", type=int, default=None,
                        help="stop after queuing this many simulations")
    parser.add_argument("--duration-seconds", type=float, default=None,
                        help="stop accepting new candidates after this many seconds; "
                             "in-flight simulations are still allowed to finish")
    parser.add_argument("--friction-profile",
                        default=os.getenv("SNIPER_FRICTION_PROFILE", "pi_realism_v1"),
                        choices=["pi_realism_v1", "zero"],
                        help="execution friction preset (default pi_realism_v1)")
    parser.add_argument("--entry-latency-ms", type=int, default=None)
    parser.add_argument("--entry-latency-jitter-ms", type=int, default=None)
    parser.add_argument("--entry-fill-prob", type=float, default=None)
    parser.add_argument("--exit-fill-prob", type=float, default=None)
    parser.add_argument("--base-fee-lamports", type=int, default=None)
    parser.add_argument("--priority-fee-lamports", type=int, default=None)
    args = parser.parse_args(argv)

    entry_atoms = int(args.entry_sol * 1_000_000_000)
    friction = _friction_from_args(args)
    summary = asyncio.run(run_paper_executor(
        entry_sol_atoms=entry_atoms,
        hold_seconds=args.hold_seconds,
        slippage_bps=args.slippage_bps,
        max_concurrent=args.max_concurrent,
        quote_url=args.quote_url,
        ledger_path=args.ledger_path,
        summary_path=args.summary_path,
        max_candidates=args.max_candidates,
        duration_seconds=args.duration_seconds,
        friction=friction,
    ))
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

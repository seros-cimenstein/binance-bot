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
import time
from dataclasses import asdict, dataclass
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
class PaperSnipeRecord:
    """One simulated round-trip on a sniped token."""

    mint: str
    symbol: str
    name: str
    entry_time_ms: int
    exit_time_ms: int
    hold_seconds: int
    entry_in_sol_atoms: int   # what we "spent"
    entry_out_token_atoms: int  # what we "received"
    exit_out_sol_atoms: int   # what we'd get back at exit
    sol_pnl_atoms: int        # exit_out - entry_in
    sol_pnl_pct: float
    entry_usd_value: float
    exit_usd_value: float
    usd_pnl: float
    entry_route_labels: tuple[str, ...]
    exit_route_labels: tuple[str, ...]
    exit_status: str  # "round_trip" | "no_exit_route" | "exit_error"
    error: str = ""

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


async def simulate_snipe(
    *,
    candidate: TradeCandidate,
    entry_sol_atoms: int,
    hold_seconds: int,
    slippage_bps: int,
    quote_url: str,
    session: aiohttp.ClientSession,
    ledger_path: str,
) -> PaperSnipeRecord | None:
    """Simulate one entry → hold → exit round-trip on a candidate.

    Returns the record on success, ``None`` if the entry quote itself
    failed (no route available right at launch).
    """
    mint = candidate.create_event.mint
    symbol = candidate.create_event.symbol
    name = candidate.create_event.name

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
        exit_status = "no_exit_route"
        error = str(exc)[:200]
    except Exception as exc:  # noqa: BLE001 — should not abort other positions
        exit_status = "exit_error"
        error = f"{type(exc).__name__}: {str(exc)[:200]}"

    exit_out = sell.out_amount if sell is not None else 0
    sol_pnl_atoms = exit_out - entry_sol_atoms
    sol_pnl_pct = (
        (exit_out / entry_sol_atoms - 1.0) * 100.0
        if entry_sol_atoms > 0 else 0.0
    )
    entry_usd = buy.swap_usd_value
    exit_usd = sell.swap_usd_value if sell is not None else 0.0
    usd_pnl = exit_usd - entry_usd

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
    wins = sum(1 for r in completed if r.get("sol_pnl_atoms", 0) > 0)
    losses = sum(1 for r in completed if r.get("sol_pnl_atoms", 0) < 0)
    total_sol_pnl = sum(int(r.get("sol_pnl_atoms", 0)) for r in completed)
    total_usd_pnl = sum(float(r.get("usd_pnl", 0)) for r in completed)
    summary = {
        "started_at": datetime.fromtimestamp(started_at_ms / 1000, tz=timezone.utc).isoformat(),
        "finished_at": _utc_timestamp(),
        "candidates_queued": queued,
        "candidates_skipped_at_capacity": skipped_at_capacity,
        "simulations_recorded": len(relevant),
        "round_trips_completed": len(completed),
        "wins": wins, "losses": losses,
        "win_rate_pct": (wins / max(wins + losses, 1)) * 100.0,
        "total_sol_pnl_atoms": total_sol_pnl,
        "total_sol_pnl": total_sol_pnl / 1_000_000_000.0,
        "total_usd_pnl": round(total_usd_pnl, 6),
        "entry_sol_atoms": entry_sol_atoms,
        "hold_seconds": hold_seconds,
    }
    Path(summary_path).parent.mkdir(parents=True, exist_ok=True)
    Path(summary_path).write_text(json.dumps(summary, indent=2, sort_keys=True))
    return summary


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
    args = parser.parse_args(argv)

    entry_atoms = int(args.entry_sol * 1_000_000_000)
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
    ))
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

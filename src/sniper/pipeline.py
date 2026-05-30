"""End-to-end sniper candidate pipeline.

Composes the components built in Phases 1 + 2:

  detector.stream_pool_creations()
      → for each pump.fun event:
            decoder.decode_create_event_from_logs(event.log_excerpt)
            → if mint extracted:
                scorer.score_token(mint)
                → if tradeable:
                    yield TradeCandidate(...)

The output is an async iterator of ``TradeCandidate`` records ready for
the Phase 4 paper executor or Phase 5 live executor to consume.

Raydium events are passed through without mint extraction for now — the
Raydium init log format does not include the embedded mint pubkey the
way pump.fun's Anchor event does, and extracting it requires
``getTransaction`` which is unreliable on public RPC. Future work.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from collections.abc import AsyncIterator
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import aiohttp

from src.sniper.detector import (
    DEFAULT_RPC_WS_URL,
    PoolCreationEvent,
    stream_pool_creations,
)
from src.sniper.pump_fun_decoder import (
    PumpFunCreateEvent,
    decode_create_event_from_logs,
)
from src.sniper.scorer import (
    DEFAULT_RPC_HTTP_URL,
    TokenSafetyScore,
    score_token,
)


@dataclass(frozen=True, slots=True)
class TradeCandidate:
    """A pool creation event that survived mint extraction + safety scoring."""

    pool_event: PoolCreationEvent
    create_event: PumpFunCreateEvent
    safety: TokenSafetyScore
    evaluated_at_ms: int

    def as_dict(self) -> dict[str, Any]:
        return {
            "pool_event": self.pool_event.as_dict(),
            "create_event": asdict(self.create_event),
            "safety": self.safety.as_dict(),
            "evaluated_at_ms": self.evaluated_at_ms,
        }


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


async def candidate_stream(
    *,
    ws_url: str = DEFAULT_RPC_WS_URL,
    rpc_http_url: str = DEFAULT_RPC_HTTP_URL,
    http_session: aiohttp.ClientSession | None = None,
    max_events: int | None = None,
    emit_rejected: bool = False,
) -> AsyncIterator[TradeCandidate]:
    """Yield evaluated TradeCandidate records (tradeable by default).

    Set ``emit_rejected=True`` to also yield candidates whose safety scorer
    returned a rejection reason. Useful for audit / debug runs to understand
    what's being filtered out.
    """
    owns_session = http_session is None
    if http_session is None:
        http_session = aiohttp.ClientSession()
    try:
        emitted = 0
        async for pool_event in stream_pool_creations(ws_url=ws_url, max_events=max_events):
            if pool_event.program_label != "pump_fun":
                continue
            create = decode_create_event_from_logs(pool_event.log_excerpt)
            if create is None:
                continue
            try:
                safety = await score_token(
                    create.mint, rpc_url=rpc_http_url, session=http_session,
                )
            except Exception:  # noqa: BLE001 — scorer errors should not crash the stream
                continue
            if not emit_rejected and not safety.is_tradeable:
                continue
            yield TradeCandidate(
                pool_event=pool_event, create_event=create,
                safety=safety, evaluated_at_ms=_utc_now_ms(),
            )
            emitted += 1
            if max_events is not None and emitted >= max_events:
                return
    finally:
        if owns_session:
            await http_session.close()


# ---------------------------------------------------------------------------
# CLI: run the pipeline and audit candidates
# ---------------------------------------------------------------------------


def _audit(path: str, payload: dict[str, Any]) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True))
        fh.write("\n")


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run the sniper detector→decoder→scorer pipeline and emit "
                    "evaluated TradeCandidate records to JSONL."
    )
    parser.add_argument("--ws-url", default=os.getenv("SNIPER_RPC_WS_URL", DEFAULT_RPC_WS_URL))
    parser.add_argument("--rpc-url", default=os.getenv("SNIPER_RPC_HTTP_URL", DEFAULT_RPC_HTTP_URL))
    parser.add_argument("--audit-path",
                        default=os.getenv("SNIPER_CANDIDATE_AUDIT_PATH",
                                          "data/sniper-candidates.jsonl"))
    parser.add_argument("--max-events", type=int, default=None,
                        help="Stop after evaluating this many candidates "
                             "(tradeable by default; --emit-rejected counts both).")
    parser.add_argument("--emit-rejected", action="store_true",
                        help="Also write rejected candidates to the audit "
                             "(useful for tuning the safety filters).")
    parser.add_argument("--run-seconds", type=float, default=None,
                        help="Stop after this many seconds (smoke-test friendly).")
    args = parser.parse_args(argv)

    async def _drive() -> dict[str, Any]:
        seen = 0
        tradeable = 0
        rejected = 0
        async for candidate in candidate_stream(
            ws_url=args.ws_url, rpc_http_url=args.rpc_url,
            max_events=args.max_events, emit_rejected=args.emit_rejected,
        ):
            _audit(args.audit_path, candidate.as_dict())
            seen += 1
            if candidate.safety.is_tradeable:
                tradeable += 1
            else:
                rejected += 1
        return {"seen": seen, "tradeable": tradeable, "rejected": rejected,
                "audit_path": args.audit_path}

    async def _drive_with_timeout() -> dict[str, Any]:
        if args.run_seconds is None:
            return await _drive()
        try:
            return await asyncio.wait_for(_drive(), timeout=args.run_seconds)
        except asyncio.TimeoutError:
            return {"timed_out_after_seconds": args.run_seconds,
                    "audit_path": args.audit_path}

    result = asyncio.run(_drive_with_timeout())
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

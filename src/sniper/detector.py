"""Solana new-pool detector via WebSocket log subscriptions.

Subscribes to the Solana JSON-RPC ``logsSubscribe`` method for each
configured launchpad program. For each notification, scans the log lines
for instruction patterns that indicate a fresh pool / bonding-curve was
just initialised. Yields one ``PoolCreationEvent`` per detection.

Pure aiohttp WebSocket client; no ``solana-py`` / ``solders`` dependency
for Phase 1. The transaction signature in each event can be fed into a
follow-up RPC call (``getTransaction``) to extract token mint addresses,
liquidity, etc. — that enrichment lives in the scorer phase, not here.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import signal
from collections.abc import AsyncIterator, Iterable, Mapping, Sequence
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import aiohttp


# Mainnet program IDs for the two highest-volume Solana launchpads.
RAYDIUM_AMM_V4_PROGRAM_ID = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
PUMP_FUN_PROGRAM_ID = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"

DEFAULT_PROGRAMS: dict[str, str] = {
    "raydium_amm_v4": RAYDIUM_AMM_V4_PROGRAM_ID,
    "pump_fun": PUMP_FUN_PROGRAM_ID,
}

# Per-launchpad log substrings that indicate a fresh pool creation. Picked
# to avoid false positives from routine swap activity: only the explicit
# initialization instructions show these strings.
DEFAULT_INIT_MARKERS: dict[str, tuple[str, ...]] = {
    "raydium_amm_v4": (
        "Program log: initialize2",
        "Program log: Initialize2",
    ),
    "pump_fun": (
        "Program log: Instruction: Create",
        "Program log: Instruction: InitializeMint2",
    ),
}

DEFAULT_RPC_WS_URL = "wss://api.mainnet-beta.solana.com"


class SniperDetectorError(RuntimeError):
    """Raised when the detector cannot proceed."""


@dataclass(frozen=True, slots=True)
class PoolCreationEvent:
    """One detected pool / bonding-curve creation."""

    program_label: str  # "raydium_amm_v4" | "pump_fun"
    program_id: str
    signature: str
    slot: int
    detected_at_ms: int
    matched_markers: tuple[str, ...]
    log_excerpt: tuple[str, ...]  # full log lines; previously a 5-line truncation

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def _matches_any(logs: Sequence[str], markers: Sequence[str]) -> tuple[str, ...]:
    """Return the subset of markers present in the log lines."""
    matched: list[str] = []
    blob = "\n".join(logs)
    for marker in markers:
        if marker in blob:
            matched.append(marker)
    return tuple(matched)


def parse_log_notification(
    payload: Mapping[str, Any],
    *,
    program_label: str,
    program_id: str,
    init_markers: Sequence[str],
) -> PoolCreationEvent | None:
    """Parse one ``logsNotification`` payload; return event or None."""
    if not isinstance(payload, Mapping):
        return None
    method = payload.get("method")
    if method != "logsNotification":
        return None
    params = payload.get("params") or {}
    if not isinstance(params, Mapping):
        return None
    result = params.get("result") or {}
    if not isinstance(result, Mapping):
        return None
    value = result.get("value") or {}
    if not isinstance(value, Mapping):
        return None
    err = value.get("err")
    if err is not None:
        return None  # failed transaction; nothing to detect
    logs = value.get("logs") or []
    if not isinstance(logs, list):
        return None
    log_strs = [str(line) for line in logs]
    matched = _matches_any(log_strs, init_markers)
    if not matched:
        return None
    signature = str(value.get("signature") or "")
    context = result.get("context") or {}
    slot_raw = context.get("slot") if isinstance(context, Mapping) else None
    try:
        slot = int(slot_raw) if slot_raw is not None else 0
    except (TypeError, ValueError):
        slot = 0
    return PoolCreationEvent(
        program_label=program_label,
        program_id=program_id,
        signature=signature,
        slot=slot,
        detected_at_ms=_utc_now_ms(),
        matched_markers=matched,
        log_excerpt=tuple(log_strs),
    )


async def _subscribe_logs(
    ws: aiohttp.ClientWebSocketResponse,
    *,
    program_id: str,
    sub_id: int,
    commitment: str,
) -> None:
    request = {
        "jsonrpc": "2.0",
        "id": sub_id,
        "method": "logsSubscribe",
        "params": [{"mentions": [program_id]}, {"commitment": commitment}],
    }
    await ws.send_str(json.dumps(request))


async def stream_pool_creations(
    *,
    ws_url: str = DEFAULT_RPC_WS_URL,
    programs: Mapping[str, str] | None = None,
    init_markers: Mapping[str, Sequence[str]] | None = None,
    commitment: str = "processed",
    session: aiohttp.ClientSession | None = None,
    max_events: int | None = None,
) -> AsyncIterator[PoolCreationEvent]:
    """Open a WebSocket, subscribe to each program, yield matched events.

    Cancels cleanly when the task is cancelled. ``max_events`` is provided
    for testing / time-boxed smoke runs.
    """
    programs = programs or DEFAULT_PROGRAMS
    init_markers = init_markers or DEFAULT_INIT_MARKERS
    owns_session = session is None
    if session is None:
        session = aiohttp.ClientSession()
    try:
        async with session.ws_connect(ws_url, heartbeat=30) as ws:
            sub_to_program: dict[int, tuple[str, str]] = {}
            for i, (label, pid) in enumerate(programs.items(), start=1):
                await _subscribe_logs(ws, program_id=pid, sub_id=i, commitment=commitment)
                sub_to_program[i] = (label, pid)

            emitted = 0
            async for msg in ws:
                if msg.type == aiohttp.WSMsgType.TEXT:
                    try:
                        payload = json.loads(msg.data)
                    except json.JSONDecodeError:
                        continue
                    # First a per-subscription confirmation arrives, then logsNotification.
                    if payload.get("method") != "logsNotification":
                        continue
                    sub_id = (payload.get("params") or {}).get("subscription")
                    pair = sub_to_program.get(int(sub_id)) if sub_id is not None else None
                    if pair is None:
                        # Fall back to scanning all configured programs.
                        for label, pid in programs.items():
                            event = parse_log_notification(
                                payload, program_label=label, program_id=pid,
                                init_markers=init_markers.get(label, ()),
                            )
                            if event is not None:
                                yield event
                                emitted += 1
                                break
                    else:
                        label, pid = pair
                        event = parse_log_notification(
                            payload, program_label=label, program_id=pid,
                            init_markers=init_markers.get(label, ()),
                        )
                        if event is not None:
                            yield event
                            emitted += 1
                    if max_events is not None and emitted >= max_events:
                        return
                elif msg.type in (aiohttp.WSMsgType.CLOSE, aiohttp.WSMsgType.CLOSED, aiohttp.WSMsgType.ERROR):
                    break
    finally:
        if owns_session:
            await session.close()


# ---------------------------------------------------------------------------
# CLI runner
# ---------------------------------------------------------------------------


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise SniperDetectorError(f"emergency stop file present at {path}; refusing to run")


def _audit(path: str, payload: Mapping[str, Any]) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True))
        fh.write("\n")


async def run_detector(
    *,
    ws_url: str,
    audit_path: str,
    emergency_stop_path: str,
    run_duration_seconds: float | None = None,
    max_events: int | None = None,
) -> dict[str, Any]:
    _ensure_no_emergency_stop(emergency_stop_path)
    start_ms = _utc_now_ms()
    events: list[PoolCreationEvent] = []
    by_label: dict[str, int] = {}

    async def _accumulate() -> None:
        async for event in stream_pool_creations(ws_url=ws_url, max_events=max_events):
            events.append(event)
            by_label[event.program_label] = by_label.get(event.program_label, 0) + 1
            _audit(audit_path, event.as_dict())
            # The sniper detector fires a lot (≈1/sec on pump.fun). Skip the
            # per-event alert here; alerts come from the scorer phase that
            # filters down to tradeable candidates.
            if Path(emergency_stop_path).exists():
                return

    if run_duration_seconds is not None:
        try:
            await asyncio.wait_for(_accumulate(), timeout=run_duration_seconds)
        except asyncio.TimeoutError:
            pass
    else:
        await _accumulate()

    return {
        "started_at_ms": start_ms,
        "ended_at_ms": _utc_now_ms(),
        "total_events": len(events),
        "by_program_label": by_label,
        "audit_path": audit_path,
    }


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Solana pool-creation detector. Streams logsNotifications "
                    "from the configured launchpad programs and writes detected "
                    "events to a JSONL audit log."
    )
    parser.add_argument("--ws-url", default=os.getenv("SNIPER_RPC_WS_URL", DEFAULT_RPC_WS_URL))
    parser.add_argument("--audit-path", default=os.getenv("SNIPER_AUDIT_PATH", "data/sniper-events.jsonl"))
    parser.add_argument("--emergency-stop-path", default=os.getenv("SNIPER_EMERGENCY_STOP_PATH", "data/sniper.stop"))
    parser.add_argument(
        "--run-seconds", type=float, default=None,
        help="Stop the detector after this many seconds (useful for smoke tests).",
    )
    parser.add_argument(
        "--max-events", type=int, default=None,
        help="Stop after detecting this many events.",
    )
    args = parser.parse_args(argv)

    # Honour SIGINT/SIGTERM cleanly when run from systemd.
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)

    def _stop(*_args: Any) -> None:
        for task in asyncio.all_tasks(loop):
            task.cancel()

    try:
        loop.add_signal_handler(signal.SIGINT, _stop)
        loop.add_signal_handler(signal.SIGTERM, _stop)
    except NotImplementedError:
        # Windows / nested loop fallback — ignore.
        pass

    try:
        result = loop.run_until_complete(run_detector(
            ws_url=args.ws_url,
            audit_path=args.audit_path,
            emergency_stop_path=args.emergency_stop_path,
            run_duration_seconds=args.run_seconds,
            max_events=args.max_events,
        ))
    except SniperDetectorError as exc:
        print(f"sniper-detector failed: {exc}")
        return 1
    except asyncio.CancelledError:
        return 0
    finally:
        loop.close()

    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

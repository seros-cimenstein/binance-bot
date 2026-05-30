"""Backtest harness for macro-event pre-positioning trades.

Hypothesis: scheduled macro releases (FOMC decisions, CPI prints, BTC ETF
decisions, the BTC halving) produce repeatable volatility expansion in
crypto. A bot that pre-positions a few minutes before the event and exits
shortly after may capture the move regardless of direction — provided the
average move exceeds round-trip transaction costs.

The harness ships with a hardcoded list of well-documented historical
events from 2024-2025 (FOMC, CPI, ETF, halving). For each event it
fetches a kline window around the scheduled time and simulates three
strategies:

  * ``long_only``: enter long ``pre_minutes`` before, exit ``post_minutes`` after
  * ``short_only``: enter short
  * ``straddle``: hold both legs, exit both — captures absolute move minus 2x costs

Aggregated metrics include win rate, average P&L per event, and the
verdict.

The backtest is read-only. Live runtime + calendar scraper is a future
phase, conditional on a positive verdict here.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import statistics
from collections.abc import Sequence
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass(frozen=True, slots=True)
class MacroEvent:
    """One scheduled macro event."""

    label: str            # e.g. "FOMC 2024-01-31", "CPI 2024-02-13"
    category: str         # e.g. "fomc", "cpi", "etf", "halving"
    event_time_utc: str   # ISO timestamp


# A non-exhaustive but representative list of well-documented events. The
# timestamps are official release times in UTC; FOMC = 19:00 UTC for
# statement releases in 2024-2025, CPI = 12:30 UTC on release day, etc.
# Sourced from public Federal Reserve / BLS release calendars.
KNOWN_EVENTS_2024_2025: tuple[MacroEvent, ...] = (
    # 2024 FOMC statements (8/year, 19:00 UTC)
    MacroEvent("FOMC 2024-01-31", "fomc", "2024-01-31T19:00:00+00:00"),
    MacroEvent("FOMC 2024-03-20", "fomc", "2024-03-20T18:00:00+00:00"),
    MacroEvent("FOMC 2024-05-01", "fomc", "2024-05-01T18:00:00+00:00"),
    MacroEvent("FOMC 2024-06-12", "fomc", "2024-06-12T18:00:00+00:00"),
    MacroEvent("FOMC 2024-07-31", "fomc", "2024-07-31T18:00:00+00:00"),
    MacroEvent("FOMC 2024-09-18", "fomc", "2024-09-18T18:00:00+00:00"),
    MacroEvent("FOMC 2024-11-07", "fomc", "2024-11-07T19:00:00+00:00"),
    MacroEvent("FOMC 2024-12-18", "fomc", "2024-12-18T19:00:00+00:00"),
    # 2025 FOMC
    MacroEvent("FOMC 2025-01-29", "fomc", "2025-01-29T19:00:00+00:00"),
    MacroEvent("FOMC 2025-03-19", "fomc", "2025-03-19T18:00:00+00:00"),
    MacroEvent("FOMC 2025-05-07", "fomc", "2025-05-07T18:00:00+00:00"),
    MacroEvent("FOMC 2025-06-18", "fomc", "2025-06-18T18:00:00+00:00"),
    MacroEvent("FOMC 2025-07-30", "fomc", "2025-07-30T18:00:00+00:00"),
    MacroEvent("FOMC 2025-09-17", "fomc", "2025-09-17T18:00:00+00:00"),
    MacroEvent("FOMC 2025-10-29", "fomc", "2025-10-29T18:00:00+00:00"),
    MacroEvent("FOMC 2025-12-10", "fomc", "2025-12-10T19:00:00+00:00"),
    # 2024 CPI release dates (~12:30 UTC)
    MacroEvent("CPI 2024-01-11", "cpi", "2024-01-11T13:30:00+00:00"),
    MacroEvent("CPI 2024-02-13", "cpi", "2024-02-13T13:30:00+00:00"),
    MacroEvent("CPI 2024-03-12", "cpi", "2024-03-12T12:30:00+00:00"),
    MacroEvent("CPI 2024-04-10", "cpi", "2024-04-10T12:30:00+00:00"),
    MacroEvent("CPI 2024-05-15", "cpi", "2024-05-15T12:30:00+00:00"),
    MacroEvent("CPI 2024-06-12", "cpi", "2024-06-12T12:30:00+00:00"),
    MacroEvent("CPI 2024-07-11", "cpi", "2024-07-11T12:30:00+00:00"),
    MacroEvent("CPI 2024-08-14", "cpi", "2024-08-14T12:30:00+00:00"),
    MacroEvent("CPI 2024-09-11", "cpi", "2024-09-11T12:30:00+00:00"),
    MacroEvent("CPI 2024-10-10", "cpi", "2024-10-10T12:30:00+00:00"),
    MacroEvent("CPI 2024-11-13", "cpi", "2024-11-13T13:30:00+00:00"),
    MacroEvent("CPI 2024-12-11", "cpi", "2024-12-11T13:30:00+00:00"),
    # 2025 CPI
    MacroEvent("CPI 2025-01-15", "cpi", "2025-01-15T13:30:00+00:00"),
    MacroEvent("CPI 2025-02-12", "cpi", "2025-02-12T13:30:00+00:00"),
    MacroEvent("CPI 2025-03-12", "cpi", "2025-03-12T12:30:00+00:00"),
    MacroEvent("CPI 2025-04-10", "cpi", "2025-04-10T12:30:00+00:00"),
    MacroEvent("CPI 2025-05-13", "cpi", "2025-05-13T12:30:00+00:00"),
    # Special events
    MacroEvent("Spot BTC ETF approval", "etf", "2024-01-10T21:00:00+00:00"),
    MacroEvent("BTC Halving", "halving", "2024-04-19T23:00:00+00:00"),
)


@dataclass(frozen=True, slots=True)
class NewsEventConfig:
    """Pre-position knobs."""

    symbol: str = "BTC/USDT"
    pre_minutes: int = 15
    post_minutes: int = 30
    taker_fee_bps: float = 10.0
    slippage_bps: float = 5.0
    notional_usdt: float = 100.0
    strategy: str = "straddle"  # "long_only" | "short_only" | "straddle"


@dataclass(frozen=True, slots=True)
class EventTrade:
    label: str
    category: str
    event_time_ms: int
    entry_price: float
    exit_price: float
    move_pct: float
    pnl_usdt: float
    fee_usdt: float
    strategy: str


@dataclass(frozen=True, slots=True)
class NewsEventResult:
    config: NewsEventConfig
    events_evaluated: int
    events_skipped_missing_data: int
    num_trades: int
    total_pnl_usdt: float
    avg_pnl_usdt: float
    win_rate_pct: float
    avg_abs_move_pct: float
    sharpe_per_event: float
    by_category_pnl: dict[str, float] = field(default_factory=dict)
    by_category_count: dict[str, int] = field(default_factory=dict)
    trades: list[EventTrade] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        payload = asdict(self)
        payload["config"] = asdict(self.config)
        payload["trades"] = [asdict(t) for t in self.trades]
        return payload

    def verdict(self) -> str:
        if self.events_evaluated == 0:
            return "no_data"
        if self.num_trades == 0:
            return "no_trades"
        # Edge if average P&L per event > 0 AND Sharpe > 0.5 (lower bar than
        # continuous strategies because we're trading discrete events).
        if self.avg_pnl_usdt > 0 and self.sharpe_per_event > 0.5:
            return "edge_likely"
        if self.avg_pnl_usdt > 0:
            return "edge_marginal"
        return "no_edge"


def _parse_iso(value: str) -> datetime:
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _kline_close(klines: Sequence[Sequence[Any]], target_ms: int) -> float | None:
    """Return the close of the kline that contains target_ms; None if not found."""
    if not klines:
        return None
    # Klines are 1-minute bars; open_time is the bar's start.
    closest = None
    closest_dist = None
    for k in klines:
        open_ms = int(k[0])
        # 1-minute bar — open_ms <= target < open_ms + 60_000
        if open_ms <= target_ms < open_ms + 60_000:
            return float(k[4])
        dist = abs(target_ms - open_ms)
        if closest_dist is None or dist < closest_dist:
            closest = k
            closest_dist = dist
    # Fallback: nearest bar within 5 minutes.
    if closest is not None and closest_dist is not None and closest_dist <= 5 * 60_000:
        return float(closest[4])
    return None


def simulate_event(
    event: MacroEvent,
    klines: Sequence[Sequence[Any]],
    cfg: NewsEventConfig,
) -> EventTrade | None:
    """Simulate one pre/post trade for one event using its surrounding klines."""
    event_dt = _parse_iso(event.event_time_utc)
    event_ms = int(event_dt.timestamp() * 1000)
    entry_ms = event_ms - cfg.pre_minutes * 60_000
    exit_ms = event_ms + cfg.post_minutes * 60_000

    entry_px = _kline_close(klines, entry_ms)
    exit_px = _kline_close(klines, exit_ms)
    if entry_px is None or exit_px is None or entry_px <= 0:
        return None

    one_side_cost_rate = (cfg.taker_fee_bps + cfg.slippage_bps) / 10_000.0
    move_pct = (exit_px / entry_px - 1.0) * 100.0
    qty = cfg.notional_usdt / entry_px

    if cfg.strategy == "long_only":
        gross_pnl = qty * (exit_px - entry_px)
        fee = 2.0 * cfg.notional_usdt * one_side_cost_rate
        pnl = gross_pnl - fee
    elif cfg.strategy == "short_only":
        gross_pnl = qty * (entry_px - exit_px)
        fee = 2.0 * cfg.notional_usdt * one_side_cost_rate
        pnl = gross_pnl - fee
    elif cfg.strategy == "straddle":
        # Long + short legs of equal notional; absolute move captured minus
        # twice the round-trip cost.
        absolute_move = abs(exit_px - entry_px) * qty
        fee = 4.0 * cfg.notional_usdt * one_side_cost_rate
        pnl = absolute_move - fee
    else:
        raise ValueError(f"unknown strategy: {cfg.strategy}")

    return EventTrade(
        label=event.label, category=event.category, event_time_ms=event_ms,
        entry_price=entry_px, exit_price=exit_px, move_pct=move_pct,
        pnl_usdt=pnl, fee_usdt=fee, strategy=cfg.strategy,
    )


def replay_events(
    events: Sequence[MacroEvent],
    klines_by_event: dict[str, Sequence[Sequence[Any]]],
    cfg: NewsEventConfig | None = None,
) -> NewsEventResult:
    cfg = cfg or NewsEventConfig()
    trades: list[EventTrade] = []
    skipped = 0
    for event in events:
        klines = klines_by_event.get(event.label, ())
        trade = simulate_event(event, klines, cfg)
        if trade is None:
            skipped += 1
            continue
        trades.append(trade)
    num = len(trades)
    if num == 0:
        return NewsEventResult(
            config=cfg, events_evaluated=len(events),
            events_skipped_missing_data=skipped, num_trades=0,
            total_pnl_usdt=0.0, avg_pnl_usdt=0.0, win_rate_pct=0.0,
            avg_abs_move_pct=0.0, sharpe_per_event=0.0,
        )
    total = sum(t.pnl_usdt for t in trades)
    avg = total / num
    wins = sum(1 for t in trades if t.pnl_usdt > 0)
    losses = sum(1 for t in trades if t.pnl_usdt < 0)
    decisive = wins + losses
    win_rate = (wins / decisive * 100.0) if decisive > 0 else 0.0
    avg_abs_move = sum(abs(t.move_pct) for t in trades) / num
    if num > 1:
        stdev = statistics.pstdev([t.pnl_usdt for t in trades])
        sharpe_per_event = (avg / stdev) if stdev > 0 else 0.0
    else:
        sharpe_per_event = 0.0
    by_cat_pnl: dict[str, float] = {}
    by_cat_count: dict[str, int] = {}
    for t in trades:
        by_cat_pnl[t.category] = by_cat_pnl.get(t.category, 0.0) + t.pnl_usdt
        by_cat_count[t.category] = by_cat_count.get(t.category, 0) + 1
    return NewsEventResult(
        config=cfg, events_evaluated=len(events),
        events_skipped_missing_data=skipped, num_trades=num,
        total_pnl_usdt=total, avg_pnl_usdt=avg,
        win_rate_pct=win_rate, avg_abs_move_pct=avg_abs_move,
        sharpe_per_event=sharpe_per_event,
        by_category_pnl=by_cat_pnl, by_category_count=by_cat_count,
        trades=trades,
    )


def _summary(result: NewsEventResult) -> dict[str, Any]:
    return {
        "verdict": result.verdict(),
        "config": asdict(result.config),
        "events_evaluated": result.events_evaluated,
        "events_skipped_missing_data": result.events_skipped_missing_data,
        "num_trades": result.num_trades,
        "total_pnl_usdt": round(result.total_pnl_usdt, 4),
        "avg_pnl_usdt": round(result.avg_pnl_usdt, 4),
        "win_rate_pct": round(result.win_rate_pct, 2),
        "avg_abs_move_pct": round(result.avg_abs_move_pct, 4),
        "sharpe_per_event": round(result.sharpe_per_event, 4),
        "by_category_pnl": {k: round(v, 4) for k, v in result.by_category_pnl.items()},
        "by_category_count": result.by_category_count,
    }


async def _fetch_event_window(
    event: MacroEvent, *, symbol: str, interval: str,
    pre_minutes: int, post_minutes: int, cache_dir: Path | None,
) -> list[list[Any]]:
    from src.data.history import HistoryFetcher
    from datetime import timedelta

    event_dt = _parse_iso(event.event_time_utc)
    # Pull a window that covers entry through exit with margin.
    start = event_dt - timedelta(minutes=pre_minutes + 5)
    end = event_dt + timedelta(minutes=post_minutes + 5)
    cache_path: Path | None = None
    if cache_dir is not None:
        key = event.label.replace(" ", "_").replace("/", "_")
        cache_path = cache_dir / f"event_{key}.json"
        if cache_path.exists():
            try:
                return json.loads(cache_path.read_text())
            except (OSError, json.JSONDecodeError):
                pass
    fetcher = HistoryFetcher()
    klines = await fetcher.fetch_klines(symbol, interval, start_time=start, end_time=end)
    rows = [list(k) for k in klines]
    if cache_path is not None:
        cache_path.parent.mkdir(parents=True, exist_ok=True)
        cache_path.write_text(json.dumps(rows))
    return rows


def _build_parser() -> argparse.ArgumentParser:
    defaults = NewsEventConfig()
    parser = argparse.ArgumentParser(
        description="Replay BTC pre-position trades around hardcoded macro events."
    )
    parser.add_argument("--symbol", default=defaults.symbol)
    parser.add_argument("--interval", default="1m")
    parser.add_argument("--strategy", default=defaults.strategy,
                        choices=("long_only", "short_only", "straddle"))
    parser.add_argument("--pre-minutes", type=int, default=defaults.pre_minutes)
    parser.add_argument("--post-minutes", type=int, default=defaults.post_minutes)
    parser.add_argument("--taker-fee-bps", type=float, default=defaults.taker_fee_bps)
    parser.add_argument("--slippage-bps", type=float, default=defaults.slippage_bps)
    parser.add_argument("--notional-usdt", type=float, default=defaults.notional_usdt)
    parser.add_argument("--categories", default="",
                        help="comma-separated category filter (fomc,cpi,etf,halving)")
    parser.add_argument("--klines-cache", default="")
    parser.add_argument("--output", default="")
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    cfg = NewsEventConfig(
        symbol=args.symbol, pre_minutes=args.pre_minutes,
        post_minutes=args.post_minutes, taker_fee_bps=args.taker_fee_bps,
        slippage_bps=args.slippage_bps, notional_usdt=args.notional_usdt,
        strategy=args.strategy,
    )
    events = KNOWN_EVENTS_2024_2025
    if args.categories:
        wanted = {c.strip() for c in args.categories.split(",") if c.strip()}
        events = tuple(e for e in events if e.category in wanted)

    cache_dir = Path(args.klines_cache) if args.klines_cache else None

    async def _fetch_all() -> dict[str, list[list[Any]]]:
        out: dict[str, list[list[Any]]] = {}
        for event in events:
            out[event.label] = await _fetch_event_window(
                event, symbol=args.symbol, interval=args.interval,
                pre_minutes=args.pre_minutes, post_minutes=args.post_minutes,
                cache_dir=cache_dir,
            )
        return out

    klines_by_event = asyncio.run(_fetch_all())
    result = replay_events(events, klines_by_event, cfg)
    print(json.dumps(_summary(result), indent=2, sort_keys=True))
    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

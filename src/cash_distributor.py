"""Performance-weighted cash distributor.

User-requested feature: a bot that reads each running strategy's success
and reallocates fresh capital to the ones that are actually working.

This module is the read-only / advisory layer. It:

  1. Reads each bot's persistent state (positions, holdings).
  2. Reads each bot's ledger (realised P&L per trade).
  3. Computes per-strategy lifetime P&L and a normalised score.
  4. Reads the current free USDT in the wallet.
  5. Emits a ``DistributionPlan`` with recommended allocations.

The plan is the source of truth for two future layers:

  * a Telegram-friendly report so an operator can re-balance manually
  * a deposit/transfer executor that actually moves USDT (lands later
    once the algorithm has been validated against real ledger data)

Allocation policy (default):
    * Each bot has a configured ``min_share_pct`` so a stretch of bad
      luck doesn't zero-out its budget (we still want to give it a
      chance to recover).
    * The remaining "performance pool" is distributed proportional to
      each bot's score above zero. Bots with negative or zero P&L share
      only their minimum allocation, freeing the surplus for winners.
    * Per-bot ``max_share_pct`` cap prevents over-concentration in one
      strategy after a single lucky run.

The algorithm is deliberately simple and inspectable — the point is
that the operator can see exactly why each bot was given what.
"""

from __future__ import annotations

import argparse
import json
import os
from collections.abc import Mapping
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass(frozen=True, slots=True)
class BotSnapshot:
    """One strategy's contribution to the distribution input."""

    name: str
    lifetime_pnl_usdt: float
    trades_lifetime: int
    current_value_usdt: float  # cash + positions attributed to this bot
    min_share_pct: float       # floor allocation
    max_share_pct: float       # ceiling allocation


@dataclass(frozen=True, slots=True)
class Allocation:
    bot: str
    score: float
    raw_share_pct: float  # what the score-weighted policy wanted
    final_share_pct: float
    target_value_usdt: float
    current_value_usdt: float
    delta_usdt: float  # positive = top up, negative = drain


@dataclass(frozen=True, slots=True)
class DistributionPlan:
    timestamp: str
    total_treasury_usdt: float
    free_usdt: float
    allocations: tuple[Allocation, ...]
    notes: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return {
            "timestamp": self.timestamp,
            "total_treasury_usdt": self.total_treasury_usdt,
            "free_usdt": self.free_usdt,
            "allocations": [asdict(a) for a in self.allocations],
            "notes": list(self.notes),
        }


@dataclass(frozen=True, slots=True)
class DistributorConfig:
    """Policy knobs."""

    # Per-bot allocation bounds (must sum to <= 100% for max, >= 0 for min).
    # Defaults reflect the actual Binance USDT live fleet:
    #   - trend_filter is the only standalone live Binance bot today.
    #   - funding_arb earns its share only via realised P&L (floor 0) — it
    #     stays inert below its $50 min-balance gate, so giving it a floor
    #     share would just freeze idle USDT.
    #   - sniper_paper / mean_reversion are paper-only and intentionally
    #     not in this map.
    bot_min_share_pct: Mapping[str, float] = field(default_factory=lambda: {
        "trend_filter": 40.0,
        "funding_arb": 0.0,
    })
    bot_max_share_pct: Mapping[str, float] = field(default_factory=lambda: {
        "trend_filter": 100.0,
        "funding_arb": 60.0,
    })
    rebalance_threshold_usdt: float = 5.0  # ignore tiny delta suggestions


# ---------------------------------------------------------------------------
# Allocation algorithm
# ---------------------------------------------------------------------------


def _normalize_score(pnl: float) -> float:
    """Bots with positive P&L score it; loss-makers floor to zero.

    Simpler than Sharpe — uses raw cumulative P&L. A bot only competes for
    the performance pool when it has actually made money.
    """
    return max(pnl, 0.0)


def compute_plan(
    snapshots: tuple[BotSnapshot, ...],
    *,
    total_treasury_usdt: float,
    free_usdt: float,
) -> DistributionPlan:
    """Produce an advisory ``DistributionPlan`` for the given snapshots."""
    if total_treasury_usdt <= 0:
        return DistributionPlan(
            timestamp=datetime.now(tz=timezone.utc).isoformat(),
            total_treasury_usdt=total_treasury_usdt, free_usdt=free_usdt,
            allocations=(),
            notes=("treasury is empty; no allocations possible",),
        )

    n_bots = len(snapshots)
    if n_bots == 0:
        return DistributionPlan(
            timestamp=datetime.now(tz=timezone.utc).isoformat(),
            total_treasury_usdt=total_treasury_usdt, free_usdt=free_usdt,
            allocations=(),
            notes=("no bots configured",),
        )

    notes: list[str] = []

    # Untested-bot rule: a bot with zero lifetime trades gets zero share
    # (floor and ceiling forced to 0) so capital can't pool in a strategy
    # that has never run. Exception: if *every* bot is at zero trades,
    # fall back to equal floors so the bootstrap allocation isn't empty.
    active = [s for s in snapshots if s.trades_lifetime > 0]
    bootstrap = len(active) == 0

    def eff_min(s: BotSnapshot) -> float:
        return s.min_share_pct if (bootstrap or s.trades_lifetime > 0) else 0.0

    def eff_max(s: BotSnapshot) -> float:
        return s.max_share_pct if (bootstrap or s.trades_lifetime > 0) else 0.0

    excluded = [s.name for s in snapshots if not bootstrap and s.trades_lifetime == 0]
    if excluded:
        notes.append(
            "untested bots excluded (0 lifetime trades): " + ", ".join(excluded)
        )

    # Step 1: floor each bot at its effective min. Sum of mins is the
    # baseline; performance pool = 100% - sum(mins).
    sum_min = sum(eff_min(s) for s in snapshots)
    if sum_min > 100.0:
        scale = 100.0 / sum_min
        renormalised_mins = {s.name: eff_min(s) * scale for s in snapshots}
        performance_pool_pct = 0.0
        notes.append(
            f"configured min shares sum to {sum_min:.1f}% > 100; "
            f"min shares renormalised by {scale:.3f} and performance pool removed"
        )
    else:
        renormalised_mins = {s.name: eff_min(s) for s in snapshots}
        performance_pool_pct = 100.0 - sum_min

    # Step 2: score-weight the performance pool. Untested bots score 0.
    scores: dict[str, float] = {}
    for s in snapshots:
        if not bootstrap and s.trades_lifetime == 0:
            scores[s.name] = 0.0
        else:
            scores[s.name] = _normalize_score(s.lifetime_pnl_usdt)
    total_score = sum(scores.values())

    # Bonus eligibility: in non-bootstrap mode, only bots that have actually
    # traded compete for the performance pool; in bootstrap mode every bot
    # gets an equal kickstart share.
    eligible_for_pool = snapshots if bootstrap else tuple(active)
    n_eligible = len(eligible_for_pool) or n_bots

    raw_shares: dict[str, float] = {}
    for s in snapshots:
        base = renormalised_mins[s.name]
        if performance_pool_pct <= 0:
            bonus = 0.0
        elif not bootstrap and s.trades_lifetime == 0:
            bonus = 0.0
        elif total_score <= 0:
            bonus = performance_pool_pct / n_eligible  # equal split among eligible
        else:
            bonus = performance_pool_pct * (scores[s.name] / total_score)
        raw_shares[s.name] = base + bonus

    # Step 3: cap at effective max and redistribute the overflow. Untested
    # bots have eff_max=0, so they cannot absorb overflow either.
    final_shares = dict(raw_shares)
    for _ in range(10):  # bounded loops for safety
        overflow = 0.0
        capped: list[str] = []
        for s in snapshots:
            cap = eff_max(s)
            if final_shares[s.name] > cap:
                overflow += final_shares[s.name] - cap
                final_shares[s.name] = cap
                capped.append(s.name)
        if overflow < 1e-9:
            break
        eligible = [s for s in snapshots if s.name not in capped
                     and final_shares[s.name] < eff_max(s)]
        if not eligible:
            notes.append(
                f"overflow {overflow:.2f}% could not be redistributed; "
                f"all eligible bots already at max"
            )
            break
        total_room = sum(eff_max(s) - final_shares[s.name] for s in eligible)
        for s in eligible:
            room = eff_max(s) - final_shares[s.name]
            final_shares[s.name] += overflow * (room / total_room) if total_room else 0.0

    # Step 4: build allocation rows.
    allocations: list[Allocation] = []
    for s in snapshots:
        target = total_treasury_usdt * (final_shares[s.name] / 100.0)
        delta = target - s.current_value_usdt
        allocations.append(Allocation(
            bot=s.name, score=scores[s.name],
            raw_share_pct=raw_shares[s.name],
            final_share_pct=final_shares[s.name],
            target_value_usdt=target,
            current_value_usdt=s.current_value_usdt,
            delta_usdt=delta,
        ))

    return DistributionPlan(
        timestamp=datetime.now(tz=timezone.utc).isoformat(),
        total_treasury_usdt=total_treasury_usdt, free_usdt=free_usdt,
        allocations=tuple(allocations),
        notes=tuple(notes),
    )


# ---------------------------------------------------------------------------
# Read snapshots from on-disk bot state
# ---------------------------------------------------------------------------


def _read_jsonl(path: Path):
    if not path.exists():
        return
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def _sum_ledger_pnl(path: Path, *, fields: tuple[str, ...]) -> tuple[float, int]:
    total = 0.0
    count = 0
    for row in _read_jsonl(path):
        for f in fields:
            v = row.get(f)
            if v is not None:
                try:
                    total += float(v)
                    count += 1
                    break
                except (TypeError, ValueError):
                    continue
        else:
            count += 1  # non-monetary row (e.g. trend-filter fill)
    return total, count


def _read_json(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def build_snapshot(
    name: str, *,
    data_dir: Path,
    ledger_filename: str,
    state_filename: str | None,
    pnl_fields: tuple[str, ...],
    config: DistributorConfig,
    current_value_usdt: float = 0.0,
) -> BotSnapshot:
    ledger_path = data_dir / ledger_filename
    pnl, trades = _sum_ledger_pnl(ledger_path, fields=pnl_fields)
    return BotSnapshot(
        name=name,
        lifetime_pnl_usdt=pnl,
        trades_lifetime=trades,
        current_value_usdt=current_value_usdt,
        min_share_pct=config.bot_min_share_pct.get(name, 0.0),
        max_share_pct=config.bot_max_share_pct.get(name, 100.0),
    )


def build_default_snapshots(
    *, data_dir: Path, current_values: Mapping[str, float],
    config: DistributorConfig,
) -> tuple[BotSnapshot, ...]:
    return (
        build_snapshot("trend_filter", data_dir=data_dir,
                       ledger_filename="trend-filter-ledger.jsonl",
                       state_filename="trend-filter-state.json",
                       pnl_fields=("net_pnl_usdt",),  # trend-filter rows have no realised P&L
                       current_value_usdt=current_values.get("trend_filter", 0.0),
                       config=config),
        build_snapshot("funding_arb", data_dir=data_dir,
                       ledger_filename="funding-arb-ledger.jsonl",
                       state_filename="funding-arb-state.json",
                       pnl_fields=("net_pnl_usdt",),
                       current_value_usdt=current_values.get("funding_arb", 0.0),
                       config=config),
    )


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def render_plan(plan: DistributionPlan, *, threshold_usdt: float = 5.0) -> str:
    lines: list[str] = []
    lines.append("=" * 72)
    lines.append(f"  CASH DISTRIBUTION PLAN  ({plan.timestamp})")
    lines.append("=" * 72)
    lines.append(f"  Treasury value:  ${plan.total_treasury_usdt:.4f}")
    lines.append(f"  Free USDT:       ${plan.free_usdt:.4f}")
    lines.append("")
    if not plan.allocations:
        lines.append("  (no allocations)")
    else:
        header = f"  {'BOT':18s}  {'SHARE':>7s}  {'TARGET':>10s}  {'CURRENT':>10s}  {'DELTA':>10s}  ACTION"
        lines.append(header)
        lines.append("  " + "-" * (len(header) - 2))
        for a in plan.allocations:
            if a.delta_usdt > threshold_usdt:
                action = f"TOP UP +${a.delta_usdt:.2f}"
            elif a.delta_usdt < -threshold_usdt:
                action = f"DRAIN ${a.delta_usdt:+.2f}"
            else:
                action = "hold"
            lines.append(
                f"  {a.bot:18s}  {a.final_share_pct:6.2f}%  "
                f"${a.target_value_usdt:9.4f}  ${a.current_value_usdt:9.4f}  "
                f"${a.delta_usdt:+9.4f}  {action}"
            )
    if plan.notes:
        lines.append("")
        lines.append("Notes:")
        for note in plan.notes:
            lines.append(f"  - {note}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Compute an advisory cash-distribution plan across all running bots."
    )
    parser.add_argument("--data-dir",
                        default=os.path.expanduser("~/.local/share/binance-bot/data"))
    parser.add_argument("--treasury-usdt", type=float, default=None,
                        help="Total treasury value; if omitted, queries the Binance wallet.")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--threshold-usdt", type=float, default=5.0,
                        help="Suggested-action threshold in $.")
    args = parser.parse_args(argv)

    cfg = DistributorConfig()

    # Pull wallet snapshot lazily so unit tests don't have to mock ccxt.
    if args.treasury_usdt is None:
        import asyncio
        from src.bot_status import collect_snapshot

        wallet, _bots = asyncio.run(collect_snapshot(Path(args.data_dir)))
        treasury = wallet.total_value_usdt
        free = wallet.spot_usdt_free + wallet.futures_usdt_free
    else:
        treasury = args.treasury_usdt
        free = args.treasury_usdt

    snapshots = build_default_snapshots(
        data_dir=Path(args.data_dir),
        current_values={},  # we don't yet attribute per-bot inventory split
        config=cfg,
    )
    plan = compute_plan(snapshots, total_treasury_usdt=treasury, free_usdt=free)
    if args.json:
        print(json.dumps(plan.as_dict(), indent=2, sort_keys=True))
    else:
        print(render_plan(plan, threshold_usdt=args.threshold_usdt))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

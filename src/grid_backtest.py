"""Historical replay harness for symmetric grid trading on a single pair.

Grid trading places limit buy orders below the current price and limit sell
orders above, evenly spaced across a configured range. When the price
crosses a level the order fills, and a replacement order is queued at the
opposite side of the next level. Profit per round-trip is
``grid_spacing - round_trip_fees``.

The replay walks historical klines bar-by-bar. For each bar it identifies
the grid levels touched by the [low, high] range and fires any open orders
at those levels. After each fill the matching opposite-side order is
queued at the adjacent level. We deliberately model only direct fills
(no intra-bar oscillation chains) so the trade count is a conservative
lower bound on what a real venue would produce.

The metric that matters is total return *after fees*, since grid trading's
edge is mechanical (capture spread) but its risk is regime-dependent
(strong trends leave inventory or cash stranded outside the active range).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import statistics
from collections.abc import Mapping, Sequence
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


Kline = Sequence[Any]  # [open_time_ms, open, high, low, close, volume, ...]


@dataclass(frozen=True, slots=True)
class GridConfig:
    """Symmetric grid parameters.

    ``maker_fee_bps`` defaults to Binance VIP0 maker (10 bps); pass 7.5 if
    you'll pay with BNB. ``qty_per_level`` is fixed in base asset units so
    the per-trade quote notional varies across the grid.
    """

    symbol: str = "BTC/USDT"
    grid_low: float = 70_000.0
    grid_high: float = 90_000.0
    num_levels: int = 20
    qty_per_level: float = 0.001
    maker_fee_bps: float = 10.0
    slippage_bps: float = 1.0  # passive fills, minimal slippage
    starting_quote: float = 1_000.0
    interval: str = "1h"


@dataclass(frozen=True, slots=True)
class GridFill:
    timestamp_ms: int
    side: str  # "buy" | "sell"
    price: float
    qty: float
    fee_quote: float


@dataclass(frozen=True, slots=True)
class GridResult:
    config: GridConfig
    initial_cash: float
    initial_inventory: float
    final_cash: float
    final_inventory: float
    final_mark_to_market: float
    final_equity: float
    total_return_pct: float
    annualized_return_pct: float
    realized_pnl_quote: float
    fees_paid_quote: float
    num_buys: int
    num_sells: int
    num_round_trips: int
    bars_evaluated: int
    bars_with_fills: int
    bars_in_range_pct: float
    max_drawdown_pct: float
    sharpe_annualized: float
    fills_by_level: dict[float, int] = field(default_factory=dict)
    equity_curve: list[tuple[int, float]] = field(default_factory=list)
    fills: list[GridFill] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        payload = asdict(self)
        payload["config"] = asdict(self.config)
        payload["fills"] = [asdict(f) for f in self.fills]
        return payload

    def verdict(self) -> str:
        if self.bars_evaluated == 0:
            return "no_data"
        if self.num_round_trips == 0:
            return "no_trades"
        if self.sharpe_annualized > 1.0 and self.total_return_pct > 0.0:
            return "edge_likely"
        if self.total_return_pct > 0.0:
            return "edge_marginal"
        return "no_edge"


_BARS_PER_YEAR_BY_INTERVAL: dict[str, float] = {
    "1m": 525_600.0, "3m": 175_200.0, "5m": 105_120.0,
    "15m": 35_040.0, "30m": 17_520.0,
    "1h": 8_760.0, "2h": 4_380.0, "4h": 2_190.0, "6h": 1_460.0,
    "8h": 1_095.0, "12h": 730.0, "1d": 365.0,
}


def _open(k: Kline) -> float:
    return float(k[1])


def _high(k: Kline) -> float:
    return float(k[2])


def _low(k: Kline) -> float:
    return float(k[3])


def _close(k: Kline) -> float:
    return float(k[4])


def _open_ms(k: Kline) -> int:
    return int(k[0])


def _bars_per_year(interval: str) -> float:
    return _BARS_PER_YEAR_BY_INTERVAL.get(interval, 8_760.0)


def _round_trip_cost_bps(cfg: GridConfig) -> float:
    return 2.0 * cfg.maker_fee_bps + 2.0 * cfg.slippage_bps


def _build_levels(cfg: GridConfig) -> list[float]:
    if cfg.num_levels < 2:
        raise ValueError("num_levels must be at least 2")
    if cfg.grid_high <= cfg.grid_low:
        raise ValueError("grid_high must be greater than grid_low")
    step = (cfg.grid_high - cfg.grid_low) / (cfg.num_levels - 1)
    return [cfg.grid_low + i * step for i in range(cfg.num_levels)]


def replay_grid(
    klines: Sequence[Kline],
    config: GridConfig | None = None,
) -> GridResult:
    """Replay grid trading against historical klines.

    Initial state distributes the starting quote across the grid so that
    inventory exists to cover sells at every level above the opening price
    and cash exists to cover buys at every level below.
    """
    cfg = config or GridConfig()
    if cfg.starting_quote <= 0:
        raise ValueError("starting_quote must be positive")
    if cfg.qty_per_level <= 0:
        raise ValueError("qty_per_level must be positive")
    if not klines:
        return GridResult(
            config=cfg,
            initial_cash=cfg.starting_quote, initial_inventory=0.0,
            final_cash=cfg.starting_quote, final_inventory=0.0,
            final_mark_to_market=0.0, final_equity=cfg.starting_quote,
            total_return_pct=0.0, annualized_return_pct=0.0,
            realized_pnl_quote=0.0, fees_paid_quote=0.0,
            num_buys=0, num_sells=0, num_round_trips=0,
            bars_evaluated=0, bars_with_fills=0, bars_in_range_pct=0.0,
            max_drawdown_pct=0.0, sharpe_annualized=0.0,
        )

    levels = _build_levels(cfg)
    grid_spacing = levels[1] - levels[0]
    fee_rate = cfg.maker_fee_bps / 10_000.0
    slip_rate = cfg.slippage_bps / 10_000.0

    opening_price = _open(klines[0])

    # Initial allocation: enough inventory to satisfy all sells above the
    # opening price; cash to satisfy all buys below.
    levels_above = [lvl for lvl in levels if lvl > opening_price]
    levels_below = [lvl for lvl in levels if lvl <= opening_price]
    inventory_needed = cfg.qty_per_level * len(levels_above)
    cash_for_buys = sum(cfg.qty_per_level * lvl for lvl in levels_below)
    cost_to_acquire_inventory = inventory_needed * opening_price
    total_required = cost_to_acquire_inventory + cash_for_buys

    if total_required > cfg.starting_quote * 1.0001:
        # Not enough capital to populate the configured grid at this price.
        # Pro-rate qty_per_level down so the capital fits exactly.
        scale = cfg.starting_quote / total_required
        adjusted_qty = cfg.qty_per_level * scale
    else:
        adjusted_qty = cfg.qty_per_level

    inventory = adjusted_qty * len(levels_above)
    cash = cfg.starting_quote - inventory * opening_price
    initial_cash = cash
    initial_inventory = inventory

    # Open orders: buys below open price, sells above.
    open_buys: set[float] = {round(lvl, 8) for lvl in levels_below}
    open_sells: set[float] = {round(lvl, 8) for lvl in levels_above}
    if levels_below and levels_below[-1] >= opening_price:
        # Edge case when open exactly matches a level; treat as buy filled.
        pass

    fills: list[GridFill] = []
    fills_by_level: dict[float, int] = {}
    realized_pnl_quote = 0.0
    fees_paid_quote = 0.0
    equity_curve: list[tuple[int, float]] = []
    per_step_returns: list[float] = []
    bars_with_fills = 0
    bars_in_range = 0
    last_equity = initial_cash + initial_inventory * opening_price
    peak_equity = last_equity
    max_drawdown_pct = 0.0

    for kline in klines:
        ts = _open_ms(kline)
        bar_low = _low(kline)
        bar_high = _high(kline)
        bar_close = _close(kline)

        if bar_high >= levels[0] and bar_low <= levels[-1]:
            bars_in_range += 1

        bar_fills = 0
        # Walk levels in sorted order; let buys below current open fill, then
        # sells above. We don't model intra-bar chains beyond one pass.
        for level in levels:
            key = round(level, 8)
            if level < bar_low or level > bar_high:
                continue
            if key in open_buys:
                # Buy fills at this level. Pay fee in quote.
                qty = adjusted_qty
                cost = qty * level * (1.0 + fee_rate + slip_rate)
                if cost <= cash:
                    cash -= cost
                    inventory += qty
                    fee_quote = qty * level * (fee_rate + slip_rate)
                    fees_paid_quote += fee_quote
                    open_buys.remove(key)
                    # Queue a sell at the next level up.
                    sell_level = level + grid_spacing
                    if sell_level <= cfg.grid_high + 1e-9:
                        open_sells.add(round(sell_level, 8))
                    fills.append(GridFill(
                        timestamp_ms=ts, side="buy", price=level,
                        qty=qty, fee_quote=fee_quote,
                    ))
                    fills_by_level[key] = fills_by_level.get(key, 0) + 1
                    bar_fills += 1
            if key in open_sells and inventory >= adjusted_qty:
                qty = adjusted_qty
                proceeds = qty * level * (1.0 - fee_rate - slip_rate)
                cash += proceeds
                inventory -= qty
                fee_quote = qty * level * (fee_rate + slip_rate)
                fees_paid_quote += fee_quote
                # Realized P&L approximation: spacing * qty (we sold one
                # spacing above the implicit cost basis).
                realized_pnl_quote += grid_spacing * qty - 2.0 * fee_quote
                open_sells.remove(key)
                buy_level = level - grid_spacing
                if buy_level >= cfg.grid_low - 1e-9:
                    open_buys.add(round(buy_level, 8))
                fills.append(GridFill(
                    timestamp_ms=ts, side="sell", price=level,
                    qty=qty, fee_quote=fee_quote,
                ))
                fills_by_level[key] = fills_by_level.get(key, 0) + 1
                bar_fills += 1

        if bar_fills:
            bars_with_fills += 1

        equity = cash + inventory * bar_close
        if last_equity > 0:
            per_step_returns.append(equity / last_equity - 1.0)
        last_equity = equity
        equity_curve.append((ts, equity))
        if equity > peak_equity:
            peak_equity = equity
        elif peak_equity > 0:
            dd = (peak_equity - equity) / peak_equity * 100.0
            if dd > max_drawdown_pct:
                max_drawdown_pct = dd

    final_close = _close(klines[-1])
    mtm = inventory * final_close
    equity = cash + mtm
    total_return_pct = (equity / cfg.starting_quote - 1.0) * 100.0

    bars_evaluated = len(klines)
    bars_per_year = _bars_per_year(cfg.interval)
    years = bars_evaluated / bars_per_year if bars_per_year > 0 else 0.0
    if years > 0 and equity > 0:
        annualized_return_pct = ((equity / cfg.starting_quote) ** (1.0 / years) - 1.0) * 100.0
    else:
        annualized_return_pct = 0.0
    if len(per_step_returns) > 1:
        mean = statistics.fmean(per_step_returns)
        stdev = statistics.pstdev(per_step_returns)
        sharpe = (mean / stdev * math.sqrt(bars_per_year)) if stdev > 0 else 0.0
    else:
        sharpe = 0.0

    bars_in_range_pct = (bars_in_range / bars_evaluated * 100.0) if bars_evaluated else 0.0
    num_buys = sum(1 for f in fills if f.side == "buy")
    num_sells = sum(1 for f in fills if f.side == "sell")
    num_round_trips = min(num_buys, num_sells)

    return GridResult(
        config=cfg,
        initial_cash=initial_cash,
        initial_inventory=initial_inventory,
        final_cash=cash,
        final_inventory=inventory,
        final_mark_to_market=mtm,
        final_equity=equity,
        total_return_pct=total_return_pct,
        annualized_return_pct=annualized_return_pct,
        realized_pnl_quote=realized_pnl_quote,
        fees_paid_quote=fees_paid_quote,
        num_buys=num_buys,
        num_sells=num_sells,
        num_round_trips=num_round_trips,
        bars_evaluated=bars_evaluated,
        bars_with_fills=bars_with_fills,
        bars_in_range_pct=bars_in_range_pct,
        max_drawdown_pct=max_drawdown_pct,
        sharpe_annualized=sharpe,
        fills_by_level=fills_by_level,
        equity_curve=equity_curve,
        fills=fills,
    )


def _result_summary(result: GridResult) -> dict[str, Any]:
    return {
        "verdict": result.verdict(),
        "config": asdict(result.config),
        "initial_cash": round(result.initial_cash, 4),
        "initial_inventory": round(result.initial_inventory, 8),
        "final_cash": round(result.final_cash, 4),
        "final_inventory": round(result.final_inventory, 8),
        "final_mark_to_market": round(result.final_mark_to_market, 4),
        "final_equity": round(result.final_equity, 4),
        "total_return_pct": round(result.total_return_pct, 4),
        "annualized_return_pct": round(result.annualized_return_pct, 4),
        "realized_pnl_quote": round(result.realized_pnl_quote, 4),
        "fees_paid_quote": round(result.fees_paid_quote, 4),
        "num_buys": result.num_buys,
        "num_sells": result.num_sells,
        "num_round_trips": result.num_round_trips,
        "bars_evaluated": result.bars_evaluated,
        "bars_with_fills": result.bars_with_fills,
        "bars_in_range_pct": round(result.bars_in_range_pct, 2),
        "max_drawdown_pct": round(result.max_drawdown_pct, 4),
        "sharpe_annualized": round(result.sharpe_annualized, 4),
    }


async def _fetch_klines(
    symbol: str, *, interval: str, start: datetime, end: datetime,
    cache_dir: Path | None = None,
) -> list[list[Any]]:
    from src.data.history import HistoryFetcher

    if cache_dir is not None:
        key = symbol.replace("/", "_")
        cache_path = cache_dir / f"grid_{interval}_{key}_{int(start.timestamp())}_{int(end.timestamp())}.json"
        if cache_path.exists():
            try:
                return json.loads(cache_path.read_text())
            except (OSError, json.JSONDecodeError):
                pass

    fetcher = HistoryFetcher()
    klines = await fetcher.fetch_klines(symbol, interval, start_time=start, end_time=end)
    rows = [list(k) for k in klines]
    if cache_dir is not None:
        cache_path.parent.mkdir(parents=True, exist_ok=True)
        cache_path.write_text(json.dumps(rows))
    return rows


def _load_fixture(path: Path) -> list[list[Any]]:
    raw = json.loads(path.read_text())
    if isinstance(raw, list):
        return [list(k) for k in raw]
    if isinstance(raw, Mapping):
        # Allow {symbol: klines} fixture format; pick the first symbol.
        for v in raw.values():
            return [list(k) for k in v]
    raise ValueError("klines fixture must be a list or a {symbol: klines} mapping")


def _parse_iso(value: str) -> datetime:
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _build_parser() -> argparse.ArgumentParser:
    defaults = GridConfig()
    parser = argparse.ArgumentParser(
        description=(
            "Replay symmetric grid trading on a single pair against historical "
            "klines. Use the verdict to decide whether to build/run the live grid."
        )
    )
    parser.add_argument("--symbol", default=defaults.symbol)
    parser.add_argument("--interval", default=defaults.interval)
    parser.add_argument("--start", default="")
    parser.add_argument("--end", default="")
    parser.add_argument("--fixture", default="")
    parser.add_argument("--klines-cache", default="")
    parser.add_argument("--output", default="")
    parser.add_argument("--grid-low", type=float, default=defaults.grid_low)
    parser.add_argument("--grid-high", type=float, default=defaults.grid_high)
    parser.add_argument("--num-levels", type=int, default=defaults.num_levels)
    parser.add_argument("--qty-per-level", type=float, default=defaults.qty_per_level)
    parser.add_argument("--maker-fee-bps", type=float, default=defaults.maker_fee_bps)
    parser.add_argument("--slippage-bps", type=float, default=defaults.slippage_bps)
    parser.add_argument("--starting-quote", type=float, default=defaults.starting_quote)
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)

    cfg = GridConfig(
        symbol=args.symbol, interval=args.interval,
        grid_low=args.grid_low, grid_high=args.grid_high,
        num_levels=args.num_levels, qty_per_level=args.qty_per_level,
        maker_fee_bps=args.maker_fee_bps, slippage_bps=args.slippage_bps,
        starting_quote=args.starting_quote,
    )

    if args.fixture:
        klines = _load_fixture(Path(args.fixture))
    else:
        if not args.start:
            parser.error("either --fixture or --start must be supplied")
        start = _parse_iso(args.start)
        end = _parse_iso(args.end) if args.end else datetime.now(tz=timezone.utc)
        cache_dir = Path(args.klines_cache) if args.klines_cache else None
        klines = asyncio.run(_fetch_klines(args.symbol, interval=args.interval,
                                            start=start, end=end, cache_dir=cache_dir))

    result = replay_grid(klines, cfg)
    summary = _result_summary(result)
    print(json.dumps(summary, indent=2, sort_keys=True))
    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

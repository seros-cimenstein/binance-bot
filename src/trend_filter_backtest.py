"""Historical replay harness for long-horizon cross-sectional momentum.

Thesis: pick the top K assets by trailing X-month return, hold them
equal-weighted, rebalance every M periods. Fees become negligible because
trades are infrequent, and the bet is on persistence of multi-month
momentum (a well-documented factor in equities and increasingly in crypto).

The replay walks a multi-symbol kline universe. At each rebalance bar it
ranks eligible symbols by lookback return, takes the top K, computes the
delta vs the current portfolio, and submits the implied buys and sells at
the bar's close (with fee + slippage charged). Between rebalances the
portfolio is held; equity = cash + sum(qty * close).

A positive verdict here suggests fee-amortising long-horizon momentum is a
real alternative to the short-horizon LLM/rules pipeline that dominated
the spot work earlier in the session.
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


Kline = Sequence[Any]

_BARS_PER_YEAR_BY_INTERVAL: dict[str, float] = {
    "1h": 8_760.0, "4h": 2_190.0, "12h": 730.0, "1d": 365.0, "1w": 52.0,
}


@dataclass(frozen=True, slots=True)
class TrendFilterConfig:
    """Trend-filter parameters.

    Defaults: rank by 90-day return, hold top 5 equal-weighted, rebalance
    every 30 bars (≈ monthly on a 1d interval), 10 bps per leg.
    """

    interval: str = "1d"
    lookback_bars: int = 90
    top_k: int = 5
    rebalance_every_bars: int = 30
    min_lookback_return_pct: float = 0.0  # require trailing return > floor
    taker_fee_bps: float = 10.0
    slippage_bps: float = 5.0
    starting_balance: float = 10_000.0


@dataclass(frozen=True, slots=True)
class TrendTrade:
    timestamp_ms: int
    symbol: str
    side: str
    qty: float
    price: float
    fee_quote: float


@dataclass(frozen=True, slots=True)
class TrendFilterResult:
    config: TrendFilterConfig
    initial_balance: float
    final_equity: float
    total_return_pct: float
    annualized_return_pct: float
    sharpe_annualized: float
    max_drawdown_pct: float
    num_trades: int
    fees_paid_quote: float
    num_rebalances: int
    bars_evaluated: int
    bars_in_position_pct: float
    per_symbol_held_bars: dict[str, int] = field(default_factory=dict)
    equity_curve: list[tuple[int, float]] = field(default_factory=list)
    trades: list[TrendTrade] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        payload = asdict(self)
        payload["config"] = asdict(self.config)
        payload["trades"] = [asdict(t) for t in self.trades]
        return payload

    def verdict(self) -> str:
        if self.bars_evaluated == 0:
            return "no_data"
        if self.num_trades == 0:
            return "no_trades"
        if self.sharpe_annualized > 1.0 and self.total_return_pct > 0.0:
            return "edge_likely"
        if self.total_return_pct > 0.0:
            return "edge_marginal"
        return "no_edge"


def _close(k: Kline) -> float:
    return float(k[4])


def _open_ms(k: Kline) -> int:
    return int(k[0])


def _bars_per_year(interval: str) -> float:
    return _BARS_PER_YEAR_BY_INTERVAL.get(interval, 365.0)


def _align_by_open_time(
    klines_by_symbol: Mapping[str, Sequence[Kline]],
) -> tuple[list[int], dict[str, dict[int, Kline]]]:
    indexed: dict[str, dict[int, Kline]] = {}
    grid: set[int] = set()
    for symbol, klines in klines_by_symbol.items():
        per_symbol: dict[int, Kline] = {_open_ms(k): k for k in klines}
        indexed[symbol] = per_symbol
        grid.update(per_symbol.keys())
    return sorted(grid), indexed


def replay_trend_filter(
    klines_by_symbol: Mapping[str, Sequence[Kline]],
    config: TrendFilterConfig | None = None,
) -> TrendFilterResult:
    """Replay a top-K trend-filter portfolio against historical klines."""
    cfg = config or TrendFilterConfig()
    if cfg.starting_balance <= 0:
        raise ValueError("starting_balance must be positive")
    if cfg.top_k <= 0:
        raise ValueError("top_k must be positive")
    if cfg.lookback_bars <= 0:
        raise ValueError("lookback_bars must be positive")
    if cfg.rebalance_every_bars <= 0:
        raise ValueError("rebalance_every_bars must be positive")

    times, by_symbol = _align_by_open_time(klines_by_symbol)
    if len(times) <= cfg.lookback_bars + 1:
        return TrendFilterResult(
            config=cfg, initial_balance=cfg.starting_balance,
            final_equity=cfg.starting_balance, total_return_pct=0.0,
            annualized_return_pct=0.0, sharpe_annualized=0.0,
            max_drawdown_pct=0.0, num_trades=0, fees_paid_quote=0.0,
            num_rebalances=0, bars_evaluated=0, bars_in_position_pct=0.0,
        )

    fee_rate = cfg.taker_fee_bps / 10_000.0
    slip_rate = cfg.slippage_bps / 10_000.0
    one_side_cost_rate = fee_rate + slip_rate

    cash = cfg.starting_balance
    inventory: dict[str, float] = {}
    trades: list[TrendTrade] = []
    fees_paid_quote = 0.0
    num_rebalances = 0
    equity_curve: list[tuple[int, float]] = []
    per_step_returns: list[float] = []
    per_symbol_held_bars: dict[str, int] = {}
    last_equity = cash
    peak_equity = cash
    max_drawdown_pct = 0.0
    bars_in_position = 0

    def _close_or_none(symbol: str, ts: int) -> float | None:
        kline = by_symbol.get(symbol, {}).get(ts)
        return _close(kline) if kline is not None else None

    for i in range(cfg.lookback_bars, len(times)):
        ts = times[i]
        ts_lookback = times[i - cfg.lookback_bars]

        # Equity mark-to-market.
        mtm_inventory = 0.0
        for symbol, qty in inventory.items():
            px = _close_or_none(symbol, ts)
            if px is None:
                continue
            mtm_inventory += qty * px
        equity = cash + mtm_inventory

        # Rebalance check.
        do_rebalance = (i - cfg.lookback_bars) % cfg.rebalance_every_bars == 0
        if do_rebalance:
            scored: list[tuple[float, str]] = []
            for symbol, klines in by_symbol.items():
                here = klines.get(ts)
                past = klines.get(ts_lookback)
                if here is None or past is None:
                    continue
                past_close = _close(past)
                if past_close <= 0:
                    continue
                ret_pct = (_close(here) / past_close - 1.0) * 100.0
                if ret_pct < cfg.min_lookback_return_pct:
                    continue
                scored.append((ret_pct, symbol))
            scored.sort(reverse=True)
            target_symbols = [s for _, s in scored[:cfg.top_k]]
            target_weight = 1.0 / len(target_symbols) if target_symbols else 0.0
            target_qty: dict[str, float] = {}
            for symbol in target_symbols:
                px = _close_or_none(symbol, ts)
                if px is None or px <= 0:
                    continue
                target_qty[symbol] = (equity * target_weight) / px

            # Sells first to free cash.
            for symbol in list(inventory.keys()):
                current = inventory[symbol]
                desired = target_qty.get(symbol, 0.0)
                if current > desired:
                    delta = current - desired
                    px = _close_or_none(symbol, ts)
                    if px is None or px <= 0:
                        continue
                    proceeds = delta * px * (1.0 - one_side_cost_rate)
                    fee = delta * px * one_side_cost_rate
                    cash += proceeds
                    fees_paid_quote += fee
                    inventory[symbol] = current - delta
                    if inventory[symbol] < 1e-12:
                        del inventory[symbol]
                    trades.append(TrendTrade(
                        timestamp_ms=ts, symbol=symbol, side="sell",
                        qty=delta, price=px, fee_quote=fee,
                    ))

            # Buys next.
            for symbol, desired in target_qty.items():
                current = inventory.get(symbol, 0.0)
                if desired > current:
                    delta = desired - current
                    px = _close_or_none(symbol, ts)
                    if px is None or px <= 0:
                        continue
                    cost = delta * px * (1.0 + one_side_cost_rate)
                    fee = delta * px * one_side_cost_rate
                    if cost > cash + 1e-6:
                        # Cap delta to available cash.
                        affordable = cash / (px * (1.0 + one_side_cost_rate))
                        delta = max(affordable, 0.0)
                        cost = delta * px * (1.0 + one_side_cost_rate)
                        fee = delta * px * one_side_cost_rate
                    if delta <= 0:
                        continue
                    cash -= cost
                    fees_paid_quote += fee
                    inventory[symbol] = current + delta
                    trades.append(TrendTrade(
                        timestamp_ms=ts, symbol=symbol, side="buy",
                        qty=delta, price=px, fee_quote=fee,
                    ))

            num_rebalances += 1

        # Track per-step return + held-bars stats.
        mtm_inventory = 0.0
        for symbol, qty in inventory.items():
            px = _close_or_none(symbol, ts)
            if px is None:
                continue
            mtm_inventory += qty * px
            per_symbol_held_bars[symbol] = per_symbol_held_bars.get(symbol, 0) + 1
        if inventory:
            bars_in_position += 1
        equity = cash + mtm_inventory
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

    bars_evaluated = len(per_step_returns)
    total_return_pct = (last_equity / cfg.starting_balance - 1.0) * 100.0
    bars_per_year = _bars_per_year(cfg.interval)
    years = bars_evaluated / bars_per_year if bars_per_year > 0 else 0.0
    if years > 0 and last_equity > 0:
        annualized_return_pct = ((last_equity / cfg.starting_balance) ** (1.0 / years) - 1.0) * 100.0
    else:
        annualized_return_pct = 0.0
    if len(per_step_returns) > 1:
        mean = statistics.fmean(per_step_returns)
        stdev = statistics.pstdev(per_step_returns)
        sharpe = (mean / stdev * math.sqrt(bars_per_year)) if stdev > 0 else 0.0
    else:
        sharpe = 0.0
    bars_in_position_pct = (bars_in_position / bars_evaluated * 100.0) if bars_evaluated else 0.0

    return TrendFilterResult(
        config=cfg,
        initial_balance=cfg.starting_balance,
        final_equity=last_equity,
        total_return_pct=total_return_pct,
        annualized_return_pct=annualized_return_pct,
        sharpe_annualized=sharpe,
        max_drawdown_pct=max_drawdown_pct,
        num_trades=len(trades),
        fees_paid_quote=fees_paid_quote,
        num_rebalances=num_rebalances,
        bars_evaluated=bars_evaluated,
        bars_in_position_pct=bars_in_position_pct,
        per_symbol_held_bars=per_symbol_held_bars,
        equity_curve=equity_curve,
        trades=trades,
    )


def _result_summary(result: TrendFilterResult) -> dict[str, Any]:
    return {
        "verdict": result.verdict(),
        "config": asdict(result.config),
        "initial_balance": result.initial_balance,
        "final_equity": round(result.final_equity, 4),
        "total_return_pct": round(result.total_return_pct, 4),
        "annualized_return_pct": round(result.annualized_return_pct, 4),
        "sharpe_annualized": round(result.sharpe_annualized, 4),
        "max_drawdown_pct": round(result.max_drawdown_pct, 4),
        "num_trades": result.num_trades,
        "fees_paid_quote": round(result.fees_paid_quote, 4),
        "num_rebalances": result.num_rebalances,
        "bars_evaluated": result.bars_evaluated,
        "bars_in_position_pct": round(result.bars_in_position_pct, 2),
        "top_held_symbols": sorted(
            result.per_symbol_held_bars.items(), key=lambda x: x[1], reverse=True
        )[:10],
    }


async def _fetch_universe_klines(
    symbols: Sequence[str], *, interval: str, start: datetime, end: datetime,
    cache_dir: Path | None = None, concurrency: int = 6,
) -> dict[str, list[list[Any]]]:
    # Reuse the spot backtest's concurrent fetcher to avoid duplication.
    from src.claude_spot_backtest import _fetch_klines_for_symbols
    return await _fetch_klines_for_symbols(
        symbols, interval=interval, start=start, end=end,
        cache_dir=cache_dir, concurrency=concurrency,
    )


def _load_fixture(path: Path) -> dict[str, list[list[Any]]]:
    raw = json.loads(path.read_text())
    if not isinstance(raw, Mapping):
        raise ValueError("klines fixture must be a mapping of symbol -> klines")
    return {str(k): [list(item) for item in v] for k, v in raw.items()}


def _parse_iso(value: str) -> datetime:
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _build_parser() -> argparse.ArgumentParser:
    defaults = TrendFilterConfig()
    parser = argparse.ArgumentParser(
        description=(
            "Replay a long-horizon top-K trend-filter portfolio. Recommended "
            "interval: 1d. Lookback 90 / rebalance every 30 = monthly trend follower."
        )
    )
    parser.add_argument("--symbols",
                        default="BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT,XRP/USDT,ADA/USDT,DOGE/USDT,AVAX/USDT,LINK/USDT,DOT/USDT,LTC/USDT,MATIC/USDT")
    parser.add_argument("--interval", default=defaults.interval)
    parser.add_argument("--start", default="")
    parser.add_argument("--end", default="")
    parser.add_argument("--fixture", default="")
    parser.add_argument("--klines-cache", default="")
    parser.add_argument("--output", default="")
    parser.add_argument("--lookback-bars", type=int, default=defaults.lookback_bars)
    parser.add_argument("--top-k", type=int, default=defaults.top_k)
    parser.add_argument("--rebalance-every", type=int, default=defaults.rebalance_every_bars)
    parser.add_argument("--min-lookback-return-pct", type=float, default=defaults.min_lookback_return_pct)
    parser.add_argument("--taker-fee-bps", type=float, default=defaults.taker_fee_bps)
    parser.add_argument("--slippage-bps", type=float, default=defaults.slippage_bps)
    parser.add_argument("--starting-balance", type=float, default=defaults.starting_balance)
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    cfg = TrendFilterConfig(
        interval=args.interval, lookback_bars=args.lookback_bars,
        top_k=args.top_k, rebalance_every_bars=args.rebalance_every,
        min_lookback_return_pct=args.min_lookback_return_pct,
        taker_fee_bps=args.taker_fee_bps, slippage_bps=args.slippage_bps,
        starting_balance=args.starting_balance,
    )
    if args.fixture:
        klines = _load_fixture(Path(args.fixture))
    else:
        if not args.start:
            parser.error("either --fixture or --start must be supplied")
        start = _parse_iso(args.start)
        end = _parse_iso(args.end) if args.end else datetime.now(tz=timezone.utc)
        symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]
        cache_dir = Path(args.klines_cache) if args.klines_cache else None
        klines = asyncio.run(_fetch_universe_klines(
            symbols, interval=args.interval, start=start, end=end,
            cache_dir=cache_dir,
        ))
    result = replay_trend_filter(klines, cfg)
    print(json.dumps(_result_summary(result), indent=2, sort_keys=True))
    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

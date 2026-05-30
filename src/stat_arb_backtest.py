"""Historical replay harness for spread-reversion pairs trading.

Strategy: pick a pair of correlated assets A and B. Compute the rolling
log-ratio z-score ``z = (log(A/B) - mean) / stdev`` over a lookback window.
When z exceeds the entry threshold, take a market-neutral position:

* z > +entry: short A, long B (A is "too high" relative to B)
* z < -entry: long A, short B (A is "too low" relative to B)

Close when |z| drops below the exit threshold, when the position hits the
max-hold cap, or when the spread blows out past a stop-loss z.

This is the simplest viable stat-arb: no cointegration testing, no
Kalman-filtered spread; just a rolling z-score on log-price ratios. The
backtest treats both legs as USD-notional matched so the dollar exposure
is delta-neutral at entry. Funding/borrow costs on the short leg are
modeled as a flat per-bar drag in bps.
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
    "1m": 525_600.0, "5m": 105_120.0, "15m": 35_040.0, "1h": 8_760.0,
    "4h": 2_190.0, "1d": 365.0,
}


@dataclass(frozen=True, slots=True)
class StatArbConfig:
    interval: str = "1h"
    lookback_bars: int = 168  # ~1 week of hourly data
    entry_z: float = 2.0
    exit_z: float = 0.3
    stop_z: float = 4.0
    max_hold_bars: int = 168
    taker_fee_bps: float = 10.0
    slippage_bps: float = 3.0
    short_borrow_drag_bps_per_bar: float = 0.0  # spot-only mode = 0; perp-short mode = funding cost
    notional_per_position: float = 1_000.0  # USDT per leg
    starting_balance: float = 5_000.0


@dataclass(frozen=True, slots=True)
class StatArbTrade:
    symbol_long: str
    symbol_short: str
    open_time_ms: int
    close_time_ms: int
    entry_z: float
    exit_z: float
    bars_held: int
    pnl_quote: float
    fees_quote: float
    exit_reason: str


@dataclass(frozen=True, slots=True)
class StatArbResult:
    config: StatArbConfig
    pair: tuple[str, str]
    initial_balance: float
    final_equity: float
    total_return_pct: float
    annualized_return_pct: float
    sharpe_annualized: float
    max_drawdown_pct: float
    num_trades: int
    win_rate_pct: float
    avg_bars_held: float
    fees_paid_quote: float
    bars_evaluated: int
    bars_in_position_pct: float
    equity_curve: list[tuple[int, float]] = field(default_factory=list)
    trades: list[StatArbTrade] = field(default_factory=list)

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
    return _BARS_PER_YEAR_BY_INTERVAL.get(interval, 8_760.0)


def _align_two_series(
    klines_a: Sequence[Kline], klines_b: Sequence[Kline]
) -> tuple[list[int], dict[int, tuple[float, float]]]:
    indexed_a = {_open_ms(k): _close(k) for k in klines_a}
    indexed_b = {_open_ms(k): _close(k) for k in klines_b}
    common = sorted(indexed_a.keys() & indexed_b.keys())
    aligned = {t: (indexed_a[t], indexed_b[t]) for t in common}
    return common, aligned


def replay_pair(
    klines_by_symbol: Mapping[str, Sequence[Kline]],
    config: StatArbConfig | None = None,
) -> StatArbResult:
    """Replay one pair from a two-symbol kline dict."""
    cfg = config or StatArbConfig()
    if cfg.starting_balance <= 0:
        raise ValueError("starting_balance must be positive")
    if cfg.notional_per_position <= 0:
        raise ValueError("notional_per_position must be positive")
    if cfg.lookback_bars <= 1:
        raise ValueError("lookback_bars must be > 1")
    if cfg.entry_z <= cfg.exit_z:
        raise ValueError("entry_z must be strictly greater than exit_z")

    symbols = sorted(klines_by_symbol.keys())
    if len(symbols) != 2:
        raise ValueError("replay_pair requires exactly two symbols")
    sa, sb = symbols
    times, prices = _align_two_series(klines_by_symbol[sa], klines_by_symbol[sb])
    if len(times) <= cfg.lookback_bars + 1:
        return StatArbResult(
            config=cfg, pair=(sa, sb), initial_balance=cfg.starting_balance,
            final_equity=cfg.starting_balance, total_return_pct=0.0,
            annualized_return_pct=0.0, sharpe_annualized=0.0, max_drawdown_pct=0.0,
            num_trades=0, win_rate_pct=0.0, avg_bars_held=0.0, fees_paid_quote=0.0,
            bars_evaluated=0, bars_in_position_pct=0.0,
        )

    fee_rate = cfg.taker_fee_bps / 10_000.0
    slip_rate = cfg.slippage_bps / 10_000.0
    one_side_cost_rate = fee_rate + slip_rate
    round_trip_per_leg = 2.0 * one_side_cost_rate  # open + close on one leg

    equity = cfg.starting_balance
    trades: list[StatArbTrade] = []
    fees_paid_quote = 0.0
    open_position: dict[str, Any] | None = None
    equity_curve: list[tuple[int, float]] = []
    per_step_returns: list[float] = []
    peak_equity = equity
    max_drawdown_pct = 0.0
    bars_in_position = 0
    last_equity = equity

    # Precompute log-ratios over the full window.
    log_ratios = {t: math.log(prices[t][0] / prices[t][1]) for t in times}

    for i in range(cfg.lookback_bars, len(times)):
        ts = times[i]
        window = [log_ratios[times[j]] for j in range(i - cfg.lookback_bars, i)]
        mean = statistics.fmean(window)
        stdev = statistics.pstdev(window)
        if stdev <= 0:
            equity_curve.append((ts, equity))
            continue
        current_lr = log_ratios[ts]
        z = (current_lr - mean) / stdev

        if open_position is not None:
            pa_open, pb_open = open_position["entry_prices"]
            pa_now, pb_now = prices[ts]
            # PnL on long leg + PnL on short leg (delta-neutral notional).
            long_qty = open_position["long_qty"]
            short_qty = open_position["short_qty"]
            long_pnl = long_qty * (pa_now - pa_open) if open_position["long_symbol"] == sa else long_qty * (pb_now - pb_open)
            short_pnl = -short_qty * (pa_now - pa_open) if open_position["short_symbol"] == sa else -short_qty * (pb_now - pb_open)
            mtm_pnl = long_pnl + short_pnl
            mtm_equity = open_position["entry_equity"] + mtm_pnl - open_position["accrued_drag"]

            # Per-bar short borrow drag accrues to "accrued_drag".
            drag = cfg.notional_per_position * (cfg.short_borrow_drag_bps_per_bar / 10_000.0)
            open_position["accrued_drag"] = open_position["accrued_drag"] + drag
            open_position["bars_held"] = open_position["bars_held"] + 1

            close_reason = ""
            if abs(z) <= cfg.exit_z:
                close_reason = f"z={z:+.2f} reverted within exit band ({cfg.exit_z})"
            elif abs(z) >= cfg.stop_z:
                close_reason = f"z={z:+.2f} hit stop ({cfg.stop_z})"
            elif open_position["bars_held"] >= cfg.max_hold_bars:
                close_reason = f"max hold {cfg.max_hold_bars} bars reached"

            if close_reason:
                # Close: pay one side of fees on each leg.
                close_fees = 2.0 * cfg.notional_per_position * one_side_cost_rate
                final_pnl = mtm_pnl - open_position["accrued_drag"] - open_position["open_fees"] - close_fees
                equity = open_position["entry_equity"] + final_pnl
                fees_paid_quote += close_fees
                trades.append(StatArbTrade(
                    symbol_long=open_position["long_symbol"],
                    symbol_short=open_position["short_symbol"],
                    open_time_ms=open_position["open_time_ms"],
                    close_time_ms=ts,
                    entry_z=open_position["entry_z"],
                    exit_z=z,
                    bars_held=open_position["bars_held"],
                    pnl_quote=final_pnl,
                    fees_quote=open_position["open_fees"] + close_fees + open_position["accrued_drag"],
                    exit_reason=close_reason,
                ))
                open_position = None

        # Entry check (only if flat).
        if open_position is None and abs(z) >= cfg.entry_z:
            pa, pb = prices[ts]
            # z > 0 means A is too high relative to B → short A, long B.
            if z > 0:
                long_symbol, short_symbol = sb, sa
                long_qty = cfg.notional_per_position / pb
                short_qty = cfg.notional_per_position / pa
            else:
                long_symbol, short_symbol = sa, sb
                long_qty = cfg.notional_per_position / pa
                short_qty = cfg.notional_per_position / pb
            open_fees = 2.0 * cfg.notional_per_position * one_side_cost_rate
            fees_paid_quote += open_fees
            open_position = {
                "open_time_ms": ts,
                "entry_z": z,
                "long_symbol": long_symbol,
                "short_symbol": short_symbol,
                "long_qty": long_qty,
                "short_qty": short_qty,
                "entry_prices": (pa, pb),
                "open_fees": open_fees,
                "accrued_drag": 0.0,
                "bars_held": 0,
                "entry_equity": equity,
            }

        if open_position is not None:
            bars_in_position += 1

        per_step_return = (equity / last_equity - 1.0) if last_equity > 0 else 0.0
        per_step_returns.append(per_step_return)
        last_equity = equity
        equity_curve.append((ts, equity))
        if equity > peak_equity:
            peak_equity = equity
        elif peak_equity > 0:
            dd = (peak_equity - equity) / peak_equity * 100.0
            if dd > max_drawdown_pct:
                max_drawdown_pct = dd

    # Force-close any still-open trade at the final bar.
    if open_position is not None:
        ts = times[-1]
        pa_now, pb_now = prices[ts]
        long_qty = open_position["long_qty"]
        short_qty = open_position["short_qty"]
        long_pnl = long_qty * (pa_now - open_position["entry_prices"][0]) if open_position["long_symbol"] == sa else long_qty * (pb_now - open_position["entry_prices"][1])
        short_pnl = -short_qty * (pa_now - open_position["entry_prices"][0]) if open_position["short_symbol"] == sa else -short_qty * (pb_now - open_position["entry_prices"][1])
        mtm_pnl = long_pnl + short_pnl
        close_fees = 2.0 * cfg.notional_per_position * one_side_cost_rate
        final_pnl = mtm_pnl - open_position["accrued_drag"] - open_position["open_fees"] - close_fees
        equity = open_position["entry_equity"] + final_pnl
        fees_paid_quote += close_fees
        trades.append(StatArbTrade(
            symbol_long=open_position["long_symbol"],
            symbol_short=open_position["short_symbol"],
            open_time_ms=open_position["open_time_ms"],
            close_time_ms=ts,
            entry_z=open_position["entry_z"],
            exit_z=0.0,
            bars_held=open_position["bars_held"],
            pnl_quote=final_pnl,
            fees_quote=open_position["open_fees"] + close_fees + open_position["accrued_drag"],
            exit_reason="window end",
        ))

    bars_evaluated = len(per_step_returns)
    total_return_pct = (equity / cfg.starting_balance - 1.0) * 100.0
    bars_per_year = _bars_per_year(cfg.interval)
    years = bars_evaluated / bars_per_year if bars_per_year > 0 else 0.0
    if years > 0 and equity > 0:
        annualized_return_pct = ((equity / cfg.starting_balance) ** (1.0 / years) - 1.0) * 100.0
    else:
        annualized_return_pct = 0.0
    if len(per_step_returns) > 1:
        mean = statistics.fmean(per_step_returns)
        stdev = statistics.pstdev(per_step_returns)
        sharpe = (mean / stdev * math.sqrt(bars_per_year)) if stdev > 0 else 0.0
    else:
        sharpe = 0.0
    wins = sum(1 for t in trades if t.pnl_quote > 0)
    losses = sum(1 for t in trades if t.pnl_quote < 0)
    decisive = wins + losses
    win_rate_pct = (wins / decisive * 100.0) if decisive > 0 else 0.0
    avg_bars_held = (sum(t.bars_held for t in trades) / len(trades)) if trades else 0.0
    bars_in_position_pct = (bars_in_position / bars_evaluated * 100.0) if bars_evaluated else 0.0

    return StatArbResult(
        config=cfg, pair=(sa, sb),
        initial_balance=cfg.starting_balance, final_equity=equity,
        total_return_pct=total_return_pct,
        annualized_return_pct=annualized_return_pct,
        sharpe_annualized=sharpe, max_drawdown_pct=max_drawdown_pct,
        num_trades=len(trades), win_rate_pct=win_rate_pct,
        avg_bars_held=avg_bars_held, fees_paid_quote=fees_paid_quote,
        bars_evaluated=bars_evaluated, bars_in_position_pct=bars_in_position_pct,
        equity_curve=equity_curve, trades=trades,
    )


def _result_summary(result: StatArbResult) -> dict[str, Any]:
    return {
        "verdict": result.verdict(),
        "pair": list(result.pair),
        "config": asdict(result.config),
        "initial_balance": result.initial_balance,
        "final_equity": round(result.final_equity, 4),
        "total_return_pct": round(result.total_return_pct, 4),
        "annualized_return_pct": round(result.annualized_return_pct, 4),
        "sharpe_annualized": round(result.sharpe_annualized, 4),
        "max_drawdown_pct": round(result.max_drawdown_pct, 4),
        "num_trades": result.num_trades,
        "win_rate_pct": round(result.win_rate_pct, 2),
        "avg_bars_held": round(result.avg_bars_held, 2),
        "fees_paid_quote": round(result.fees_paid_quote, 4),
        "bars_evaluated": result.bars_evaluated,
        "bars_in_position_pct": round(result.bars_in_position_pct, 2),
    }


async def _fetch_pair_klines(
    symbol_a: str, symbol_b: str, *, interval: str,
    start: datetime, end: datetime, cache_dir: Path | None = None,
) -> dict[str, list[list[Any]]]:
    from src.claude_spot_backtest import _fetch_klines_for_symbols
    return await _fetch_klines_for_symbols(
        [symbol_a, symbol_b], interval=interval, start=start, end=end,
        cache_dir=cache_dir, concurrency=2,
    )


def _load_fixture(path: Path) -> dict[str, list[list[Any]]]:
    raw = json.loads(path.read_text())
    if not isinstance(raw, Mapping):
        raise ValueError("fixture must be a mapping of symbol -> klines")
    return {str(k): [list(item) for item in v] for k, v in raw.items()}


def _parse_iso(value: str) -> datetime:
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _build_parser() -> argparse.ArgumentParser:
    defaults = StatArbConfig()
    parser = argparse.ArgumentParser(
        description=(
            "Replay z-score mean-reversion pairs trading on a single asset pair. "
            "Pass two --symbol args, OR --fixture with two symbols."
        )
    )
    parser.add_argument("--symbol-a", default="ETH/USDT")
    parser.add_argument("--symbol-b", default="BTC/USDT")
    parser.add_argument("--interval", default=defaults.interval)
    parser.add_argument("--start", default="")
    parser.add_argument("--end", default="")
    parser.add_argument("--fixture", default="")
    parser.add_argument("--klines-cache", default="")
    parser.add_argument("--output", default="")
    parser.add_argument("--lookback-bars", type=int, default=defaults.lookback_bars)
    parser.add_argument("--entry-z", type=float, default=defaults.entry_z)
    parser.add_argument("--exit-z", type=float, default=defaults.exit_z)
    parser.add_argument("--stop-z", type=float, default=defaults.stop_z)
    parser.add_argument("--max-hold-bars", type=int, default=defaults.max_hold_bars)
    parser.add_argument("--taker-fee-bps", type=float, default=defaults.taker_fee_bps)
    parser.add_argument("--slippage-bps", type=float, default=defaults.slippage_bps)
    parser.add_argument("--short-borrow-drag-bps-per-bar", type=float, default=defaults.short_borrow_drag_bps_per_bar)
    parser.add_argument("--notional-per-position", type=float, default=defaults.notional_per_position)
    parser.add_argument("--starting-balance", type=float, default=defaults.starting_balance)
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    cfg = StatArbConfig(
        interval=args.interval, lookback_bars=args.lookback_bars,
        entry_z=args.entry_z, exit_z=args.exit_z, stop_z=args.stop_z,
        max_hold_bars=args.max_hold_bars,
        taker_fee_bps=args.taker_fee_bps, slippage_bps=args.slippage_bps,
        short_borrow_drag_bps_per_bar=args.short_borrow_drag_bps_per_bar,
        notional_per_position=args.notional_per_position,
        starting_balance=args.starting_balance,
    )
    if args.fixture:
        klines = _load_fixture(Path(args.fixture))
    else:
        if not args.start:
            parser.error("either --fixture or --start must be supplied")
        start = _parse_iso(args.start)
        end = _parse_iso(args.end) if args.end else datetime.now(tz=timezone.utc)
        cache_dir = Path(args.klines_cache) if args.klines_cache else None
        klines = asyncio.run(_fetch_pair_klines(
            args.symbol_a, args.symbol_b, interval=args.interval,
            start=start, end=end, cache_dir=cache_dir,
        ))
    result = replay_pair(klines, cfg)
    print(json.dumps(_result_summary(result), indent=2, sort_keys=True))
    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

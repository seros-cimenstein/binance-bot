"""Historical replay harness for Claude Spot's decision regime.

This module exists so we can answer one question before letting Claude Spot
submit live orders: does the strategy *empirically* clear its own cost bar?

The replay is deliberately deterministic. It scores the same kind of signal
the live pipeline computes (lookback return per candidate, half-spread proxy,
modeled taker fee) and routes capital to whichever non-base asset's expected
edge after friction is the largest and above ``min_edge_after_friction_bps``.
Otherwise it sits in the base asset. Sharpe, drawdown, turnover, fee drag,
and win rate fall out of the equity curve.

The CLI wraps this engine with ``src.data.history.HistoryFetcher`` so we can
backtest against real Binance klines without API keys.

A *positive* result here is the artifact that justifies setting
``CLAUDE_SPOT_PROVEN_EDGE=yes`` in the deployed environment.
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

BASE_CASH = "__BASE__"
_BARS_PER_YEAR_BY_INTERVAL: dict[str, float] = {
    "1m": 525_600.0,
    "3m": 175_200.0,
    "5m": 105_120.0,
    "15m": 35_040.0,
    "30m": 17_520.0,
    "1h": 8_760.0,
    "2h": 4_380.0,
    "4h": 2_190.0,
    "6h": 1_460.0,
    "8h": 1_095.0,
    "12h": 730.0,
    "1d": 365.0,
}


@dataclass(frozen=True, slots=True)
class BacktestConfig:
    """Cost-aware replay knobs.

    Defaults model Binance VIP0 spot taker fees (10 bps each side) plus a
    5 bps slippage allowance and a half-spread proxy derived from each bar's
    intraperiod range. ``min_edge_after_friction_bps`` is the threshold a
    candidate's expected edge must clear to displace the current holding.
    """

    base_asset: str = "USDT"
    interval: str = "1h"
    taker_fee_bps: float = 10.0
    slippage_bps: float = 5.0
    min_spread_bps: float = 2.0
    max_spread_bps: float = 100.0
    lookback_bars: int = 24
    min_edge_after_friction_bps: float = 50.0
    rebalance_cooldown_bars: int = 1
    starting_balance: float = 10_000.0


@dataclass(frozen=True, slots=True)
class TradeRecord:
    timestamp_ms: int
    from_asset: str
    to_asset: str
    cost_bps: float
    equity_before: float
    equity_after: float


@dataclass(frozen=True, slots=True)
class BacktestResult:
    config: BacktestConfig
    initial_balance: float
    final_balance: float
    total_return_pct: float
    annualized_return_pct: float
    sharpe_annualized: float
    max_drawdown_pct: float
    num_trades: int
    win_rate_pct: float
    fee_drag_bps: float
    bars_in_position_pct: float
    bars_evaluated: int
    equity_curve: list[tuple[int, float]] = field(default_factory=list)
    per_asset_bars_held: dict[str, int] = field(default_factory=dict)
    trades: list[TradeRecord] = field(default_factory=list)

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


def _close(kline: Kline) -> float:
    return float(kline[4])


def _high(kline: Kline) -> float:
    return float(kline[2])


def _low(kline: Kline) -> float:
    return float(kline[3])


def _open_ms(kline: Kline) -> int:
    return int(kline[0])


def _half_spread_bps(kline: Kline, *, cfg: BacktestConfig) -> float:
    close = _close(kline)
    if close <= 0:
        return cfg.max_spread_bps
    raw = (_high(kline) - _low(kline)) / close * 10_000.0 / 2.0
    return max(cfg.min_spread_bps, min(cfg.max_spread_bps, raw))


def _round_trip_cost_bps(spread_bps: float, *, cfg: BacktestConfig) -> float:
    return 2.0 * cfg.taker_fee_bps + spread_bps + cfg.slippage_bps


def _bars_per_year(interval: str) -> float:
    return _BARS_PER_YEAR_BY_INTERVAL.get(interval, 8_760.0)


def _align_by_open_time(
    klines_by_symbol: Mapping[str, Sequence[Kline]],
) -> tuple[list[int], dict[str, dict[int, Kline]]]:
    by_ts: dict[str, dict[int, Kline]] = {}
    common_times: set[int] | None = None
    for symbol, klines in klines_by_symbol.items():
        indexed: dict[int, Kline] = {_open_ms(k): k for k in klines}
        by_ts[symbol] = indexed
        if common_times is None:
            common_times = set(indexed.keys())
        else:
            common_times &= indexed.keys()
    if not common_times:
        return [], by_ts
    return sorted(common_times), by_ts


def replay(
    klines_by_symbol: Mapping[str, Sequence[Kline]],
    config: BacktestConfig | None = None,
) -> BacktestResult:
    """Replay deterministic decisions against historical klines.

    Returns a ``BacktestResult`` whose ``verdict()`` summarises whether the
    rule cleared the modeled cost bar on this window. A passing run is what
    promotes Claude Spot from paper to live (via ``CLAUDE_SPOT_PROVEN_EDGE``).
    """
    cfg = config or BacktestConfig()
    if cfg.lookback_bars <= 0:
        raise ValueError("lookback_bars must be positive")
    if cfg.starting_balance <= 0:
        raise ValueError("starting_balance must be positive")

    times, by_ts = _align_by_open_time(klines_by_symbol)
    if len(times) <= cfg.lookback_bars + 1:
        return BacktestResult(
            config=cfg,
            initial_balance=cfg.starting_balance,
            final_balance=cfg.starting_balance,
            total_return_pct=0.0,
            annualized_return_pct=0.0,
            sharpe_annualized=0.0,
            max_drawdown_pct=0.0,
            num_trades=0,
            win_rate_pct=0.0,
            fee_drag_bps=0.0,
            bars_in_position_pct=0.0,
            bars_evaluated=0,
        )

    equity = cfg.starting_balance
    held = BASE_CASH
    last_switch_idx = -10**9
    fee_paid_bps_total = 0.0
    equity_curve: list[tuple[int, float]] = [(times[cfg.lookback_bars], equity)]
    per_asset_bars: dict[str, int] = {}
    trades: list[TradeRecord] = []
    per_step_returns: list[float] = []
    wins = 0
    losses = 0
    peak_equity = equity
    max_drawdown_pct = 0.0
    symbols = sorted(klines_by_symbol.keys())

    for i in range(cfg.lookback_bars, len(times) - 1):
        ts = times[i]
        next_ts = times[i + 1]

        # Score every candidate from the current bar's perspective.
        best_symbol: str | None = None
        best_edge_bps = -math.inf
        signals: dict[str, float] = {}
        for symbol in symbols:
            klines = by_ts[symbol]
            here = klines[ts]
            past = klines[times[i - cfg.lookback_bars]]
            past_close = _close(past)
            if past_close <= 0:
                continue
            lookback_return_bps = (_close(here) / past_close - 1.0) * 10_000.0
            spread_bps = _half_spread_bps(here, cfg=cfg)
            cost_bps = _round_trip_cost_bps(spread_bps, cfg=cfg)
            expected_edge_bps = lookback_return_bps - cost_bps
            signals[symbol] = expected_edge_bps
            if expected_edge_bps > best_edge_bps:
                best_edge_bps = expected_edge_bps
                best_symbol = symbol

        # Decide whether to switch.
        eligible = (
            best_symbol is not None
            and best_edge_bps >= cfg.min_edge_after_friction_bps
            and best_symbol != held
            and i - last_switch_idx >= cfg.rebalance_cooldown_bars
        )
        if eligible and best_symbol is not None:
            spread_bps = _half_spread_bps(by_ts[best_symbol][ts], cfg=cfg)
            switch_cost_bps = (
                2.0 * cfg.taker_fee_bps + spread_bps + cfg.slippage_bps
                if held == BASE_CASH or best_symbol == BASE_CASH
                else 2.0 * (2.0 * cfg.taker_fee_bps + spread_bps + cfg.slippage_bps)
            )
            equity_before = equity
            equity *= 1.0 - switch_cost_bps / 10_000.0
            fee_paid_bps_total += switch_cost_bps
            trades.append(
                TradeRecord(
                    timestamp_ms=ts,
                    from_asset=held,
                    to_asset=best_symbol,
                    cost_bps=switch_cost_bps,
                    equity_before=equity_before,
                    equity_after=equity,
                )
            )
            held = best_symbol
            last_switch_idx = i

        # Apply next-bar return for the held asset.
        if held == BASE_CASH:
            step_return = 0.0
        else:
            klines = by_ts[held]
            here_close = _close(klines[ts])
            next_close = _close(klines[next_ts])
            if here_close <= 0:
                step_return = 0.0
            else:
                step_return = next_close / here_close - 1.0
        equity *= 1.0 + step_return
        per_step_returns.append(step_return)
        if step_return > 0:
            wins += 1
        elif step_return < 0:
            losses += 1
        per_asset_bars[held] = per_asset_bars.get(held, 0) + 1
        equity_curve.append((next_ts, equity))
        if equity > peak_equity:
            peak_equity = equity
        else:
            drawdown_pct = (peak_equity - equity) / peak_equity * 100.0
            if drawdown_pct > max_drawdown_pct:
                max_drawdown_pct = drawdown_pct

    bars_evaluated = len(per_step_returns)
    if bars_evaluated == 0:
        sharpe = 0.0
        total_return_pct = 0.0
        annualized_return_pct = 0.0
        win_rate_pct = 0.0
        bars_in_position_pct = 0.0
    else:
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
        decisive = wins + losses
        win_rate_pct = (wins / decisive * 100.0) if decisive > 0 else 0.0
        bars_in_position_pct = (
            (bars_evaluated - per_asset_bars.get(BASE_CASH, 0)) / bars_evaluated * 100.0
        )

    return BacktestResult(
        config=cfg,
        initial_balance=cfg.starting_balance,
        final_balance=equity,
        total_return_pct=total_return_pct,
        annualized_return_pct=annualized_return_pct,
        sharpe_annualized=sharpe,
        max_drawdown_pct=max_drawdown_pct,
        num_trades=len(trades),
        win_rate_pct=win_rate_pct,
        fee_drag_bps=fee_paid_bps_total,
        bars_in_position_pct=bars_in_position_pct,
        bars_evaluated=bars_evaluated,
        equity_curve=equity_curve,
        per_asset_bars_held=per_asset_bars,
        trades=trades,
    )


def _result_summary(result: BacktestResult) -> dict[str, Any]:
    """Compact, human-readable JSON for CLI output."""
    return {
        "verdict": result.verdict(),
        "config": asdict(result.config),
        "initial_balance": result.initial_balance,
        "final_balance": round(result.final_balance, 4),
        "total_return_pct": round(result.total_return_pct, 4),
        "annualized_return_pct": round(result.annualized_return_pct, 4),
        "sharpe_annualized": round(result.sharpe_annualized, 4),
        "max_drawdown_pct": round(result.max_drawdown_pct, 4),
        "num_trades": result.num_trades,
        "win_rate_pct": round(result.win_rate_pct, 2),
        "fee_drag_bps": round(result.fee_drag_bps, 2),
        "bars_in_position_pct": round(result.bars_in_position_pct, 2),
        "bars_evaluated": result.bars_evaluated,
        "per_asset_bars_held": result.per_asset_bars_held,
    }


async def _fetch_klines_for_symbols(
    symbols: Sequence[str],
    *,
    interval: str,
    start: datetime,
    end: datetime,
) -> dict[str, list[list[Any]]]:
    from src.data.history import HistoryFetcher

    fetcher = HistoryFetcher()
    result: dict[str, list[list[Any]]] = {}
    for symbol in symbols:
        klines = await fetcher.fetch_klines(
            symbol,
            interval,
            start_time=start,
            end_time=end,
        )
        result[symbol] = [list(k) for k in klines]
    return result


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
    defaults = BacktestConfig()
    parser = argparse.ArgumentParser(
        description=(
            "Replay Claude Spot's cost-aware decision rule against historical "
            "klines. Use the JSON output (verdict + Sharpe + return) to decide "
            "whether to set CLAUDE_SPOT_PROVEN_EDGE=yes."
        )
    )
    parser.add_argument(
        "--symbols",
        required=False,
        default="BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT",
        help="comma-separated CCXT-style symbols (default: top liquid USDT pairs)",
    )
    parser.add_argument("--interval", default=defaults.interval)
    parser.add_argument(
        "--start",
        default="",
        help="ISO start timestamp; required unless --fixture is supplied",
    )
    parser.add_argument(
        "--end",
        default="",
        help="ISO end timestamp; defaults to now if --start is set",
    )
    parser.add_argument(
        "--fixture",
        default="",
        help="optional path to a JSON {symbol: [kline,...]} file for offline runs",
    )
    parser.add_argument("--output", default="", help="optional path to dump full report JSON")
    parser.add_argument("--lookback-bars", type=int, default=defaults.lookback_bars)
    parser.add_argument(
        "--min-edge-bps",
        type=float,
        default=defaults.min_edge_after_friction_bps,
    )
    parser.add_argument("--taker-fee-bps", type=float, default=defaults.taker_fee_bps)
    parser.add_argument("--slippage-bps", type=float, default=defaults.slippage_bps)
    parser.add_argument(
        "--starting-balance",
        type=float,
        default=defaults.starting_balance,
    )
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)

    cfg = BacktestConfig(
        interval=args.interval,
        taker_fee_bps=args.taker_fee_bps,
        slippage_bps=args.slippage_bps,
        lookback_bars=args.lookback_bars,
        min_edge_after_friction_bps=args.min_edge_bps,
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
        klines = asyncio.run(
            _fetch_klines_for_symbols(symbols, interval=args.interval, start=start, end=end)
        )

    result = replay(klines, cfg)
    summary = _result_summary(result)
    print(json.dumps(summary, indent=2, sort_keys=True))

    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))

    return 0


if __name__ == "__main__":
    raise SystemExit(run())

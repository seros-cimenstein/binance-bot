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
    """Build a union-of-times grid plus per-symbol open_ms -> kline index.

    Using the union (not intersection) lets newer listings join the grid
    when they actually have data, without truncating the entire analysis to
    the shortest series. The replay engine checks per-symbol availability
    at each timestep before scoring or applying returns.
    """
    by_ts: dict[str, dict[int, Kline]] = {}
    all_times: set[int] = set()
    for symbol, klines in klines_by_symbol.items():
        indexed: dict[int, Kline] = {_open_ms(k): k for k in klines}
        by_ts[symbol] = indexed
        all_times.update(indexed.keys())
    return sorted(all_times), by_ts


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
        ts_lookback = times[i - cfg.lookback_bars]

        # Score every candidate from the current bar's perspective. Skip
        # symbols that are missing klines at the lookback or current timestamp.
        best_symbol: str | None = None
        best_edge_bps = -math.inf
        for symbol in symbols:
            klines = by_ts[symbol]
            here = klines.get(ts)
            past = klines.get(ts_lookback)
            if here is None or past is None:
                continue
            past_close = _close(past)
            if past_close <= 0:
                continue
            lookback_return_bps = (_close(here) / past_close - 1.0) * 10_000.0
            spread_bps = _half_spread_bps(here, cfg=cfg)
            cost_bps = _round_trip_cost_bps(spread_bps, cfg=cfg)
            expected_edge_bps = lookback_return_bps - cost_bps
            if expected_edge_bps > best_edge_bps:
                best_edge_bps = expected_edge_bps
                best_symbol = symbol

        # Force-exit if the held asset has no data this bar.
        held_here = None if held == BASE_CASH else by_ts.get(held, {}).get(ts)
        if held != BASE_CASH and held_here is None:
            held = BASE_CASH

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

        # Apply next-bar return for the held asset. If the held asset has no
        # data at either ts or next_ts, treat the bar as flat cash and force
        # the next iteration to consider rotating out.
        if held == BASE_CASH:
            step_return = 0.0
        else:
            klines = by_ts[held]
            here_kline = klines.get(ts)
            next_kline = klines.get(next_ts)
            if here_kline is None or next_kline is None:
                step_return = 0.0
                if next_kline is None:
                    held = BASE_CASH
            else:
                here_close = _close(here_kline)
                if here_close <= 0:
                    step_return = 0.0
                else:
                    step_return = _close(next_kline) / here_close - 1.0
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
    cache_dir: Path | None = None,
    concurrency: int = 6,
) -> dict[str, list[list[Any]]]:
    """Fetch klines for many symbols concurrently with optional disk cache."""
    from src.data.history import HistoryFetcher

    fetcher = HistoryFetcher()
    sem = asyncio.Semaphore(max(concurrency, 1))
    result: dict[str, list[list[Any]]] = {}

    def _cache_path(symbol: str) -> Path | None:
        if cache_dir is None:
            return None
        key = symbol.replace("/", "_").replace(":", "_")
        return cache_dir / f"{interval}_{key}_{int(start.timestamp())}_{int(end.timestamp())}.json"

    async def _one(symbol: str) -> None:
        path = _cache_path(symbol)
        if path is not None and path.exists():
            try:
                result[symbol] = json.loads(path.read_text())
                return
            except (OSError, json.JSONDecodeError):
                pass
        async with sem:
            klines = await fetcher.fetch_klines(
                symbol,
                interval,
                start_time=start,
                end_time=end,
            )
        rows = [list(k) for k in klines]
        result[symbol] = rows
        if path is not None:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(rows))

    await asyncio.gather(*(_one(s) for s in symbols))
    return result


async def _fetch_spot_usdt_universe(
    *,
    max_symbols: int,
    min_quote_volume_usdt: float,
) -> list[str]:
    """Return the top-N USDT spot symbols by 24h volume, excluding fiat bases."""
    import aiohttp

    from src.claude_spot import CLAUDE_SPOT_FIAT_ASSETS

    async with aiohttp.ClientSession() as session:
        async with session.get("https://api.binance.com/api/v3/exchangeInfo") as resp:
            resp.raise_for_status()
            info = await resp.json()
        async with session.get("https://api.binance.com/api/v3/ticker/24hr") as resp:
            resp.raise_for_status()
            tickers = await resp.json()

    volume_by_symbol: dict[str, float] = {}
    for row in tickers:
        try:
            volume_by_symbol[row["symbol"]] = float(row.get("quoteVolume", 0.0))
        except (KeyError, ValueError, TypeError):
            continue

    candidates: list[tuple[float, str]] = []
    for sym in info.get("symbols", []):
        if sym.get("status") != "TRADING":
            continue
        if not sym.get("isSpotTradingAllowed"):
            continue
        if sym.get("quoteAsset") != "USDT":
            continue
        base = sym.get("baseAsset", "")
        if base in CLAUDE_SPOT_FIAT_ASSETS:
            continue
        if base.endswith("UP") or base.endswith("DOWN") or base.endswith("BULL") or base.endswith("BEAR"):
            continue  # leveraged tokens are not in scope
        binance_symbol = sym.get("symbol", "")
        volume = volume_by_symbol.get(binance_symbol, 0.0)
        if volume < min_quote_volume_usdt:
            continue
        candidates.append((volume, f"{base}/USDT"))

    candidates.sort(reverse=True)
    return [s for _, s in candidates[:max_symbols]]


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
    parser.add_argument(
        "--universe",
        default="",
        choices=("", "spot_usdt_top"),
        help=(
            "if set to spot_usdt_top, ignore --symbols and pull all Binance spot "
            "USDT pairs (ex fiats / leveraged tokens) ranked by 24h quote volume"
        ),
    )
    parser.add_argument(
        "--universe-size",
        type=int,
        default=200,
        help="max symbols when --universe is set",
    )
    parser.add_argument(
        "--min-quote-volume-usdt",
        type=float,
        default=1_000_000.0,
        help="minimum 24h quote volume in USDT for --universe filtering",
    )
    parser.add_argument(
        "--klines-cache",
        default="",
        help="directory to cache fetched klines so re-runs are fast",
    )
    parser.add_argument(
        "--fetch-concurrency",
        type=int,
        default=6,
        help="parallel kline fetches (respect Binance rate limits)",
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
        if args.universe == "spot_usdt_top":
            symbols = asyncio.run(
                _fetch_spot_usdt_universe(
                    max_symbols=args.universe_size,
                    min_quote_volume_usdt=args.min_quote_volume_usdt,
                )
            )
            print(
                json.dumps(
                    {
                        "universe": "spot_usdt_top",
                        "symbol_count": len(symbols),
                        "first_10": symbols[:10],
                    },
                    indent=2,
                )
            )
        else:
            symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]
        cache_dir = Path(args.klines_cache) if args.klines_cache else None
        klines = asyncio.run(
            _fetch_klines_for_symbols(
                symbols,
                interval=args.interval,
                start=start,
                end=end,
                cache_dir=cache_dir,
                concurrency=args.fetch_concurrency,
            )
        )

    result = replay(klines, cfg)
    summary = _result_summary(result)
    print(json.dumps(summary, indent=2, sort_keys=True))

    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))

    return 0


if __name__ == "__main__":
    raise SystemExit(run())

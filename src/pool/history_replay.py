"""Isolated public-candle replay for paper Spot strategy research."""

from __future__ import annotations

import argparse
import asyncio
import json
import math
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Protocol, Sequence

from loguru import logger

from src.data.history import BinanceHistoryError, HistoryFetcher
from src.pool.replay import ReplaySummary, run_replay_fixture


class HistoricalReplayError(RuntimeError):
    """Raised when public-candle research input cannot be evaluated."""


class CandleFetcher(Protocol):
    """Subset of the historical data reader required by this analysis."""

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str,
        start_time: datetime,
        end_time: datetime | None = None,
    ) -> list[list[float | int]]:
        """Return public historical candles."""


@dataclass(frozen=True, slots=True)
class HistoricalReplaySummary:
    """Outcome and boundaries of a public-candle strategy evaluation."""

    symbols: tuple[str, ...]
    timeframe: str
    candle_cycles: int
    started_at: str
    ended_at: str
    db_path: str
    replay: ReplaySummary
    scope: str = (
        "research only: public direct-USDT candles with modeled fees/spread/minimums; "
        "does not satisfy live readiness"
    )

    def as_dict(self) -> dict[str, object]:
        payload = asdict(self)
        payload["replay"] = self.replay.as_dict()
        return payload


async def run_historical_replay(
    *,
    symbols: Sequence[str],
    initial_balances: dict[str, float],
    start_time: datetime,
    end_time: datetime,
    db_path: str | Path,
    timeframe: str = "15m",
    required_observations: int = 96,
    cycles: int | None = None,
    taker_fee: float = 0.001,
    spread_bps: float = 10.0,
    slippage_buffer_bps: float = 25.0,
    min_notional: float = 5.0,
    fetcher: CandleFetcher | None = None,
) -> HistoricalReplaySummary:
    """Evaluate selected direct-USDT assets using historical public candles only."""
    if end_time <= start_time:
        raise HistoricalReplayError("end time must be after start time")
    if required_observations <= 1:
        raise HistoricalReplayError("required observations must be greater than one")
    balances = _balances(initial_balances)
    selected = _required_symbols(symbols, balances)
    candle_fetcher = fetcher or HistoryFetcher()

    series: dict[str, dict[int, tuple[float, float]]] = {}
    for symbol in selected:
        rows = await candle_fetcher.fetch_ohlcv(symbol, timeframe, start_time, end_time)
        parsed = _candle_series(rows)
        if not parsed:
            raise HistoricalReplayError(f"no historical candles returned for {symbol}")
        series[symbol] = parsed
    common_times = sorted(set.intersection(*(set(values) for values in series.values())))
    if len(common_times) < required_observations:
        raise HistoricalReplayError(
            "historical series do not contain enough aligned candles: "
            f"{len(common_times)}/{required_observations}"
        )
    if cycles is not None:
        if cycles <= 0:
            raise HistoricalReplayError("cycles must be positive")
        common_times = common_times[-cycles:]

    fixture = _fixture(
        selected,
        balances,
        series,
        common_times,
        timeframe=timeframe,
        required_observations=required_observations,
        taker_fee=taker_fee,
        spread_bps=spread_bps,
        slippage_buffer_bps=slippage_buffer_bps,
        min_notional=min_notional,
    )
    replay = await run_replay_fixture(
        fixture,
        cycles=len(common_times),
        db_path=db_path,
        reset_db=True,
    )
    return HistoricalReplaySummary(
        symbols=selected,
        timeframe=timeframe,
        candle_cycles=len(common_times),
        started_at=datetime.fromtimestamp(common_times[0] / 1000, tz=timezone.utc).isoformat(),
        ended_at=datetime.fromtimestamp(common_times[-1] / 1000, tz=timezone.utc).isoformat(),
        db_path=str(db_path),
        replay=replay,
    )


def _required_symbols(
    requested_symbols: Sequence[str],
    balances: dict[str, float],
) -> tuple[str, ...]:
    symbols = {
        _direct_usdt_symbol(symbol)
        for symbol in requested_symbols
        if symbol.strip()
    }
    for asset in (*balances, "BTC", "PAXG", "XAUT"):
        if asset != "USDT":
            symbols.add(f"{asset}/USDT")
    return tuple(sorted(symbols))


def _direct_usdt_symbol(symbol: str) -> str:
    value = symbol.strip().upper().replace("-", "/")
    if "/" not in value:
        if value.endswith("USDT") and len(value) > 4:
            value = f"{value[:-4]}/USDT"
        else:
            raise HistoricalReplayError(
                "historical analysis symbols must be direct USDT markets"
            )
    base, quote = value.split("/", 1)
    if not base or quote != "USDT":
        raise HistoricalReplayError(
            "historical analysis currently supports direct */USDT Spot markets only"
        )
    return f"{base}/USDT"


def _balances(raw: dict[str, float]) -> dict[str, float]:
    balances: dict[str, float] = {}
    for asset, amount in raw.items():
        normalized = asset.strip().upper()
        number = float(amount)
        if not normalized or not math.isfinite(number) or number <= 0:
            raise HistoricalReplayError("initial balances must be positive asset amounts")
        balances[normalized] = number
    if not balances:
        raise HistoricalReplayError("at least one initial balance is required")
    return balances


def _candle_series(rows: list[list[float | int]]) -> dict[int, tuple[float, float]]:
    series: dict[int, tuple[float, float]] = {}
    for row in rows:
        if len(row) < 6:
            raise HistoricalReplayError("historical candle row is incomplete")
        timestamp = int(row[0])
        close = float(row[4])
        volume = float(row[5])
        if not math.isfinite(close) or close <= 0 or not math.isfinite(volume) or volume < 0:
            raise HistoricalReplayError("historical candle values are invalid")
        series[timestamp] = (close, close * volume)
    return series


def _fixture(
    symbols: tuple[str, ...],
    balances: dict[str, float],
    series: dict[str, dict[int, tuple[float, float]]],
    common_times: list[int],
    *,
    timeframe: str,
    required_observations: int,
    taker_fee: float,
    spread_bps: float,
    slippage_buffer_bps: float,
    min_notional: float,
) -> dict[str, Any]:
    if any(value < 0 or not math.isfinite(value) for value in (taker_fee, spread_bps, slippage_buffer_bps, min_notional)):
        raise HistoricalReplayError("modeled cost inputs must be non-negative")
    return {
        "started_at": datetime.fromtimestamp(common_times[0] / 1000, tz=timezone.utc).isoformat(),
        "cycle_seconds": _timeframe_seconds(timeframe),
        "initial_balances": balances,
        "commission": {
            "maker": str(taker_fee),
            "taker": str(taker_fee),
            "discount_enabled": False,
        },
        "settings": {
            "benchmarks": ["USDT", "BTC", "PAXG", "XAUT"],
            "benchmark_weights": [0.25, 0.25, 0.25, 0.25],
            "min_quote_volume_usdt": 0,
            # Allow the modeled spread itself; leave a small tolerance for
            # floating-point reconstruction from synthetic bid/ask prices.
            "max_spread_bps": max(spread_bps + 0.001, 1.0),
            "slippage_buffer_bps": slippage_buffer_bps,
            "ticker_stale_seconds": 0,
            "max_scoring_assets": 0,
            "momentum_observations": required_observations,
            "decision_cooldown_seconds": 0,
            "max_allocation_percent": 100,
            "max_reallocation_percent": 10,
            "min_improvement_bps": 0,
        },
        "markets": [
            {
                "symbol": symbol,
                "price_series": [series[symbol][timestamp][0] for timestamp in common_times],
                "quote_volume_series": [
                    series[symbol][timestamp][1] for timestamp in common_times
                ],
                "min_amount": 0.00000001,
                "min_notional": min_notional,
                "amount_precision": 0.00000001,
                "spread_bps": spread_bps,
            }
            for symbol in symbols
        ],
    }


def _timeframe_seconds(timeframe: str) -> int:
    units = {"m": 60, "h": 3600, "d": 86400}
    normalized = timeframe.strip().lower()
    try:
        value = int(normalized[:-1])
        seconds = units[normalized[-1]] * value
    except (KeyError, ValueError, IndexError) as exc:
        raise HistoricalReplayError("timeframe must look like 15m, 1h, or 1d") from exc
    if seconds <= 0:
        raise HistoricalReplayError("timeframe must be positive")
    return seconds


def _parse_timestamp(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    return parsed if parsed.tzinfo is not None else parsed.replace(tzinfo=timezone.utc)


def _parse_balance(value: str) -> tuple[str, float]:
    try:
        asset, raw_amount = value.split("=", 1)
        amount = float(raw_amount)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("balance must look like BNB=0.016") from exc
    return asset, amount


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Run isolated Spot research replay from Binance public candles"
    )
    parser.add_argument("--symbol", action="append", default=[], help="candidate direct */USDT market")
    parser.add_argument("--balance", action="append", type=_parse_balance, required=True)
    parser.add_argument("--start", type=_parse_timestamp, required=True)
    parser.add_argument("--end", type=_parse_timestamp, required=True)
    parser.add_argument("--timeframe", default="15m")
    parser.add_argument("--required-observations", type=int, default=96)
    parser.add_argument("--cycles", type=int, default=None)
    parser.add_argument("--taker-fee", type=float, default=0.001)
    parser.add_argument("--spread-bps", type=float, default=10.0)
    parser.add_argument("--slippage-buffer-bps", type=float, default=25.0)
    parser.add_argument("--min-notional", type=float, default=5.0)
    parser.add_argument("--db-path", default="/tmp/binance-bot-history-replay.db")
    parser.add_argument("--verbose", action="store_true", help="show candle fetch and replay logs")
    args = parser.parse_args()
    if not args.verbose:
        logger.remove()
    try:
        summary = asyncio.run(
            run_historical_replay(
                symbols=args.symbol,
                initial_balances=dict(args.balance),
                start_time=args.start,
                end_time=args.end,
                db_path=args.db_path,
                timeframe=args.timeframe,
                required_observations=args.required_observations,
                cycles=args.cycles,
                taker_fee=args.taker_fee,
                spread_bps=args.spread_bps,
                slippage_buffer_bps=args.slippage_buffer_bps,
                min_notional=args.min_notional,
            )
        )
    except (BinanceHistoryError, HistoricalReplayError, RuntimeError) as exc:
        raise SystemExit(f"historical replay failed: {exc}") from exc
    print(json.dumps(summary.as_dict(), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()

"""Historical replay harness for delta-neutral funding-rate harvesting.

Trade idea: when a USD-M perp's funding rate is positive enough, long the
spot and short the same perp 1:1. Every 8 hours we receive the funding
rate times notional (since we're net short the perp). Spot/perp price
moves wash out at delta-neutral, leaving funding income minus four taker
fees (open spot + open perp + close spot + close perp) and slippage.

The replay steps through actual funding events from Binance. At each event
it pays the position holder, then runs entry/exit logic on each candidate:

* Open if ``funding_rate >= entry_funding_bps`` and we have free capital.
* Close if ``funding_rate <= exit_funding_bps`` or held duration exceeds
  ``max_hold_funding_events``.

A *positive* verdict here justifies building the live execution path. A
negative verdict says: even with idealised delta-neutral assumptions, the
funding income does not cover four legs of taker fees plus slippage on
realistic markets. In that case, don't build execution.
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


FundingRow = Mapping[str, Any]  # {"fundingTime": int, "fundingRate": str|float, "markPrice": str|float, "symbol": str}


@dataclass(frozen=True, slots=True)
class FundingArbConfig:
    """Cost-aware funding-harvest knobs.

    Defaults assume Binance VIP0: taker fees 10 bps spot, 5 bps futures.
    Funding is paid every 8h. Slippage is per-leg, applied four times
    (open spot, open perp, close spot, close perp).
    """

    entry_funding_bps: float = 1.0
    exit_funding_bps: float = -0.5
    max_hold_funding_events: int = 90  # ~30 days at 3/day
    taker_fee_spot_bps: float = 10.0
    taker_fee_futures_bps: float = 5.0
    slippage_per_leg_bps: float = 2.0
    max_concurrent_positions: int = 5
    per_position_notional_pct: float = 20.0  # 20% of starting balance per position
    starting_balance: float = 10_000.0


@dataclass(frozen=True, slots=True)
class FundingTrade:
    symbol: str
    open_time_ms: int
    close_time_ms: int
    entry_funding_bps: float
    funding_received_bps: float
    funding_events_held: int
    round_trip_cost_bps: float
    net_pnl_bps: float


@dataclass(frozen=True, slots=True)
class FundingBacktestResult:
    config: FundingArbConfig
    initial_balance: float
    final_balance: float
    total_return_pct: float
    annualized_return_pct: float
    sharpe_annualized: float
    max_drawdown_pct: float
    num_trades: int
    win_rate_pct: float
    avg_holding_events: float
    funding_collected_bps: float
    fees_paid_bps: float
    bars_in_position_pct: float
    bars_evaluated: int
    equity_curve: list[tuple[int, float]] = field(default_factory=list)
    trades: list[FundingTrade] = field(default_factory=list)

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


def _rate_to_bps(value: Any) -> float:
    return float(value) * 10_000.0


def _funding_time_ms(row: FundingRow) -> int:
    return int(row["fundingTime"])


def _funding_rate_bps(row: FundingRow) -> float:
    return _rate_to_bps(row["fundingRate"])


def _build_grid(
    funding_by_symbol: Mapping[str, Sequence[FundingRow]],
) -> tuple[list[int], dict[str, dict[int, FundingRow]]]:
    indexed: dict[str, dict[int, FundingRow]] = {}
    grid: set[int] = set()
    for symbol, rows in funding_by_symbol.items():
        per_symbol: dict[int, FundingRow] = {}
        for row in rows:
            ts = _funding_time_ms(row)
            per_symbol[ts] = row
            grid.add(ts)
        indexed[symbol] = per_symbol
    return sorted(grid), indexed


def replay_funding(
    funding_by_symbol: Mapping[str, Sequence[FundingRow]],
    config: FundingArbConfig | None = None,
) -> FundingBacktestResult:
    """Replay delta-neutral funding harvest over historical funding events.

    The simulator owns the spot leg outright (no margin borrow), and shorts
    the perp at notional matching the spot leg (1x leverage, isolated, with
    USDT collateral set aside from equity). Funding income at each event is
    ``rate * notional`` paid to the short side. Round-trip cost is four
    taker fees plus four slippage legs.
    """
    cfg = config or FundingArbConfig()
    if cfg.starting_balance <= 0:
        raise ValueError("starting_balance must be positive")
    if cfg.per_position_notional_pct <= 0 or cfg.per_position_notional_pct > 100:
        raise ValueError("per_position_notional_pct must be in (0, 100]")
    if cfg.max_concurrent_positions <= 0:
        raise ValueError("max_concurrent_positions must be positive")

    times, by_symbol = _build_grid(funding_by_symbol)
    if not times:
        return FundingBacktestResult(
            config=cfg,
            initial_balance=cfg.starting_balance,
            final_balance=cfg.starting_balance,
            total_return_pct=0.0,
            annualized_return_pct=0.0,
            sharpe_annualized=0.0,
            max_drawdown_pct=0.0,
            num_trades=0,
            win_rate_pct=0.0,
            avg_holding_events=0.0,
            funding_collected_bps=0.0,
            fees_paid_bps=0.0,
            bars_in_position_pct=0.0,
            bars_evaluated=0,
        )

    one_side_cost_bps = (
        cfg.taker_fee_spot_bps
        + cfg.taker_fee_futures_bps
        + 2.0 * cfg.slippage_per_leg_bps
    )
    round_trip_cost_bps = 2.0 * one_side_cost_bps
    notional_per_position = cfg.starting_balance * (cfg.per_position_notional_pct / 100.0)

    equity = cfg.starting_balance
    open_positions: dict[str, dict[str, Any]] = {}
    trades: list[FundingTrade] = []
    equity_curve: list[tuple[int, float]] = []
    per_step_returns: list[float] = []
    wins = 0
    losses = 0
    peak_equity = equity
    max_drawdown_pct = 0.0
    funding_collected_bps_total = 0.0
    fees_paid_bps_total = 0.0
    bars_in_position = 0

    for ts in times:
        equity_before_step = equity

        # 1. Collect funding on open positions whose symbol has a row at ts.
        to_close: list[str] = []
        for symbol, pos in list(open_positions.items()):
            row = by_symbol.get(symbol, {}).get(ts)
            if row is None:
                continue
            funding_bps = _funding_rate_bps(row)
            # Short perp earns when funding > 0; pays when funding < 0.
            funding_income = pos["notional"] * (funding_bps / 10_000.0)
            equity += funding_income
            pos["funding_received_bps"] += funding_bps
            pos["events_held"] += 1
            funding_collected_bps_total += funding_bps
            # Exit logic.
            if funding_bps <= cfg.exit_funding_bps:
                pos["exit_reason"] = f"funding {funding_bps:.2f}bps below exit"
                to_close.append(symbol)
            elif pos["events_held"] >= cfg.max_hold_funding_events:
                pos["exit_reason"] = "max hold reached"
                to_close.append(symbol)

        # 2. Close positions flagged in step 1. Only the close-side leg cost is
        # charged here; the open-side leg was charged at entry.
        for symbol in to_close:
            pos = open_positions.pop(symbol)
            cost = pos["notional"] * (one_side_cost_bps / 10_000.0)
            equity -= cost
            fees_paid_bps_total += one_side_cost_bps
            net_pnl_bps = pos["funding_received_bps"] - round_trip_cost_bps
            trades.append(
                FundingTrade(
                    symbol=symbol,
                    open_time_ms=pos["open_time_ms"],
                    close_time_ms=ts,
                    entry_funding_bps=pos["entry_funding_bps"],
                    funding_received_bps=pos["funding_received_bps"],
                    funding_events_held=pos["events_held"],
                    round_trip_cost_bps=round_trip_cost_bps,
                    net_pnl_bps=net_pnl_bps,
                )
            )
            if net_pnl_bps > 0:
                wins += 1
            elif net_pnl_bps < 0:
                losses += 1

        # 3. Open new positions if capacity remains.
        if len(open_positions) < cfg.max_concurrent_positions:
            scored: list[tuple[float, str, float]] = []
            for symbol, per_symbol in by_symbol.items():
                if symbol in open_positions:
                    continue
                row = per_symbol.get(ts)
                if row is None:
                    continue
                funding_bps = _funding_rate_bps(row)
                if funding_bps < cfg.entry_funding_bps:
                    continue
                scored.append((funding_bps, symbol, funding_bps))
            scored.sort(reverse=True)
            free_slots = cfg.max_concurrent_positions - len(open_positions)
            for funding_bps, symbol, _ in scored[:free_slots]:
                cost = notional_per_position * (one_side_cost_bps / 10_000.0)
                equity -= cost
                fees_paid_bps_total += one_side_cost_bps
                open_positions[symbol] = {
                    "open_time_ms": ts,
                    "entry_funding_bps": funding_bps,
                    "funding_received_bps": 0.0,
                    "events_held": 0,
                    "notional": notional_per_position,
                    "exit_reason": "",
                }

        if open_positions:
            bars_in_position += 1

        step_return = (equity / equity_before_step - 1.0) if equity_before_step > 0 else 0.0
        per_step_returns.append(step_return)
        if step_return > 0:
            pass  # win/loss tracked at trade close, not per-step
        equity_curve.append((ts, equity))
        if equity > peak_equity:
            peak_equity = equity
        else:
            drawdown_pct = (peak_equity - equity) / peak_equity * 100.0
            if drawdown_pct > max_drawdown_pct:
                max_drawdown_pct = drawdown_pct

    # Force-close any still-open positions at the final ts. Only the close-side
    # leg cost is charged; the open-side leg was charged at entry.
    for symbol, pos in open_positions.items():
        cost = pos["notional"] * (one_side_cost_bps / 10_000.0)
        equity -= cost
        fees_paid_bps_total += one_side_cost_bps
        trades.append(
            FundingTrade(
                symbol=symbol,
                open_time_ms=pos["open_time_ms"],
                close_time_ms=times[-1],
                entry_funding_bps=pos["entry_funding_bps"],
                funding_received_bps=pos["funding_received_bps"],
                funding_events_held=pos["events_held"],
                round_trip_cost_bps=round_trip_cost_bps,
                net_pnl_bps=pos["funding_received_bps"] - round_trip_cost_bps,
            )
        )
    open_positions.clear()

    bars_evaluated = len(per_step_returns)
    if bars_evaluated == 0:
        return FundingBacktestResult(
            config=cfg,
            initial_balance=cfg.starting_balance,
            final_balance=equity,
            total_return_pct=0.0,
            annualized_return_pct=0.0,
            sharpe_annualized=0.0,
            max_drawdown_pct=0.0,
            num_trades=len(trades),
            win_rate_pct=0.0,
            avg_holding_events=0.0,
            funding_collected_bps=funding_collected_bps_total,
            fees_paid_bps=fees_paid_bps_total,
            bars_in_position_pct=0.0,
            bars_evaluated=0,
        )

    total_return_pct = (equity / cfg.starting_balance - 1.0) * 100.0
    # 3 funding events per day → ~1095 per year.
    bars_per_year = 365.0 * 3.0
    years = bars_evaluated / bars_per_year
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
    avg_holding_events = (
        sum(t.funding_events_held for t in trades) / len(trades) if trades else 0.0
    )
    bars_in_position_pct = bars_in_position / bars_evaluated * 100.0

    return FundingBacktestResult(
        config=cfg,
        initial_balance=cfg.starting_balance,
        final_balance=equity,
        total_return_pct=total_return_pct,
        annualized_return_pct=annualized_return_pct,
        sharpe_annualized=sharpe,
        max_drawdown_pct=max_drawdown_pct,
        num_trades=len(trades),
        win_rate_pct=win_rate_pct,
        avg_holding_events=avg_holding_events,
        funding_collected_bps=funding_collected_bps_total,
        fees_paid_bps=fees_paid_bps_total,
        bars_in_position_pct=bars_in_position_pct,
        bars_evaluated=bars_evaluated,
        equity_curve=equity_curve,
        trades=trades,
    )


def _result_summary(result: FundingBacktestResult) -> dict[str, Any]:
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
        "avg_holding_events": round(result.avg_holding_events, 2),
        "funding_collected_bps": round(result.funding_collected_bps, 2),
        "fees_paid_bps": round(result.fees_paid_bps, 2),
        "bars_in_position_pct": round(result.bars_in_position_pct, 2),
        "bars_evaluated": result.bars_evaluated,
    }


async def _fetch_funding_history(
    symbol: str,
    *,
    start: datetime,
    end: datetime,
) -> list[FundingRow]:
    """Page through Binance USD-M funding rate history for one symbol."""
    import aiohttp

    rest_symbol = symbol.replace("/", "").replace(":USDT", "").upper()
    url = "https://fapi.binance.com/fapi/v1/fundingRate"
    out: list[FundingRow] = []
    cursor = int(start.timestamp() * 1000)
    end_ms = int(end.timestamp() * 1000)
    async with aiohttp.ClientSession() as session:
        while cursor <= end_ms:
            params = {
                "symbol": rest_symbol,
                "startTime": cursor,
                "endTime": end_ms,
                "limit": 1000,
            }
            async with session.get(url, params=params) as resp:
                resp.raise_for_status()
                page = await resp.json()
            if not page:
                break
            for row in page:
                out.append(
                    {
                        "symbol": row["symbol"],
                        "fundingTime": int(row["fundingTime"]),
                        "fundingRate": row["fundingRate"],
                        "markPrice": row.get("markPrice", "0"),
                    }
                )
            last_time = int(page[-1]["fundingTime"])
            if last_time <= cursor:
                break
            cursor = last_time + 1
            if len(page) < 1000:
                break
    return out


async def _fetch_funding_for_symbols(
    symbols: Sequence[str],
    *,
    start: datetime,
    end: datetime,
    cache_dir: Path | None = None,
    concurrency: int = 4,
) -> dict[str, list[FundingRow]]:
    sem = asyncio.Semaphore(max(concurrency, 1))
    result: dict[str, list[FundingRow]] = {}

    def _cache_path(symbol: str) -> Path | None:
        if cache_dir is None:
            return None
        key = symbol.replace("/", "_").replace(":", "_")
        return cache_dir / f"funding_{key}_{int(start.timestamp())}_{int(end.timestamp())}.json"

    async def _one(symbol: str) -> None:
        path = _cache_path(symbol)
        if path is not None and path.exists():
            try:
                result[symbol] = json.loads(path.read_text())
                return
            except (OSError, json.JSONDecodeError):
                pass
        async with sem:
            rows = await _fetch_funding_history(symbol, start=start, end=end)
        result[symbol] = rows
        if path is not None:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(rows))

    await asyncio.gather(*(_one(s) for s in symbols))
    return result


async def _fetch_perp_universe(
    *,
    max_symbols: int,
    min_quote_volume_usdt: float,
) -> list[str]:
    """Top-N USD-M perp symbols by 24h quote volume."""
    import aiohttp

    async with aiohttp.ClientSession() as session:
        async with session.get("https://fapi.binance.com/fapi/v1/exchangeInfo") as resp:
            resp.raise_for_status()
            info = await resp.json()
        async with session.get("https://fapi.binance.com/fapi/v1/ticker/24hr") as resp:
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
        if sym.get("contractType") != "PERPETUAL":
            continue
        if sym.get("quoteAsset") != "USDT":
            continue
        binance_symbol = sym.get("symbol", "")
        volume = volume_by_symbol.get(binance_symbol, 0.0)
        if volume < min_quote_volume_usdt:
            continue
        base = sym.get("baseAsset", "")
        candidates.append((volume, f"{base}/USDT"))

    candidates.sort(reverse=True)
    return [s for _, s in candidates[:max_symbols]]


def _load_fixture(path: Path) -> dict[str, list[FundingRow]]:
    raw = json.loads(path.read_text())
    if not isinstance(raw, Mapping):
        raise ValueError("funding fixture must be a mapping of symbol -> rows")
    return {str(k): list(v) for k, v in raw.items()}


def _parse_iso(value: str) -> datetime:
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _build_parser() -> argparse.ArgumentParser:
    defaults = FundingArbConfig()
    parser = argparse.ArgumentParser(
        description=(
            "Replay delta-neutral funding-rate harvesting on Binance USD-M perps "
            "against historical funding events. Use the verdict to decide whether "
            "to build the live execution path."
        )
    )
    parser.add_argument(
        "--symbols",
        default="BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT,XRP/USDT,ADA/USDT",
        help="comma-separated CCXT-style symbols",
    )
    parser.add_argument(
        "--universe",
        default="",
        choices=("", "perp_usdt_top"),
        help="if set, ignore --symbols and pull all USD-M perp USDT pairs by 24h volume",
    )
    parser.add_argument("--universe-size", type=int, default=50)
    parser.add_argument("--min-quote-volume-usdt", type=float, default=10_000_000.0)
    parser.add_argument("--start", default="", help="ISO start; required unless --fixture")
    parser.add_argument("--end", default="", help="ISO end; defaults to now")
    parser.add_argument("--fixture", default="")
    parser.add_argument("--output", default="")
    parser.add_argument(
        "--funding-cache",
        default="",
        help="directory to cache fetched funding rows",
    )
    parser.add_argument("--fetch-concurrency", type=int, default=4)
    parser.add_argument(
        "--entry-funding-bps",
        type=float,
        default=defaults.entry_funding_bps,
    )
    parser.add_argument(
        "--exit-funding-bps",
        type=float,
        default=defaults.exit_funding_bps,
    )
    parser.add_argument(
        "--max-hold-events",
        type=int,
        default=defaults.max_hold_funding_events,
    )
    parser.add_argument(
        "--taker-fee-spot-bps",
        type=float,
        default=defaults.taker_fee_spot_bps,
    )
    parser.add_argument(
        "--taker-fee-futures-bps",
        type=float,
        default=defaults.taker_fee_futures_bps,
    )
    parser.add_argument(
        "--slippage-per-leg-bps",
        type=float,
        default=defaults.slippage_per_leg_bps,
    )
    parser.add_argument(
        "--max-concurrent-positions",
        type=int,
        default=defaults.max_concurrent_positions,
    )
    parser.add_argument(
        "--per-position-notional-pct",
        type=float,
        default=defaults.per_position_notional_pct,
    )
    parser.add_argument(
        "--starting-balance",
        type=float,
        default=defaults.starting_balance,
    )
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)

    cfg = FundingArbConfig(
        entry_funding_bps=args.entry_funding_bps,
        exit_funding_bps=args.exit_funding_bps,
        max_hold_funding_events=args.max_hold_events,
        taker_fee_spot_bps=args.taker_fee_spot_bps,
        taker_fee_futures_bps=args.taker_fee_futures_bps,
        slippage_per_leg_bps=args.slippage_per_leg_bps,
        max_concurrent_positions=args.max_concurrent_positions,
        per_position_notional_pct=args.per_position_notional_pct,
        starting_balance=args.starting_balance,
    )

    if args.fixture:
        funding = _load_fixture(Path(args.fixture))
    else:
        if not args.start:
            parser.error("either --fixture or --start must be supplied")
        start = _parse_iso(args.start)
        end = _parse_iso(args.end) if args.end else datetime.now(tz=timezone.utc)
        if args.universe == "perp_usdt_top":
            symbols = asyncio.run(
                _fetch_perp_universe(
                    max_symbols=args.universe_size,
                    min_quote_volume_usdt=args.min_quote_volume_usdt,
                )
            )
            print(
                json.dumps(
                    {
                        "universe": "perp_usdt_top",
                        "symbol_count": len(symbols),
                        "first_10": symbols[:10],
                    },
                    indent=2,
                )
            )
        else:
            symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]
        cache_dir = Path(args.funding_cache) if args.funding_cache else None
        funding = asyncio.run(
            _fetch_funding_for_symbols(
                symbols,
                start=start,
                end=end,
                cache_dir=cache_dir,
                concurrency=args.fetch_concurrency,
            )
        )

    result = replay_funding(funding, cfg)
    summary = _result_summary(result)
    print(json.dumps(summary, indent=2, sort_keys=True))
    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

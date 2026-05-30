"""Historical replay harness for spot/perp basis-convergence arbitrage.

Strategy: when the perp mark price diverges from spot beyond an entry
threshold, take a delta-neutral position betting on basis convergence.

* basis_bps = (mark - spot) / spot * 10000
* basis_bps > +entry: perp is expensive → short perp, long spot
* basis_bps < -entry: perp is cheap → long perp, short spot (skipped in
  spot-only mode since shorting spot requires margin or borrowing)
* Close when |basis_bps| drops below exit, max-hold elapses, or basis
  blows out past stop threshold.

In contrast to funding arb (which is paid at fixed 8h windows on the
funding rate), basis arb realizes its edge on the *convergence* of the
spot/perp gap — usually quickly, often within hours.

The harness operates in one of two modes:

* spot_only: only takes basis > +entry signals (short perp + long spot)
* both_sides: also takes basis < -entry (requires shortable spot)
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
class BasisArbConfig:
    interval: str = "1h"
    entry_basis_bps: float = 30.0
    exit_basis_bps: float = 5.0
    stop_basis_bps: float = 200.0
    max_hold_bars: int = 24
    taker_fee_spot_bps: float = 10.0
    taker_fee_futures_bps: float = 5.0
    slippage_per_leg_bps: float = 2.0
    notional_per_position: float = 1_000.0
    starting_balance: float = 5_000.0
    mode: str = "spot_only"  # "spot_only" | "both_sides"


@dataclass(frozen=True, slots=True)
class BasisTrade:
    symbol: str
    side: str  # "long_basis" | "short_basis"
    open_time_ms: int
    close_time_ms: int
    entry_basis_bps: float
    exit_basis_bps: float
    bars_held: int
    realized_basis_bps: float
    pnl_quote: float
    fees_quote: float
    exit_reason: str


@dataclass(frozen=True, slots=True)
class BasisArbResult:
    config: BasisArbConfig
    symbol: str
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
    avg_basis_bps: float
    median_basis_bps: float
    equity_curve: list[tuple[int, float]] = field(default_factory=list)
    trades: list[BasisTrade] = field(default_factory=list)

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


def _align_spot_perp(
    spot_klines: Sequence[Kline], perp_klines: Sequence[Kline]
) -> tuple[list[int], dict[int, tuple[float, float]]]:
    spot_idx = {_open_ms(k): _close(k) for k in spot_klines}
    perp_idx = {_open_ms(k): _close(k) for k in perp_klines}
    common = sorted(spot_idx.keys() & perp_idx.keys())
    return common, {t: (spot_idx[t], perp_idx[t]) for t in common}


def replay_basis_arb(
    symbol: str,
    spot_klines: Sequence[Kline],
    perp_klines: Sequence[Kline],
    config: BasisArbConfig | None = None,
) -> BasisArbResult:
    """Replay basis-convergence arbitrage for one symbol."""
    cfg = config or BasisArbConfig()
    if cfg.starting_balance <= 0:
        raise ValueError("starting_balance must be positive")
    if cfg.notional_per_position <= 0:
        raise ValueError("notional_per_position must be positive")
    if cfg.entry_basis_bps <= cfg.exit_basis_bps:
        raise ValueError("entry_basis_bps must be > exit_basis_bps")
    if cfg.mode not in {"spot_only", "both_sides"}:
        raise ValueError("mode must be spot_only or both_sides")

    times, prices = _align_spot_perp(spot_klines, perp_klines)
    if not times:
        return BasisArbResult(
            config=cfg, symbol=symbol,
            initial_balance=cfg.starting_balance, final_equity=cfg.starting_balance,
            total_return_pct=0.0, annualized_return_pct=0.0,
            sharpe_annualized=0.0, max_drawdown_pct=0.0,
            num_trades=0, win_rate_pct=0.0, avg_bars_held=0.0,
            fees_paid_quote=0.0, bars_evaluated=0, bars_in_position_pct=0.0,
            avg_basis_bps=0.0, median_basis_bps=0.0,
        )

    one_side_cost_bps = (
        cfg.taker_fee_spot_bps + cfg.taker_fee_futures_bps + 2.0 * cfg.slippage_per_leg_bps
    )
    round_trip_cost_bps = 2.0 * one_side_cost_bps

    equity = cfg.starting_balance
    fees_paid_quote = 0.0
    trades: list[BasisTrade] = []
    open_position: dict[str, Any] | None = None
    equity_curve: list[tuple[int, float]] = []
    per_step_returns: list[float] = []
    basis_history: list[float] = []
    peak_equity = equity
    max_drawdown_pct = 0.0
    bars_in_position = 0
    last_equity = equity

    for ts in times:
        spot_price, mark_price = prices[ts]
        if spot_price <= 0:
            continue
        basis_bps = (mark_price - spot_price) / spot_price * 10_000.0
        basis_history.append(basis_bps)

        if open_position is not None:
            open_position["bars_held"] = open_position["bars_held"] + 1
            close_reason = ""
            if open_position["side"] == "short_basis":
                # basis converging means basis_bps drops toward 0
                if basis_bps <= cfg.exit_basis_bps:
                    close_reason = f"basis {basis_bps:.1f}bps within exit ({cfg.exit_basis_bps})"
                elif basis_bps >= cfg.stop_basis_bps:
                    close_reason = f"basis {basis_bps:.1f}bps blew past stop ({cfg.stop_basis_bps})"
            else:  # long_basis
                if basis_bps >= -cfg.exit_basis_bps:
                    close_reason = f"basis {basis_bps:.1f}bps within exit ({cfg.exit_basis_bps})"
                elif basis_bps <= -cfg.stop_basis_bps:
                    close_reason = f"basis {basis_bps:.1f}bps blew past stop ({cfg.stop_basis_bps})"
            if not close_reason and open_position["bars_held"] >= cfg.max_hold_bars:
                close_reason = f"max hold ({cfg.max_hold_bars} bars) reached"

            if close_reason:
                realized_basis_bps = open_position["entry_basis_bps"] - basis_bps
                if open_position["side"] == "long_basis":
                    realized_basis_bps = -realized_basis_bps
                gross_pnl = cfg.notional_per_position * (realized_basis_bps / 10_000.0)
                close_fees = cfg.notional_per_position * (one_side_cost_bps / 10_000.0)
                final_pnl = gross_pnl - open_position["open_fees"] - close_fees
                equity = open_position["entry_equity"] + final_pnl
                fees_paid_quote += close_fees
                trades.append(BasisTrade(
                    symbol=symbol, side=open_position["side"],
                    open_time_ms=open_position["open_time_ms"],
                    close_time_ms=ts,
                    entry_basis_bps=open_position["entry_basis_bps"],
                    exit_basis_bps=basis_bps,
                    bars_held=open_position["bars_held"],
                    realized_basis_bps=realized_basis_bps,
                    pnl_quote=final_pnl,
                    fees_quote=open_position["open_fees"] + close_fees,
                    exit_reason=close_reason,
                ))
                open_position = None

        if open_position is None:
            if basis_bps >= cfg.entry_basis_bps:
                open_fees = cfg.notional_per_position * (one_side_cost_bps / 10_000.0)
                fees_paid_quote += open_fees
                open_position = {
                    "side": "short_basis",
                    "open_time_ms": ts,
                    "entry_basis_bps": basis_bps,
                    "bars_held": 0,
                    "open_fees": open_fees,
                    "entry_equity": equity,
                }
            elif cfg.mode == "both_sides" and basis_bps <= -cfg.entry_basis_bps:
                open_fees = cfg.notional_per_position * (one_side_cost_bps / 10_000.0)
                fees_paid_quote += open_fees
                open_position = {
                    "side": "long_basis",
                    "open_time_ms": ts,
                    "entry_basis_bps": basis_bps,
                    "bars_held": 0,
                    "open_fees": open_fees,
                    "entry_equity": equity,
                }

        if open_position is not None:
            bars_in_position += 1
        per_step_returns.append((equity / last_equity - 1.0) if last_equity > 0 else 0.0)
        last_equity = equity
        equity_curve.append((ts, equity))
        if equity > peak_equity:
            peak_equity = equity
        elif peak_equity > 0:
            dd = (peak_equity - equity) / peak_equity * 100.0
            if dd > max_drawdown_pct:
                max_drawdown_pct = dd

    # Force-close at the end of the window.
    if open_position is not None and times:
        ts = times[-1]
        spot_price, mark_price = prices[ts]
        basis_bps = (mark_price - spot_price) / spot_price * 10_000.0
        realized_basis_bps = open_position["entry_basis_bps"] - basis_bps
        if open_position["side"] == "long_basis":
            realized_basis_bps = -realized_basis_bps
        gross_pnl = cfg.notional_per_position * (realized_basis_bps / 10_000.0)
        close_fees = cfg.notional_per_position * (one_side_cost_bps / 10_000.0)
        final_pnl = gross_pnl - open_position["open_fees"] - close_fees
        equity = open_position["entry_equity"] + final_pnl
        fees_paid_quote += close_fees
        trades.append(BasisTrade(
            symbol=symbol, side=open_position["side"],
            open_time_ms=open_position["open_time_ms"], close_time_ms=ts,
            entry_basis_bps=open_position["entry_basis_bps"],
            exit_basis_bps=basis_bps, bars_held=open_position["bars_held"],
            realized_basis_bps=realized_basis_bps, pnl_quote=final_pnl,
            fees_quote=open_position["open_fees"] + close_fees,
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
    avg_basis = statistics.fmean(basis_history) if basis_history else 0.0
    median_basis = statistics.median(basis_history) if basis_history else 0.0

    return BasisArbResult(
        config=cfg, symbol=symbol,
        initial_balance=cfg.starting_balance, final_equity=equity,
        total_return_pct=total_return_pct,
        annualized_return_pct=annualized_return_pct,
        sharpe_annualized=sharpe, max_drawdown_pct=max_drawdown_pct,
        num_trades=len(trades), win_rate_pct=win_rate_pct,
        avg_bars_held=avg_bars_held, fees_paid_quote=fees_paid_quote,
        bars_evaluated=bars_evaluated, bars_in_position_pct=bars_in_position_pct,
        avg_basis_bps=avg_basis, median_basis_bps=median_basis,
        equity_curve=equity_curve, trades=trades,
    )


def _result_summary(result: BasisArbResult) -> dict[str, Any]:
    return {
        "verdict": result.verdict(),
        "symbol": result.symbol,
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
        "avg_basis_bps": round(result.avg_basis_bps, 4),
        "median_basis_bps": round(result.median_basis_bps, 4),
    }


async def _fetch_spot_klines(symbol: str, *, interval: str, start: datetime,
                              end: datetime) -> list[list[Any]]:
    from src.data.history import HistoryFetcher
    fetcher = HistoryFetcher()
    rows = await fetcher.fetch_klines(symbol, interval, start_time=start, end_time=end)
    return [list(k) for k in rows]


async def _fetch_perp_klines(symbol: str, *, interval: str, start: datetime,
                              end: datetime) -> list[list[Any]]:
    """Fetch USD-M perp klines via Binance Futures REST."""
    import aiohttp
    rest_symbol = symbol.replace("/", "").upper()
    url = "https://fapi.binance.com/fapi/v1/klines"
    out: list[list[Any]] = []
    cursor = int(start.timestamp() * 1000)
    end_ms = int(end.timestamp() * 1000)
    async with aiohttp.ClientSession() as session:
        while cursor <= end_ms:
            params = {"symbol": rest_symbol, "interval": interval,
                      "startTime": cursor, "endTime": end_ms, "limit": 1500}
            async with session.get(url, params=params) as resp:
                resp.raise_for_status()
                page = await resp.json()
            if not page:
                break
            out.extend([list(row) for row in page])
            last_open = int(page[-1][0])
            if last_open <= cursor:
                break
            cursor = last_open + 1
            if len(page) < 1500:
                break
    return out


def _load_fixture(path: Path) -> dict[str, list[list[Any]]]:
    return json.loads(path.read_text())


def _parse_iso(value: str) -> datetime:
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _build_parser() -> argparse.ArgumentParser:
    defaults = BasisArbConfig()
    parser = argparse.ArgumentParser(
        description=(
            "Replay perp/spot basis-convergence arbitrage on one symbol. "
            "Reads spot klines from /api/v3/klines and perp from /fapi/v1/klines."
        )
    )
    parser.add_argument("--symbol", default="BTC/USDT")
    parser.add_argument("--interval", default=defaults.interval)
    parser.add_argument("--start", default="")
    parser.add_argument("--end", default="")
    parser.add_argument("--fixture", default="")
    parser.add_argument("--output", default="")
    parser.add_argument("--entry-basis-bps", type=float, default=defaults.entry_basis_bps)
    parser.add_argument("--exit-basis-bps", type=float, default=defaults.exit_basis_bps)
    parser.add_argument("--stop-basis-bps", type=float, default=defaults.stop_basis_bps)
    parser.add_argument("--max-hold-bars", type=int, default=defaults.max_hold_bars)
    parser.add_argument("--taker-fee-spot-bps", type=float, default=defaults.taker_fee_spot_bps)
    parser.add_argument("--taker-fee-futures-bps", type=float, default=defaults.taker_fee_futures_bps)
    parser.add_argument("--slippage-per-leg-bps", type=float, default=defaults.slippage_per_leg_bps)
    parser.add_argument("--notional-per-position", type=float, default=defaults.notional_per_position)
    parser.add_argument("--starting-balance", type=float, default=defaults.starting_balance)
    parser.add_argument("--mode", default=defaults.mode, choices=("spot_only", "both_sides"))
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    cfg = BasisArbConfig(
        interval=args.interval, entry_basis_bps=args.entry_basis_bps,
        exit_basis_bps=args.exit_basis_bps, stop_basis_bps=args.stop_basis_bps,
        max_hold_bars=args.max_hold_bars,
        taker_fee_spot_bps=args.taker_fee_spot_bps,
        taker_fee_futures_bps=args.taker_fee_futures_bps,
        slippage_per_leg_bps=args.slippage_per_leg_bps,
        notional_per_position=args.notional_per_position,
        starting_balance=args.starting_balance, mode=args.mode,
    )
    if args.fixture:
        payload = _load_fixture(Path(args.fixture))
        spot_klines = payload["spot"]
        perp_klines = payload["perp"]
    else:
        if not args.start:
            parser.error("either --fixture or --start must be supplied")
        start = _parse_iso(args.start)
        end = _parse_iso(args.end) if args.end else datetime.now(tz=timezone.utc)

        async def _fetch_both():
            return await asyncio.gather(
                _fetch_spot_klines(args.symbol, interval=args.interval, start=start, end=end),
                _fetch_perp_klines(args.symbol, interval=args.interval, start=start, end=end),
            )

        spot_klines, perp_klines = asyncio.run(_fetch_both())
    result = replay_basis_arb(args.symbol, spot_klines, perp_klines, cfg)
    print(json.dumps(_result_summary(result), indent=2, sort_keys=True))
    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

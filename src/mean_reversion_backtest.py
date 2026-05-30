"""Historical replay harness for short-horizon dump-reversion trades.

Hypothesis: when a liquid spot asset dumps materially over a short window
(e.g. > 5% in 15 minutes), buyers step in and the price retraces a portion
of the move within the next hour. The trade is:

  * Enter long when trailing 15m return <= dump_threshold_pct.
  * Exit when one of:
      - price retraces ``retrace_target_pct`` of the dump (take profit)
      - price drops a further ``stop_loss_pct`` from entry (stop loss)
      - we've held ``max_hold_bars`` minutes (time exit)

The backtest replays 1-minute klines per symbol, fires the rule, and
tracks net P&L after taker fees + slippage. The harness is intentionally
single-position-per-symbol — if a dump fires while we already hold the
same symbol we ignore the signal, no pyramiding.

A positive verdict here justifies the live WebSocket runtime build.
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

_BARS_PER_YEAR_BY_INTERVAL: dict[str, float] = {
    "1m": 525_600.0, "3m": 175_200.0, "5m": 105_120.0,
    "15m": 35_040.0, "1h": 8_760.0, "1d": 365.0,
}


@dataclass(frozen=True, slots=True)
class MeanReversionConfig:
    """Dump-reversion knobs.

    Defaults: enter on 15m drops >= 5%, take profit when 50% of the drop
    is recovered, stop out on a further 3% drop from entry, time-exit after
    60 minutes. 10 bps taker fee + 5 bps slippage per side mirrors the
    other Binance-side bots.
    """

    interval: str = "1m"
    dump_lookback_bars: int = 15
    dump_threshold_pct: float = -5.0  # enter when 15m return <= this
    retrace_target_pct: float = 0.5   # fraction of the drop to recover
    stop_loss_pct: float = -3.0       # additional drop from entry = stop
    max_hold_bars: int = 60
    taker_fee_bps: float = 10.0
    slippage_bps: float = 5.0
    starting_balance: float = 1_000.0
    notional_per_position: float = 100.0
    max_concurrent_positions: int = 5


@dataclass(frozen=True, slots=True)
class ReversionTrade:
    symbol: str
    entry_time_ms: int
    exit_time_ms: int
    entry_price: float
    exit_price: float
    notional_usdt: float
    dump_pct: float
    bars_held: int
    pnl_quote: float
    fee_quote: float
    exit_reason: str


@dataclass(frozen=True, slots=True)
class MeanReversionResult:
    config: MeanReversionConfig
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
    avg_pnl_quote: float
    bars_evaluated: int
    bars_in_position_pct: float
    by_symbol_trade_count: dict[str, int] = field(default_factory=dict)
    by_exit_reason: dict[str, int] = field(default_factory=dict)
    equity_curve: list[tuple[int, float]] = field(default_factory=list)
    trades: list[ReversionTrade] = field(default_factory=list)

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


def _open_ms(k: Kline) -> int: return int(k[0])
def _open_px(k: Kline) -> float: return float(k[1])
def _high(k: Kline) -> float: return float(k[2])
def _low(k: Kline) -> float: return float(k[3])
def _close(k: Kline) -> float: return float(k[4])


def _bars_per_year(interval: str) -> float:
    return _BARS_PER_YEAR_BY_INTERVAL.get(interval, 525_600.0)


def _align_by_open_time(
    klines_by_symbol: Mapping[str, Sequence[Kline]],
) -> tuple[list[int], dict[str, dict[int, Kline]]]:
    indexed: dict[str, dict[int, Kline]] = {}
    grid: set[int] = set()
    for symbol, klines in klines_by_symbol.items():
        per_symbol = {_open_ms(k): k for k in klines}
        indexed[symbol] = per_symbol
        grid.update(per_symbol.keys())
    return sorted(grid), indexed


def replay_mean_reversion(
    klines_by_symbol: Mapping[str, Sequence[Kline]],
    config: MeanReversionConfig | None = None,
) -> MeanReversionResult:
    cfg = config or MeanReversionConfig()
    if cfg.starting_balance <= 0:
        raise ValueError("starting_balance must be positive")
    if cfg.notional_per_position <= 0:
        raise ValueError("notional_per_position must be positive")
    if cfg.dump_lookback_bars <= 0:
        raise ValueError("dump_lookback_bars must be positive")
    if cfg.dump_threshold_pct >= 0:
        raise ValueError("dump_threshold_pct must be negative")
    if cfg.max_hold_bars <= 0:
        raise ValueError("max_hold_bars must be positive")
    if cfg.max_concurrent_positions <= 0:
        raise ValueError("max_concurrent_positions must be positive")

    times, by_symbol = _align_by_open_time(klines_by_symbol)
    if len(times) <= cfg.dump_lookback_bars + 1:
        return MeanReversionResult(
            config=cfg, initial_balance=cfg.starting_balance,
            final_equity=cfg.starting_balance, total_return_pct=0.0,
            annualized_return_pct=0.0, sharpe_annualized=0.0,
            max_drawdown_pct=0.0, num_trades=0, win_rate_pct=0.0,
            avg_bars_held=0.0, fees_paid_quote=0.0, avg_pnl_quote=0.0,
            bars_evaluated=0, bars_in_position_pct=0.0,
        )

    fee_rate = cfg.taker_fee_bps / 10_000.0
    slip_rate = cfg.slippage_bps / 10_000.0
    one_side_cost_rate = fee_rate + slip_rate
    round_trip_cost_rate = 2.0 * one_side_cost_rate

    equity = cfg.starting_balance
    open_positions: dict[str, dict[str, Any]] = {}
    trades: list[ReversionTrade] = []
    by_symbol_count: dict[str, int] = {}
    by_exit: dict[str, int] = {}
    equity_curve: list[tuple[int, float]] = []
    per_step_returns: list[float] = []
    peak_equity = equity
    max_drawdown_pct = 0.0
    fees_paid_quote = 0.0
    last_equity = equity
    bars_in_position = 0

    for i, ts in enumerate(times):
        if i < cfg.dump_lookback_bars:
            continue
        ts_back = times[i - cfg.dump_lookback_bars]

        # 1. Check exits on open positions.
        to_close: list[str] = []
        for symbol, pos in open_positions.items():
            kline = by_symbol[symbol].get(ts)
            if kline is None:
                continue
            current_low = _low(kline)
            current_high = _high(kline)
            entry = pos["entry_price"]
            dump = pos["dump_pct"]
            target_price = entry * (1.0 + abs(dump) * cfg.retrace_target_pct / 100.0)
            stop_price = entry * (1.0 + cfg.stop_loss_pct / 100.0)
            exit_reason = ""
            exit_price = entry
            if current_low <= stop_price:
                exit_reason = "stop_loss"
                exit_price = stop_price
            elif current_high >= target_price:
                exit_reason = "take_profit"
                exit_price = target_price
            elif pos["bars_held"] >= cfg.max_hold_bars:
                exit_reason = "max_hold"
                exit_price = _close(kline)
            if exit_reason:
                qty = pos["qty"]
                proceeds = qty * exit_price * (1.0 - one_side_cost_rate)
                cost_basis = pos["entry_equity_committed"]
                pnl = proceeds - cost_basis
                fee_paid = qty * exit_price * one_side_cost_rate + pos["entry_fee"]
                # Cash returns to the wallet; the per-trade P&L is implicit in
                # (proceeds - cost_basis). cost_basis was already deducted at entry.
                equity += proceeds
                fees_paid_quote += qty * exit_price * one_side_cost_rate
                trades.append(ReversionTrade(
                    symbol=symbol, entry_time_ms=pos["entry_time_ms"], exit_time_ms=ts,
                    entry_price=entry, exit_price=exit_price,
                    notional_usdt=pos["notional_usdt"], dump_pct=dump,
                    bars_held=pos["bars_held"], pnl_quote=pnl, fee_quote=fee_paid,
                    exit_reason=exit_reason,
                ))
                by_symbol_count[symbol] = by_symbol_count.get(symbol, 0) + 1
                by_exit[exit_reason] = by_exit.get(exit_reason, 0) + 1
                to_close.append(symbol)
            else:
                pos["bars_held"] += 1
        for symbol in to_close:
            del open_positions[symbol]

        # 2. Look for new entries (only if capacity remains).
        if len(open_positions) < cfg.max_concurrent_positions:
            for symbol, per_sym in by_symbol.items():
                if symbol in open_positions:
                    continue
                here = per_sym.get(ts)
                past = per_sym.get(ts_back)
                if here is None or past is None:
                    continue
                past_close = _close(past)
                if past_close <= 0:
                    continue
                ret_pct = (_close(here) / past_close - 1.0) * 100.0
                if ret_pct > cfg.dump_threshold_pct:
                    continue
                # Enter long at this bar's close.
                entry_price = _close(here)
                qty = cfg.notional_per_position / entry_price
                entry_fee = cfg.notional_per_position * one_side_cost_rate
                # Commit the notional (capital is held until exit).
                cost_basis = cfg.notional_per_position + entry_fee
                if cost_basis > equity:
                    continue
                equity -= cost_basis
                fees_paid_quote += entry_fee
                open_positions[symbol] = {
                    "entry_time_ms": ts, "entry_price": entry_price,
                    "qty": qty, "dump_pct": ret_pct,
                    "notional_usdt": cfg.notional_per_position,
                    "entry_fee": entry_fee,
                    "entry_equity_committed": cost_basis,
                    "bars_held": 0,
                }
                if len(open_positions) >= cfg.max_concurrent_positions:
                    break

        # 3. Mark equity (cash + open position MTM).
        mtm = 0.0
        for symbol, pos in open_positions.items():
            kline = by_symbol[symbol].get(ts)
            if kline is None:
                mtm += pos["entry_equity_committed"]
                continue
            mtm += pos["qty"] * _close(kline)
        bars_in_position += 1 if open_positions else 0
        total_equity = equity + mtm
        per_step_returns.append((total_equity / last_equity - 1.0) if last_equity > 0 else 0.0)
        last_equity = total_equity
        equity_curve.append((ts, total_equity))
        if total_equity > peak_equity:
            peak_equity = total_equity
        elif peak_equity > 0:
            dd = (peak_equity - total_equity) / peak_equity * 100.0
            if dd > max_drawdown_pct:
                max_drawdown_pct = dd

    # Force-close anything still open at the final bar. Same accounting:
    # cost_basis was already taken out at entry; only proceeds return.
    final_ts = times[-1]
    for symbol, pos in list(open_positions.items()):
        kline = by_symbol[symbol].get(final_ts)
        exit_price = _close(kline) if kline else pos["entry_price"]
        qty = pos["qty"]
        proceeds = qty * exit_price * (1.0 - one_side_cost_rate)
        cost_basis = pos["entry_equity_committed"]
        pnl = proceeds - cost_basis
        equity += proceeds
        fees_paid_quote += qty * exit_price * one_side_cost_rate
        trades.append(ReversionTrade(
            symbol=symbol, entry_time_ms=pos["entry_time_ms"], exit_time_ms=final_ts,
            entry_price=pos["entry_price"], exit_price=exit_price,
            notional_usdt=pos["notional_usdt"], dump_pct=pos["dump_pct"],
            bars_held=pos["bars_held"], pnl_quote=pnl,
            fee_quote=qty * exit_price * one_side_cost_rate + pos["entry_fee"],
            exit_reason="window_end",
        ))
        by_symbol_count[symbol] = by_symbol_count.get(symbol, 0) + 1
        by_exit["window_end"] = by_exit.get("window_end", 0) + 1
    open_positions.clear()

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
    avg_bars = sum(t.bars_held for t in trades) / len(trades) if trades else 0.0
    avg_pnl = sum(t.pnl_quote for t in trades) / len(trades) if trades else 0.0
    bars_in_position_pct = bars_in_position / bars_evaluated * 100.0 if bars_evaluated else 0.0

    return MeanReversionResult(
        config=cfg, initial_balance=cfg.starting_balance, final_equity=equity,
        total_return_pct=total_return_pct, annualized_return_pct=annualized_return_pct,
        sharpe_annualized=sharpe, max_drawdown_pct=max_drawdown_pct,
        num_trades=len(trades), win_rate_pct=win_rate_pct,
        avg_bars_held=avg_bars, fees_paid_quote=fees_paid_quote,
        avg_pnl_quote=avg_pnl,
        bars_evaluated=bars_evaluated, bars_in_position_pct=bars_in_position_pct,
        by_symbol_trade_count=by_symbol_count, by_exit_reason=by_exit,
        equity_curve=equity_curve, trades=trades,
    )


def _result_summary(result: MeanReversionResult) -> dict[str, Any]:
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
        "win_rate_pct": round(result.win_rate_pct, 2),
        "avg_bars_held": round(result.avg_bars_held, 2),
        "fees_paid_quote": round(result.fees_paid_quote, 4),
        "avg_pnl_quote": round(result.avg_pnl_quote, 6),
        "bars_evaluated": result.bars_evaluated,
        "bars_in_position_pct": round(result.bars_in_position_pct, 2),
        "by_exit_reason": result.by_exit_reason,
        "by_symbol_trade_count": result.by_symbol_trade_count,
    }


async def _fetch_klines(
    symbols: Sequence[str], *, interval: str, start: datetime, end: datetime,
    cache_dir: Path | None = None, concurrency: int = 6,
) -> dict[str, list[list[Any]]]:
    from src.claude_spot_backtest import _fetch_klines_for_symbols
    return await _fetch_klines_for_symbols(
        symbols, interval=interval, start=start, end=end,
        cache_dir=cache_dir, concurrency=concurrency,
    )


def _load_fixture(path: Path) -> dict[str, list[list[Any]]]:
    raw = json.loads(path.read_text())
    return {str(k): [list(item) for item in v] for k, v in raw.items()}


def _parse_iso(value: str) -> datetime:
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _build_parser() -> argparse.ArgumentParser:
    defaults = MeanReversionConfig()
    parser = argparse.ArgumentParser(
        description=(
            "Replay dump-reversion trades on 1m klines. Enters long on N-min "
            "drops >= dump_threshold; exits on retrace target / stop loss / max hold."
        )
    )
    parser.add_argument("--symbols",
                        default="BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT,XRP/USDT,ADA/USDT,DOGE/USDT,AVAX/USDT")
    parser.add_argument("--interval", default=defaults.interval)
    parser.add_argument("--start", default="")
    parser.add_argument("--end", default="")
    parser.add_argument("--fixture", default="")
    parser.add_argument("--klines-cache", default="")
    parser.add_argument("--output", default="")
    parser.add_argument("--dump-lookback-bars", type=int, default=defaults.dump_lookback_bars)
    parser.add_argument("--dump-threshold-pct", type=float, default=defaults.dump_threshold_pct)
    parser.add_argument("--retrace-target-pct", type=float, default=defaults.retrace_target_pct)
    parser.add_argument("--stop-loss-pct", type=float, default=defaults.stop_loss_pct)
    parser.add_argument("--max-hold-bars", type=int, default=defaults.max_hold_bars)
    parser.add_argument("--taker-fee-bps", type=float, default=defaults.taker_fee_bps)
    parser.add_argument("--slippage-bps", type=float, default=defaults.slippage_bps)
    parser.add_argument("--starting-balance", type=float, default=defaults.starting_balance)
    parser.add_argument("--notional-per-position", type=float, default=defaults.notional_per_position)
    parser.add_argument("--max-concurrent-positions", type=int, default=defaults.max_concurrent_positions)
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    cfg = MeanReversionConfig(
        interval=args.interval,
        dump_lookback_bars=args.dump_lookback_bars,
        dump_threshold_pct=args.dump_threshold_pct,
        retrace_target_pct=args.retrace_target_pct,
        stop_loss_pct=args.stop_loss_pct,
        max_hold_bars=args.max_hold_bars,
        taker_fee_bps=args.taker_fee_bps, slippage_bps=args.slippage_bps,
        starting_balance=args.starting_balance,
        notional_per_position=args.notional_per_position,
        max_concurrent_positions=args.max_concurrent_positions,
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
        klines = asyncio.run(_fetch_klines(
            symbols, interval=args.interval, start=start, end=end, cache_dir=cache_dir,
        ))
    result = replay_mean_reversion(klines, cfg)
    print(json.dumps(_result_summary(result), indent=2, sort_keys=True))
    if args.output:
        Path(args.output).write_text(json.dumps(result.as_dict(), indent=2, sort_keys=True, default=list))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

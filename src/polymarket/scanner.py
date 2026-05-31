"""Polymarket binary-arbitrage scanner.

For each active binary (YES/NO) market, fetches both tokens' orderbooks
and looks for one of two arbitrage patterns:

* **Buy both for less than $1** — if ``best_ask(YES) + best_ask(NO) < 1``
  then buying one share of each guarantees at least $1 at resolution
  (one side wins, paying out $1; the loser pays $0). Profit per pair
  before fees = ``1 - (ask_yes + ask_no)``.

* **Sell both for more than $1** — if ``best_bid(YES) + best_bid(NO) > 1``
  and you can short both, you collect more than $1 in premium against
  a $1 liability. Requires short-selling capacity (most retail accounts
  don't have this without owning the tokens first), so this pattern is
  primarily *advisory* — useful for understanding mispricing but not
  directly executable.

Polymarket charges trading fees per side. Default taker fee of 10 bps
each leg subtracts 0.2% from the gross.

The scanner returns ranked opportunities; an operator (or future
executor) decides whether to act. No order placement here.
"""

from __future__ import annotations

import argparse
import asyncio
import json
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from typing import Any

import aiohttp

from src.polymarket.client import (
    CLOB_API_BASE,
    Orderbook,
    SimplifiedMarket,
    fetch_orderbooks,
    iter_active_markets,
)


@dataclass(frozen=True, slots=True)
class ScanConfig:
    """Knobs for the binary-arbitrage scanner."""

    base_url: str = CLOB_API_BASE
    max_markets: int = 500
    taker_fee_bps_per_leg: float = 10.0
    min_size_per_leg: float = 5.0      # required depth at the BBO
    min_gross_profit: float = 0.005    # 0.5¢ per share gross before listing
    fetch_concurrency: int = 8


@dataclass(frozen=True, slots=True)
class ArbOpportunity:
    """One scored arbitrage candidate from a single binary market."""

    condition_id: str
    yes_token_id: str
    no_token_id: str
    yes_outcome: str
    no_outcome: str

    best_ask_yes: float
    best_ask_no: float
    best_bid_yes: float
    best_bid_no: float
    size_at_best_ask_yes: float
    size_at_best_ask_no: float
    size_at_best_bid_yes: float
    size_at_best_bid_no: float

    # Net-of-fee profit per $1 of payout, on each side
    buy_both_gross_profit: float          # 1 - (ask_yes + ask_no)
    buy_both_net_profit: float            # gross - 4 leg-fees
    sell_both_gross_profit: float         # (bid_yes + bid_no) - 1
    sell_both_net_profit: float           # gross - 4 leg-fees

    max_executable_pairs: float
    direction: str  # "buy_both" | "sell_both" | "none"

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


def _round_trip_fee_per_pair(taker_fee_bps_per_leg: float) -> float:
    """Total fee on the YES + NO buy or sell pair.

    Each share resolves to either $0 or $1, so a fee in bps applies to
    the price paid/received on each leg (max $1). We approximate the
    upper bound by assuming each leg's notional is $1.
    """
    # 2 legs × bps/10000 × $1 each = 2 × bps / 10000
    return 2.0 * (taker_fee_bps_per_leg / 10_000.0)


def evaluate_market(
    market: SimplifiedMarket,
    orderbooks: dict[str, Orderbook],
    *,
    cfg: ScanConfig,
) -> ArbOpportunity | None:
    """Compute the arb metrics for one market; return None if not a binary."""
    if len(market.tokens) != 2:
        return None
    yes_token, no_token = market.tokens[0], market.tokens[1]
    book_yes = orderbooks.get(yes_token.token_id)
    book_no = orderbooks.get(no_token.token_id)
    if book_yes is None or book_no is None:
        return None

    ba_yes = book_yes.best_ask
    ba_no = book_no.best_ask
    bb_yes = book_yes.best_bid
    bb_no = book_no.best_bid
    if ba_yes is None or ba_no is None or bb_yes is None or bb_no is None:
        return None

    fee_per_pair = _round_trip_fee_per_pair(cfg.taker_fee_bps_per_leg)
    buy_gross = 1.0 - (ba_yes.price + ba_no.price)
    sell_gross = (bb_yes.price + bb_no.price) - 1.0
    buy_net = buy_gross - fee_per_pair
    sell_net = sell_gross - fee_per_pair

    # Executable size: the lesser of the two leg sizes (we trade in pairs).
    buy_max_pairs = min(ba_yes.size, ba_no.size) if buy_net > 0 else 0.0
    sell_max_pairs = min(bb_yes.size, bb_no.size) if sell_net > 0 else 0.0

    if buy_net >= sell_net:
        direction = "buy_both" if buy_net > cfg.min_gross_profit and buy_max_pairs >= cfg.min_size_per_leg else "none"
        max_pairs = buy_max_pairs
    else:
        direction = "sell_both" if sell_net > cfg.min_gross_profit and sell_max_pairs >= cfg.min_size_per_leg else "none"
        max_pairs = sell_max_pairs

    return ArbOpportunity(
        condition_id=market.condition_id,
        yes_token_id=yes_token.token_id, no_token_id=no_token.token_id,
        yes_outcome=yes_token.outcome, no_outcome=no_token.outcome,
        best_ask_yes=ba_yes.price, best_ask_no=ba_no.price,
        best_bid_yes=bb_yes.price, best_bid_no=bb_no.price,
        size_at_best_ask_yes=ba_yes.size, size_at_best_ask_no=ba_no.size,
        size_at_best_bid_yes=bb_yes.size, size_at_best_bid_no=bb_no.size,
        buy_both_gross_profit=buy_gross, buy_both_net_profit=buy_net,
        sell_both_gross_profit=sell_gross, sell_both_net_profit=sell_net,
        max_executable_pairs=max_pairs, direction=direction,
    )


async def scan(
    cfg: ScanConfig | None = None,
    *,
    session: aiohttp.ClientSession | None = None,
) -> list[ArbOpportunity]:
    cfg = cfg or ScanConfig()
    owns = session is None
    if session is None:
        session = aiohttp.ClientSession()
    try:
        markets: list[SimplifiedMarket] = []
        async for market in iter_active_markets(
            session=session, base_url=cfg.base_url,
            limit_per_page=100, max_pages=(cfg.max_markets // 100) + 1,
        ):
            if not market.accepting_orders or market.closed:
                continue
            if len(market.tokens) != 2:
                continue
            markets.append(market)
            if len(markets) >= cfg.max_markets:
                break

        all_token_ids: list[str] = []
        for m in markets:
            all_token_ids.extend(t.token_id for t in m.tokens)
        orderbooks = await fetch_orderbooks(
            token_ids=all_token_ids, session=session,
            base_url=cfg.base_url, concurrency=cfg.fetch_concurrency,
        )

        opportunities: list[ArbOpportunity] = []
        for market in markets:
            opp = evaluate_market(market, orderbooks, cfg=cfg)
            if opp is None:
                continue
            if opp.direction == "none":
                continue
            opportunities.append(opp)
        # Rank by net-of-fee absolute profit margin × executable size.
        opportunities.sort(
            key=lambda o: max(o.buy_both_net_profit, o.sell_both_net_profit) * o.max_executable_pairs,
            reverse=True,
        )
        return opportunities
    finally:
        if owns:
            await session.close()


def _summary(opportunities: Sequence[ArbOpportunity], *, max_rows: int = 20) -> dict[str, Any]:
    rows = []
    for opp in opportunities[:max_rows]:
        if opp.direction == "buy_both":
            margin = opp.buy_both_net_profit
        elif opp.direction == "sell_both":
            margin = opp.sell_both_net_profit
        else:
            margin = 0.0
        rows.append({
            "condition_id": opp.condition_id,
            "yes_ask": opp.best_ask_yes, "no_ask": opp.best_ask_no,
            "yes_bid": opp.best_bid_yes, "no_bid": opp.best_bid_no,
            "direction": opp.direction,
            "net_margin_per_pair": round(margin, 6),
            "executable_pairs": opp.max_executable_pairs,
            "max_dollar_profit": round(margin * opp.max_executable_pairs, 4),
        })
    return {
        "opportunities_found": len(opportunities),
        "top_rows": rows,
    }


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Scan Polymarket binary markets for buy-both / sell-both arbitrage."
    )
    parser.add_argument("--base-url", default=CLOB_API_BASE)
    parser.add_argument("--max-markets", type=int, default=300)
    parser.add_argument("--taker-fee-bps-per-leg", type=float, default=10.0)
    parser.add_argument("--min-gross-profit", type=float, default=0.005)
    parser.add_argument("--min-size-per-leg", type=float, default=5.0)
    parser.add_argument("--max-rows", type=int, default=20)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    cfg = ScanConfig(
        base_url=args.base_url, max_markets=args.max_markets,
        taker_fee_bps_per_leg=args.taker_fee_bps_per_leg,
        min_gross_profit=args.min_gross_profit,
        min_size_per_leg=args.min_size_per_leg,
    )
    opportunities = asyncio.run(scan(cfg))
    summary = _summary(opportunities, max_rows=args.max_rows)
    if args.json:
        print(json.dumps(summary, indent=2, sort_keys=True))
    else:
        print(f"Found {summary['opportunities_found']} opportunities. Top {len(summary['top_rows'])}:")
        for row in summary["top_rows"]:
            print(
                f"  {row['direction']:10s}  margin/pair=${row['net_margin_per_pair']:+.4f}  "
                f"size={row['executable_pairs']:>9.2f}  total=${row['max_dollar_profit']:+.4f}  "
                f"yes={row['yes_ask']:.3f}/{row['yes_bid']:.3f}  no={row['no_ask']:.3f}/{row['no_bid']:.3f}"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

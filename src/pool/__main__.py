"""Command-line entrypoint for paper-only modular Binance pool evaluation."""

from __future__ import annotations

import argparse
import asyncio
import signal

from loguru import logger

from src.exchange.client import BinanceClient
from src.pool.config import PoolSettings
from src.pool.runtime import PaperPoolRuntime


async def run(*, once: bool = False) -> None:
    """Run one or repeated read-only/paper pool cycles."""
    settings = PoolSettings.from_env()
    client = BinanceClient(
        settings.api_key,
        settings.api_secret,
        testnet=settings.use_testnet,
    )
    runtime = PaperPoolRuntime(client, settings)
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for event in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(event, stop.set)
        except NotImplementedError:
            pass
    try:
        while not stop.is_set():
            report = await runtime.cycle()
            for portfolio_id, totals in report.benchmark_totals.items():
                logger.bind(portfolio_id=portfolio_id, totals=totals).info(
                    "Paper portfolio benchmark valuation"
                )
            if once:
                return
            try:
                await asyncio.wait_for(stop.wait(), timeout=settings.cycle_seconds)
            except asyncio.TimeoutError:
                pass
    finally:
        runtime.close()
        await client.close()


def main() -> None:
    parser = argparse.ArgumentParser(description="Run the Binance paper asset pool")
    parser.add_argument("--once", action="store_true", help="evaluate one cycle and exit")
    args = parser.parse_args()
    asyncio.run(run(once=args.once))


if __name__ == "__main__":
    main()

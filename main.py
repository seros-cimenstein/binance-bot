"""Repository-level entry point for the Binance trading bot.

The runtime implementation lives in :mod:`src.main`; this module keeps
``python main.py`` working while also exposing the async entrypoint for callers
that import ``main`` directly.
"""

from src.main import BotSettings, StrategyOrderClient, main, run, run_bot

__all__ = ["BotSettings", "StrategyOrderClient", "main", "run", "run_bot"]


if __name__ == "__main__":
    raise SystemExit(run())

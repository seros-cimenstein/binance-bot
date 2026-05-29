"""Read-only operational verification for a configured Binance bot runtime."""

from __future__ import annotations

import argparse
import asyncio
from collections.abc import Callable
from typing import Any

from src.config import Settings
from src.exchange.client import BinanceClient
from src.telegram_alerts import TelegramAlerter


class OperationsCheckError(RuntimeError):
    """Raised when an operational verification step fails."""


def _mode(settings: Settings) -> str:
    if settings.dry_run:
        return "DRY_RUN"
    if not settings.use_testnet:
        return "LIVE"
    return "testnet"


def _ensure_live_confirmation(settings: Settings) -> None:
    if not settings.use_testnet and settings.confirm_live_trading != "yes":
        raise OperationsCheckError(
            "live configuration requires CONFIRM_LIVE_TRADING=yes"
        )


def _default_client_factory(settings: Settings) -> BinanceClient:
    return BinanceClient(
        api_key=settings.api_key,
        api_secret=settings.api_secret,
        testnet=settings.use_testnet,
    )


async def run_checks(
    settings: Settings,
    *,
    telegram_test: bool = False,
    client_factory: Callable[[Settings], Any] = _default_client_factory,
    alerter_factory: Callable[[str, str], Any] = TelegramAlerter,
) -> list[str]:
    """Perform exchange checks and optionally confirm Telegram delivery."""
    _ensure_live_confirmation(settings)
    mode = _mode(settings)
    client = client_factory(settings)
    results: list[str] = []
    try:
        ticker = await client.fetch_ticker(settings.trading_symbol)
        last_price = ticker.get("last") or ticker.get("close")
        if last_price is None:
            raise OperationsCheckError("ticker response did not include a price")
        results.append(
            f"Exchange ticker OK: {settings.trading_symbol} last={last_price}"
        )

        await client.fetch_balance()
        results.append("Account access OK: balance request succeeded")

        if telegram_test:
            alerter = alerter_factory(
                settings.telegram_bot_token,
                settings.telegram_chat_id,
            )
            delivered = await alerter.alert_operations_test(
                mode,
                settings.trading_symbol,
            )
            if not delivered:
                raise OperationsCheckError("Telegram verification delivery failed")
            results.append("Telegram delivery OK: verification message sent")
    finally:
        await client.close()

    return [f"Mode: {mode}", *results]


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run read-only Binance bot operational checks."
    )
    parser.add_argument(
        "--telegram-test",
        action="store_true",
        help="send one operational verification message to the configured chat",
    )
    args = parser.parse_args(argv)
    try:
        results = asyncio.run(
            run_checks(
                Settings.from_env(),
                telegram_test=args.telegram_test,
            )
        )
    except Exception as exc:
        print(f"Operational check failed: {exc}")
        return 1

    for result in results:
        print(result)
    return 0


if __name__ == "__main__":
    raise SystemExit(run())

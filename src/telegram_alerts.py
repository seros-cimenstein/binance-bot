"""Telegram alerting for critical bot events."""

from __future__ import annotations

import aiohttp
from loguru import logger


_API_URL = "https://api.telegram.org/bot{token}/sendMessage"
_TIMEOUT = aiohttp.ClientTimeout(total=5)


class TelegramAlerter:
    """Send fire-and-forget alerts to a Telegram chat via the Bot API."""

    def __init__(self, bot_token: str, chat_id: str) -> None:
        self._token = bot_token
        self._chat_id = chat_id
        self._enabled = bool(bot_token and chat_id)
        if not self._enabled:
            logger.warning(
                "Telegram alerts disabled: set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID to enable"
            )

    async def send(self, message: str) -> bool:
        if not self._enabled:
            return False
        try:
            url = _API_URL.format(token=self._token)
            payload = {"chat_id": self._chat_id, "text": message, "parse_mode": "HTML"}
            async with aiohttp.ClientSession(timeout=_TIMEOUT) as session:
                async with session.post(url, json=payload) as resp:
                    if not resp.ok:
                        body = await resp.text()
                        logger.warning(f"Telegram alert failed ({resp.status}): {body[:200]}")
                        return False
            return True
        except Exception as exc:
            logger.warning(f"Telegram alert error: {exc}")
            return False

    async def alert_started(self, mode: str) -> None:
        emoji = "\U0001f534" if mode == "LIVE" else "\U0001f7e1"
        await self.send(f"{emoji} <b>Bot started</b> — mode: <code>{mode}</code>")

    async def alert_crash(self, attempt: int, max_attempts: int, delay: float, error: str) -> None:
        await self.send(
            f"\U0001f4a5 <b>Bot crashed</b> (attempt {attempt}/{max_attempts})\n"
            f"Restarting in {delay:.0f}s\n"
            f"<code>{error[:300]}</code>"
        )

    async def alert_terminal_failure(self, error: str) -> None:
        await self.send(
            "\U0001f6d1 <b>Bot stopped after repeated failures</b>\n"
            f"<code>{error[:300]}</code>"
        )

    async def alert_stream_failure(self, stream: str, reason: str) -> None:
        await self.send(
            "\u26a0\ufe0f <b>WebSocket stream failure</b>\n"
            f"Stream: <code>{stream[:100]}</code>\n"
            f"<code>{reason[:300]}</code>"
        )

    async def alert_operations_test(self, mode: str, symbol: str) -> bool:
        return await self.send(
            "\u2705 <b>Operational verification</b>\n"
            f"Mode: <code>{mode}</code>\n"
            f"Symbol: <code>{symbol}</code>\n"
            "Read-only connectivity check completed."
        )

    async def alert_daily_loss_limit(self, current: float, limit: float) -> None:
        await self.send(
            f"\U0001f6a8 <b>Daily loss limit hit</b>\n"
            f"Loss: <code>${current:.2f}</code> / Limit: <code>${limit:.2f}</code>\n"
            "No further orders will be placed today."
        )

    async def alert_risk_breach(self, reason: str) -> None:
        await self.send(f"⚠️ <b>Risk breach blocked an order</b>\n<code>{reason[:300]}</code>")

    async def alert_trade(
        self,
        side: str,
        symbol: str,
        amount: float,
        price: float | None,
        dry_run: bool = False,
    ) -> None:
        emoji = "\U0001f7e2" if side.lower() == "buy" else "\U0001f534"
        label = "BUY" if side.lower() == "buy" else "SELL"
        suffix = " <i>(dry run)</i>" if dry_run else ""
        price_str = f"${price:,.4f}" if price is not None else "market"
        await self.send(
            f"{emoji} <b>{label}</b>{suffix}\n"
            f"Symbol: <code>{symbol}</code>\n"
            f"Amount: <code>{amount:g}</code>\n"
            f"Price: <code>{price_str}</code>"
        )

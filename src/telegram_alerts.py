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

    async def send(self, message: str) -> None:
        if not self._enabled:
            return
        try:
            url = _API_URL.format(token=self._token)
            payload = {"chat_id": self._chat_id, "text": message, "parse_mode": "HTML"}
            async with aiohttp.ClientSession(timeout=_TIMEOUT) as session:
                async with session.post(url, json=payload) as resp:
                    if not resp.ok:
                        body = await resp.text()
                        logger.warning(f"Telegram alert failed ({resp.status}): {body[:200]}")
        except Exception as exc:
            logger.warning(f"Telegram alert error: {exc}")

    async def alert_started(self, mode: str) -> None:
        emoji = "\U0001f534" if mode == "LIVE" else "\U0001f7e1"
        await self.send(f"{emoji} <b>Bot started</b> — mode: <code>{mode}</code>")

    async def alert_crash(self, attempt: int, max_attempts: int, delay: float, error: str) -> None:
        await self.send(
            f"\U0001f4a5 <b>Bot crashed</b> (attempt {attempt}/{max_attempts})\n"
            f"Restarting in {delay:.0f}s\n"
            f"<code>{error[:300]}</code>"
        )

    async def alert_daily_loss_limit(self, current: float, limit: float) -> None:
        await self.send(
            f"\U0001f6a8 <b>Daily loss limit hit</b>\n"
            f"Loss: <code>${current:.2f}</code> / Limit: <code>${limit:.2f}</code>\n"
            "No further orders will be placed today."
        )

    async def alert_risk_breach(self, reason: str) -> None:
        await self.send(f"⚠️ <b>Risk breach blocked an order</b>\n<code>{reason[:300]}</code>")

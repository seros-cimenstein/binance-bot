"""Shared Telegram alert helper, used by every deployed bot.

Wraps ``src.telegram_alerts.TelegramAlerter`` with a fire-and-forget
contract: ``maybe_telegram`` never raises, never blocks the caller's
core logic. If env credentials are missing or Telegram is unreachable,
the call quietly returns False and the bot continues.
"""

from __future__ import annotations

import html as html_lib
import os

from src.telegram_alerts import TelegramAlerter


async def maybe_telegram(message: str, *, monospace: bool = False) -> bool:
    """Send a Telegram alert if credentials are configured. Never raises.

    When ``monospace`` is True the message is wrapped in HTML <pre> tags
    after escaping; useful for tabular/log-style content.
    """
    try:
        token = os.getenv("TELEGRAM_BOT_TOKEN", "")
        chat = os.getenv("TELEGRAM_CHAT_ID", "")
        if not token or not chat:
            return False
        if monospace:
            payload = "<pre>" + html_lib.escape(message) + "</pre>"
        else:
            # Non-monospace path still wants HTML safety, but lets us keep
            # the message readable in regular Telegram font.
            payload = html_lib.escape(message)
        return await TelegramAlerter(token, chat).send(payload)
    except Exception:  # noqa: BLE001 — alerts are never fatal
        return False

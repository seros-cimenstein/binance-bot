"""Telegram alerting for critical bot events."""

from __future__ import annotations

import json
import os
import time
from pathlib import Path

import aiohttp
from loguru import logger


_API_URL = "https://api.telegram.org/bot{token}/sendMessage"
_TIMEOUT = aiohttp.ClientTimeout(total=5)

# Persistent 429 cooldown state. Survives container restarts so the bot can't
# resume the retry storm after a crash-loop (see RestartCount 3,977 on Pi).
_COOLDOWN_PATH = Path("data/telegram-cooldown.json")
_DEFAULT_COOLDOWN_S = 60
_COOLDOWN_CAP_S = 24 * 60 * 60


def _read_cooldown_until_ms() -> int:
    try:
        data = json.loads(_COOLDOWN_PATH.read_text())
        return int(data.get("cooldown_until_ms", 0))
    except (FileNotFoundError, json.JSONDecodeError, ValueError, TypeError):
        return 0


def _write_cooldown(retry_after_s: int) -> None:
    retry_after_s = min(max(int(retry_after_s), 0), _COOLDOWN_CAP_S)
    now_ms = int(time.time() * 1000)
    payload = {
        "cooldown_until_ms": now_ms + retry_after_s * 1000,
        "last_429_at_ms": now_ms,
        "retry_after_observed_s": retry_after_s,
    }
    _COOLDOWN_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = _COOLDOWN_PATH.with_suffix(_COOLDOWN_PATH.suffix + ".tmp")
    tmp.write_text(json.dumps(payload))
    os.replace(tmp, _COOLDOWN_PATH)


def is_cooled_down() -> bool:
    return int(time.time() * 1000) < _read_cooldown_until_ms()


def _parse_retry_after(body: str) -> int:
    try:
        data = json.loads(body)
        ra = data.get("parameters", {}).get("retry_after")
        if isinstance(ra, int) and ra > 0:
            return ra
    except (json.JSONDecodeError, AttributeError):
        pass
    return _DEFAULT_COOLDOWN_S


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
        if is_cooled_down():
            logger.debug("Telegram alert suppressed (429 cooldown active)")
            return False
        try:
            url = _API_URL.format(token=self._token)
            payload = {"chat_id": self._chat_id, "text": message, "parse_mode": "HTML"}
            async with aiohttp.ClientSession(timeout=_TIMEOUT) as session:
                async with session.post(url, json=payload) as resp:
                    if not resp.ok:
                        body = await resp.text()
                        if resp.status == 429:
                            retry_after_s = _parse_retry_after(body)
                            _write_cooldown(retry_after_s)
                            logger.warning(
                                f"Telegram 429 — cooling down {retry_after_s}s "
                                f"(persisted to {_COOLDOWN_PATH})"
                            )
                        else:
                            logger.warning(
                                f"Telegram alert failed ({resp.status}): {body[:200]}"
                            )
                        return False
            return True
        except Exception as exc:
            logger.warning(f"Telegram alert error: {exc}")
            return False

    async def check_for_go_off_command(self) -> bool:
        from pathlib import Path

        flag_path = Path("data/go_off.flag")
        if not self._enabled:
            return flag_path.exists()

        offset_path = Path("data/telegram_offset.txt")
        offset = 0
        if offset_path.exists():
            try:
                offset = int(offset_path.read_text().strip())
            except ValueError:
                pass

        try:
            url = f"https://api.telegram.org/bot{self._token}/getUpdates"
            params = {"timeout": "0", "offset": str(offset), "allowed_updates": '["message"]'}
            async with aiohttp.ClientSession(timeout=_TIMEOUT) as session:
                async with session.get(url, params=params) as resp:
                    if resp.ok:
                        data = await resp.json()
                        updates = data.get("result", [])
                        max_update_id = offset - 1 if offset > 0 else 0

                        for update in updates:
                            update_id = update.get("update_id", 0)
                            max_update_id = max(max_update_id, update_id)

                            message = update.get("message", {})
                            chat = message.get("chat", {})

                            if str(chat.get("id")) != str(self._chat_id):
                                continue

                            text = str(message.get("text", "")).strip()
                            if text == "/go_off":
                                flag_path.touch(exist_ok=True)
                                await self.send("🚀 <b>GO OFF MODE INITIATED</b>\nLoss streaks and daily loss limits will be ignored.")
                            elif text == "/go_on":
                                if flag_path.exists():
                                    flag_path.unlink()
                                await self.send("🛡️ <b>NORMAL MODE RESTORED</b>\nAll safety limits are back in effect.")

                        if max_update_id >= offset:
                            offset_path.write_text(str(max_update_id + 1))
        except Exception as exc:
            logger.warning(f"Failed to check Telegram commands: {exc}")

        return flag_path.exists()

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

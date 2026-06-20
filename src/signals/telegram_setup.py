"""Interactive first-time Telegram user-account authentication helper."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path


def run() -> int:
    api_id = os.getenv("TELEGRAM_API_ID", "")
    api_hash = os.getenv("TELEGRAM_API_HASH", "")
    session_path = os.getenv("TELEGRAM_SESSION_PATH", "data/telegram-session.session")

    if not api_id or not api_hash:
        print(
            "TELEGRAM_API_ID and TELEGRAM_API_HASH are required. "
            "Create them at https://my.telegram.org/apps and export them first."
        )
        return 2
    try:
        api_id_int = int(api_id)
    except ValueError:
        print("TELEGRAM_API_ID must be an integer.")
        return 2

    try:
        from telethon import TelegramClient
    except ImportError:
        print("telethon is not installed. Install requirements.txt, then retry.")
        return 2

    async def _main() -> int:
        Path(session_path).parent.mkdir(parents=True, exist_ok=True)
        client = TelegramClient(session_path, api_id_int, api_hash)
        try:
            await client.start()
        except Exception as exc:  # noqa: BLE001 - interactive helper reports failures
            print(f"Telegram authentication failed: {exc}")
            print("Retry with: python -m src.signals.telegram_setup")
            return 1
        finally:
            await client.disconnect()

        if not Path(session_path).exists():
            print(
                f"Telegram auth completed, but session file was not found at {session_path}."
            )
            print("Check TELEGRAM_SESSION_PATH and retry.")
            return 1

        print(f"Telegram session saved: {session_path}")
        print("You can now start: python -m src.signals.telegram_ingester --daemon")
        return 0

    return asyncio.run(_main())


if __name__ == "__main__":
    raise SystemExit(run())

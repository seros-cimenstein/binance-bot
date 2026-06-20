"""Tests for the Telegram signal ingester."""

from __future__ import annotations

import json
from collections import OrderedDict
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock

import pytest

from src.signals import dexscreener as dex_mod
from src.signals import telegram_ingester as tg_mod
from src.signals.common import AlertRateLimiter


EVM_ADDRESS = "0x1234567890abcdef1234567890ABCDEF12345678"
SOLANA_ADDRESS = "9xQeWvG816bUx9EPfT8GK8uE2LJQku3Z34Y3tjnT4W"
SOLANA_ADDRESS_2 = "So11111111111111111111111111111111111111112"


def _settings(tmp_path: Path, **overrides: Any) -> tg_mod.TelegramSettings:
    values = {
        "api_id": "12345",
        "api_hash": "hash",
        "session_path": str(tmp_path / "telegram-session.session"),
        "channels_path": str(tmp_path / "telegram-channels.json"),
        "max_alerts_per_hour": 30,
        "stop_path": str(tmp_path / "stop"),
        "ledger_path": str(tmp_path / "ledger.jsonl"),
        "watchlist_path": str(tmp_path / "watchlist.json"),
        "watchlist_ttl_hours": 12.0,
    }
    values.update(overrides)
    return tg_mod.TelegramSettings(**values)


def test_extract_evm_address() -> None:
    detections = tg_mod.extract_contract_addresses(f"fresh pair {EVM_ADDRESS}")
    assert detections == [
        tg_mod.DetectedAddress(chain="ethereum", address=EVM_ADDRESS),
    ]


def test_extract_solana_address() -> None:
    detections = tg_mod.extract_contract_addresses(f"sol ca {SOLANA_ADDRESS}")
    assert detections == [
        tg_mod.DetectedAddress(chain="solana", address=SOLANA_ADDRESS),
    ]


@pytest.mark.parametrize(
    "text",
    [
        "short Abc123",
        "abcdefghijklmnopqrstuvwxyzabcdefghjkmnpqrstuv",
        "thisisaverylonglowercasestringwithoutdigits",
    ],
)
def test_extract_solana_ignores_common_false_positives(text: str) -> None:
    assert tg_mod.extract_contract_addresses(text) == []


def test_extract_mixed_message_preserves_order_and_uniques() -> None:
    detections = tg_mod.extract_contract_addresses(
        f"{SOLANA_ADDRESS} then {EVM_ADDRESS} then {SOLANA_ADDRESS_2} "
        f"and duplicate {EVM_ADDRESS}"
    )
    assert detections == [
        tg_mod.DetectedAddress(chain="solana", address=SOLANA_ADDRESS),
        tg_mod.DetectedAddress(chain="ethereum", address=EVM_ADDRESS),
        tg_mod.DetectedAddress(chain="solana", address=SOLANA_ADDRESS_2),
    ]


def test_load_channel_list_writes_defaults_when_missing(tmp_path: Path) -> None:
    path = tmp_path / "channels.json"
    channels = tg_mod.load_channel_list(path)
    assert path.exists()
    assert [channel.username for channel in channels.channels] == [
        "binance_announcements",
        "WatcherGuru",
        "coingecko",
    ]
    assert json.loads(path.read_text(encoding="utf-8"))["channels"][0]["weight"] == 0.4


def test_load_channel_list_reads_existing_and_ignores_malformed(tmp_path: Path) -> None:
    path = tmp_path / "channels.json"
    path.write_text(
        json.dumps({
            "channels": [
                {"username": "@AlphaRoom", "weight": 0.9},
                {"username": "", "weight": 0.5},
                {"username": "BadWeight", "weight": "high"},
                {"username": "TooHigh", "weight": 1.1},
                {"username": "BoolWeight", "weight": True},
                "not an object",
            ],
        }),
        encoding="utf-8",
    )
    channels = tg_mod.load_channel_list(path)
    assert channels.channels == (
        tg_mod.TelegramChannel(username="AlphaRoom", weight=0.9),
    )
    assert channels.weight_for("missing") == 0.5
    assert channels.weight_for("@alpharoom") == 0.9


def test_load_channel_list_malformed_json_returns_empty(tmp_path: Path) -> None:
    path = tmp_path / "channels.json"
    path.write_text("{", encoding="utf-8")
    assert tg_mod.load_channel_list(path).channels == ()


def test_dedup_window_per_channel_address_pair() -> None:
    seen: OrderedDict[tuple[str, str], int] = OrderedDict()
    assert tg_mod.dedup_allows(
        seen,
        channel_id="1",
        address=EVM_ADDRESS,
        now_ms=1_000_000,
    )
    assert not tg_mod.dedup_allows(
        seen,
        channel_id="1",
        address=EVM_ADDRESS,
        now_ms=1_000_100,
    )
    assert tg_mod.dedup_allows(
        seen,
        channel_id="2",
        address=EVM_ADDRESS,
        now_ms=1_000_200,
    )
    assert tg_mod.dedup_allows(
        seen,
        channel_id="1",
        address=EVM_ADDRESS,
        now_ms=1_000_000 + 3_600_000,
    )


def test_alert_rate_limiter_shared_with_dexscreener() -> None:
    limiter = AlertRateLimiter(2)
    assert dex_mod.AlertRateLimiter is AlertRateLimiter
    assert limiter.allow(10_000)
    assert limiter.allow(20_000)
    assert not limiter.allow(30_000)
    assert limiter.allow(10_000 + 3_600_001)


@pytest.mark.asyncio
async def test_process_new_message_writes_ledger_watchlist_and_alerts(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    channels = tg_mod.TelegramChannelList((
        tg_mod.TelegramChannel(username="WatcherGuru", weight=0.3),
    ))
    event = SimpleNamespace(
        id=77,
        chat_id=123,
        chat=SimpleNamespace(username="WatcherGuru", id=123),
        message=SimpleNamespace(
            id=77,
            text=f"new contract {EVM_ADDRESS}",
        ),
    )
    maybe_telegram = AsyncMock(return_value=True)
    monkeypatch.setattr(tg_mod, "utc_ms", lambda: 2_000_000)
    monkeypatch.setattr(tg_mod, "maybe_telegram", maybe_telegram)

    candidates = await tg_mod.process_new_message(
        event,
        settings=settings,
        channels=channels,
        seen=OrderedDict(),
        watchlist=[],
        alert_limiter=AlertRateLimiter(30),
    )

    assert len(candidates) == 1
    assert candidates[0].source == "telegram"
    assert candidates[0].confidence == 0.3
    assert candidates[0].raw_signal == {
        "channel_username": "WatcherGuru",
        "channel_id": "123",
        "message_id": "77",
        "message_text": f"new contract {EVM_ADDRESS}",
        "message_url": "https://t.me/WatcherGuru/77",
    }
    ledger_rows = [
        json.loads(line)
        for line in Path(settings.ledger_path).read_text(encoding="utf-8").splitlines()
    ]
    assert ledger_rows[0]["source"] == "telegram"
    assert ledger_rows[0]["token_address"] == EVM_ADDRESS
    watchlist = json.loads(Path(settings.watchlist_path).read_text(encoding="utf-8"))
    assert watchlist["watching"][0]["token"] == EVM_ADDRESS
    assert watchlist["watching"][0]["source"] == "telegram"
    maybe_telegram.assert_awaited_once()


@pytest.mark.asyncio
async def test_process_new_message_respects_rate_limit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    channels = tg_mod.TelegramChannelList((
        tg_mod.TelegramChannel(username="coingecko", weight=0.2),
    ))
    event = SimpleNamespace(
        id=5,
        chat_id=9,
        chat=SimpleNamespace(username="coingecko", id=9),
        message=SimpleNamespace(text=f"{EVM_ADDRESS} {SOLANA_ADDRESS}"),
    )
    maybe_telegram = AsyncMock(return_value=True)
    monkeypatch.setattr(tg_mod, "utc_ms", lambda: 2_000_000)
    monkeypatch.setattr(tg_mod, "maybe_telegram", maybe_telegram)

    candidates = await tg_mod.process_new_message(
        event,
        settings=settings,
        channels=channels,
        seen=OrderedDict(),
        watchlist=[],
        alert_limiter=AlertRateLimiter(1),
    )

    assert len(candidates) == 2
    maybe_telegram.assert_awaited_once()


def test_run_missing_session_exits_2_with_clear_error(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    logs: list[str] = []
    monkeypatch.setenv("TELEGRAM_API_ID", "12345")
    monkeypatch.setenv("TELEGRAM_API_HASH", "hash")
    monkeypatch.setenv("TELEGRAM_SESSION_PATH", str(tmp_path / "missing.session"))
    monkeypatch.setenv("SIGNALS_TELEGRAM_CHANNELS_PATH", str(tmp_path / "channels.json"))
    monkeypatch.setenv("SIGNALS_LEDGER_PATH", str(tmp_path / "ledger.jsonl"))
    monkeypatch.setenv("SIGNALS_WATCHLIST_PATH", str(tmp_path / "watchlist.json"))
    monkeypatch.setattr(tg_mod.logger, "error", logs.append)
    monkeypatch.setattr(
        tg_mod,
        "_load_telethon",
        lambda: (_raise_assertion("Telethon should not be loaded"), None),
    )

    assert tg_mod.run(["--once"]) == 2
    assert "telegram_setup" in logs[0]
    assert "session file missing" in logs[0].lower()


def _raise_assertion(message: str) -> None:
    raise AssertionError(message)

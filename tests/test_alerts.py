"""Tests for the shared maybe_telegram helper."""

from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from src import alerts, telegram_alerts


class _Recorder:
    def __init__(self, ok: bool = True, raise_exc: Exception | None = None) -> None:
        self.ok = ok
        self.raise_exc = raise_exc
        self.calls: list[str] = []

    async def send(self, message: str) -> bool:
        if self.raise_exc:
            raise self.raise_exc
        self.calls.append(message)
        return self.ok


@pytest.mark.asyncio
async def test_returns_false_when_credentials_missing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TELEGRAM_BOT_TOKEN", raising=False)
    monkeypatch.delenv("TELEGRAM_CHAT_ID", raising=False)
    assert await alerts.maybe_telegram("hi") is False


@pytest.mark.asyncio
async def test_calls_send_when_credentials_present(monkeypatch: pytest.MonkeyPatch) -> None:
    rec = _Recorder(ok=True)
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", "t"); monkeypatch.setenv("TELEGRAM_CHAT_ID", "c")
    monkeypatch.setattr(alerts, "TelegramAlerter", lambda *a, **kw: rec)
    sent = await alerts.maybe_telegram("hello & <world>")
    assert sent is True
    # HTML-escaped
    assert "&amp;" in rec.calls[0] and "&lt;world&gt;" in rec.calls[0]


@pytest.mark.asyncio
async def test_monospace_mode_wraps_in_pre(monkeypatch: pytest.MonkeyPatch) -> None:
    rec = _Recorder(ok=True)
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", "t"); monkeypatch.setenv("TELEGRAM_CHAT_ID", "c")
    monkeypatch.setattr(alerts, "TelegramAlerter", lambda *a, **kw: rec)
    sent = await alerts.maybe_telegram("a < b", monospace=True)
    assert sent is True
    assert rec.calls[0].startswith("<pre>") and rec.calls[0].endswith("</pre>")
    assert "a &lt; b" in rec.calls[0]


@pytest.mark.asyncio
async def test_never_raises_on_send_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    rec = _Recorder(raise_exc=RuntimeError("network down"))
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", "t"); monkeypatch.setenv("TELEGRAM_CHAT_ID", "c")
    monkeypatch.setattr(alerts, "TelegramAlerter", lambda *a, **kw: rec)
    # Should not raise; should return False.
    assert await alerts.maybe_telegram("oops") is False


# ---------------------------------------------------------------------------
# 429 cooldown
# ---------------------------------------------------------------------------


@pytest.fixture
def _isolated_cooldown(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Path:
    path = tmp_path / "telegram-cooldown.json"
    monkeypatch.setattr(telegram_alerts, "_COOLDOWN_PATH", path)
    return path


def test_parse_retry_after_valid() -> None:
    body = json.dumps({"ok": False, "error_code": 429, "parameters": {"retry_after": 42}})
    assert telegram_alerts._parse_retry_after(body) == 42


def test_parse_retry_after_missing_falls_back() -> None:
    body = json.dumps({"ok": False, "error_code": 429})
    assert telegram_alerts._parse_retry_after(body) == telegram_alerts._DEFAULT_COOLDOWN_S


def test_parse_retry_after_invalid_json_falls_back() -> None:
    assert telegram_alerts._parse_retry_after("not json") == telegram_alerts._DEFAULT_COOLDOWN_S


def test_write_then_is_cooled_down(_isolated_cooldown: Path) -> None:
    assert telegram_alerts.is_cooled_down() is False
    telegram_alerts._write_cooldown(60)
    assert _isolated_cooldown.exists()
    payload = json.loads(_isolated_cooldown.read_text())
    assert payload["retry_after_observed_s"] == 60
    assert payload["cooldown_until_ms"] > int(time.time() * 1000)
    assert telegram_alerts.is_cooled_down() is True


def test_cooldown_clears_after_expiry(_isolated_cooldown: Path) -> None:
    telegram_alerts._write_cooldown(60)
    # Forge a past expiry directly.
    past_ms = int(time.time() * 1000) - 1000
    _isolated_cooldown.write_text(json.dumps({"cooldown_until_ms": past_ms}))
    assert telegram_alerts.is_cooled_down() is False


def test_cooldown_clamps_at_24h(_isolated_cooldown: Path) -> None:
    telegram_alerts._write_cooldown(10_000_000)
    payload = json.loads(_isolated_cooldown.read_text())
    assert payload["retry_after_observed_s"] == telegram_alerts._COOLDOWN_CAP_S


def test_is_cooled_down_missing_file_is_false(_isolated_cooldown: Path) -> None:
    assert not _isolated_cooldown.exists()
    assert telegram_alerts.is_cooled_down() is False


def test_is_cooled_down_corrupt_file_is_false(_isolated_cooldown: Path) -> None:
    _isolated_cooldown.write_text("{not json")
    assert telegram_alerts.is_cooled_down() is False


@pytest.mark.asyncio
async def test_maybe_telegram_short_circuits_during_cooldown(
    _isolated_cooldown: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rec = _Recorder(ok=True)
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", "t"); monkeypatch.setenv("TELEGRAM_CHAT_ID", "c")
    monkeypatch.setattr(alerts, "TelegramAlerter", lambda *a, **kw: rec)
    telegram_alerts._write_cooldown(60)
    sent = await alerts.maybe_telegram("hello")
    assert sent is False
    assert rec.calls == []  # send() must NOT be invoked while cooled down

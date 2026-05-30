"""Tests for the shared maybe_telegram helper."""

from __future__ import annotations

import pytest

from src import alerts


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

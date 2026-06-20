"""Tests for smart-money wallet bootstrap."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from src.signals import wallet_bootstrap as boot_mod


class DummyResponse:
    def __init__(self, payload: Any, *, status_error: Exception | None = None) -> None:
        self.payload = payload
        self.status_error = status_error

    async def __aenter__(self) -> "DummyResponse":
        return self

    async def __aexit__(self, *_args: Any) -> None:
        return None

    def raise_for_status(self) -> None:
        if self.status_error is not None:
            raise self.status_error

    async def json(self) -> Any:
        return self.payload


class DummySession:
    def __init__(self, response: DummyResponse | None = None) -> None:
        self.response = response
        self.get_calls: list[dict[str, Any]] = []

    def get(self, url: str, *, headers: dict[str, str] | None = None) -> DummyResponse:
        self.get_calls.append({"url": url, "headers": headers})
        if self.response is None:
            raise AssertionError("unexpected GET")
        return self.response


def test_load_wallets_returns_empty_on_missing_or_malformed(tmp_path: Path) -> None:
    assert boot_mod.load_wallets(tmp_path / "missing.json") == []
    path = tmp_path / "wallets.json"
    path.write_text("{", encoding="utf-8")
    assert boot_mod.load_wallets(path) == []


@pytest.mark.asyncio
async def test_refresh_does_nothing_when_fresh(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "wallets.json"
    monkeypatch.setattr(boot_mod, "utc_ms", lambda: 2_000_000)
    payload = {
        "wallets": [{"pubkey": "Manual111", "source": "manual"}],
        "last_refreshed_ms": 1_999_000,
    }
    path.write_text(json.dumps(payload), encoding="utf-8")

    wallets = await boot_mod.refresh_wallets_if_stale(
        path,
        session=DummySession(),
        max_age_hours=1,
        birdeye_api_key="key",
    )

    assert wallets == payload["wallets"]
    assert json.loads(path.read_text(encoding="utf-8")) == payload


@pytest.mark.asyncio
async def test_refresh_calls_birdeye_when_stale_and_key_set(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "wallets.json"
    monkeypatch.setattr(boot_mod, "utc_ms", lambda: 10_000_000)
    session = DummySession(DummyResponse({"data": [{"walletAddress": "Bird111"}]}))

    wallets = await boot_mod.refresh_wallets_if_stale(
        path,
        session=session,
        max_age_hours=1,
        birdeye_api_key="key",
    )

    assert wallets == [{
        "pubkey": "Bird111",
        "label": "birdeye-top-7d-#1",
        "source": "birdeye",
        "weight": 0.6,
    }]
    assert session.get_calls[0]["headers"] == {"X-API-KEY": "key"}


@pytest.mark.asyncio
async def test_refresh_preserves_manual_entries_across_birdeye_refresh(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "wallets.json"
    monkeypatch.setattr(boot_mod, "utc_ms", lambda: 10_000_000)
    path.write_text(
        json.dumps({
            "wallets": [
                {"pubkey": "Manual111", "label": "curated", "source": "manual"},
                {"pubkey": "OldBird", "source": "birdeye"},
            ],
            "last_refreshed_ms": 1,
        }),
        encoding="utf-8",
    )
    session = DummySession(DummyResponse({"data": {"items": [{"owner": "Bird222"}]}}))

    wallets = await boot_mod.refresh_wallets_if_stale(
        path,
        session=session,
        max_age_hours=1,
        birdeye_api_key="key",
    )

    assert wallets[0]["pubkey"] == "Manual111"
    assert wallets[0]["source"] == "manual"
    assert wallets[1]["pubkey"] == "Bird222"
    assert [wallet["pubkey"] for wallet in wallets] == ["Manual111", "Bird222"]


@pytest.mark.asyncio
async def test_refresh_without_key_on_stale_empty_file_writes_warning_shape(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "wallets.json"
    warnings: list[str] = []
    monkeypatch.setattr(boot_mod, "utc_ms", lambda: 10_000_000)
    monkeypatch.setattr(boot_mod.logger, "warning", warnings.append)

    wallets = await boot_mod.refresh_wallets_if_stale(
        path,
        session=DummySession(),
        max_age_hours=1,
        birdeye_api_key=None,
    )

    payload = json.loads(path.read_text(encoding="utf-8"))
    assert wallets == []
    assert payload["wallets"] == []
    assert payload["last_refreshed_ms"] == 10_000_000
    assert "BIRDEYE_API_KEY" in payload["_note"]
    assert "BIRDEYE_API_KEY" in warnings[0]


@pytest.mark.asyncio
async def test_birdeye_http_error_logs_and_keeps_existing_list(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "wallets.json"
    warnings: list[str] = []
    payload = {
        "wallets": [{"pubkey": "Existing111", "source": "manual"}],
        "last_refreshed_ms": 1,
    }
    path.write_text(json.dumps(payload), encoding="utf-8")
    monkeypatch.setattr(boot_mod, "utc_ms", lambda: 10_000_000)
    monkeypatch.setattr(boot_mod.logger, "warning", warnings.append)
    session = DummySession(DummyResponse({}, status_error=RuntimeError("500")))

    wallets = await boot_mod.refresh_wallets_if_stale(
        path,
        session=session,
        max_age_hours=1,
        birdeye_api_key="key",
    )

    assert wallets == payload["wallets"]
    assert json.loads(path.read_text(encoding="utf-8")) == payload
    assert "Birdeye" in warnings[0]

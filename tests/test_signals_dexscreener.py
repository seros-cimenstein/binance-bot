"""Tests for the DexScreener signal detector."""

from __future__ import annotations

import json
from collections import OrderedDict
from pathlib import Path
from typing import Any

import pytest

from src.signals import dexscreener as dex_mod


class DummySession:
    pass


def _settings(tmp_path: Path, **overrides: Any) -> dex_mod.DexScreenerSettings:
    values = {
        "poll_interval_seconds": 0.0,
        "max_age_hours": 24.0,
        "min_liquidity_usd": 5_000.0,
        "min_volume_usd_24h": 10_000.0,
        "ledger_path": str(tmp_path / "ledger.jsonl"),
        "watchlist_path": str(tmp_path / "watchlist.json"),
        "watchlist_ttl_hours": 12.0,
        "max_alerts_per_hour": 20,
        "stop_path": str(tmp_path / "stop"),
    }
    values.update(overrides)
    return dex_mod.DexScreenerSettings(**values)


def _pair(
    token: str = "Token111",
    *,
    created_at: int = 1_999_999,
    liquidity: float = 25_000.0,
    volume: float = 50_000.0,
) -> dict[str, Any]:
    return {
        "chainId": "solana",
        "dexId": "raydium",
        "url": "https://dexscreener.com/solana/pair",
        "pairCreatedAt": created_at,
        "liquidity": {"usd": liquidity},
        "volume": {"h24": volume},
        "priceChange": {"h24": 5.0},
        "baseToken": {"address": token, "symbol": "TOK", "name": "Token"},
        "quoteToken": {"address": "So111", "symbol": "SOL"},
    }


@pytest.mark.asyncio
async def test_full_cycle_emits_ledger_and_watchlist(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    monkeypatch.setattr(dex_mod, "utc_ms", lambda: 2_000_000)
    alerts: list[str] = []

    async def _profiles(_session: Any) -> list[dict[str, Any]]:
        return [{"tokenAddress": "Token111", "chainId": "solana"}]

    async def _pairs(_session: Any, token_address: str) -> list[dict[str, Any]]:
        assert token_address == "Token111"
        return [_pair()]

    async def _alert(msg: str) -> bool:
        alerts.append(msg)
        return True

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles)
    monkeypatch.setattr(dex_mod, "fetch_token_pairs", _pairs)
    monkeypatch.setattr(dex_mod, "maybe_telegram", _alert)

    candidates = await dex_mod.run_cycle(
        session=DummySession(),
        settings=settings,
        seen=OrderedDict(),
        watchlist=[],
        alert_limiter=dex_mod.AlertRateLimiter(20),
    )

    assert len(candidates) == 1
    assert candidates[0].source == "dexscreener"
    assert candidates[0].token_address == "Token111"
    ledger_rows = [
        json.loads(line)
        for line in Path(settings.ledger_path).read_text(encoding="utf-8").splitlines()
    ]
    assert ledger_rows[0]["raw_signal"]["dexscreener_pair_url"].startswith(
        "https://dexscreener.com"
    )
    watchlist = json.loads(Path(settings.watchlist_path).read_text(encoding="utf-8"))
    assert watchlist["watching"][0]["token"] == "Token111"
    assert watchlist["watching"][0]["source"] == "dexscreener"
    assert len(alerts) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "pair",
    [
        _pair(created_at=-84_400_001),
        _pair(liquidity=4_999.0),
        _pair(volume=9_999.0),
    ],
)
async def test_skip_cycles_do_not_emit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    pair: dict[str, Any],
) -> None:
    settings = _settings(tmp_path)
    monkeypatch.setattr(dex_mod, "utc_ms", lambda: 2_000_000)

    async def _profiles(_session: Any) -> list[dict[str, Any]]:
        return [{"tokenAddress": "Token111"}]

    async def _pairs(_session: Any, _token_address: str) -> list[dict[str, Any]]:
        return [pair]

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles)
    monkeypatch.setattr(dex_mod, "fetch_token_pairs", _pairs)
    candidates = await dex_mod.run_cycle(
        session=DummySession(),
        settings=settings,
        seen=OrderedDict(),
        watchlist=[],
        alert_limiter=dex_mod.AlertRateLimiter(20),
    )
    assert candidates == []
    assert not Path(settings.ledger_path).exists()
    assert json.loads(Path(settings.watchlist_path).read_text(encoding="utf-8")) == {
        "watching": []
    }


@pytest.mark.asyncio
async def test_dedupe_across_cycles(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    seen: OrderedDict[str, None] = OrderedDict()
    watchlist: list[dict[str, str]] = []
    calls = {"pairs": 0}
    monkeypatch.setattr(dex_mod, "utc_ms", lambda: 2_000_000)

    async def _profiles(_session: Any) -> list[dict[str, Any]]:
        return [{"tokenAddress": "Token111"}]

    async def _pairs(_session: Any, _token_address: str) -> list[dict[str, Any]]:
        calls["pairs"] += 1
        return [_pair()]

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles)
    monkeypatch.setattr(dex_mod, "fetch_token_pairs", _pairs)
    monkeypatch.setattr(dex_mod, "maybe_telegram", _noop_alert)

    first = await dex_mod.run_cycle(
        session=DummySession(),
        settings=settings,
        seen=seen,
        watchlist=watchlist,
        alert_limiter=dex_mod.AlertRateLimiter(20),
    )
    second = await dex_mod.run_cycle(
        session=DummySession(),
        settings=settings,
        seen=seen,
        watchlist=watchlist,
        alert_limiter=dex_mod.AlertRateLimiter(20),
    )
    assert len(first) == 1
    assert second == []
    assert calls["pairs"] == 1


def test_once_exits_cleanly(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SIGNALS_LEDGER_PATH", str(tmp_path / "ledger.jsonl"))
    monkeypatch.setenv("SIGNALS_WATCHLIST_PATH", str(tmp_path / "watchlist.json"))
    monkeypatch.setenv("SIGNALS_DEXSCREENER_STOP_PATH", str(tmp_path / "stop"))
    monkeypatch.setattr(dex_mod, "utc_ms", lambda: 2_000_000)

    async def _profiles(_session: Any) -> list[dict[str, Any]]:
        return []

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles)
    assert dex_mod.run(["--once"]) == 0
    assert json.loads((tmp_path / "watchlist.json").read_text(encoding="utf-8")) == {
        "watching": []
    }


@pytest.mark.asyncio
async def test_alert_rate_limit_suppresses_21st_alert(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    profiles = [{"tokenAddress": f"Token{i}"} for i in range(21)]
    alerts: list[str] = []
    monkeypatch.setattr(dex_mod, "utc_ms", lambda: 2_000_000)

    async def _profiles(_session: Any) -> list[dict[str, Any]]:
        return profiles

    async def _pairs(_session: Any, token_address: str) -> list[dict[str, Any]]:
        return [_pair(token=token_address)]

    async def _alert(msg: str) -> bool:
        alerts.append(msg)
        return True

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles)
    monkeypatch.setattr(dex_mod, "fetch_token_pairs", _pairs)
    monkeypatch.setattr(dex_mod, "maybe_telegram", _alert)
    candidates = await dex_mod.run_cycle(
        session=DummySession(),
        settings=settings,
        seen=OrderedDict(),
        watchlist=[],
        alert_limiter=dex_mod.AlertRateLimiter(20),
    )
    assert len(candidates) == 21
    assert len(alerts) == 20


@pytest.mark.asyncio
async def test_stop_file_presence_exits_cleanly(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    Path(settings.stop_path).touch()

    async def _profiles(_session: Any) -> list[dict[str, Any]]:
        raise AssertionError("profiles should not be fetched")

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles)
    await dex_mod.run_detector(settings=settings, once=True)


@pytest.mark.asyncio
async def test_malformed_empty_api_responses_do_not_crash(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    settings = _settings(tmp_path)
    seen: OrderedDict[str, None] = OrderedDict()
    watchlist: list[dict[str, str]] = []

    async def _profiles_bad(_session: Any) -> Any:
        return {"not": "a list"}

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles_bad)
    candidates = await dex_mod.run_cycle(
        session=DummySession(),
        settings=settings,
        seen=seen,
        watchlist=watchlist,
        alert_limiter=dex_mod.AlertRateLimiter(20),
    )
    assert candidates == []

    async def _profiles(_session: Any) -> list[dict[str, Any]]:
        return [{"tokenAddress": "Token111"}]

    async def _pairs_bad(_session: Any, _token_address: str) -> Any:
        return {}

    monkeypatch.setattr(dex_mod, "fetch_token_profiles", _profiles)
    monkeypatch.setattr(dex_mod, "fetch_token_pairs", _pairs_bad)
    candidates = await dex_mod.run_cycle(
        session=DummySession(),
        settings=settings,
        seen=seen,
        watchlist=watchlist,
        alert_limiter=dex_mod.AlertRateLimiter(20),
    )
    assert candidates == []


async def _noop_alert(_msg: str) -> bool:
    return True

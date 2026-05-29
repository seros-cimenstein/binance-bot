from __future__ import annotations

import pytest

import src.scope_audit as scope_audit
from src.config import Settings
from src.scope_audit import (
    ScopeAuditError,
    build_scope_audit_report,
    build_scope_items,
    ensure_scope_allowed,
    render_scope_report,
)


def _settings(
    *,
    symbols: tuple[str, ...] = ("BTC/USDT",),
    symbol_whitelist: tuple[str, ...] = ("BTC/USDT", "BNB/USDT"),
    asset_whitelist: tuple[str, ...] = ("BNB", "USDT"),
) -> Settings:
    return Settings(
        api_key="key",
        api_secret="secret",
        trading_symbols=symbols,
        api_symbol_whitelist=symbol_whitelist,
        api_asset_whitelist=asset_whitelist,
    )


def test_scope_audit_allows_configured_symbols_and_assets() -> None:
    settings = _settings()

    report = build_scope_audit_report(
        settings,
        canary_symbols=("bnbusdt",),
        transfer_assets=("usdt",),
    )

    assert report.allowed is True
    assert report.live_ready is True
    assert {check.item.value for check in report.checks} == {
        "BTC/USDT",
        "BNB/USDT",
        "USDT",
    }
    assert "Live scope ready: yes" in render_scope_report(report)


def test_scope_audit_blocks_missing_whitelist_for_live_scope() -> None:
    settings = _settings(symbol_whitelist=(), asset_whitelist=())

    report = build_scope_audit_report(settings, transfer_assets=("BNB",))

    assert report.allowed is False
    assert report.live_ready is False
    assert "no local API_SYMBOL_WHITELIST configured" in render_scope_report(report)
    with pytest.raises(ScopeAuditError, match="API scope audit failed"):
        ensure_scope_allowed(report)


def test_scope_audit_blocks_symbols_and_assets_outside_local_scope() -> None:
    settings = _settings(symbol_whitelist=("BTC/USDT",), asset_whitelist=("BNB",))

    report = build_scope_audit_report(
        settings,
        futures_symbols=("ETH/USDT",),
        withdrawal_assets=("USDT",),
    )

    blocked = {check.item.value: check.reason for check in report.blocked_checks}
    assert blocked["ETH/USDT"] == "symbol is not in local whitelist"
    assert blocked["USDT"] == "asset is not in local whitelist"


def test_scope_items_cover_pool_canary_margin_futures_transfer_withdrawal_prediction() -> None:
    items = build_scope_items(
        _settings(symbols=("BTC/USDT", "ETH/USDT")),
        pool_route_symbols=("BNB/USDT",),
        canary_symbols=("BNB/USDT",),
        margin_symbols=("ETH/USDT",),
        futures_symbols=("BTC/USDT",),
        transfer_assets=("USDT",),
        withdrawal_assets=("BNB",),
        prediction_markets=("BTC/USDT",),
    )

    sources = {item.source for item in items}
    assert {
        "trading_symbol",
        "pool_route",
        "canary",
        "margin",
        "futures",
        "transfer",
        "withdrawal",
        "prediction",
    } <= sources


def test_scope_audit_cli_reports_and_fails_on_blocked(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.setattr(
        scope_audit.Settings,
        "from_env",
        classmethod(lambda cls: _settings(symbol_whitelist=("BTC/USDT",))),
    )

    assert scope_audit.run(["--symbol", "ETH/USDT"]) == 0
    assert "ETH/USDT" in capsys.readouterr().out
    assert scope_audit.run(["--symbol", "ETH/USDT", "--fail-on-blocked"]) == 1
    assert "API scope audit failed" in capsys.readouterr().out

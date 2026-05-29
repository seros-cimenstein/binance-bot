from __future__ import annotations

from typing import Any

import pytest

from src.api_permissions import (
    FUTURES,
    READING,
    SPOT_TRADING,
    STATUS_MISSING,
    STATUS_OK,
    STATUS_POLICY_BLOCKED,
    STATUS_UNKNOWN,
    WITHDRAWALS,
    ApiPermissionError,
    ensure_feature_allowed,
    ensure_spot_order_test_allowed,
    inspect_api_permissions,
    render_report,
)
from src.config import Settings


class FakeExchangeError(RuntimeError):
    def __init__(
        self,
        message: str,
        *,
        error_code: int | str | None = None,
        http_status: int | None = None,
    ) -> None:
        super().__init__(message)
        self.error_code = error_code
        self.http_status = http_status


class FakePermissionClient:
    def __init__(
        self,
        *,
        account: dict[str, Any] | None = None,
        account_error: Exception | None = None,
        order_test_error: Exception | None = None,
    ) -> None:
        self.account = account or {
            "canTrade": True,
            "canWithdraw": False,
            "accountType": "SPOT",
            "permissions": ["SPOT"],
        }
        self.account_error = account_error
        self.order_test_error = order_test_error
        self.closed = False
        self.requests: list[tuple[str, tuple[Any, ...]]] = []

    async def request(self, method: str, *args: Any, **_: Any) -> Any:
        self.requests.append((method, args))
        if method == "privateGetAccount":
            if self.account_error is not None:
                raise self.account_error
            return self.account
        if method == "load_markets":
            return {
                "BTC/USDT": {
                    "spot": True,
                    "active": True,
                    "precision": {"amount": 0.00001, "price": 0.01},
                    "limits": {
                        "amount": {"min": 0.00001},
                        "cost": {"min": 5.0},
                    },
                }
            }
        if method == "privatePostOrderTest":
            if self.order_test_error is not None:
                raise self.order_test_error
            return {}
        raise AssertionError(f"unexpected request: {method}")

    async def fetch_ticker(self, symbol: str) -> dict[str, float]:
        assert symbol == "BTC/USDT"
        return {"last": 77000.0}

    async def close(self) -> None:
        self.closed = True


def _settings() -> Settings:
    return Settings(
        api_key="key",
        api_secret="secret",
        use_testnet=False,
        dry_run=False,
        confirm_live_trading="yes",
    )


@pytest.mark.asyncio
async def test_permission_report_allows_current_read_and_live_probe_features() -> None:
    client = FakePermissionClient()

    report = await inspect_api_permissions(_settings(), client=client)

    assert report.capability(READING).status == STATUS_OK
    assert report.capability(SPOT_TRADING).status == STATUS_OK
    assert report.capability(FUTURES).status == STATUS_POLICY_BLOCKED
    assert report.capability(WITHDRAWALS).status == STATUS_POLICY_BLOCKED
    assert report.feature("operations_check").allowed is True
    assert report.feature("paper_pool").allowed is True
    assert report.feature("manual_live_probe").allowed is True
    assert report.feature("spot_live_canary").allowed is True
    assert report.feature("claude_spot_autonomous").allowed is True
    assert report.feature("margin_dry_run").allowed is True
    assert report.feature("margin_canary_plan").allowed is True
    assert report.feature("margin_trading").allowed is False
    assert report.feature("futures_dry_run").allowed is True
    assert report.feature("futures_canary_plan").allowed is True
    assert report.feature("futures_trading").allowed is False
    assert report.feature("transfer_dry_run").allowed is True
    assert report.feature("transfer_canary_plan").allowed is True
    assert report.feature("unified_pool_dry_run").allowed is True
    assert report.feature("symbol_scope_audit").allowed is True
    assert report.feature("withdrawal_dry_run").allowed is True
    assert report.feature("prediction_dry_run").allowed is True
    assert report.feature("universal_transfer").allowed is False
    assert report.feature("prediction_trading").allowed is False
    assert report.feature("future_product_module").allowed is False
    assert ensure_feature_allowed(report, "manual_live_probe").allowed is True
    assert ensure_feature_allowed(report, "claude_spot_autonomous").allowed is True
    assert ensure_feature_allowed(report, "margin_dry_run").allowed is True
    assert ensure_feature_allowed(report, "margin_canary_plan").allowed is True
    assert ensure_feature_allowed(report, "futures_dry_run").allowed is True
    assert ensure_feature_allowed(report, "futures_canary_plan").allowed is True
    assert ensure_feature_allowed(report, "transfer_dry_run").allowed is True
    assert ensure_feature_allowed(report, "transfer_canary_plan").allowed is True
    assert ensure_feature_allowed(report, "unified_pool_dry_run").allowed is True
    assert ensure_feature_allowed(report, "symbol_scope_audit").allowed is True
    assert ensure_feature_allowed(report, "withdrawal_dry_run").allowed is True
    assert ensure_feature_allowed(report, "prediction_dry_run").allowed is True
    with pytest.raises(ApiPermissionError, match="margin_trading is not allowed"):
        ensure_feature_allowed(report, "margin_trading")
    with pytest.raises(ApiPermissionError, match="futures_trading is not allowed"):
        ensure_feature_allowed(report, "futures_trading")
    with pytest.raises(ApiPermissionError, match="prediction_trading is not allowed"):
        ensure_feature_allowed(report, "prediction_trading")
    with pytest.raises(ApiPermissionError, match="future_product_module is not allowed"):
        ensure_feature_allowed(report, "future_product_module")

    order_test_calls = [
        args for method, args in client.requests if method == "privatePostOrderTest"
    ]
    assert order_test_calls == [
        (
            {
                "symbol": "BTCUSDT",
                "side": "BUY",
                "type": "LIMIT",
                "quantity": "0.00007",
                "timeInForce": "GTC",
                "price": "77000",
            },
        )
    ]


@pytest.mark.asyncio
async def test_spot_order_test_permission_error_blocks_live_probe_feature() -> None:
    client = FakePermissionClient(
        order_test_error=FakeExchangeError(
            "binance {'code': -2015, 'msg': 'Invalid API-key, IP, or permissions for action.'}",
            error_code=-2015,
            http_status=400,
        )
    )

    report = await inspect_api_permissions(_settings(), client=client)

    spot = report.capability(SPOT_TRADING)
    assert spot.status == STATUS_MISSING
    assert spot.error_code == -2015
    assert "Spot & Margin Trading" in spot.detail
    assert "server IP" in spot.detail
    assert report.feature("manual_live_probe").allowed is False
    assert report.feature("manual_live_probe").missing_capabilities == (SPOT_TRADING,)
    assert "spot_trading: missing" in render_report(report)


@pytest.mark.asyncio
async def test_account_permission_error_blocks_account_read_features() -> None:
    client = FakePermissionClient(
        account_error=FakeExchangeError(
            "binance {'code': -2015, 'msg': 'Invalid API-key, IP, or permissions for action.'}",
            error_code="-2015",
        )
    )

    report = await inspect_api_permissions(_settings(), client=client)

    assert report.capability(READING).status == STATUS_MISSING
    assert report.capability(SPOT_TRADING).status == STATUS_UNKNOWN
    assert report.feature("operations_check").allowed is False
    assert "verify the current server IP" in report.capability("ip_restriction").detail


@pytest.mark.asyncio
async def test_spot_order_test_guard_raises_before_real_order_path() -> None:
    client = FakePermissionClient(
        order_test_error=FakeExchangeError(
            "Invalid API-key, IP, or permissions for action.",
            error_code=-2015,
        )
    )

    with pytest.raises(ApiPermissionError, match="spot_trading permission check failed"):
        await ensure_spot_order_test_allowed(
            client,
            symbol="BNB/USDT",
            side="sell",
            order_type="market",
            quantity=0.01,
        )

    assert client.requests == [
        (
            "privatePostOrderTest",
            (
                {
                    "symbol": "BNBUSDT",
                    "side": "SELL",
                    "type": "MARKET",
                    "quantity": "0.01",
                },
            ),
        )
    ]

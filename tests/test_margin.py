from __future__ import annotations

from typing import Any

import pytest

from src import margin
from src.config import Settings
from src.margin import (
    ACCOUNT_CROSS,
    ACCOUNT_ISOLATED,
    MarginBorrowRequest,
    MarginModelError,
    MarginRepayRequest,
    MarginRiskLimits,
    assert_margin_live_disabled,
    build_margin_borrow_plan,
    build_margin_dry_run_report,
    build_margin_repay_plan,
    discover_margin_accounts,
)


class FakeMarginClient:
    def __init__(
        self,
        *,
        cross: dict[str, Any] | None = None,
        isolated: dict[str, Any] | None = None,
        max_borrowable: dict[str, str] | None = None,
        interest: list[dict[str, str]] | None = None,
    ) -> None:
        self.cross = cross or _cross_payload()
        self.isolated = isolated or _isolated_payload()
        self.max_borrowable = max_borrowable or {
            "amount": "50",
            "borrowLimit": "100",
        }
        self.interest = interest or [
            {"asset": "USDT", "nextHourlyInterestRate": "0.00001"},
            {"asset": "BNB", "nextHourlyInterestRate": "0.00002"},
        ]
        self.requests: list[tuple[str, tuple[Any, ...], dict[str, Any]]] = []

    async def request(self, method: str, *args: Any, **kwargs: Any) -> Any:
        self.requests.append((method, args, kwargs))
        if method.startswith("sapiPost"):
            raise AssertionError(f"write endpoint should not be used: {method}")
        if method == "sapiGetMarginAccount":
            return self.cross
        if method == "sapiGetMarginIsolatedAccount":
            return self.isolated
        if method == "sapiGetMarginMaxBorrowable":
            return self.max_borrowable
        if method == "sapiGetMarginNextHourlyInterestRate":
            return self.interest
        raise AssertionError(f"unexpected request: {method}")


def _settings() -> Settings:
    return Settings(
        api_key="key",
        api_secret="secret",
        use_testnet=False,
        dry_run=False,
        confirm_live_trading="yes",
    )


def _cross_payload(
    *,
    margin_level: str = "3.2",
    collateral_usdt: str = "100",
) -> dict[str, Any]:
    return {
        "created": True,
        "borrowEnabled": True,
        "tradeEnabled": True,
        "transferInEnabled": True,
        "transferOutEnabled": True,
        "accountType": "MARGIN_1",
        "marginLevel": margin_level,
        "collateralMarginLevel": margin_level,
        "totalAssetOfBtc": "0.01000000",
        "totalLiabilityOfBtc": "0.00100000",
        "totalNetAssetOfBtc": "0.00900000",
        "TotalCollateralValueInUSDT": collateral_usdt,
        "totalOpenOrderLossInUSDT": "0",
        "userAssets": [
            {
                "asset": "USDT",
                "free": "20",
                "locked": "0",
                "borrowed": "5",
                "interest": "0.01",
                "netAsset": "14.99",
            },
            {
                "asset": "BNB",
                "free": "0",
                "locked": "0",
                "borrowed": "0",
                "interest": "0",
                "netAsset": "0",
            },
        ],
    }


def _isolated_payload() -> dict[str, Any]:
    return {
        "assets": [
            {
                "symbol": "BNBUSDT",
                "isolatedCreated": True,
                "enabled": True,
                "tradeEnabled": True,
                "marginLevel": "2.8",
                "marginLevelStatus": "NORMAL",
                "marginRatio": "1.2",
                "indexPrice": "600",
                "liquidatePrice": "420",
                "liquidateRate": "1.1",
                "baseAsset": {
                    "asset": "BNB",
                    "borrowEnabled": True,
                    "repayEnabled": True,
                    "free": "0.09",
                    "locked": "0",
                    "borrowed": "0.01",
                    "interest": "0.0001",
                    "netAsset": "0.0799",
                    "netAssetOfBtc": "0.001",
                    "totalAsset": "0.1",
                },
                "quoteAsset": {
                    "asset": "USDT",
                    "borrowEnabled": True,
                    "repayEnabled": True,
                    "free": "40",
                    "locked": "0",
                    "borrowed": "0",
                    "interest": "0",
                    "netAsset": "40",
                    "netAssetOfBtc": "0.0005",
                    "totalAsset": "40",
                },
            }
        ],
        "totalAssetOfBtc": "0.0015",
        "totalLiabilityOfBtc": "0.0001",
        "totalNetAssetOfBtc": "0.0014",
    }


@pytest.mark.asyncio
async def test_margin_discovery_reads_cross_and_isolated_without_write_calls() -> None:
    client = FakeMarginClient()

    discovery = await discover_margin_accounts(
        client,
        include_isolated=True,
        isolated_symbols=("BNB/USDT",),
    )

    assert discovery.live_trading_enabled is False
    assert discovery.write_endpoints_used == ()
    assert discovery.cross.account_type == "MARGIN_1"
    assert discovery.cross.asset_liability("USDT") == pytest.approx(5.01)
    assert discovery.isolated_snapshot("BNB/USDT") is not None
    assert client.requests[0][0] == "sapiGetMarginAccount"
    assert client.requests[1] == (
        "sapiGetMarginIsolatedAccount",
        ({"symbols": "BNBUSDT"},),
        {"request_weight": 10},
    )


@pytest.mark.asyncio
async def test_borrow_plan_caps_requested_amount_and_models_interest() -> None:
    client = FakeMarginClient()
    discovery = await discover_margin_accounts(client, include_isolated=False)

    plan = await build_margin_borrow_plan(
        client,
        discovery,
        MarginBorrowRequest(
            account_type=ACCOUNT_CROSS,
            asset="USDT",
            requested_amount=30,
        ),
        limits=MarginRiskLimits(
            min_margin_level=1.5,
            liquidation_margin_level=1.1,
            min_liquidation_buffer_percent=10,
            max_borrow_utilization_percent=20,
            max_hourly_interest_usdt=1,
            max_total_liability_usdt=1000,
        ),
    )

    assert plan.allowed is True
    assert plan.max_borrowable == 50
    assert plan.max_allowed_by_utilization == 10
    assert plan.planned_borrow_amount == 10
    assert plan.hourly_interest_rate == 0.00001
    assert plan.projected_hourly_interest == pytest.approx(0.0001)
    assert plan.projected_total_liability_usdt == pytest.approx(15.0101)
    assert plan.debt_before == pytest.approx(5.01)
    assert "requested amount was capped" in plan.warnings[0]


@pytest.mark.asyncio
async def test_borrow_plan_rejects_interest_and_liquidation_risk() -> None:
    client = FakeMarginClient(
        cross=_cross_payload(margin_level="1.2", collateral_usdt="12"),
        interest=[{"asset": "USDT", "nextHourlyInterestRate": "0.01"}],
    )
    discovery = await discover_margin_accounts(client, include_isolated=False)

    plan = await build_margin_borrow_plan(
        client,
        discovery,
        MarginBorrowRequest(
            account_type=ACCOUNT_CROSS,
            asset="USDT",
            requested_amount=10,
        ),
        limits=MarginRiskLimits(
            min_margin_level=2.0,
            liquidation_margin_level=1.1,
            min_liquidation_buffer_percent=25,
            max_borrow_utilization_percent=100,
            max_hourly_interest_usdt=0.05,
            max_total_liability_usdt=1000,
        ),
    )

    assert plan.allowed is False
    assert "current margin level is below the configured minimum" in plan.rejection_reasons
    assert "estimated margin level after borrow is below minimum" in plan.rejection_reasons
    assert "estimated liquidation buffer is below minimum" in plan.rejection_reasons
    assert "projected hourly interest exceeds the configured cap" in plan.rejection_reasons


@pytest.mark.asyncio
async def test_cross_non_stable_borrow_requires_usdt_price_for_caps() -> None:
    client = FakeMarginClient()
    discovery = await discover_margin_accounts(client, include_isolated=False)

    plan = await build_margin_borrow_plan(
        client,
        discovery,
        MarginBorrowRequest(
            account_type=ACCOUNT_CROSS,
            asset="BNB",
            requested_amount=1,
        ),
        limits=MarginRiskLimits(
            min_margin_level=1.5,
            liquidation_margin_level=1.1,
            min_liquidation_buffer_percent=10,
            max_borrow_utilization_percent=100,
            max_hourly_interest_usdt=1,
            max_total_liability_usdt=1000,
        ),
    )

    assert plan.allowed is False
    assert (
        "asset_usdt_price is required for non-stable asset liability caps"
        in plan.rejection_reasons
    )


@pytest.mark.asyncio
async def test_isolated_borrow_plan_uses_isolated_symbol_and_pair_values() -> None:
    client = FakeMarginClient(max_borrowable={"amount": "0.05", "borrowLimit": "1"})
    discovery = await discover_margin_accounts(
        client,
        include_isolated=True,
        isolated_symbols=("BNB/USDT",),
    )

    plan = await build_margin_borrow_plan(
        client,
        discovery,
        MarginBorrowRequest(
            account_type=ACCOUNT_ISOLATED,
            isolated_symbol="BNB/USDT",
            asset="BNB",
            requested_amount=0.02,
            asset_usdt_price=600,
        ),
        limits=MarginRiskLimits(
            min_margin_level=2.0,
            liquidation_margin_level=1.1,
            min_liquidation_buffer_percent=25,
            max_borrow_utilization_percent=100,
            max_hourly_interest_usdt=1,
            max_total_liability_usdt=100,
        ),
    )

    assert plan.allowed is True
    assert plan.planned_borrow_amount == 0.02
    assert plan.margin_level_after_estimate == pytest.approx(100 / 18.06024)
    assert (
        "sapiGetMarginMaxBorrowable",
        ({"asset": "BNB", "isolatedSymbol": "BNBUSDT"},),
        {"request_weight": 750},
    ) in client.requests


@pytest.mark.asyncio
async def test_margin_dry_run_report_never_enables_live_trading() -> None:
    client = FakeMarginClient()

    report = await build_margin_dry_run_report(
        _settings(),
        client=client,
        include_isolated=False,
        borrow_request=MarginBorrowRequest(
            account_type=ACCOUNT_CROSS,
            asset="USDT",
            requested_amount=1,
        ),
        repay_request=MarginRepayRequest(
            account_type=ACCOUNT_CROSS,
            asset="USDT",
            requested_amount=2,
        ),
        limits=MarginRiskLimits(
            min_margin_level=1.5,
            liquidation_margin_level=1.1,
            min_liquidation_buffer_percent=10,
            max_borrow_utilization_percent=50,
            max_hourly_interest_usdt=1,
            max_total_liability_usdt=1000,
        ),
    )

    assert report.live_trading_enabled is False
    assert report.write_endpoints_used == ()
    assert report.borrow_plan is not None
    assert report.repay_plan is not None


@pytest.mark.asyncio
async def test_repay_plan_caps_repayment_to_liability() -> None:
    client = FakeMarginClient()
    discovery = await discover_margin_accounts(client, include_isolated=False)

    plan = build_margin_repay_plan(
        discovery,
        MarginRepayRequest(
            account_type=ACCOUNT_CROSS,
            asset="USDT",
            requested_amount=6,
        ),
    )

    assert plan.allowed is True
    assert plan.borrowed_amount == 5
    assert plan.interest_amount == 0.01
    assert plan.planned_repay_amount == pytest.approx(5.01)
    assert plan.remaining_liability == 0


def test_margin_live_activation_and_cli_fail_closed_without_env(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    with pytest.raises(MarginModelError, match="live margin activation is not implemented"):
        assert_margin_live_disabled(confirm_live_margin=True)

    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)
    monkeypatch.delenv("BINANCE_SECRET_KEY", raising=False)

    assert margin.run([]) == 1

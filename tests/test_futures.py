from __future__ import annotations

from typing import Any

import pytest

from src import futures
from src.config import Settings
from src.futures import (
    FuturesModelError,
    FuturesOrderIntent,
    FuturesRiskLimits,
    assert_futures_live_disabled,
    build_futures_dry_run_report,
    build_futures_order_plan,
    discover_usdm_futures,
    reconcile_futures_state,
)


class FakeFuturesClient:
    def __init__(
        self,
        *,
        account: dict[str, Any] | None = None,
        positions: list[dict[str, Any]] | None = None,
        exchange_info: dict[str, Any] | None = None,
        symbol_config: list[dict[str, Any]] | None = None,
        leverage_brackets: list[dict[str, Any]] | None = None,
        mark_price: dict[str, Any] | list[dict[str, Any]] | None = None,
    ) -> None:
        self.account = account or _account_payload()
        self.positions = positions if positions is not None else [_flat_position()]
        self.exchange_info = exchange_info or _exchange_info_payload()
        self.symbol_config = symbol_config or [_symbol_config_payload()]
        self.leverage_brackets = leverage_brackets or [_leverage_bracket_payload()]
        self.mark_price = mark_price or _mark_price_payload()
        self.requests: list[tuple[str, tuple[Any, ...], dict[str, Any]]] = []

    async def request(self, method: str, *args: Any, **kwargs: Any) -> Any:
        self.requests.append((method, args, kwargs))
        if method.startswith("fapiPrivatePost"):
            raise AssertionError(f"write endpoint should not be used: {method}")
        if method == "fapiPrivateV3GetAccount":
            return self.account
        if method == "fapiPrivateV3GetPositionRisk":
            return self.positions
        if method == "fapiPublicGetExchangeInfo":
            return self.exchange_info
        if method == "fapiPrivateGetSymbolConfig":
            return self.symbol_config
        if method == "fapiPrivateGetLeverageBracket":
            return self.leverage_brackets
        if method == "fapiPublicGetPremiumIndex":
            return self.mark_price
        raise AssertionError(f"unexpected request: {method}")


def _settings() -> Settings:
    return Settings(
        api_key="key",
        api_secret="secret",
        use_testnet=False,
        dry_run=False,
        confirm_live_trading="yes",
    )


def _account_payload(
    *,
    total_margin_balance: str = "200",
    available_balance: str = "180",
    total_maint_margin: str = "1",
) -> dict[str, Any]:
    return {
        "totalWalletBalance": "200",
        "totalUnrealizedProfit": "0",
        "totalMarginBalance": total_margin_balance,
        "totalInitialMargin": "0",
        "totalMaintMargin": total_maint_margin,
        "totalPositionInitialMargin": "0",
        "totalOpenOrderInitialMargin": "0",
        "totalCrossWalletBalance": "200",
        "totalCrossUnPnl": "0",
        "availableBalance": available_balance,
        "maxWithdrawAmount": available_balance,
        "assets": [
            {
                "asset": "USDT",
                "walletBalance": "200",
                "unrealizedProfit": "0",
                "marginBalance": total_margin_balance,
                "maintMargin": total_maint_margin,
                "initialMargin": "0",
                "positionInitialMargin": "0",
                "openOrderInitialMargin": "0",
                "crossWalletBalance": "200",
                "crossUnPnl": "0",
                "availableBalance": available_balance,
                "maxWithdrawAmount": available_balance,
            }
        ],
    }


def _flat_position() -> dict[str, Any]:
    return {
        "symbol": "BTCUSDT",
        "positionSide": "BOTH",
        "positionAmt": "0",
        "entryPrice": "0",
        "breakEvenPrice": "0",
        "markPrice": "50000",
        "unRealizedProfit": "0",
        "liquidationPrice": "0",
        "notional": "0",
        "marginAsset": "USDT",
        "isolatedMargin": "0",
        "isolatedWallet": "0",
        "initialMargin": "0",
        "maintMargin": "0",
        "positionInitialMargin": "0",
        "openOrderInitialMargin": "0",
        "leverage": "3",
        "marginType": "cross",
        "adl": "0",
        "updateTime": "0",
    }


def _long_position(
    *,
    position_amt: str = "0.01",
    notional: str = "500",
) -> dict[str, Any]:
    payload = _flat_position()
    payload.update(
        {
            "positionAmt": position_amt,
            "entryPrice": "50000",
            "breakEvenPrice": "50000",
            "liquidationPrice": "40000",
            "notional": notional,
            "initialMargin": "166.67",
            "maintMargin": "2",
            "positionInitialMargin": "166.67",
            "leverage": "3",
        }
    )
    return payload


def _exchange_info_payload() -> dict[str, Any]:
    return {
        "symbols": [
            {
                "symbol": "BTCUSDT",
                "pair": "BTCUSDT",
                "contractType": "PERPETUAL",
                "status": "TRADING",
                "baseAsset": "BTC",
                "quoteAsset": "USDT",
                "marginAsset": "USDT",
                "triggerProtect": "0.0500",
                "filters": [
                    {
                        "filterType": "PRICE_FILTER",
                        "minPrice": "1",
                        "maxPrice": "1000000",
                        "tickSize": "0.1",
                    },
                    {
                        "filterType": "LOT_SIZE",
                        "minQty": "0.001",
                        "maxQty": "100",
                        "stepSize": "0.001",
                    },
                    {
                        "filterType": "MIN_NOTIONAL",
                        "notional": "5",
                    },
                ],
            }
        ]
    }


def _symbol_config_payload(
    *,
    leverage: str = "3",
    margin_type: str = "CROSSED",
) -> dict[str, Any]:
    return {
        "symbol": "BTCUSDT",
        "marginType": margin_type,
        "isAutoAddMargin": "false",
        "leverage": leverage,
        "maxNotionalValue": "1000000",
    }


def _leverage_bracket_payload(
    *,
    maint_margin_ratio: str = "0.004",
    initial_leverage: str = "20",
) -> dict[str, Any]:
    return {
        "symbol": "BTCUSDT",
        "brackets": [
            {
                "bracket": 1,
                "initialLeverage": initial_leverage,
                "notionalCap": "50000",
                "notionalFloor": "0",
                "maintMarginRatio": maint_margin_ratio,
                "cum": "0",
            }
        ],
    }


def _mark_price_payload(
    *,
    last_funding_rate: str = "0.0001",
) -> dict[str, Any]:
    return {
        "symbol": "BTCUSDT",
        "markPrice": "50000",
        "indexPrice": "49990",
        "estimatedSettlePrice": "49995",
        "lastFundingRate": last_funding_rate,
        "interestRate": "0.0001",
        "nextFundingTime": "1710000000000",
        "time": "1709990000000",
    }


@pytest.mark.asyncio
async def test_futures_discovery_reads_usdm_state_without_write_calls() -> None:
    client = FakeFuturesClient()

    discovery = await discover_usdm_futures(client, symbols=("BTC/USDT",))

    assert discovery.product == "USD-M"
    assert discovery.live_trading_enabled is False
    assert discovery.write_endpoints_used == ()
    assert discovery.account.total_margin_balance == 200
    assert discovery.symbol_rules("BTC/USDT") is not None
    assert discovery.symbol_config("BTC/USDT").leverage == 3
    assert discovery.mark_price("BTC/USDT").last_funding_rate == 0.0001
    assert client.requests == [
        ("fapiPrivateV3GetAccount", (), {"request_weight": 5}),
        ("fapiPrivateV3GetPositionRisk", ({"symbol": "BTCUSDT"},), {"request_weight": 5}),
        ("fapiPublicGetExchangeInfo", (), {"request_weight": 1}),
        ("fapiPrivateGetSymbolConfig", ({"symbol": "BTCUSDT"},), {"request_weight": 5}),
        ("fapiPrivateGetLeverageBracket", ({"symbol": "BTCUSDT"},), {"request_weight": 1}),
        ("fapiPublicGetPremiumIndex", ({"symbol": "BTCUSDT"},), {"request_weight": 1}),
    ]


@pytest.mark.asyncio
async def test_futures_order_plan_models_leverage_funding_and_liquidation() -> None:
    discovery = await discover_usdm_futures(
        FakeFuturesClient(),
        symbols=("BTC/USDT",),
    )

    plan = build_futures_order_plan(
        discovery,
        FuturesOrderIntent(
            symbol="BTC/USDT",
            side="BUY",
            quantity=0.001,
        ),
        limits=FuturesRiskLimits(
            max_leverage=3,
            max_notional_usdt=100,
            max_account_notional_percent=100,
            max_initial_margin_usdt=20,
            min_free_balance_after_initial_margin_usdt=5,
            min_liquidation_distance_percent=20,
            max_abs_funding_rate=0.001,
            max_abs_funding_payment_usdt=0.05,
            min_maintenance_buffer_percent=50,
        ),
    )

    assert plan.allowed is True
    assert plan.notional_usdt == 50
    assert plan.configured_leverage == 3
    assert plan.initial_margin_required_usdt == pytest.approx(16.6666667)
    assert plan.maintenance_margin_estimate_usdt == pytest.approx(0.2)
    assert plan.liquidation_distance_percent == pytest.approx(32.9333333)
    assert plan.projected_funding_payment_usdt == pytest.approx(0.005)


@pytest.mark.asyncio
async def test_futures_order_plan_rejects_leverage_funding_and_liquidation_risk() -> None:
    client = FakeFuturesClient(
        symbol_config=[_symbol_config_payload(leverage="10")],
        mark_price=_mark_price_payload(last_funding_rate="0.002"),
    )
    discovery = await discover_usdm_futures(client, symbols=("BTC/USDT",))

    plan = build_futures_order_plan(
        discovery,
        FuturesOrderIntent(
            symbol="BTC/USDT",
            side="BUY",
            quantity=0.001,
        ),
        limits=FuturesRiskLimits(
            max_leverage=3,
            max_notional_usdt=100,
            max_account_notional_percent=100,
            max_initial_margin_usdt=20,
            min_free_balance_after_initial_margin_usdt=5,
            min_liquidation_distance_percent=20,
            max_abs_funding_rate=0.001,
            max_abs_funding_payment_usdt=1,
            min_maintenance_buffer_percent=50,
        ),
    )

    assert plan.allowed is False
    assert "configured leverage exceeds the risk limit" in plan.rejection_reasons
    assert "estimated liquidation distance is below minimum" in plan.rejection_reasons
    assert "absolute funding rate exceeds configured cap" in plan.rejection_reasons


@pytest.mark.asyncio
async def test_reduce_only_plan_requires_existing_reducible_position() -> None:
    discovery = await discover_usdm_futures(
        FakeFuturesClient(
            account=_account_payload(
                total_margin_balance="500",
                available_balance="480",
                total_maint_margin="1",
            ),
            positions=[_long_position()],
        ),
        symbols=("BTC/USDT",),
    )

    allowed = build_futures_order_plan(
        discovery,
        FuturesOrderIntent(
            symbol="BTC/USDT",
            side="SELL",
            quantity=0.005,
            reduce_only=True,
        ),
        limits=FuturesRiskLimits(
            max_leverage=3,
            max_notional_usdt=300,
            max_account_notional_percent=100,
            max_initial_margin_usdt=100,
            min_free_balance_after_initial_margin_usdt=5,
            min_liquidation_distance_percent=20,
            max_abs_funding_rate=0.001,
            max_abs_funding_payment_usdt=1,
            min_maintenance_buffer_percent=50,
        ),
    )

    rejected = build_futures_order_plan(
        discovery,
        FuturesOrderIntent(
            symbol="BTC/USDT",
            side="BUY",
            quantity=0.005,
            reduce_only=True,
        ),
        limits=FuturesRiskLimits(
            max_leverage=3,
            max_notional_usdt=300,
            max_account_notional_percent=100,
            max_initial_margin_usdt=100,
            min_free_balance_after_initial_margin_usdt=5,
            min_liquidation_distance_percent=20,
            max_abs_funding_rate=0.001,
            max_abs_funding_payment_usdt=1,
            min_maintenance_buffer_percent=50,
        ),
    )

    assert allowed.allowed is True
    assert allowed.closing_position is True
    assert rejected.allowed is False
    assert "reduce-only order would not reduce an existing position" in rejected.rejection_reasons


@pytest.mark.asyncio
async def test_futures_reconciliation_rejects_low_buffer_and_excess_notional() -> None:
    discovery = await discover_usdm_futures(
        FakeFuturesClient(
            account=_account_payload(
                total_margin_balance="10",
                available_balance="5",
                total_maint_margin="9",
            ),
            positions=[_long_position()],
        ),
        symbols=("BTC/USDT",),
    )

    reconciliation = reconcile_futures_state(
        discovery,
        limits=FuturesRiskLimits(
            max_leverage=3,
            max_notional_usdt=100,
            max_account_notional_percent=100,
            max_initial_margin_usdt=100,
            min_free_balance_after_initial_margin_usdt=0,
            min_liquidation_distance_percent=20,
            max_abs_funding_rate=0.001,
            max_abs_funding_payment_usdt=1,
            min_maintenance_buffer_percent=50,
        ),
    )

    assert reconciliation.allowed is False
    assert reconciliation.nonzero_positions == 1
    assert reconciliation.maintenance_buffer_percent == pytest.approx(11.1111111)
    assert "account maintenance buffer is below minimum" in reconciliation.rejection_reasons
    assert "open position notional exceeds max_notional_usdt" in reconciliation.rejection_reasons


@pytest.mark.asyncio
async def test_futures_dry_run_report_never_enables_live_trading() -> None:
    report = await build_futures_dry_run_report(
        _settings(),
        client=FakeFuturesClient(),
        symbols=("BTC/USDT",),
        intent=FuturesOrderIntent(
            symbol="BTC/USDT",
            side="BUY",
            quantity=0.001,
        ),
        limits=FuturesRiskLimits(
            max_leverage=3,
            max_notional_usdt=100,
            max_account_notional_percent=100,
            max_initial_margin_usdt=20,
            min_free_balance_after_initial_margin_usdt=5,
            min_liquidation_distance_percent=20,
            max_abs_funding_rate=0.001,
            max_abs_funding_payment_usdt=0.05,
            min_maintenance_buffer_percent=50,
        ),
    )

    assert report.live_trading_enabled is False
    assert report.write_endpoints_used == ()
    assert report.order_plan is not None
    assert report.reconciliation is not None


def test_futures_live_activation_and_cli_fail_closed_without_env(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    with pytest.raises(FuturesModelError, match="live futures activation is not implemented"):
        assert_futures_live_disabled(confirm_live_futures=True)

    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)
    monkeypatch.delenv("BINANCE_SECRET_KEY", raising=False)

    assert futures.run([]) == 1
    assert futures.run(["--dry-run", "--confirm-live-futures"]) == 1

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.futures import (
    FuturesAccountSnapshot,
    FuturesAssetBalance,
    FuturesDiscovery,
    FuturesPosition,
)
from src.margin import (
    CrossMarginSnapshot,
    MarginAssetBalance,
    MarginDiscovery,
)
from src.pool.unified import (
    ACCOUNT_FUNDING,
    ACCOUNT_FUTURES,
    ACCOUNT_MARGIN,
    ACCOUNT_SPOT,
    UnifiedHolding,
    UnifiedPoolLimits,
    build_unified_pool_snapshot,
    holdings_from_futures_discovery,
    holdings_from_margin_discovery,
    holdings_from_transfer_discovery,
    run,
)
from src.transfers import (
    WALLET_CROSS_MARGIN,
    WALLET_FUNDING,
    WALLET_SPOT,
    WALLET_USD_M_FUTURES,
    TransferDiscovery,
    WalletAssetBalance,
)


def _prices() -> dict[str, float]:
    return {
        "USDT": 1,
        "BNB": 600,
        "BTC": 50_000,
        "PAXG": 2_300,
        "XAUT": 2_300,
    }


def test_unified_pool_values_spot_margin_futures_and_benchmarks() -> None:
    snapshot = build_unified_pool_snapshot(
        [
            UnifiedHolding(
                account_type=ACCOUNT_SPOT,
                wallet=WALLET_SPOT,
                asset="BNB",
                total=0.02,
                free=0.017,
                locked=0.003,
                liquidity_score=0.9,
                volatility_percent=20,
            ),
            UnifiedHolding(
                account_type=ACCOUNT_FUNDING,
                wallet=WALLET_FUNDING,
                asset="USDT",
                total=5,
                free=5,
            ),
            UnifiedHolding(
                account_type=ACCOUNT_MARGIN,
                wallet=WALLET_CROSS_MARGIN,
                asset="USDT",
                total=10,
                free=8,
                borrowed=2,
                interest=0.1,
            ),
            UnifiedHolding(
                account_type=ACCOUNT_FUTURES,
                wallet=WALLET_USD_M_FUTURES,
                asset="USDT",
                total=20,
                free=16,
                available=16,
                unrealized_pnl=-1,
                initial_margin=3,
                maintenance_margin=0.5,
                leverage=2,
                symbol="BTCUSDT",
            ),
        ],
        _prices(),
        limits=UnifiedPoolLimits(
            max_asset_allocation_percent=100,
            max_account_type_allocation_percent=100,
            max_debt_percent=100,
            min_liquidity_score=0.5,
            max_volatility_percent=50,
        ),
    )

    assert snapshot.allowed is True
    assert snapshot.total_gross_usdt == pytest.approx(47)
    assert snapshot.total_net_usdt == pytest.approx(43.9)
    assert snapshot.total_debt_usdt == pytest.approx(2)
    assert snapshot.total_interest_usdt == pytest.approx(0.1)
    assert snapshot.total_open_pnl_usdt == pytest.approx(-1)
    assert snapshot.total_initial_margin_usdt == pytest.approx(3)
    assert snapshot.total_maintenance_margin_usdt == pytest.approx(0.5)
    assert snapshot.asset_values_usdt["BNB"] == pytest.approx(12)
    assert snapshot.account_type_values_usdt[ACCOUNT_MARGIN] == pytest.approx(7.9)
    assert snapshot.benchmark_values["USDT"] == pytest.approx(43.9)
    assert snapshot.benchmark_values["BTC"] == pytest.approx(43.9 / 50_000)
    assert snapshot.benchmark_values["PAXG"] == pytest.approx(43.9 / 2_300)
    assert snapshot.basket_components["USDT"] == pytest.approx(43.9 / 4)
    assert snapshot.live_trading_enabled is False
    assert snapshot.live_transfer_enabled is False
    assert snapshot.write_endpoints_used == ()


def test_unified_pool_rejects_allocation_leverage_liquidity_volatility_and_reserve() -> None:
    snapshot = build_unified_pool_snapshot(
        [
            UnifiedHolding(
                account_type=ACCOUNT_FUTURES,
                wallet=WALLET_USD_M_FUTURES,
                asset="BNB",
                total=1,
                free=0,
                leverage=5,
                symbol="BNBUSDT",
                liquidity_score=0.1,
                volatility_percent=120,
                transfer_enabled=False,
            )
        ],
        _prices(),
        limits=UnifiedPoolLimits(
            max_asset_allocation_percent=50,
            max_account_type_allocation_percent=80,
            max_leverage=3,
            max_debt_percent=10,
            min_transferable_usdt=10,
            min_liquidity_score=0.5,
            max_volatility_percent=50,
            asset_reserves={"BNB": 0.00005},
        ),
    )

    blocked = {constraint.name for constraint in snapshot.constraints if constraint.status == "blocked"}
    assert snapshot.allowed is False
    assert "asset_allocation" in blocked
    assert "account_type_allocation" in blocked
    assert "leverage" in blocked
    assert "transferability" in blocked
    assert "liquidity" in blocked
    assert "volatility" in blocked
    assert "reserve" in blocked


def test_unified_pool_converts_transfer_margin_and_futures_sources() -> None:
    transfer = TransferDiscovery(
        timestamp="now",
        asset="USDT",
        quote_asset="USDT",
        wallet_activations=(),
        balances=(
            WalletAssetBalance(wallet=WALLET_SPOT, asset="USDT", free=2),
            WalletAssetBalance(wallet=WALLET_FUNDING, asset="USDT", free=3),
        ),
    )
    margin = MarginDiscovery(
        timestamp="now",
        cross=CrossMarginSnapshot(
            created=True,
            borrow_enabled=True,
            trade_enabled=True,
            transfer_in_enabled=True,
            transfer_out_enabled=True,
            account_type="MARGIN_1",
            margin_level=3,
            collateral_margin_level=3,
            total_asset_btc=0,
            total_liability_btc=0,
            total_net_asset_btc=0,
            total_collateral_usdt=0,
            total_open_order_loss_usdt=0,
            assets=(
                MarginAssetBalance(
                    asset="USDT",
                    free=5,
                    borrowed=1,
                    interest=0.05,
                ),
            ),
        ),
    )
    futures = FuturesDiscovery(
        timestamp="now",
        product="USD-M",
        account=FuturesAccountSnapshot(
            total_wallet_balance=10,
            total_unrealized_profit=0.5,
            total_margin_balance=10.5,
            total_initial_margin=2,
            total_maint_margin=0.2,
            total_position_initial_margin=2,
            total_open_order_initial_margin=0,
            total_cross_wallet_balance=10,
            total_cross_unrealized_pnl=0.5,
            available_balance=8,
            max_withdraw_amount=8,
            assets=(
                FuturesAssetBalance(
                    asset="USDT",
                    wallet_balance=10,
                    unrealized_profit=0.5,
                    margin_balance=10.5,
                    initial_margin=2,
                    maint_margin=0.2,
                    available_balance=8,
                ),
            ),
        ),
        positions=(
            FuturesPosition(
                symbol="BTCUSDT",
                position_side="BOTH",
                position_amt=0.001,
                entry_price=50_000,
                break_even_price=50_000,
                mark_price=50_500,
                unrealized_profit=0.5,
                liquidation_price=40_000,
                notional=50.5,
                margin_asset="USDT",
                isolated_margin=0,
                isolated_wallet=0,
                initial_margin=2,
                maint_margin=0.2,
                position_initial_margin=2,
                open_order_initial_margin=0,
                leverage=3,
                margin_type="CROSSED",
                adl=0,
                update_time=0,
            ),
        ),
        symbols=(),
        symbol_configs=(),
        leverage_brackets=(),
        mark_prices=(),
    )

    holdings = (
        *holdings_from_transfer_discovery(transfer),
        *holdings_from_margin_discovery(margin),
        *holdings_from_futures_discovery(futures),
    )
    snapshot = build_unified_pool_snapshot(
        holdings,
        _prices(),
        limits=UnifiedPoolLimits(
            max_asset_allocation_percent=100,
            max_account_type_allocation_percent=100,
            max_debt_percent=100,
            asset_reserves={},
        ),
    )

    assert any(holding.source == "transfer_discovery" for holding in holdings)
    assert any(holding.source == "margin_cross" for holding in holdings)
    assert any(holding.source == "futures_account" for holding in holdings)
    assert any(holding.source == "futures_position" for holding in holdings)
    assert snapshot.account_type_values_usdt[ACCOUNT_SPOT] == pytest.approx(2)
    assert snapshot.account_type_values_usdt[ACCOUNT_FUNDING] == pytest.approx(3)
    assert snapshot.account_type_values_usdt[ACCOUNT_MARGIN] == pytest.approx(3.95)
    assert snapshot.account_type_values_usdt[ACCOUNT_FUTURES] == pytest.approx(11)
    assert snapshot.total_open_pnl_usdt == pytest.approx(1.0)


def test_unified_pool_cli_uses_local_json_and_fails_without_dry_run(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    holdings_path = tmp_path / "holdings.json"
    prices_path = tmp_path / "prices.json"
    holdings_path.write_text(
        json.dumps(
            [
                UnifiedHolding(
                    account_type=ACCOUNT_SPOT,
                    wallet=WALLET_SPOT,
                    asset="USDT",
                    total=10,
                    free=10,
                ).as_dict()
            ]
        ),
        encoding="utf-8",
    )
    prices_path.write_text(json.dumps({"USDT": 1, "BTC": 50_000}), encoding="utf-8")

    assert run([]) == 1
    assert (
        run(
            [
                "--dry-run",
                "--holdings-json",
                str(holdings_path),
                "--prices-json",
                str(prices_path),
                "--benchmarks",
                "USDT,BTC",
                "--json",
            ]
        )
        == 0
    )
    output = capsys.readouterr().out
    assert '"total_net_usdt": 10.0' in output

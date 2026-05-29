from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from src import transfers
from src.config import Settings
from src.transfers import (
    WALLET_CROSS_MARGIN,
    WALLET_FUNDING,
    WALLET_ISOLATED_MARGIN,
    WALLET_SPOT,
    WALLET_USD_M_FUTURES,
    TransferPlanError,
    TransferRequest,
    TransferRiskLimits,
    assert_transfer_live_disabled,
    build_transfer_dry_run_report,
    build_transfer_plan,
    discover_transfer_wallets,
    write_transfer_audit,
)


class FakeTransferClient:
    def __init__(
        self,
        *,
        wallet_summary: list[dict[str, Any]] | None = None,
        spot_assets: list[dict[str, Any]] | None = None,
        funding_assets: list[dict[str, Any]] | None = None,
        margin: dict[str, Any] | None = None,
        futures: dict[str, Any] | None = None,
        history: dict[str, Any] | None = None,
    ) -> None:
        self.wallet_summary = wallet_summary or _wallet_summary()
        self.spot_assets = spot_assets or _spot_assets()
        self.funding_assets = funding_assets or _funding_assets()
        self.margin = margin or _margin_payload()
        self.futures = futures or _futures_payload()
        self.history = history or _history_payload()
        self.requests: list[tuple[str, tuple[Any, ...], dict[str, Any]]] = []

    async def request(self, method: str, *args: Any, **kwargs: Any) -> Any:
        self.requests.append((method, args, kwargs))
        if method == "sapiPostAssetTransfer":
            raise AssertionError("live transfer endpoint should not be used")
        if method == "sapiGetAssetWalletBalance":
            return self.wallet_summary
        if method == "sapiV3PostAssetGetUserAsset":
            return self.spot_assets
        if method == "sapiPostAssetGetFundingAsset":
            return self.funding_assets
        if method == "sapiGetMarginAccount":
            return self.margin
        if method == "fapiPrivateV3GetAccount":
            return self.futures
        if method == "sapiGetAssetTransfer":
            return self.history
        raise AssertionError(f"unexpected request: {method}")


def _settings() -> Settings:
    return Settings(
        api_key="key",
        api_secret="secret",
        use_testnet=False,
        dry_run=False,
        confirm_live_trading="yes",
    )


def _wallet_summary(*, funding_active: bool = True) -> list[dict[str, Any]]:
    return [
        {"walletName": "Spot", "activate": True, "balance": "20"},
        {"walletName": "Funding", "activate": funding_active, "balance": "1"},
        {"walletName": "Cross Margin", "activate": True, "balance": "8"},
        {"walletName": "Isolated Margin", "activate": True, "balance": "0"},
        {"walletName": "USDⓈ-M Futures", "activate": True, "balance": "4"},
    ]


def _spot_assets() -> list[dict[str, Any]]:
    return [
        {
            "asset": "USDT",
            "free": "20",
            "locked": "0",
            "freeze": "0",
            "withdrawing": "0",
            "btcValuation": "0.0002",
        }
    ]


def _funding_assets() -> list[dict[str, Any]]:
    return [
        {
            "asset": "USDT",
            "free": "1",
            "locked": "0",
            "freeze": "0",
            "withdrawing": "0",
            "btcValuation": "0.00001",
        }
    ]


def _margin_payload(*, borrowed: str = "0", interest: str = "0") -> dict[str, Any]:
    return {
        "userAssets": [
            {
                "asset": "USDT",
                "free": "8",
                "locked": "0",
                "borrowed": borrowed,
                "interest": interest,
            }
        ]
    }


def _futures_payload() -> dict[str, Any]:
    return {
        "assets": [
            {
                "asset": "USDT",
                "availableBalance": "4",
                "walletBalance": "5",
                "initialMargin": "1",
                "maintMargin": "0.1",
            }
        ]
    }


def _history_payload() -> dict[str, Any]:
    return {
        "total": 1,
        "rows": [
            {
                "asset": "USDT",
                "amount": "2",
                "type": "MAIN_FUNDING",
                "status": "CONFIRMED",
                "tranId": "123",
                "timestamp": 1700000000000,
            }
        ],
    }


@pytest.mark.asyncio
async def test_transfer_discovery_reads_wallets_without_live_transfer_call() -> None:
    client = FakeTransferClient()

    discovery = await discover_transfer_wallets(
        client,
        asset="USDT",
        history_type="MAIN_FUNDING",
        include_margin=True,
        include_futures=True,
    )

    assert discovery.live_transfers_enabled is False
    assert discovery.write_endpoints_used == ()
    assert discovery.activation(WALLET_SPOT).active is True
    assert discovery.balance(WALLET_SPOT).free == 20
    assert discovery.balance(WALLET_FUNDING).free == 1
    assert discovery.balance(WALLET_CROSS_MARGIN).free == 8
    assert discovery.balance(WALLET_USD_M_FUTURES).transferable_free == 4
    assert discovery.recent_transfers[0].transfer_type == "MAIN_FUNDING"
    assert [request[0] for request in client.requests] == [
        "sapiGetAssetWalletBalance",
        "sapiV3PostAssetGetUserAsset",
        "sapiPostAssetGetFundingAsset",
        "sapiGetMarginAccount",
        "fapiPrivateV3GetAccount",
        "sapiGetAssetTransfer",
    ]


@pytest.mark.asyncio
async def test_transfer_plan_models_route_and_audit(tmp_path: Path) -> None:
    client = FakeTransferClient()

    report = await build_transfer_dry_run_report(
        _settings(),
        client=client,
        request=TransferRequest(
            source_wallet=WALLET_SPOT,
            destination_wallet=WALLET_FUNDING,
            asset="USDT",
            amount=5,
            reason="funding wallet reserve",
        ),
        limits=TransferRiskLimits(
            max_transfer_amount=10,
            max_transfer_value_usdt=25,
            min_source_free_after=10,
        ),
        audit_path=str(tmp_path / "transfer-plans.jsonl"),
    )

    assert report.live_transfers_enabled is False
    assert report.write_endpoints_used == ()
    assert report.plan is not None
    assert report.plan["allowed"] is True
    assert report.plan["transfer_type"] == "MAIN_FUNDING"
    assert report.plan["source_free_after"] == 15
    assert report.plan["destination_free_after"] == 6
    assert report.plan["submission_attempted"] is False
    assert Path(report.audit_path).exists()
    rows = Path(report.audit_path).read_text(encoding="utf-8").splitlines()
    assert len(rows) == 1
    assert json.loads(rows[0])["reason"] == "funding wallet reserve"


@pytest.mark.asyncio
async def test_transfer_plan_rejects_inactive_wallet_and_insufficient_balance() -> None:
    discovery = await discover_transfer_wallets(
        FakeTransferClient(wallet_summary=_wallet_summary(funding_active=False)),
        asset="USDT",
        include_margin=False,
        include_futures=False,
    )

    plan = build_transfer_plan(
        discovery,
        TransferRequest(
            source_wallet=WALLET_SPOT,
            destination_wallet=WALLET_FUNDING,
            asset="USDT",
            amount=50,
        ),
        limits=TransferRiskLimits(
            max_transfer_amount=100,
            max_transfer_value_usdt=100,
            min_source_free_after=0,
        ),
    )

    assert plan.allowed is False
    assert "destination wallet is not active" in plan.rejection_reasons
    assert "amount exceeds source transferable balance" in plan.rejection_reasons


@pytest.mark.asyncio
async def test_transfer_plan_blocks_margin_debt_transfer_out() -> None:
    discovery = await discover_transfer_wallets(
        FakeTransferClient(margin=_margin_payload(borrowed="1", interest="0.01")),
        asset="USDT",
        include_margin=True,
        include_futures=False,
    )

    plan = build_transfer_plan(
        discovery,
        TransferRequest(
            source_wallet=WALLET_CROSS_MARGIN,
            destination_wallet=WALLET_SPOT,
            asset="USDT",
            amount=1,
        ),
        limits=TransferRiskLimits(
            max_transfer_amount=10,
            max_transfer_value_usdt=25,
            min_source_free_after=0,
        ),
    )

    assert plan.allowed is False
    assert "source margin wallet has outstanding debt or interest" in plan.rejection_reasons


@pytest.mark.asyncio
async def test_transfer_plan_enforces_isolated_symbol_constraints() -> None:
    discovery = await discover_transfer_wallets(
        FakeTransferClient(),
        asset="USDT",
        include_margin=True,
        include_futures=False,
    )

    missing_symbol = build_transfer_plan(
        discovery,
        TransferRequest(
            source_wallet=WALLET_CROSS_MARGIN,
            destination_wallet=WALLET_ISOLATED_MARGIN,
            asset="USDT",
            amount=1,
        ),
        limits=TransferRiskLimits(
            max_transfer_amount=10,
            max_transfer_value_usdt=25,
            min_source_free_after=0,
        ),
    )
    with_symbol = build_transfer_plan(
        discovery,
        TransferRequest(
            source_wallet=WALLET_CROSS_MARGIN,
            destination_wallet=WALLET_ISOLATED_MARGIN,
            asset="USDT",
            amount=1,
            to_symbol="BNB/USDT",
        ),
        limits=TransferRiskLimits(
            max_transfer_amount=10,
            max_transfer_value_usdt=25,
            min_source_free_after=0,
        ),
    )

    assert missing_symbol.allowed is False
    assert "to_symbol is required for isolated-margin destination" in missing_symbol.rejection_reasons
    assert with_symbol.allowed is True
    assert with_symbol.to_symbol == "BNBUSDT"


@pytest.mark.asyncio
async def test_non_stable_transfer_requires_price_for_usdt_cap() -> None:
    discovery = await discover_transfer_wallets(
        FakeTransferClient(
            spot_assets=[{"asset": "BNB", "free": "0.05", "locked": "0"}],
            funding_assets=[{"asset": "BNB", "free": "0", "locked": "0"}],
        ),
        asset="BNB",
        include_margin=False,
        include_futures=False,
    )

    plan = build_transfer_plan(
        discovery,
        TransferRequest(
            source_wallet=WALLET_SPOT,
            destination_wallet=WALLET_FUNDING,
            asset="BNB",
            amount=0.01,
        ),
    )

    assert plan.allowed is False
    assert "asset_usdt_price is required for non-stable transfer caps" in plan.rejection_reasons


def test_transfer_audit_and_live_activation_fail_closed(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    with pytest.raises(TransferPlanError, match="live transfer activation is not implemented"):
        assert_transfer_live_disabled(confirm_live_transfer=True)

    audit_path = tmp_path / "manual.jsonl"
    plan = build_transfer_plan(
        transfers.TransferDiscovery(
            timestamp="now",
            asset="USDT",
            quote_asset="USDT",
            wallet_activations=(),
            balances=(
                transfers.WalletAssetBalance(
                    wallet=WALLET_SPOT,
                    asset="USDT",
                    free=2,
                ),
                transfers.WalletAssetBalance(
                    wallet=WALLET_FUNDING,
                    asset="USDT",
                    free=0,
                ),
            ),
        ),
        TransferRequest(
            source_wallet=WALLET_SPOT,
            destination_wallet=WALLET_FUNDING,
            asset="USDT",
            amount=1,
        ),
    )
    write_transfer_audit(audit_path, plan)
    assert json.loads(audit_path.read_text(encoding="utf-8"))["submitted"] is False

    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)
    monkeypatch.delenv("BINANCE_SECRET_KEY", raising=False)

    assert transfers.run([]) == 1
    assert transfers.run(["--dry-run", "--confirm-live-transfer"]) == 1

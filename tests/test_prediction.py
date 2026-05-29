from __future__ import annotations

from typing import Any

import pytest

from src import prediction
from src.config import Settings
from src.prediction import (
    PredictionIntent,
    PredictionModelError,
    PredictionRiskLimits,
    assert_prediction_live_disabled,
    build_prediction_dry_run_report,
    build_prediction_plan,
    discover_prediction_markets,
    parse_prediction_markets,
    render_prediction_report,
)


class FakePredictionClient:
    def __init__(self, payload: dict[str, Any] | None = None) -> None:
        self.payload = payload or _market_payload()
        self.requests: list[tuple[str, tuple[Any, ...], dict[str, Any]]] = []

    async def request(self, method: str, *args: Any, **kwargs: Any) -> Any:
        self.requests.append((method, args, kwargs))
        if method.startswith("predictionPost") or method.startswith("bapiPredictionPost"):
            raise AssertionError(f"write endpoint should not be used: {method}")
        if method == "predictionGetMarkets":
            return self.payload
        raise AssertionError(f"unexpected request: {method}")


def _settings() -> Settings:
    return Settings(
        api_key="key",
        api_secret="secret",
        use_testnet=False,
        dry_run=False,
        confirm_live_trading="yes",
    )


def _market_payload(
    *,
    status: str = "OPEN",
    lock_timestamp: int = 1_900_000_600_000,
    close_timestamp: int = 1_900_001_800_000,
    bull_amount: str = "55",
    bear_amount: str = "45",
    total_amount: str = "100",
) -> dict[str, Any]:
    return {
        "data": [
            {
                "id": "market-1",
                "epoch": "123",
                "symbol": "BTCUSDT",
                "status": status,
                "lockTimestamp": lock_timestamp,
                "closeTimestamp": close_timestamp,
                "lockPrice": "50000",
                "currentPrice": "50100",
                "bullAmount": bull_amount,
                "bearAmount": bear_amount,
                "totalAmount": total_amount,
                "bullPayout": "1.8",
                "bearPayout": "2.2",
                "baseAsset": "BTC",
                "quoteAsset": "USDT",
            }
        ]
    }


def _limits(**overrides: Any) -> PredictionRiskLimits:
    values = {
        "max_stake_usdt": 10,
        "max_total_stake_usdt": 20,
        "max_stake_bankroll_percent": 10,
        "min_seconds_to_lock": 60,
        "max_seconds_to_settlement": 3600,
        "min_total_pool_usdt": 25,
        "max_pool_imbalance_percent": 80,
        "max_settlement_risk_score": 50,
        "max_drawdown_percent": 10,
        "emergency_stop": False,
    }
    values.update(overrides)
    return PredictionRiskLimits(**values)


@pytest.mark.asyncio
async def test_prediction_discovery_defaults_disabled_and_uses_no_write_calls() -> None:
    client = FakePredictionClient()

    discovery = await discover_prediction_markets(client, symbols=("BTC/USDT",))

    assert discovery.live_trading_enabled is False
    assert discovery.write_endpoints_used == ()
    assert discovery.market("market-1").symbol == "BTCUSDT"
    assert client.requests == [("predictionGetMarkets", (), {"request_weight": 1})]


def test_prediction_live_activation_and_cli_fail_closed_without_env(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    with pytest.raises(
        PredictionModelError,
        match="live prediction activation is not implemented",
    ):
        assert_prediction_live_disabled(confirm_live_prediction=True)

    monkeypatch.delenv("BINANCE_API_KEY", raising=False)
    monkeypatch.delenv("BINANCE_API_SECRET", raising=False)
    monkeypatch.delenv("BINANCE_SECRET_KEY", raising=False)

    assert prediction.run([]) == 1
    assert prediction.run(["--dry-run", "--confirm-live-prediction"]) == 1


def test_prediction_market_data_parser_accepts_binance_like_payload() -> None:
    markets = parse_prediction_markets(_market_payload(), symbols=("BTC/USDT",))

    assert len(markets) == 1
    market = markets[0]
    assert market.market_id == "market-1"
    assert market.round_id == "123"
    assert market.symbol == "BTCUSDT"
    assert market.lock_time == 1_900_000_600
    assert market.close_time == 1_900_001_800
    assert market.pool_up_usdt == 55
    assert market.pool_down_usdt == 45
    assert market.payout_up == 1.8


@pytest.mark.asyncio
async def test_prediction_plan_rejects_stake_drawdown_settlement_and_pool_risk() -> None:
    discovery = await discover_prediction_markets(
        payload=_market_payload(
            lock_timestamp=1_900_000_030_000,
            close_timestamp=1_900_005_000_000,
            bull_amount="95",
            bear_amount="5",
            total_amount="100",
        ),
        symbols=("BTC/USDT",),
    )

    plan = build_prediction_plan(
        discovery,
        PredictionIntent(
            market="BTC/USDT",
            direction="UP",
            stake_usdt=12,
            bankroll_usdt=100,
            current_open_stake_usdt=10,
            current_drawdown_percent=12,
            emergency_stop=True,
            now=1_900_000_000,
        ),
        limits=_limits(max_settlement_risk_score=20),
    )

    assert plan.allowed is False
    assert "emergency stop is active" in plan.rejection_reasons
    assert "stake exceeds max_stake_usdt" in plan.rejection_reasons
    assert "projected open stake exceeds max_total_stake_usdt" in plan.rejection_reasons
    assert "stake exceeds bankroll percent limit" in plan.rejection_reasons
    assert "current drawdown exceeds configured maximum" in plan.rejection_reasons
    assert "market is too close to lock" in plan.rejection_reasons
    assert "settlement window exceeds configured maximum" in plan.rejection_reasons
    assert "market pool imbalance exceeds configured maximum" in plan.rejection_reasons
    assert "settlement risk exceeds configured maximum" in plan.rejection_reasons


@pytest.mark.asyncio
async def test_prediction_report_includes_audit_output() -> None:
    report = await build_prediction_dry_run_report(
        _settings(),
        payload=_market_payload(),
        symbols=("BTC/USDT",),
        intent=PredictionIntent(
            market="market-1",
            direction="DOWN",
            stake_usdt=2,
            bankroll_usdt=100,
            now=1_900_000_000,
        ),
        limits=_limits(),
    )

    rendered = render_prediction_report(report)

    assert report.live_trading_enabled is False
    assert report.write_endpoints_used == ()
    assert report.audit["market_data_available"] is True
    assert report.audit["allowed"] is True
    assert report.plan is not None
    assert "Live prediction trading: disabled" in rendered
    assert "Write endpoints used: none" in rendered
    assert "Audit:" in rendered
    assert "Prediction plan:" in rendered


@pytest.mark.asyncio
async def test_prediction_unavailable_data_fails_closed() -> None:
    with pytest.raises(
        PredictionModelError,
        match="prediction market data did not include any markets",
    ):
        await discover_prediction_markets(payload={"data": []}, symbols=("BTC/USDT",))

    discovery = await discover_prediction_markets(
        payload=_market_payload(status="CLOSED"),
        symbols=("BTC/USDT",),
    )

    plan = build_prediction_plan(
        discovery,
        PredictionIntent(
            market="BTC/USDT",
            direction="UP",
            stake_usdt=1,
            bankroll_usdt=100,
            now=1_900_000_000,
        ),
        limits=_limits(),
    )

    assert plan.allowed is False
    assert "prediction market is not available" in plan.rejection_reasons

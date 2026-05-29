from __future__ import annotations

import sqlite3
import time
from pathlib import Path
from typing import Any

import pytest

from src.pool.catalog import MarketCatalog
from src.pool.config import PoolSettings
from src.pool.fees import CommissionService, estimate_bnb_reserve
from src.pool.ledger import PaperLedger
from src.pool.models import MarketSnapshot
from src.pool.routes import RouteEngine


def _settings(**overrides: Any) -> PoolSettings:
    values = {
        "api_key": "key",
        "api_secret": "secret",
        "min_quote_volume_usdt": 100.0,
        "max_spread_bps": 20.0,
        "slippage_buffer_bps": 0.0,
    }
    values.update(overrides)
    return PoolSettings(**values)


def _market(symbol: str, base: str, quote: str) -> dict[str, Any]:
    amount_step = "0.00001000"
    return {
        "symbol": symbol,
        "base": base,
        "quote": quote,
        "spot": True,
        "active": True,
        "precision": {"amount": 0.00001},
        "limits": {"amount": {"min": 0.00001}, "cost": {"min": 5.0}},
        "info": {
            "filters": [
                {
                    "filterType": "LOT_SIZE",
                    "minQty": amount_step,
                    "stepSize": amount_step,
                }
            ]
        },
    }


def _ticker(bid: float, ask: float, volume: float) -> dict[str, Any]:
    return {
        "bid": bid,
        "ask": ask,
        "last": bid,
        "quoteVolume": volume,
        "timestamp": int(time.time() * 1000),
    }


class FakeCatalogClient:
    async def request(self, method: str, *args: Any, **kwargs: Any) -> dict[str, Any]:
        if method == "load_markets":
            inactive = _market("OLD/USDT", "OLD", "USDT")
            inactive["active"] = False
            return {
                "BNB/USDT": _market("BNB/USDT", "BNB", "USDT"),
                "COIN/BNB": _market("COIN/BNB", "COIN", "BNB"),
                "OLD/USDT": inactive,
            }
        return {
            "BNB/USDT": _ticker(600.0, 600.1, 10_000.0),
            "COIN/BNB": _ticker(0.01, 0.010001, 1.0),
            "OLD/USDT": _ticker(1.0, 1.0001, 10_000.0),
        }


@pytest.mark.asyncio
async def test_catalog_discovers_all_spot_markets_and_normalizes_quote_volume() -> None:
    catalog = MarketCatalog(FakeCatalogClient(), _settings())

    markets = await catalog.refresh()

    assert set(markets) == {"BNB/USDT", "COIN/BNB", "OLD/USDT"}
    assert markets["BNB/USDT"].eligible is True
    assert markets["BNB/USDT"].amount_step == pytest.approx(0.00001)
    assert markets["COIN/BNB"].quote_volume_usdt == pytest.approx(600.0)
    assert markets["COIN/BNB"].eligible is True
    assert markets["OLD/USDT"].eligible is False
    assert "market is not active" in markets["OLD/USDT"].rejection_reasons


class FakeCommissionClient:
    def __init__(self, taker_rate: str = "0.001", *, discount: bool = True) -> None:
        self.calls: list[str] = []
        self.taker_rate = taker_rate
        self.discount = discount

    async def get_commission_rates(self, symbol: str) -> dict[str, Any]:
        self.calls.append(symbol)
        return {
            "symbol": symbol,
            "standardCommission": {
                "maker": "0.001",
                "taker": self.taker_rate,
                "buyer": "0",
                "seller": "0",
            },
            "taxCommission": {},
            "specialCommission": {},
            "discount": {
                "enabledForAccount": self.discount,
                "enabledForSymbol": self.discount,
                "discountAsset": "BNB",
                "discount": "0.75",
            },
        }


def _snapshot(
    symbol: str,
    base: str,
    quote: str,
    bid: float,
    ask: float,
    *,
    min_amount: float = 0.00001,
    min_notional: float = 5.0,
    amount_precision: int | float = 0.00001,
    amount_step: float | None = None,
) -> MarketSnapshot:
    return MarketSnapshot(
        symbol=symbol,
        base=base,
        quote=quote,
        bid=bid,
        ask=ask,
        last=bid,
        quote_volume=1_000_000,
        quote_volume_usdt=1_000_000,
        min_amount=min_amount,
        min_notional=min_notional,
        amount_precision=amount_precision,
        timestamp_ms=int(time.time() * 1000),
        spread_bps=1,
        eligible=True,
        amount_step=amount_step,
    )


@pytest.mark.asyncio
async def test_route_engine_costs_multileg_route_with_dynamic_discounted_fees() -> None:
    settings = _settings()
    commission_client = FakeCommissionClient()
    engine = RouteEngine(
        {
            "BNB/USDT": _snapshot("BNB/USDT", "BNB", "USDT", 600, 600.1),
            "ETH/USDT": _snapshot("ETH/USDT", "ETH", "USDT", 3000, 3000.1),
        },
        CommissionService(commission_client),
        settings,
    )

    quote = await engine.quote("BNB", "ETH", 0.02)

    assert quote is not None
    assert quote.symbols == ("BNB/USDT", "ETH/USDT")
    assert len(quote.legs) == 2
    assert quote.legs[0].fee_rate == pytest.approx(0.00075)
    assert quote.output_amount < (0.02 * 600 / 3000.1)
    assert set(commission_client.calls) == {"BNB/USDT", "ETH/USDT"}
    assert await engine.quote("BNB", "ETH", 0.001) is None


@pytest.mark.asyncio
async def test_route_engine_always_costs_direct_path_before_choosing_multihop() -> None:
    settings = _settings(max_candidate_routes=1)
    engine = RouteEngine(
        {
            "BNB/ETH": _snapshot("BNB/ETH", "BNB", "ETH", 100.0, 100.1),
            "BNB/USDT": _snapshot("BNB/USDT", "BNB", "USDT", 102.0, 102.1),
            "ETH/USDT": _snapshot("ETH/USDT", "ETH", "USDT", 1.0, 1.0),
        },
        CommissionService(FakeCommissionClient("0.10", discount=False)),
        settings,
    )

    quote = await engine.quote("BNB", "ETH", 10.0)

    assert quote is not None
    assert quote.symbols == ("BNB/ETH",)


@pytest.mark.asyncio
async def test_route_engine_uses_lot_step_over_precision_for_market_amounts() -> None:
    settings = _settings(max_route_legs=1)
    engine = RouteEngine(
        {
            "HBAR/BNB": _snapshot(
                "HBAR/BNB",
                "HBAR",
                "BNB",
                0.000132,
                0.0001325,
                min_amount=1.0,
                min_notional=0.000001,
                amount_precision=0.1,
                amount_step=1.0,
            )
        },
        CommissionService(FakeCommissionClient("0", discount=False)),
        settings,
    )

    quote = await engine.quote("BNB", "HBAR", 0.017013)

    assert quote is not None
    assert quote.legs[0].gross_output_amount == pytest.approx(128.0)
    assert quote.legs[0].spent_amount == pytest.approx(128.0 * 0.0001325)


def test_dynamic_bnb_reserve_uses_route_fees_and_floor() -> None:
    settings = _settings()
    engine = RouteEngine(
        {"BNB/USDT": _snapshot("BNB/USDT", "BNB", "USDT", 600, 600.1)},
        CommissionService(FakeCommissionClient()),
        settings,
    )
    assert engine.indicative_rate("USDT", "BNB") is not None

    assert estimate_bnb_reserve(
        [],
        destination_to_bnb={},
        multiplier=3,
        floor_bnb=0.00005,
    ) == 0.00005


def test_paper_ledger_applies_routes_without_live_order_state(tmp_path: Path) -> None:
    path = tmp_path / "pool.db"
    ledger = PaperLedger(str(path))
    portfolio = ledger.seed_if_absent("simple", "simple_momentum", {"BNB": 1.0})
    assert portfolio.balances == {"BNB": 1.0}
    from src.pool.models import RouteQuote

    route = RouteQuote("BNB", "USDT", 0.5, 299.0, (), 0.0)
    updated = ledger.apply_route("simple", route)
    assert updated.balances == {"BNB": 0.5, "USDT": 299.0}
    ledger.close()


def test_paper_ledger_keeps_latest_catalog_and_rolling_observations(
    tmp_path: Path,
) -> None:
    path = tmp_path / "pool.db"
    ledger = PaperLedger(str(path))
    market = _snapshot("BNB/USDT", "BNB", "USDT", 600, 600.1)
    ledger.record_catalog("2026-05-25T01:00:00+00:00", {"BNB/USDT": market})
    ledger.record_catalog("2026-05-25T01:15:00+00:00", {"BNB/USDT": market})
    for index in range(3):
        ledger.record_observations(
            f"2026-05-25T01:{index:02d}:00+00:00",
            {("BNB", "USDT"): 600.0 + index},
            retain_latest=2,
        )
    ledger.close()

    with sqlite3.connect(path) as connection:
        assert connection.execute("select count(*) from market_catalog_latest").fetchone()[0] == 1
        assert connection.execute("select count(*) from market_catalog_cycles").fetchone()[0] == 2
        rates = connection.execute(
            "select rate from pool_observations order by observed_at"
        ).fetchall()
    assert rates == [(601.0,), (602.0,)]

"""Fast deterministic replay harness for the paper-only asset pool."""

from __future__ import annotations

import argparse
import asyncio
import json
import math
from collections import Counter
from dataclasses import dataclass, fields
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Mapping

from loguru import logger

from src.pool.config import PoolSettings
from src.pool.ledger import PaperLedger
from src.pool.runtime import PaperPoolRuntime


@dataclass(frozen=True, slots=True)
class ReplaySummary:
    """Compact outcome of a fixture replay run."""

    cycles: int
    db_path: str
    decisions: dict[str, int]
    final_balances: dict[str, dict[str, float]]
    request_methods: tuple[str, ...]
    balance_reads: int
    commission_reads: int

    def as_dict(self) -> dict[str, object]:
        return {
            "cycles": self.cycles,
            "db_path": self.db_path,
            "decisions": self.decisions,
            "final_balances": self.final_balances,
            "request_counts": dict(Counter(self.request_methods)),
            "balance_reads": self.balance_reads,
            "commission_reads": self.commission_reads,
        }


class ReplayFixtureClient:
    """Read-only fake Binance client backed by a replay fixture."""

    def __init__(self, fixture: Mapping[str, Any]) -> None:
        self._fixture = fixture
        self._cycle_index = 0
        self.balance_reads = 0
        self.commission_reads = 0
        self.request_methods: list[str] = []
        self.initial_balances = _positive_balances(
            fixture.get("initial_balances", {})
        )
        self._markets = _required_list(fixture, "markets")
        self._commission = _mapping(fixture.get("commission", {}))
        self._started_at = _parse_start_time(
            str(fixture.get("started_at", "2026-01-01T00:00:00+00:00"))
        )
        self._cycle_seconds = _positive_float(fixture.get("cycle_seconds", 900.0))

    @property
    def fixture_cycles(self) -> int:
        lengths = [
            len(_series(market))
            for market in self._markets
        ]
        return max(lengths, default=1)

    def now(self) -> datetime:
        return self._started_at + timedelta(
            seconds=self._cycle_index * self._cycle_seconds
        )

    def advance(self) -> None:
        self._cycle_index += 1

    async def get_balance(self) -> dict[str, dict[str, float]]:
        self.balance_reads += 1
        return {
            "total": dict(self.initial_balances),
            "free": dict(self.initial_balances),
        }

    async def get_commission_rates(self, symbol: str) -> dict[str, Any]:
        self.commission_reads += 1
        taker = str(self._commission.get("taker", "0.001"))
        maker = str(self._commission.get("maker", taker))
        discount_enabled = bool(self._commission.get("discount_enabled", True))
        return {
            "symbol": symbol,
            "standardCommission": {
                "maker": maker,
                "taker": taker,
                "buyer": str(self._commission.get("buyer", "0")),
                "seller": str(self._commission.get("seller", "0")),
            },
            "taxCommission": {},
            "specialCommission": {},
            "discount": {
                "enabledForAccount": discount_enabled,
                "enabledForSymbol": discount_enabled,
                "discountAsset": str(self._commission.get("discount_asset", "BNB")),
                "discount": str(self._commission.get("discount_multiplier", "0.75")),
            },
        }

    async def request(self, method: str, *args: Any, **kwargs: Any) -> dict[str, Any]:
        self.request_methods.append(method)
        if method == "load_markets":
            return {
                str(market["symbol"]): self._market_payload(market)
                for market in self._markets
            }
        if method == "fetch_tickers":
            return {
                str(market["symbol"]): self._ticker_payload(market)
                for market in self._markets
            }
        raise RuntimeError(f"replay fixture does not allow exchange method: {method}")

    def _market_payload(self, market: Mapping[str, Any]) -> dict[str, Any]:
        symbol = str(market["symbol"])
        base, quote = symbol.split("/", 1)
        base = str(market.get("base", base)).upper()
        quote = str(market.get("quote", quote)).upper()
        return {
            "symbol": symbol,
            "base": base,
            "quote": quote,
            "spot": bool(market.get("spot", True)),
            "active": bool(market.get("active", True)),
            "precision": {
                "amount": market.get("amount_precision", 0.00001),
            },
            "limits": {
                "amount": {"min": market.get("min_amount", 0.00001)},
                "cost": {"min": market.get("min_notional", 5.0)},
            },
        }

    def _ticker_payload(self, market: Mapping[str, Any]) -> dict[str, Any]:
        price = _cycle_value(_series(market), self._cycle_index)
        spread_bps = _non_negative_float(market.get("spread_bps", 2.0))
        half_spread = price * spread_bps / 20_000
        quote_volume = market.get("quote_volume_series", market.get("quote_volume", 1_000_000))
        if isinstance(quote_volume, list):
            quote_volume = _cycle_value(
                [_non_negative_float(item) for item in quote_volume],
                self._cycle_index,
            )
        return {
            "bid": price - half_spread,
            "ask": price + half_spread,
            "last": price,
            "quoteVolume": _non_negative_float(quote_volume),
            "timestamp": int(self.now().timestamp() * 1000),
        }


async def run_replay(
    fixture_path: str | Path,
    *,
    cycles: int | None = None,
    db_path: str | Path = "data/pool-replay.db",
    reset_db: bool = True,
) -> ReplaySummary:
    """Run the paper runtime quickly against deterministic market data."""
    fixture = load_replay_fixture(fixture_path)
    return await run_replay_fixture(
        fixture,
        cycles=cycles,
        db_path=db_path,
        reset_db=reset_db,
    )


async def run_replay_fixture(
    fixture: Mapping[str, Any],
    *,
    cycles: int | None = None,
    db_path: str | Path = "data/pool-replay.db",
    reset_db: bool = True,
) -> ReplaySummary:
    """Run a validated or generated read-only replay payload."""
    _positive_balances(fixture.get("initial_balances", {}))
    _required_list(fixture, "markets")
    db = Path(db_path)
    _ensure_replay_db_path(db)
    if reset_db and db.exists():
        if db.is_dir():
            raise RuntimeError(f"replay db path is a directory: {db}")
        db.unlink()

    client = ReplayFixtureClient(fixture)
    total_cycles = cycles if cycles is not None else client.fixture_cycles
    if total_cycles <= 0:
        raise RuntimeError("replay cycles must be positive")

    settings = _settings_from_fixture(fixture, db)
    ledger = PaperLedger(str(db))
    runtime = PaperPoolRuntime(client, settings, ledger=ledger, clock=client.now)
    decisions: dict[str, int] = {}
    try:
        for _ in range(total_cycles):
            report = await runtime.cycle()
            for decision in report.decisions:
                decisions[decision.action] = decisions.get(decision.action, 0) + 1
            client.advance()
        final_balances = {
            scorer: ledger.load_portfolio(scorer).balances
            for scorer in settings.scorers
        }
    finally:
        runtime.close()

    return ReplaySummary(
        cycles=total_cycles,
        db_path=str(db),
        decisions=decisions,
        final_balances=final_balances,
        request_methods=tuple(client.request_methods),
        balance_reads=client.balance_reads,
        commission_reads=client.commission_reads,
    )


def load_replay_fixture(path: str | Path) -> dict[str, Any]:
    payload = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise RuntimeError("replay fixture must be a JSON object")
    _positive_balances(payload.get("initial_balances", {}))
    _required_list(payload, "markets")
    return payload


def _settings_from_fixture(fixture: Mapping[str, Any], db_path: Path) -> PoolSettings:
    raw_settings = _mapping(fixture.get("settings", {}))
    allowed = {field.name for field in fields(PoolSettings)}
    unknown = set(raw_settings) - allowed
    if unknown:
        names = ", ".join(sorted(unknown))
        raise RuntimeError(f"unknown replay PoolSettings field(s): {names}")
    settings = dict(raw_settings)
    settings.update(
        {
            "api_key": "replay",
            "api_secret": "replay",
            "db_path": str(db_path),
        }
    )
    return PoolSettings(**settings)


def _ensure_replay_db_path(path: Path) -> None:
    if path.name == "pool-paper.db":
        raise RuntimeError("replay db path must not use the production paper ledger")


def _mapping(value: object) -> Mapping[str, Any]:
    if value is None:
        return {}
    if not isinstance(value, Mapping):
        raise RuntimeError("replay fixture section must be a JSON object")
    return value


def _required_list(fixture: Mapping[str, Any], key: str) -> list[Mapping[str, Any]]:
    value = fixture.get(key)
    if not isinstance(value, list) or not value:
        raise RuntimeError(f"replay fixture requires non-empty list: {key}")
    if not all(isinstance(item, Mapping) for item in value):
        raise RuntimeError(f"replay fixture {key} entries must be JSON objects")
    return value


def _positive_balances(value: object) -> dict[str, float]:
    if not isinstance(value, Mapping):
        raise RuntimeError("replay fixture initial_balances must be a JSON object")
    balances = {
        str(asset).upper(): _positive_float(amount)
        for asset, amount in value.items()
        if _positive_float(amount) > 0
    }
    if not balances:
        raise RuntimeError("replay fixture requires at least one positive balance")
    return balances


def _series(market: Mapping[str, Any]) -> list[float]:
    raw = market.get("price_series", market.get("prices"))
    if raw is None:
        raw = [market.get("price")]
    if not isinstance(raw, list) or not raw:
        raise RuntimeError(f"market {market.get('symbol')} requires price_series")
    return [_positive_float(value) for value in raw]


def _cycle_value(values: list[float], index: int) -> float:
    return values[min(index, len(values) - 1)]


def _parse_start_time(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _positive_float(value: object) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise RuntimeError("replay values must be positive") from exc
    if not math.isfinite(number) or number <= 0:
        raise RuntimeError("replay values must be positive")
    return number


def _non_negative_float(value: object) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise RuntimeError("replay values must be non-negative") from exc
    if not math.isfinite(number) or number < 0:
        raise RuntimeError("replay values must be non-negative")
    return number


def main() -> None:
    parser = argparse.ArgumentParser(description="Run fast paper-pool fixture replay")
    parser.add_argument("--fixture", required=True, help="path to replay fixture JSON")
    parser.add_argument("--cycles", type=int, default=None, help="number of replay cycles")
    parser.add_argument(
        "--db-path",
        default="data/pool-replay.db",
        help="isolated replay SQLite path; must not be pool-paper.db",
    )
    parser.add_argument(
        "--append",
        action="store_true",
        help="append to an existing replay database instead of resetting it",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="suppress per-cycle logs and print only the replay summary",
    )
    args = parser.parse_args()
    if args.quiet:
        logger.remove()
    summary = asyncio.run(
        run_replay(
            args.fixture,
            cycles=args.cycles,
            db_path=args.db_path,
            reset_db=not args.append,
        )
    )
    print(json.dumps(summary.as_dict(), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()

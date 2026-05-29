"""SQLite persistence for paper portfolio balances, observations, and decisions."""

from __future__ import annotations

import json
import sqlite3
import threading
from datetime import datetime, timezone
from pathlib import Path

from src.pool.models import PaperDecision, RouteQuote, VirtualPortfolio


_SCHEMA = """
CREATE TABLE IF NOT EXISTS virtual_portfolios (
    portfolio_id TEXT PRIMARY KEY,
    scorer_name TEXT NOT NULL,
    seeded_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS virtual_balances (
    portfolio_id TEXT NOT NULL,
    asset TEXT NOT NULL,
    amount REAL NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (portfolio_id, asset)
);
CREATE TABLE IF NOT EXISTS pool_observations (
    observed_at TEXT NOT NULL,
    asset TEXT NOT NULL,
    benchmark TEXT NOT NULL,
    rate REAL NOT NULL,
    PRIMARY KEY (observed_at, asset, benchmark)
);
CREATE TABLE IF NOT EXISTS market_catalog_latest (
    symbol TEXT PRIMARY KEY,
    observed_at TEXT NOT NULL,
    base_asset TEXT NOT NULL,
    quote_asset TEXT NOT NULL,
    eligible INTEGER NOT NULL,
    quote_volume_usdt REAL,
    spread_bps REAL NOT NULL,
    rejection_reasons TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS market_catalog_cycles (
    observed_at TEXT PRIMARY KEY,
    markets_count INTEGER NOT NULL,
    eligible_count INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS pool_snapshots (
    observed_at TEXT NOT NULL,
    portfolio_id TEXT NOT NULL,
    benchmark TEXT NOT NULL,
    value REAL NOT NULL,
    basket_score REAL,
    PRIMARY KEY (observed_at, portfolio_id, benchmark)
);
CREATE TABLE IF NOT EXISTS paper_decisions (
    decided_at TEXT NOT NULL,
    portfolio_id TEXT NOT NULL,
    scorer_name TEXT NOT NULL,
    action TEXT NOT NULL,
    source_asset TEXT,
    destination_asset TEXT,
    input_amount REAL,
    output_amount REAL,
    score REAL,
    reason TEXT NOT NULL,
    route_json TEXT NOT NULL
);
"""


class PaperLedger:
    """Persistent virtual state that is isolated from live order storage."""

    def __init__(self, path: str) -> None:
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        self._connection = sqlite3.connect(path, check_same_thread=False)
        self._connection.row_factory = sqlite3.Row
        self._lock = threading.Lock()
        with self._lock:
            self._connection.executescript(_SCHEMA)
            self._connection.commit()

    def seed_if_absent(
        self,
        portfolio_id: str,
        scorer_name: str,
        balances: dict[str, float],
    ) -> VirtualPortfolio:
        with self._lock:
            row = self._connection.execute(
                "SELECT portfolio_id FROM virtual_portfolios WHERE portfolio_id = ?",
                (portfolio_id,),
            ).fetchone()
            if row is None:
                timestamp = _timestamp()
                self._connection.execute(
                    "INSERT INTO virtual_portfolios (portfolio_id, scorer_name, seeded_at) VALUES (?, ?, ?)",
                    (portfolio_id, scorer_name, timestamp),
                )
                for asset, amount in balances.items():
                    if amount > 0:
                        self._connection.execute(
                            "INSERT INTO virtual_balances (portfolio_id, asset, amount, updated_at) VALUES (?, ?, ?, ?)",
                            (portfolio_id, asset.upper(), amount, timestamp),
                        )
                self._connection.commit()
        return self.load_portfolio(portfolio_id)

    def load_portfolio(self, portfolio_id: str) -> VirtualPortfolio:
        with self._lock:
            portfolio = self._connection.execute(
                "SELECT * FROM virtual_portfolios WHERE portfolio_id = ?",
                (portfolio_id,),
            ).fetchone()
            if portfolio is None:
                raise KeyError(f"unknown virtual portfolio: {portfolio_id}")
            balances = self._connection.execute(
                "SELECT asset, amount FROM virtual_balances WHERE portfolio_id = ?",
                (portfolio_id,),
            ).fetchall()
        return VirtualPortfolio(
            portfolio_id=portfolio["portfolio_id"],
            scorer_name=portfolio["scorer_name"],
            balances={row["asset"]: float(row["amount"]) for row in balances},
            seeded_at=datetime.fromisoformat(portfolio["seeded_at"]),
        )

    def apply_route(self, portfolio_id: str, route: RouteQuote) -> VirtualPortfolio:
        """Apply a simulated fill; this method never calls an exchange."""
        portfolio = self.load_portfolio(portfolio_id)
        available = portfolio.balances.get(route.source_asset, 0.0)
        if route.input_amount > available + 1e-12:
            raise ValueError("paper route spends more than virtual balance")
        balances = dict(portfolio.balances)
        balances[route.source_asset] = max(available - route.input_amount, 0.0)
        balances[route.destination_asset] = (
            balances.get(route.destination_asset, 0.0) + route.output_amount
        )
        timestamp = _timestamp()
        with self._lock:
            for asset, amount in balances.items():
                self._connection.execute(
                    """
                    INSERT OR REPLACE INTO virtual_balances
                        (portfolio_id, asset, amount, updated_at)
                    VALUES (?, ?, ?, ?)
                    """,
                    (portfolio_id, asset, amount, timestamp),
                )
            self._connection.commit()
        return self.load_portfolio(portfolio_id)

    def record_observations(
        self,
        observed_at: str,
        rates: dict[tuple[str, str], float],
        *,
        retain_latest: int,
    ) -> None:
        with self._lock:
            self._connection.executemany(
                """
                INSERT OR REPLACE INTO pool_observations
                    (observed_at, asset, benchmark, rate)
                VALUES (?, ?, ?, ?)
                """,
                [
                    (observed_at, asset, benchmark, rate)
                    for (asset, benchmark), rate in rates.items()
                ],
            )
            self._connection.execute(
                """
                DELETE FROM pool_observations
                WHERE rowid IN (
                    SELECT rowid FROM (
                        SELECT rowid,
                               ROW_NUMBER() OVER (
                                   PARTITION BY asset, benchmark
                                   ORDER BY observed_at DESC
                               ) AS observation_rank
                        FROM pool_observations
                    )
                    WHERE observation_rank > ?
                )
                """,
                (retain_latest,),
            )
            self._connection.commit()

    def record_catalog(self, observed_at: str, markets: dict[str, object]) -> None:
        """Persist market admission decisions for paper-run auditability."""
        with self._lock:
            self._connection.execute(
                """
                INSERT OR REPLACE INTO market_catalog_cycles
                    (observed_at, markets_count, eligible_count)
                VALUES (?, ?, ?)
                """,
                (
                    observed_at,
                    len(markets),
                    sum(int(market.eligible) for market in markets.values()),
                ),
            )
            self._connection.executemany(
                """
                INSERT OR REPLACE INTO market_catalog_latest
                    (observed_at, symbol, base_asset, quote_asset, eligible,
                     quote_volume_usdt, spread_bps, rejection_reasons)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                """,
                [
                    (
                        observed_at,
                        market.symbol,
                        market.base,
                        market.quote,
                        int(market.eligible),
                        market.quote_volume_usdt,
                        market.spread_bps,
                        json.dumps(market.rejection_reasons),
                    )
                    for market in markets.values()
                ],
            )
            self._connection.commit()

    def observation_history(
        self,
        asset: str,
        benchmarks: tuple[str, ...],
        *,
        limit: int,
    ) -> dict[str, list[float]]:
        histories: dict[str, list[float]] = {}
        with self._lock:
            for benchmark in benchmarks:
                rows = self._connection.execute(
                    """
                    SELECT rate FROM pool_observations
                    WHERE asset = ? AND benchmark = ?
                    ORDER BY observed_at DESC LIMIT ?
                    """,
                    (asset, benchmark, limit),
                ).fetchall()
                histories[benchmark] = [float(row["rate"]) for row in reversed(rows)]
        return histories

    def record_snapshot(
        self,
        observed_at: str,
        portfolio_id: str,
        totals: dict[str, float],
        basket_score: float | None,
    ) -> None:
        with self._lock:
            self._connection.executemany(
                """
                INSERT OR REPLACE INTO pool_snapshots
                    (observed_at, portfolio_id, benchmark, value, basket_score)
                VALUES (?, ?, ?, ?, ?)
                """,
                [
                    (observed_at, portfolio_id, benchmark, value, basket_score)
                    for benchmark, value in totals.items()
                ],
            )
            self._connection.commit()

    def initial_totals(self, portfolio_id: str) -> dict[str, float]:
        with self._lock:
            rows = self._connection.execute(
                """
                SELECT s.benchmark, s.value
                FROM pool_snapshots s
                JOIN (
                    SELECT benchmark, MIN(observed_at) AS first_at
                    FROM pool_snapshots WHERE portfolio_id = ? GROUP BY benchmark
                ) first
                ON first.benchmark = s.benchmark AND first.first_at = s.observed_at
                WHERE s.portfolio_id = ?
                """,
                (portfolio_id, portfolio_id),
            ).fetchall()
        return {row["benchmark"]: float(row["value"]) for row in rows}

    def record_decision(self, decision: PaperDecision) -> None:
        route_payload = []
        if decision.route is not None:
            route_payload = [
                {
                    "symbol": leg.symbol,
                    "side": leg.side,
                    "source": leg.source_asset,
                    "destination": leg.destination_asset,
                    "input": leg.input_amount,
                    "output": leg.net_output_amount,
                    "fee_rate": leg.fee_rate,
                    "fee": leg.fee_output_amount,
                }
                for leg in decision.route.legs
            ]
        with self._lock:
            self._connection.execute(
                """
                INSERT INTO paper_decisions
                    (decided_at, portfolio_id, scorer_name, action,
                     source_asset, destination_asset, input_amount,
                     output_amount, score, reason, route_json)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    decision.decided_at.isoformat(),
                    decision.portfolio_id,
                    decision.scorer_name,
                    decision.action,
                    decision.source_asset,
                    decision.destination_asset,
                    decision.input_amount,
                    decision.route.output_amount if decision.route else None,
                    decision.score,
                    decision.reason,
                    json.dumps(route_payload, sort_keys=True),
                ),
            )
            self._connection.commit()

    def last_rebalance_at(self, portfolio_id: str) -> datetime | None:
        with self._lock:
            row = self._connection.execute(
                """
                SELECT decided_at FROM paper_decisions
                WHERE portfolio_id = ? AND action = 'rebalance'
                ORDER BY decided_at DESC LIMIT 1
                """,
                (portfolio_id,),
            ).fetchone()
        return datetime.fromisoformat(row["decided_at"]) if row is not None else None

    def close(self) -> None:
        with self._lock:
            self._connection.close()


def _timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()

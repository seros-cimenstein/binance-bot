"""SQLite persistence for order history and daily loss state."""

from __future__ import annotations

import sqlite3
import threading
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from src.orders.models import Order


_SCHEMA = """
CREATE TABLE IF NOT EXISTS orders (
    id TEXT PRIMARY KEY,
    symbol TEXT NOT NULL,
    type TEXT NOT NULL,
    side TEXT NOT NULL,
    status TEXT NOT NULL,
    amount REAL NOT NULL,
    price REAL,
    filled_amount REAL NOT NULL DEFAULT 0.0,
    remaining_amount REAL NOT NULL DEFAULT 0.0,
    avg_fill_price REAL,
    timestamp TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS fills (
    id TEXT NOT NULL,
    order_id TEXT NOT NULL,
    symbol TEXT NOT NULL,
    side TEXT NOT NULL,
    price REAL NOT NULL,
    amount REAL NOT NULL,
    fee REAL NOT NULL,
    fee_currency TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    PRIMARY KEY (id, order_id),
    FOREIGN KEY (order_id) REFERENCES orders(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS daily_loss (
    date TEXT PRIMARY KEY,
    loss_usd REAL NOT NULL
);
"""


class OrderDB:
    """Thread-safe SQLite store for orders, fills, and daily loss."""

    def __init__(self, db_path: str = "data/bot.db") -> None:
        Path(db_path).parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(db_path, check_same_thread=False)
        self._conn.execute("PRAGMA foreign_keys = ON")
        self._conn.row_factory = sqlite3.Row
        self._lock = threading.Lock()
        with self._lock:
            self._conn.executescript(_SCHEMA)
            self._conn.commit()

    def upsert_order(self, order: "Order") -> None:
        with self._lock:
            self._conn.execute(
                """
                INSERT OR REPLACE INTO orders
                    (id, symbol, type, side, status, amount, price,
                     filled_amount, remaining_amount, avg_fill_price, timestamp)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    order.id,
                    order.symbol,
                    order.type.value,
                    order.side.value,
                    order.status.value,
                    order.amount,
                    order.price,
                    order.filled_amount,
                    order.remaining_amount,
                    order.avg_fill_price,
                    order.timestamp.isoformat(),
                ),
            )
            for fill in order.fills:
                self._conn.execute(
                    """
                    INSERT OR IGNORE INTO fills
                        (id, order_id, symbol, side, price, amount, fee, fee_currency, timestamp)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (
                        fill.id,
                        fill.order_id,
                        fill.symbol,
                        fill.side.value,
                        fill.price,
                        fill.amount,
                        fill.fee,
                        fill.fee_currency,
                        fill.timestamp.isoformat(),
                    ),
                )
            self._conn.commit()

    def delete_order(self, order_id: str) -> None:
        with self._lock:
            self._conn.execute("DELETE FROM orders WHERE id = ?", (order_id,))
            self._conn.commit()

    def load_orders(self) -> list["Order"]:
        from src.orders.models import Fill, Order, OrderSide, OrderStatus, OrderType

        with self._lock:
            order_rows = self._conn.execute(
                "SELECT * FROM orders ORDER BY timestamp"
            ).fetchall()
            fill_rows = self._conn.execute(
                "SELECT * FROM fills ORDER BY timestamp"
            ).fetchall()

        fills_by_order: dict[str, list[Fill]] = {}
        for row in fill_rows:
            fill = Fill(
                id=row["id"],
                order_id=row["order_id"],
                symbol=row["symbol"],
                side=OrderSide(row["side"]),
                price=row["price"],
                amount=row["amount"],
                fee=row["fee"],
                fee_currency=row["fee_currency"],
                timestamp=datetime.fromisoformat(row["timestamp"]),
            )
            fills_by_order.setdefault(row["order_id"], []).append(fill)

        orders = []
        for row in order_rows:
            order = Order(
                id=row["id"],
                symbol=row["symbol"],
                type=OrderType(row["type"]),
                side=OrderSide(row["side"]),
                status=OrderStatus(row["status"]),
                amount=row["amount"],
                price=row["price"],
                filled_amount=row["filled_amount"],
                remaining_amount=row["remaining_amount"],
                avg_fill_price=row["avg_fill_price"],
                fills=fills_by_order.get(row["id"], []),
                timestamp=datetime.fromisoformat(row["timestamp"]),
            )
            orders.append(order)
        return orders

    def upsert_daily_loss(self, date: str, loss_usd: float) -> None:
        with self._lock:
            self._conn.execute(
                "INSERT OR REPLACE INTO daily_loss (date, loss_usd) VALUES (?, ?)",
                (date, loss_usd),
            )
            self._conn.commit()

    def load_daily_loss(self, date: str) -> float:
        with self._lock:
            row = self._conn.execute(
                "SELECT loss_usd FROM daily_loss WHERE date = ?", (date,)
            ).fetchone()
        return float(row["loss_usd"]) if row is not None else 0.0

    def close(self) -> None:
        with self._lock:
            self._conn.close()

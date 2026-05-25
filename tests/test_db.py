from __future__ import annotations

from pathlib import Path

import pytest

from src.db import OrderDB, OrderDBError


def test_order_db_creates_missing_parent_directories(tmp_path: Path) -> None:
    db_path = tmp_path / "missing" / "nested" / "bot.db"

    db = OrderDB(str(db_path))
    try:
        assert db_path.exists()
        assert db.load_orders() == []
    finally:
        db.close()


def test_order_db_reports_corrupt_database(tmp_path: Path) -> None:
    db_path = tmp_path / "corrupt.db"
    db_path.write_bytes(b"this is not a sqlite database")

    with pytest.raises(OrderDBError, match="failed to initialize order database"):
        OrderDB(str(db_path))

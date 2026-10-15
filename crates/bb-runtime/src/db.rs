//! Port of `src/db.py`: SQLite persistence for orders and fills.
//!
//! The orders/fills schema is byte-for-byte the Python one so an existing
//! `data/bot.db` opens unchanged. Money columns are SQLite `REAL` (as written
//! by Python); values are converted at this boundary only.

use parking_lot::{Mutex, MutexGuard};
use std::path::Path;
use std::str::FromStr;

use bb_core::Decimal;
use rusqlite::{Connection, params};

use crate::orders::models::{Fill, Order, OrderSide, OrderStatus, OrderType};
use crate::orders::payload::{iso_naive, parse_iso_datetime};

const SCHEMA: &str = "
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
";

#[derive(Debug, thiserror::Error)]
pub enum OrderDbError {
    #[error("failed to open order database: {path}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("failed to initialize order database: {path}")]
    Initialize {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("failed to create order database directory: {path}")]
    Directory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to load orders from database")]
    LoadOrders(#[source] rusqlite::Error),
    #[error("invalid {field} value {value:?} in order database")]
    Corrupt { field: &'static str, value: String },
    #[error("order database write failed")]
    Write(#[source] rusqlite::Error),
}

/// Thread-safe SQLite store for orders and fills.
#[derive(Debug)]
pub struct OrderDb {
    conn: Mutex<Connection>,
}

impl OrderDb {
    /// Open (creating parent directories and tables as needed).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OrderDbError> {
        let path = path.as_ref();
        let display = path.display().to_string();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| OrderDbError::Directory {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let conn = Connection::open(path).map_err(|source| OrderDbError::Open {
            path: display.clone(),
            source,
        })?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|source| OrderDbError::Open {
                path: display.clone(),
                source,
            })?;
        conn.execute_batch(SCHEMA)
            .map_err(|source| OrderDbError::Initialize {
                path: display,
                source,
            })?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// `INSERT OR REPLACE` the order and `INSERT OR IGNORE` its fills, atomically.
    pub fn upsert_order(&self, order: &Order) -> Result<(), OrderDbError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(OrderDbError::Write)?;
        tx.execute(
            "INSERT OR REPLACE INTO orders
                (id, symbol, type, side, status, amount, price,
                 filled_amount, remaining_amount, avg_fill_price, timestamp)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                order.id,
                order.symbol,
                order.order_type.as_str(),
                order.side.as_str(),
                order.status.as_str(),
                real(order.amount),
                order.price.map(real),
                real(order.filled_amount),
                real(order.remaining_amount),
                order.avg_fill_price.map(real),
                iso_naive(order.timestamp),
            ],
        )
        .map_err(OrderDbError::Write)?;
        for fill in &order.fills {
            tx.execute(
                "INSERT OR IGNORE INTO fills
                    (id, order_id, symbol, side, price, amount, fee, fee_currency, timestamp)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    fill.id,
                    fill.order_id,
                    fill.symbol,
                    fill.side.as_str(),
                    real(fill.price),
                    real(fill.amount),
                    real(fill.fee),
                    fill.fee_currency,
                    iso_naive(fill.timestamp),
                ],
            )
            .map_err(OrderDbError::Write)?;
        }
        tx.commit().map_err(OrderDbError::Write)
    }

    /// Delete an order (its fills cascade).
    pub fn delete_order(&self, order_id: &str) -> Result<(), OrderDbError> {
        self.conn()
            .execute("DELETE FROM orders WHERE id = ?", params![order_id])
            .map(|_| ())
            .map_err(OrderDbError::Write)
    }

    /// Every order (sorted by timestamp text) with its fills.
    pub fn load_orders(&self) -> Result<Vec<Order>, OrderDbError> {
        let conn = self.conn();
        let fill_rows = query_rows(
            &conn,
            "SELECT id, order_id, symbol, side, price, amount, fee, fee_currency, timestamp FROM fills ORDER BY timestamp",
            |row| {
                Ok(RawFill {
                    id: row.get(0)?,
                    order_id: row.get(1)?,
                    symbol: row.get(2)?,
                    side: row.get(3)?,
                    price: row.get(4)?,
                    amount: row.get(5)?,
                    fee: row.get(6)?,
                    fee_currency: row.get(7)?,
                    timestamp: row.get(8)?,
                })
            },
        )?;
        let order_rows = query_rows(
            &conn,
            "SELECT id, symbol, type, side, status, amount, price, filled_amount, remaining_amount, avg_fill_price, timestamp FROM orders ORDER BY timestamp",
            |row| {
                Ok(RawOrder {
                    id: row.get(0)?,
                    symbol: row.get(1)?,
                    order_type: row.get(2)?,
                    side: row.get(3)?,
                    status: row.get(4)?,
                    amount: row.get(5)?,
                    price: row.get(6)?,
                    filled_amount: row.get(7)?,
                    remaining_amount: row.get(8)?,
                    avg_fill_price: row.get(9)?,
                    timestamp: row.get(10)?,
                })
            },
        )?;
        drop(conn);

        let mut fills_by_order: std::collections::HashMap<String, Vec<Fill>> =
            std::collections::HashMap::new();
        for raw in fill_rows {
            let fill = Fill {
                side: parse_enum::<OrderSide>("side", &raw.side)?,
                price: decimal("price", raw.price)?,
                amount: decimal("amount", raw.amount)?,
                fee: decimal("fee", raw.fee)?,
                timestamp: timestamp(&raw.timestamp)?,
                id: raw.id,
                order_id: raw.order_id.clone(),
                symbol: raw.symbol,
                fee_currency: raw.fee_currency,
            };
            fills_by_order.entry(raw.order_id).or_default().push(fill);
        }

        order_rows
            .into_iter()
            .map(|raw| {
                Ok(Order {
                    order_type: parse_enum::<OrderType>("type", &raw.order_type)?,
                    side: parse_enum::<OrderSide>("side", &raw.side)?,
                    status: OrderStatus::from_str(&raw.status).map_err(|_| {
                        OrderDbError::Corrupt {
                            field: "status",
                            value: raw.status.clone(),
                        }
                    })?,
                    amount: decimal("amount", raw.amount)?,
                    price: raw.price.map(|p| decimal("price", p)).transpose()?,
                    filled_amount: decimal("filled_amount", raw.filled_amount)?,
                    remaining_amount: decimal("remaining_amount", raw.remaining_amount)?,
                    avg_fill_price: raw
                        .avg_fill_price
                        .map(|p| decimal("avg_fill_price", p))
                        .transpose()?,
                    timestamp: timestamp(&raw.timestamp)?,
                    fills: fills_by_order.remove(&raw.id).unwrap_or_default(),
                    id: raw.id,
                    symbol: raw.symbol,
                })
            })
            .collect()
    }
}

struct RawFill {
    id: String,
    order_id: String,
    symbol: String,
    side: String,
    price: f64,
    amount: f64,
    fee: f64,
    fee_currency: String,
    timestamp: String,
}

struct RawOrder {
    id: String,
    symbol: String,
    order_type: String,
    side: String,
    status: String,
    amount: f64,
    price: Option<f64>,
    filled_amount: f64,
    remaining_amount: f64,
    avg_fill_price: Option<f64>,
    timestamp: String,
}

fn query_rows<T>(
    conn: &Connection,
    sql: &str,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, OrderDbError> {
    let mut stmt = conn.prepare(sql).map_err(OrderDbError::LoadOrders)?;
    let rows = stmt
        .query_map([], map)
        .map_err(OrderDbError::LoadOrders)?
        .collect::<rusqlite::Result<Vec<T>>>()
        .map_err(OrderDbError::LoadOrders)?;
    Ok(rows)
}

fn parse_enum<T: FromStr>(field: &'static str, value: &str) -> Result<T, OrderDbError> {
    T::from_str(value).map_err(|_| OrderDbError::Corrupt {
        field,
        value: value.to_owned(),
    })
}

fn timestamp(value: &str) -> Result<chrono::NaiveDateTime, OrderDbError> {
    parse_iso_datetime(value).ok_or_else(|| OrderDbError::Corrupt {
        field: "timestamp",
        value: value.to_owned(),
    })
}

/// `REAL` column value for a `Decimal` (the persisted format is f64).
fn real(value: Decimal) -> f64 {
    bb_core::money::to_f64(value)
}

/// Shortest round-trip decimal for a stored `REAL` (82396.84 stays 82396.84).
pub(crate) fn decimal(field: &'static str, value: f64) -> Result<Decimal, OrderDbError> {
    if !value.is_finite() {
        return Err(OrderDbError::Corrupt {
            field,
            value: value.to_string(),
        });
    }
    Decimal::from_str(&value.to_string())
        .ok()
        .or_else(|| bb_core::money::from_f64(value))
        .ok_or_else(|| OrderDbError::Corrupt {
            field,
            value: value.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orders::models::utc_now;
    use rust_decimal_macros::dec;

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state").join("bot.db");
        let db = OrderDb::open(&path).unwrap();
        assert!(path.exists());
        assert!(db.load_orders().unwrap().is_empty());
    }

    #[test]
    fn reports_corrupt_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bot.db");
        std::fs::write(&path, b"not a sqlite database").unwrap();
        let err = OrderDb::open(&path).unwrap_err();
        assert!(err.to_string().starts_with("failed to"), "{err}");
    }

    #[test]
    fn orders_and_fills_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = OrderDb::open(dir.path().join("bot.db")).unwrap();
        let mut order = Order::new(
            "1",
            "BTC/USDT",
            OrderType::Market,
            OrderSide::Buy,
            dec!(0.001),
        );
        order.status = OrderStatus::Closed;
        order.price = Some(dec!(82396.84));
        order.filled_amount = dec!(0.001);
        order.avg_fill_price = Some(dec!(82396.84));
        order.fills.push(Fill {
            id: "f1".into(),
            order_id: "1".into(),
            symbol: "BTC/USDT".into(),
            side: OrderSide::Buy,
            price: dec!(82396.84),
            amount: dec!(0.001),
            fee: dec!(0),
            fee_currency: "BTC".into(),
            timestamp: utc_now(),
        });
        db.upsert_order(&order).unwrap();
        db.upsert_order(&order).unwrap();
        let loaded = db.load_orders().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].price, Some(dec!(82396.84)));
        assert_eq!(loaded[0].fills.len(), 1);
        assert_eq!(loaded[0].timestamp, order.timestamp.and_utc().naive_utc());

        db.delete_order("1").unwrap();
        assert!(db.load_orders().unwrap().is_empty());
    }
}

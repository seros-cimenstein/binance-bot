//! Module performance: scorecards (refactor `src/capital/scorecard.py`) rebuilt from order fills or
//! from a bot's JSONL ledger (the readers `bot_status._ledger_pnl` and `cash_distributor._sum_ledger_pnl`
//! merged into one).

use std::collections::BTreeMap;
use std::path::Path;

use bb_core::money::to_f64;
use rust_decimal::Decimal;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

/// Ledger fields holding realised P&L, first present wins: funding-arb, sniper paper,
/// mean-reversion / listing sniper.
pub const DEFAULT_PNL_FIELDS: [&str; 3] = ["net_pnl_usdt", "usd_pnl", "pnl_usdt"];
/// Ledger timestamp fields, first non-zero wins.
pub const LEDGER_TIME_FIELDS: [&str; 3] = ["close_time_ms", "exit_time_ms", "timestamp_ms"];

/// Compact performance snapshot for one module / bot.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleScorecard {
    pub module_id: String,
    pub realized_pnl_usdt: Decimal,
    pub trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub max_drawdown_pct: f64,
    pub risk_breaches: u64,
}

impl ModuleScorecard {
    /// An empty (untested) scorecard.
    pub fn new(module_id: impl Into<String>) -> Self {
        Self {
            module_id: module_id.into(),
            realized_pnl_usdt: Decimal::ZERO,
            trades: 0,
            wins: 0,
            losses: 0,
            max_drawdown_pct: 0.0,
            risk_breaches: 0,
        }
    }

    /// Wins over decided trades (wins + losses). For fill-derived scorecards this equals the
    /// refactor's `wins / trades`; ledger rows without realised P&L (fills) do not dilute it.
    pub fn win_rate(&self) -> f64 {
        let decided = self.wins + self.losses;
        if self.trades == 0 || decided == 0 {
            return 0.0;
        }
        (self.wins as f64 / decided as f64).clamp(0.0, 1.0)
    }

    /// Bounded allocation multiplier in `0.1..=3.0`; untested modules score 1.0.
    pub fn score(&self) -> f64 {
        if self.trades == 0 {
            return 1.0;
        }
        let pnl_bonus = (to_f64(self.realized_pnl_usdt) / 100.0).clamp(-0.75, 0.75);
        // Ledger rows without realised P&L (fills) decide nothing: no win-rate bonus or penalty.
        let win_bonus = if self.wins + self.losses > 0 {
            (self.win_rate() - 0.5) * 0.5
        } else {
            0.0
        };
        let drawdown_penalty = (self.max_drawdown_pct.max(0.0) / 50.0).min(0.75);
        let breach_penalty = (self.risk_breaches as f64 * 0.15).min(0.75);
        let score = 1.0 + pnl_bonus + win_bonus - drawdown_penalty - breach_penalty;
        if score.is_finite() {
            score.clamp(0.1, 3.0)
        } else {
            0.1
        }
    }

    /// Lenient restore from persisted metadata (refactor `safe_scorecard`).
    pub fn from_value(module_id: &str, payload: &Value) -> Self {
        let count =
            |key: &str| value_f64(payload.get(key)).map_or(0, |v| v.max(0.0).trunc() as u64);
        Self {
            module_id: module_id.to_owned(),
            realized_pnl_usdt: value_decimal(payload.get("realized_pnl_usdt")).unwrap_or_default(),
            trades: count("trades"),
            wins: count("wins"),
            losses: count("losses"),
            max_drawdown_pct: value_f64(payload.get("max_drawdown_pct"))
                .unwrap_or(0.0)
                .max(0.0),
            risk_breaches: count("risk_breaches"),
        }
    }

    /// Scorecard view of a ledger summary.
    pub fn from_ledger(module_id: impl Into<String>, stats: &LedgerStats) -> Self {
        Self {
            module_id: module_id.into(),
            realized_pnl_usdt: stats.pnl_usdt,
            trades: stats.trades,
            wins: stats.wins,
            losses: stats.losses,
            max_drawdown_pct: stats.max_drawdown_pct,
            risk_breaches: 0,
        }
    }
}

impl Serialize for ModuleScorecard {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = Map::new();
        map.insert("module_id".into(), Value::String(self.module_id.clone()));
        map.insert(
            "realized_pnl_usdt".into(),
            decimal_value(self.realized_pnl_usdt),
        );
        map.insert("trades".into(), self.trades.into());
        map.insert("wins".into(), self.wins.into());
        map.insert("losses".into(), self.losses.into());
        map.insert("win_rate".into(), f64_value(self.win_rate()));
        map.insert("max_drawdown_pct".into(), f64_value(self.max_drawdown_pct));
        map.insert("risk_breaches".into(), self.risk_breaches.into());
        map.insert("score".into(), f64_value(self.score()));
        Value::Object(map).serialize(serializer)
    }
}

/// One persisted order (fill) used to rebuild scorecards.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderFill {
    pub timestamp_ms: i64,
    pub symbol: String,
    /// `buy` / `sell` (any case).
    pub side: String,
    pub filled_amount: Decimal,
    /// Average fill price (falls back to the order price upstream).
    pub price: Decimal,
    /// Order metadata (`strategy`, `symbol`, `capital_pool.module_id`, `risk_breach`, `risk.breached`).
    pub metadata: Value,
}

/// Module id of an order: `capital_pool.module_id` when stamped, else `<strategy>:<SYMBOL>`.
pub fn module_id_from_metadata(metadata: &Value, symbol: &str) -> String {
    if let Some(id) = metadata
        .pointer("/capital_pool/module_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return id.to_owned();
    }
    let strategy = metadata
        .get("strategy")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let metadata_symbol = metadata
        .get("symbol")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(symbol);
    format!("{strategy}:{}", metadata_symbol.to_uppercase())
}

fn risk_breach_count(metadata: &Value) -> u64 {
    if let Some(breach) = metadata.get("risk_breach").and_then(Value::as_bool) {
        return u64::from(breach);
    }
    let breached = metadata.pointer("/risk/breached").is_some_and(truthy);
    u64::from(breached)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[derive(Default)]
struct Running {
    pnl: Decimal,
    trades: u64,
    wins: u64,
    losses: u64,
    risk_breaches: u64,
    peak: Decimal,
    max_drawdown: Decimal,
}

impl Running {
    fn record_trade(&mut self, pnl: Decimal) {
        self.trades += 1;
        if pnl >= Decimal::ZERO {
            self.wins += 1;
        } else {
            self.losses += 1;
        }
        self.record_pnl(pnl);
    }

    fn record_pnl(&mut self, pnl: Decimal) {
        self.pnl += pnl;
        self.peak = self.peak.max(self.pnl);
        self.max_drawdown = self.max_drawdown.max(self.peak - self.pnl);
    }

    fn max_drawdown_pct(&self) -> f64 {
        let basis = to_f64(self.peak.abs()).max(1.0);
        (to_f64(self.max_drawdown) / basis * 100.0).max(0.0)
    }
}

/// Rebuild per-module scorecards from fills: buys update an average cost per (module, symbol);
/// sells close against it and record realised P&L (win when ≥ 0).
pub fn scorecards_from_orders(orders: &[OrderFill]) -> BTreeMap<String, ModuleScorecard> {
    let mut sorted: Vec<&OrderFill> = orders.iter().collect();
    sorted.sort_by_key(|order| order.timestamp_ms);
    let mut stats: BTreeMap<String, Running> = BTreeMap::new();
    let mut positions: BTreeMap<(String, String), (Decimal, Decimal)> = BTreeMap::new();

    for order in sorted {
        if order.filled_amount <= Decimal::ZERO || order.price <= Decimal::ZERO {
            continue;
        }
        let side = order.side.trim().to_ascii_lowercase();
        if side != "buy" && side != "sell" {
            continue;
        }
        let symbol = order.symbol.trim().to_uppercase();
        if symbol.is_empty() {
            continue;
        }
        let module_id = module_id_from_metadata(&order.metadata, &symbol);
        let entry = stats.entry(module_id.clone()).or_default();
        entry.risk_breaches += risk_breach_count(&order.metadata);

        let (amount, avg) = positions.entry((module_id, symbol)).or_default();
        if side == "buy" {
            let old_notional = *amount * *avg;
            *amount += order.filled_amount;
            *avg = if *amount > Decimal::ZERO {
                (old_notional + order.filled_amount * order.price) / *amount
            } else {
                Decimal::ZERO
            };
            continue;
        }
        if *amount <= Decimal::ZERO {
            continue;
        }
        let closed = (*amount).min(order.filled_amount);
        entry.record_trade(closed * (order.price - *avg));
        *amount = (*amount - closed).max(Decimal::ZERO);
        if *amount <= Decimal::ZERO {
            *avg = Decimal::ZERO;
        }
    }

    stats
        .into_iter()
        .map(|(module_id, running)| {
            let card = ModuleScorecard {
                module_id: module_id.clone(),
                realized_pnl_usdt: running.pnl,
                trades: running.trades,
                wins: running.wins,
                losses: running.losses,
                max_drawdown_pct: running.max_drawdown_pct(),
                risk_breaches: running.risk_breaches,
            };
            (module_id, card)
        })
        .collect()
}

/// Summary of one bot's JSONL ledger.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerStats {
    /// Lifetime realised P&L.
    pub pnl_usdt: Decimal,
    /// Lifetime rows; rows without a P&L field (individual fills) count as trades with 0 P&L.
    pub trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub max_drawdown_pct: f64,
    pub pnl_24h_usdt: Decimal,
    pub trades_24h: u64,
}

/// Read a ledger. Corrupt lines and non-object rows are skipped; a missing file is empty.
/// The P&L of a row is the first of `pnl_fields` that parses as a number.
pub fn read_ledger(path: &Path, pnl_fields: &[&str], now_ms: i64) -> LedgerStats {
    let rows: Vec<Value> = match bb_core::audit::JsonlLog::new(path).read_all() {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ledger unreadable");
            Vec::new()
        }
    };
    ledger_stats(&rows, pnl_fields, now_ms)
}

/// [`read_ledger`] over already parsed rows.
pub fn ledger_stats(rows: &[Value], pnl_fields: &[&str], now_ms: i64) -> LedgerStats {
    let cutoff_ms = now_ms - 24 * 3_600_000;
    let mut running = Running::default();
    let mut stats = LedgerStats::default();
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let pnl = pnl_fields
            .iter()
            .find_map(|field| value_decimal(object.get(*field)));
        let row_ms = row_timestamp_ms(row).unwrap_or(0);
        match pnl {
            Some(pnl) => running.record_trade(pnl),
            None => running.trades += 1,
        }
        if row_ms >= cutoff_ms {
            stats.trades_24h += 1;
            stats.pnl_24h_usdt += pnl.unwrap_or_default();
        }
    }
    stats.pnl_usdt = running.pnl;
    stats.trades = running.trades;
    stats.wins = running.wins;
    stats.losses = running.losses;
    stats.max_drawdown_pct = running.max_drawdown_pct();
    stats
}

/// First non-zero of [`LEDGER_TIME_FIELDS`].
pub fn row_timestamp_ms(row: &Value) -> Option<i64> {
    LEDGER_TIME_FIELDS
        .iter()
        .find_map(|field| value_f64(row.get(*field)).filter(|v| *v != 0.0))
        .map(|v| v as i64)
}

/// Number or numeric string → `Decimal` (exact for JSON number text).
pub fn value_decimal(value: Option<&Value>) -> Option<Decimal> {
    let text = match value? {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_owned(),
        _ => return None,
    };
    Decimal::from_str_exact(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .ok()
}

/// Number or numeric string → finite `f64`.
pub fn value_f64(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    parsed.is_finite().then_some(parsed)
}

/// JSON number carrying the exact decimal text.
pub fn decimal_value(value: Decimal) -> Value {
    serde_json::from_str(&value.normalize().to_string()).unwrap_or(Value::Null)
}

/// JSON number (`null` for NaN/inf).
pub fn f64_value(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;

    fn fill(ts: i64, side: &str, price: Decimal, amount: Decimal) -> OrderFill {
        OrderFill {
            timestamp_ms: ts,
            symbol: "BTC/USDT".into(),
            side: side.into(),
            filled_amount: amount,
            price,
            metadata: json!({"strategy": "spot-sma-crossover", "symbol": "BTC/USDT"}),
        }
    }

    #[test]
    fn scorecards_from_orders_groups_realized_pnl_by_module() {
        let orders = [
            fill(1, "sell", dec!(110), dec!(1)),
            fill(0, "buy", dec!(100), dec!(1)),
        ];
        let cards = scorecards_from_orders(&orders);
        let card = &cards["spot-sma-crossover:BTC/USDT"];
        assert_eq!(card.realized_pnl_usdt, dec!(10));
        assert_eq!(card.trades, 1);
        assert_eq!(card.wins, 1);
    }

    #[test]
    fn stamped_module_id_and_risk_breaches_are_respected() {
        let mut buy = fill(0, "buy", dec!(100), dec!(2));
        buy.metadata =
            json!({"capital_pool": {"module_id": " pool:X "}, "risk": {"breached": true}});
        let mut sell = fill(1, "SELL", dec!(90), dec!(1));
        sell.metadata = json!({"capital_pool": {"module_id": "pool:X"}, "risk_breach": true});
        let cards = scorecards_from_orders(&[buy, sell]);
        let card = &cards["pool:X"];
        assert_eq!(card.risk_breaches, 2);
        assert_eq!(card.losses, 1);
        assert_eq!(card.realized_pnl_usdt, dec!(-10));
        assert!((card.max_drawdown_pct - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn score_is_bounded_and_neutral_for_untested_modules() {
        assert!((ModuleScorecard::new("m").score() - 1.0).abs() < 1e-12);
        let card = ModuleScorecard {
            realized_pnl_usdt: dec!(12),
            trades: 10,
            wins: 7,
            losses: 3,
            ..ModuleScorecard::new("m")
        };
        assert!((card.score() - 1.22).abs() < 1e-9);
        let terrible = ModuleScorecard {
            realized_pnl_usdt: dec!(-500),
            trades: 4,
            losses: 4,
            max_drawdown_pct: 90.0,
            risk_breaches: 9,
            ..ModuleScorecard::new("m")
        };
        assert!((terrible.score() - 0.1).abs() < 1e-12);
    }

    #[test]
    fn scorecard_metadata_round_trips_leniently() {
        let card = ModuleScorecard {
            realized_pnl_usdt: dec!(-1.5),
            trades: 3,
            wins: 1,
            losses: 2,
            max_drawdown_pct: 12.5,
            risk_breaches: 1,
            ..ModuleScorecard::new("a:B")
        };
        let value = serde_json::to_value(&card).unwrap();
        assert!(value.get("score").is_some() && value.get("win_rate").is_some());
        assert_eq!(ModuleScorecard::from_value("a:B", &value), card);
        let loose = ModuleScorecard::from_value(
            "x",
            &json!({"trades": "4", "wins": -2, "max_drawdown_pct": null}),
        );
        assert_eq!(loose.trades, 4);
        assert_eq!(loose.wins, 0);
    }

    #[test]
    fn ledger_reads_first_present_numeric_field_and_counts_fill_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.jsonl");
        std::fs::write(
            &path,
            "{\"usd_pnl\": 5.0}\n{\"net_pnl_usdt\": 2.5}\n{\"other_field\": 99}\n{\"net_pnl_usdt\": \"not-a-num\"}\n",
        )
        .unwrap();
        let stats = read_ledger(&path, &["net_pnl_usdt", "usd_pnl"], 0);
        assert_eq!(stats.pnl_usdt, dec!(7.5));
        assert_eq!(stats.trades, 4);
        assert_eq!(stats.wins, 2);
    }

    #[test]
    fn ledger_missing_file_is_empty_and_corrupt_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_ledger(&dir.path().join("nope.jsonl"), &DEFAULT_PNL_FIELDS, 0),
            LedgerStats::default()
        );
        let path = dir.path().join("l.jsonl");
        std::fs::write(&path, "not valid json\n[1,2]\n{\"net_pnl_usdt\": 1.0}\n").unwrap();
        let stats = read_ledger(&path, &["net_pnl_usdt"], 0);
        assert_eq!(stats.pnl_usdt, dec!(1.0));
        assert_eq!(stats.trades, 1);
    }

    #[test]
    fn ledger_window_uses_close_exit_or_timestamp_and_zero_pnl_is_kept() {
        let now = 100 * 3_600_000;
        let rows = vec![
            json!({"close_time_ms": now - 1000, "net_pnl_usdt": 0.0, "usd_pnl": 9}),
            json!({"exit_time_ms": now - 30 * 3_600_000, "usd_pnl": -2}),
            json!({"timestamp_ms": now - 5, "pnl_usdt": "1.25"}),
        ];
        let stats = ledger_stats(&rows, &DEFAULT_PNL_FIELDS, now);
        // `0.0` is a real P&L (the Python `or` chain wrongly fell through to usd_pnl).
        assert_eq!(stats.pnl_usdt, dec!(-0.75));
        assert_eq!(stats.trades, 3);
        assert_eq!(stats.trades_24h, 2);
        assert_eq!(stats.pnl_24h_usdt, dec!(1.25));
        assert_eq!((stats.wins, stats.losses), (2, 1));
    }
}

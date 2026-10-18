//! Fleet status (port of `src/bot_status.py`): wallet, per-bot activity, state summary, last audit
//! row and ledger P&L. Liveness comes from the bots' own files (state/audit/ledger mtimes and the
//! newest audit timestamp), which works inside Docker; `systemctl --user` is only consulted when a
//! bot has no file at all and the binary exists.

use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use bb_binance::BinanceClient;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Map, Value, json};

use bb_listing::status::{
    LISTING_BOTS, ListingBot, ListingBotKind, read_detector_status, read_ledger_summary,
    read_sniper_status,
};

use crate::scorecard::{
    DEFAULT_PNL_FIELDS, LedgerStats, decimal_value, read_ledger, value_decimal, value_f64,
};
use crate::wallet::{WalletSnapshot, fetch_wallet};

/// Bytes read from the end of an audit file to find its newest row.
const TAIL_BYTES: u64 = 1 << 20;
/// Grace added to twice the expected interval before a bot counts as stale.
const STALE_GRACE_SECONDS: i64 = 300;

/// Where one bot keeps its files (names relative to the data dir; empty = none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BotSpec {
    pub name: &'static str,
    pub state: &'static str,
    pub audit: &'static str,
    pub ledger: &'static str,
    /// Extra files that only signal activity (databases, summaries).
    pub activity: &'static [&'static str],
    /// Shared signal ledgers: only rows whose `source` matches belong to this bot.
    pub audit_source: Option<&'static str>,
    /// Expected cadence of new writes; `None` for event-driven bots.
    pub expected_interval_seconds: Option<i64>,
    /// systemd unit of the pre-Docker deployment (fallback only).
    pub unit: &'static str,
    /// Which reader understands the state and ledger files.
    pub kind: BotKind,
}

/// State/ledger format of a bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotKind {
    /// JSON state + JSONL ledger with a realised-P&L field.
    Generic,
    /// `bb-listing` detector cursor.
    ListingDetector,
    /// `bb-listing` sniper state and fire ledger (paper `pnl_usdt` + live `pnl_quote`).
    ListingSniper,
}

const fn bot(
    name: &'static str,
    state: &'static str,
    audit: &'static str,
    ledger: &'static str,
    unit: &'static str,
) -> BotSpec {
    BotSpec {
        name,
        state,
        audit,
        ledger,
        activity: &[],
        audit_source: None,
        expected_interval_seconds: None,
        unit,
        kind: BotKind::Generic,
    }
}

const fn listing(spec: ListingBot) -> BotSpec {
    BotSpec {
        kind: match spec.kind {
            ListingBotKind::Detector => BotKind::ListingDetector,
            ListingBotKind::Sniper => BotKind::ListingSniper,
        },
        ..bot(spec.name, spec.state, spec.audit, spec.ledger, spec.unit)
    }
}

/// Every bot of the Rust fleet.
pub const KNOWN_BOTS: &[BotSpec] = &[
    BotSpec {
        expected_interval_seconds: Some(86_400),
        ..bot(
            "trend_filter",
            "trend-filter-state.json",
            "trend-filter-decisions.jsonl",
            "trend-filter-ledger.jsonl",
            "binance-trend-filter.timer",
        )
    },
    BotSpec {
        expected_interval_seconds: Some(28_800),
        ..bot(
            "funding_arb",
            "funding-arb-state.json",
            "funding-arb-decisions.jsonl",
            "funding-arb-ledger.jsonl",
            "binance-funding-arb.timer",
        )
    },
    BotSpec {
        activity: &["bot-spot-journal.jsonl"],
        ..bot(
            "bot_spot",
            "bot-spot-agent-state.json",
            "bot-spot-decisions.jsonl",
            "",
            "binance-bot-spot.timer",
        )
    },
    BotSpec {
        expected_interval_seconds: Some(60),
        ..bot(
            "mean_reversion_paper",
            "mean-reversion-state.json",
            "mean-reversion-decisions.jsonl",
            "mean-reversion-ledger.jsonl",
            "binance-mean-reversion-paper.service",
        )
    },
    BotSpec {
        activity: &["pool-paper.db", "pool-paper.db-wal"],
        ..bot("pool_paper", "", "", "", "binance-pool-paper.service")
    },
    BotSpec {
        activity: &["sniper-paper-summary.json"],
        ..bot(
            "sniper_paper",
            "",
            // Each row is a completed round-trip.
            "sniper-paper-ledger.jsonl",
            "sniper-paper-ledger.jsonl",
            "binance-sniper-paper.service",
        )
    },
    BotSpec {
        audit_source: Some("dexscreener"),
        ..bot(
            "signals_dexscreener",
            "",
            "signals-ledger.jsonl",
            "",
            "binance-signals-dexscreener.service",
        )
    },
    bot(
        "signals_dexscreener_launches",
        "",
        "signals-v2-ledger.jsonl",
        "",
        "binance-signals-dexscreener-launches.service",
    ),
    BotSpec {
        audit_source: Some("telegram"),
        ..bot(
            "signals_telegram",
            "",
            "signals-ledger.jsonl",
            "",
            "binance-signals-telegram.service",
        )
    },
    BotSpec {
        audit_source: Some("smart_money"),
        ..bot(
            "signals_smart_money",
            "smart-money-state.json",
            "signals-ledger.jsonl",
            "",
            "binance-signals-smart-money.service",
        )
    },
    BotSpec {
        expected_interval_seconds: Some(120),
        ..listing(LISTING_BOTS[0])
    },
    listing(LISTING_BOTS[1]),
    listing(LISTING_BOTS[2]),
    listing(LISTING_BOTS[3]),
];

/// Registry entry by name.
pub fn bot_spec(name: &str) -> Option<&'static BotSpec> {
    KNOWN_BOTS.iter().find(|b| b.name == name)
}

/// `BOT_STATUS_DATA_DIR`, else `~/.local/share/binance-bot/data`.
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = bb_core::env::optional_string(&["BOT_STATUS_DATA_DIR"]) {
        return PathBuf::from(dir);
    }
    let home = bb_core::env::string(&["HOME"], ".");
    PathBuf::from(home).join(".local/share/binance-bot/data")
}

/// File-based liveness.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActivitySnapshot {
    /// `recent`, `stale`, `seen` (no expected cadence) or `never`.
    pub status: String,
    pub last_activity: Option<String>,
    pub age_seconds: Option<i64>,
    pub expected_interval_seconds: Option<i64>,
    /// The file (or audit row) that proved the activity.
    pub evidence: Option<String>,
}

/// systemd fallback (`systemctl --user show`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimerSnapshot {
    pub unit: String,
    pub active: bool,
    pub enabled: bool,
    pub next_trigger: String,
    pub last_trigger: String,
}

/// One bot's status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BotSnapshot {
    pub name: String,
    pub activity: ActivitySnapshot,
    pub timer: Option<TimerSnapshot>,
    pub state_summary: String,
    pub last_audit: Option<Value>,
    pub trades_lifetime: u64,
    pub trades_24h: u64,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub pnl_lifetime_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub pnl_24h_usdt: Decimal,
}

/// Whole-fleet snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetSnapshot {
    pub timestamp: DateTime<Utc>,
    pub wallet: WalletSnapshot,
    pub bots: Vec<BotSnapshot>,
}

/// Mainnet client from `BINANCE_API_KEY`/`BINANCE_API_SECRET` (the status tool ignored testnet).
pub fn wallet_client() -> Option<BinanceClient> {
    let credentials = match bb_binance::Credentials::from_env() {
        Ok(Some(credentials)) => credentials,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "invalid Binance credentials; wallet skipped");
            return None;
        }
    };
    match BinanceClient::new(bb_binance::BinanceConfig::new(false).with_credentials(credentials)) {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!(%error, "Binance client unavailable; wallet skipped");
            None
        }
    }
}

/// Wallet (when a client is given) plus every [`KNOWN_BOTS`] entry.
pub async fn collect_snapshot(client: Option<&BinanceClient>, data_dir: &Path) -> FleetSnapshot {
    let wallet = match client {
        Some(client) => fetch_wallet(client).await,
        None => WalletSnapshot::default(),
    };
    let now = bb_core::time::now();
    let mut bots = Vec::with_capacity(KNOWN_BOTS.len());
    for spec in KNOWN_BOTS {
        bots.push(bot_snapshot(spec, data_dir, &wallet, now).await);
    }
    FleetSnapshot {
        timestamp: now,
        wallet,
        bots,
    }
}

fn mtime_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// The newest non-empty lines of a file (up to [`TAIL_BYTES`]), newest first.
fn tail_lines(path: &Path) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // The first line may be cut mid-row.
        lines.remove(0);
    }
    lines
        .into_iter()
        .rev()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Newest audit row (matching `source` when set).
pub fn last_audit_row(path: &Path, source: Option<&str>) -> Option<Value> {
    let lines = tail_lines(path);
    match source {
        None => lines
            .first()
            .and_then(|line| serde_json::from_str(line).ok()),
        Some(source) => lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|row| row.get("source").and_then(Value::as_str) == Some(source)),
    }
}

/// Timestamp of an audit row: `timestamp` (ISO), `timestamp_ms`, `detected_at_ms`, `close_time_ms`, `exit_time_ms`.
pub fn audit_timestamp_ms(row: &Value) -> Option<i64> {
    if let Some(text) = row.get("timestamp").and_then(Value::as_str)
        && let Ok(ts) = DateTime::parse_from_rfc3339(text)
    {
        return Some(ts.timestamp_millis());
    }
    [
        "timestamp_ms",
        "detected_at_ms",
        "close_time_ms",
        "exit_time_ms",
        "timestamp",
    ]
    .iter()
    .find_map(|k| value_f64(row.get(*k)).filter(|v| *v > 0.0))
    .map(|v| v as i64)
}

fn fmt_utc(ms: i64) -> String {
    bb_core::time::from_ms(ms).map_or_else(
        || "-".into(),
        |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// Classify liveness from file evidence.
pub fn activity_from(
    evidence: Option<(i64, String)>,
    expected: Option<i64>,
    now_ms: i64,
) -> ActivitySnapshot {
    let Some((ms, source)) = evidence else {
        return ActivitySnapshot {
            status: "never".into(),
            last_activity: None,
            age_seconds: None,
            expected_interval_seconds: expected,
            evidence: None,
        };
    };
    let age = ((now_ms - ms) / 1000).max(0);
    let status = match expected {
        None => "seen",
        Some(interval) if age > 2 * interval + STALE_GRACE_SECONDS => "stale",
        Some(_) => "recent",
    };
    ActivitySnapshot {
        status: status.into(),
        last_activity: bb_core::time::from_ms(ms).map(bb_core::time::iso),
        age_seconds: Some(age),
        expected_interval_seconds: expected,
        evidence: Some(source),
    }
}

async fn bot_snapshot(
    spec: &BotSpec,
    data_dir: &Path,
    wallet: &WalletSnapshot,
    now: DateTime<Utc>,
) -> BotSnapshot {
    let now_ms = now.timestamp_millis();
    let file = |name: &str| (!name.is_empty()).then(|| data_dir.join(name));

    let last_audit = file(spec.audit).and_then(|p| last_audit_row(&p, spec.audit_source));
    let state_summary = file(spec.state)
        .map(|p| state_summary(spec, &p, wallet))
        .unwrap_or_default();
    let ledger = ledger_stats_for(spec, data_dir, now_ms);

    // Shared signal ledgers prove activity only through this bot's own rows.
    let mut evidence: Option<(i64, String)> = None;
    let mut consider = |ms: Option<i64>, label: String| {
        if let Some(ms) = ms
            && evidence.as_ref().is_none_or(|(best, _)| ms > *best)
        {
            evidence = Some((ms, label));
        }
    };
    if let Some(row) = &last_audit {
        consider(audit_timestamp_ms(row), format!("{} row", spec.audit));
    }
    let own_files = [spec.state, spec.ledger]
        .into_iter()
        .chain(spec.audit_source.is_none().then_some(spec.audit))
        .chain(spec.activity.iter().copied());
    let mut any_file = last_audit.is_some();
    for name in own_files.filter(|n| !n.is_empty()) {
        let path = data_dir.join(name);
        let ms = mtime_ms(&path);
        any_file |= ms.is_some();
        consider(ms, name.to_owned());
    }
    let timer = if any_file {
        None
    } else {
        systemd_timer(spec.unit).await
    };

    BotSnapshot {
        name: spec.name.into(),
        activity: activity_from(evidence, spec.expected_interval_seconds, now_ms),
        timer,
        state_summary,
        last_audit,
        trades_lifetime: ledger.trades,
        trades_24h: ledger.trades_24h,
        pnl_lifetime_usdt: ledger.pnl_usdt,
        pnl_24h_usdt: ledger.pnl_24h_usdt,
    }
}

fn state_summary(spec: &BotSpec, path: &Path, wallet: &WalletSnapshot) -> String {
    let summary = match spec.kind {
        BotKind::ListingDetector => read_detector_status(path).map(|s| s.map(|s| s.summary())),
        BotKind::ListingSniper => read_sniper_status(path).map(|s| s.map(|s| s.summary())),
        BotKind::Generic => {
            return match std::fs::read(path) {
                Ok(bytes) => {
                    let state =
                        serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| json!({}));
                    summarize_state(spec.name, &state, wallet)
                }
                Err(_) => String::new(),
            };
        }
    };
    summary
        .unwrap_or_else(|error| {
            tracing::warn!(bot = spec.name, %error, "listing state unreadable");
            None
        })
        .unwrap_or_default()
}

/// Lifetime/24h ledger statistics of one bot (listing snipers through `bb-listing`).
pub fn ledger_stats_for(spec: &BotSpec, data_dir: &Path, now_ms: i64) -> LedgerStats {
    if spec.ledger.is_empty() {
        return LedgerStats::default();
    }
    let path = data_dir.join(spec.ledger);
    if spec.kind != BotKind::ListingSniper {
        return read_ledger(&path, &DEFAULT_PNL_FIELDS, now_ms);
    }
    let since = bb_core::time::from_ms(now_ms - 24 * 3_600_000);
    match (
        read_ledger_summary(&path, None),
        read_ledger_summary(&path, since),
    ) {
        (Ok(all), Ok(day)) => LedgerStats {
            pnl_usdt: all.pnl_usdt,
            trades: all.fires as u64,
            pnl_24h_usdt: day.pnl_usdt,
            trades_24h: day.fires as u64,
            ..LedgerStats::default()
        },
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(bot = spec.name, %error, "listing ledger unreadable");
            LedgerStats::default()
        }
    }
}

fn number(value: Option<&Value>) -> Decimal {
    value_decimal(value).unwrap_or_default()
}

/// One-line state summary (trend-filter holdings, funding-arb positions).
pub fn summarize_state(name: &str, state: &Value, wallet: &WalletSnapshot) -> String {
    match name {
        "trend_filter" => {
            let holdings = state
                .get("holdings")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if holdings.is_empty() {
                return "no holdings (cash position)".into();
            }
            holdings
                .iter()
                .map(|h| {
                    let symbol = h.get("symbol").and_then(Value::as_str).unwrap_or("");
                    let qty = number(h.get("qty"));
                    let entry = number(h.get("entry_price"));
                    let asset = symbol.split('/').next().unwrap_or("");
                    let price = wallet.last_price(symbol).unwrap_or_default();
                    let pnl_pct = if entry > Decimal::ZERO && price > Decimal::ZERO {
                        (price / entry - Decimal::ONE) * Decimal::ONE_HUNDRED
                    } else {
                        Decimal::ZERO
                    };
                    let value = if price > Decimal::ZERO {
                        qty * price
                    } else {
                        Decimal::ZERO
                    };
                    format!(
                        "{:.6} {asset} (entry ${:.2}, now ${:.2}, {}%, value ${:.4})",
                        Fx(qty),
                        Fx(entry),
                        Fx(price),
                        signed(pnl_pct, 2),
                        Fx(value)
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
        "funding_arb" => {
            let positions = state
                .get("positions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if positions.is_empty() {
                return "no open positions".into();
            }
            let symbols: Vec<&str> = positions
                .iter()
                .map(|p| p.get("symbol").and_then(Value::as_str).unwrap_or(""))
                .collect();
            format!("{} open: {}", positions.len(), symbols.join(", "))
        }
        _ => String::new(),
    }
}

/// `{value:+.dp}`.
pub fn signed(value: Decimal, dp: usize) -> String {
    if value >= Decimal::ZERO {
        format!("+{:.dp$}", Fx(value))
    } else {
        format!("{:.dp$}", Fx(value))
    }
}

/// Display adapter that rounds a `Decimal` to the format precision (`rust_decimal` truncates).
#[derive(Debug, Clone, Copy)]
pub struct Fx(pub Decimal);

impl std::fmt::Display for Fx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match f.precision() {
            Some(dp) => {
                let rounded = self.0.round_dp_with_strategy(
                    u32::try_from(dp).unwrap_or(u32::MAX),
                    rust_decimal::RoundingStrategy::MidpointNearestEven,
                );
                std::fmt::Display::fmt(&rounded, f)
            }
            None => std::fmt::Display::fmt(&self.0, f),
        }
    }
}

/// Query a systemd user unit; `None` when systemctl is missing (Docker) or fails.
async fn systemd_timer(unit: &str) -> Option<TimerSnapshot> {
    if unit.is_empty() {
        return None;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("systemctl")
            .args([
                "--user",
                "show",
                unit,
                "-p",
                "ActiveState,UnitFileState,NextElapseUSecRealtime,LastTriggerUSec",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
            .unwrap_or("")
            .to_owned()
    };
    let unit_file_state = field("UnitFileState");
    if unit_file_state.is_empty() || unit_file_state == "not-found" {
        return None;
    }
    Some(TimerSnapshot {
        unit: unit.into(),
        active: field("ActiveState") == "active",
        enabled: unit_file_state == "enabled",
        next_trigger: field("NextElapseUSecRealtime"),
        last_trigger: field("LastTriggerUSec"),
    })
}

fn format_age(seconds: i64) -> String {
    match seconds {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{}h", s / 86_400, (s % 86_400) / 3600),
    }
}

fn status_chip(bot: &BotSnapshot) -> &'static str {
    if let Some(timer) = &bot.timer {
        return match (timer.active, timer.enabled) {
            (true, true) => "[RUNNING]",
            (false, true) => "[ENABLED, idle]",
            (true, false) => "[RUNNING, not persistent]",
            (false, false) => "[STOPPED]",
        };
    }
    match bot.activity.status.as_str() {
        "recent" => "[RUNNING]",
        "stale" => "[STALE]",
        "seen" => "[SEEN]",
        _ => "[NO DATA]",
    }
}

/// One-screen plain-text block.
pub fn render_plaintext(snapshot: &FleetSnapshot) -> String {
    let wallet = &snapshot.wallet;
    let rule = "=".repeat(72);
    let mut out = String::new();
    let _ = writeln!(out, "{rule}");
    let _ = writeln!(
        out,
        "  BINANCE BOT STATUS  ({})",
        snapshot.timestamp.format("%Y-%m-%d %H:%M:%S UTC")
    );
    let _ = writeln!(out, "{rule}\n");
    let _ = writeln!(out, "WALLET");
    let _ = writeln!(
        out,
        "  Spot USDT free:    ${:.4}",
        Fx(wallet.spot_usdt_free)
    );
    let _ = writeln!(
        out,
        "  Futures USDT free: ${:.4}",
        Fx(wallet.futures_usdt_free)
    );
    let dust_floor = Decimal::new(1, 1);
    let mut nonzero: Vec<_> = wallet
        .positions
        .iter()
        .filter(|p| p.asset != "USDT" && p.value_usdt >= dust_floor)
        .collect();
    nonzero.sort_by_key(|p| std::cmp::Reverse(p.value_usdt));
    if !nonzero.is_empty() {
        let _ = writeln!(out, "  Positions:");
        for p in nonzero {
            let _ = writeln!(
                out,
                "    {:8}  {:>18}  ≈ ${:.4}",
                p.asset,
                format!("{:.8}", Fx(p.qty)),
                Fx(p.value_usdt)
            );
        }
    }
    let dust = wallet
        .positions
        .iter()
        .filter(|p| p.asset != "USDT" && p.value_usdt > Decimal::ZERO && p.value_usdt < dust_floor)
        .count();
    if dust > 0 {
        let _ = writeln!(out, "  +{dust} dust positions (<$0.10 each)");
    }
    let _ = writeln!(
        out,
        "  TOTAL:             ${:.4}\n",
        Fx(wallet.total_value_usdt)
    );

    let _ = writeln!(out, "BOTS");
    for bot in &snapshot.bots {
        let _ = writeln!(out, "  {:18} {}", bot.name, status_chip(bot));
        let activity = &bot.activity;
        if let (Some(age), Some(evidence)) = (activity.age_seconds, &activity.evidence) {
            let last = activity
                .last_activity
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map_or_else(|| "-".into(), |t| fmt_utc(t.timestamp_millis()));
            let cadence = activity
                .expected_interval_seconds
                .map(|s| format!(", expected every {}", format_age(s)))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "    activity: last={last} ({} ago{cadence}; {evidence})",
                format_age(age)
            );
        }
        if let Some(timer) = &bot.timer {
            let _ = writeln!(
                out,
                "    timer: next={}, last={}",
                format_usec(&timer.next_trigger),
                format_usec(&timer.last_trigger)
            );
        }
        if !bot.state_summary.is_empty() {
            let _ = writeln!(out, "    state: {}", bot.state_summary);
        }
        if let Some(audit) = &bot.last_audit {
            let mode = text_field(audit, &["mode", "listing_type"]);
            let ts = match audit
                .get("timestamp")
                .or_else(|| audit.get("detected_at_ms"))
            {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n
                    .as_i64()
                    .and_then(bb_core::time::from_ms)
                    .map_or_else(|| n.to_string(), |t| t.to_rfc3339()),
                _ => String::new(),
            };
            let _ = writeln!(out, "    last cycle: {ts} (mode={mode})");
        }
        if bot.trades_lifetime > 0 {
            let _ = writeln!(
                out,
                "    trades: {} lifetime, {} in 24h",
                bot.trades_lifetime, bot.trades_24h
            );
            let _ = writeln!(
                out,
                "    P&L:    ${} lifetime, ${} 24h",
                signed(bot.pnl_lifetime_usdt, 4),
                signed(bot.pnl_24h_usdt, 4)
            );
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_owned()
}

fn text_field(row: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| match row.get(*k) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Null | Value::String(_)) | None => None,
            Some(other) => Some(other.to_string()),
        })
        .unwrap_or_default()
}

fn format_usec(usec: &str) -> String {
    match usec.parse::<i64>() {
        Ok(v) if v > 0 => fmt_utc(v / 1000),
        Ok(_) => "-".into(),
        Err(_) if usec.is_empty() => "-".into(),
        Err(_) => usec.into(),
    }
}

/// JSON document (Python layout plus `activity`).
pub fn render_json(snapshot: &FleetSnapshot) -> Value {
    let wallet = &snapshot.wallet;
    let mut bots = Vec::with_capacity(snapshot.bots.len());
    for bot in &snapshot.bots {
        let mut row = Map::new();
        row.insert("name".into(), json!(bot.name));
        row.insert("activity".into(), json!(bot.activity));
        row.insert("timer".into(), json!(bot.timer));
        row.insert("state_summary".into(), json!(bot.state_summary));
        row.insert(
            "last_audit".into(),
            bot.last_audit.clone().unwrap_or(Value::Null),
        );
        row.insert("trades_lifetime".into(), json!(bot.trades_lifetime));
        row.insert("trades_24h".into(), json!(bot.trades_24h));
        row.insert(
            "pnl_lifetime_usdt".into(),
            decimal_value(bot.pnl_lifetime_usdt),
        );
        row.insert("pnl_24h_usdt".into(), decimal_value(bot.pnl_24h_usdt));
        bots.push(Value::Object(row));
    }
    json!({
        "timestamp": snapshot.timestamp.to_rfc3339(),
        "wallet": {
            "spot_usdt_free": decimal_value(wallet.spot_usdt_free),
            "futures_usdt_free": decimal_value(wallet.futures_usdt_free),
            "positions": wallet.positions,
            "total_value_usdt": decimal_value(wallet.total_value_usdt),
        },
        "bots": bots,
    })
}

/// Send the rendered snapshot to Telegram (`<pre>` block); `false` when disabled or failed.
pub async fn send_telegram_status(message: &str) -> bool {
    match bb_core::telegram::Telegram::from_env() {
        Some(telegram) => telegram.send_text(message, true).await,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rust_decimal_macros::dec;

    use super::*;

    fn wallet() -> WalletSnapshot {
        WalletSnapshot::from_parts(
            &[
                ("USDT".into(), dec!(10), dec!(10)),
                ("BTC".into(), dec!(0.001), dec!(0.001)),
            ],
            BTreeMap::from([("BTCUSDT".to_owned(), dec!(110000))]),
            dec!(2),
        )
    }

    #[test]
    fn registry_covers_the_rust_fleet() {
        for name in [
            "bot_spot",
            "funding_arb",
            "trend_filter",
            "mean_reversion_paper",
            "pool_paper",
            "sniper_paper",
            "signals_dexscreener",
            "signals_telegram",
            "signals_smart_money",
            "signals_dexscreener_launches",
            "listing_detector",
            "listing_sniper_flat",
            "listing_sniper_trailing",
            "listing_sniper_momentum",
        ] {
            assert!(bot_spec(name).is_some(), "{name}");
        }
    }

    #[test]
    fn state_summaries_match_the_python_wording() {
        let w = wallet();
        let trend =
            json!({"holdings": [{"symbol": "BTC/USDT", "qty": 0.001, "entry_price": 100000}]});
        assert_eq!(
            summarize_state("trend_filter", &trend, &w),
            "0.001000 BTC (entry $100000.00, now $110000.00, +10.00%, value $110.0000)"
        );
        assert_eq!(
            summarize_state("trend_filter", &json!({}), &w),
            "no holdings (cash position)"
        );
        let arb = json!({"positions": [{"symbol": "ETH/USDT"}, {"symbol": "SOL/USDT"}]});
        assert_eq!(
            summarize_state("funding_arb", &arb, &w),
            "2 open: ETH/USDT, SOL/USDT"
        );
        assert_eq!(
            summarize_state("funding_arb", &json!({"positions": []}), &w),
            "no open positions"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("listing-detector-state.json");
        std::fs::write(
            &path,
            r#"{"last_seen_article_id": 271828, "last_poll_time_ms": 1}"#,
        )
        .unwrap();
        let detector = bot_spec("listing_detector").unwrap();
        assert!(state_summary(detector, &path, &w).contains("271828"));
        assert_eq!(
            state_summary(detector, &dir.path().join("missing.json"), &w),
            ""
        );
        assert_eq!(summarize_state("bot_spot", &json!({}), &w), "");
    }

    #[test]
    fn activity_status_uses_expected_cadence() {
        let now = 10_000_000_000;
        assert_eq!(activity_from(None, Some(60), now).status, "never");
        assert_eq!(
            activity_from(Some((now - 30_000, "f".into())), Some(60), now).status,
            "recent"
        );
        assert_eq!(
            activity_from(Some((now - 3_600_000, "f".into())), Some(60), now).status,
            "stale"
        );
        let seen = activity_from(Some((now - 3_600_000, "f".into())), None, now);
        assert_eq!(
            (seen.status.as_str(), seen.age_seconds),
            ("seen", Some(3600))
        );
    }

    #[test]
    fn audit_rows_and_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signals-ledger.jsonl");
        std::fs::write(
            &path,
            "{\"source\":\"telegram\",\"detected_at_ms\":5}\n{\"source\":\"dexscreener\",\"detected_at_ms\":7}\n\nbroken\n",
        )
        .unwrap();
        assert_eq!(
            last_audit_row(&path, Some("telegram")).unwrap()["detected_at_ms"],
            5
        );
        assert!(last_audit_row(&path, None).is_none());
        assert!(last_audit_row(&path, Some("smart_money")).is_none());
        assert_eq!(
            audit_timestamp_ms(&json!({"timestamp": "2026-10-11T00:00:00+00:00"})),
            Some(1_791_676_800_000)
        );
        assert_eq!(audit_timestamp_ms(&json!({"timestamp_ms": 12})), Some(12));
        assert_eq!(audit_timestamp_ms(&json!({})), None);
    }

    #[tokio::test]
    async fn collect_snapshot_reads_state_audit_ledger_and_activity() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let now_ms = bb_core::time::now_ms();
        std::fs::write(
            data.join("funding-arb-ledger.jsonl"),
            format!(
                "{{\"close_time_ms\": {}, \"net_pnl_usdt\": 1.5}}\n{{\"close_time_ms\": 1, \"net_pnl_usdt\": -0.5}}\n",
                now_ms - 1000
            ),
        )
        .unwrap();
        std::fs::write(data.join("funding-arb-state.json"), "{\"positions\": []}").unwrap();
        std::fs::write(
            data.join("funding-arb-decisions.jsonl"),
            format!(
                "{{\"mode\": \"plan_only\", \"timestamp\": \"{}\"}}\n",
                bb_core::time::iso(bb_core::time::now())
            ),
        )
        .unwrap();
        let snapshot = collect_snapshot(None, data).await;
        let arb = snapshot
            .bots
            .iter()
            .find(|b| b.name == "funding_arb")
            .unwrap();
        assert_eq!(arb.pnl_lifetime_usdt, dec!(1.0));
        assert_eq!((arb.trades_lifetime, arb.trades_24h), (2, 1));
        assert_eq!(arb.pnl_24h_usdt, dec!(1.5));
        assert_eq!(arb.state_summary, "no open positions");
        assert_eq!(arb.activity.status, "recent");
        assert_eq!(arb.last_audit.as_ref().unwrap()["mode"], "plan_only");

        let text = render_plaintext(&snapshot);
        assert!(text.contains("BINANCE BOT STATUS"));
        assert!(text.contains("funding_arb        [RUNNING]"));
        assert!(text.contains("P&L:    $+1.0000 lifetime, $+1.5000 24h"));
        assert!(text.contains("last cycle:") && text.contains("mode=plan_only"));
        let json = render_json(&snapshot);
        assert_eq!(json["bots"].as_array().unwrap().len(), KNOWN_BOTS.len());
        assert_eq!(json["wallet"]["total_value_usdt"], json!(0));
    }
}

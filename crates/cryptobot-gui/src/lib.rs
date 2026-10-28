//! `cryptobot-gui`: read-only, localhost-only web dashboard for the bot fleet.
//!
//! One embedded page plus GET endpoints built from what the bots already write and from
//! signed Binance GET requests. Nothing here writes a file or calls an order, transfer or
//! withdrawal endpoint.
//!
//! - `GET /`: the dashboard page.
//! - `GET /api/status`: Bot Spot stop file, failed-route halt, agent state, recent cycles,
//!   the /biz/ feed the newest cycle read (Phase 51) and journal notes.
//! - `GET /api/history`: every Bot Spot trade with its P&L ([`history`]).
//! - `GET /api/wallet`: balances, Binance wallet totals, account and API-key permissions
//!   ([`wallet`]).
//! - `GET /api/fleet`: `bb bot-status --json` bots (activity, trades, P&L).
//! - `GET /api/exits`: Phase 45 exit plans: settings, running plans with their sell lines,
//!   plans waiting for the watcher and the newest watch-log events ([`exits`]).
//! - `GET /api/sessions`: results per trading session (setup), ranked, with the winner and
//!   each session's configuration ([`sessions`]).
//! - `GET /api/signals`: Phase 43 X insighter (paper only): collector health, focus, traction
//!   curves, Haiku takes, recent signals and the weekly report ([`signals`]).
//! - `GET /api/market`: market mood, bearish to bullish, from the breadth of the most-traded
//!   USDT coins, with its last 24 hours ([`market`]; public market data).
//!
//! Every request must address `localhost`, `127.0.0.1` or `[::1]` in its `Host` header, so a
//! web page that rebinds its DNS name to 127.0.0.1 cannot read the data.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod exits;
pub mod history;
pub mod market;
pub mod sessions;
pub mod signals;
pub mod wallet;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use bb_binance::BinanceClient;
use bb_capital::status::{collect_snapshot, render_json};
use bb_signals::x_insighter::schema::XPaths;
use bb_trader::exit_book::ExitPaths;
use bb_trader::audit::failed_route_halt;
use bb_trader::config::{
    DEFAULT_AUDIT_PATH, DEFAULT_JOURNAL_PATH, DEFAULT_STATE_PATH, DEFAULT_STOP_PATH,
};
use bb_trader::journal::{AgentState, Journal};
use bb_trader::memory::load_audit_rows;
use serde_json::{Value, json};

use crate::history::{ParsedAudit, parse_audit};
use bb_trader::trades::build_trades;
use crate::market::MarketView;
use crate::signals::SignalsCache;
use crate::wallet::WalletView;

/// Newest cycle summaries sent to the page.
pub const CYCLE_LIMIT: usize = 120;
/// Newest journal entries sent to the page (it shows 20 and pages through the rest).
pub const JOURNAL_LIMIT: usize = 150;
/// How long one Binance wallet read is reused (the page polls every 30 s).
pub const WALLET_TTL: Duration = Duration::from_secs(20);
/// How long one market-mood read is reused (it moves on 15m candles).
pub const MARKET_TTL: Duration = Duration::from_secs(60);

const INDEX_HTML: &str = include_str!("index.html");

/// The Bot Spot files the dashboard reads (same variables and defaults as the trader).
#[derive(Debug, Clone)]
pub struct BotSpotFiles {
    pub audit_path: PathBuf,
    pub journal_path: PathBuf,
    pub state_path: PathBuf,
    pub stop_path: PathBuf,
    /// Phase 45 exit-plan book, watch state and watch log.
    pub exits: ExitPaths,
}

impl BotSpotFiles {
    /// From `BOT_SPOT_{AUDIT,JOURNAL,STATE,STOP}_PATH`.
    pub fn from_env() -> Self {
        let path =
            |name: &str, default: &str| PathBuf::from(bb_core::env::string(&[name], default));
        Self {
            audit_path: path("BOT_SPOT_AUDIT_PATH", DEFAULT_AUDIT_PATH),
            journal_path: path("BOT_SPOT_JOURNAL_PATH", DEFAULT_JOURNAL_PATH),
            state_path: path("BOT_SPOT_STATE_PATH", DEFAULT_STATE_PATH),
            stop_path: path("BOT_SPOT_STOP_PATH", DEFAULT_STOP_PATH),
            exits: ExitPaths::from_env(),
        }
    }
}

/// Parsed audit log, reused until the file's size or mtime changes.
#[derive(Debug)]
struct AuditCache {
    len: u64,
    modified: Option<SystemTime>,
    parsed: Arc<ParsedAudit>,
}

/// Shared handler state.
#[derive(Debug)]
pub struct AppState {
    /// Fleet data dir (`BOT_STATUS_DATA_DIR` layout).
    data_dir: PathBuf,
    bot_spot: BotSpotFiles,
    /// Mainnet read client; `None` leaves the wallet empty.
    client: Option<BinanceClient>,
    /// Client for public market data: the wallet client, or an unauthenticated one.
    market_client: Option<BinanceClient>,
    /// Held across the Binance read so concurrent requests share one fetch.
    wallet_cache: tokio::sync::Mutex<Option<(Instant, Arc<WalletView>)>>,
    market_cache: tokio::sync::Mutex<Option<(Instant, Arc<MarketView>)>>,
    audit_cache: Mutex<Option<AuditCache>>,
    /// Phase 43 files, in the fleet data dir.
    x_paths: XPaths,
    signals_cache: Mutex<SignalsCache>,
}

impl AppState {
    pub fn new(
        data_dir: PathBuf,
        bot_spot: BotSpotFiles,
        client: Option<BinanceClient>,
    ) -> Self {
        Self {
            x_paths: XPaths::in_dir(&data_dir),
            data_dir,
            bot_spot,
            market_client: client.clone().or_else(public_client),
            client,
            wallet_cache: tokio::sync::Mutex::new(None),
            market_cache: tokio::sync::Mutex::new(None),
            audit_cache: Mutex::new(None),
            signals_cache: Mutex::new(SignalsCache::default()),
        }
    }

    async fn wallet(&self) -> Arc<WalletView> {
        let mut cache = self.wallet_cache.lock().await;
        if let Some((at, view)) = cache.as_ref()
            && at.elapsed() < WALLET_TTL
        {
            return view.clone();
        }
        let view = Arc::new(wallet::fetch(self.client.as_ref()).await);
        *cache = Some((Instant::now(), view.clone()));
        view
    }

    async fn market(&self) -> Arc<MarketView> {
        let mut cache = self.market_cache.lock().await;
        if let Some((at, view)) = cache.as_ref()
            && at.elapsed() < MARKET_TTL
        {
            return view.clone();
        }
        let view = Arc::new(market::fetch(self.market_client.as_ref()).await);
        *cache = Some((Instant::now(), view.clone()));
        view
    }

    fn audit(&self) -> Arc<ParsedAudit> {
        let path = &self.bot_spot.audit_path;
        let meta = std::fs::metadata(path).ok();
        let len = meta.as_ref().map_or(0, std::fs::Metadata::len);
        let modified = meta.as_ref().and_then(|m| m.modified().ok());
        let mut cache = self
            .audit_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = cache.as_ref()
            && hit.len == len
            && hit.modified == modified
        {
            return hit.parsed.clone();
        }
        let parsed = Arc::new(parse_audit(path));
        *cache = Some(AuditCache {
            len,
            modified,
            parsed: parsed.clone(),
        });
        parsed
    }

    fn signals(&self) -> Value {
        let mut cache = self
            .signals_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        signals::signals_view(&mut cache, &self.x_paths, bb_core::time::now_ms())
    }
}

/// Mainnet client without credentials, for public market data.
fn public_client() -> Option<BinanceClient> {
    BinanceClient::new(bb_binance::BinanceConfig::new(false))
        .inspect_err(|error| tracing::warn!(%error, "public Binance client unavailable"))
        .ok()
}

/// The dashboard: GET routes only, behind the localhost `Host` check.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/history", get(history_handler))
        .route("/api/wallet", get(wallet_handler))
        .route("/api/fleet", get(fleet))
        .route("/api/exits", get(exits_handler))
        .route("/api/signals", get(signals_handler))
        .route("/api/market", get(market_handler))
        .route("/api/sessions", get(sessions_handler))
        .layer(middleware::from_fn(localhost_only))
        .with_state(Arc::new(state))
}

/// Whether a `Host` header value names the loopback interface (port optional).
pub fn is_local_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((ip, port)) if port.is_empty() || port.starts_with(':') => ip,
            _ => return false,
        },
        None => host.split_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1"
}

async fn localhost_only(request: Request, next: Next) -> Response {
    let local = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_local_host);
    if local {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "cryptobot-gui only answers requests addressed to localhost",
        )
            .into_response()
    }
}

/// The page is embedded in the binary; `no-store` makes a rebuilt binary's page show up on
/// the next load instead of a cached copy.
async fn index() -> Response {
    no_store(Html(INDEX_HTML).into_response())
}

async fn status(State(app): State<Arc<AppState>>) -> Response {
    let files = app.bot_spot.clone();
    blocking_json(move || status_view(&files)).await
}

async fn history_handler(State(app): State<Arc<AppState>>) -> Response {
    let wallet = app.wallet().await;
    let state = app.clone();
    let audit = match tokio::task::spawn_blocking(move || state.audit()).await {
        Ok(audit) => audit,
        Err(error) => return internal_error("history", &error),
    };
    json_response(history_view(&audit, &wallet))
}

async fn sessions_handler(State(app): State<Arc<AppState>>) -> Response {
    let wallet = app.wallet().await;
    let state = app.clone();
    blocking_json(move || {
        let audit = state.audit();
        let trades = build_trades(&audit.routes, |asset| wallet.mark(asset), bb_core::time::now());
        sessions::sessions_view(&audit, &trades, &state.bot_spot.exits.log)
    })
    .await
}

async fn wallet_handler(State(app): State<Arc<AppState>>) -> Response {
    let view = app.wallet().await;
    json_response(json!(*view))
}

async fn market_handler(State(app): State<Arc<AppState>>) -> Response {
    let view = app.market().await;
    json_response(json!(*view))
}

async fn fleet(State(app): State<Arc<AppState>>) -> Response {
    // The client prices bot holdings in the state summaries; the wallet itself is the
    // wallet tab's job, so it is dropped from this payload.
    let snapshot = collect_snapshot(app.client.as_ref(), &app.data_dir).await;
    let mut view = render_json(&snapshot);
    if let Some(object) = view.as_object_mut() {
        object.remove("wallet");
    }
    json_response(view)
}

async fn exits_handler(State(app): State<Arc<AppState>>) -> Response {
    let paths = app.bot_spot.exits.clone();
    blocking_json(move || {
        let audit = app.audit();
        exits::exits_view(
            &paths,
            |name| std::env::var(name).ok(),
            &audit.plan_exits,
            bb_core::time::now_ms(),
        )
    })
    .await
}

async fn signals_handler(State(app): State<Arc<AppState>>) -> Response {
    blocking_json(move || app.signals()).await
}

async fn blocking_json(work: impl FnOnce() -> Value + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(work).await {
        Ok(view) => json_response(view),
        Err(error) => internal_error("view", &error),
    }
}

fn internal_error(what: &str, error: &tokio::task::JoinError) -> Response {
    tracing::error!(%error, what, "view task failed");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{what} failed")).into_response()
}

fn json_response(value: Value) -> Response {
    no_store(axum::Json(value).into_response())
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Bot Spot state: stop file, the trader's own failed-route halt verdict, agent state,
/// newest cycle summaries and journal notes (newest first), the /biz/ feed the newest cycle
/// that read one recorded (`biz`: its cycle time and the audit `biz` field, `null` before
/// Phase 51), and where the data came from (`source`: a missing audit file means this host
/// has no Bot Spot data at all).
pub fn status_view(files: &BotSpotFiles) -> Value {
    let audit = load_audit_rows(&files.audit_path);
    let halt = failed_route_halt(&audit);
    let cycles: Vec<Value> = audit
        .rows
        .iter()
        .rev()
        .filter(|row| row.get("row_type").and_then(Value::as_str) == Some("cycle_summary"))
        .take(CYCLE_LIMIT)
        .map(|row| {
            json!({
                "timestamp": row.get("timestamp"),
                "cycle_id": row.get("cycle_id"),
                "status": row.get("status"),
                "mode": row.get("mode"),
                "prompt_version": row.get("prompt_version"),
                "candidate_route_count": row.get("candidate_route_count"),
                "error": row.get("error"),
                "decision": row.get("decision"),
                "agent": row.get("agent"),
                // Phase 51: `llm` or `algo` (absent before, which was the LLM), and the
                // algo rule's verdict.
                "decider": row.get("decider").and_then(Value::as_str).unwrap_or("llm"),
                "algo": row.get("algo"),
            })
        })
        .collect();
    let biz = audit
        .rows
        .iter()
        .rev()
        .filter(|row| row.get("row_type").and_then(Value::as_str) == Some("cycle_summary"))
        .find_map(|row| {
            row.get("biz")
                .map(|feed| json!({"cycle_at": row.get("timestamp"), "feed": feed}))
        });
    let (journal, journal_error) = match Journal::new(&files.journal_path).read(JOURNAL_LIMIT) {
        Ok(mut entries) => {
            entries.reverse();
            (entries, None)
        }
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let audit_meta = std::fs::metadata(&files.audit_path).ok();
    let audit_modified = audit_meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
    json!({
        "source": {
            "audit_path": files.audit_path.display().to_string(),
            "audit_present": audit_meta.is_some(),
            "audit_modified": audit_modified,
        },
        "stop_active": files.stop_path.exists(),
        "stop_path": files.stop_path.display().to_string(),
        "halt": halt,
        "audit_error": audit.error,
        "agent": AgentState::load(&files.state_path),
        "cycles": cycles,
        "biz": biz,
        "journal": journal,
        "journal_error": journal_error,
    })
}

/// Trades (oldest first) with open positions marked from the wallet.
pub fn history_view(audit: &ParsedAudit, wallet: &WalletView) -> Value {
    let trades = build_trades(
        &audit.routes,
        |asset| wallet.mark(asset),
        bb_core::time::now(),
    );
    json!({
        "trades": trades,
        "failed_routes": audit.failed,
        "error": audit.error,
        "marks_error": wallet.error,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn host_check_accepts_only_loopback_names() {
        for host in [
            "localhost",
            "LOCALHOST:8787",
            "127.0.0.1",
            "127.0.0.1:8787",
            "[::1]",
            "[::1]:8787",
        ] {
            assert!(is_local_host(host), "{host} should be accepted");
        }
        for host in [
            "",
            "evil.example",
            "evil.example:8787",
            "localhost.evil.example",
            "127.0.0.1.nip.io",
            "192.168.1.10:8787",
            "[::1]evil",
            "[::2]:8787",
        ] {
            assert!(!is_local_host(host), "{host} should be rejected");
        }
    }

    fn files(dir: &Path) -> BotSpotFiles {
        BotSpotFiles {
            audit_path: dir.join("audit.jsonl"),
            journal_path: dir.join("journal.jsonl"),
            state_path: dir.join("state.json"),
            stop_path: dir.join("bot-spot.stop"),
            exits: ExitPaths {
                plans: dir.join("exit-plans.json"),
                state: dir.join("exit-watch-state.json"),
                log: dir.join("exit-watch.jsonl"),
            },
        }
    }

    fn write_lines(path: &Path, rows: &[Value]) {
        let text: String = rows.iter().map(|row| format!("{row}\n")).collect();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn status_reports_stop_file_unreviewed_failed_route_and_newest_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        write_lines(
            &files.audit_path,
            &[
                json!({"timestamp": "t1", "status": "submitted", "submission_attempted": true, "row_type": "route"}),
                json!({"timestamp": "t2", "status": "held", "row_type": "cycle_summary"}),
                json!({"timestamp": "t3", "status": "failed", "submission_attempted": true, "row_type": "route"}),
                json!({"timestamp": "t4", "status": "held", "row_type": "cycle_summary"}),
            ],
        );
        std::fs::write(&files.stop_path, "").unwrap();

        let view = status_view(&files);

        assert_eq!(view["stop_active"], json!(true));
        assert!(view["halt"].as_str().unwrap().contains("operator review"));
        let cycles: Vec<&Value> = view["cycles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| &c["timestamp"])
            .collect();
        assert_eq!(cycles, [&json!("t4"), &json!("t2")]);
    }

    #[test]
    fn status_clears_halt_after_operator_review_and_without_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());

        let empty = status_view(&files);
        assert_eq!(empty["stop_active"], json!(false));
        assert_eq!(empty["halt"], Value::Null);
        assert_eq!(empty["cycles"], json!([]));
        // No audit file: the page must say this host has no Bot Spot data.
        assert_eq!(empty["source"]["audit_present"], json!(false));
        assert_eq!(empty["source"]["audit_modified"], Value::Null);

        write_lines(
            &files.audit_path,
            &[
                json!({"timestamp": "t1", "status": "failed", "submission_attempted": true}),
                json!({"timestamp": "t2", "status": "reviewed",
                       "operator_review": {"failed_submission_reviewed": true}}),
            ],
        );
        assert_eq!(status_view(&files)["halt"], Value::Null);
        assert_eq!(status_view(&files)["source"]["audit_present"], json!(true));
    }

    #[test]
    fn status_shows_the_biz_feed_of_the_newest_cycle_that_read_one() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        assert_eq!(status_view(&files)["biz"], Value::Null);
        write_lines(
            &files.audit_path,
            &[
                json!({"timestamp": "t1", "row_type": "cycle_summary", "biz": {"status": "unavailable", "reason": "old"}}),
                json!({"timestamp": "t2", "row_type": "cycle_summary", "biz": {"status": "fresh", "coins": {}}}),
                json!({"timestamp": "t3", "row_type": "route", "biz": {"status": "unavailable", "reason": "not a cycle"}}),
                // A skipped cycle never read the feed.
                json!({"timestamp": "t4", "row_type": "cycle_summary"}),
            ],
        );
        let biz = &status_view(&files)["biz"];
        assert_eq!(biz["cycle_at"], json!("t2"));
        assert_eq!(biz["feed"]["status"], json!("fresh"));
    }

    #[test]
    fn audit_cache_reparses_when_the_file_grows() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        let route = json!({"timestamp": "t1", "status": "submitted", "submission_attempted": true});
        write_lines(&files.audit_path, std::slice::from_ref(&route));
        let state = AppState::new(dir.path().into(), files.clone(), None);

        assert_eq!(state.audit().routes.len(), 1);
        write_lines(&files.audit_path, &[route.clone(), route]);
        assert_eq!(state.audit().routes.len(), 2);
    }
}

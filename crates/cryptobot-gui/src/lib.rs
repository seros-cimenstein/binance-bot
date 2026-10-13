//! `cryptobot-gui`: read-only, localhost-only web dashboard for the bot fleet.
//!
//! One embedded page plus GET endpoints built from what the bots already write and from
//! signed Binance GET requests. Nothing here writes a file or calls an order, transfer or
//! withdrawal endpoint.
//!
//! - `GET /`: the dashboard page.
//! - `GET /api/status`: Claude Spot stop file, failed-route halt, agent state, recent cycles
//!   and journal notes.
//! - `GET /api/history`: every Claude Spot trade with its P&L ([`history`]).
//! - `GET /api/wallet`: balances, Binance wallet totals, account and API-key permissions
//!   ([`wallet`]).
//! - `GET /api/fleet`: `bb bot-status --json` bots (activity, trades, P&L).
//!
//! Every request must address `localhost`, `127.0.0.1` or `[::1]` in its `Host` header, so a
//! web page that rebinds its DNS name to 127.0.0.1 cannot read the data.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod history;
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
use bb_trader::audit::failed_route_halt;
use bb_trader::config::{
    DEFAULT_AUDIT_PATH, DEFAULT_JOURNAL_PATH, DEFAULT_STATE_PATH, DEFAULT_STOP_PATH,
};
use bb_trader::journal::{AgentState, Journal};
use bb_trader::memory::load_audit_rows;
use serde_json::{Value, json};

use crate::history::{ParsedAudit, build_trades, parse_audit};
use crate::wallet::WalletView;

/// Newest cycle summaries sent to the page.
pub const CYCLE_LIMIT: usize = 120;
/// Newest journal entries sent to the page.
pub const JOURNAL_LIMIT: usize = 50;
/// How long one Binance wallet read is reused (the page polls every 30 s).
pub const WALLET_TTL: Duration = Duration::from_secs(20);

const INDEX_HTML: &str = include_str!("index.html");

/// The Claude Spot files the dashboard reads (same variables and defaults as the trader).
#[derive(Debug, Clone)]
pub struct ClaudeSpotFiles {
    pub audit_path: PathBuf,
    pub journal_path: PathBuf,
    pub state_path: PathBuf,
    pub stop_path: PathBuf,
}

impl ClaudeSpotFiles {
    /// From `CLAUDE_SPOT_{AUDIT,JOURNAL,STATE,STOP}_PATH`.
    pub fn from_env() -> Self {
        let path = |name: &str, default: &str| PathBuf::from(bb_core::env::string(&[name], default));
        Self {
            audit_path: path("CLAUDE_SPOT_AUDIT_PATH", DEFAULT_AUDIT_PATH),
            journal_path: path("CLAUDE_SPOT_JOURNAL_PATH", DEFAULT_JOURNAL_PATH),
            state_path: path("CLAUDE_SPOT_STATE_PATH", DEFAULT_STATE_PATH),
            stop_path: path("CLAUDE_SPOT_STOP_PATH", DEFAULT_STOP_PATH),
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
    claude_spot: ClaudeSpotFiles,
    /// Mainnet read client; `None` leaves the wallet empty.
    client: Option<BinanceClient>,
    /// Held across the Binance read so concurrent requests share one fetch.
    wallet_cache: tokio::sync::Mutex<Option<(Instant, Arc<WalletView>)>>,
    audit_cache: Mutex<Option<AuditCache>>,
}

impl AppState {
    pub fn new(data_dir: PathBuf, claude_spot: ClaudeSpotFiles, client: Option<BinanceClient>) -> Self {
        Self {
            data_dir,
            claude_spot,
            client,
            wallet_cache: tokio::sync::Mutex::new(None),
            audit_cache: Mutex::new(None),
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

    fn audit(&self) -> Arc<ParsedAudit> {
        let path = &self.claude_spot.audit_path;
        let meta = std::fs::metadata(path).ok();
        let len = meta.as_ref().map_or(0, std::fs::Metadata::len);
        let modified = meta.as_ref().and_then(|m| m.modified().ok());
        let mut cache = self.audit_cache.lock().unwrap_or_else(PoisonError::into_inner);
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
}

/// The dashboard: GET routes only, behind the localhost `Host` check.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/history", get(history_handler))
        .route("/api/wallet", get(wallet_handler))
        .route("/api/fleet", get(fleet))
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
    let files = app.claude_spot.clone();
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

async fn wallet_handler(State(app): State<Arc<AppState>>) -> Response {
    let view = app.wallet().await;
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

/// Claude Spot state: stop file, the trader's own failed-route halt verdict, agent state,
/// newest cycle summaries and journal notes (newest first).
pub fn status_view(files: &ClaudeSpotFiles) -> Value {
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
                "status": row.get("status"),
                "error": row.get("error"),
                "decision": row.get("decision"),
                "agent": row.get("agent"),
            })
        })
        .collect();
    let (journal, journal_error) = match Journal::new(&files.journal_path).read(JOURNAL_LIMIT) {
        Ok(mut entries) => {
            entries.reverse();
            (entries, None)
        }
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    json!({
        "stop_active": files.stop_path.exists(),
        "stop_path": files.stop_path.display().to_string(),
        "halt": halt,
        "audit_error": audit.error,
        "agent": AgentState::load(&files.state_path),
        "cycles": cycles,
        "journal": journal,
        "journal_error": journal_error,
    })
}

/// Trades (oldest first) with open positions marked from the wallet.
pub fn history_view(audit: &ParsedAudit, wallet: &WalletView) -> Value {
    let trades = build_trades(&audit.routes, |asset| wallet.mark(asset), bb_core::time::now());
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

    fn files(dir: &Path) -> ClaudeSpotFiles {
        ClaudeSpotFiles {
            audit_path: dir.join("audit.jsonl"),
            journal_path: dir.join("journal.jsonl"),
            state_path: dir.join("state.json"),
            stop_path: dir.join("claude-spot.stop"),
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
        let cycles: Vec<&Value> = view["cycles"].as_array().unwrap().iter().map(|c| &c["timestamp"]).collect();
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

        write_lines(
            &files.audit_path,
            &[
                json!({"timestamp": "t1", "status": "failed", "submission_attempted": true}),
                json!({"timestamp": "t2", "status": "reviewed",
                       "operator_review": {"failed_submission_reviewed": true}}),
            ],
        );
        assert_eq!(status_view(&files)["halt"], Value::Null);
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

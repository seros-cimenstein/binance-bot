//! Solana new-pool detector via WebSocket `logsSubscribe`.
//!
//! Subscribes to each configured launchpad program and scans the log lines of
//! every notification for instruction markers that indicate a fresh pool /
//! bonding curve. Each hit becomes one [`PoolCreationEvent`].

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use futures::{SinkExt as _, Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, Interval};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::SniperError;

/// Mainnet program IDs for the two highest-volume Solana launchpads.
pub const RAYDIUM_AMM_V4_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const RAYDIUM_LABEL: &str = "raydium_amm_v4";
pub const PUMP_FUN_LABEL: &str = "pump_fun";

pub const DEFAULT_RPC_WS_URL: &str = "wss://api.mainnet-beta.solana.com";
pub const DEFAULT_COMMITMENT: &str = "processed";

/// aiohttp `heartbeat=30` equivalent: ping cadence, and the connection is
/// considered dead after two silent intervals.
const HEARTBEAT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// One launchpad program to watch, with the log substrings that indicate a
/// fresh pool creation (picked so routine swaps never match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedProgram {
    pub label: String,
    pub program_id: String,
    pub init_markers: Vec<String>,
}

/// Raydium AMM v4 + pump.fun with their default init markers.
pub fn default_programs() -> Vec<WatchedProgram> {
    let program = |label: &str, id: &str, markers: &[&str]| WatchedProgram {
        label: label.to_owned(),
        program_id: id.to_owned(),
        init_markers: markers.iter().map(|m| (*m).to_owned()).collect(),
    };
    vec![
        program(
            RAYDIUM_LABEL,
            RAYDIUM_AMM_V4_PROGRAM_ID,
            &["Program log: initialize2", "Program log: Initialize2"],
        ),
        program(
            PUMP_FUN_LABEL,
            PUMP_FUN_PROGRAM_ID,
            &[
                "Program log: Instruction: Create",
                "Program log: Instruction: InitializeMint2",
            ],
        ),
    ]
}

/// One detected pool / bonding-curve creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolCreationEvent {
    pub program_label: String,
    pub program_id: String,
    pub signature: String,
    pub slot: u64,
    pub detected_at_ms: i64,
    pub matched_markers: Vec<String>,
    /// Full log lines of the transaction.
    pub log_excerpt: Vec<String>,
}

/// The subset of `markers` present anywhere in the log lines.
pub fn matches_any<S: AsRef<str>, M: AsRef<str>>(logs: &[S], markers: &[M]) -> Vec<String> {
    let blob = logs
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("\n");
    markers
        .iter()
        .map(AsRef::as_ref)
        .filter(|marker| blob.contains(marker))
        .map(str::to_owned)
        .collect()
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn slot_of(raw: Option<&Value>) -> u64 {
    match raw {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Parse one `logsNotification` payload; `Some` when it is a successful
/// transaction whose logs contain at least one init marker.
pub fn parse_log_notification(
    payload: &Value,
    program: &WatchedProgram,
    detected_at_ms: i64,
) -> Option<PoolCreationEvent> {
    if payload.get("method").and_then(Value::as_str) != Some("logsNotification") {
        return None;
    }
    let result = payload
        .get("params")?
        .as_object()?
        .get("result")?
        .as_object()?;
    let value = result.get("value")?.as_object()?;
    if value.get("err").is_some_and(|err| !err.is_null()) {
        return None; // failed transaction; nothing to detect
    }
    let logs: Vec<String> = value
        .get("logs")?
        .as_array()?
        .iter()
        .map(value_to_string)
        .collect();
    let matched = matches_any(&logs, &program.init_markers);
    if matched.is_empty() {
        return None;
    }
    let signature = value
        .get("signature")
        .filter(|s| !s.is_null())
        .map(value_to_string)
        .unwrap_or_default();
    let slot = slot_of(result.get("context").and_then(|c| c.get("slot")));
    Some(PoolCreationEvent {
        program_label: program.label.clone(),
        program_id: program.program_id.clone(),
        signature,
        slot,
        detected_at_ms,
        matched_markers: matched,
        log_excerpt: logs,
    })
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Live `logsSubscribe` feed. Ends (returns `None`) when the socket closes or errors.
pub struct PoolCreationStream {
    ws: Ws,
    programs: Vec<WatchedProgram>,
    /// JSON-RPC request id → program index, until the server confirms.
    pending_requests: HashMap<u64, usize>,
    /// Server-assigned subscription id → program index.
    subscriptions: HashMap<u64, usize>,
    heartbeat: Interval,
    last_inbound: Instant,
}

impl PoolCreationStream {
    /// Open the WebSocket and subscribe to every program.
    pub async fn connect(
        ws_url: &str,
        programs: Vec<WatchedProgram>,
        commitment: &str,
    ) -> Result<Self, SniperError> {
        let (mut ws, _) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
                .await
                .map_err(|_| {
                    SniperError::Detector(format!("websocket connect to {ws_url} timed out"))
                })??;
        let mut pending_requests = HashMap::new();
        for (index, program) in programs.iter().enumerate() {
            let request_id = index as u64 + 1;
            let request = json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "logsSubscribe",
                "params": [{"mentions": [program.program_id]}, {"commitment": commitment}],
            });
            ws.send(Message::text(request.to_string())).await?;
            pending_requests.insert(request_id, index);
        }
        Ok(Self {
            ws,
            programs,
            pending_requests,
            subscriptions: HashMap::new(),
            heartbeat: tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT),
            last_inbound: Instant::now(),
        })
    }

    /// Next detected pool creation; `None` once the connection is gone.
    pub async fn next_event(&mut self) -> Option<PoolCreationEvent> {
        loop {
            tokio::select! {
                frame = self.ws.next() => {
                    self.last_inbound = Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if let Some(event) = self.handle_text(text.as_str()) {
                                return Some(event);
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => return None,
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            tracing::warn!(error = %err, "sniper websocket error; ending stream");
                            return None;
                        }
                    }
                }
                _ = self.heartbeat.tick() => {
                    if self.last_inbound.elapsed() > HEARTBEAT * 2 {
                        tracing::warn!("sniper websocket silent for two heartbeats; ending stream");
                        return None;
                    }
                    if let Err(err) = self.ws.send(Message::Ping(Vec::new().into())).await {
                        tracing::warn!(error = %err, "sniper websocket ping failed; ending stream");
                        return None;
                    }
                }
            }
        }
    }

    fn handle_text(&mut self, text: &str) -> Option<PoolCreationEvent> {
        let payload: Value = serde_json::from_str(text).ok()?;
        if let (Some(request_id), Some(sub_id)) = (
            payload.get("id").and_then(Value::as_u64),
            payload.get("result").and_then(Value::as_u64),
        ) {
            if let Some(index) = self.pending_requests.remove(&request_id) {
                self.subscriptions.insert(sub_id, index);
            }
            return None;
        }
        if payload.get("method").and_then(Value::as_str) != Some("logsNotification") {
            return None;
        }
        let now = bb_core::time::now_ms();
        let sub_id = payload
            .get("params")
            .and_then(|p| p.get("subscription"))
            .and_then(Value::as_u64);
        match sub_id.and_then(|id| self.subscriptions.get(&id)) {
            Some(&index) => parse_log_notification(&payload, &self.programs[index], now),
            // Unknown subscription: scan all configured programs.
            None => self
                .programs
                .iter()
                .find_map(|program| parse_log_notification(&payload, program, now)),
        }
    }

    pub fn into_stream(self) -> impl Stream<Item = PoolCreationEvent> + Send + Unpin {
        Box::pin(futures::stream::unfold(self, |mut stream| async move {
            stream.next_event().await.map(|event| (event, stream))
        }))
    }
}

/// Result of a detector run (`sniper-detector` stdout).
#[derive(Debug, Clone, Serialize)]
pub struct DetectorSummary {
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub total_events: usize,
    pub by_program_label: BTreeMap<String, usize>,
    pub audit_path: String,
}

pub struct DetectorRun<'a> {
    pub ws_url: &'a str,
    pub audit_path: &'a Path,
    pub emergency_stop_path: &'a Path,
    pub run_duration: Option<Duration>,
    pub max_events: Option<usize>,
}

pub(crate) fn ensure_no_emergency_stop(path: &Path) -> Result<(), SniperError> {
    if bb_core::fsutil::stop_requested(path) {
        return Err(SniperError::Detector(format!(
            "emergency stop file present at {}; refusing to run",
            path.display()
        )));
    }
    Ok(())
}

/// Stream detections into the JSONL audit until the duration / event cap /
/// emergency stop / socket close ends the run.
pub async fn run_detector(run: DetectorRun<'_>) -> Result<DetectorSummary, SniperError> {
    ensure_no_emergency_stop(run.emergency_stop_path)?;
    let started_at_ms = bb_core::time::now_ms();
    let audit = bb_core::audit::JsonlLog::new(run.audit_path);
    let mut total_events = 0usize;
    let mut by_label: BTreeMap<String, usize> = BTreeMap::new();

    let accumulate = async {
        let mut stream =
            PoolCreationStream::connect(run.ws_url, default_programs(), DEFAULT_COMMITMENT).await?;
        while let Some(event) = stream.next_event().await {
            total_events += 1;
            *by_label.entry(event.program_label.clone()).or_default() += 1;
            // No per-event alert: the detector fires ~1/sec on pump.fun.
            audit.append(&event)?;
            if run.max_events.is_some_and(|max| total_events >= max)
                || bb_core::fsutil::stop_requested(run.emergency_stop_path)
            {
                break;
            }
        }
        Ok::<(), SniperError>(())
    };
    match run.run_duration {
        Some(duration) => {
            if let Ok(result) = tokio::time::timeout(duration, accumulate).await {
                result?;
            }
        }
        None => accumulate.await?,
    }

    Ok(DetectorSummary {
        started_at_ms,
        ended_at_ms: bb_core::time::now_ms(),
        total_events,
        by_program_label: by_label,
        audit_path: run.audit_path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(signature: &str, slot: u64, logs: &[&str], err: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "logsNotification",
            "params": {
                "subscription": 1,
                "result": {
                    "context": {"slot": slot},
                    "value": {"signature": signature, "err": err, "logs": logs},
                },
            },
        })
    }

    fn raydium() -> WatchedProgram {
        default_programs().remove(0)
    }

    fn pump() -> WatchedProgram {
        default_programs().remove(1)
    }

    #[test]
    fn matches_any_finds_present_marker() {
        let logs = [
            "Program log: starting",
            "Program log: initialize2 detected here",
            "Program log: done",
        ];
        let matched = matches_any(&logs, &["initialize2", "InitializeNotPresent"]);
        assert_eq!(matched, vec!["initialize2".to_owned()]);
    }

    #[test]
    fn matches_any_empty_when_nothing_matches() {
        assert!(matches_any(&["Program log: swap"], &["initialize2"]).is_empty());
    }

    #[test]
    fn parse_raydium_initialization_yields_event() {
        let payload = notification(
            "5sigRaydium",
            999,
            &[
                "Program 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 invoke [1]",
                "Program log: initialize2: amm_open_time=now",
                "Program 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 success",
            ],
            Value::Null,
        );
        let event = parse_log_notification(&payload, &raydium(), 7).unwrap();
        assert_eq!(event.signature, "5sigRaydium");
        assert_eq!(event.slot, 999);
        assert_eq!(event.program_label, "raydium_amm_v4");
        assert_eq!(event.detected_at_ms, 7);
        assert!(
            event
                .matched_markers
                .iter()
                .any(|m| m.contains("initialize2"))
        );
        assert_eq!(event.log_excerpt.len(), 3);
    }

    #[test]
    fn parse_pump_fun_create_yields_event() {
        let payload = notification(
            "5sigPump",
            1234,
            &[
                "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]",
                "Program log: Instruction: Create",
                "Program log: bonding curve initialized",
            ],
            Value::Null,
        );
        let event = parse_log_notification(&payload, &pump(), 0).unwrap();
        assert_eq!(event.signature, "5sigPump");
        assert_eq!(event.program_label, "pump_fun");
    }

    #[test]
    fn parse_returns_none_for_swap_only_logs() {
        let payload = notification(
            "swapSig",
            1,
            &["Program log: swap_base_in", "Program 675k... success"],
            Value::Null,
        );
        assert!(parse_log_notification(&payload, &raydium(), 0).is_none());
    }

    #[test]
    fn parse_returns_none_for_failed_transactions() {
        let payload = notification(
            "failedSig",
            1,
            &["Program log: initialize2"],
            json!({"InstructionError": [0, "InvalidAccountData"]}),
        );
        assert!(parse_log_notification(&payload, &raydium(), 0).is_none());
    }

    #[test]
    fn parse_returns_none_for_unrelated_or_malformed_payloads() {
        for bad in [
            json!({"jsonrpc": "2.0", "method": "accountNotification", "params": {}}),
            json!(null),
            json!({}),
            json!({"method": "logsNotification"}),
            json!("not a dict"),
            json!({"method": "logsNotification", "params": {"result": {"value": {"logs": "x"}}}}),
        ] {
            assert!(
                parse_log_notification(&bad, &raydium(), 0).is_none(),
                "{bad}"
            );
        }
    }

    #[test]
    fn event_round_trips_as_json() {
        let event = PoolCreationEvent {
            program_label: "raydium_amm_v4".into(),
            program_id: RAYDIUM_AMM_V4_PROGRAM_ID.into(),
            signature: "sig1".into(),
            slot: 42,
            detected_at_ms: 1_000_000,
            matched_markers: vec!["Program log: initialize2".into()],
            log_excerpt: vec!["Program log: initialize2".into()],
        };
        let decoded: Value = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(decoded["signature"], "sig1");
        assert_eq!(
            decoded["matched_markers"],
            json!(["Program log: initialize2"])
        );
    }

    #[tokio::test]
    async fn run_detector_refuses_when_emergency_stop_present() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("stop");
        std::fs::write(&stop, "halt").unwrap();
        let err = run_detector(DetectorRun {
            ws_url: "wss://example.invalid",
            audit_path: &dir.path().join("audit.jsonl"),
            emergency_stop_path: &stop,
            run_duration: Some(Duration::from_millis(100)),
            max_events: None,
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("emergency stop"));
    }
}

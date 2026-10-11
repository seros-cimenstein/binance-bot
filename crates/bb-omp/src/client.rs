//! Low-level RPC client: handshake, request/response correlation, events, host tools.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::codec::{FrameLimits, FrameReader, encode_frame};
use crate::error::{CloseReason, OmpError, Result};
use crate::transport::Transport;
use crate::types::{HostToolHandler, HostToolOutcome};

/// Extension UI methods that block the server until answered; the client cancels them.
const DIALOG_METHODS: &[&str] = &["select", "confirm", "input", "editor", "ask"];

/// Connection options.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Max wait for the `ready` frame and protocol negotiation.
    pub handshake_timeout: Duration,
    /// Negotiate protocol v2 (chunked lossless frames) when the server offers it.
    pub negotiate_v2: bool,
    /// Broadcast channel capacity for events.
    pub event_capacity: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(60),
            negotiate_v2: true,
            event_capacity: 4096,
        }
    }
}

/// An unsolicited server frame (session event, prompt result, host request, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct RpcEvent {
    /// The frame `type`.
    pub kind: String,
    pub frame: Map<String, Value>,
}

impl RpcEvent {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.frame.get(key)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.frame.get(key).and_then(Value::as_str)
    }
}

type Writer = Box<dyn AsyncWrite + Send + Unpin>;
type Pending = HashMap<String, oneshot::Sender<Map<String, Value>>>;

struct Shared {
    writer: tokio::sync::Mutex<Option<Writer>>,
    limits: FrameLimits,
    pending: Mutex<Pending>,
    host_calls: Mutex<HashMap<String, CancellationToken>>,
    handler: RwLock<Option<Arc<dyn HostToolHandler>>>,
    events: broadcast::Sender<Arc<RpcEvent>>,
    closed: watch::Sender<Option<CloseReason>>,
    next_id: AtomicU64,
    shutdown: CancellationToken,
}

impl Shared {
    fn disconnected(&self) -> OmpError {
        match self.closed.borrow().as_ref() {
            Some(reason) => OmpError::disconnected(reason),
            None => OmpError::disconnected("connection closed"),
        }
    }

    async fn send(&self, frame: &Value) -> Result<()> {
        let bytes = encode_frame(frame, self.limits)?;
        let mut guard = self.writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| OmpError::disconnected("stdin closed"))?;
        let written = async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        }
        .await;
        written.map_err(|error| OmpError::disconnected(format!("stdin write failed: {error}")))
    }

    fn close(&self, reason: CloseReason) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        self.closed.send_replace(Some(reason));
        // Dropping the senders fails every outstanding request.
        pending.clear();
        drop(pending);
        let calls = std::mem::take(
            &mut *self
                .host_calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for token in calls.into_values() {
            token.cancel();
        }
    }
}

/// A connected omp RPC client. Dropping it stops the reader and cancels host tool calls.
pub struct RpcClient {
    shared: Arc<Shared>,
    reader: JoinHandle<()>,
    protocol_version: u32,
}

impl RpcClient {
    /// Performs the handshake (`ready`, optional v2 negotiation) and starts the reader task.
    pub async fn connect(transport: Transport, options: ClientOptions) -> Result<Self> {
        let Transport { reader, writer } = transport;
        let mut frames = FrameReader::new(reader, FrameLimits::default());
        let ready = tokio::time::timeout(options.handshake_timeout, read_ready(&mut frames))
            .await
            .map_err(|_| {
                OmpError::Handshake(format!(
                    "no ready frame within {:?}",
                    options.handshake_timeout
                ))
            })??;
        let limit = |key: &str, default: usize| {
            ready
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value > 0)
                .unwrap_or(default)
        };
        let defaults = FrameLimits::default();
        let limits = FrameLimits {
            max_frame_bytes: limit("maxFrameBytes", defaults.max_frame_bytes),
            max_reassembled_bytes: limit(
                "maxReassembledFrameBytes",
                defaults.max_reassembled_bytes,
            ),
        };
        frames.set_limits(limits);
        let supports_v2 = ready
            .get("supportedProtocolVersions")
            .and_then(Value::as_array)
            .is_some_and(|versions| versions.iter().any(|v| v.as_u64() == Some(2)));

        let (events, _) = broadcast::channel(options.event_capacity.max(16));
        let shared = Arc::new(Shared {
            writer: tokio::sync::Mutex::new(Some(writer)),
            limits,
            pending: Mutex::new(HashMap::new()),
            host_calls: Mutex::new(HashMap::new()),
            handler: RwLock::new(None),
            events,
            closed: watch::Sender::new(None),
            next_id: AtomicU64::new(1),
            shutdown: CancellationToken::new(),
        });
        let reader = tokio::spawn(read_loop(frames, Arc::clone(&shared)));
        let mut client = Self {
            shared,
            reader,
            protocol_version: 1,
        };
        if options.negotiate_v2 && supports_v2 {
            let data = client
                .request(
                    "negotiate_protocol",
                    json!({"protocolVersion": 2}),
                    options.handshake_timeout,
                )
                .await?;
            if data.get("protocolVersion").and_then(Value::as_u64) != Some(2) {
                return Err(OmpError::Handshake(format!(
                    "server did not accept protocol v2: {data}"
                )));
            }
            client.protocol_version = 2;
        }
        Ok(client)
    }

    /// Negotiated protocol version (1 or 2).
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    /// A fresh request id (`bb-<n>`).
    pub fn new_id(&self) -> String {
        format!("bb-{}", self.shared.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Sends a command and waits for its response; returns `data` (`Null` when absent).
    pub async fn request(&self, command: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.new_id();
        self.request_with_id(&id, command, params, timeout).await
    }

    /// Like [`Self::request`] with a caller-chosen id (for prompts correlated by `prompt_result`).
    pub async fn request_with_id(
        &self,
        id: &str,
        command: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let mut frame = match params {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => {
                return Err(OmpError::decode(
                    command,
                    format!("params must be an object, got {other}"),
                ));
            }
        };
        frame.insert("id".into(), Value::String(id.to_owned()));
        frame.insert("type".into(), Value::String(command.to_owned()));
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.shared.closed.borrow().is_some() {
                return Err(self.shared.disconnected());
            }
            pending.insert(id.to_owned(), tx);
        }
        let outcome = tokio::time::timeout(timeout, async {
            self.shared.send(&Value::Object(frame)).await?;
            rx.await.map_err(|_| self.shared.disconnected())
        })
        .await;
        let response = match outcome {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                self.forget(id);
                return Err(error);
            }
            Err(_) => {
                self.forget(id);
                return Err(OmpError::Timeout {
                    command: command.to_owned(),
                    after: timeout,
                });
            }
        };
        if response.get("success").and_then(Value::as_bool) == Some(true) {
            return Ok(response.get("data").cloned().unwrap_or(Value::Null));
        }
        Err(OmpError::CommandFailed {
            command: command.to_owned(),
            message: response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned(),
            code: response
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    fn forget(&self, id: &str) {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    /// Writes a raw inbound frame (no response expected).
    pub async fn send_frame(&self, frame: &Value) -> Result<()> {
        self.shared.send(frame).await
    }

    /// Subscribes to unsolicited frames received from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<RpcEvent>> {
        self.shared.events.subscribe()
    }

    /// Watch that turns `Some` once the server stops delivering frames.
    pub fn closed(&self) -> watch::Receiver<Option<CloseReason>> {
        self.shared.closed.subscribe()
    }

    pub fn close_reason(&self) -> Option<CloseReason> {
        self.shared.closed.borrow().clone()
    }

    /// Installs the handler serving `host_tool_call` requests.
    pub fn set_host_tool_handler(&self, handler: Option<Arc<dyn HostToolHandler>>) {
        *self
            .shared
            .handler
            .write()
            .unwrap_or_else(PoisonError::into_inner) = handler;
    }

    /// Closes stdin and waits (up to `timeout`) for the server to close stdout.
    /// Returns whether stdout reached EOF in time.
    pub async fn close(&self, timeout: Duration) -> bool {
        if let Some(mut writer) = self.shared.writer.lock().await.take()
            && let Err(error) = writer.shutdown().await
        {
            tracing::debug!(%error, "closing omp stdin failed");
        }
        let mut closed = self.closed();
        tokio::time::timeout(timeout, closed.wait_for(Option::is_some))
            .await
            .is_ok()
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.shared.shutdown.cancel();
        self.reader.abort();
    }
}

async fn read_ready<R: AsyncRead + Unpin>(
    frames: &mut FrameReader<R>,
) -> Result<Map<String, Value>> {
    loop {
        match frames.next_frame().await {
            Ok(Some(frame)) if frame.get("type").and_then(Value::as_str) == Some("ready") => {
                return Ok(frame);
            }
            Ok(Some(frame)) => {
                tracing::warn!(kind = ?frame.get("type"), "ignoring frame before ready");
            }
            Ok(None) => {
                return Err(OmpError::Handshake(
                    "server closed stdout before the ready frame".into(),
                ));
            }
            Err(error) if error.is_fatal() => return Err(error.into()),
            Err(error) => tracing::warn!(%error, "dropping malformed frame before ready"),
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(mut frames: FrameReader<R>, shared: Arc<Shared>) {
    let reason = loop {
        match frames.next_frame().await {
            Ok(Some(frame)) => dispatch(frame, &shared),
            Ok(None) => break CloseReason::Eof,
            Err(error) if error.is_fatal() => break CloseReason::Io(error.to_string()),
            Err(error) => tracing::warn!(%error, "dropping malformed omp frame"),
        }
    };
    tracing::debug!(%reason, "omp stdout closed");
    shared.close(reason);
}

fn dispatch(frame: Map<String, Value>, shared: &Arc<Shared>) {
    let kind = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if kind == "response" {
        let id = frame.get("id").and_then(Value::as_str).map(str::to_owned);
        let sender = id.as_ref().and_then(|id| {
            shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(id)
        });
        match sender {
            Some(sender) => {
                // The requester may have timed out; nothing to do then.
                let _ = sender.send(frame);
            }
            None => tracing::warn!(
                ?id,
                command = ?frame.get("command"),
                error = ?frame.get("error"),
                "uncorrelated omp response"
            ),
        }
        return;
    }
    match kind.as_str() {
        "host_tool_call" => start_host_call(&frame, shared),
        "host_tool_cancel" => {
            let target = frame.get("targetId").and_then(Value::as_str);
            let token = target.and_then(|target| {
                shared
                    .host_calls
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(target)
            });
            if let Some(token) = token {
                token.cancel();
            }
        }
        "extension_ui_request" => {
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if DIALOG_METHODS.contains(&method)
                && let Some(id) = frame.get("id").and_then(Value::as_str)
            {
                let reply = json!({"type": "extension_ui_response", "id": id, "cancelled": true});
                let shared = Arc::clone(shared);
                tokio::spawn(async move {
                    if let Err(error) = shared.send(&reply).await {
                        tracing::debug!(%error, "failed to cancel extension UI dialog");
                    }
                });
            }
        }
        _ => {}
    }
    // No subscribers is fine.
    let _ = shared.events.send(Arc::new(RpcEvent { kind, frame }));
}

fn start_host_call(frame: &Map<String, Value>, shared: &Arc<Shared>) {
    let Some(id) = frame.get("id").and_then(Value::as_str).map(str::to_owned) else {
        tracing::warn!("host_tool_call without id");
        return;
    };
    let tool_call_id = frame.get("toolCallId").cloned().unwrap_or(Value::Null);
    let name = frame
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let arguments = frame.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let handler = shared
        .handler
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let token = shared.shutdown.child_token();
    shared
        .host_calls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id.clone(), token.clone());
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let outcome = match handler {
            None => HostToolOutcome::error(format!("host has no handler for tool {name}")),
            Some(handler) => {
                let call_name = name.clone();
                let call_token = token.clone();
                // A panicking handler must still answer, or the agent waits forever.
                match tokio::spawn(
                    async move { handler.call(&call_name, arguments, call_token).await },
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        HostToolOutcome::error(format!("host tool {name} failed: {error}"))
                    }
                }
            }
        };
        shared
            .host_calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if token.is_cancelled() {
            tracing::debug!(%id, tool = %name, "host tool call cancelled; result dropped");
            return;
        }
        tracing::debug!(%id, tool = %name, is_error = outcome.is_error, elapsed = ?started.elapsed(), "host tool call done");
        let mut reply = json!({
            "type": "host_tool_result",
            "id": id,
            "toolCallId": tool_call_id,
            "result": {"content": outcome.content},
        });
        if outcome.is_error
            && let Some(object) = reply.as_object_mut()
        {
            object.insert("isError".into(), Value::Bool(true));
        }
        if let Err(error) = shared.send(&reply).await {
            tracing::warn!(%error, "failed to deliver host tool result");
        }
    });
}

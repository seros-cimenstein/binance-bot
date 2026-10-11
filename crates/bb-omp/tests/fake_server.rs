//! AgentSession / RpcClient against an in-process fake omp RPC server over `tokio::io::duplex`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bb_omp::codec::{FrameLimits, FrameReader, chunk_frame};
use bb_omp::{
    AgentSession, CancellationToken, ClientOptions, HostTool, HostToolHandler, HostToolOutcome,
    OmpError, PromptStatus, RpcClient, SessionConfig, Transport,
};
use pretty_assertions::assert_eq;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(5);

struct FakeServer {
    reader: FrameReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl FakeServer {
    fn pair() -> (Self, Transport) {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);
        (
            Self {
                reader: FrameReader::new(server_read, FrameLimits::default()),
                writer: server_write,
            },
            Transport::new(client_read, client_write),
        )
    }

    async fn send(&mut self, frame: Value) {
        let mut line = frame.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn send_raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> Map<String, Value> {
        tokio::time::timeout(TIMEOUT, self.reader.next_frame())
            .await
            .expect("client frame in time")
            .unwrap()
            .expect("client still connected")
    }

    /// Next client frame, which must be command `kind`; returns its id and the frame.
    async fn expect(&mut self, kind: &str) -> (String, Map<String, Value>) {
        let frame = self.recv().await;
        assert_eq!(frame["type"], kind, "unexpected frame {frame:?}");
        (frame["id"].as_str().unwrap_or_default().to_owned(), frame)
    }

    async fn respond(&mut self, id: &str, command: &str, data: Value) {
        self.send(json!({"id": id, "type": "response", "command": command, "success": true, "data": data}))
            .await;
    }

    async fn ready(&mut self) {
        self.send(json!({
            "type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1, 2],
            "maxFrameBytes": 1_048_576, "maxReassembledFrameBytes": 67_108_864
        }))
        .await;
    }

    /// ready + v2 negotiation + event filter (what `AgentSession::connect` performs).
    async fn handshake(&mut self) {
        self.ready().await;
        let (id, frame) = self.expect("negotiate_protocol").await;
        assert_eq!(frame["protocolVersion"], 2);
        self.respond(&id, "negotiate_protocol", json!({"protocolVersion": 2}))
            .await;
        let (id, frame) = self.expect("set_event_filter").await;
        assert!(
            frame["events"]
                .as_array()
                .unwrap()
                .contains(&json!("message_end"))
        );
        self.respond(&id, "set_event_filter", json!({"events": frame["events"]}))
            .await;
    }

    async fn assistant(&mut self, text: &str, input: u64, output: u64, cost: &str) {
        let message = json!({
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "provider": "anthropic", "model": "claude-haiku-4-5", "stopReason": "stop",
            "usage": {"input": input, "output": output, "cacheRead": 1, "cacheWrite": 2,
                      "cost": {"total": serde_json::from_str::<Value>(cost).unwrap()}}
        });
        self.send(json!({"type": "message_end", "message": message}))
            .await;
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        command_timeout: TIMEOUT,
        startup_timeout: TIMEOUT,
        abort_grace: TIMEOUT,
        ..SessionConfig::default()
    }
}

async fn session<F, Fut>(script: F) -> AgentSession
where
    F: FnOnce(FakeServer) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (server, transport) = FakeServer::pair();
    tokio::spawn(script(server));
    AgentSession::connect(transport, config()).await.unwrap()
}

struct Tools {
    calls: AtomicUsize,
    cancelled: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl HostToolHandler for Tools {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> HostToolOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match name {
            "get_secret_number" => {
                HostToolOutcome::text(format!("secret for {}", arguments["who"]))
            }
            "broken" => HostToolOutcome::error("database offline"),
            "slow" => {
                cancel.cancelled().await;
                self.cancelled.send(name.to_owned()).unwrap();
                HostToolOutcome::text("too late")
            }
            other => HostToolOutcome::error(format!("unknown tool {other}")),
        }
    }
}

fn tools() -> (Arc<Tools>, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Arc::new(Tools {
            calls: AtomicUsize::new(0),
            cancelled: tx,
        }),
        rx,
    )
}

fn tool_defs() -> Vec<HostTool> {
    vec![HostTool::new(
        "get_secret_number",
        "Secret",
        "Returns the secret number",
        json!({"type": "object", "properties": {"who": {"type": "string"}}}),
    )]
}

#[tokio::test]
async fn handshake_negotiates_v2_and_reassembles_chunked_responses() {
    let (mut server, transport) = FakeServer::pair();
    let script = tokio::spawn(async move {
        server.handshake().await;
        let (id, _) = server.expect("get_state").await;
        let state = json!({
            "id": id, "type": "response", "command": "get_state", "success": true,
            "data": {
                "model": {"provider": "anthropic", "id": "claude-haiku-4-5", "name": "x".repeat(300)},
                "thinkingLevel": "off", "isStreaming": false, "isSettled": true,
                "sessionFile": "/tmp/s/a.jsonl", "sessionId": "sid", "messageCount": 4,
                "autoCompactionEnabled": true,
                "contextUsage": {"tokens": 1100, "contextWindow": 200000, "percent": 0.55}
            }
        });
        // A rejected interleaved sequence (carrying a forged response) must be dropped.
        let forged = json!({"id": id, "type": "response", "command": "get_state", "success": false, "error": "forged"});
        let a = chunk_frame("rpc-a", &forged, 16);
        let b = chunk_frame("rpc-b", &forged, 16);
        server.send(a[0].clone()).await;
        server.send(b[0].clone()).await;
        // A short sequence interrupted by an ordinary frame is dropped too; the frame survives.
        server.send(a[0].clone()).await;
        server
            .send(json!({"type": "notice", "level": "info", "message": "hi"}))
            .await;
        for chunk in chunk_frame("rpc-2", &state, 64) {
            server.send(chunk).await;
        }
        server
    });
    let session = AgentSession::connect(transport, config()).await.unwrap();
    assert_eq!(session.client().protocol_version(), 2);
    let mut events = session.client().subscribe();
    let state = session.state().await.unwrap();
    assert_eq!(state.model.unwrap().id, "claude-haiku-4-5");
    assert_eq!(state.session_id.as_deref(), Some("sid"));
    assert_eq!(state.message_count, 4);
    assert_eq!(state.context_usage.unwrap().tokens, Some(1100));
    let notice = events.recv().await.unwrap();
    assert_eq!(notice.kind, "notice");
    drop(script.await.unwrap());
}

#[tokio::test]
async fn v1_only_server_skips_negotiation_and_ready_limits_apply() {
    let (mut server, transport) = FakeServer::pair();
    tokio::spawn(async move {
        server.send_raw("garbage before ready\n").await;
        server
            .send(json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1], "maxFrameBytes": 128}))
            .await;
        let (id, _) = server.expect("get_last_assistant_text").await;
        server
            .send(json!({"id": id, "type": "response", "command": "get_last_assistant_text", "success": true, "data": {"text": "y".repeat(200)}}))
            .await;
        server
            .respond(&id, "get_last_assistant_text", json!({"text": "ok"}))
            .await;
        server.expect("ping").await;
    });
    let client = RpcClient::connect(transport, ClientOptions::default())
        .await
        .unwrap();
    assert_eq!(client.protocol_version(), 1);
    // The oversized physical frame is rejected; the next valid response resolves the request.
    let data = client
        .request("get_last_assistant_text", Value::Null, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(data["text"], "ok");
    let err = client
        .request(
            "ping",
            json!({"payload": "z".repeat(200)}),
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OmpError::Frame(_)), "{err}");
}

#[tokio::test]
async fn out_of_order_responses_are_correlated_by_id() {
    let session = session(|mut server: FakeServer| async move {
        server.handshake().await;
        let (first, _) = server.expect("get_last_assistant_text").await;
        let (second, _) = server.expect("get_state").await;
        let (third, _) = server.expect("steer").await;
        server
            .send(json!({"id": third, "type": "response", "command": "steer", "success": false, "error": "busy", "code": "session_busy"}))
            .await;
        server
            .respond(&second, "get_state", json!({"sessionId": "s2", "messageCount": 9}))
            .await;
        server
            .respond(&first, "get_last_assistant_text", json!({"text": "first"}))
            .await;
        server.expect("abort").await;
        // Stay connected so the unanswered abort times out instead of disconnecting.
        tokio::time::sleep(TIMEOUT).await;
    })
    .await;
    let (text, state, steer) = tokio::join!(
        session.last_assistant_text(),
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            session.state().await
        },
        async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            session.steer("hurry").await
        }
    );
    assert_eq!(text.unwrap().as_deref(), Some("first"));
    assert_eq!(state.unwrap().message_count, 9);
    match steer.unwrap_err() {
        OmpError::CommandFailed {
            command,
            message,
            code,
        } => {
            assert_eq!((command.as_str(), message.as_str()), ("steer", "busy"));
            assert_eq!(code.as_deref(), Some("session_busy"));
        }
        other => panic!("unexpected {other}"),
    }
    // Server never answers abort: per-command timeout.
    let started = std::time::Instant::now();
    let err = session
        .client()
        .request("abort", Value::Null, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(
        matches!(err, OmpError::Timeout { ref command, .. } if command == "abort"),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn prompt_runs_host_tools_and_collects_outcome() {
    let (handler, _cancelled) = tools();
    let mut session = session(|mut server: FakeServer| async move {
        server.handshake().await;
        let (id, frame) = server.expect("set_host_tools").await;
        assert_eq!(frame["tools"][0]["name"], "get_secret_number");
        assert_eq!(frame["tools"][0]["label"], "Secret");
        assert_eq!(frame["tools"][0]["parameters"]["type"], "object");
        server
            .respond(&id, "set_host_tools", json!({"toolNames": ["get_secret_number", "broken"]}))
            .await;

        let (pid, frame) = server.expect("prompt").await;
        assert_eq!(frame["message"], "go");
        server.respond(&pid, "prompt", Value::Null).await;
        server.send(json!({"type": "agent_start"})).await;
        server.assistant("", 100, 10, "0.001").await;
        for (call, tool, args) in [
            ("h1", "get_secret_number", json!({"who": "bot"})),
            ("h2", "broken", json!({})),
        ] {
            server
                .send(json!({"type": "tool_execution_start", "toolCallId": format!("tc-{call}"), "toolName": tool, "args": args}))
                .await;
            server
                .send(json!({"type": "host_tool_call", "id": call, "toolCallId": format!("tc-{call}"), "toolName": tool, "arguments": args}))
                .await;
        }
        let mut results = vec![
            server.expect("host_tool_result").await.1,
            server.expect("host_tool_result").await.1,
        ];
        results.sort_by_key(|r| r["id"].as_str().unwrap().to_owned());
        assert_eq!(
            Value::Object(results[0].clone()),
            json!({"type": "host_tool_result", "id": "h1", "toolCallId": "tc-h1",
                   "result": {"content": [{"type": "text", "text": "secret for \"bot\""}]}})
        );
        assert_eq!(
            Value::Object(results[1].clone()),
            json!({"type": "host_tool_result", "id": "h2", "toolCallId": "tc-h2", "isError": true,
                   "result": {"content": [{"type": "text", "text": "database offline"}]}})
        );
        for result in &results {
            let call = result["id"].as_str().unwrap();
            server
                .send(json!({"type": "tool_execution_end", "toolCallId": format!("tc-{call}"),
                             "toolName": "x", "result": result["result"], "isError": result.get("isError").is_some()}))
                .await;
        }
        server.assistant("The number is 4217.", 200, 20, "7.500000000000001e-05").await;
        // A stale prompt_result for another prompt is ignored.
        server
            .send(json!({"type": "prompt_result", "id": "other", "agentInvoked": true, "status": "aborted", "sessionSettled": true}))
            .await;
        server
            .send(json!({"type": "agent_end", "messages": [], "isTerminal": true, "yielded": true}))
            .await;
        server
            .send(json!({"type": "prompt_result", "id": pid, "agentInvoked": true, "status": "completed", "sessionSettled": true}))
            .await;
    })
    .await;
    let names = session
        .set_host_tools(tool_defs(), handler.clone())
        .await
        .unwrap();
    assert_eq!(names, ["get_secret_number", "broken"]);
    let outcome = session.prompt("go", TIMEOUT).await.unwrap();
    assert_eq!(outcome.status, PromptStatus::Completed);
    assert!(outcome.agent_invoked && outcome.session_settled);
    assert_eq!(outcome.final_text.as_deref(), Some("The number is 4217."));
    assert_eq!(outcome.assistant_messages.len(), 2);
    assert_eq!(outcome.tool_calls.len(), 2);
    let ok = &outcome.tool_calls[0];
    assert_eq!(
        (ok.name.as_str(), ok.is_error),
        ("get_secret_number", false)
    );
    assert_eq!(ok.arguments, json!({"who": "bot"}));
    assert_eq!(ok.result_text, "secret for \"bot\"");
    let failed = &outcome.tool_calls[1];
    assert_eq!((failed.name.as_str(), failed.is_error), ("broken", true));
    assert_eq!(failed.result_text, "database offline");
    assert_eq!(outcome.usage.input_tokens, 300);
    assert_eq!(outcome.usage.output_tokens, 30);
    assert_eq!(outcome.usage.cache_read_tokens, 2);
    assert_eq!(outcome.usage.cache_write_tokens, 4);
    assert_eq!(outcome.usage.cost_usd.to_string(), "0.00107500000000000001");
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn host_tool_cancel_reaches_handler_and_drops_result() {
    let (handler, mut cancelled) = tools();
    let (mut server, transport) = FakeServer::pair();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let script = tokio::spawn(async move {
        server.ready().await;
        let (id, _) = server.expect("negotiate_protocol").await;
        server
            .respond(&id, "negotiate_protocol", json!({"protocolVersion": 2}))
            .await;
        go_rx.await.unwrap();
        server
            .send(json!({"type": "host_tool_call", "id": "h9", "toolCallId": "tc9", "toolName": "slow", "arguments": {}}))
            .await;
        // Dialogs are auto-cancelled so the agent never blocks on a host without UI.
        server
            .send(json!({"type": "extension_ui_request", "id": "ui1", "method": "confirm", "title": "?"}))
            .await;
        server
            .send(json!({"type": "extension_ui_request", "id": "ui2", "method": "setWidget"}))
            .await;
        let (_, reply) = server.expect("extension_ui_response").await;
        assert_eq!(
            Value::Object(reply),
            json!({"type": "extension_ui_response", "id": "ui1", "cancelled": true})
        );
        server
            .send(json!({"type": "host_tool_cancel", "id": "c1", "targetId": "h9"}))
            .await;
        server
    });
    let client = RpcClient::connect(transport, ClientOptions::default())
        .await
        .unwrap();
    client.set_host_tool_handler(Some(handler));
    go_tx.send(()).unwrap();
    let mut server = script.await.unwrap();
    let name = tokio::time::timeout(TIMEOUT, cancelled.recv())
        .await
        .unwrap();
    assert_eq!(name.as_deref(), Some("slow"));
    // No host_tool_result follows the cancellation: the next frame is our own request.
    let request =
        tokio::spawn(async move { client.request("get_state", Value::Null, TIMEOUT).await });
    let (id, _) = server.expect("get_state").await;
    server.respond(&id, "get_state", json!({})).await;
    request.await.unwrap().unwrap();
}

#[tokio::test]
async fn prompt_timeout_aborts_and_returns_partial_outcome() {
    let session = session(|mut server: FakeServer| async move {
        server.handshake().await;
        let (pid, _) = server.expect("prompt").await;
        server.respond(&pid, "prompt", Value::Null).await;
        server.send(json!({"type": "agent_start"})).await;
        server.assistant("partial", 50, 5, "0.0002").await;
        let (aid, _) = server.expect("abort").await;
        server.respond(&aid, "abort", Value::Null).await;
        server
            .send(json!({"type": "prompt_result", "id": pid, "agentInvoked": true, "status": "aborted", "sessionSettled": true}))
            .await;
        server.expect("get_state").await;
    })
    .await;
    let err = session
        .prompt("write a novel", Duration::from_millis(300))
        .await
        .unwrap_err();
    match err {
        OmpError::PromptTimeout { after, outcome } => {
            assert_eq!(after, Duration::from_millis(300));
            assert_eq!(outcome.status, PromptStatus::Aborted);
            assert_eq!(outcome.final_text.as_deref(), Some("partial"));
            assert_eq!(outcome.usage.input_tokens, 50);
        }
        other => panic!("unexpected {other}"),
    }
}

#[tokio::test]
async fn prompt_error_status_and_local_completion() {
    let session = session(|mut server: FakeServer| async move {
        server.handshake().await;
        let (pid, _) = server.expect("prompt").await;
        server.respond(&pid, "prompt", json!({"agentInvoked": false})).await;
        let (pid, _) = server.expect("prompt").await;
        server.respond(&pid, "prompt", Value::Null).await;
        server
            .send(json!({"type": "prompt_result", "id": pid, "agentInvoked": true, "status": "error", "sessionSettled": true,
                         "error": {"message": "overloaded", "provider": "anthropic", "httpStatus": 529, "retryable": true}}))
            .await;
        server.expect("never").await;
    })
    .await;
    let local = session.prompt("/local", TIMEOUT).await.unwrap();
    assert!(!local.agent_invoked);
    assert_eq!(local.status, PromptStatus::Completed);
    let failed = session.prompt("hi", TIMEOUT).await.unwrap();
    assert_eq!(failed.status, PromptStatus::Error);
    let error = failed.error.unwrap();
    assert_eq!(
        (error.message.as_str(), error.http_status, error.retryable),
        ("overloaded", Some(529), true)
    );
}

#[tokio::test]
async fn connection_loss_mid_turn_is_a_disconnect_error() {
    let session = session(|mut server: FakeServer| async move {
        server.handshake().await;
        let (pid, _) = server.expect("prompt").await;
        server.respond(&pid, "prompt", Value::Null).await;
        server.send(json!({"type": "agent_start"})).await;
        drop(server);
    })
    .await;
    let err = session.prompt("hi", TIMEOUT).await.unwrap_err();
    assert!(err.is_disconnect(), "{err}");
    assert!(
        matches!(err, OmpError::Disconnected { ref reason } if reason.contains("mid-turn")),
        "{err}"
    );
    // Later commands fail fast.
    let err = session.state().await.unwrap_err();
    assert!(err.is_disconnect(), "{err}");
}

const FAKE_OMP: &str = r#"
printf '%s\n' '{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}'
read -r line; printf '%s\n' '{"id":"bb-1","type":"response","command":"negotiate_protocol","success":true,"data":{"protocolVersion":2}}'
read -r line; printf '%s\n' '{"id":"bb-2","type":"response","command":"set_event_filter","success":true,"data":{}}'
read -r line; printf '%s\n' '{"id":"bb-3","type":"response","command":"open_session","success":true,"data":{"cancelled":false,"resumed":true,"sessionId":"s1"}}'
read -r line; printf '%s\n' '{"id":"bb-4","type":"response","command":"prompt","success":true}' '{"type":"agent_start"}'
echo "fatal: provider exploded" >&2
exit 7
"#;

fn fake_omp_config(dir: &Path) -> SessionConfig {
    SessionConfig {
        program: PathBuf::from("sh"),
        program_args: vec!["-c".into(), FAKE_OMP.into(), "fake-omp".into()],
        session_dir: Some(dir.to_path_buf()),
        ..config()
    }
}

#[tokio::test]
async fn process_death_mid_turn_reports_exit_status_and_stderr_then_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = AgentSession::start(fake_omp_config(dir.path()))
        .await
        .unwrap();
    let opened = session.open_session(dir.path()).await.unwrap();
    assert!(opened.resumed);
    match session.prompt("hi", TIMEOUT).await.unwrap_err() {
        OmpError::ProcessDied {
            status,
            reason,
            stderr,
        } => {
            assert!(status.contains('7'), "{status}");
            assert!(reason.contains("mid-turn"), "{reason}");
            assert_eq!(stderr, "fatal: provider exploded\n");
        }
        other => panic!("unexpected {other}"),
    }
    // restart spawns a fresh process and re-opens the same session dir.
    let reopened = session.restart().await.unwrap().unwrap();
    assert_eq!(reopened.session_id.as_deref(), Some("s1"));
    let status = session.shutdown().await.unwrap().unwrap();
    assert!(!status.success());
}

#[tokio::test]
async fn start_failure_surfaces_process_death() {
    let config = SessionConfig {
        program: PathBuf::from("sh"),
        program_args: vec![
            "-c".into(),
            "echo 'no credentials' >&2; exit 2".into(),
            "fake".into(),
        ],
        ..config()
    };
    match AgentSession::start(config).await {
        Err(OmpError::ProcessDied { status, stderr, .. }) => {
            assert!(status.contains('2'), "{status}");
            assert_eq!(stderr, "no credentials\n");
        }
        Err(other) => panic!("unexpected {other}"),
        Ok(_) => panic!("start should fail"),
    }
}

#[tokio::test]
async fn shutdown_kills_a_process_that_ignores_stdin_close() {
    let script = r#"printf '%s\n' '{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1]}'
read -r line; printf '%s\n' '{"id":"bb-1","type":"response","command":"set_event_filter","success":true}'
exec sleep 30"#;
    let config = SessionConfig {
        program: PathBuf::from("sh"),
        program_args: vec!["-c".into(), script.into(), "fake".into()],
        shutdown_timeout: Duration::from_millis(300),
        ..config()
    };
    let session = AgentSession::start(config).await.unwrap();
    let started = std::time::Instant::now();
    let status = session.shutdown().await.unwrap().unwrap();
    assert!(!status.success());
    assert!(started.elapsed() < Duration::from_secs(5));
}

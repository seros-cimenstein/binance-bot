//! High-level long-lived agent session over omp RPC.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::client::{ClientOptions, RpcClient, RpcEvent};
use crate::error::{OmpError, Result};
use crate::transport::{OmpProcess, ProcessConfig, Transport};
use crate::types::{
    AssistantMessage, CompactResult, HostTool, HostToolHandler, ModelRef, OpenSessionResult,
    PromptError, PromptStatus, SessionState, ThinkingLevel, ToolCallRecord, TurnOutcome,
    UsageTotals, content_text,
};

/// Session events forwarded when `forward_message_updates` is off (drops streaming deltas).
pub const DEFAULT_EVENTS: &[&str] = &[
    "agent_start",
    "agent_end",
    "turn_start",
    "turn_end",
    "message_start",
    "message_end",
    "tool_execution_start",
    "tool_execution_end",
    "auto_compaction_start",
    "auto_compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "retry_fallback_applied",
    "retry_fallback_succeeded",
    "model_changed",
    "thinking_level_changed",
    "notice",
    "queue_update",
];

/// Flags that strip omp down to a pure model + host tools agent.
pub const ISOLATION_ARGS: &[&str] = &[
    "--no-tools",
    "--no-extensions",
    "--no-skills",
    "--no-rules",
    "--no-lsp",
];

/// How to start and drive the agent.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Agent binary (`omp` on `PATH` by default).
    pub program: PathBuf,
    /// Arguments placed before the generated ones (e.g. a wrapper script).
    pub program_args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    /// `--model` pattern (e.g. `haiku`, `anthropic/claude-sonnet-4-5`).
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    /// Persistent session directory; `None` runs with `--no-session`.
    pub session_dir: Option<PathBuf>,
    /// Pass [`ISOLATION_ARGS`] (no built-in tools, extensions, skills, rules, LSP).
    pub isolated: bool,
    /// omp built-in tools to keep while isolated (e.g. `web_search`); replaces
    /// `--no-tools` with `--tools <list>`. Ignored when `isolated` is false.
    pub builtin_tools: Vec<String>,
    pub extra_args: Vec<String>,
    /// Forward `message_update` streaming deltas to subscribers.
    pub forward_message_updates: bool,
    /// Default timeout for ordinary commands (and the prompt acknowledgement).
    pub command_timeout: Duration,
    /// Handshake timeout; also used for commands that may wait on model discovery.
    pub startup_timeout: Duration,
    pub compact_timeout: Duration,
    /// How long a timed-out prompt may take to report its abort.
    pub abort_grace: Duration,
    /// How long `shutdown` waits for a clean exit before killing.
    pub shutdown_timeout: Duration,
    pub stderr_capacity: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from("omp"),
            program_args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            model: None,
            thinking: None,
            system_prompt: None,
            append_system_prompt: None,
            session_dir: None,
            isolated: true,
            builtin_tools: Vec::new(),
            extra_args: Vec::new(),
            forward_message_updates: false,
            command_timeout: Duration::from_secs(30),
            startup_timeout: Duration::from_secs(90),
            compact_timeout: Duration::from_secs(600),
            abort_grace: Duration::from_secs(15),
            shutdown_timeout: Duration::from_secs(15),
            stderr_capacity: 64 * 1024,
        }
    }
}

impl SessionConfig {
    /// Full argument list passed to the program.
    pub fn args(&self) -> Vec<String> {
        let mut args = self.program_args.clone();
        args.extend(["--mode".to_owned(), "rpc".to_owned()]);
        if self.isolated {
            if self.builtin_tools.is_empty() {
                args.extend(ISOLATION_ARGS.iter().map(|arg| (*arg).to_owned()));
            } else {
                args.extend(["--tools".to_owned(), self.builtin_tools.join(",")]);
                args.extend(
                    ISOLATION_ARGS
                        .iter()
                        .filter(|arg| **arg != "--no-tools")
                        .map(|arg| (*arg).to_owned()),
                );
            }
        }
        let mut flag = |name: &str, value: &str| {
            args.push(name.to_owned());
            args.push(value.to_owned());
        };
        if let Some(model) = &self.model {
            flag("--model", model);
        }
        if let Some(thinking) = self.thinking {
            flag("--thinking", thinking.as_str());
        }
        if let Some(prompt) = &self.system_prompt {
            flag("--system-prompt", prompt);
        }
        if let Some(prompt) = &self.append_system_prompt {
            flag("--append-system-prompt", prompt);
        }
        match &self.session_dir {
            Some(dir) => flag("--session-dir", &dir.to_string_lossy()),
            None => args.push("--no-session".to_owned()),
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }

    pub fn process_config(&self) -> ProcessConfig {
        ProcessConfig {
            program: self.program.clone(),
            args: self.args(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            env_remove: self.env_remove.clone(),
            stderr_capacity: self.stderr_capacity,
        }
    }

    fn client_options(&self) -> ClientOptions {
        ClientOptions {
            handshake_timeout: self.startup_timeout,
            ..ClientOptions::default()
        }
    }
}

type HostTools = (Vec<HostTool>, Arc<dyn HostToolHandler>);

/// A long-lived omp agent. Prompts are serialised; `steer`, `abort` and `state` may run
/// concurrently with a prompt.
pub struct AgentSession {
    config: SessionConfig,
    client: RpcClient,
    process: Option<OmpProcess>,
    host_tools: Option<HostTools>,
    opened_dir: Option<PathBuf>,
    turn: tokio::sync::Mutex<()>,
}

impl AgentSession {
    /// Spawns omp per `config` and completes the handshake.
    pub async fn start(config: SessionConfig) -> Result<Self> {
        let (client, process) = Self::launch(&config).await?;
        Ok(Self {
            config,
            client,
            process: Some(process),
            host_tools: None,
            opened_dir: None,
            turn: tokio::sync::Mutex::new(()),
        })
    }

    /// Drives an already-connected transport (tests, custom launchers). Not restartable.
    pub async fn connect(transport: Transport, config: SessionConfig) -> Result<Self> {
        let client = RpcClient::connect(transport, config.client_options()).await?;
        init(&client, &config).await?;
        Ok(Self {
            config,
            client,
            process: None,
            host_tools: None,
            opened_dir: None,
            turn: tokio::sync::Mutex::new(()),
        })
    }

    async fn launch(config: &SessionConfig) -> Result<(RpcClient, OmpProcess)> {
        let (process, transport) = OmpProcess::spawn(&config.process_config())?;
        let connected = async {
            let client = RpcClient::connect(transport, config.client_options()).await?;
            init(&client, config).await?;
            Ok::<_, OmpError>(client)
        }
        .await;
        match connected {
            Ok(client) => Ok((client, process)),
            Err(error) => {
                let error = process_died(Some(&process), error).await;
                if let Err(kill) = process.kill().await {
                    tracing::warn!(error = %kill, "failed to kill omp after failed start");
                }
                Err(error)
            }
        }
    }

    /// The underlying client (raw commands, event subscription).
    pub fn client(&self) -> &RpcClient {
        &self.client
    }

    pub fn process(&self) -> Option<&OmpProcess> {
        self.process.as_ref()
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Sends a command; disconnects are reported as [`OmpError::ProcessDied`] when process-backed.
    pub async fn request(&self, command: &str, params: Value, timeout: Duration) -> Result<Value> {
        match self.client.request(command, params, timeout).await {
            Ok(data) => Ok(data),
            Err(error) => Err(self.died(error).await),
        }
    }

    async fn request_as<T: DeserializeOwned>(
        &self,
        command: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<T> {
        let data = self.request(command, params, timeout).await?;
        serde_json::from_value(data).map_err(|error| OmpError::decode(command, error))
    }

    async fn died(&self, error: OmpError) -> OmpError {
        process_died(self.process.as_ref(), error).await
    }

    /// Continues the newest session in `dir` (or starts one there). Remembered for [`Self::restart`].
    pub async fn open_session(&mut self, dir: &Path) -> Result<OpenSessionResult> {
        let result: OpenSessionResult = self
            .request_as(
                "open_session",
                json!({"sessionDir": dir.to_string_lossy()}),
                self.config.startup_timeout,
            )
            .await?;
        if !result.cancelled {
            self.opened_dir = Some(dir.to_path_buf());
        }
        Ok(result)
    }

    /// Registers host tools and the handler serving them; replaces the previous set.
    pub async fn set_host_tools(
        &mut self,
        tools: Vec<HostTool>,
        handler: Arc<dyn HostToolHandler>,
    ) -> Result<Vec<String>> {
        let names =
            register_host_tools(&self.client, &tools, &handler, self.config.command_timeout).await;
        let names = match names {
            Ok(names) => names,
            Err(error) => return Err(self.died(error).await),
        };
        self.host_tools = Some((tools, handler));
        Ok(names)
    }

    /// Runs one prompt to its `prompt_result`. On timeout the turn is aborted and
    /// [`OmpError::PromptTimeout`] carries the partial outcome.
    pub async fn prompt(&self, message: &str, timeout: Duration) -> Result<TurnOutcome> {
        let _turn = self.turn.lock().await;
        let started = tokio::time::Instant::now();
        let deadline = started + timeout;
        let mut events = self.client.subscribe();
        let mut closed = self.client.closed();
        let id = self.client.new_id();
        let ack_timeout = self.config.command_timeout.min(timeout);
        let ack = match self
            .client
            .request_with_id(&id, "prompt", json!({"message": message}), ack_timeout)
            .await
        {
            Ok(ack) => ack,
            Err(error) => return Err(self.died(error).await),
        };
        let mut turn = TurnCollector::new(id);
        if ack.get("agentInvoked").and_then(Value::as_bool) == Some(false) {
            return Ok(turn.finish(PromptStatus::Completed, false, None, true));
        }
        loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(event) => {
                        if let Some(outcome) = turn.push(&event) {
                            return Ok(outcome);
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "event receiver lagged; turn record may be incomplete");
                    }
                    Err(RecvError::Closed) => {
                        return Err(self.died(OmpError::disconnected("event channel closed")).await);
                    }
                },
                _ = async { closed.wait_for(Option::is_some).await.map(drop) } => {
                    let reason = self.client.close_reason().map_or_else(
                        || "connection closed".to_owned(),
                        |reason| reason.to_string(),
                    );
                    return Err(self.died(OmpError::disconnected(format!("{reason} mid-turn"))).await);
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }

        tracing::warn!(?timeout, "prompt timed out; aborting turn");
        if let Err(error) = self
            .client
            .request("abort", Value::Null, self.config.abort_grace)
            .await
        {
            return Err(self.died(error).await);
        }
        let grace = tokio::time::Instant::now() + self.config.abort_grace;
        let outcome = loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(event) => {
                        if let Some(outcome) = turn.push(&event) {
                            break outcome;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => {
                        break turn.finish(PromptStatus::Aborted, true, None, false);
                    }
                },
                _ = async { closed.wait_for(Option::is_some).await.map(drop) } => {
                    break turn.finish(PromptStatus::Aborted, true, None, false);
                }
                () = tokio::time::sleep_until(grace) => {
                    break turn.finish(PromptStatus::Aborted, true, None, false);
                }
            }
        };
        Err(OmpError::PromptTimeout {
            after: timeout,
            outcome: Box::new(outcome),
        })
    }

    /// Injects steering into the running turn (or queues it).
    pub async fn steer(&self, message: &str) -> Result<()> {
        self.request(
            "steer",
            json!({"message": message}),
            self.config.command_timeout,
        )
        .await
        .map(drop)
    }

    /// Aborts the running turn.
    pub async fn abort(&self) -> Result<()> {
        self.request("abort", Value::Null, self.config.command_timeout)
            .await
            .map(drop)
    }

    /// Compacts the context now, optionally focused by `instructions`.
    pub async fn compact(&self, instructions: Option<&str>) -> Result<CompactResult> {
        let params = match instructions {
            Some(text) => json!({"customInstructions": text}),
            None => Value::Null,
        };
        let data = self
            .request("compact", params, self.config.compact_timeout)
            .await?;
        if data.is_null() {
            return Ok(CompactResult::default());
        }
        serde_json::from_value(data).map_err(|error| OmpError::decode("compact", error))
    }

    pub async fn state(&self) -> Result<SessionState> {
        self.request_as("get_state", Value::Null, self.config.command_timeout)
            .await
    }

    /// Switches model; returns the selected model.
    pub async fn set_model(&self, provider: &str, model_id: &str) -> Result<ModelRef> {
        self.request_as(
            "set_model",
            json!({"provider": provider, "modelId": model_id}),
            self.config.startup_timeout,
        )
        .await
    }

    pub async fn set_thinking_level(&self, level: ThinkingLevel) -> Result<()> {
        self.request(
            "set_thinking_level",
            json!({"level": level}),
            self.config.command_timeout,
        )
        .await
        .map(drop)
    }

    /// Starts a fresh session (in the same session dir). Returns `false` if an extension cancelled it.
    pub async fn new_session(&self) -> Result<bool> {
        let data = self
            .request("new_session", Value::Null, self.config.command_timeout)
            .await?;
        Ok(!data
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub async fn last_assistant_text(&self) -> Result<Option<String>> {
        let data = self
            .request(
                "get_last_assistant_text",
                Value::Null,
                self.config.command_timeout,
            )
            .await?;
        Ok(data.get("text").and_then(Value::as_str).map(str::to_owned))
    }

    /// Closes stdin, drains stdout, waits for exit (killing after `shutdown_timeout`).
    pub async fn shutdown(self) -> Result<Option<ExitStatus>> {
        stop(
            &self.client,
            self.process.as_ref(),
            self.config.shutdown_timeout,
        )
        .await
    }

    /// Replaces the process with a fresh one, re-registers host tools and re-opens the session dir,
    /// so the conversation context survives a crash.
    pub async fn restart(&mut self) -> Result<Option<OpenSessionResult>> {
        let Some(old) = self.process.as_ref() else {
            return Err(OmpError::NotRestartable("restart"));
        };
        if let Err(error) = stop(&self.client, Some(old), self.config.shutdown_timeout).await {
            tracing::warn!(%error, "stopping previous omp process failed");
        }
        let (client, process) = Self::launch(&self.config).await?;
        self.client = client;
        self.process = Some(process);
        if let Some((tools, handler)) = &self.host_tools {
            let registered =
                register_host_tools(&self.client, tools, handler, self.config.command_timeout)
                    .await;
            if let Err(error) = registered {
                return Err(self.died(error).await);
            }
        }
        match self
            .opened_dir
            .clone()
            .or_else(|| self.config.session_dir.clone())
        {
            Some(dir) => self.open_session(&dir).await.map(Some),
            None => Ok(None),
        }
    }
}

async fn init(client: &RpcClient, config: &SessionConfig) -> Result<()> {
    if !config.forward_message_updates {
        client
            .request(
                "set_event_filter",
                json!({"events": DEFAULT_EVENTS}),
                config.command_timeout,
            )
            .await?;
    }
    Ok(())
}

async fn register_host_tools(
    client: &RpcClient,
    tools: &[HostTool],
    handler: &Arc<dyn HostToolHandler>,
    timeout: Duration,
) -> Result<Vec<String>> {
    client.set_host_tool_handler(Some(Arc::clone(handler)));
    let data = client
        .request("set_host_tools", json!({"tools": tools}), timeout)
        .await?;
    let names = data
        .get("toolNames")
        .cloned()
        .map(serde_json::from_value::<Vec<String>>)
        .transpose()
        .map_err(|error| OmpError::decode("set_host_tools", error))?;
    Ok(names.unwrap_or_default())
}

async fn stop(
    client: &RpcClient,
    process: Option<&OmpProcess>,
    timeout: Duration,
) -> Result<Option<ExitStatus>> {
    let started = Instant::now();
    let drained = client.close(timeout).await;
    let Some(process) = process else {
        return Ok(None);
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    if drained && let Some(status) = process.wait_timeout(remaining).await? {
        return Ok(Some(status));
    }
    tracing::warn!(pid = ?process.pid(), "omp did not exit in time; killing");
    Ok(Some(process.kill().await?))
}

/// Converts a disconnect into [`OmpError::ProcessDied`] with exit status and stderr tail.
async fn process_died(process: Option<&OmpProcess>, error: OmpError) -> OmpError {
    let Some(process) = process else {
        return error;
    };
    if !error.is_disconnect() && !matches!(error, OmpError::Handshake(_)) {
        return error;
    }
    let status = match process.wait_timeout(Duration::from_secs(2)).await {
        Ok(Some(status)) => status.to_string(),
        Ok(None) => {
            if matches!(error, OmpError::Handshake(_)) {
                return error;
            }
            "still running".to_owned()
        }
        Err(wait) => format!("unknown ({wait})"),
    };
    process.wait_stderr(Duration::from_millis(500)).await;
    let reason = match error {
        OmpError::Disconnected { reason } => reason,
        other => other.to_string(),
    };
    OmpError::ProcessDied {
        status,
        reason,
        stderr: process.stderr().tail(),
    }
}

/// Accumulates one prompt's events into a [`TurnOutcome`].
struct TurnCollector {
    prompt_id: String,
    assistant: Vec<AssistantMessage>,
    agent_end_assistant: Vec<AssistantMessage>,
    tool_calls: Vec<ToolCallRecord>,
    open_tools: HashMap<String, (String, Value, Instant)>,
}

impl TurnCollector {
    fn new(prompt_id: String) -> Self {
        Self {
            prompt_id,
            assistant: Vec::new(),
            agent_end_assistant: Vec::new(),
            tool_calls: Vec::new(),
            open_tools: HashMap::new(),
        }
    }

    fn push(&mut self, event: &RpcEvent) -> Option<TurnOutcome> {
        match event.kind.as_str() {
            "message_end" => {
                if let Some(message) = event
                    .get("message")
                    .and_then(AssistantMessage::from_message)
                {
                    self.assistant.push(message);
                }
            }
            "agent_end" => {
                // Oversized agent_end frames drop already-streamed messages; message_end is primary.
                if let Some(messages) = event.get("messages").and_then(Value::as_array) {
                    self.agent_end_assistant
                        .extend(messages.iter().filter_map(AssistantMessage::from_message));
                }
            }
            "tool_execution_start" => {
                let id = event.str("toolCallId").unwrap_or_default().to_owned();
                let name = event.str("toolName").unwrap_or_default().to_owned();
                let args = event.get("args").cloned().unwrap_or(Value::Null);
                self.open_tools.insert(id, (name, args, Instant::now()));
            }
            "tool_execution_end" => {
                let id = event.str("toolCallId").unwrap_or_default().to_owned();
                let (name, arguments, duration) = match self.open_tools.remove(&id) {
                    Some((name, args, started)) => (name, args, started.elapsed()),
                    None => (
                        event.str("toolName").unwrap_or_default().to_owned(),
                        Value::Null,
                        Duration::ZERO,
                    ),
                };
                let result = event
                    .get("result")
                    .map(|result| {
                        result
                            .get("content")
                            .cloned()
                            .unwrap_or_else(|| result.clone())
                    })
                    .unwrap_or(Value::Null);
                self.tool_calls.push(ToolCallRecord {
                    tool_call_id: id,
                    name,
                    arguments,
                    result_text: content_text(Some(&result)),
                    result,
                    is_error: event
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    duration,
                });
            }
            "prompt_result" if event.str("id") == Some(self.prompt_id.as_str()) => {
                let status = event
                    .get("status")
                    .cloned()
                    .and_then(|status| serde_json::from_value(status).ok())
                    .unwrap_or(PromptStatus::Error);
                let error = event
                    .get("error")
                    .cloned()
                    .and_then(|error| serde_json::from_value::<PromptError>(error).ok());
                let agent_invoked = event
                    .get("agentInvoked")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let settled = event
                    .get("sessionSettled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                return Some(self.finish(status, agent_invoked, error, settled));
            }
            _ => {}
        }
        None
    }

    fn finish(
        &mut self,
        status: PromptStatus,
        agent_invoked: bool,
        error: Option<PromptError>,
        session_settled: bool,
    ) -> TurnOutcome {
        let mut assistant_messages = std::mem::take(&mut self.assistant);
        if assistant_messages.is_empty() {
            assistant_messages = std::mem::take(&mut self.agent_end_assistant);
        }
        let mut usage = UsageTotals::default();
        for message in &assistant_messages {
            usage.add(&message.usage);
        }
        let final_text = assistant_messages
            .iter()
            .rev()
            .find(|message| !message.text.trim().is_empty())
            .map(|message| message.text.clone());
        TurnOutcome {
            prompt_id: self.prompt_id.clone(),
            status,
            agent_invoked,
            final_text,
            assistant_messages,
            tool_calls: std::mem::take(&mut self.tool_calls),
            usage,
            error,
            session_settled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn args_cover_config() {
        let config = SessionConfig {
            program_args: vec!["wrapper".into()],
            model: Some("haiku".into()),
            thinking: Some(ThinkingLevel::Off),
            system_prompt: Some("be terse".into()),
            session_dir: Some(PathBuf::from("/tmp/s")),
            extra_args: vec!["--no-ui".into()],
            ..SessionConfig::default()
        };
        assert_eq!(
            config.args(),
            [
                "wrapper",
                "--mode",
                "rpc",
                "--no-tools",
                "--no-extensions",
                "--no-skills",
                "--no-rules",
                "--no-lsp",
                "--model",
                "haiku",
                "--thinking",
                "off",
                "--system-prompt",
                "be terse",
                "--session-dir",
                "/tmp/s",
                "--no-ui",
            ]
        );
        let ephemeral = SessionConfig {
            isolated: false,
            ..SessionConfig::default()
        };
        assert_eq!(ephemeral.args(), ["--mode", "rpc", "--no-session"]);
    }

    #[test]
    fn builtin_tools_replace_no_tools_but_keep_other_isolation() {
        let config = SessionConfig {
            builtin_tools: vec!["web_search".into(), "fetch".into()],
            ..SessionConfig::default()
        };
        assert_eq!(
            config.args(),
            [
                "--mode",
                "rpc",
                "--tools",
                "web_search,fetch",
                "--no-extensions",
                "--no-skills",
                "--no-rules",
                "--no-lsp",
                "--no-session",
            ]
        );
    }
}

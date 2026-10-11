//! Typed views of the omp RPC payloads the client relies on.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A host-owned tool definition sent with `set_host_tools`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostTool {
    pub name: String,
    pub label: String,
    pub description: String,
    /// JSON schema for the tool arguments.
    pub parameters: Value,
}

impl HostTool {
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// Result of one host tool execution.
#[derive(Debug, Clone, PartialEq)]
pub struct HostToolOutcome {
    /// Tool result content blocks (`{"type":"text","text":..}` etc.).
    pub content: Vec<Value>,
    /// Rejects the tool call: the model sees the content as a tool error.
    pub is_error: bool,
}

impl HostToolOutcome {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![text_block(text.into())],
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![text_block(text.into())],
            is_error: true,
        }
    }

    pub fn content(content: Vec<Value>, is_error: bool) -> Self {
        Self { content, is_error }
    }
}

fn text_block(text: String) -> Value {
    json!({"type": "text", "text": text})
}

/// Executes host tool calls issued by the agent. Calls may run concurrently.
#[async_trait]
pub trait HostToolHandler: Send + Sync {
    /// `cancel` fires when omp sends `host_tool_cancel` for this call (or the client shuts down).
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> HostToolOutcome;
}

/// Thinking effort accepted by `set_thinking_level` / `--thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Inherit,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// How a prompt ended (`prompt_result.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptStatus {
    Completed,
    Aborted,
    Error,
}

/// Provider failure reported by `prompt_result.error`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptError {
    pub message: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub http_status: Option<u16>,
    #[serde(default)]
    pub retryable: bool,
}

/// Token and cost usage (one message, or summed over a turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// `usage.cost.total`, USD.
    pub cost_usd: Decimal,
}

impl UsageTotals {
    /// Parses an assistant message `usage` object; missing fields count as zero.
    pub fn from_usage(usage: &Value) -> Self {
        let tokens = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input_tokens: tokens("input"),
            output_tokens: tokens("output"),
            cache_read_tokens: tokens("cacheRead"),
            cache_write_tokens: tokens("cacheWrite"),
            cost_usd: usage
                .pointer("/cost/total")
                .and_then(json_decimal)
                .unwrap_or_default(),
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn add(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cost_usd += other.cost_usd;
    }
}

/// Exact decimal from a JSON number (plain or scientific notation) without going through `f64`.
pub(crate) fn json_decimal(value: &Value) -> Option<Decimal> {
    let Value::Number(number) = value else {
        return None;
    };
    let text = number.to_string();
    Decimal::from_str(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .ok()
}

/// One assistant message of a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    /// Concatenated text blocks.
    pub text: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    pub usage: UsageTotals,
    /// The full message as sent by omp.
    pub raw: Value,
}

impl AssistantMessage {
    /// Parses an `AgentMessage`; `None` unless `role == "assistant"`.
    pub fn from_message(message: &Value) -> Option<Self> {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let string = |key: &str| message.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            text: content_text(message.get("content")),
            provider: string("provider"),
            model: string("model"),
            stop_reason: string("stopReason"),
            error_message: string("errorMessage"),
            usage: message
                .get("usage")
                .map(UsageTotals::from_usage)
                .unwrap_or_default(),
            raw: message.clone(),
        })
    }
}

/// Joins the `text` blocks of a content array (or returns a bare string content).
pub(crate) fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// One tool execution observed during a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: Value,
    /// Result content blocks.
    pub result: Value,
    /// Text of the result content.
    pub result_text: String,
    pub is_error: bool,
    pub duration: Duration,
}

/// Everything a prompt produced.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub prompt_id: String,
    pub status: PromptStatus,
    /// False when the prompt finished locally without an agent turn.
    pub agent_invoked: bool,
    /// Text of the last assistant message with text, if any.
    pub final_text: Option<String>,
    pub assistant_messages: Vec<AssistantMessage>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub usage: UsageTotals,
    pub error: Option<PromptError>,
    /// Whether the session was quiescent when the prompt yielded.
    pub session_settled: bool,
}

/// `provider` + `id` of a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

/// `get_state.contextUsage`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub percent: Option<f64>,
}

/// Subset of `get_state` the host cares about.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub is_compacting: bool,
    #[serde(default)]
    pub is_settled: bool,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub auto_compaction_enabled: bool,
    #[serde(default)]
    pub context_usage: Option<ContextUsage>,
}

/// `open_session` result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionResult {
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub resumed: bool,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_file: Option<String>,
}

/// `compact` result (`CompactionResult`).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactResult {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub first_kept_entry_id: Option<String>,
    #[serde(default)]
    pub tokens_before: Option<u64>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromPrimitive;

    #[test]
    fn usage_parses_exact_cost_including_scientific() {
        let usage: Value = serde_json::from_str(
            r#"{"input":2806,"output":15,"cacheRead":3,"cacheWrite":4,
                "cost":{"total":7.500000000000001e-05}}"#,
        )
        .unwrap();
        let totals = UsageTotals::from_usage(&usage);
        assert_eq!(totals.input_tokens, 2806);
        assert_eq!(totals.total_tokens(), 2828);
        assert_eq!(
            totals.cost_usd,
            Decimal::from_str("0.00007500000000000001").unwrap()
        );
        let plain: Value =
            serde_json::from_str(r#"{"cost":{"total":0.0030169999999999997}}"#).unwrap();
        assert_eq!(
            UsageTotals::from_usage(&plain).cost_usd,
            Decimal::from_str("0.0030169999999999997").unwrap()
        );
        assert_eq!(json_decimal(&json!(2)), Decimal::from_i64(2));
    }

    #[test]
    fn assistant_message_text_joins_text_blocks() {
        let message = json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "a"},
                {"type": "toolCall", "id": "t", "name": "x", "arguments": {}},
                {"type": "text", "text": "b"}
            ],
            "model": "m", "provider": "p", "stopReason": "stop",
            "usage": {"input": 1, "output": 2, "cost": {"total": 0.5}}
        });
        let parsed = AssistantMessage::from_message(&message).unwrap();
        assert_eq!(parsed.text, "ab");
        assert_eq!(parsed.stop_reason.as_deref(), Some("stop"));
        assert_eq!(parsed.usage.output_tokens, 2);
        assert!(AssistantMessage::from_message(&json!({"role": "user"})).is_none());
    }
}

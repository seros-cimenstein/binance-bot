//! Client for the omp coding-agent RPC mode (`omp --mode rpc`).
//!
//! Layers, bottom up:
//! - [`codec`]: JSONL framing with protocol-v2 `rpc_chunk` reassembly and limits.
//! - [`transport`]: any `AsyncRead`/`AsyncWrite` pair, plus the `omp` process launcher with a
//!   bounded stderr ring buffer.
//! - [`client`]: request/response correlation, per-command timeouts, event broadcast and
//!   concurrent host-tool dispatch (including `host_tool_cancel`).
//! - [`session`]: [`AgentSession`], the high-level long-lived agent API (prompt turns with tool
//!   call records and usage totals, compaction, state, restart that re-opens the session dir).
//!
//! The crate is generic: it contains no trading logic.

pub mod client;
pub mod codec;
pub mod error;
pub mod session;
pub mod transport;
pub mod types;

pub use client::{ClientOptions, RpcClient, RpcEvent};
pub use error::{CloseReason, OmpError, Result};
pub use session::{AgentSession, SessionConfig};
pub use tokio_util::sync::CancellationToken;
pub use transport::{OmpProcess, ProcessConfig, StderrBuffer, Transport};
pub use types::{
    AssistantMessage, CompactResult, ContextUsage, HostTool, HostToolHandler, HostToolOutcome,
    ModelRef, OpenSessionResult, PromptError, PromptStatus, SessionState, ThinkingLevel,
    ToolCallRecord, TurnOutcome, UsageTotals,
};

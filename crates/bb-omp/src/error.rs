//! Typed errors for the omp RPC client.

use std::path::PathBuf;
use std::time::Duration;

use crate::codec::FrameError;
use crate::types::TurnOutcome;

/// Why the RPC connection stopped delivering frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The server closed stdout (normal shutdown or process exit).
    Eof,
    /// Reading stdout failed.
    Io(String),
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => f.write_str("server closed stdout"),
            Self::Io(message) => write!(f, "stdout read failed: {message}"),
        }
    }
}

/// Every failure surfaced by `bb-omp`.
#[derive(Debug, thiserror::Error)]
pub enum OmpError {
    /// The agent binary could not be started.
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Local I/O failure (process management, pipes).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The server did not complete the startup handshake.
    #[error("RPC handshake failed: {0}")]
    Handshake(String),
    /// A frame could not be decoded or encoded.
    #[error("RPC framing error: {0}")]
    Frame(#[from] FrameError),
    /// A payload did not match the expected wire shape.
    #[error("unexpected payload for {context}: {message}")]
    Decode { context: String, message: String },
    /// A command did not receive its response in time.
    #[error("{command} timed out after {after:?}")]
    Timeout { command: String, after: Duration },
    /// A prompt exceeded its deadline; it was aborted. Carries what the turn produced so far.
    #[error("prompt timed out after {after:?} and was aborted")]
    PromptTimeout {
        after: Duration,
        outcome: Box<TurnOutcome>,
    },
    /// The server answered a command with `success: false`.
    #[error("{command} failed: {message}")]
    CommandFailed {
        command: String,
        message: String,
        code: Option<String>,
    },
    /// The connection closed while the request was outstanding (transport without a process).
    #[error("RPC connection closed: {reason}")]
    Disconnected { reason: String },
    /// The omp process exited or closed its pipes while work was outstanding.
    #[error("omp process died ({status}): {reason}; stderr tail: {stderr}")]
    ProcessDied {
        status: String,
        reason: String,
        stderr: String,
    },
    /// The operation needs a process-backed session.
    #[error("{0} requires a process-backed session")]
    NotRestartable(&'static str),
}

impl OmpError {
    /// True when the error means the connection/process is gone and the session needs a restart.
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Self::Disconnected { .. } | Self::ProcessDied { .. })
    }

    pub(crate) fn disconnected(reason: impl std::fmt::Display) -> Self {
        Self::Disconnected {
            reason: reason.to_string(),
        }
    }

    pub(crate) fn decode(context: impl Into<String>, error: impl std::fmt::Display) -> Self {
        Self::Decode {
            context: context.into(),
            message: error.to_string(),
        }
    }
}

pub type Result<T, E = OmpError> = std::result::Result<T, E>;

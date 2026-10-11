//! Byte transports for the RPC client and the `omp` process launcher.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::process::{Child, Command};

use crate::error::{OmpError, Result};

/// A bidirectional byte stream: `reader` is the server's stdout, `writer` its stdin.
pub struct Transport {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
}

impl Transport {
    pub fn new(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
        }
    }
}

/// Bounded ring buffer of the most recent stderr bytes, for diagnostics.
#[derive(Debug, Clone)]
pub struct StderrBuffer {
    inner: Arc<Mutex<VecDeque<u8>>>,
    capacity: usize,
}

impl StderrBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(64 * 1024)))),
            capacity,
        }
    }

    pub fn push(&self, bytes: &[u8]) {
        if self.capacity == 0 {
            return;
        }
        let mut buf = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let keep = bytes.len().min(self.capacity);
        let bytes = &bytes[bytes.len() - keep..];
        let overflow = (buf.len() + bytes.len()).saturating_sub(self.capacity);
        buf.drain(..overflow);
        buf.extend(bytes);
    }

    /// The retained tail, lossily decoded.
    pub fn tail(&self) -> String {
        let buf = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (a, b) = buf.as_slices();
        let mut bytes = Vec::with_capacity(a.len() + b.len());
        bytes.extend_from_slice(a);
        bytes.extend_from_slice(b);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// How to launch the agent process.
#[derive(Debug, Clone)]
pub struct ProcessConfig {
    /// Binary to run (`omp` resolved via `PATH` by default).
    pub program: PathBuf,
    /// Full argument list.
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Environment variables removed from the inherited environment.
    pub env_remove: Vec<String>,
    /// Bytes of stderr retained for diagnostics.
    pub stderr_capacity: usize,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from("omp"),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            stderr_capacity: 64 * 1024,
        }
    }
}

/// A running agent process. The child is killed when this is dropped.
#[derive(Debug)]
pub struct OmpProcess {
    child: tokio::sync::Mutex<Child>,
    stderr: StderrBuffer,
    stderr_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pid: Option<u32>,
}

impl OmpProcess {
    /// Spawns the process with piped stdio and starts the stderr collector.
    pub fn spawn(config: &ProcessConfig) -> Result<(Self, Transport)> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        for key in &config.env_remove {
            command.env_remove(key);
        }
        command.envs(config.env.iter().map(|(k, v)| (k, v)));
        let mut child = command.spawn().map_err(|source| OmpError::Spawn {
            program: config.program.clone(),
            source,
        })?;
        let missing =
            |name: &str| OmpError::Io(std::io::Error::other(format!("child {name} was not piped")));
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
        let buffer = StderrBuffer::new(config.stderr_capacity);
        let stderr_task = tokio::spawn(collect_stderr(stderr, buffer.clone()));
        let pid = child.id();
        tracing::debug!(program = %config.program.display(), ?pid, "spawned omp");
        Ok((
            Self {
                child: tokio::sync::Mutex::new(child),
                stderr: buffer,
                stderr_task: tokio::sync::Mutex::new(Some(stderr_task)),
                pid,
            },
            Transport::new(stdout, stdin),
        ))
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn stderr(&self) -> &StderrBuffer {
        &self.stderr
    }

    /// Waits up to `timeout` for stderr to reach EOF so [`Self::stderr`] holds the final output.
    pub async fn wait_stderr(&self, timeout: Duration) {
        let mut task = self.stderr_task.lock().await;
        if let Some(handle) = task.as_mut()
            && tokio::time::timeout(timeout, handle).await.is_ok()
        {
            *task = None;
        }
    }

    /// Exit status if the process has already exited.
    pub async fn try_status(&self) -> Result<Option<ExitStatus>> {
        Ok(self.child.lock().await.try_wait()?)
    }

    /// Waits up to `timeout` for the process to exit.
    pub async fn wait_timeout(&self, timeout: Duration) -> Result<Option<ExitStatus>> {
        let mut child = self.child.lock().await;
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => Ok(Some(status?)),
            Err(_) => Ok(None),
        }
    }

    /// Kills the process and reaps it.
    pub async fn kill(&self) -> Result<ExitStatus> {
        let mut child = self.child.lock().await;
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        child.kill().await?;
        Ok(child.wait().await?)
    }
}

async fn collect_stderr(stderr: tokio::process::ChildStderr, buffer: StderrBuffer) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => return,
            Ok(_) => {
                tracing::trace!(target: "bb_omp::stderr", "{}", String::from_utf8_lossy(&line).trim_end());
                buffer.push(&line);
            }
            Err(error) => {
                tracing::debug!(%error, "omp stderr read failed");
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn stderr_buffer_keeps_tail() {
        let buffer = StderrBuffer::new(8);
        buffer.push(b"hello ");
        buffer.push(b"world!");
        assert_eq!(buffer.tail(), "o world!");
        buffer.push(b"0123456789abc");
        assert_eq!(buffer.tail(), "56789abc");
    }

    #[tokio::test]
    async fn spawn_reports_missing_binary() {
        let config = ProcessConfig {
            program: PathBuf::from("/nonexistent/omp-binary"),
            ..ProcessConfig::default()
        };
        assert!(matches!(
            OmpProcess::spawn(&config),
            Err(OmpError::Spawn { .. })
        ));
    }

    #[tokio::test]
    async fn spawn_captures_stderr_and_status() {
        let config = ProcessConfig {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), "echo oops >&2; exit 3".into()],
            ..ProcessConfig::default()
        };
        let (process, _transport) = OmpProcess::spawn(&config).unwrap();
        let status = process
            .wait_timeout(Duration::from_secs(5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(3));
        for _ in 0..50 {
            if !process.stderr().tail().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(process.stderr().tail(), "oops\n");
    }
}

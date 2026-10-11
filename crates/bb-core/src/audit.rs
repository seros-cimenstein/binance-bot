//! Append-only JSONL audit logs.
//!
//! Each record is one JSON object per line, flushed and `fsync`ed before
//! `append` returns so a crash never loses an acknowledged decision. Readers
//! skip malformed lines (a torn final line after power loss) instead of failing.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("audit record is not serialisable: {0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct JsonlLog {
    path: PathBuf,
}

impl JsonlLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, source: std::io::Error) -> AuditError {
        AuditError::Io {
            path: self.path.clone(),
            source,
        }
    }

    /// Serialise `record` as one line and durably append it.
    pub fn append<T: Serialize + ?Sized>(&self, record: &T) -> Result<(), AuditError> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| self.io(e))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| self.io(e))?;
        file.write_all(&line).map_err(|e| self.io(e))?;
        file.sync_data().map_err(|e| self.io(e))
    }

    /// All well-formed records, oldest first. Missing file = empty.
    pub fn read_all<T: DeserializeOwned>(&self) -> Result<Vec<T>, AuditError> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.io(e)),
        };
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| self.io(e))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<T>(&line) {
                Ok(rec) => out.push(rec),
                Err(err) => {
                    tracing::warn!(path = %self.path.display(), %err, "skipping malformed audit line")
                }
            }
        }
        Ok(out)
    }

    /// The newest `n` well-formed records, oldest first.
    pub fn read_tail<T: DeserializeOwned>(&self, n: usize) -> Result<Vec<T>, AuditError> {
        let mut all = self.read_all::<T>()?;
        let skip = all.len().saturating_sub(n);
        Ok(all.split_off(skip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_skips_torn_lines_and_keeps_order() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let log = JsonlLog::new(dir.path().join("nested/audit.jsonl"));
        for i in 0..5 {
            log.append(&serde_json::json!({ "i": i }))?;
        }
        let mut f = OpenOptions::new().append(true).open(log.path())?;
        f.write_all(b"{\"i\": 99")?; // torn write
        let tail: Vec<serde_json::Value> = log.read_tail(2)?;
        assert_eq!(
            tail,
            vec![serde_json::json!({"i": 3}), serde_json::json!({"i": 4})]
        );
        Ok(())
    }
}

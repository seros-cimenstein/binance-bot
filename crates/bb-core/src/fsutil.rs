//! File safety primitives: atomic replace, exclusive run locks, stop files.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Write `bytes` to `path` via a sibling temp file + fsync + rename, so readers
/// never observe a partially written file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    atomic_write(path, &bytes)
}

/// `Ok(None)` when the file does not exist.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Operator emergency stop: the presence of the file halts the owning runtime.
pub fn stop_requested(path: &Path) -> bool {
    path.exists()
}

/// Exclusive, non-blocking advisory lock held for the guard's lifetime.
/// Prevents two cycles of the same runtime from trading concurrently.
#[derive(Debug)]
pub struct RunLock {
    file: File,
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another process holds the run lock {0}")]
    Busy(PathBuf),
    #[error("cannot open run lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl RunLock {
    pub fn try_acquire(path: impl Into<PathBuf>) -> Result<Self, LockError> {
        let path = path.into();
        let io = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { file, path }),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Busy(path)),
            Err(std::fs::TryLockError::Error(e)) => Err(io(e)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_busy_until_first_dropped() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("x.lock");
        let first = RunLock::try_acquire(&p)?;
        assert!(matches!(RunLock::try_acquire(&p), Err(LockError::Busy(_))));
        drop(first);
        RunLock::try_acquire(&p)?;
        Ok(())
    }

    #[test]
    fn atomic_write_replaces_content() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("a/b.json");
        write_json_atomic(&p, &serde_json::json!({"v": 1}))?;
        write_json_atomic(&p, &serde_json::json!({"v": 2}))?;
        let v: Option<serde_json::Value> = read_json(&p)?;
        assert_eq!(v, Some(serde_json::json!({"v": 2})));
        Ok(())
    }
}

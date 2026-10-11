//! File-backed `grammers` session storage.
//!
//! Telethon's `.session` SQLite file cannot be reused, so the Rust ingester
//! keeps its own session as a JSON document (datacenter options + permanent
//! auth keys, cached peers, update state). It is created by
//! `signals-telegram-setup`, rewritten atomically on every change with `0600`
//! permissions, and must be treated like a password: it grants full access to
//! the Telegram account.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use grammers_session::types::{
    ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState,
};
use grammers_session::{BoxFuture, Session, SessionData};
use serde::{Deserialize, Serialize};

/// Default session location; distinct from Telethon's
/// `data/telegram-session.session` so the old file is never misread.
pub const DEFAULT_SESSION_PATH: &str = "data/telegram-session.grammers.json";
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(
        "telegram session {path} is not a grammers JSON session (re-run signals-telegram-setup): {source}"
    )]
    Format {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("telegram session io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("telegram session lock poisoned")]
    Poisoned,
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    home_dc: i32,
    dc_options: Vec<DcOption>,
    peers: Vec<PeerInfo>,
    updates_state: UpdatesState,
}

struct Data {
    home_dc: i32,
    dc_options: HashMap<i32, DcOption>,
    peers: HashMap<PeerId, PeerInfo>,
    updates_state: UpdatesState,
}

impl Data {
    fn fresh() -> Self {
        let d = SessionData::default();
        Self {
            home_dc: d.home_dc,
            dc_options: d.dc_options,
            peers: d.peer_infos,
            updates_state: d.updates_state,
        }
    }

    fn stored(&self) -> Stored {
        let mut dc_options: Vec<DcOption> = self.dc_options.values().cloned().collect();
        dc_options.sort_by_key(|d| d.id);
        let mut peers: Vec<PeerInfo> = self.peers.values().cloned().collect();
        peers.sort_by_key(|p| p.id().bot_api_dialog_id());
        Stored {
            version: FORMAT_VERSION,
            home_dc: self.home_dc,
            dc_options,
            peers,
            updates_state: self.updates_state.clone(),
        }
    }
}

pub struct JsonFileSession {
    path: PathBuf,
    data: Mutex<Data>,
    /// Serialises snapshot+write so a stale snapshot never overwrites a newer one.
    write_lock: tokio::sync::Mutex<()>,
}

impl JsonFileSession {
    /// Open an existing session file.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let bytes = std::fs::read(&path).map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        let stored: Stored =
            serde_json::from_slice(&bytes).map_err(|source| SessionError::Format {
                path: path.clone(),
                source,
            })?;
        let mut data = Data::fresh();
        data.home_dc = stored.home_dc;
        data.dc_options
            .extend(stored.dc_options.into_iter().map(|d| (d.id, d)));
        data.peers = stored.peers.into_iter().map(|p| (p.id(), p)).collect();
        data.updates_state = stored.updates_state;
        Ok(Self::with_data(path, data))
    }

    /// Open `path` if it exists, otherwise start a fresh (unauthorised) session
    /// that is written on the first change.
    pub fn open_or_create(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        if path.exists() {
            Self::open(path)
        } else {
            Ok(Self::with_data(path, Data::fresh()))
        }
    }

    fn with_data(path: PathBuf, data: Data) -> Self {
        Self {
            path,
            data: Mutex::new(data),
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn data(&self) -> Result<MutexGuard<'_, Data>, SessionError> {
        self.data.lock().map_err(|_| SessionError::Poisoned)
    }

    /// Persist the current state now.
    pub async fn save(&self) -> Result<(), SessionError> {
        let _guard = self.write_lock.lock().await;
        let bytes = {
            let data = self.data()?;
            serde_json::to_vec_pretty(&data.stored()).map_err(|source| SessionError::Format {
                path: self.path.clone(),
                source,
            })?
        };
        write_private(&self.path, &bytes).map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })
    }

    /// Apply `change`; persist only when it reports a modification.
    async fn mutate(&self, change: impl FnOnce(&mut Data) -> bool) -> Result<(), SessionError> {
        let changed = change(&mut *self.data()?);
        if changed { self.save().await } else { Ok(()) }
    }
}

/// Atomic replace with owner-only permissions.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

impl Session for JsonFileSession {
    type Error = SessionError;

    fn home_dc_id(&self) -> Result<i32, SessionError> {
        Ok(self.data()?.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(self.mutate(move |d| {
            let changed = d.home_dc != dc_id;
            d.home_dc = dc_id;
            changed
        }))
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, SessionError> {
        Ok(self.data()?.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), SessionError>> {
        let dc_option = dc_option.clone();
        Box::pin(self.mutate(move |d| {
            d.dc_options.insert(dc_option.id, dc_option.clone()) != Some(dc_option)
        }))
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, SessionError>> {
        Box::pin(async move {
            let data = self.data()?;
            if peer.bot_api_dialog_id().is_some() {
                return Ok(data.peers.get(&peer).cloned());
            }
            // The self-user sentinel resolves to whichever user is flagged as self.
            Ok(data
                .peers
                .values()
                .find(|p| {
                    matches!(
                        p,
                        PeerInfo::User {
                            is_self: Some(true),
                            ..
                        }
                    )
                })
                .cloned())
        })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), SessionError>> {
        let peer = peer.clone();
        Box::pin(self.mutate(move |d| match d.peers.get_mut(&peer.id()) {
            Some(existing) => {
                let before = existing.clone();
                existing.extend_info(&peer);
                *existing != before
            }
            None => {
                d.peers.insert(peer.id(), peer);
                true
            }
        }))
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, SessionError>> {
        Box::pin(async move { Ok(self.data()?.updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(self.mutate(move |d| {
            let before = d.updates_state.clone();
            let state = &mut d.updates_state;
            match update {
                UpdateState::All(all) => *state = all,
                UpdateState::Primary { pts, date, seq } => {
                    state.pts = pts;
                    state.date = date;
                    state.seq = seq;
                }
                UpdateState::Secondary { qts } => state.qts = qts,
                UpdateState::Channel { id, pts } => {
                    state.channels.retain(|c| c.id != id);
                    state.channels.push(ChannelState { id, pts });
                }
            }
            *state != before
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn state_round_trips_through_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let session = JsonFileSession::open_or_create(&path).unwrap();
        assert!(!path.exists());
        session.set_home_dc_id(4).await.unwrap();
        session
            .cache_peer(&PeerInfo::User {
                id: 42,
                auth: None,
                bot: Some(false),
                is_self: Some(true),
            })
            .await
            .unwrap();
        session
            .set_update_state(UpdateState::Channel { id: 7, pts: 9 })
            .await
            .unwrap();

        let reopened = JsonFileSession::open(&path).unwrap();
        assert_eq!(reopened.home_dc_id().unwrap(), 4);
        assert!(reopened.dc_option(2).unwrap().is_some());
        let me = reopened.peer(PeerId::self_user()).await.unwrap();
        assert_eq!(me.map(|p| p.id()), Some(PeerId::user_unchecked(42)));
        assert_eq!(
            reopened.updates_state().await.unwrap().channels,
            vec![ChannelState { id: 7, pts: 9 }]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn telethon_sqlite_file_is_rejected_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram-session.session");
        std::fs::write(&path, b"SQLite format 3\0garbage").unwrap();
        let err = JsonFileSession::open(&path).err().unwrap().to_string();
        assert!(err.contains("signals-telegram-setup"), "{err}");
    }
}

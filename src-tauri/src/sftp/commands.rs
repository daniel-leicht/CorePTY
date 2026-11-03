//! IPC commands that open SFTP filesystems on SSH connections and register
//! them with the file manager. One filesystem per connection, shared by every
//! panel showing it, so a move between two panels on one host stays a
//! server-side rename.

use crate::connections::{ConnectionRegistry, HostKeys, SshAuth, SshTarget};
use crate::vfs::{sftp::SftpFs, FsId, FsRegistry, Location, LocationView, VPath, Vfs};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};
use tauri::State;
use uuid::Uuid;

struct Opened {
    fs: Arc<SftpFs>,
    connection_id: String,
    label: String,
    /// Panels using it; it's closed when the last one lets go.
    users: usize,
}

/// Tauri-managed state: the open SFTP filesystems.
#[derive(Default)]
pub struct SftpSessions {
    open: Mutex<HashMap<FsId, Opened>>,
}

impl SftpSessions {
    fn open(&self) -> MutexGuard<'_, HashMap<FsId, Opened>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpConnectOptions {
    /// Use this live SSH connection (no second login).
    #[serde(default)]
    pub connection_id: Option<String>,
    /// Otherwise log in, as an SSH tab would.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub auth: Option<SshAuth>,
}

fn default_port() -> u16 {
    22
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpView {
    pub fs: FsId,
    pub connection_id: String,
    pub label: String,
    pub home: LocationView,
    /// Something worth telling the user, such as a newly trusted host key.
    pub notice: Option<String>,
}

pub fn fs_id(connection_id: &str) -> FsId {
    FsId(format!("sftp:{connection_id}"))
}

impl SftpConnectOptions {
    fn target(&self) -> Option<SshTarget> {
        Some(SshTarget {
            host: self.host.clone()?,
            port: self.port,
            username: self.username.clone()?,
            auth: self.auth.clone()?,
        })
    }
}

async fn view(fs: &FsId, opened: &Arc<SftpFs>, connection_id: &str, label: &str) -> SftpView {
    let home = opened
        .home()
        .await
        .unwrap_or_else(|_| VPath::posix(b"/".to_vec()));
    SftpView {
        fs: fs.clone(),
        connection_id: connection_id.to_string(),
        label: label.to_string(),
        home: Location {
            fs: fs.clone(),
            path: home,
        }
        .view(),
        notice: None,
    }
}

/// Open SFTP on a live connection, or log in first. Call `sftp_close` once
/// per successful `sftp_connect`.
#[tauri::command]
pub async fn sftp_connect(
    connections: State<'_, ConnectionRegistry>,
    registry: State<'_, FsRegistry>,
    sessions: State<'_, SftpSessions>,
    options: SftpConnectOptions,
) -> Result<SftpView, String> {
    // Already open on this connection: share it.
    if let Some(connection_id) = options.connection_id.as_deref() {
        let id = fs_id(connection_id);
        let shared = {
            let mut open = sessions.open();
            match open.get_mut(&id) {
                Some(opened) if !opened.fs.is_closed() => {
                    opened.users += 1;
                    Some((opened.fs.clone(), opened.label.clone()))
                }
                _ => None,
            }
        };
        if let Some((fs, label)) = shared {
            return Ok(view(&id, &fs, connection_id, &label).await);
        }
    }

    let notices = Mutex::new(Vec::<String>::new());
    let lease = match options
        .connection_id
        .as_deref()
        .and_then(|id| connections.attach(id))
    {
        Some(lease) => lease,
        None => {
            let target = options.target().ok_or_else(|| {
                "that SSH connection has closed; connect again to browse its files".to_string()
            })?;
            let report = |message: String| {
                notices
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message);
            };
            connections
                .connect(
                    Uuid::new_v4().to_string(),
                    &target,
                    &HostKeys::user(),
                    &report,
                )
                .await?
        }
    };
    let connection_id = lease.id.clone();
    let label = lease.label();
    let fs = Arc::new(SftpFs::over_connection(lease).await?);
    let id = fs_id(&connection_id);

    // Another panel may have opened it meanwhile: keep the first one.
    let fs = {
        let mut open = sessions.open();
        match open.get_mut(&id) {
            Some(opened) if !opened.fs.is_closed() => {
                opened.users += 1;
                opened.fs.clone()
            }
            _ => {
                registry.register(id.clone(), fs.clone());
                open.insert(
                    id.clone(),
                    Opened {
                        fs: fs.clone(),
                        connection_id: connection_id.clone(),
                        label: label.clone(),
                        users: 1,
                    },
                );
                fs
            }
        }
    };
    let mut result = view(&id, &fs, &connection_id, &label).await;
    let notices = notices.into_inner().unwrap_or_else(|e| e.into_inner());
    result.notice = (!notices.is_empty()).then(|| notices.join("; "));
    Ok(result)
}

/// A panel stopped using `fs`. The last one closes the SFTP session (and the
/// SSH connection, unless a terminal tab still uses it).
#[tauri::command]
pub fn sftp_close(registry: State<'_, FsRegistry>, sessions: State<'_, SftpSessions>, fs: FsId) {
    let mut open = sessions.open();
    let Some(opened) = open.get_mut(&fs) else {
        return;
    };
    opened.users = opened.users.saturating_sub(1);
    if opened.users == 0 {
        open.remove(&fs);
        registry.unregister(&fs);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpSessionView {
    pub fs: FsId,
    pub connection_id: String,
    pub label: String,
    pub users: usize,
    pub connected: bool,
}

/// The open SFTP filesystems (for the location menu).
#[tauri::command]
pub fn sftp_sessions(sessions: State<'_, SftpSessions>) -> Vec<SftpSessionView> {
    let mut views: Vec<_> = sessions
        .open()
        .iter()
        .map(|(fs, opened)| SftpSessionView {
            fs: fs.clone(),
            connection_id: opened.connection_id.clone(),
            label: opened.label.clone(),
            users: opened.users,
            connected: !opened.fs.is_closed(),
        })
        .collect();
    views.sort_by(|a, b| a.label.cmp(&b.label));
    views
}

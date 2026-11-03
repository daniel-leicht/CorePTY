//! IPC commands that open SFTP filesystems on SSH connections and register
//! them with the file manager. One filesystem per connection, shared by every
//! panel showing it, so a move between two panels on one host stays a
//! server-side rename. Each is an [`SftpMount`]: when its connection drops,
//! `sftp://status` says so, and `sftp_reconnect` carries on under the same id.

use crate::connections::{ConnectionLease, ConnectionRegistry, HostKeys, SshAuth, SshTarget};
use crate::vfs::{
    mount::SftpMount, sftp::SftpFs, FsId, FsRegistry, Location, LocationView, VPath, Vfs,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

struct Opened {
    mount: Arc<SftpMount>,
    /// The SSH connection the current session runs on.
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

/// Sent as `sftp://status` when a filesystem drops or comes back.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpStatus {
    pub fs: FsId,
    pub connected: bool,
    /// Why it dropped.
    pub message: Option<String>,
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

async fn view(fs: &FsId, mount: &Arc<SftpMount>, connection_id: &str, label: &str) -> SftpView {
    let home = mount
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

/// A lease on the requested connection, logging in if it's gone (and the
/// options say how). Returns notices for the user (a newly trusted key).
async fn lease_for(
    connections: &ConnectionRegistry,
    options: &SftpConnectOptions,
) -> Result<(ConnectionLease, Option<String>), String> {
    if let Some(lease) = options
        .connection_id
        .as_deref()
        .and_then(|id| connections.attach(id))
    {
        return Ok((lease, None));
    }
    let target = options.target().ok_or_else(|| {
        "that SSH connection has closed; connect again to browse its files".to_string()
    })?;
    let notices = Mutex::new(Vec::<String>::new());
    let report = |message: String| {
        notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message);
    };
    let lease = connections
        .connect(
            Uuid::new_v4().to_string(),
            &target,
            &HostKeys::user(),
            &report,
        )
        .await?;
    let notices = notices.into_inner().unwrap_or_else(|e| e.into_inner());
    Ok((lease, (!notices.is_empty()).then(|| notices.join("; "))))
}

/// Tell the UI whenever `mount` drops or comes back. Ends with the mount.
fn report_status(app: AppHandle, fs: FsId, mount: &Arc<SftpMount>) {
    let Some(mut connection) = mount.connection() else {
        return;
    };
    let mount = Arc::downgrade(mount);
    tauri::async_runtime::spawn(async move {
        while connection.changed().await.is_ok() {
            let connected = *connection.borrow_and_update();
            let message = mount.upgrade().and_then(|mount| mount.lost_reason());
            let _ = app.emit(
                "sftp://status",
                SftpStatus {
                    fs: fs.clone(),
                    connected,
                    message,
                },
            );
        }
    });
}

/// Open SFTP on a live connection, or log in first. Call `sftp_close` once
/// per successful `sftp_connect`.
#[tauri::command]
pub async fn sftp_connect(
    app: AppHandle,
    connections: State<'_, ConnectionRegistry>,
    registry: State<'_, FsRegistry>,
    sessions: State<'_, SftpSessions>,
    options: SftpConnectOptions,
) -> Result<SftpView, String> {
    // Already open on this connection: share it.
    if let Some(connection_id) = options.connection_id.as_deref() {
        let shared = {
            let mut open = sessions.open();
            open.iter_mut()
                .find(|(_, opened)| {
                    opened.connection_id == connection_id && opened.mount.is_connected()
                })
                .map(|(id, opened)| {
                    opened.users += 1;
                    (id.clone(), opened.mount.clone(), opened.label.clone())
                })
        };
        if let Some((id, mount, label)) = shared {
            return Ok(view(&id, &mount, connection_id, &label).await);
        }
    }

    let (lease, notice) = lease_for(&connections, &options).await?;
    let connection_id = lease.id.clone();
    let label = lease.label();
    let fs = SftpFs::over_connection(lease).await?;
    let id = FsId(format!("sftp:{connection_id}"));

    // Another panel may have opened it meanwhile: keep the first one.
    let mount = {
        let mut open = sessions.open();
        match open.get_mut(&id) {
            Some(opened) if opened.mount.is_connected() => {
                opened.users += 1;
                opened.mount.clone()
            }
            _ => {
                let mount = SftpMount::new(fs);
                registry.register(id.clone(), mount.clone());
                report_status(app, id.clone(), &mount);
                open.insert(
                    id.clone(),
                    Opened {
                        mount: mount.clone(),
                        connection_id: connection_id.clone(),
                        label: label.clone(),
                        users: 1,
                    },
                );
                mount
            }
        }
    };
    let mut result = view(&id, &mount, &connection_id, &label).await;
    result.notice = notice;
    Ok(result)
}

/// Carry on with `fs` over a new session (after its connection dropped):
/// panels keep their folders and paused transfers continue.
#[tauri::command]
pub async fn sftp_reconnect(
    connections: State<'_, ConnectionRegistry>,
    sessions: State<'_, SftpSessions>,
    fs: FsId,
    options: SftpConnectOptions,
) -> Result<SftpView, String> {
    let mount = sessions
        .open()
        .get(&fs)
        .map(|opened| opened.mount.clone())
        .ok_or_else(|| format!("{} is not open", fs.0))?;
    let (lease, notice) = lease_for(&connections, &options).await?;
    let connection_id = lease.id.clone();
    let label = lease.label();
    mount.replace(SftpFs::over_connection(lease).await?);
    if let Some(opened) = sessions.open().get_mut(&fs) {
        opened.connection_id = connection_id.clone();
        opened.label = label.clone();
    }
    let mut result = view(&fs, &mount, &connection_id, &label).await;
    result.notice = notice;
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
            connected: opened.mount.is_connected(),
        })
        .collect();
    views.sort_by(|a, b| a.label.cmp(&b.label));
    views
}

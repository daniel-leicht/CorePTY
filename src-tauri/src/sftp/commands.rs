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
    /// Where it was first opened (host, port, user): a reconnect must go
    /// back there.
    server: Server,
    /// Panels, transfer jobs and edited files using it; it's closed when the
    /// last one lets go.
    users: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Server {
    host: String,
    port: u16,
    username: String,
}

impl Server {
    fn of(lease: &ConnectionLease) -> Self {
        Self {
            host: lease.host.to_ascii_lowercase(),
            port: lease.port,
            username: lease.username.clone(),
        }
    }
}

type OpenMap = Arc<Mutex<HashMap<FsId, Opened>>>;

/// Tauri-managed state: the open SFTP filesystems.
#[derive(Default)]
pub struct SftpSessions {
    open: OpenMap,
}

fn lock(open: &OpenMap) -> MutexGuard<'_, HashMap<FsId, Opened>> {
    open.lock().unwrap_or_else(|e| e.into_inner())
}

/// One user fewer of `fs`; the last one closes it (and the SSH connection,
/// unless a terminal tab still uses it).
fn release(open: &OpenMap, registry: &FsRegistry, fs: &FsId) {
    let mut open = lock(open);
    let Some(opened) = open.get_mut(fs) else {
        return;
    };
    opened.users = opened.users.saturating_sub(1);
    if opened.users == 0 {
        open.remove(fs);
        registry.unregister(fs);
    }
}

impl SftpSessions {
    fn open(&self) -> MutexGuard<'_, HashMap<FsId, Opened>> {
        lock(&self.open)
    }

    /// One more user of `fs`, if it's open.
    pub fn retain(&self, fs: &FsId) -> bool {
        match self.open().get_mut(fs) {
            Some(opened) => {
                opened.users += 1;
                true
            }
            None => false,
        }
    }

    pub fn release(&self, registry: &FsRegistry, fs: &FsId) {
        release(&self.open, registry, fs);
    }

    /// Keep `fs` open while the returned guard lives (`None` for one that
    /// isn't an open SFTP filesystem: local, say).
    pub fn use_of(&self, registry: &FsRegistry, fs: &FsId) -> Option<SessionUse> {
        self.retain(fs).then(|| SessionUse {
            open: self.open.clone(),
            registry: registry.clone(),
            fs: fs.clone(),
        })
    }
}

/// Keeps an SFTP filesystem open, and so reconnectable, while it lives: held
/// by a transfer job or an edited file, which may outlast every panel
/// showing the server.
pub struct SessionUse {
    open: OpenMap,
    registry: FsRegistry,
    fs: FsId,
}

impl Drop for SessionUse {
    fn drop(&mut self) {
        release(&self.open, &self.registry, &self.fs);
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
        let existing = sessions
            .open()
            .iter()
            .find(|(_, opened)| opened.connection_id == connection_id)
            .map(|(id, opened)| (id.clone(), opened.mount.clone(), opened.label.clone()));
        if let Some((id, mount, label)) = existing {
            // A session that ended on a connection that's still up (the
            // server's sftp process went away, say) starts again in the same
            // mount, so its other panels and paused jobs carry on.
            let usable = mount.is_connected()
                || match connections.attach(connection_id) {
                    Some(lease) => {
                        let _reconnecting = mount.reconnecting().await;
                        if !mount.is_connected() {
                            mount.replace(SftpFs::over_connection(lease).await?);
                        }
                        true
                    }
                    None => false,
                };
            if usable && share(&sessions, &id, &mount) {
                return Ok(view(&id, &mount, connection_id, &label).await);
            }
        }
    }

    let (lease, notice) = lease_for(&connections, &options).await?;
    let connection_id = lease.id.clone();
    let label = lease.label();
    let server = Server::of(&lease);
    let fs = SftpFs::over_connection(lease).await?;
    let id = FsId(format!("sftp:{connection_id}"));

    let mount = {
        let mut open = sessions.open();
        match open.get_mut(&id) {
            Some(opened) => {
                // Another panel opened it meanwhile (keep the first), or its
                // session ended (carry on in the same mount, which panels and
                // paused jobs hold).
                if !opened.mount.is_connected() {
                    opened.mount.replace(fs);
                    opened.connection_id = connection_id.clone();
                    opened.label = label.clone();
                }
                opened.users += 1;
                opened.mount.clone()
            }
            None => {
                let mount = SftpMount::new(fs);
                registry.register(id.clone(), mount.clone());
                report_status(app, id.clone(), &mount);
                open.insert(
                    id.clone(),
                    Opened {
                        mount: mount.clone(),
                        connection_id: connection_id.clone(),
                        label: label.clone(),
                        server,
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

/// Count one more panel on `id`, if it's still open as `mount`.
fn share(sessions: &SftpSessions, id: &FsId, mount: &Arc<SftpMount>) -> bool {
    match sessions.open().get_mut(id) {
        Some(opened) if Arc::ptr_eq(&opened.mount, mount) => {
            opened.users += 1;
            true
        }
        _ => false,
    }
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
    // One at a time; and a mount that's connected (another panel reconnected
    // it first) keeps its session: replacing it would cut off its transfers.
    let _reconnecting = mount.reconnecting().await;
    if mount.is_connected() {
        let current = sessions
            .open()
            .get(&fs)
            .map(|opened| (opened.connection_id.clone(), opened.label.clone()));
        if let Some((connection_id, label)) = current {
            return Ok(view(&fs, &mount, &connection_id, &label).await);
        }
    }
    let (lease, notice) = lease_for(&connections, &options).await?;
    let connection_id = lease.id.clone();
    let label = lease.label();
    // The same server and account: paused jobs and edited files carry on
    // under this id, and must not land somewhere else.
    let first = sessions
        .open()
        .get(&fs)
        .map(|opened| (opened.server.clone(), opened.label.clone()));
    if let Some((server, first_label)) = first {
        if server != Server::of(&lease) {
            return Err(format!(
                "that login is for {label}, not {first_label}; open it on its own instead"
            ));
        }
    }
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
    sessions.release(&registry, &fs);
}

/// One more panel on an open filesystem (connected or not): a panel going
/// back to a server another panel or a job keeps open. False when it's not
/// open any more. Release it with `sftp_close`.
#[tauri::command]
pub fn sftp_retain(sessions: State<'_, SftpSessions>, fs: FsId) -> bool {
    sessions.retain(&fs)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_job_keeps_a_server_open_after_its_last_panel_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let Some((sftp, _server)) = crate::sftp::testing::local_sftp(dir.path()).await else {
            return;
        };
        let fs = FsId("sftp:conn".to_string());
        let registry = FsRegistry::default();
        let sessions = SftpSessions::default();
        let mount = SftpMount::new(sftp);
        registry.register(fs.clone(), mount.clone());
        sessions.open().insert(
            fs.clone(),
            Opened {
                mount,
                connection_id: "conn".to_string(),
                label: "me@host".to_string(),
                server: Server {
                    host: "host".to_string(),
                    port: 22,
                    username: "me".to_string(),
                },
                users: 1, // the panel that opened it
            },
        );
        let job = sessions.use_of(&registry, &fs).expect("open");
        // The panel closes; the job still has the server (and can reconnect it).
        sessions.release(&registry, &fs);
        assert!(registry.get(&fs).is_ok());
        assert!(sessions.retain(&fs), "a panel can come back to it");
        sessions.release(&registry, &fs);
        // The job ends: now it closes.
        drop(job);
        assert!(registry.get(&fs).is_err());
        assert!(!sessions.retain(&fs));
        assert!(sessions.use_of(&registry, &FsId::local()).is_none());
    }
}

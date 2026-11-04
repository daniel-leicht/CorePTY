//! Shared SSH connections.
//!
//! A connection is authenticated once and then carries any number of channels:
//! terminal shells, and the SFTP sessions of the file manager. Each user holds
//! a [`ConnectionLease`]; the connection hangs up when the last lease is
//! dropped, and is reported lost (and forgotten) when the server goes away.

use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, RwLock, Weak,
    },
    time::Duration,
};

use russh::client::{self, AuthResult, DisconnectReason, Handle, Msg};
use russh::keys::known_hosts::{
    check_known_hosts_path, known_host_keys_path, learn_known_hosts_path,
};
use russh::keys::{load_secret_key, ssh_key, Error as KeysError, PrivateKeyWithHashAlg};
use russh::{Channel, Disconnect};
use serde::{Deserialize, Serialize};
use tauri::State;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Password prompts and key checks; a server that never answers mustn't
/// leave a tab (and its socket) stuck forever.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// Opening a channel on a connection that may have died unnoticed.
const OPEN_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 3 } else { 10 });
/// Keepalives notice a dead peer (and keep NAT/firewall state alive) on idle
/// connections, such as an SFTP panel nobody is clicking in. A peer that
/// stops answering is declared lost after about (MAX + 1) × INTERVAL = 2 min.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_MAX: usize = 3;
/// How much a server may send on a channel before we acknowledge it. russh's
/// 2 MiB (topped up at half) held SFTP downloads to ~1 MiB per round trip:
/// 23 MB/s at 40 ms. Readers always drain their channels, so a bigger window
/// costs no memory beyond what each one asked for.
const WINDOW_SIZE: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SshAuth {
    Password {
        password: String,
    },
    Key {
        #[serde(rename = "keyPath")]
        path: String,
        #[serde(default)]
        passphrase: Option<String>,
    },
}

/// Where to connect, and as whom.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
}

/// The trusted host keys file: `~/.ssh/known_hosts` (tests use a temp file).
#[derive(Debug, Clone)]
pub struct HostKeys {
    pub path: Option<PathBuf>,
}

impl HostKeys {
    pub fn user() -> Self {
        Self {
            path: home_dir().map(|home| home.join(".ssh").join("known_hosts")),
        }
    }
}

/// Sent to the frontend as `ssh://connection` events.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectionEvent {
    /// The server went away (or the network did). Its users' channels fail.
    Lost { id: String, message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionView {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Shells and file panels currently using it.
    pub users: usize,
}

pub struct SshConnection {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Tells this connection apart from an earlier one under the same id.
    generation: u64,
    handle: Handle<Client>,
    users: AtomicUsize,
}

impl SshConnection {
    /// `user@host` (with `:port` when it isn't 22; IPv6 hosts in brackets).
    pub fn label(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 22 {
            format!("{}@{host}", self.username)
        } else {
            format!("{}@{host}:{}", self.username, self.port)
        }
    }

    /// A new session channel, for a shell or a subsystem such as SFTP. Times
    /// out rather than wait for keepalives to notice a connection that died
    /// quietly (a sleeping laptop, a VPN switch).
    pub async fn open_session(&self) -> Result<Channel<Msg>, String> {
        match tokio::time::timeout(OPEN_TIMEOUT, self.handle.channel_open_session()).await {
            Ok(Ok(channel)) => Ok(channel),
            Ok(Err(e)) => Err(format!("could not open a channel on {}: {e}", self.label())),
            Err(_) => Err(format!("{} is not responding", self.label())),
        }
    }

    /// Whether this connection goes where `target` asks (credentials aside).
    fn serves(&self, target: &SshTarget) -> bool {
        self.host.eq_ignore_ascii_case(&target.host)
            && self.port == target.port
            && self.username == target.username
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    fn view(&self) -> ConnectionView {
        ConnectionView {
            id: self.id.clone(),
            label: self.label(),
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            users: self.users.load(Ordering::SeqCst),
        }
    }
}

type EventSink = Arc<dyn Fn(ConnectionEvent) + Send + Sync>;

#[derive(Default)]
struct Inner {
    connections: Mutex<HashMap<String, Arc<SshConnection>>>,
    events: RwLock<Option<EventSink>>,
    generations: AtomicU64,
}

impl Inner {
    fn connections(&self) -> MutexGuard<'_, HashMap<String, Arc<SshConnection>>> {
        self.connections.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The server side ended connection `generation` of `id`: forget it and
    /// tell the UI. A connection we closed ourselves is already gone from the
    /// map, and a newer one under the same id is left alone.
    fn lost(&self, id: &str, generation: u64, message: String) {
        {
            let mut connections = self.connections();
            if connections
                .get(id)
                .is_none_or(|known| known.generation != generation)
            {
                return;
            }
            connections.remove(id);
        }
        let sink = self
            .events
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(sink) = sink {
            sink(ConnectionEvent::Lost {
                id: id.to_string(),
                message,
            });
        }
    }
}

/// Tauri-managed state: every live SSH connection, by id.
#[derive(Clone, Default)]
pub struct ConnectionRegistry {
    inner: Arc<Inner>,
}

impl ConnectionRegistry {
    pub fn set_events(&self, sink: EventSink) {
        *self.inner.events.write().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// Connect, check the host key, authenticate, and register the connection
    /// under `id`. `progress` receives status lines for the UI (such as a
    /// newly trusted host key).
    pub async fn connect(
        &self,
        id: String,
        target: &SshTarget,
        host_keys: &HostKeys,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ConnectionLease, String> {
        if self
            .inner
            .connections()
            .get(&id)
            .is_some_and(|known| !known.is_closed())
        {
            return Err(format!("connection {id} is already open"));
        }
        let generation = self.inner.generations.fetch_add(1, Ordering::SeqCst);
        let config = Arc::new(client::Config {
            keepalive_interval: Some(KEEPALIVE_INTERVAL),
            keepalive_max: KEEPALIVE_MAX,
            window_size: WINDOW_SIZE,
            preferred: preferred_algorithms(host_keys, &target.host, target.port),
            ..Default::default()
        });
        let reject_reason = Arc::new(Mutex::new(None::<String>));
        let trusted = Arc::new(Mutex::new(None::<String>));
        let handler = Client {
            id: id.clone(),
            generation,
            host: target.host.clone(),
            port: target.port,
            host_keys: host_keys.clone(),
            reject_reason: reject_reason.clone(),
            trusted: trusted.clone(),
            registry: Arc::downgrade(&self.inner),
        };

        let connecting = client::connect(config, (target.host.as_str(), target.port), handler);
        let mut handle = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
            Err(_) => return Err("connection timed out".to_string()),
            Ok(Ok(handle)) => handle,
            Ok(Err(e)) => {
                // A rejected host key surfaces here as a generic handshake
                // failure; prefer the specific reason (changed key / MITM…).
                if let Some(reason) = take(&reject_reason) {
                    return Err(reason);
                }
                return Err(format!("could not connect: {e}"));
            }
        };
        if let Some(fingerprint) = take(&trusted) {
            progress(format!("new host key trusted — {fingerprint}"));
        }
        match tokio::time::timeout(AUTH_TIMEOUT, authenticate(&mut handle, target)).await {
            Ok(result) => result?,
            Err(_) => return Err("the server didn't finish authentication in time".to_string()),
        }

        let connection = Arc::new(SshConnection {
            id: id.clone(),
            host: target.host.clone(),
            port: target.port,
            username: target.username.clone(),
            generation,
            handle,
            users: AtomicUsize::new(1),
        });
        self.inner.connections().insert(id, connection.clone());
        Ok(ConnectionLease {
            connection,
            registry: self.clone(),
        })
    }

    /// Share an existing connection (no new login), if it's still up.
    pub fn attach(&self, id: &str) -> Option<ConnectionLease> {
        self.attach_if(id, |_| true)
    }

    /// [`Self::attach`], but only to a connection that goes to `target`'s
    /// host and port as its user (a stale id never lands a shell elsewhere).
    pub fn attach_to(&self, id: &str, target: &SshTarget) -> Option<ConnectionLease> {
        self.attach_if(id, |connection| connection.serves(target))
    }

    fn attach_if(
        &self,
        id: &str,
        suits: impl Fn(&SshConnection) -> bool,
    ) -> Option<ConnectionLease> {
        let connections = self.inner.connections();
        let connection = connections.get(id)?;
        if connection.is_closed() || !suits(connection) {
            return None;
        }
        connection.users.fetch_add(1, Ordering::SeqCst);
        Some(ConnectionLease {
            connection: connection.clone(),
            registry: self.clone(),
        })
    }

    pub fn list(&self) -> Vec<ConnectionView> {
        let mut views: Vec<ConnectionView> = self
            .inner
            .connections()
            .values()
            .map(|connection| connection.view())
            .collect();
        views.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));
        views
    }

    fn release(&self, connection: &Arc<SshConnection>) {
        let mut connections = self.inner.connections();
        if connection.users.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        // The last user is done: forget it and hang up.
        if connections
            .get(&connection.id)
            .is_some_and(|known| Arc::ptr_eq(known, connection))
        {
            connections.remove(&connection.id);
        }
        drop(connections);
        let connection = connection.clone();
        tauri::async_runtime::spawn(async move {
            let _ = connection
                .handle
                .disconnect(Disconnect::ByApplication, "", "en")
                .await;
        });
    }
}

/// A share of a connection. Dropping the last one closes the connection.
pub struct ConnectionLease {
    connection: Arc<SshConnection>,
    registry: ConnectionRegistry,
}

impl Deref for ConnectionLease {
    type Target = SshConnection;

    fn deref(&self) -> &SshConnection {
        &self.connection
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        self.registry.release(&self.connection);
    }
}

/// The live SSH connections (terminal tabs and SFTP panels share them).
#[tauri::command]
pub fn ssh_connections(registry: State<'_, ConnectionRegistry>) -> Vec<ConnectionView> {
    registry.list()
}

fn take(slot: &Mutex<Option<String>>) -> Option<String> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

async fn authenticate(handle: &mut Handle<Client>, target: &SshTarget) -> Result<(), String> {
    match &target.auth {
        SshAuth::Password { password } => {
            let result = handle
                .authenticate_password(target.username.as_str(), password.as_str())
                .await
                .map_err(|e| format!("authentication error: {e}"))?;
            if !matches!(result, AuthResult::Success) {
                return Err("authentication failed — check the username and password".into());
            }
        }
        SshAuth::Key { path, passphrase } => {
            let key = load_secret_key(path, passphrase.as_deref())
                .map_err(|e| format!("could not load private key: {e}"))?;
            ensure_safe_private_key_algorithm(key.algorithm())?;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
            let result = handle
                .authenticate_publickey(target.username.as_str(), key)
                .await
                .map_err(|e| format!("authentication error: {e}"))?;
            if !matches!(result, AuthResult::Success) {
                return Err("key rejected by server — is the public key installed?".into());
            }
        }
    }
    Ok(())
}

fn ensure_safe_private_key_algorithm(algorithm: ssh_key::Algorithm) -> Result<(), String> {
    if algorithm.is_rsa() {
        return Err(
            "RSA private keys are disabled because the available implementation has an \
             unresolved timing-side-channel advisory; use an Ed25519 or ECDSA key"
                .into(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Host-key verification (known_hosts, trust-on-first-use)
// ---------------------------------------------------------------------------

pub struct Client {
    id: String,
    generation: u64,
    host: String,
    port: u16,
    host_keys: HostKeys,
    /// Set when we reject the server's host key, so the connect error carries
    /// the specific reason instead of russh's generic handshake failure.
    reject_reason: Arc<Mutex<Option<String>>>,
    /// The fingerprint of a host key trusted for the first time.
    trusted: Arc<Mutex<Option<String>>>,
    registry: Weak<Inner>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        match verify_host_key(&self.host_keys, &self.host, self.port, server_public_key) {
            Ok(newly_trusted) => {
                *self.trusted.lock().unwrap_or_else(|p| p.into_inner()) = newly_trusted;
                Ok(true)
            }
            Err(reason) => {
                *self.reject_reason.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason);
                Ok(false)
            }
        }
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        if let Some(inner) = self.registry.upgrade() {
            let message = match &reason {
                DisconnectReason::ReceivedDisconnect(info) if !info.message.is_empty() => {
                    format!("the server closed the connection: {}", info.message)
                }
                DisconnectReason::ReceivedDisconnect(_) => {
                    "the server closed the connection".to_string()
                }
                DisconnectReason::Error(e) => format!("the connection was lost: {e}"),
            };
            inner.lost(&self.id, self.generation, message);
        }
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(e) => Err(e),
        }
    }
}

/// Decide whether to trust the server's host key. `Ok(None)`: already known;
/// `Ok(Some(fingerprint))`: first contact, now recorded (trust on first use);
/// `Err(reason)`: refused. Fails closed on anything it can't positively verify.
fn verify_host_key(
    host_keys: &HostKeys,
    host: &str,
    port: u16,
    key: &ssh_key::PublicKey,
) -> Result<Option<String>, String> {
    let Some(path) = host_keys.path.as_ref() else {
        // No home directory → nowhere to anchor trust. Fail closed rather than
        // blindly accept a host key we can never verify against anything.
        return Err("cannot locate ~/.ssh/known_hosts to verify the host key — refused".into());
    };
    if is_revoked(path, key) {
        return Err("the server's host key is marked @revoked in known_hosts — refused".into());
    }
    match check_known_hosts_path(host, port, key, path) {
        Ok(true) => Ok(None),
        Ok(false) => {
            // Keys of another type are on record for this host: a server
            // doesn't drop its old key type overnight, but an impostor that
            // lacks it would offer something else. Refuse, as OpenSSH warns.
            let known = known_host_keys_path(host, port, path)
                .map_err(|e| format!("could not verify the host key — refused ({e})"))?;
            if let Some((_, recorded)) = known.first() {
                return Err(format!(
                    "the server offered a {} host key, but a {} key is on record for it — \
                     refused (possible MITM; remove the old entry from known_hosts if the \
                     server really changed)",
                    key.algorithm(),
                    recorded.algorithm()
                ));
            }
            if let Some(directory) = path.parent() {
                std::fs::create_dir_all(directory).map_err(|e| {
                    format!("could not prepare the SSH host-key store — refused ({e})")
                })?;
            }
            require_host_key_persisted(learn_known_hosts_path(host, port, key, path))?;
            Ok(Some(key.fingerprint(ssh_key::HashAlg::Sha256).to_string()))
        }
        Err(KeysError::KeyChanged { .. }) => {
            Err("host key has CHANGED since last connect — refused (possible MITM)".into())
        }
        // An unexpected error (unreadable / corrupt known_hosts, etc.): fail
        // closed — never silently trust a key we could not verify.
        Err(e) => Err(format!("could not verify the host key — refused ({e})")),
    }
}

/// Host-key types to negotiate: those on record for this host first, so a
/// server with several keys presents the one we can verify.
fn preferred_algorithms(host_keys: &HostKeys, host: &str, port: u16) -> russh::Preferred {
    let mut preferred = russh::Preferred::default();
    let recorded: Vec<ssh_key::Algorithm> = host_keys
        .path
        .as_ref()
        .and_then(|path| known_host_keys_path(host, port, path).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|(_, key)| key.algorithm())
        .collect();
    if !recorded.is_empty() {
        let mut order: Vec<ssh_key::Algorithm> = Vec::new();
        for algorithm in recorded.iter().chain(preferred.key.iter()) {
            if preferred.key.contains(algorithm) && !order.contains(algorithm) {
                order.push(algorithm.clone());
            }
        }
        preferred.key = order.into();
    }
    preferred
}

/// `@revoked` lines (which russh's known_hosts reader skips): a revoked key is
/// refused for every host.
fn is_revoked(path: &std::path::Path, key: &ssh_key::PublicKey) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let offered = key.to_openssh().ok();
    contents.lines().any(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("@revoked") {
            return false;
        }
        let (Some(_hosts), Some(kind), Some(data)) = (fields.next(), fields.next(), fields.next())
        else {
            return false;
        };
        offered
            .as_deref()
            .is_some_and(|offered| offered.split_whitespace().take(2).eq([kind, data]))
    })
}

fn require_host_key_persisted<E: std::fmt::Display>(result: Result<(), E>) -> Result<(), String> {
    result.map_err(|e| format!("could not record the new host key — refused ({e})"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use russh::keys::{ssh_key::Algorithm, ssh_key::HashAlg, PrivateKey};
    use russh::server::{self, Auth, Msg as ServerMsg, Session};
    use russh::ChannelId;
    use std::sync::atomic::AtomicU64;
    use tokio::net::TcpListener;

    /// A tiny in-process SSH server: password "secret", answers `exec` with
    /// "ok:<command>", runs a real OpenSSH `sftp-server` for the `sftp`
    /// subsystem (see [`start_sftp_server`]), and counts the TCP connections
    /// it accepted.
    pub(crate) struct TestServer {
        pub addr: std::net::SocketAddr,
        pub connections: Arc<AtomicU64>,
        pub closed: Arc<AtomicU64>,
        /// Session channels the client closed (CHANNEL_CLOSE received).
        pub channel_closes: Arc<AtomicU64>,
        /// Server-side handles of accepted sessions, to drop them from the server end.
        pub handles: Arc<Mutex<Vec<server::Handle>>>,
    }

    /// How a [`TestServer`] behaves beyond the basics.
    #[derive(Default)]
    pub(crate) struct ServerOptions {
        pub key: Option<PrivateKey>,
        /// Where the `sftp` subsystem runs; `None` refuses SFTP.
        pub sftp_root: Option<PathBuf>,
        /// Like sshd's MaxSessions: open session channels per connection.
        pub max_sessions: Option<u64>,
        /// The server's receive window (a small one makes a paste wait).
        pub window_size: Option<u32>,
    }

    #[derive(Clone)]
    struct Handler {
        handles: Arc<Mutex<Vec<server::Handle>>>,
        channels: Arc<Mutex<HashMap<ChannelId, russh::Channel<ServerMsg>>>>,
        sftp_root: Option<PathBuf>,
        max_sessions: Option<u64>,
        open_sessions: Arc<AtomicU64>,
        channel_closes: Arc<AtomicU64>,
        /// Channels running the echoing shell (not SFTP or exec).
        shells: Arc<Mutex<std::collections::HashSet<ChannelId>>>,
    }

    impl server::Handler for Handler {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            _user: &str,
            password: &str,
        ) -> Result<Auth, Self::Error> {
            Ok(if password == "secret" {
                Auth::Accept
            } else {
                Auth::reject()
            })
        }

        async fn channel_open_session(
            &mut self,
            channel: russh::Channel<ServerMsg>,
            reply: server::ChannelOpenHandle,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if self
                .max_sessions
                .is_some_and(|max| self.open_sessions.load(Ordering::SeqCst) >= max)
            {
                reply
                    .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
                return Ok(());
            }
            self.open_sessions.fetch_add(1, Ordering::SeqCst);
            self.handles.lock().unwrap().push(session.handle());
            self.channels.lock().unwrap().insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn channel_close(
            &mut self,
            channel: ChannelId,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.lock().unwrap().remove(&channel);
            self.channel_closes.fetch_add(1, Ordering::SeqCst);
            self.open_sessions.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }

        #[allow(clippy::too_many_arguments)] // the trait's signature
        async fn pty_request(
            &mut self,
            channel: ChannelId,
            _term: &str,
            _cols: u32,
            _rows: u32,
            _pixel_width: u32,
            _pixel_height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            Ok(())
        }

        /// A "shell" that prints a prompt and echoes its input, as a tty does.
        /// As with sshd and a PTY, EOF alone does not end it.
        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // Unread channel objects would fill up and stall the session loop.
            self.channels.lock().unwrap().remove(&channel);
            self.shells.lock().unwrap().insert(channel);
            session.channel_success(channel)?;
            session.data(channel, b"$ ".to_vec())?;
            Ok(())
        }

        async fn data(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if !self.shells.lock().unwrap().contains(&channel) {
                return Ok(());
            }
            for chunk in data.chunks(1024) {
                session.data(channel, chunk.to_vec())?;
            }
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            channel: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            let taken = self.channels.lock().unwrap().remove(&channel);
            let binary = crate::sftp::testing::sftp_server();
            let (Some(open), Some(root), Some(binary), "sftp") =
                (taken, self.sftp_root.clone(), binary, name)
            else {
                session.channel_failure(channel)?;
                return Ok(());
            };
            session.channel_success(channel)?;
            tokio::spawn(async move {
                let mut child = crate::sftp::testing::spawn_server(&binary, &root);
                let mut to_server = child.stdin.take().unwrap();
                let mut from_server = child.stdout.take().unwrap();
                let (mut from_client, mut to_client) = tokio::io::split(open.into_stream());
                let up = async {
                    let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
                    drop(to_server); // EOF: sftp-server exits
                };
                let down = async {
                    let _ = tokio::io::copy(&mut from_server, &mut to_client).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut to_client).await;
                };
                tokio::join!(up, down);
                let _ = child.wait().await;
            });
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.lock().unwrap().remove(&channel);
            let mut reply = b"ok:".to_vec();
            reply.extend_from_slice(data);
            session.channel_success(channel)?;
            session.data(channel, reply)?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            Ok(())
        }
    }

    pub(crate) async fn start_server(key: PrivateKey) -> TestServer {
        start_with(ServerOptions {
            key: Some(key),
            ..ServerOptions::default()
        })
        .await
    }

    /// A test server whose `sftp` subsystem serves `root`.
    pub(crate) async fn start_sftp_server(root: PathBuf) -> TestServer {
        start_with(ServerOptions {
            sftp_root: Some(root),
            ..ServerOptions::default()
        })
        .await
    }

    pub(crate) async fn start_with(options: ServerOptions) -> TestServer {
        let mut config = server::Config {
            auth_rejection_time: Duration::from_millis(10),
            inactivity_timeout: None,
            keys: vec![options.key.unwrap_or_else(host_key)],
            ..Default::default()
        };
        if let Some(window_size) = options.window_size {
            config.window_size = window_size;
        }
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicU64::new(0));
        let channel_closes = Arc::new(AtomicU64::new(0));
        let handles = Arc::new(Mutex::new(Vec::new()));
        let (count, done, kept, closes) = (
            connections.clone(),
            closed.clone(),
            handles.clone(),
            channel_closes.clone(),
        );
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let config = config.clone();
                let handler = Handler {
                    handles: kept.clone(),
                    channels: Arc::default(),
                    sftp_root: options.sftp_root.clone(),
                    max_sessions: options.max_sessions,
                    open_sessions: Arc::default(),
                    channel_closes: closes.clone(),
                    shells: Arc::default(),
                };
                let done = done.clone();
                tokio::spawn(async move {
                    if let Ok(running) = server::run_stream(config, socket, handler).await {
                        let _ = running.await;
                    }
                    done.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        TestServer {
            addr,
            connections,
            closed,
            channel_closes,
            handles,
        }
    }

    pub(crate) fn host_key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
    }

    pub(crate) fn target(server: &TestServer, password: &str) -> SshTarget {
        SshTarget {
            host: "127.0.0.1".to_string(),
            port: server.addr.port(),
            username: "tester".to_string(),
            auth: SshAuth::Password {
                password: password.to_string(),
            },
        }
    }

    pub(crate) fn temp_host_keys(dir: &tempfile::TempDir) -> HostKeys {
        HostKeys {
            path: Some(dir.path().join("known_hosts")),
        }
    }

    async fn exec(lease: &ConnectionLease, command: &str) -> String {
        let mut channel = lease.open_session().await.unwrap();
        channel.exec(true, command).await.unwrap();
        let mut output = Vec::new();
        while let Some(message) = channel.wait().await {
            if let russh::ChannelMsg::Data { data } = message {
                output.extend_from_slice(&data);
            }
        }
        String::from_utf8(output).unwrap()
    }

    async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn the_first_connection_trusts_the_host_key_and_later_ones_verify_it() {
        let server = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let keys = temp_host_keys(&dir);
        let registry = ConnectionRegistry::default();
        let messages = Mutex::new(Vec::new());
        let progress = |message: String| messages.lock().unwrap().push(message);

        let lease = registry
            .connect("a".into(), &target(&server, "secret"), &keys, &progress)
            .await
            .unwrap();
        assert!(messages.lock().unwrap()[0].starts_with("new host key trusted — SHA256:"));
        assert!(std::fs::read_to_string(dir.path().join("known_hosts"))
            .unwrap()
            .contains("ssh-ed25519"));
        drop(lease);

        messages.lock().unwrap().clear();
        let again = registry
            .connect("b".into(), &target(&server, "secret"), &keys, &progress)
            .await
            .unwrap();
        assert!(
            messages.lock().unwrap().is_empty(),
            "a known key is not announced again"
        );
        drop(again);
    }

    #[tokio::test]
    async fn a_changed_host_key_is_refused() {
        let first = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let keys = temp_host_keys(&dir);
        let registry = ConnectionRegistry::default();
        let lease = registry
            .connect("a".into(), &target(&first, "secret"), &keys, &|_| {})
            .await
            .unwrap();
        drop(lease);

        // Same host:port in known_hosts, but a server with a different key.
        let impostor = start_server(host_key()).await;
        let mut spoofed = target(&impostor, "secret");
        let known = std::fs::read_to_string(dir.path().join("known_hosts")).unwrap();
        std::fs::write(
            dir.path().join("known_hosts"),
            known.replace(
                &first.addr.port().to_string(),
                &impostor.addr.port().to_string(),
            ),
        )
        .unwrap();
        spoofed.port = impostor.addr.port();
        let error = registry
            .connect("b".into(), &spoofed, &keys, &|_| {})
            .await
            .err()
            .unwrap();
        assert!(error.contains("CHANGED"), "{error}");
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn a_wrong_password_is_reported_and_nothing_is_registered() {
        let server = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let error = registry
            .connect(
                "a".into(),
                &target(&server, "nope"),
                &temp_host_keys(&dir),
                &|_| {},
            )
            .await
            .err()
            .unwrap();
        assert!(error.contains("authentication failed"), "{error}");
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn leases_share_one_connection_until_the_last_is_dropped() {
        let server = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let first = registry
            .connect(
                "conn".into(),
                &target(&server, "secret"),
                &temp_host_keys(&dir),
                &|_| {},
            )
            .await
            .unwrap();
        let second = registry
            .attach("conn")
            .expect("a live connection can be shared");
        assert_eq!(exec(&first, "one").await, "ok:one");
        assert_eq!(exec(&second, "two").await, "ok:two");
        assert_eq!(
            server.connections.load(Ordering::SeqCst),
            1,
            "one TCP connection"
        );
        assert_eq!(registry.list()[0].users, 2);
        assert_eq!(
            registry.list()[0].label,
            format!("tester@127.0.0.1:{}", server.addr.port())
        );

        drop(first);
        assert_eq!(
            registry.list()[0].users,
            1,
            "still open for the second user"
        );
        assert_eq!(exec(&second, "three").await, "ok:three");
        drop(second);
        assert!(registry.list().is_empty());
        assert!(registry.attach("conn").is_none());
        assert!(
            eventually(|| server.closed.load(Ordering::SeqCst) == 1).await,
            "the last lease hangs up"
        );
    }

    #[tokio::test]
    async fn a_connection_lost_on_the_server_side_is_reported_and_forgotten() {
        let server = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        registry.set_events(Arc::new(move |event| sink.lock().unwrap().push(event)));
        let lease = registry
            .connect(
                "conn".into(),
                &target(&server, "secret"),
                &temp_host_keys(&dir),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(exec(&lease, "x").await, "ok:x");

        // The server hangs up on us.
        let handle = server.handles.lock().unwrap()[0].clone();
        let _ = handle
            .disconnect(Disconnect::ByApplication, "maintenance".into(), "en".into())
            .await;
        assert!(
            eventually(|| !events.lock().unwrap().is_empty()).await,
            "lost is reported"
        );
        match &events.lock().unwrap()[0] {
            ConnectionEvent::Lost { id, message } => {
                assert_eq!(id, "conn");
                assert!(message.contains("maintenance"), "{message}");
            }
        }
        assert!(registry.list().is_empty());
        assert!(registry.attach("conn").is_none());
        assert!(
            lease.open_session().await.is_err(),
            "the lease's connection is dead"
        );
        drop(lease); // releasing a lost connection is harmless
    }

    #[test]
    fn unknown_host_is_rejected_when_its_key_cannot_be_recorded() {
        let result = require_host_key_persisted::<&str>(Err("read-only file"));
        assert_eq!(
            result.unwrap_err(),
            "could not record the new host key — refused (read-only file)"
        );
    }

    #[test]
    fn rsa_private_keys_are_rejected_until_the_timing_advisory_is_resolved() {
        let rsa = Algorithm::Rsa {
            hash: Some(HashAlg::Sha256),
        };
        assert!(ensure_safe_private_key_algorithm(rsa)
            .unwrap_err()
            .contains("timing-side-channel"));
        assert!(ensure_safe_private_key_algorithm(Algorithm::Ed25519).is_ok());
    }

    #[test]
    fn a_missing_known_hosts_location_fails_closed() {
        let key = host_key();
        let error =
            verify_host_key(&HostKeys { path: None }, "h", 22, key.public_key()).unwrap_err();
        assert!(error.contains("refused"));
    }

    #[tokio::test]
    async fn a_known_host_offering_a_different_key_type_is_refused() {
        let genuine = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let keys = temp_host_keys(&dir);
        let registry = ConnectionRegistry::default();
        drop(
            registry
                .connect("a".into(), &target(&genuine, "secret"), &keys, &|_| {})
                .await
                .unwrap(),
        );
        // An impostor on the recorded host:port with only an ECDSA key.
        let ecdsa = PrivateKey::random(
            &mut rand::rng(),
            Algorithm::Ecdsa {
                curve: russh::keys::ssh_key::EcdsaCurve::NistP256,
            },
        )
        .unwrap();
        let impostor = start_server(ecdsa).await;
        let path = dir.path().join("known_hosts");
        let known = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            known.replace(
                &genuine.addr.port().to_string(),
                &impostor.addr.port().to_string(),
            ),
        )
        .unwrap();
        let error = registry
            .connect("b".into(), &target(&impostor, "secret"), &keys, &|_| {})
            .await
            .err()
            .unwrap();
        assert!(error.contains("on record"), "{error}");
        // Nothing new was learned.
        let recorded = std::fs::read_to_string(&path).unwrap();
        assert_eq!(recorded.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    }

    #[tokio::test]
    async fn a_revoked_host_key_is_refused() {
        let key = host_key();
        let public = key.public_key().to_openssh().unwrap();
        let server = start_server(key).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("known_hosts"),
            format!("@revoked * {public}\n"),
        )
        .unwrap();
        let error = ConnectionRegistry::default()
            .connect(
                "a".into(),
                &target(&server, "secret"),
                &temp_host_keys(&dir),
                &|_| {},
            )
            .await
            .err()
            .unwrap();
        assert!(error.contains("@revoked"), "{error}");
    }

    #[tokio::test]
    async fn an_open_id_is_not_reused_and_attach_to_checks_the_destination() {
        let server = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let keys = temp_host_keys(&dir);
        let registry = ConnectionRegistry::default();
        let lease = registry
            .connect("x".into(), &target(&server, "secret"), &keys, &|_| {})
            .await
            .unwrap();
        let error = registry
            .connect("x".into(), &target(&server, "secret"), &keys, &|_| {})
            .await
            .err()
            .unwrap();
        assert!(error.contains("already open"), "{error}");
        assert_eq!(server.connections.load(Ordering::SeqCst), 1);

        let same = target(&server, "other password: not compared");
        assert!(registry.attach_to("x", &same).is_some());
        let mut elsewhere = target(&server, "secret");
        elsewhere.username = "root".into();
        assert!(registry.attach_to("x", &elsewhere).is_none());
        elsewhere = target(&server, "secret");
        elsewhere.port += 1;
        assert!(registry.attach_to("x", &elsewhere).is_none());
        drop(lease);
    }

    /// A TCP relay to `upstream` that can stop forwarding (a dead Wi-Fi link:
    /// sockets stay open, nothing gets through).
    async fn relay(
        upstream: std::net::SocketAddr,
    ) -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicBool>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let frozen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let freeze = frozen.clone();
        tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut server = tokio::net::TcpStream::connect(upstream).await.unwrap();
            let (mut up, mut down) = (vec![0; 65536], vec![0; 65536]);
            loop {
                tokio::select! {
                    read = client.read(&mut up) => match read {
                        Ok(0) | Err(_) => return,
                        Ok(n) if !freeze.load(Ordering::SeqCst) => {
                            if server.write_all(&up[..n]).await.is_err() { return; }
                        }
                        Ok(_) => {}
                    },
                    read = server.read(&mut down) => match read {
                        Ok(0) | Err(_) => return,
                        Ok(n) if !freeze.load(Ordering::SeqCst) => {
                            if client.write_all(&down[..n]).await.is_err() { return; }
                        }
                        Ok(_) => {}
                    },
                }
            }
        });
        (addr, frozen)
    }

    #[tokio::test]
    async fn opening_a_channel_gives_up_on_a_connection_that_stopped_answering() {
        let server = start_server(host_key()).await;
        let (addr, frozen) = relay(server.addr).await;
        let dir = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let mut through_relay = target(&server, "secret");
        through_relay.port = addr.port();
        let lease = registry
            .connect("c".into(), &through_relay, &temp_host_keys(&dir), &|_| {})
            .await
            .unwrap();
        frozen.store(true, Ordering::SeqCst);
        let shared = registry.attach("c").expect("not noticed as dead yet");
        let started = std::time::Instant::now();
        let error = shared.open_session().await.err().unwrap();
        assert!(error.contains("not responding"), "{error}");
        assert!(started.elapsed() < OPEN_TIMEOUT * 3);
        drop((shared, lease));
    }

    #[tokio::test]
    async fn hanging_up_deliberately_is_not_reported_as_lost() {
        let server = start_server(host_key()).await;
        let dir = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        registry.set_events(Arc::new(move |event| sink.lock().unwrap().push(event)));
        for i in 0..5 {
            let lease = registry
                .connect(
                    format!("c{i}"),
                    &target(&server, "secret"),
                    &temp_host_keys(&dir),
                    &|_| {},
                )
                .await
                .unwrap();
            drop(lease);
        }
        assert!(eventually(|| server.closed.load(Ordering::SeqCst) == 5).await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(events.lock().unwrap().is_empty());
    }
}

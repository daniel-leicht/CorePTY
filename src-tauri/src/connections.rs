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
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, RwLock, Weak,
    },
    time::Duration,
};

use russh::client::{self, AuthResult, DisconnectReason, Handle, Msg};
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{load_secret_key, ssh_key, Error as KeysError, PrivateKeyWithHashAlg};
use russh::{Channel, Disconnect};
use serde::{Deserialize, Serialize};
use tauri::State;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Keepalives notice a dead peer (and keep NAT/firewall state alive) on idle
/// connections, such as an SFTP panel nobody is clicking in.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_MAX: usize = 3;

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
    handle: Handle<Client>,
    users: AtomicUsize,
}

impl SshConnection {
    /// `user@host` (with `:port` when it isn't 22).
    pub fn label(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.username, self.host)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port)
        }
    }

    /// A new session channel, for a shell or a subsystem such as SFTP.
    pub async fn open_session(&self) -> Result<Channel<Msg>, String> {
        self.handle
            .channel_open_session()
            .await
            .map_err(|e| format!("could not open a channel on {}: {e}", self.label()))
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
}

impl Inner {
    fn connections(&self) -> MutexGuard<'_, HashMap<String, Arc<SshConnection>>> {
        self.connections.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The server side ended `id`: forget it and tell the UI. A connection we
    /// closed ourselves is already gone from the map, so it isn't reported.
    fn lost(&self, id: &str, message: String) {
        if self.connections().remove(id).is_none() {
            return;
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
        let config = Arc::new(client::Config {
            keepalive_interval: Some(KEEPALIVE_INTERVAL),
            keepalive_max: KEEPALIVE_MAX,
            ..Default::default()
        });
        let reject_reason = Arc::new(Mutex::new(None::<String>));
        let trusted = Arc::new(Mutex::new(None::<String>));
        let handler = Client {
            id: id.clone(),
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
        authenticate(&mut handle, target).await?;

        let connection = Arc::new(SshConnection {
            id: id.clone(),
            host: target.host.clone(),
            port: target.port,
            username: target.username.clone(),
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
        let connections = self.inner.connections();
        let connection = connections.get(id)?;
        if connection.is_closed() {
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
            inner.lost(&self.id, message);
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
    match check_known_hosts_path(host, port, key, path) {
        Ok(true) => Ok(None),
        Ok(false) => {
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
    /// "ok:<command>", and counts the TCP connections it accepted.
    pub(crate) struct TestServer {
        pub addr: std::net::SocketAddr,
        pub connections: Arc<AtomicU64>,
        pub closed: Arc<AtomicU64>,
        /// Server-side handles of accepted sessions, to drop them from the server end.
        pub handles: Arc<Mutex<Vec<server::Handle>>>,
    }

    #[derive(Clone)]
    struct Handler {
        handles: Arc<Mutex<Vec<server::Handle>>>,
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
            _channel: russh::Channel<ServerMsg>,
            reply: server::ChannelOpenHandle,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.handles.lock().unwrap().push(session.handle());
            reply.accept().await;
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
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
        let config = Arc::new(server::Config {
            auth_rejection_time: Duration::from_millis(10),
            inactivity_timeout: None,
            keys: vec![key],
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicU64::new(0));
        let handles = Arc::new(Mutex::new(Vec::new()));
        let (count, done, kept) = (connections.clone(), closed.clone(), handles.clone());
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let config = config.clone();
                let handler = Handler {
                    handles: kept.clone(),
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
}

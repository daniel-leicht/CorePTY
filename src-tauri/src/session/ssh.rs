// SSH sessions via `russh` (pure-Rust SSH). Supports password and public-key
// auth, host-key verification against ~/.ssh/known_hosts (trust-on-first-use),
// an interactive PTY + shell, live resize, and streamed I/O.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, AuthResult, Handle};
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{load_secret_key, ssh_key, Error as KeysError, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use uuid::Uuid;

use super::{
    emit_data, emit_exit, emit_status, SessionInfo, SessionInput, SessionKind, SessionManager,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshOptions {
    #[serde(default)]
    pub id: Option<String>,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
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

fn default_port() -> u16 {
    22
}
fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

/// Synchronous entry point: registers the session, kicks off the async
/// connect + shell driver, and returns immediately so the UI shows a
/// "connecting" tab. All progress/errors surface via events.
pub fn connect(
    app: AppHandle,
    manager: &SessionManager,
    opts: SshOptions,
) -> Result<SessionInfo, String> {
    let id = opts
        .id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if manager.exists(&id) {
        return Err(format!("session id already in use: {id}"));
    }
    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| format!("{}@{}", opts.username, opts.host));
    let info = SessionInfo {
        id: id.clone(),
        kind: SessionKind::Ssh,
        title,
    };

    let (tx, rx) = unbounded_channel::<SessionInput>();
    manager.register(info.clone(), tx);
    emit_status(&app, &id, "connecting", Some(format!("{}:{}", opts.host, opts.port)));

    let app_task = app.clone();
    let id_task = id.clone();
    tauri::async_runtime::spawn(async move {
        match run_session(app_task.clone(), id_task.clone(), opts, rx).await {
            Ok(code) => emit_exit(&app_task, &id_task, code, None),
            Err(e) => {
                emit_status(&app_task, &id_task, "error", Some(e.clone()));
                emit_exit(&app_task, &id_task, None, Some(e));
            }
        }
        app_task.state::<SessionManager>().remove(&id_task);
    });

    Ok(info)
}

enum Action {
    Read(Option<ChannelMsg>),
    Input(Option<SessionInput>),
}

async fn run_session(
    app: AppHandle,
    id: String,
    opts: SshOptions,
    mut rx: UnboundedReceiver<SessionInput>,
) -> Result<Option<i32>, String> {
    let config = Arc::new(client::Config::default());
    let reject_reason = Arc::new(Mutex::new(None::<String>));
    let handler = Client {
        app: app.clone(),
        id: id.clone(),
        host: opts.host.clone(),
        port: opts.port,
        reject_reason: reject_reason.clone(),
    };

    let connect_fut = client::connect(config, (opts.host.as_str(), opts.port), handler);
    let mut session = match tokio::time::timeout(Duration::from_secs(20), connect_fut).await {
        Err(_) => return Err("connection timed out".to_string()),
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            // A rejected host key surfaces here as a generic handshake failure;
            // prefer the specific reason (changed key / possible MITM / etc.).
            if let Some(reason) = reject_reason.lock().unwrap_or_else(|p| p.into_inner()).take() {
                return Err(reason);
            }
            return Err(format!("could not connect: {e}"));
        }
    };

    authenticate(&mut session, &opts).await?;

    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| format!("failed to open channel: {e}"))?;
    channel
        .request_pty(
            false,
            "xterm-256color",
            opts.cols as u32,
            opts.rows as u32,
            0,
            0,
            // VERASE = 127 (DEL): make the remote's tty erase char match the 0x7f
            // xterm sends for Backspace. Without it the remote defaults to ^H/0x08,
            // and vim (which keys backspace off the erase char) inserts a literal
            // ^? instead of deleting. Unset modes fall back to the remote defaults.
            &[(russh::Pty::VERASE, 127)],
        )
        .await
        .map_err(|e| format!("pty request failed: {e}"))?;
    channel
        .request_shell(true)
        .await
        .map_err(|e| format!("shell request failed: {e}"))?;

    emit_status(&app, &id, "connected", None);

    let mut channel = channel;
    let mut exit_code: Option<i32> = None;
    loop {
        let action = tokio::select! {
            msg = channel.wait() => Action::Read(msg),
            input = rx.recv() => Action::Input(input),
        };
        match action {
            Action::Read(Some(ChannelMsg::Data { data })) => emit_data(&app, &id, &data[..]),
            Action::Read(Some(ChannelMsg::ExtendedData { data, .. })) => {
                emit_data(&app, &id, &data[..])
            }
            Action::Read(Some(ChannelMsg::ExitStatus { exit_status })) => {
                exit_code = Some(exit_status as i32);
            }
            Action::Read(Some(ChannelMsg::Eof))
            | Action::Read(Some(ChannelMsg::Close))
            | Action::Read(None) => break,
            Action::Read(_) => {}
            Action::Input(Some(SessionInput::Data(bytes))) => {
                if channel.data(&bytes[..]).await.is_err() {
                    break;
                }
            }
            Action::Input(Some(SessionInput::Resize { cols, rows })) => {
                let _ = channel
                    .window_change(cols as u32, rows as u32, 0, 0)
                    .await;
            }
            Action::Input(Some(SessionInput::Close)) | Action::Input(None) => break,
        }
    }

    let _ = channel.eof().await;
    Ok(exit_code)
}

async fn authenticate(session: &mut Handle<Client>, opts: &SshOptions) -> Result<(), String> {
    match &opts.auth {
        SshAuth::Password { password } => {
            let result = session
                .authenticate_password(opts.username.as_str(), password.as_str())
                .await
                .map_err(|e| format!("authentication error: {e}"))?;
            if !matches!(result, AuthResult::Success) {
                return Err("authentication failed — check the username and password".into());
            }
        }
        SshAuth::Key { path, passphrase } => {
            let key = load_secret_key(path, passphrase.as_deref())
                .map_err(|e| format!("could not load private key: {e}"))?;
            let hash_alg = session
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
            let result = session
                .authenticate_publickey(opts.username.as_str(), key)
                .await
                .map_err(|e| format!("authentication error: {e}"))?;
            if !matches!(result, AuthResult::Success) {
                return Err("key rejected by server — is the public key installed?".into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Host-key verification (known_hosts, trust-on-first-use)
// ---------------------------------------------------------------------------

struct Client {
    app: AppHandle,
    id: String,
    host: String,
    port: u16,
    /// Set when we reject the server's host key, so the connect error carries the
    /// specific reason instead of russh's generic handshake failure.
    reject_reason: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        match verify_host_key(&self.app, &self.id, &self.host, self.port, server_public_key) {
            Ok(()) => Ok(true),
            Err(reason) => {
                *self.reject_reason.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason);
                Ok(false)
            }
        }
    }
}

/// Decide whether to trust the server's host key. `Ok(())` trusts it; `Err(reason)`
/// rejects it and carries a user-facing reason (so it isn't lost as a generic
/// handshake error). Fails closed on anything it cannot positively verify.
fn verify_host_key(
    app: &AppHandle,
    id: &str,
    host: &str,
    port: u16,
    key: &ssh_key::PublicKey,
) -> Result<(), String> {
    let Some(path) = known_hosts_file() else {
        // No home directory → nowhere to anchor trust. Fail closed rather than
        // blindly accept a host key we can never verify against anything.
        return Err("cannot locate ~/.ssh/known_hosts to verify the host key — refused".into());
    };
    // Ensure ~/.ssh exists so a first-connect can actually persist the new key.
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match check_known_hosts_path(host, port, key, &path) {
        Ok(true) => Ok(()),
        Ok(false) => {
            // First time we've seen this host: trust on first use and record it,
            // surfacing the fingerprint so the user can at least eyeball it.
            let _ = learn_known_hosts_path(host, port, key, &path);
            let fp = key.fingerprint(ssh_key::HashAlg::Sha256);
            emit_status(
                app,
                id,
                "connecting",
                Some(format!("new host key trusted — {fp}")),
            );
            Ok(())
        }
        Err(KeysError::KeyChanged { .. }) => {
            Err("host key has CHANGED since last connect — refused (possible MITM)".into())
        }
        // An unexpected error (unreadable / corrupt known_hosts, etc.): fail
        // closed — never silently trust a key we could not verify.
        Err(e) => Err(format!("could not verify the host key — refused ({e})")),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn known_hosts_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ssh").join("known_hosts"))
}

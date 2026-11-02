// SSH terminal sessions: an interactive PTY + shell on a shared connection
// (see `connections`), with live resize and streamed I/O. Duplicated tabs and
// file panels can share the same connection instead of logging in again.

use russh::ChannelMsg;
use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use uuid::Uuid;

use super::{
    emit_data, emit_exit, emit_status, SessionInfo, SessionInput, SessionKind, SessionManager,
};
use crate::connections::{ConnectionLease, ConnectionRegistry, HostKeys, SshAuth, SshTarget};

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
    /// Open the shell on this connection (no new login) while it is still up;
    /// otherwise connect as usual.
    #[serde(default)]
    pub connection_id: Option<String>,
}

impl SshOptions {
    fn target(&self) -> SshTarget {
        SshTarget {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            auth: self.auth.clone(),
        }
    }
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
    registry: &ConnectionRegistry,
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
    // Share the requested connection if it's still up (decided now, so the
    // returned info names the connection the shell will really use).
    let shared = opts
        .connection_id
        .as_deref()
        .and_then(|connection| registry.attach(connection));
    let connection_id = shared
        .as_ref()
        .map(|lease| lease.id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let info = SessionInfo {
        id: id.clone(),
        kind: SessionKind::Ssh,
        title,
        connection_id: Some(connection_id.clone()),
    };

    let (tx, rx) = unbounded_channel::<SessionInput>();
    manager.register(info.clone(), tx);
    emit_status(
        &app,
        &id,
        "connecting",
        Some(format!("{}:{}", opts.host, opts.port)),
    );

    let app_task = app.clone();
    let id_task = id.clone();
    let registry = registry.clone();
    tauri::async_runtime::spawn(async move {
        let lease = match shared {
            Some(lease) => Ok(lease),
            None => {
                let progress = |message: String| {
                    emit_status(&app_task, &id_task, "connecting", Some(message));
                };
                registry
                    .connect(connection_id, &opts.target(), &HostKeys::user(), &progress)
                    .await
            }
        };
        let result = match lease {
            Ok(lease) => run_shell(&app_task, &id_task, &opts, lease, rx).await,
            Err(e) => Err(e),
        };
        match result {
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

/// Run a PTY shell on the connection until it exits. The lease is held for the
/// shell's lifetime; the connection hangs up when no one else uses it.
async fn run_shell(
    app: &AppHandle,
    id: &str,
    opts: &SshOptions,
    lease: ConnectionLease,
    mut rx: UnboundedReceiver<SessionInput>,
) -> Result<Option<i32>, String> {
    let channel = lease.open_session().await?;
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

    emit_status(app, id, "connected", None);

    let mut channel = channel;
    let mut exit_code: Option<i32> = None;
    loop {
        let action = tokio::select! {
            msg = channel.wait() => Action::Read(msg),
            input = rx.recv() => Action::Input(input),
        };
        match action {
            Action::Read(Some(ChannelMsg::Data { data })) => emit_data(app, id, &data[..]),
            Action::Read(Some(ChannelMsg::ExtendedData { data, .. })) => {
                emit_data(app, id, &data[..])
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
                let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
            }
            Action::Input(Some(SessionInput::Close)) | Action::Input(None) => break,
        }
    }

    let _ = channel.eof().await;
    Ok(exit_code)
}

// SSH terminal sessions: an interactive PTY + shell on a shared connection
// (see `connections`), with live resize and streamed I/O. Duplicated tabs and
// file panels can share the same connection instead of logging in again.

use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use serde::Deserialize;
use std::sync::Arc;
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
    // Share the requested connection if it's still up and goes where this tab
    // asks to (decided now, so the returned info names the connection the
    // shell will most likely use).
    let shared = opts
        .connection_id
        .as_deref()
        .and_then(|connection| registry.attach_to(connection, &opts.target()));
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
        let progress = |message: String| {
            emit_status(&app_task, &id_task, "connecting", Some(message));
        };
        let keys = HostKeys::user();
        let opened = open_or_login(&registry, shared, connection_id, &opts, &keys, &progress).await;
        let result = match opened {
            Ok((lease, channel)) => {
                emit_status(&app_task, &id_task, "connected", None);
                let output = |data: &[u8]| emit_data(&app_task, &id_task, data);
                let code = drive_shell(channel, rx, &output).await;
                // The lease is held for the shell's lifetime; the connection
                // hangs up when no one else uses it.
                drop(lease);
                Ok(code)
            }
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

/// A shell on the shared connection when there is one, else on a new login
/// under `connection_id`.
async fn open_or_login(
    registry: &ConnectionRegistry,
    shared: Option<ConnectionLease>,
    connection_id: String,
    opts: &SshOptions,
    host_keys: &HostKeys,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<(ConnectionLease, Channel<Msg>), String> {
    let connection_id = match shared {
        Some(lease) => match open_shell(&lease, opts).await {
            Ok(channel) => return Ok((lease, channel)),
            // The connection can't take another session (it died unnoticed,
            // or the server limits sessions per connection): log in again,
            // as a new tab would.
            Err(reason) => {
                drop(lease);
                progress(format!("{reason} — logging in again"));
                Uuid::new_v4().to_string()
            }
        },
        None => connection_id,
    };
    let lease = registry
        .connect(connection_id, &opts.target(), host_keys, progress)
        .await?;
    let channel = open_shell(&lease, opts).await?;
    Ok((lease, channel))
}

/// A session channel with a PTY and a shell running in it.
async fn open_shell(lease: &ConnectionLease, opts: &SshOptions) -> Result<Channel<Msg>, String> {
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
    Ok(channel)
}

/// Run the shell until it exits or the tab closes; returns its exit code.
async fn drive_shell(
    channel: Channel<Msg>,
    mut rx: UnboundedReceiver<SessionInput>,
    output: &(dyn Fn(&[u8]) + Send + Sync),
) -> Option<i32> {
    // Keystrokes go out from their own task: a write waiting for window space
    // must never stop this loop from reading, or the unread backlog stalls
    // the whole shared connection (every tab and file panel on it).
    let (mut reader, writer) = channel.split();
    let writer = Arc::new(writer);
    let mut input = tokio::spawn({
        let writer = writer.clone();
        async move {
            while let Some(message) = rx.recv().await {
                match message {
                    SessionInput::Data(bytes) => {
                        if writer.data(&bytes[..]).await.is_err() {
                            break;
                        }
                    }
                    SessionInput::Resize { cols, rows } => {
                        let _ = writer.window_change(cols as u32, rows as u32, 0, 0).await;
                    }
                    SessionInput::Close => break,
                }
            }
        }
    });
    let mut exit_code: Option<i32> = None;
    loop {
        tokio::select! {
            message = reader.wait() => match message {
                Some(ChannelMsg::Data { data }) => output(&data[..]),
                Some(ChannelMsg::ExtendedData { data, .. }) => output(&data[..]),
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status as i32);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            // The tab closed (or its input ended).
            _ = &mut input => break,
        }
    }
    input.abort();

    // Close the channel, not just EOF: on a shared connection nothing else
    // ends it, and sshd keeps a PTY shell running after EOF.
    let _ = writer.eof().await;
    let _ = writer.close().await;
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::tests::{start_with, temp_host_keys, ServerOptions, TestServer};
    use std::sync::{atomic::Ordering, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc::UnboundedSender;

    fn options(server: &TestServer) -> SshOptions {
        SshOptions {
            id: None,
            host: "127.0.0.1".into(),
            port: server.addr.port(),
            username: "tester".into(),
            auth: SshAuth::Password {
                password: "secret".into(),
            },
            cols: 80,
            rows: 24,
            title: None,
            connection_id: None,
        }
    }

    async fn eventually(condition: impl FnMut() -> bool) -> bool {
        within(Duration::from_secs(3), condition).await
    }

    async fn within(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while tokio::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        condition()
    }

    /// A tab: its shell runs in the background; `output` collects what it shows.
    struct Tab {
        input: UnboundedSender<SessionInput>,
        output: Arc<Mutex<Vec<u8>>>,
        done: tokio::task::JoinHandle<Option<i32>>,
    }

    fn run_tab(channel: Channel<Msg>) -> Tab {
        let (input, rx) = unbounded_channel();
        let output = Arc::new(Mutex::new(Vec::new()));
        let shown = output.clone();
        let done = tokio::spawn(async move {
            let sink = move |data: &[u8]| shown.lock().unwrap().extend_from_slice(data);
            drive_shell(channel, rx, &sink).await
        });
        Tab {
            input,
            output,
            done,
        }
    }

    #[tokio::test]
    async fn closing_a_tab_closes_its_channel_and_leaves_the_shared_connection_up() {
        let server = start_with(ServerOptions::default()).await;
        let keys = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let opts = options(&server);
        let (first, shell) = open_or_login(
            &registry,
            None,
            "conn".into(),
            &opts,
            &temp_host_keys(&keys),
            &|_| {},
        )
        .await
        .unwrap();
        let tab1 = run_tab(shell);
        // Duplicate: same connection, its own shell.
        let shared = registry.attach_to("conn", &opts.target());
        let (second, shell) = open_or_login(
            &registry,
            shared,
            "unused".into(),
            &opts,
            &temp_host_keys(&keys),
            &|_| {},
        )
        .await
        .unwrap();
        let tab2 = run_tab(shell);
        assert_eq!(server.connections.load(Ordering::SeqCst), 1);
        assert!(eventually(|| tab2.output.lock().unwrap().starts_with(b"$ ")).await);

        // Close the duplicate: the server must see its channel close.
        tab2.input.send(SessionInput::Close).unwrap();
        tab2.done.await.unwrap();
        drop(second);
        assert!(eventually(|| server.channel_closes.load(Ordering::SeqCst) == 1).await);
        assert_eq!(server.closed.load(Ordering::SeqCst), 0);
        assert_eq!(registry.list()[0].users, 1);

        // The first tab still works.
        tab1.input
            .send(SessionInput::Data(b"still here".to_vec()))
            .unwrap();
        assert!(eventually(|| tab1.output.lock().unwrap().ends_with(b"still here")).await);
        tab1.input.send(SessionInput::Close).unwrap();
        tab1.done.await.unwrap();
        drop(first);
        assert!(eventually(|| server.closed.load(Ordering::SeqCst) == 1).await);
    }

    #[tokio::test]
    async fn a_big_paste_does_not_stall_other_tabs_on_the_connection() {
        // A small server window: the paste has to wait for window space.
        let server = start_with(ServerOptions {
            window_size: Some(64 * 1024),
            ..ServerOptions::default()
        })
        .await;
        let keys = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let opts = options(&server);
        let (lease, shell) = open_or_login(
            &registry,
            None,
            "conn".into(),
            &opts,
            &temp_host_keys(&keys),
            &|_| {},
        )
        .await
        .unwrap();
        let tab = run_tab(shell);
        let paste = vec![b'a'; 4 * 1024 * 1024];
        tab.input.send(SessionInput::Data(paste)).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Another tab on the same connection gets a command through meanwhile.
        let other = registry.attach("conn").unwrap();
        let ran = tokio::time::timeout(Duration::from_secs(5), async {
            let mut channel = other.open_session().await.unwrap();
            channel.exec(true, "uptime").await.unwrap();
            while channel.wait().await.is_some() {}
        })
        .await;
        assert!(ran.is_ok(), "the connection stalled behind the paste");
        // And the whole paste was echoed back in the end.
        // (Echoing 4 MiB back through a 64 KiB window takes a while on slow CI.)
        let echoed = || tab.output.lock().unwrap().len() >= 2 + 4 * 1024 * 1024;
        assert!(within(Duration::from_secs(30), echoed).await);
        tab.input.send(SessionInput::Close).unwrap();
        tab.done.await.unwrap();
        drop((lease, other));
    }

    #[tokio::test]
    async fn a_duplicate_logs_in_again_when_the_connection_takes_no_more_sessions() {
        // Network gear often allows one session per connection.
        let server = start_with(ServerOptions {
            max_sessions: Some(1),
            ..ServerOptions::default()
        })
        .await;
        let keys = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let opts = options(&server);
        let (first, shell) = open_or_login(
            &registry,
            None,
            "conn".into(),
            &opts,
            &temp_host_keys(&keys),
            &|_| {},
        )
        .await
        .unwrap();
        let _tab1 = run_tab(shell);
        let messages = Mutex::new(Vec::new());
        let progress = |message: String| messages.lock().unwrap().push(message);
        let shared = registry.attach_to("conn", &opts.target());
        let (second, _shell) = open_or_login(
            &registry,
            shared,
            "unused".into(),
            &opts,
            &temp_host_keys(&keys),
            &progress,
        )
        .await
        .unwrap();
        assert_ne!(second.id, first.id);
        assert_eq!(server.connections.load(Ordering::SeqCst), 2);
        assert!(messages.lock().unwrap()[0].contains("logging in again"));
    }
}

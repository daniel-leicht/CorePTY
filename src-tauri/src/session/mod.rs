// Session core: a uniform abstraction over local PTYs, SSH, and Telnet.
//
// Every backend is driven the same way: the UI sends `SessionInput` (keystrokes,
// resize, close) into a channel, and the backend streams bytes back to the UI as
// `pty://data` events plus lifecycle `pty://status` / `pty://exit` events.

#[cfg(windows)]
pub mod elevated;
pub mod local;
#[cfg(windows)]
pub mod proto;
pub mod ssh;
pub mod telnet;

use std::collections::HashMap;
use std::sync::Mutex;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::UnboundedSender;

/// Control messages sent from the UI to a session's driver.
#[derive(Debug)]
pub enum SessionInput {
    /// Raw bytes to write to the session (keystrokes / paste).
    Data(Vec<u8>),
    /// Terminal was resized (columns x rows).
    Resize { cols: u16, rows: u16 },
    /// Tear the session down.
    Close,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Local,
    Ssh,
    Telnet,
}

/// Metadata about an open session, surfaced to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub kind: SessionKind,
    pub title: String,
}

struct SessionHandle {
    input_tx: UnboundedSender<SessionInput>,
    info: SessionInfo,
}

/// Registry of live sessions, stored in Tauri's managed state.
#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<String, SessionHandle>>,
}

impl SessionManager {
    /// Lock the registry, tolerating a poisoned mutex — a panic in one session's
    /// driver thread must not brick every subsequent IPC call.
    fn guard(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionHandle>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether a session with this id is currently registered. Callers check
    /// this before spawning so a (buggy or hostile) caller can't reuse a live
    /// id and hijack its event routing / cleanup.
    pub fn exists(&self, id: &str) -> bool {
        self.guard().contains_key(id)
    }

    pub fn register(&self, info: SessionInfo, input_tx: UnboundedSender<SessionInput>) {
        self.guard()
            .insert(info.id.clone(), SessionHandle { input_tx, info });
    }

    pub fn send(&self, id: &str, input: SessionInput) -> Result<(), String> {
        let map = self.guard();
        let handle = map
            .get(id)
            .ok_or_else(|| format!("no such session: {id}"))?;
        handle
            .input_tx
            .send(input)
            .map_err(|_| "session is no longer running".to_string())
    }

    pub fn remove(&self, id: &str) {
        self.guard().remove(id);
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self.guard().values().map(|h| h.info.clone()).collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    #[allow(dead_code)]
    pub fn set_title(&self, id: &str, title: String) {
        if let Some(h) = self.guard().get_mut(id) {
            h.info.title = title;
        }
    }
}

// ---------------------------------------------------------------------------
// Events emitted to the frontend
// ---------------------------------------------------------------------------

pub const EVT_DATA: &str = "pty://data";
pub const EVT_EXIT: &str = "pty://exit";
pub const EVT_STATUS: &str = "pty://status";

#[derive(Clone, Serialize)]
struct DataPayload {
    id: String,
    /// base64-encoded raw bytes (binary-safe across the IPC bridge).
    data: String,
}

#[derive(Clone, Serialize)]
struct ExitPayload {
    id: String,
    code: Option<i32>,
    message: Option<String>,
}

#[derive(Clone, Serialize)]
struct StatusPayload {
    id: String,
    status: String,
    detail: Option<String>,
}

pub fn emit_data(app: &AppHandle, id: &str, bytes: &[u8]) {
    let _ = app.emit(
        EVT_DATA,
        DataPayload {
            id: id.to_string(),
            data: STANDARD.encode(bytes),
        },
    );
}

pub fn emit_exit(app: &AppHandle, id: &str, code: Option<i32>, message: Option<String>) {
    let _ = app.emit(
        EVT_EXIT,
        ExitPayload {
            id: id.to_string(),
            code,
            message,
        },
    );
}

pub fn emit_status(app: &AppHandle, id: &str, status: &str, detail: Option<String>) {
    let _ = app.emit(
        EVT_STATUS,
        StatusPayload {
            id: id.to_string(),
            status: status.to_string(),
            detail,
        },
    );
}

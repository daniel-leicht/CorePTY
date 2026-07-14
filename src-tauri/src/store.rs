// Persistence: passwords / passphrases live in the OS keychain (via `keyring`),
// connection profiles + folders live in a TOML file, and UI settings live in a
// JSON file — both in the app config dir. Secrets are keyed by the saved-session
// id and are never written to disk here.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

const KEYRING_SERVICE: &str = "CorePTY";

// ---------------------------------------------------------------------------
// Secrets — OS keychain (Windows Credential Manager / macOS Keychain / libsecret)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn secret_set(id: String, secret: String) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &id).map_err(|e| e.to_string())?;
    entry.set_password(&secret).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn secret_get(id: String) -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &id).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn secret_delete(id: String) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &id).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Saved connection profiles + folders — TOML
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSession {
    pub id: String,
    pub name: String,
    pub kind: String, // "ssh" | "telnet"
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub auth_type: Option<String>, // "password" | "key"
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub save_secret: bool,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    /// Sort index within its folder (drag-to-reorder). Absent = sort by name.
    #[serde(default)]
    pub order: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    folders: Vec<Folder>,
    #[serde(default)]
    sessions: Vec<SavedSession>,
}

fn config_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_config_dir().map_err(|e| e.to_string())
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_dir(app)?.join("sessions.toml"))
}

fn read_store(app: &AppHandle) -> Result<Store, String> {
    let path = store_path(app)?;
    match fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("failed to parse sessions.toml: {e}")),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e.to_string()),
    }
}

fn write_store(app: &AppHandle, store: &Store) -> Result<(), String> {
    let path = store_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = toml::to_string_pretty(store).map_err(|e| e.to_string())?;
    fs::write(&path, body).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn sessions_load(app: AppHandle) -> Result<Vec<SavedSession>, String> {
    Ok(read_store(&app)?.sessions)
}

#[tauri::command]
pub fn sessions_upsert(app: AppHandle, session: SavedSession) -> Result<(), String> {
    let mut store = read_store(&app)?;
    if let Some(existing) = store.sessions.iter_mut().find(|s| s.id == session.id) {
        *existing = session;
    } else {
        store.sessions.push(session);
    }
    write_store(&app, &store)
}

/// Set the folder + order of a batch of sessions in one write. `ids` is the new
/// display order of the sessions now in `folder`; each is moved into `folder`
/// (None = root) and given its list index as `order`. Used by drag-to-reorder.
#[tauri::command]
pub fn sessions_reorder(
    app: AppHandle,
    folder: Option<String>,
    ids: Vec<String>,
) -> Result<(), String> {
    let mut store = read_store(&app)?;
    for (i, id) in ids.iter().enumerate() {
        if let Some(s) = store.sessions.iter_mut().find(|s| &s.id == id) {
            s.folder_id = folder.clone();
            s.order = Some(i as i64);
        }
    }
    write_store(&app, &store)
}

#[tauri::command]
pub fn sessions_delete(app: AppHandle, id: String) -> Result<(), String> {
    let mut store = read_store(&app)?;
    store.sessions.retain(|s| s.id != id);
    write_store(&app, &store)?;
    let _ = secret_delete(id); // best-effort secret cleanup
    Ok(())
}

#[tauri::command]
pub fn folders_load(app: AppHandle) -> Result<Vec<Folder>, String> {
    Ok(read_store(&app)?.folders)
}

#[tauri::command]
pub fn folder_upsert(app: AppHandle, folder: Folder) -> Result<(), String> {
    let mut store = read_store(&app)?;
    if let Some(existing) = store.folders.iter_mut().find(|f| f.id == folder.id) {
        *existing = folder;
    } else {
        store.folders.push(folder);
    }
    write_store(&app, &store)
}

/// Delete a folder, promoting its direct children (subfolders + sessions) to
/// the folder's parent so nothing is silently lost.
#[tauri::command]
pub fn folder_delete(app: AppHandle, id: String) -> Result<(), String> {
    let mut store = read_store(&app)?;
    let parent = store
        .folders
        .iter()
        .find(|f| f.id == id)
        .and_then(|f| f.parent_id.clone());
    for f in store.folders.iter_mut() {
        if f.parent_id.as_deref() == Some(id.as_str()) {
            f.parent_id = parent.clone();
        }
    }
    for s in store.sessions.iter_mut() {
        if s.folder_id.as_deref() == Some(id.as_str()) {
            s.folder_id = parent.clone();
        }
    }
    store.folders.retain(|f| f.id != id);
    write_store(&app, &store)
}

// ---------------------------------------------------------------------------
// UI settings — JSON (schema owned by the frontend)
// ---------------------------------------------------------------------------

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_dir(app)?.join("settings.json"))
}

#[tauri::command]
pub fn settings_load(app: AppHandle) -> Result<Value, String> {
    let path = settings_path(&app)?;
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("failed to parse settings.json: {e}")),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Default::default())),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn settings_save(app: AppHandle, settings: Value) -> Result<(), String> {
    let path = settings_path(&app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(&path, body).map_err(|e| e.to_string())
}

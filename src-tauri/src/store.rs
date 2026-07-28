// Persistence: passwords / passphrases live in the OS keychain (via `keyring`),
// connection profiles + folders live in a TOML file, and UI settings live in a
// JSON file — both in the app config dir. Secrets are keyed by the saved-session
// id and are never written to disk here.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const KEYRING_SERVICE: &str = "CorePTY";
const LEGACY_IDENTIFIER: &str = "com.corepty.app";
static STORE_LOCK: Mutex<()> = Mutex::new(());
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

fn lock(mutex: &'static Mutex<()>) -> MutexGuard<'static, ()> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

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

fn config_file(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    let directory = config_dir(app)?;
    let current = directory.join(name);
    if let Some(config_root) = directory.parent() {
        let legacy = config_root.join(LEGACY_IDENTIFIER).join(name);
        migrate_legacy_file(&legacy, &current)?;
    }
    Ok(current)
}

/// Preserve user data after correcting the legacy bundle identifier. The old
/// file is intentionally retained so downgrading the application remains safe.
fn migrate_legacy_file(legacy: &Path, current: &Path) -> Result<(), String> {
    if current.exists() {
        return Ok(());
    }
    match fs::read(legacy) {
        Ok(body) => atomic_write(current, &body)
            .map_err(|e| format!("failed to migrate {}: {e}", current.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to read legacy configuration {}: {error}",
            legacy.display()
        )),
    }
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    config_file(app, "sessions.toml")
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
    let body = toml::to_string_pretty(store).map_err(|e| e.to_string())?;
    atomic_write(&path, body.as_bytes())
}

#[tauri::command]
pub fn sessions_load(app: AppHandle) -> Result<Vec<SavedSession>, String> {
    let _guard = lock(&STORE_LOCK);
    Ok(read_store(&app)?.sessions)
}

#[tauri::command]
pub fn sessions_upsert(app: AppHandle, session: SavedSession) -> Result<(), String> {
    let _guard = lock(&STORE_LOCK);
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
    let _guard = lock(&STORE_LOCK);
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
    let _guard = lock(&STORE_LOCK);
    let mut store = read_store(&app)?;
    store.sessions.retain(|s| s.id != id);
    write_store(&app, &store)?;
    let _ = secret_delete(id); // best-effort secret cleanup
    Ok(())
}

#[tauri::command]
pub fn folders_load(app: AppHandle) -> Result<Vec<Folder>, String> {
    let _guard = lock(&STORE_LOCK);
    Ok(read_store(&app)?.folders)
}

#[tauri::command]
pub fn folder_upsert(app: AppHandle, folder: Folder) -> Result<(), String> {
    let _guard = lock(&STORE_LOCK);
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
    let _guard = lock(&STORE_LOCK);
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
    config_file(app, "settings.json")
}

#[tauri::command]
pub fn settings_load(app: AppHandle) -> Result<Value, String> {
    let _guard = lock(&SETTINGS_LOCK);
    let path = settings_path(&app)?;
    match fs::read_to_string(&path) {
        Ok(s) => {
            serde_json::from_str(&s).map_err(|e| format!("failed to parse settings.json: {e}"))
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Object(Default::default()))
        }
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn settings_save(app: AppHandle, settings: Value) -> Result<(), String> {
    let _guard = lock(&SETTINGS_LOCK);
    let path = settings_path(&app)?;
    let body = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    atomic_write(&path, body.as_bytes())
}

/// Write a complete replacement beside the destination, flush it, then swap it
/// into place. A crash can leave an unused temp file, but never a truncated
/// sessions/settings file.
fn atomic_write(path: &Path, body: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "configuration path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temp = parent.join(format!(".corepty-{}.tmp", Uuid::new_v4()));

    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        file.write_all(body).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        replace_file(&temp, path).map_err(|e| e.to_string())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| std::io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("corepty-store-test-{}", Uuid::new_v4()))
    }

    #[test]
    fn atomic_write_creates_and_replaces_complete_files() {
        let dir = test_dir();
        let path = dir.join("settings.json");
        atomic_write(&path, br#"{"version":1}"#).unwrap();
        assert_eq!(fs::read(&path).unwrap(), br#"{"version":1}"#);

        atomic_write(&path, br#"{"version":2,"complete":true}"#).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            br#"{"version":2,"complete":true}"#
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_configuration_is_copied_once_without_removing_the_source() {
        let dir = test_dir();
        let legacy = dir.join("legacy").join("sessions.toml");
        let current = dir.join("current").join("sessions.toml");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"original").unwrap();

        migrate_legacy_file(&legacy, &current).unwrap();
        assert_eq!(fs::read(&legacy).unwrap(), b"original");
        assert_eq!(fs::read(&current).unwrap(), b"original");

        fs::write(&current, b"updated").unwrap();
        migrate_legacy_file(&legacy, &current).unwrap();
        assert_eq!(fs::read(&current).unwrap(), b"updated");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn store_schema_round_trips_reorder_metadata() {
        let store = Store {
            folders: vec![Folder {
                id: "folder".into(),
                name: "Servers".into(),
                parent_id: None,
            }],
            sessions: vec![SavedSession {
                id: "session".into(),
                name: "Production".into(),
                kind: "ssh".into(),
                host: "example.com".into(),
                port: Some(22),
                username: Some("alice".into()),
                auth_type: Some("password".into()),
                key_path: None,
                save_secret: true,
                folder_id: Some("folder".into()),
                color: Some("blue".into()),
                order: Some(3),
            }],
        };
        let encoded = toml::to_string(&store).unwrap();
        let decoded: Store = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.sessions[0].order, Some(3));
        assert_eq!(decoded.sessions[0].color.as_deref(), Some("blue"));
    }
}

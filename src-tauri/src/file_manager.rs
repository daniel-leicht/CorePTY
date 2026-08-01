//! Provider-neutral local filesystem commands for the dual-pane file manager.
//!
//! The frontend never constructs paths itself. Native paths cross IPC as opaque
//! tokens plus a lossy display string; this preserves non-UTF-8 Unix names and
//! gives a future SFTP provider the same location/entry shape.

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::UNIX_EPOCH,
};
use tauri::{ipc::Channel, State};
use uuid::Uuid;

const TEXT_PREVIEW_LIMIT: usize = 512 * 1024;
const IMAGE_PREVIEW_LIMIT: u64 = 12 * 1024 * 1024;
const COPY_BUFFER_SIZE: usize = 128 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLocation {
    pub provider: &'static str,
    pub path: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRoot {
    pub label: String,
    pub location: FileLocation,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub token: String,
    pub kind: FileEntryKind,
    pub is_directory: bool,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub hidden: bool,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryListing {
    pub location: FileLocation,
    pub parent: Option<FileLocation>,
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileConflict {
    pub name: String,
    pub path: String,
    pub same_source: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePreview {
    pub name: String,
    pub path: String,
    pub kind: PreviewKind,
    pub mime: Option<String>,
    pub content: Option<String>,
    pub size: u64,
    pub truncated: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKind {
    Text,
    Image,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperationKind {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    Error,
    Replace,
    Skip,
    KeepBoth,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOperationRequest {
    pub id: String,
    pub kind: FileOperationKind,
    pub sources: Vec<String>,
    pub destination: String,
    pub conflict_policy: ConflictPolicy,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOperationResult {
    pub id: String,
    pub completed_items: u64,
    pub completed_bytes: u64,
    pub skipped_items: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FileOperationEvent {
    Started {
        id: String,
    },
    Planned {
        id: String,
        total_items: u64,
        total_bytes: u64,
    },
    Progress {
        id: String,
        current: String,
        completed_items: u64,
        completed_bytes: u64,
        total_items: u64,
        total_bytes: u64,
    },
    Finished {
        id: String,
        completed_items: u64,
        completed_bytes: u64,
        skipped_items: u64,
    },
    Failed {
        id: String,
        message: String,
    },
}

#[derive(Default)]
pub struct FileOperationManager {
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl FileOperationManager {
    fn register(&self, id: &str) -> Result<Arc<AtomicBool>, String> {
        let mut operations = self
            .cancellations
            .lock()
            .map_err(|_| "file operation state is unavailable".to_string())?;
        if operations.contains_key(id) {
            return Err(format!("file operation {id} already exists"));
        }
        let flag = Arc::new(AtomicBool::new(false));
        operations.insert(id.to_string(), flag.clone());
        Ok(flag)
    }

    fn finish(&self, id: &str) {
        if let Ok(mut operations) = self.cancellations.lock() {
            operations.remove(id);
        }
    }

    fn cancel(&self, id: &str) -> bool {
        let Ok(operations) = self.cancellations.lock() else {
            return false;
        };
        let Some(flag) = operations.get(id) else {
            return false;
        };
        flag.store(true, Ordering::Relaxed);
        true
    }
}

#[tauri::command]
pub fn files_home() -> Result<FileLocation, String> {
    let home = dirs::home_dir()
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "could not determine the home directory".to_string())?;
    Ok(location(&home))
}

#[tauri::command]
pub fn files_roots() -> Result<Vec<FileRoot>, String> {
    let home = dirs::home_dir();
    let mut roots = Vec::new();
    if let Some(path) = home.as_ref() {
        roots.push(FileRoot {
            label: "Home".to_string(),
            location: location(path),
        });
    }

    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetLogicalDrives;
        // The bitmask is immediate and avoids probing every possible drive,
        // which can stall on an empty optical drive or disconnected mapping.
        let drive_mask = unsafe { GetLogicalDrives() };
        for (index, drive) in (b'A'..=b'Z').enumerate() {
            if drive_mask & (1_u32 << index) == 0 {
                continue;
            }
            let path = PathBuf::from(format!("{}:\\", drive as char));
            if home.as_ref() != Some(&path) {
                roots.push(FileRoot {
                    label: format!("{}:", drive as char),
                    location: location(&path),
                });
            }
        }
    }

    #[cfg(not(windows))]
    {
        let root = PathBuf::from("/");
        if home.as_ref() != Some(&root) {
            roots.push(FileRoot {
                label: "Filesystem".to_string(),
                location: location(&root),
            });
        }
        #[cfg(target_os = "macos")]
        {
            let volumes = PathBuf::from("/Volumes");
            if volumes.is_dir() {
                roots.push(FileRoot {
                    label: "Volumes".to_string(),
                    location: location(&volumes),
                });
            }
        }
    }

    Ok(roots)
}

#[tauri::command]
pub async fn files_resolve(
    path: String,
    base_token: Option<String>,
) -> Result<FileLocation, String> {
    tauri::async_runtime::spawn_blocking(move || resolve_location(&path, base_token.as_deref()))
        .await
        .map_err(|e| format!("path resolution failed: {e}"))?
}

#[tauri::command]
pub async fn files_list(token: String) -> Result<DirectoryListing, String> {
    let path = decode_path(&token)?;
    tauri::async_runtime::spawn_blocking(move || list_directory(&path))
        .await
        .map_err(|e| format!("directory listing failed: {e}"))?
}

#[tauri::command]
pub async fn files_conflicts(
    sources: Vec<String>,
    destination: String,
) -> Result<Vec<FileConflict>, String> {
    tauri::async_runtime::spawn_blocking(move || find_conflicts(&sources, &destination))
        .await
        .map_err(|e| format!("conflict check failed: {e}"))?
}

#[tauri::command]
pub fn files_create_directory(parent: String, name: String) -> Result<FileLocation, String> {
    validate_name(&name)?;
    let path = decode_path(&parent)?.join(name);
    fs::create_dir(&path)
        .map_err(|e| format!("could not create {}: {e}", path.to_string_lossy()))?;
    Ok(location(&path))
}

#[tauri::command]
pub fn files_rename(token: String, name: String) -> Result<FileLocation, String> {
    validate_name(&name)?;
    let source = decode_path(&token)?;
    let parent = source
        .parent()
        .ok_or_else(|| "the filesystem root cannot be renamed".to_string())?;
    let destination = parent.join(name);
    if source == destination {
        return Ok(location(&source));
    }
    if path_exists(&destination) && !same_path(&source, &destination) {
        return Err(format!("{} already exists", destination.to_string_lossy()));
    }
    fs::rename(&source, &destination).map_err(|e| {
        format!(
            "could not rename {}: {e}",
            source
                .file_name()
                .unwrap_or(source.as_os_str())
                .to_string_lossy()
        )
    })?;
    Ok(location(&destination))
}

#[tauri::command]
pub async fn files_trash(tokens: Vec<String>) -> Result<(), String> {
    let paths = decode_paths(&tokens)?;
    tauri::async_runtime::spawn_blocking(move || {
        for path in paths {
            trash::delete(&path)
                .map_err(|e| format!("could not move {} to Trash: {e}", path.to_string_lossy()))?;
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("trash operation failed: {e}"))?
}

#[tauri::command]
pub async fn files_delete(tokens: Vec<String>) -> Result<(), String> {
    let paths = decode_paths(&tokens)?;
    tauri::async_runtime::spawn_blocking(move || {
        for path in paths {
            remove_path(&path)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("delete operation failed: {e}"))?
}

#[tauri::command]
pub fn files_open(token: String) -> Result<(), String> {
    let path = decode_path(&token)?;
    open::that_detached(&path)
        .map_err(|e| format!("could not open {}: {e}", path.to_string_lossy()))
}

#[tauri::command]
pub async fn files_preview(token: String) -> Result<FilePreview, String> {
    let path = decode_path(&token)?;
    tauri::async_runtime::spawn_blocking(move || preview_file(&path))
        .await
        .map_err(|e| format!("preview failed: {e}"))?
}

#[tauri::command]
pub async fn files_operate(
    manager: State<'_, FileOperationManager>,
    request: FileOperationRequest,
    on_event: Channel<FileOperationEvent>,
) -> Result<FileOperationResult, String> {
    if request.sources.is_empty() {
        return Err("select at least one file or folder".to_string());
    }
    let id = request.id.clone();
    let cancel = manager.register(&id)?;
    let event_for_failure = on_event.clone();
    let id_for_failure = id.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        run_operation(request, cancel, on_event)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(format!("file operation task failed: {error}")),
    };
    manager.finish(&id);
    if let Err(message) = &result {
        let _ = event_for_failure.send(FileOperationEvent::Failed {
            id: id_for_failure,
            message: message.clone(),
        });
    }
    result
}

#[tauri::command]
pub fn files_cancel(manager: State<'_, FileOperationManager>, id: String) -> bool {
    manager.cancel(&id)
}

fn resolve_location(path: &str, base_token: Option<&str>) -> Result<FileLocation, String> {
    let entered = expand_home(path)?;
    let resolved = if entered.is_absolute() {
        entered
    } else if let Some(base) = base_token {
        decode_path(base)?.join(entered)
    } else {
        std::env::current_dir()
            .map_err(|e| format!("could not determine the current directory: {e}"))?
            .join(entered)
    };
    if !resolved.is_dir() {
        return Err(format!("{} is not a directory", resolved.to_string_lossy()));
    }
    // Normalize typed relative paths (`..`, `.`) and avoid Windows' verbatim
    // `\\?\` display prefix. Entry navigation still keeps the visible symlink
    // path because it arrives as an already-tokenized location.
    let normalized = dunce::canonicalize(&resolved).unwrap_or(resolved);
    Ok(location(&normalized))
}

fn find_conflicts(
    source_tokens: &[String],
    destination_token: &str,
) -> Result<Vec<FileConflict>, String> {
    let destination = decode_path(destination_token)?;
    ensure_directory(&destination)?;
    let mut conflicts = Vec::new();
    for source in decode_paths(source_tokens)? {
        let name = source
            .file_name()
            .ok_or_else(|| format!("{} has no file name", source.to_string_lossy()))?;
        let target = destination.join(name);
        if path_exists(&target) {
            conflicts.push(FileConflict {
                name: name.to_string_lossy().into_owned(),
                path: target.to_string_lossy().into_owned(),
                same_source: same_path(&source, &target),
            });
        }
    }
    Ok(conflicts)
}

fn list_directory(path: &Path) -> Result<DirectoryListing, String> {
    ensure_directory(path)?;
    let read_dir = fs::read_dir(path)
        .map_err(|e| format!("could not read {}: {e}", path.to_string_lossy()))?;
    let mut entries = Vec::new();
    for item in read_dir {
        let item = item.map_err(|e| format!("could not read directory entry: {e}"))?;
        entries.push(entry_from_path(&item.path())?);
    }
    Ok(DirectoryListing {
        location: location(path),
        parent: path.parent().map(location),
        entries,
    })
}

fn entry_from_path(path: &Path) -> Result<FileEntry, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("could not inspect {}: {e}", path.to_string_lossy()))?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileEntryKind::Symlink
    } else if file_type.is_dir() {
        FileEntryKind::Directory
    } else if file_type.is_file() {
        FileEntryKind::File
    } else {
        FileEntryKind::Other
    };
    let is_directory = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
    let name_os = path.file_name().unwrap_or(path.as_os_str());
    Ok(FileEntry {
        name: name_os.to_string_lossy().into_owned(),
        path: path.to_string_lossy().into_owned(),
        token: encode_path(path),
        kind,
        is_directory,
        size: if file_type.is_file() {
            Some(metadata.len())
        } else {
            None
        },
        modified: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64),
        hidden: is_hidden(name_os, &metadata),
        readonly: metadata.permissions().readonly(),
    })
}

fn location(path: &Path) -> FileLocation {
    FileLocation {
        provider: "local",
        path: path.to_string_lossy().into_owned(),
        token: encode_path(path),
    }
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(format!("{} is not a directory", path.to_string_lossy()))
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name == "." || name == ".." {
        return Err("enter a valid name".to_string());
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("the name cannot contain a path separator".to_string());
    }
    Ok(())
}

fn expand_home(value: &str) -> Result<PathBuf, String> {
    if value == "~" {
        return dirs::home_dir()
            .ok_or_else(|| "could not determine the home directory".to_string());
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .ok_or_else(|| "could not determine the home directory".to_string());
    }
    Ok(PathBuf::from(value))
}

fn decode_paths(tokens: &[String]) -> Result<Vec<PathBuf>, String> {
    tokens.iter().map(|token| decode_path(token)).collect()
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!(
        "local:u:{}",
        URL_SAFE_NO_PAD.encode(path.as_os_str().as_bytes())
    )
}

#[cfg(unix)]
fn decode_path(token: &str) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    let encoded = token
        .strip_prefix("local:u:")
        .ok_or_else(|| "invalid local path token".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "invalid local path token".to_string())?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    format!("local:w:{}", URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(windows)]
fn decode_path(token: &str) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    let encoded = token
        .strip_prefix("local:w:")
        .ok_or_else(|| "invalid local path token".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "invalid local path token".to_string())?;
    if bytes.len() % 2 != 0 {
        return Err("invalid local path token".to_string());
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
fn encode_path(path: &Path) -> String {
    format!(
        "local:s:{}",
        URL_SAFE_NO_PAD.encode(path.to_string_lossy().as_bytes())
    )
}

#[cfg(not(any(unix, windows)))]
fn decode_path(token: &str) -> Result<PathBuf, String> {
    let encoded = token
        .strip_prefix("local:s:")
        .ok_or_else(|| "invalid local path token".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "invalid local path token".to_string())?;
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| "invalid local path token".to_string())
}

#[cfg(windows)]
fn is_hidden(name: &OsStr, metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    name.to_string_lossy().starts_with('.')
        || metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn is_hidden(name: &OsStr, _metadata: &Metadata) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn preview_file(path: &Path) -> Result<FilePreview, String> {
    let metadata = fs::metadata(path)
        .map_err(|e| format!("could not inspect {}: {e}", path.to_string_lossy()))?;
    if !metadata.is_file() {
        return Err("only files can be previewed".to_string());
    }
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned();
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if let Some(mime) = image_mime(&extension) {
        if metadata.len() > IMAGE_PREVIEW_LIMIT {
            return Ok(FilePreview {
                name,
                path: path.to_string_lossy().into_owned(),
                kind: PreviewKind::Unavailable,
                mime: Some(mime.to_string()),
                content: None,
                size: metadata.len(),
                truncated: false,
                message: Some("This image is too large for an inline preview".to_string()),
            });
        }
        let bytes = fs::read(path)
            .map_err(|e| format!("could not read {}: {e}", path.to_string_lossy()))?;
        return Ok(FilePreview {
            name,
            path: path.to_string_lossy().into_owned(),
            kind: PreviewKind::Image,
            mime: Some(mime.to_string()),
            content: Some(STANDARD.encode(bytes)),
            size: metadata.len(),
            truncated: false,
            message: None,
        });
    }

    let file =
        File::open(path).map_err(|e| format!("could not read {}: {e}", path.to_string_lossy()))?;
    let mut bytes = Vec::with_capacity(TEXT_PREVIEW_LIMIT.min(metadata.len() as usize));
    file.take((TEXT_PREVIEW_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("could not read {}: {e}", path.to_string_lossy()))?;
    let truncated = bytes.len() > TEXT_PREVIEW_LIMIT;
    bytes.truncate(TEXT_PREVIEW_LIMIT);
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Ok(FilePreview {
            name,
            path: path.to_string_lossy().into_owned(),
            kind: PreviewKind::Unavailable,
            mime: None,
            content: None,
            size: metadata.len(),
            truncated: false,
            message: Some("Binary preview is not supported".to_string()),
        });
    }
    Ok(FilePreview {
        name,
        path: path.to_string_lossy().into_owned(),
        kind: PreviewKind::Text,
        mime: Some("text/plain".to_string()),
        content: Some(String::from_utf8_lossy(&bytes).into_owned()),
        size: metadata.len(),
        truncated,
        message: None,
    })
}

fn image_mime(extension: &str) -> Option<&'static str> {
    match extension {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

#[derive(Default, Clone, Copy)]
struct WorkStats {
    items: u64,
    bytes: u64,
}

impl std::ops::AddAssign for WorkStats {
    fn add_assign(&mut self, rhs: Self) {
        self.items = self.items.saturating_add(rhs.items);
        self.bytes = self.bytes.saturating_add(rhs.bytes);
    }
}

struct OperationContext {
    id: String,
    on_event: Channel<FileOperationEvent>,
    cancel: Arc<AtomicBool>,
    total: WorkStats,
    completed: WorkStats,
    skipped: u64,
}

impl OperationContext {
    fn check_cancelled(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            Err("Operation cancelled".to_string())
        } else {
            Ok(())
        }
    }

    fn progress(&self, path: &Path) {
        let _ = self.on_event.send(FileOperationEvent::Progress {
            id: self.id.clone(),
            current: path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            completed_items: self.completed.items,
            completed_bytes: self.completed.bytes,
            total_items: self.total.items,
            total_bytes: self.total.bytes,
        });
    }
}

fn run_operation(
    request: FileOperationRequest,
    cancel: Arc<AtomicBool>,
    on_event: Channel<FileOperationEvent>,
) -> Result<FileOperationResult, String> {
    let _ = on_event.send(FileOperationEvent::Started {
        id: request.id.clone(),
    });
    let sources = decode_paths(&request.sources)?;
    let destination = decode_path(&request.destination)?;
    ensure_directory(&destination)?;

    let mut total = WorkStats::default();
    for source in &sources {
        total += measure_with_cancel(source, Some(&cancel))?;
    }
    let _ = on_event.send(FileOperationEvent::Planned {
        id: request.id.clone(),
        total_items: total.items,
        total_bytes: total.bytes,
    });

    let mut context = OperationContext {
        id: request.id.clone(),
        on_event,
        cancel,
        total,
        completed: WorkStats::default(),
        skipped: 0,
    };

    for source in sources {
        context.check_cancelled()?;
        let name = source
            .file_name()
            .ok_or_else(|| format!("{} has no file name", source.to_string_lossy()))?;
        let target = destination.join(name);
        guard_recursive_destination(&source, &target)?;
        match request.kind {
            FileOperationKind::Copy => {
                copy_path(&source, &target, request.conflict_policy, &mut context)?;
            }
            FileOperationKind::Move => {
                move_path(&source, &target, request.conflict_policy, &mut context)?;
            }
        }
    }

    let result = FileOperationResult {
        id: request.id,
        completed_items: context.completed.items,
        completed_bytes: context.completed.bytes,
        skipped_items: context.skipped,
    };
    let _ = context.on_event.send(FileOperationEvent::Finished {
        id: result.id.clone(),
        completed_items: result.completed_items,
        completed_bytes: result.completed_bytes,
        skipped_items: result.skipped_items,
    });
    Ok(result)
}

fn measure(path: &Path) -> Result<WorkStats, String> {
    measure_with_cancel(path, None)
}

fn measure_with_cancel(path: &Path, cancel: Option<&AtomicBool>) -> Result<WorkStats, String> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("Operation cancelled".to_string());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("could not inspect {}: {e}", path.to_string_lossy()))?;
    let mut stats = WorkStats {
        items: 1,
        bytes: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
    };
    if metadata.is_dir() {
        for entry in fs::read_dir(path)
            .map_err(|e| format!("could not read {}: {e}", path.to_string_lossy()))?
        {
            stats += measure_with_cancel(
                &entry
                    .map_err(|e| format!("could not read directory entry: {e}"))?
                    .path(),
                cancel,
            )?;
        }
    }
    Ok(stats)
}

fn copy_path(
    source: &Path,
    requested_target: &Path,
    policy: ConflictPolicy,
    context: &mut OperationContext,
) -> Result<Option<PathBuf>, String> {
    context.check_cancelled()?;
    if same_path(source, requested_target) {
        return match policy {
            ConflictPolicy::KeepBoth => {
                let unique = unique_target(requested_target);
                copy_path(source, &unique, policy, context)
            }
            ConflictPolicy::Skip => {
                context.skipped = context.skipped.saturating_add(1);
                Ok(None)
            }
            ConflictPolicy::Error | ConflictPolicy::Replace => {
                Err("source and destination are the same; choose Keep both".to_string())
            }
        };
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|e| format!("could not inspect {}: {e}", source.to_string_lossy()))?;
    let target = match resolve_target(requested_target, policy, context)? {
        Some(target) => target,
        None => return Ok(None),
    };

    if metadata.file_type().is_symlink() {
        if path_exists(&target) {
            remove_path(&target)?;
        }
        copy_symlink(source, &target)?;
        context.completed.items = context.completed.items.saturating_add(1);
        context.progress(source);
        return Ok(Some(target));
    }
    if metadata.is_dir() {
        if path_exists(&target)
            && !fs::symlink_metadata(&target)
                .map_err(|e| format!("could not inspect {}: {e}", target.to_string_lossy()))?
                .is_dir()
        {
            remove_path(&target)?;
        }
        if !path_exists(&target) {
            fs::create_dir(&target)
                .map_err(|e| format!("could not create {}: {e}", target.to_string_lossy()))?;
        }
        context.completed.items = context.completed.items.saturating_add(1);
        context.progress(source);
        for entry in fs::read_dir(source)
            .map_err(|e| format!("could not read {}: {e}", source.to_string_lossy()))?
        {
            let child = entry
                .map_err(|e| format!("could not read directory entry: {e}"))?
                .path();
            let child_name = child
                .file_name()
                .ok_or_else(|| format!("{} has no file name", child.to_string_lossy()))?;
            copy_path(&child, &target.join(child_name), policy, context)?;
        }
        let _ = fs::set_permissions(&target, metadata.permissions());
        return Ok(Some(target));
    }
    if metadata.is_file() {
        return copy_regular_file(source, &target, &metadata, policy, context);
    }
    Err(format!(
        "{} is not a regular file, folder, or symbolic link",
        source.to_string_lossy()
    ))
}

fn move_path(
    source: &Path,
    requested_target: &Path,
    policy: ConflictPolicy,
    context: &mut OperationContext,
) -> Result<(), String> {
    context.check_cancelled()?;
    if same_path(source, requested_target) {
        return Err("source and destination are the same".to_string());
    }
    let target = match resolve_target(requested_target, policy, context)? {
        Some(target) => target,
        None => return Ok(()),
    };

    // A plain rename is atomic and preserves all metadata. It fails across
    // volumes, where we intentionally fall back to copy-then-delete.
    if !path_exists(&target) && fs::rename(source, &target).is_ok() {
        let stats = measure(&target)?;
        context.completed += stats;
        context.progress(source);
        return Ok(());
    }

    let copied = copy_path(source, &target, policy, context)?;
    if copied.is_some() {
        context.check_cancelled()?;
        remove_path(source)?;
    }
    Ok(())
}

fn resolve_target(
    requested: &Path,
    policy: ConflictPolicy,
    context: &mut OperationContext,
) -> Result<Option<PathBuf>, String> {
    if !path_exists(requested) {
        return Ok(Some(requested.to_path_buf()));
    }
    match policy {
        ConflictPolicy::Error => Err(format!("{} already exists", requested.to_string_lossy())),
        ConflictPolicy::Skip => {
            context.skipped = context.skipped.saturating_add(1);
            Ok(None)
        }
        ConflictPolicy::KeepBoth => Ok(Some(unique_target(requested))),
        // Replacing two folders merges their contents. Type mismatches and
        // regular files are swapped only after their replacement is ready.
        ConflictPolicy::Replace => Ok(Some(requested.to_path_buf())),
    }
}

fn copy_regular_file(
    source: &Path,
    target: &Path,
    metadata: &Metadata,
    policy: ConflictPolicy,
    context: &mut OperationContext,
) -> Result<Option<PathBuf>, String> {
    let mut input = File::open(source)
        .map_err(|e| format!("could not open {}: {e}", source.to_string_lossy()))?;
    let temporary = temporary_target(target);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| format!("could not create {}: {e}", temporary.to_string_lossy()))?;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let copied = (|| {
        loop {
            context.check_cancelled()?;
            let read = input
                .read(&mut buffer)
                .map_err(|e| format!("could not read {}: {e}", source.to_string_lossy()))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|e| format!("could not write {}: {e}", temporary.to_string_lossy()))?;
            context.completed.bytes = context.completed.bytes.saturating_add(read as u64);
            context.progress(source);
        }
        output
            .flush()
            .map_err(|e| format!("could not finish {}: {e}", temporary.to_string_lossy()))?;
        let mut times = fs::FileTimes::new();
        if let Ok(modified) = metadata.modified() {
            times = times.set_modified(modified);
        }
        if let Ok(accessed) = metadata.accessed() {
            times = times.set_accessed(accessed);
        }
        let _ = output.set_times(times);
        // Permission models differ across NTFS, APFS, ext filesystems, FAT,
        // network shares, and future providers. Preserve when possible without
        // turning an otherwise successful cross-filesystem copy into a failure.
        let _ = fs::set_permissions(&temporary, metadata.permissions());
        Ok::<(), String>(())
    })();
    drop(output);
    if let Err(error) = copied {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let Some(committed_target) = commit_temporary_file(&temporary, target, policy)? else {
        context.skipped = context.skipped.saturating_add(1);
        return Ok(None);
    };
    context.completed.items = context.completed.items.saturating_add(1);
    context.progress(source);
    Ok(Some(committed_target))
}

fn temporary_target(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new(""));
    let mut name = target
        .file_name()
        .unwrap_or_else(|| OsStr::new("file"))
        .to_os_string();
    name.push(format!(".corepty-{}.part", Uuid::new_v4()));
    parent.join(name)
}

fn commit_temporary_file(
    temporary: &Path,
    target: &Path,
    policy: ConflictPolicy,
) -> Result<Option<PathBuf>, String> {
    if path_exists(target) {
        match policy {
            ConflictPolicy::Error => {
                let _ = fs::remove_file(temporary);
                return Err(format!("{} already exists", target.to_string_lossy()));
            }
            ConflictPolicy::Skip => {
                let _ = fs::remove_file(temporary);
                return Ok(None);
            }
            ConflictPolicy::KeepBoth => {
                let alternate = unique_target(target);
                return fs::rename(temporary, &alternate)
                    .map(|()| Some(alternate.clone()))
                    .map_err(|e| {
                        let _ = fs::remove_file(temporary);
                        format!("could not finish {}: {e}", alternate.to_string_lossy())
                    });
            }
            ConflictPolicy::Replace => {}
        }
        #[cfg(unix)]
        {
            let target_is_directory = fs::symlink_metadata(target)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false);
            if !target_is_directory {
                return fs::rename(temporary, target)
                    .map(|()| Some(target.to_path_buf()))
                    .map_err(|e| {
                        let _ = fs::remove_file(temporary);
                        format!("could not replace {}: {e}", target.to_string_lossy())
                    });
            }
        }
        if let Err(error) = remove_path(target) {
            let _ = fs::remove_file(temporary);
            return Err(error);
        }
    }
    fs::rename(temporary, target)
        .map(|()| Some(target.to_path_buf()))
        .map_err(|e| {
            let _ = fs::remove_file(temporary);
            format!("could not finish {}: {e}", target.to_string_lossy())
        })
}

fn unique_target(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let file_name = path.file_name().unwrap_or(path.as_os_str());
    for number in 1_u32.. {
        let suffix = if number == 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {number})")
        };
        let name = if let Some(file_name) = file_name.to_str() {
            let parsed = Path::new(file_name);
            let stem = parsed
                .file_stem()
                .unwrap_or(parsed.as_os_str())
                .to_string_lossy();
            let mut name = OsString::from(format!("{stem}{suffix}"));
            if let Some(extension) = parsed.extension() {
                name.push(".");
                name.push(extension);
            }
            name
        } else {
            // Preserve arbitrary native names. Appending after the extension is
            // preferable to corrupting bytes that cannot be represented in JS.
            let mut name = file_name.to_os_string();
            name.push(&suffix);
            name
        };
        let candidate = parent.join(name);
        if !path_exists(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn guard_recursive_destination(source: &Path, target: &Path) -> Result<(), String> {
    let Ok(source_abs) = fs::canonicalize(source) else {
        return Ok(());
    };
    let target_parent = target.parent().unwrap_or(target);
    let Ok(parent_abs) = fs::canonicalize(target_parent) else {
        return Ok(());
    };
    if parent_abs.starts_with(&source_abs) {
        return Err("a folder cannot be copied or moved into itself".to_string());
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn remove_path(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("could not inspect {}: {e}", path.to_string_lossy()))?;
    #[cfg(windows)]
    make_writable_for_deletion(path, &metadata)?;
    let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|e| format!("could not delete {}: {e}", path.to_string_lossy()))
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn make_writable_for_deletion(path: &Path, metadata: &Metadata) -> Result<(), String> {
    if !metadata.permissions().readonly() {
        return Ok(());
    }
    // On Windows this only clears FILE_ATTRIBUTE_READONLY. Clippy's warning is
    // about Unix mode bits, and this helper is not compiled on Unix.
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).map_err(|e| {
        format!(
            "could not make {} writable for deletion: {e}",
            path.to_string_lossy()
        )
    })
}

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::unix::fs::symlink;
    let link = fs::read_link(source)
        .map_err(|e| format!("could not read link {}: {e}", source.to_string_lossy()))?;
    symlink(link, target)
        .map_err(|e| format!("could not create link {}: {e}", target.to_string_lossy()))
}

#[cfg(windows)]
fn copy_symlink(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    let link = fs::read_link(source)
        .map_err(|e| format!("could not read link {}: {e}", source.to_string_lossy()))?;
    let result = if source.is_dir() {
        symlink_dir(link, target)
    } else {
        symlink_file(link, target)
    };
    result.map_err(|e| format!("could not create link {}: {e}", target.to_string_lossy()))
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(_source: &Path, _target: &Path) -> Result<(), String> {
    Err("copying symbolic links is not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_context(total: WorkStats) -> OperationContext {
        OperationContext {
            id: "test".to_string(),
            on_event: Channel::new(|_| Ok(())),
            cancel: Arc::new(AtomicBool::new(false)),
            total,
            completed: WorkStats::default(),
            skipped: 0,
        }
    }

    #[test]
    fn native_path_tokens_round_trip() {
        let path = PathBuf::from("folder").join("hello 🌍.txt");
        assert_eq!(decode_path(&encode_path(&path)).unwrap(), path);
    }

    #[test]
    fn operation_events_use_the_frontend_camel_case_contract() {
        let value = serde_json::to_value(FileOperationEvent::Progress {
            id: "op".to_string(),
            current: "file.txt".to_string(),
            completed_items: 1,
            completed_bytes: 2,
            total_items: 3,
            total_bytes: 4,
        })
        .unwrap();
        assert_eq!(value["event"], "progress");
        assert_eq!(value["completedItems"], 1);
        assert_eq!(value["totalBytes"], 4);
        assert!(value.get("completed_items").is_none());
    }

    #[test]
    fn relative_path_resolution_normalizes_parent_components() {
        let temp = tempdir().unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        let resolved = resolve_location("..", Some(&encode_path(&child))).unwrap();
        assert_eq!(
            decode_path(&resolved.token).unwrap(),
            dunce::canonicalize(temp.path()).unwrap()
        );
    }

    #[test]
    fn listing_reports_files_directories_and_hidden_entries() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("folder")).unwrap();
        fs::write(temp.path().join("hello.txt"), b"hello").unwrap();
        fs::write(temp.path().join(".hidden"), b"secret").unwrap();

        let listing = list_directory(temp.path()).unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert!(listing
            .entries
            .iter()
            .any(|entry| entry.name == "folder" && entry.is_directory));
        assert!(listing.entries.iter().any(|entry| {
            entry.name == "hello.txt" && entry.kind == FileEntryKind::File && entry.size == Some(5)
        }));
        assert!(listing
            .entries
            .iter()
            .any(|entry| entry.name == ".hidden" && entry.hidden));
    }

    #[test]
    fn preview_handles_text_and_rejects_binary() {
        let temp = tempdir().unwrap();
        let text = temp.path().join("notes.txt");
        let binary = temp.path().join("data.bin");
        fs::write(&text, b"hello\nworld").unwrap();
        fs::write(&binary, b"hello\0world").unwrap();

        assert!(matches!(
            preview_file(&text).unwrap().kind,
            PreviewKind::Text
        ));
        assert!(matches!(
            preview_file(&binary).unwrap().kind,
            PreviewKind::Unavailable
        ));
    }

    #[test]
    fn unique_target_keeps_extension() {
        let temp = tempdir().unwrap();
        let original = temp.path().join("report.txt");
        fs::write(&original, b"one").unwrap();
        assert_eq!(
            unique_target(&original).file_name().unwrap(),
            OsStr::new("report (copy).txt")
        );
        fs::write(temp.path().join("report (copy).txt"), b"two").unwrap();
        assert_eq!(
            unique_target(&original).file_name().unwrap(),
            OsStr::new("report (copy 2).txt")
        );
    }

    #[test]
    fn copying_over_the_same_file_is_rejected_without_data_loss() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("important.txt");
        fs::write(&source, b"keep me").unwrap();
        let mut context = test_context(measure(&source).unwrap());

        let error = copy_path(&source, &source, ConflictPolicy::Replace, &mut context).unwrap_err();
        assert!(error.contains("same"));
        assert_eq!(fs::read(&source).unwrap(), b"keep me");
    }

    #[test]
    fn conflict_checks_identify_the_same_source_directory() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("same.txt");
        fs::write(&source, b"same").unwrap();
        let conflicts = find_conflicts(&[encode_path(&source)], &encode_path(temp.path())).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].same_source);
    }

    #[test]
    fn keep_both_copies_a_file_beside_itself() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("report.txt");
        fs::write(&source, b"report body").unwrap();
        let mut context = test_context(measure(&source).unwrap());

        let copied = copy_path(&source, &source, ConflictPolicy::KeepBoth, &mut context)
            .unwrap()
            .unwrap();
        assert_eq!(copied.file_name().unwrap(), OsStr::new("report (copy).txt"));
        assert_eq!(fs::read(copied).unwrap(), b"report body");
    }

    #[test]
    fn copying_a_directory_preserves_nested_files() {
        let source_root = tempdir().unwrap();
        let destination_root = tempdir().unwrap();
        let source = source_root.path().join("project");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("src").join("main.rs"), b"fn main() {}").unwrap();
        let target = destination_root.path().join("project");
        let mut context = test_context(measure(&source).unwrap());

        copy_path(&source, &target, ConflictPolicy::Replace, &mut context).unwrap();
        assert_eq!(
            fs::read(target.join("src").join("main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert_eq!(context.completed.items, 3);
    }

    #[test]
    fn replacing_a_file_commits_the_complete_new_contents() {
        let source_root = tempdir().unwrap();
        let destination_root = tempdir().unwrap();
        let source = source_root.path().join("release.bin");
        let target = destination_root.path().join("release.bin");
        fs::write(&source, vec![7_u8; COPY_BUFFER_SIZE * 2 + 17]).unwrap();
        fs::write(&target, b"old contents").unwrap();
        let mut context = test_context(measure(&source).unwrap());

        copy_path(&source, &target, ConflictPolicy::Replace, &mut context).unwrap();
        assert_eq!(
            fs::read(&target).unwrap(),
            vec![7_u8; COPY_BUFFER_SIZE * 2 + 17]
        );
        assert_eq!(
            fs::read_dir(destination_root.path()).unwrap().count(),
            1,
            "temporary copy artifacts should be cleaned up"
        );
    }
}

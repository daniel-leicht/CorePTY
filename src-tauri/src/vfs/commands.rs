//! IPC commands over the filesystem registry. Each takes a [`Location`] (or a
//! filesystem id) and dispatches to whichever provider owns it.

use super::{Capabilities, EntryView, FsId, FsRegistry, Location, LocationView, VName, VPath, Vfs};
use serde::Serialize;
use std::{io, sync::Arc};
use tauri::State;

#[derive(Debug, Clone, Serialize)]
pub struct RootView {
    pub label: String,
    pub location: LocationView,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingView {
    pub location: LocationView,
    pub parent: Option<LocationView>,
    pub entries: Vec<EntryView>,
}

fn provider(registry: &FsRegistry, fs: &FsId) -> Result<Arc<dyn Vfs>, String> {
    registry.get(fs)
}

fn located(fs: &FsId, path: VPath) -> LocationView {
    Location {
        fs: fs.clone(),
        path,
    }
    .view()
}

fn failed(action: &str, target: &VPath, error: io::Error) -> String {
    format!("could not {action} {}: {error}", target.display())
}

#[tauri::command]
pub fn fs_capabilities(registry: State<'_, FsRegistry>, fs: FsId) -> Result<Capabilities, String> {
    Ok(provider(&registry, &fs)?.capabilities())
}

#[tauri::command]
pub async fn fs_home(registry: State<'_, FsRegistry>, fs: FsId) -> Result<LocationView, String> {
    let home = provider(&registry, &fs)?
        .home()
        .await
        .map_err(|e| format!("could not find the home folder: {e}"))?;
    Ok(located(&fs, home))
}

#[tauri::command]
pub async fn fs_roots(registry: State<'_, FsRegistry>, fs: FsId) -> Result<Vec<RootView>, String> {
    let roots = provider(&registry, &fs)?
        .roots()
        .await
        .map_err(|e| format!("could not list locations: {e}"))?;
    Ok(roots
        .into_iter()
        .map(|root| RootView {
            label: root.label,
            location: located(&fs, root.path),
        })
        .collect())
}

#[tauri::command]
pub async fn fs_resolve(
    registry: State<'_, FsRegistry>,
    fs: FsId,
    input: String,
    base: Option<Location>,
) -> Result<LocationView, String> {
    let base = base.filter(|base| base.fs == fs).map(|base| base.path);
    let resolved = provider(&registry, &fs)?
        .resolve(&input, base.as_ref())
        .await
        .map_err(|e| format!("could not open {input}: {e}"))?;
    Ok(located(&fs, resolved))
}

#[tauri::command]
pub async fn fs_list(
    registry: State<'_, FsRegistry>,
    location: Location,
) -> Result<ListingView, String> {
    let entries = provider(&registry, &location.fs)?
        .list(&location.path)
        .await
        .map_err(|e| failed("read", &location.path, e))?;
    Ok(ListingView {
        parent: location
            .path
            .parent()
            .map(|parent| located(&location.fs, parent)),
        entries: entries.iter().map(EntryView::from).collect(),
        location: location.view(),
    })
}

#[tauri::command]
pub async fn fs_stat(
    registry: State<'_, FsRegistry>,
    location: Location,
) -> Result<EntryView, String> {
    let meta = provider(&registry, &location.fs)?
        .stat(&location.path)
        .await
        .map_err(|e| failed("inspect", &location.path, e))?;
    let name = location
        .path
        .file_name()
        .unwrap_or_else(|| VName::for_path(&location.path, &location.path.display()));
    Ok(EntryView::from(&super::Entry {
        name,
        path: location.path,
        meta,
        error: None,
    }))
}

#[tauri::command]
pub async fn fs_mkdir(
    registry: State<'_, FsRegistry>,
    parent: Location,
    name: String,
) -> Result<LocationView, String> {
    let path = parent
        .path
        .join(&VName::for_path(&parent.path, &name))
        .map_err(|e| e.to_string())?;
    provider(&registry, &parent.fs)?
        .mkdir(&path)
        .await
        .map_err(|e| failed("create", &path, e))?;
    Ok(located(&parent.fs, path))
}

/// Rename in place. Never replaces an existing entry.
#[tauri::command]
pub async fn fs_rename(
    registry: State<'_, FsRegistry>,
    location: Location,
    name: String,
) -> Result<LocationView, String> {
    let parent = location
        .path
        .parent()
        .ok_or_else(|| "the filesystem root cannot be renamed".to_string())?;
    let target = parent
        .join(&VName::for_path(&parent, &name))
        .map_err(|e| e.to_string())?;
    provider(&registry, &location.fs)?
        .rename(&location.path, &target, false)
        .await
        .map_err(|e| failed("rename", &location.path, e))?;
    Ok(located(&location.fs, target))
}

/// Permanently delete, folders included.
#[tauri::command]
pub async fn fs_delete(
    registry: State<'_, FsRegistry>,
    locations: Vec<Location>,
) -> Result<(), String> {
    for location in locations {
        provider(&registry, &location.fs)?
            .remove(&location.path, true)
            .await
            .map_err(|e| failed("delete", &location.path, e))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn fs_trash(
    registry: State<'_, FsRegistry>,
    locations: Vec<Location>,
) -> Result<(), String> {
    let Some(first) = locations.first() else {
        return Ok(());
    };
    let fs = first.fs.clone();
    if locations.iter().any(|location| location.fs != fs) {
        return Err("items from different filesystems can't be trashed together".to_string());
    }
    let paths: Vec<VPath> = locations
        .into_iter()
        .map(|location| location.path)
        .collect();
    provider(&registry, &fs)?
        .trash(&paths)
        .await
        .map_err(|e| format!("could not move to Trash: {e}"))
}

/// Open a local file with the OS default application.
#[tauri::command]
pub async fn fs_open_native(location: Location) -> Result<(), String> {
    if !location.fs.is_local() {
        return Err("only local files can be opened directly".to_string());
    }
    let path = location
        .path
        .to_native()
        .map_err(|e| e.to_string())?
        .to_path_buf();
    tokio::task::spawn_blocking(move || {
        open::that_detached(&path)
            .map_err(|e| format!("could not open {}: {e}", path.to_string_lossy()))
    })
    .await
    .map_err(|e| format!("open failed: {e}"))?
}

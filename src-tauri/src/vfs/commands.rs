//! IPC commands over the filesystem registry. Each takes a [`Location`] (or a
//! filesystem id) and dispatches to whichever provider owns it.

use super::{
    Capabilities, EntryKind, EntryView, FsId, FsRegistry, Location, LocationView, VName, VPath, Vfs,
};
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

/// Set POSIX permissions (the Properties dialog): the bits in `set` on, those
/// in `clear` off, the rest as each entry has them. Returns how many entries
/// changed.
#[tauri::command]
pub async fn fs_chmod(
    registry: State<'_, FsRegistry>,
    location: Location,
    set: u32,
    clear: u32,
    recursive: bool,
) -> Result<u64, String> {
    let fs = provider(&registry, &location.fs)?;
    chmod(
        fs.as_ref(),
        &location.path,
        ModeChange { set, clear },
        recursive,
    )
    .await
    .map_err(|e| failed("change the permissions of", &location.path, e))
}

/// Permission bits to turn on (`set`) and off (`clear`); the others stay as
/// they are, so one change can apply to entries with different modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeChange {
    pub set: u32,
    pub clear: u32,
}

impl ModeChange {
    /// Exactly `mode`, whatever was there.
    #[cfg(test)]
    pub fn to(mode: u32) -> Self {
        Self {
            set: mode & 0o7777,
            clear: !mode & 0o7777,
        }
    }

    /// The new mode for an entry that has `old`. `None` when the mode isn't
    /// known and the change doesn't decide every bit.
    fn apply(&self, old: Option<u32>) -> Option<u32> {
        let decides_all = (self.set | self.clear) & 0o7777 == 0o7777;
        let base = match old {
            Some(mode) => mode & 0o7777,
            None if decides_all => 0,
            None => return None,
        };
        Some(((base & !self.clear) | self.set) & 0o7777)
    }
}

/// Apply `change` to `path`; with `recursive`, to everything inside it too.
/// Recursively, folders also get `x` wherever `r` is set (so they stay
/// openable), each after its contents (a mode that locks its owner out
/// would otherwise stop the walk), and links inside are left alone rather
/// than followed out of the tree. An item that can't be changed doesn't stop
/// the rest; the error then says how many failed.
pub async fn chmod(
    fs: &dyn Vfs,
    path: &VPath,
    change: ModeChange,
    recursive: bool,
) -> io::Result<u64> {
    let mut top = fs.lstat(path).await?;
    if top.kind == EntryKind::Symlink {
        // A link given directly: its target's mode is what changes.
        top = fs.stat(path).await?;
    }
    let unknown = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "its permissions aren't known, so only a complete mode can be set",
        )
    };
    if !(recursive && top.is_real_directory()) {
        let mode = change.apply(top.mode).ok_or_else(unknown)?;
        fs.set_mode(path, mode).await?;
        return Ok(1);
    }
    // (path, metadata, depth, its contents are done)
    let mut pending = vec![(path.clone(), top, 0_usize, false)];
    let mut changed = 0;
    let mut failed = 0;
    let mut first_error: Option<(VPath, io::Error)> = None;
    let mut fail = |path: VPath, error: io::Error| {
        failed += 1;
        first_error.get_or_insert((path, error));
    };
    while let Some((path, meta, depth, contents_done)) = pending.pop() {
        if depth > 0 && meta.kind == EntryKind::Symlink {
            continue;
        }
        let folder = meta.is_real_directory();
        if folder && !contents_done {
            match fs.list(&path).await {
                Ok(children) => {
                    pending.push((path, meta, depth, true));
                    for child in children {
                        pending.push((child.path, child.meta, depth + 1, false));
                    }
                }
                Err(error) => {
                    // Its contents stay as they are; the folder itself is
                    // still changed (that may be what lets it be listed).
                    fail(path.clone(), error);
                    pending.push((path, meta, depth, true));
                }
            }
            continue;
        }
        let Some(mut mode) = change.apply(meta.mode) else {
            fail(path, unknown());
            continue;
        };
        if folder {
            mode |= (mode & 0o444) >> 2;
        }
        match fs.set_mode(&path, mode).await {
            Ok(()) => changed += 1,
            Err(error) => fail(path, error),
        }
    }
    match first_error {
        None => Ok(changed),
        Some((path, error)) => Err(io::Error::new(
            error.kind(),
            format!(
                "{failed} item{} could not be changed (first, {}: {error})",
                if failed == 1 { "" } else { "s" },
                path.display()
            ),
        )),
    }
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

/// Up to `length` bytes from `offset` (at most 4 MiB), as a raw binary
/// response (no base64).
/// The viewer reads text heads and images in chunks with this.
#[tauri::command]
pub async fn fs_read(
    registry: State<'_, FsRegistry>,
    location: Location,
    offset: u64,
    length: u32,
) -> Result<tauri::ipc::Response, String> {
    let fs = provider(&registry, &location.fs)?;
    let bytes = fs
        .read_range(&location.path, offset, length as usize)
        .await
        .map_err(|e| failed("read", &location.path, e))?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FolderSize {
    /// Entries inside the folder (not counting the folder itself).
    pub items: u64,
    pub bytes: u64,
}

/// Total size of a folder's contents, for Space in the file manager. Links are
/// not followed, and subfolders that can't be read are left out rather than
/// failing the whole count.
#[tauri::command]
pub async fn fs_dir_size(
    registry: State<'_, FsRegistry>,
    location: Location,
) -> Result<FolderSize, String> {
    let fs = provider(&registry, &location.fs)?;
    folder_size(fs.as_ref(), &location.path)
        .await
        .map_err(|e| failed("measure", &location.path, e))
}

async fn folder_size(fs: &dyn Vfs, folder: &VPath) -> io::Result<FolderSize> {
    let mut size = FolderSize::default();
    let mut pending = vec![fs.list(folder).await?];
    while let Some(entries) = pending.pop() {
        for entry in entries {
            size.items += 1;
            size.bytes += entry.meta.size.unwrap_or(0);
            if entry.meta.kind == EntryKind::Directory {
                if let Ok(children) = fs.list(&entry.path).await {
                    pending.push(children);
                }
            }
        }
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::memory::MemFs;

    #[tokio::test]
    async fn reads_a_range_and_stops_at_the_end() {
        let fs = MemFs::new();
        fs.add_file("/data.txt", b"0123456789");
        let path = MemFs::path("/data.txt");
        assert_eq!(fs.read_range(&path, 2, 4).await.unwrap(), b"2345");
        assert_eq!(fs.read_range(&path, 8, 100).await.unwrap(), b"89");
        assert!(fs.read_range(&MemFs::path("/missing"), 0, 4).await.is_err());
    }

    #[tokio::test]
    async fn folder_size_counts_nested_entries() {
        let fs = MemFs::new();
        fs.add_dir("/site");
        fs.add_file("/site/index.html", &[0; 100]);
        fs.add_dir("/site/css");
        fs.add_file("/site/css/app.css", &[0; 50]);
        let size = folder_size(&fs, &MemFs::path("/site")).await.unwrap();
        assert_eq!(
            size,
            FolderSize {
                items: 3,
                bytes: 150
            }
        );
    }

    #[tokio::test]
    async fn chmod_applies_down_a_folder_and_keeps_folders_openable() {
        let fs = MemFs::new();
        fs.add_dir("/site");
        fs.add_file("/site/index.html", b"<html>");
        fs.add_dir("/site/css");
        fs.add_file("/site/css/app.css", b"*{}");
        // Just the folder.
        assert_eq!(
            chmod(&fs, &MemFs::path("/site"), ModeChange::to(0o750), false)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            fs.stat(&MemFs::path("/site/index.html"))
                .await
                .unwrap()
                .mode,
            Some(0o644)
        );
        // Everything in it: files get the mode as given.
        let changed = chmod(&fs, &MemFs::path("/site"), ModeChange::to(0o640), true)
            .await
            .unwrap();
        assert_eq!(changed, 4);
        for file in ["/site/index.html", "/site/css/app.css"] {
            assert_eq!(
                fs.stat(&MemFs::path(file)).await.unwrap().mode,
                Some(0o640),
                "{file}"
            );
        }
    }

    #[tokio::test]
    async fn a_change_keeps_each_entrys_other_bits() {
        let fs = MemFs::new();
        fs.add_dir("/site");
        fs.add_file("/site/deploy.sh", b"#!/bin/sh");
        fs.add_file("/site/notes.txt", b"x");
        let set = |path: &str, mode: u32| {
            let fs = &fs;
            let path = MemFs::path(path);
            async move { fs.set_mode(&path, mode).await.unwrap() }
        };
        set("/site", 0o755).await;
        set("/site/deploy.sh", 0o750).await;
        set("/site/notes.txt", 0o600).await;
        // Group write, nothing else: each keeps the rest of its own mode.
        // (MemFs keeps modes on files only.)
        let group_write = ModeChange {
            set: 0o020,
            clear: 0,
        };
        chmod(&fs, &MemFs::path("/site"), group_write, true)
            .await
            .unwrap();
        let mode = |path: &str| {
            let fs = &fs;
            let path = MemFs::path(path);
            async move { fs.stat(&path).await.unwrap().mode }
        };
        assert_eq!(mode("/site/deploy.sh").await, Some(0o770));
        assert_eq!(mode("/site/notes.txt").await, Some(0o620));
        let no_group_write = ModeChange {
            set: 0,
            clear: 0o020,
        };
        chmod(&fs, &MemFs::path("/site"), no_group_write, true)
            .await
            .unwrap();
        assert_eq!(mode("/site/deploy.sh").await, Some(0o750));
        assert_eq!(mode("/site/notes.txt").await, Some(0o600));
        // A partial change can't apply to a mode that isn't known.
        assert_eq!(group_write.apply(None), None);
        assert_eq!(ModeChange::to(0o640).apply(None), Some(0o640));
    }
}

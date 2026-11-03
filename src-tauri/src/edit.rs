//! Opening remote files in local apps (WinSCP style): a copy is downloaded
//! into its own temp folder and opened there. The folder goes when the file
//! manager tab that opened it closes.

use crate::vfs::{FsRegistry, Location, Meta, VPath, Vfs};
use serde::Serialize;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};
use tauri::State;
use tokio::io::AsyncWriteExt;

/// A downloaded copy of a remote file.
struct Copy {
    dir: PathBuf,
}

/// Tauri-managed state: the copies opened this session, by id.
#[derive(Default)]
pub struct OpenedCopies {
    copies: Mutex<HashMap<String, Copy>>,
}

impl OpenedCopies {
    fn copies(&self) -> MutexGuard<'_, HashMap<String, Copy>> {
        self.copies.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Download `path` from `fs` into a fresh temp folder under `root`.
    async fn download(
        &self,
        fs: &dyn Vfs,
        path: &VPath,
        root: &Path,
    ) -> io::Result<(String, PathBuf, Meta)> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "that has no file name"))?;
        let name = name.storable_here().unwrap_or(name);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = root.join(&id);
        tokio::fs::create_dir_all(&dir).await?;
        let local = dir.join(&*name.to_native());
        let downloaded = download(fs, path, &local).await;
        match downloaded {
            Ok(meta) => {
                self.copies().insert(id.clone(), Copy { dir });
                Ok((id, local, meta))
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                Err(e)
            }
        }
    }

    /// Delete copies (their apps may still have them open: best effort).
    fn close(&self, ids: &[String]) {
        let removed: Vec<Copy> = {
            let mut copies = self.copies();
            ids.iter().filter_map(|id| copies.remove(id)).collect()
        };
        for copy in removed {
            let _ = std::fs::remove_dir_all(&copy.dir);
        }
    }
}

/// Copy a remote file to `local`, keeping its modified time.
async fn download(fs: &dyn Vfs, path: &VPath, local: &Path) -> io::Result<Meta> {
    let meta = fs.stat(path).await?;
    if meta.is_directory {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "folders can't be opened this way",
        ));
    }
    let mut reader = fs.open_read(path, 0).await?;
    let mut file = tokio::fs::File::create(local).await?;
    tokio::io::copy(&mut reader, &mut file).await?;
    file.flush().await?;
    drop(file);
    if let Some(modified) = meta.modified {
        let local = local.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            std::fs::File::options()
                .write(true)
                .open(local)?
                .set_modified(modified)
        })
        .await;
    }
    Ok(meta)
}

fn copies_root() -> PathBuf {
    std::env::temp_dir().join("CorePTY").join("open")
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyView {
    pub id: String,
    /// Where the copy is on this machine.
    pub path: String,
}

/// Download a copy of a remote file and open it with the default app.
/// Close it with `fs_close_copies` (the file manager does on tab close).
#[tauri::command]
pub async fn fs_open_copy(
    registry: State<'_, FsRegistry>,
    copies: State<'_, OpenedCopies>,
    location: Location,
) -> Result<CopyView, String> {
    let fs = registry.get(&location.fs)?;
    let (id, local, _) = copies
        .download(fs.as_ref(), &location.path, &copies_root())
        .await
        .map_err(|e| format!("could not download {}: {e}", location.path.display()))?;
    let opened = local.clone();
    tokio::task::spawn_blocking(move || open::that_detached(&opened))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not open {}: {e}", local.to_string_lossy()))?;
    Ok(CopyView {
        id,
        path: local.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
pub fn fs_close_copies(copies: State<'_, OpenedCopies>, ids: Vec<String>) {
    copies.close(&ids);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::memory::MemFs;
    use std::time::{Duration, UNIX_EPOCH};

    #[tokio::test]
    async fn a_copy_lands_in_its_own_folder_with_the_remote_time_and_goes_on_close() {
        let remote = MemFs::new();
        remote.add_dir("/srv");
        remote.add_file("/srv/notes.txt", b"remote text");
        let when = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        remote
            .set_times(&MemFs::path("/srv/notes.txt"), when, None)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let copies = OpenedCopies::default();
        let (id, local, meta) = copies
            .download(&remote, &MemFs::path("/srv/notes.txt"), root.path())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&local).unwrap(), b"remote text");
        assert_eq!(local.file_name().unwrap(), "notes.txt");
        assert_eq!(std::fs::metadata(&local).unwrap().modified().unwrap(), when);
        assert_eq!(meta.size, Some(11));
        // Each copy gets its own folder: two copies of one name don't clash.
        let (second, other, _) = copies
            .download(&remote, &MemFs::path("/srv/notes.txt"), root.path())
            .await
            .unwrap();
        assert_ne!(local.parent(), other.parent());

        copies.close(&[id, second]);
        assert!(!local.exists() && !other.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_failed_download_leaves_nothing_behind() {
        let remote = MemFs::new();
        remote.add_dir("/srv");
        let root = tempfile::tempdir().unwrap();
        let copies = OpenedCopies::default();
        assert!(copies
            .download(&remote, &MemFs::path("/srv/missing.txt"), root.path())
            .await
            .is_err());
        assert!(copies
            .download(&remote, &MemFs::path("/srv"), root.path())
            .await
            .is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

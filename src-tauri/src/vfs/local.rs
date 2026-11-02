//! This machine's filesystem. Blocking `std::fs` calls run on the blocking
//! pool; file contents stream through `tokio::fs`.

use super::{
    Capabilities, Entry, EntryKind, Meta, Root, VName, VPath, VRead, VWrite, Vfs, WriteMode,
};
use async_trait::async_trait;
use std::{
    ffi::OsStr,
    fs::{self, Metadata},
    io::{self, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::io::AsyncSeekExt;

pub struct LocalFs;

/// Run blocking filesystem work off the async threads.
async fn blocking<T, F>(work: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

fn native(path: &VPath) -> io::Result<PathBuf> {
    path.to_native().map(Path::to_path_buf)
}

#[async_trait]
impl Vfs for LocalFs {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            trash: true,
            open_native: true,
            posix_permissions: cfg!(unix),
            symlinks: true,
        }
    }

    async fn home(&self) -> io::Result<VPath> {
        dirs::home_dir()
            .or_else(|| std::env::current_dir().ok())
            .map(VPath::Native)
            .ok_or_else(|| io::Error::other("could not determine the home directory"))
    }

    async fn roots(&self) -> io::Result<Vec<Root>> {
        blocking(|| Ok(roots())).await
    }

    async fn resolve(&self, input: &str, base: Option<&VPath>) -> io::Result<VPath> {
        let input = input.to_string();
        let base = base.map(native).transpose()?;
        blocking(move || resolve(&input, base.as_deref()).map(VPath::Native)).await
    }

    async fn canonicalize(&self, path: &VPath) -> io::Result<VPath> {
        let path = native(path)?;
        blocking(move || dunce::canonicalize(path).map(VPath::Native)).await
    }

    async fn list(&self, dir: &VPath) -> io::Result<Vec<Entry>> {
        let dir = native(dir)?;
        blocking(move || list(&dir)).await
    }

    async fn stat(&self, path: &VPath) -> io::Result<Meta> {
        let path = native(path)?;
        blocking(move || fs::metadata(&path).map(|metadata| meta(&path, &metadata))).await
    }

    async fn lstat(&self, path: &VPath) -> io::Result<Meta> {
        let path = native(path)?;
        blocking(move || fs::symlink_metadata(&path).map(|metadata| meta(&path, &metadata))).await
    }

    async fn mkdir(&self, path: &VPath) -> io::Result<()> {
        let path = native(path)?;
        blocking(move || fs::create_dir(path)).await
    }

    async fn rename(&self, from: &VPath, to: &VPath, overwrite: bool) -> io::Result<()> {
        let (from, to) = (native(from)?, native(to)?);
        blocking(move || rename(&from, &to, overwrite)).await
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> io::Result<()> {
        let path = native(path)?;
        blocking(move || remove(&path, recursive)).await
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> io::Result<VRead> {
        let mut file = tokio::fs::File::open(native(path)?).await?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset)).await?;
        }
        Ok(Box::pin(file))
    }

    async fn open_write(&self, path: &VPath, mode: WriteMode) -> io::Result<VWrite> {
        let mut options = tokio::fs::OpenOptions::new();
        match mode {
            WriteMode::CreateNew => options.write(true).create_new(true),
            WriteMode::Truncate => options.write(true).create(true).truncate(true),
            WriteMode::Append => options.append(true),
        };
        Ok(Box::pin(options.open(native(path)?).await?))
    }

    async fn set_times(
        &self,
        path: &VPath,
        modified: SystemTime,
        accessed: Option<SystemTime>,
    ) -> io::Result<()> {
        let path = native(path)?;
        blocking(move || {
            let mut times = fs::FileTimes::new().set_modified(modified);
            if let Some(accessed) = accessed {
                times = times.set_accessed(accessed);
            }
            fs::File::options().write(true).open(path)?.set_times(times)
        })
        .await
    }

    async fn set_mode(&self, path: &VPath, mode: u32) -> io::Result<()> {
        let path = native(path)?;
        blocking(move || set_mode(&path, mode)).await
    }

    async fn read_link(&self, path: &VPath) -> io::Result<VPath> {
        let path = native(path)?;
        blocking(move || fs::read_link(path).map(VPath::Native)).await
    }

    async fn symlink(&self, target: &VPath, link: &VPath, target_is_dir: bool) -> io::Result<()> {
        let (target, link) = (native(target)?, native(link)?);
        blocking(move || symlink(&target, &link, target_is_dir)).await
    }

    async fn trash(&self, paths: &[VPath]) -> io::Result<()> {
        let paths = paths.iter().map(native).collect::<io::Result<Vec<_>>>()?;
        blocking(move || trash::delete_all(paths).map_err(io::Error::other)).await
    }
}

fn roots() -> Vec<Root> {
    let home = dirs::home_dir();
    let mut roots = Vec::new();
    if let Some(path) = home.as_ref() {
        roots.push(Root {
            label: "Home".to_string(),
            path: VPath::Native(path.clone()),
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
            roots.push(Root {
                label: format!("{}:", drive as char),
                path: VPath::Native(PathBuf::from(format!("{}:\\", drive as char))),
            });
        }
    }

    #[cfg(not(windows))]
    {
        let root = PathBuf::from("/");
        if home.as_ref() != Some(&root) {
            roots.push(Root {
                label: "Filesystem".to_string(),
                path: VPath::Native(root),
            });
        }
        #[cfg(target_os = "macos")]
        {
            let volumes = PathBuf::from("/Volumes");
            if volumes.is_dir() {
                roots.push(Root {
                    label: "Volumes".to_string(),
                    path: VPath::Native(volumes),
                });
            }
        }
    }
    roots
}

fn resolve(input: &str, base: Option<&Path>) -> io::Result<PathBuf> {
    let entered = expand_home(input)?;
    let resolved = if entered.is_absolute() {
        entered
    } else if let Some(base) = base {
        base.join(entered)
    } else {
        std::env::current_dir()?.join(entered)
    };
    if !resolved.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a folder", resolved.to_string_lossy()),
        ));
    }
    // Normalize `..`/`.` and avoid Windows' verbatim `\\?\` display prefix.
    Ok(dunce::canonicalize(&resolved).unwrap_or(resolved))
}

fn expand_home(value: &str) -> io::Result<PathBuf> {
    let home = || {
        dirs::home_dir().ok_or_else(|| io::Error::other("could not determine the home directory"))
    };
    if value == "~" {
        return home();
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return Ok(home()?.join(rest));
    }
    Ok(PathBuf::from(value))
}

fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)? {
        // An error from the scan itself fails the listing: retrying it could
        // loop forever. Per-entry failures below don't.
        let item = item?;
        let path = item.path();
        // DirEntry::metadata doesn't follow links, and on Windows it comes from
        // the directory scan itself instead of opening every file again.
        match item.metadata() {
            Ok(metadata) => entries.push(Entry {
                name: VName::Native(item.file_name()),
                meta: meta(&path, &metadata),
                path: VPath::Native(path),
                error: None,
            }),
            // Gone between the scan and the stat (temp folders churn constantly).
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => entries.push(Entry {
                meta: Meta::unknown(item.file_name().to_string_lossy().starts_with('.')),
                name: VName::Native(item.file_name()),
                path: VPath::Native(path),
                error: Some(e.to_string()),
            }),
        }
    }
    Ok(entries)
}

fn meta(path: &Path, metadata: &Metadata) -> Meta {
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let name = path.file_name().unwrap_or(path.as_os_str());
    Meta {
        kind,
        is_directory: file_type.is_dir() || (file_type.is_symlink() && path.is_dir()),
        size: file_type.is_file().then_some(metadata.len()),
        modified: metadata.modified().ok(),
        accessed: metadata.accessed().ok(),
        mode: mode(metadata),
        readonly: metadata.permissions().readonly(),
        hidden: is_hidden(name, metadata),
        owner: None,
        group: None,
    }
}

#[cfg(unix)]
fn mode(metadata: &Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode(_metadata: &Metadata) -> Option<u32> {
    None
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

fn rename(from: &Path, to: &Path, overwrite: bool) -> io::Result<()> {
    let existing = match fs::symlink_metadata(to) {
        Ok(metadata) => Some(metadata),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let Some(existing) = existing else {
        return fs::rename(from, to);
    };
    // Renaming a name onto itself (or onto a differently-cased spelling of
    // itself on Windows/macOS) is a plain rename, not a replacement.
    if same_entry(from, to) {
        return fs::rename(from, to);
    }
    if !overwrite {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.to_string_lossy()),
        ));
    }
    if existing.is_dir() || is_directory_link(&existing) {
        // Nothing can be renamed over a folder; it has to go first.
        remove(to, true)?;
        return fs::rename(from, to);
    }
    rename_over(from, to, &existing)
}

/// Replace a file in one step, so the old one survives if anything fails.
fn rename_over(from: &Path, to: &Path, existing: &Metadata) -> io::Result<()> {
    // Windows refuses to rename over a read-only file: lift the flag for the
    // rename and restore it if the rename fails.
    #[cfg(windows)]
    clear_readonly(to, existing)?;
    #[cfg(not(windows))]
    let _ = existing;
    fs::rename(from, to).inspect_err(|_| {
        #[cfg(windows)]
        let _ = fs::set_permissions(to, existing.permissions());
    })
}

fn same_entry(left: &Path, right: &Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Windows directory symlinks and junctions refuse a file renamed over them.
#[cfg(windows)]
fn is_directory_link(metadata: &Metadata) -> bool {
    use std::os::windows::fs::FileTypeExt;
    metadata.file_type().is_symlink_dir()
}

#[cfg(not(windows))]
fn is_directory_link(_metadata: &Metadata) -> bool {
    false
}

fn remove(path: &Path, recursive: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    clear_readonly(path, &metadata)?;
    if metadata.is_dir() {
        if recursive {
            fs::remove_dir_all(path)
        } else {
            fs::remove_dir(path)
        }
    } else if is_directory_link(&metadata) {
        // A directory link is removed like an empty folder, never followed.
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly(path: &Path, metadata: &Metadata) -> io::Result<()> {
    if !metadata.permissions().readonly() {
        return Ok(());
    }
    // On Windows this only clears FILE_ATTRIBUTE_READONLY; Clippy's warning is
    // about Unix mode bits, and this helper is not compiled on Unix.
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path, _target_is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path, target_is_dir: bool) -> io::Result<()> {
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn symlink(_target: &Path, _link: &Path, _target_is_dir: bool) -> io::Result<()> {
    Err(super::unsupported("symbolic links"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn vpath(path: &Path) -> VPath {
        VPath::native(path)
    }

    #[tokio::test]
    async fn lists_files_folders_and_hidden_entries() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("folder")).unwrap();
        fs::write(temp.path().join("hello.txt"), b"hello").unwrap();
        fs::write(temp.path().join(".hidden"), b"secret").unwrap();

        let entries = LocalFs.list(&vpath(temp.path())).await.unwrap();
        assert_eq!(entries.len(), 3);
        let find = |name: &str| entries.iter().find(|e| e.name.display() == name).unwrap();
        assert!(find("folder").meta.is_directory);
        assert_eq!(find("hello.txt").meta.size, Some(5));
        assert_eq!(find("hello.txt").meta.kind, EntryKind::File);
        assert!(find(".hidden").meta.hidden);
    }

    #[tokio::test]
    async fn streams_round_trip_and_reads_resume_from_an_offset() {
        let temp = tempdir().unwrap();
        let file = vpath(&temp.path().join("data.bin"));
        let mut writer = LocalFs
            .open_write(&file, WriteMode::CreateNew)
            .await
            .unwrap();
        writer.write_all(b"0123456789").await.unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);
        assert_eq!(
            LocalFs
                .open_write(&file, WriteMode::CreateNew)
                .await
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::AlreadyExists
        );

        let mut reader = LocalFs.open_read(&file, 4).await.unwrap();
        let mut rest = String::new();
        reader.read_to_string(&mut rest).await.unwrap();
        assert_eq!(rest, "456789");
    }

    #[tokio::test]
    async fn truncate_rewrites_and_append_resumes() {
        let temp = tempdir().unwrap();
        let file = vpath(&temp.path().join("part.bin"));
        fs::write(temp.path().join("part.bin"), b"stale data").unwrap();

        let mut writer = LocalFs
            .open_write(&file, WriteMode::Truncate)
            .await
            .unwrap();
        writer.write_all(b"abc").await.unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);
        let mut writer = LocalFs.open_write(&file, WriteMode::Append).await.unwrap();
        writer.write_all(b"def").await.unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);
        assert_eq!(fs::read(temp.path().join("part.bin")).unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn rename_without_overwrite_refuses_an_existing_target() {
        let temp = tempdir().unwrap();
        let (a, b) = (temp.path().join("a.txt"), temp.path().join("b.txt"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let error = LocalFs
            .rename(&vpath(&a), &vpath(&b), false)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"b");
    }

    #[tokio::test]
    async fn rename_with_overwrite_replaces_files_including_read_only_ones() {
        let temp = tempdir().unwrap();
        let (a, b) = (temp.path().join("a.txt"), temp.path().join("b.txt"));
        fs::write(&a, b"new").unwrap();
        fs::write(&b, b"old").unwrap();
        let mut permissions = fs::metadata(&b).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&b, permissions).unwrap();

        LocalFs.rename(&vpath(&a), &vpath(&b), true).await.unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"new");
        assert!(!a.exists());
    }

    #[tokio::test]
    async fn rename_with_overwrite_replaces_a_folder() {
        let temp = tempdir().unwrap();
        let (file, folder) = (temp.path().join("item.txt"), temp.path().join("item"));
        fs::write(&file, b"file").unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("inner.txt"), b"old").unwrap();

        LocalFs
            .rename(&vpath(&file), &vpath(&folder), true)
            .await
            .unwrap();
        assert_eq!(fs::read(&folder).unwrap(), b"file");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_failed_overwrite_leaves_the_old_file_intact() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempdir().unwrap();
        let (a, b) = (temp.path().join("a.txt"), temp.path().join("b.txt"));
        fs::write(&a, b"new").unwrap();
        fs::write(&b, b"old").unwrap();
        // No sharing at all: the old file can be neither replaced nor deleted.
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&b)
            .unwrap();
        assert!(LocalFs.rename(&vpath(&a), &vpath(&b), true).await.is_err());
        drop(lock);
        assert_eq!(fs::read(&b).unwrap(), b"old");
        assert_eq!(fs::read(&a).unwrap(), b"new");
    }

    #[tokio::test]
    async fn remove_refuses_a_non_empty_folder_unless_recursive() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("inner.txt"), b"x").unwrap();
        assert!(LocalFs.remove(&vpath(&folder), false).await.is_err());
        assert!(folder.join("inner.txt").exists());
        LocalFs.remove(&vpath(&folder), true).await.unwrap();
        assert!(!folder.exists());
    }

    #[tokio::test]
    async fn times_and_read_only_mode_are_applied() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("stamp.txt");
        fs::write(&file, b"x").unwrap();
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        LocalFs.set_times(&vpath(&file), when, None).await.unwrap();
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), when);

        LocalFs.set_mode(&vpath(&file), 0o444).await.unwrap();
        assert!(fs::metadata(&file).unwrap().permissions().readonly());
        LocalFs.set_mode(&vpath(&file), 0o644).await.unwrap();
        assert!(!fs::metadata(&file).unwrap().permissions().readonly());
    }

    #[tokio::test]
    async fn resolve_normalizes_relative_input() {
        let temp = tempdir().unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        let resolved = LocalFs.resolve("..", Some(&vpath(&child))).await.unwrap();
        assert_eq!(resolved, vpath(&dunce::canonicalize(temp.path()).unwrap()));
        assert!(LocalFs
            .resolve("missing", Some(&vpath(temp.path())))
            .await
            .is_err());
    }
}

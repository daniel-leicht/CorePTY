//! A provider-neutral filesystem layer for the file manager.
//!
//! Every filesystem the file manager can browse (this machine today, SFTP
//! connections next) implements [`Vfs`] and is registered in the [`FsRegistry`]
//! under an [`FsId`]. The frontend addresses everything as a [`Location`]: the
//! filesystem plus an opaque, provider-native [`VPath`].

pub mod commands;
pub mod local;
#[cfg(test)]
pub mod memory;
mod path;

pub use path::{VName, VPath};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncWrite};

/// Which filesystem a path belongs to: [`FsId::LOCAL`] or a connection id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FsId(pub String);

impl FsId {
    pub const LOCAL: &'static str = "local";

    pub fn local() -> Self {
        Self(Self::LOCAL.to_string())
    }

    pub fn is_local(&self) -> bool {
        self.0 == Self::LOCAL
    }
}

/// A path on a particular filesystem — the unit the frontend works with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub fs: FsId,
    pub path: VPath,
}

impl Location {
    pub fn view(&self) -> LocationView {
        LocationView {
            location: self.clone(),
            display: self.path.display(),
        }
    }
}

/// A location plus its human-readable path, for the UI. It can be passed back
/// wherever a [`Location`] is expected: serde ignores the extra `display`.
#[derive(Debug, Clone, Serialize)]
pub struct LocationView {
    #[serde(flatten)]
    pub location: Location,
    pub display: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// What a provider knows about one entry. Optional fields are those some
/// filesystems don't have (POSIX modes on Windows, owners on most locals).
#[derive(Debug, Clone)]
pub struct Meta {
    pub kind: EntryKind,
    /// True for folders and for links that resolve to a folder.
    pub is_directory: bool,
    /// Byte length; files only.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    /// POSIX permission bits, where the filesystem has them.
    pub mode: Option<u32>,
    pub readonly: bool,
    pub hidden: bool,
    pub owner: Option<String>,
    pub group: Option<String>,
}

impl Meta {
    /// Placeholder for an entry that was listed but couldn't be inspected.
    pub fn unknown(hidden: bool) -> Self {
        Self {
            kind: EntryKind::Other,
            is_directory: false,
            size: None,
            modified: None,
            accessed: None,
            mode: None,
            readonly: false,
            hidden,
            owner: None,
            group: None,
        }
    }

    pub fn is_real_directory(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

/// One row of a folder listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: VName,
    pub path: VPath,
    pub meta: Meta,
    /// Set when the entry was listed but could not be inspected.
    pub error: Option<String>,
}

/// An [`Entry`] as the UI sees it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryView {
    pub name: String,
    pub path: VPath,
    pub display: String,
    pub kind: EntryKind,
    pub is_directory: bool,
    pub size: Option<u64>,
    /// Milliseconds since the Unix epoch.
    pub modified: Option<u64>,
    pub mode: Option<u32>,
    pub readonly: bool,
    pub hidden: bool,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub error: Option<String>,
}

impl From<&Entry> for EntryView {
    fn from(entry: &Entry) -> Self {
        Self {
            name: entry.name.display(),
            path: entry.path.clone(),
            display: entry.path.display(),
            kind: entry.meta.kind,
            is_directory: entry.meta.is_directory,
            size: entry.meta.size,
            modified: entry.meta.modified.and_then(epoch_millis),
            mode: entry.meta.mode,
            readonly: entry.meta.readonly,
            hidden: entry.meta.hidden,
            owner: entry.meta.owner.clone(),
            group: entry.meta.group.clone(),
            error: entry.error.clone(),
        }
    }
}

pub fn epoch_millis(time: SystemTime) -> Option<u64> {
    let millis = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    Some(millis.min(u128::from(u64::MAX)) as u64)
}

/// A starting point offered in the location menu (home, drives, `/`).
#[derive(Debug, Clone)]
pub struct Root {
    pub label: String,
    pub path: VPath,
}

/// Optional features; the UI hides what a filesystem can't do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Can move items to an OS Trash / Recycle Bin.
    pub trash: bool,
    /// Can open files with the OS default application.
    pub open_native: bool,
    /// Has POSIX permission bits (and `set_mode` applies them).
    pub posix_permissions: bool,
    pub symlinks: bool,
}

// Truncate/Append serve resumed and remote transfers (Phase 6); LocalFs
// supports and tests them already.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Fail if the file exists.
    CreateNew,
    /// Create, or empty an existing file.
    Truncate,
    /// Continue an existing file (resumed transfers).
    Append,
}

pub type VRead = Pin<Box<dyn AsyncRead + Send>>;
pub type VWrite = Pin<Box<dyn AsyncWrite + Send>>;

/// A browsable filesystem. Errors are plain `io::Error`s with the usual kinds
/// (`NotFound`, `AlreadyExists`, …) so callers can react to them.
#[async_trait]
pub trait Vfs: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    async fn home(&self) -> io::Result<VPath>;

    async fn roots(&self) -> io::Result<Vec<Root>>;

    /// Turn typed input (absolute, `~`-relative, or relative to `base`) into a
    /// normalized path of an existing folder.
    async fn resolve(&self, input: &str, base: Option<&VPath>) -> io::Result<VPath>;

    /// The absolute path with links and `..` resolved; the entry must exist.
    async fn canonicalize(&self, path: &VPath) -> io::Result<VPath>;

    async fn list(&self, dir: &VPath) -> io::Result<Vec<Entry>>;

    /// Metadata, following symbolic links.
    async fn stat(&self, path: &VPath) -> io::Result<Meta>;

    /// Metadata of the entry itself (a link is reported as a link).
    async fn lstat(&self, path: &VPath) -> io::Result<Meta>;

    async fn mkdir(&self, path: &VPath) -> io::Result<()>;

    /// Move `from` to `to` on this filesystem. With `overwrite`, whatever is at
    /// `to` is replaced — a file in one step, so the old one survives a failure;
    /// a folder is removed first. Without it, an existing `to` is an
    /// `AlreadyExists` error.
    async fn rename(&self, from: &VPath, to: &VPath, overwrite: bool) -> io::Result<()>;

    /// Delete a file, link or folder. A folder must be empty unless `recursive`.
    async fn remove(&self, path: &VPath, recursive: bool) -> io::Result<()>;

    async fn open_read(&self, path: &VPath, offset: u64) -> io::Result<VRead>;

    async fn open_write(&self, path: &VPath, mode: WriteMode) -> io::Result<VWrite>;

    async fn set_times(
        &self,
        path: &VPath,
        modified: SystemTime,
        accessed: Option<SystemTime>,
    ) -> io::Result<()>;

    /// Apply POSIX permission bits. Without POSIX permissions, a mode with no
    /// write bits marks the file read-only.
    async fn set_mode(&self, path: &VPath, mode: u32) -> io::Result<()>;

    async fn read_link(&self, path: &VPath) -> io::Result<VPath>;

    async fn symlink(&self, target: &VPath, link: &VPath, target_is_dir: bool) -> io::Result<()>;

    async fn trash(&self, _paths: &[VPath]) -> io::Result<()> {
        Err(unsupported("moving to the Trash"))
    }
}

pub fn unsupported(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{what} is not supported on this filesystem"),
    )
}

/// The filesystems the file manager can reach, by id. `local` is always there;
/// remote ones come and go with their connections.
pub struct FsRegistry {
    filesystems: RwLock<HashMap<FsId, Arc<dyn Vfs>>>,
}

impl Default for FsRegistry {
    fn default() -> Self {
        let registry = Self {
            filesystems: RwLock::new(HashMap::new()),
        };
        registry.register(FsId::local(), Arc::new(local::LocalFs));
        registry
    }
}

impl FsRegistry {
    pub fn register(&self, id: FsId, fs: Arc<dyn Vfs>) {
        self.filesystems
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, fs);
    }

    #[allow(dead_code)] // Used once remote connections register (Phase 5).
    pub fn unregister(&self, id: &FsId) {
        if !id.is_local() {
            self.filesystems
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
        }
    }

    pub fn get(&self, id: &FsId) -> Result<Arc<dyn Vfs>, String> {
        self.filesystems
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| format!("{} is not connected", id.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_serialize_with_a_display_and_parse_back() {
        let location = Location {
            fs: FsId::local(),
            path: VPath::posix(b"/srv/app".to_vec()),
        };
        let json = serde_json::to_value(location.view()).unwrap();
        assert_eq!(json["fs"], "local");
        assert_eq!(json["display"], "/srv/app");
        // The UI passes the whole view back; the extra field is ignored.
        let parsed: Location = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, location);
    }

    #[test]
    fn the_registry_always_has_local_and_reports_missing_connections() {
        let registry = FsRegistry::default();
        assert!(registry.get(&FsId::local()).is_ok());
        let missing = FsId("ssh-1".to_string());
        assert!(registry
            .get(&missing)
            .err()
            .unwrap()
            .contains("not connected"));
        registry.unregister(&FsId::local());
        assert!(
            registry.get(&FsId::local()).is_ok(),
            "local can't be removed"
        );
    }
}

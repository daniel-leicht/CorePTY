//! A remote filesystem over SFTP. Paths are raw POSIX bytes ([`VPath::Posix`]),
//! so no remote name is ever mangled on its way to the UI and back.

use super::{
    Capabilities, Entry, EntryKind, Meta, Root, VName, VPath, VRead, VWrite, Vfs, WriteMode,
};
use crate::connections::ConnectionLease;
use crate::sftp::{
    client::{Limits, SftpClient},
    file::{Pipeline, SftpReader, SftpWriter},
    protocol::{self as fx, Attrs, NameEntry},
};
use async_trait::async_trait;
use std::{
    io,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncWrite};

/// Requests sent at once when statting a folder's links.
const STAT_BATCH: usize = 64;

pub struct SftpFs {
    client: Arc<SftpClient>,
    home: Vec<u8>,
    read: Pipeline,
    write: Pipeline,
    /// Keeps the SSH connection up while this filesystem is in use (`None`
    /// over a plain stream, in tests).
    _lease: Option<ConnectionLease>,
}

impl SftpFs {
    /// Start SFTP over `stream` (a subsystem channel, or a test server's pipes).
    pub async fn start<S>(stream: S, lease: Option<ConnectionLease>) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let client = Arc::new(SftpClient::start(stream).await?);
        let home = client.realpath(b".").await?;
        let limits = if client.extensions.limits {
            client.limits().await.unwrap_or_default()
        } else {
            Limits::default()
        };
        Ok(Self {
            client,
            home,
            read: pipeline_for(limits.max_read),
            write: pipeline_for(limits.max_write),
            _lease: lease,
        })
    }

    /// Open the `sftp` subsystem on a shared SSH connection.
    pub async fn over_connection(lease: ConnectionLease) -> Result<Self, String> {
        let mut channel = lease.open_session().await?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| format!("could not start SFTP on {}: {e}", lease.label()))?;
        loop {
            match channel.wait().await {
                Some(russh::ChannelMsg::Success) => break,
                Some(russh::ChannelMsg::Failure) | None => {
                    return Err(format!(
                        "{} refused SFTP (is the SFTP subsystem enabled on the server?)",
                        lease.label()
                    ))
                }
                Some(_) => {}
            }
        }
        let label = lease.label();
        Self::start(channel.into_stream(), Some(lease))
            .await
            .map_err(|e| format!("could not start SFTP on {label}: {e}"))
    }

    pub fn has_ended(&self) -> bool {
        self.client.has_ended()
    }

    /// Resolves with the reason once the session has ended.
    pub fn ended(&self) -> impl std::future::Future<Output = String> + Send + 'static {
        self.client.ended()
    }

    /// Override the chunk sizes and pipeline depths (benchmarks).
    #[cfg(test)]
    pub fn set_pipelines(&mut self, read: Pipeline, write: Pipeline) {
        self.read = read;
        self.write = write;
    }

    #[cfg(test)]
    pub fn pipelines(&self) -> (Pipeline, Pipeline) {
        (self.read, self.write)
    }
}

/// Every server must take 32 KiB reads and writes (the draft's minimum).
const SAFE_CHUNK: u64 = 32 * 1024;
/// OpenSSH takes up to 256 KiB packets; past that, bigger chunks gain nothing.
const LARGEST_CHUNK: u64 = 255 * 1024;
/// Bytes kept in flight per open file: enough for a long fast link (more than
/// the SSH window adds nothing), bounded so memory stays small.
const IN_FLIGHT: u64 = 8 * 1024 * 1024;

/// Chunk size within the server's limit (`0`: none given, so the safe size),
/// and as many in flight as make up [`IN_FLIGHT`].
fn pipeline_for(limit: u64) -> Pipeline {
    let chunk = match limit {
        0 => SAFE_CHUNK,
        limit => limit.min(LARGEST_CHUNK),
    };
    Pipeline {
        chunk: chunk as u32,
        depth: (IN_FLIGHT / chunk).clamp(4, 256) as usize,
    }
}

fn bytes(path: &VPath) -> io::Result<&[u8]> {
    path.to_posix()
}

fn time(seconds: Option<u32>) -> Option<SystemTime> {
    seconds.map(|seconds| UNIX_EPOCH + Duration::from_secs(u64::from(seconds)))
}

fn seconds(time: SystemTime) -> u32 {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    seconds.min(u64::from(u32::MAX)) as u32
}

fn kind(attrs: &Attrs) -> EntryKind {
    match attrs.file_type() {
        Some(fx::S_IFDIR) => EntryKind::Directory,
        Some(fx::S_IFREG) => EntryKind::File,
        Some(fx::S_IFLNK) => EntryKind::Symlink,
        Some(_) => EntryKind::Other,
        // No type given: a size suggests a plain file.
        None if attrs.size.is_some() => EntryKind::File,
        None => EntryKind::Other,
    }
}

/// `target` is what a link points at (when `attrs` is a link's own).
fn meta(name: &[u8], attrs: &Attrs, target: Option<&Attrs>) -> Meta {
    let kind = kind(attrs);
    let effective = target.unwrap_or(attrs);
    let is_file = matches!(self::kind(effective), EntryKind::File);
    let mode = attrs.permissions.map(|mode| mode & 0o7777);
    Meta {
        kind,
        is_directory: effective.is_dir(),
        size: if is_file { effective.size } else { None },
        modified: time(effective.mtime),
        accessed: time(effective.atime),
        mode,
        readonly: mode.is_some_and(|mode| mode & 0o222 == 0),
        hidden: name.first() == Some(&b'.'),
        owner: attrs.uid.map(|uid| uid.to_string()),
        group: attrs.gid.map(|gid| gid.to_string()),
    }
}

/// Owner and group from an `ls -l` style line, as servers send in listings:
/// `-rw-r--r--    1 alice    staff        1234 Jan  1 00:00 notes.txt`.
fn owner_and_group(longname: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(longname);
    let mut fields = text.split_whitespace();
    let mode = fields.next()?;
    let looks_like_mode = mode.len() >= 10
        && mode
            .chars()
            .next()
            .is_some_and(|kind| "-dlcbps".contains(kind));
    if !looks_like_mode {
        return None;
    }
    fields.next()?.parse::<u64>().ok()?; // the link count
    let owner = fields.next()?;
    let group = fields.next()?;
    Some((owner.to_string(), group.to_string()))
}

fn name_of(path: &[u8]) -> &[u8] {
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    trimmed
        .rsplit(|&byte| byte == b'/')
        .next()
        .unwrap_or(trimmed)
}

fn already_exists(path: &[u8]) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} already exists", String::from_utf8_lossy(path)),
    )
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !path.ends_with(b"/") {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

impl SftpFs {
    async fn exists(&self, path: &[u8]) -> io::Result<Option<Attrs>> {
        match self.client.lstat(path).await {
            Ok(attrs) => Ok(Some(attrs)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// v3 has no "already exists" code: servers say FAILURE. Look, and say so.
    async fn explain_failure(&self, path: &[u8], error: io::Error) -> io::Error {
        if error.kind() == io::ErrorKind::Other && matches!(self.exists(path).await, Ok(Some(_))) {
            already_exists(path)
        } else {
            error
        }
    }

    async fn entries(&self, dir: &[u8]) -> io::Result<Vec<Entry>> {
        let listed = self.client.read_dir(dir).await?;
        // Links: one STAT each, sent in batches, to learn what they point at.
        let mut targets: Vec<Option<Attrs>> = vec![None; listed.len()];
        let links: Vec<usize> = (0..listed.len())
            .filter(|&index| listed[index].attrs.is_symlink())
            .collect();
        for batch in links.chunks(STAT_BATCH) {
            let requests: Vec<_> = batch
                .iter()
                .map(|&index| {
                    self.client
                        .stat_request(&join(dir, &listed[index].filename))
                })
                .collect();
            for (&index, request) in batch.iter().zip(requests) {
                // A dangling link just stays a link.
                targets[index] = request.await.ok();
            }
        }
        Ok(listed
            .into_iter()
            .zip(targets)
            .map(|(entry, target)| {
                let NameEntry {
                    filename,
                    longname,
                    attrs,
                } = entry;
                let mut meta = meta(&filename, &attrs, target.as_ref());
                // v3 carries owner and group names only in the `ls -l` line.
                if let Some((owner, group)) = owner_and_group(&longname) {
                    meta.owner = Some(owner);
                    meta.group = Some(group);
                }
                Entry {
                    meta,
                    path: VPath::Posix(join(dir, &filename)),
                    name: VName::Posix(filename),
                    error: None,
                }
            })
            .collect())
    }

    /// Delete a folder's contents (depth first), then the folder.
    async fn remove_tree(&self, dir: Vec<u8>) -> io::Result<()> {
        let mut stack = vec![(dir, false)];
        while let Some((dir, emptied)) = stack.pop() {
            if emptied {
                self.client.rmdir(&dir).await?;
                continue;
            }
            stack.push((dir.clone(), true));
            let mut files = Vec::new();
            for entry in self.client.read_dir(&dir).await? {
                let path = join(&dir, &entry.filename);
                // Links are removed, never followed.
                if entry.attrs.is_dir() {
                    stack.push((path, false));
                } else {
                    files.push(path);
                }
            }
            let requests: Vec<_> = files
                .iter()
                .map(|path| self.client.remove_request(path))
                .collect();
            for request in requests {
                request.await?;
            }
        }
        Ok(())
    }

    /// Nothing replaces a folder in one step. Move it aside, put the new entry
    /// in its place, then delete the old one (restored if the rename fails).
    async fn replace_by_moving_aside(&self, from: &[u8], to: &[u8], dir: bool) -> io::Result<()> {
        let mut aside = to.to_vec();
        aside.extend_from_slice(
            format!(".corepty-{}.old", uuid::Uuid::new_v4().simple()).as_bytes(),
        );
        self.client.rename(to, &aside).await?;
        if let Err(e) = self.client.rename(from, to).await {
            return Err(match self.client.rename(&aside, to).await {
                Ok(()) => e,
                Err(_) => io::Error::new(
                    e.kind(),
                    format!(
                        "{e}; the original is now at {}",
                        String::from_utf8_lossy(&aside)
                    ),
                ),
            });
        }
        // The new entry is in place; the old one is cleanup.
        let _ = if dir {
            self.remove_tree(aside).await
        } else {
            self.client.remove(&aside).await
        };
        Ok(())
    }
}

#[async_trait]
impl Vfs for SftpFs {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            trash: false,
            open_native: false,
            posix_permissions: true,
            symlinks: true,
        }
    }

    async fn home(&self) -> io::Result<VPath> {
        Ok(VPath::Posix(self.home.clone()))
    }

    async fn roots(&self) -> io::Result<Vec<Root>> {
        let mut roots = vec![Root {
            label: "Home".to_string(),
            path: VPath::Posix(self.home.clone()),
        }];
        if self.home != b"/" {
            roots.push(Root {
                label: "Filesystem".to_string(),
                path: VPath::posix(b"/".to_vec()),
            });
        }
        Ok(roots)
    }

    async fn resolve(&self, input: &str, base: Option<&VPath>) -> io::Result<VPath> {
        let input = input.trim_end_matches(['\r', '\n']).as_bytes();
        let path = if input == b"~" {
            self.home.clone()
        } else if let Some(rest) = input.strip_prefix(b"~/") {
            join(&self.home, rest)
        } else if input.starts_with(b"~") {
            // ~user: only the server knows where that is.
            self.client.expand_path(input).await?
        } else if input.starts_with(b"/") {
            input.to_vec()
        } else {
            let base = match base {
                Some(base) => bytes(base)?.to_vec(),
                None => self.home.clone(),
            };
            join(&base, input)
        };
        // The server resolves `..`, `.` and links.
        let resolved = self.client.realpath(&path).await?;
        if !self.client.stat(&resolved).await?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", String::from_utf8_lossy(&resolved)),
            ));
        }
        Ok(VPath::Posix(resolved))
    }

    async fn canonicalize(&self, path: &VPath) -> io::Result<VPath> {
        self.client.realpath(bytes(path)?).await.map(VPath::Posix)
    }

    async fn list(&self, dir: &VPath) -> io::Result<Vec<Entry>> {
        self.entries(bytes(dir)?).await
    }

    async fn stat(&self, path: &VPath) -> io::Result<Meta> {
        let path = bytes(path)?;
        let attrs = self.client.stat(path).await?;
        Ok(meta(name_of(path), &attrs, None))
    }

    async fn lstat(&self, path: &VPath) -> io::Result<Meta> {
        let path = bytes(path)?;
        let attrs = self.client.lstat(path).await?;
        if attrs.is_symlink() {
            let target = self.client.stat(path).await.ok();
            return Ok(meta(name_of(path), &attrs, target.as_ref()));
        }
        Ok(meta(name_of(path), &attrs, None))
    }

    async fn mkdir(&self, path: &VPath) -> io::Result<()> {
        let path = bytes(path)?;
        match self.client.mkdir(path).await {
            Ok(()) => Ok(()),
            Err(e) => Err(self.explain_failure(path, e).await),
        }
    }

    async fn rename(&self, from: &VPath, to: &VPath, overwrite: bool) -> io::Result<()> {
        let (from, to) = (bytes(from)?, bytes(to)?);
        if from == to {
            return Ok(());
        }
        let Some(existing) = self.exists(to).await? else {
            return match self.client.rename(from, to).await {
                Ok(()) => Ok(()),
                Err(e) => Err(self.explain_failure(to, e).await),
            };
        };
        if !overwrite {
            return Err(already_exists(to));
        }
        if existing.is_dir() {
            return self.replace_by_moving_aside(from, to, true).await;
        }
        if self.client.extensions.posix_rename {
            // Replaces the file in one step: the old one survives a failure.
            return self.client.posix_rename(from, to).await;
        }
        self.replace_by_moving_aside(from, to, false).await
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> io::Result<()> {
        let path = bytes(path)?;
        let attrs = self.client.lstat(path).await?;
        if attrs.is_dir() {
            if recursive {
                self.remove_tree(path.to_vec()).await
            } else {
                self.client.rmdir(path).await
            }
        } else {
            self.client.remove(path).await
        }
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> io::Result<VRead> {
        let handle = self
            .client
            .open(bytes(path)?, fx::FXF_READ, &Attrs::default())
            .await?;
        Ok(Box::pin(SftpReader::new(
            self.client.clone(),
            handle,
            offset,
            self.read,
        )))
    }

    async fn open_write(&self, path: &VPath, mode: WriteMode) -> io::Result<VWrite> {
        let path = bytes(path)?;
        let flags = match mode {
            WriteMode::CreateNew => fx::FXF_WRITE | fx::FXF_CREAT | fx::FXF_EXCL,
            WriteMode::Truncate => fx::FXF_WRITE | fx::FXF_CREAT | fx::FXF_TRUNC,
            // Not SSH_FXF_APPEND: servers differ on whether it ignores the
            // write offsets. Write at the current end instead.
            WriteMode::Append => fx::FXF_WRITE,
        };
        let handle = match self.client.open(path, flags, &Attrs::default()).await {
            Ok(handle) => handle,
            Err(e) if mode == WriteMode::CreateNew => {
                return Err(self.explain_failure(path, e).await)
            }
            Err(e) => return Err(e),
        };
        let offset = if mode == WriteMode::Append {
            match self.client.fstat(&handle).await {
                Ok(attrs) => attrs.size.unwrap_or(0),
                Err(e) => {
                    self.client.close_quietly(&handle);
                    return Err(e);
                }
            }
        } else {
            0
        };
        Ok(Box::pin(SftpWriter::new(
            self.client.clone(),
            handle,
            offset,
            self.write,
        )))
    }

    async fn set_times(
        &self,
        path: &VPath,
        modified: SystemTime,
        accessed: Option<SystemTime>,
    ) -> io::Result<()> {
        // v3 sets both times together; keep "accessed" at "modified" if unknown.
        let attrs = Attrs {
            mtime: Some(seconds(modified)),
            atime: Some(seconds(accessed.unwrap_or(modified))),
            ..Attrs::default()
        };
        self.client.setstat(bytes(path)?, &attrs).await
    }

    async fn set_mode(&self, path: &VPath, mode: u32) -> io::Result<()> {
        let attrs = Attrs {
            permissions: Some(mode & 0o7777),
            ..Attrs::default()
        };
        self.client.setstat(bytes(path)?, &attrs).await
    }

    async fn read_link(&self, path: &VPath) -> io::Result<VPath> {
        self.client.readlink(bytes(path)?).await.map(VPath::Posix)
    }

    async fn symlink(&self, target: &VPath, link: &VPath, _target_is_dir: bool) -> io::Result<()> {
        // A local link's target is a native path; only POSIX targets make sense here.
        self.client.symlink(bytes(target)?, bytes(link)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::testing::local_sftp;
    use crate::transfer::engine::{tests as engine, Side, TransferKind};
    use crate::vfs::FsId;
    use std::fs;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A path under the server's home (the temp dir it runs in).
    fn at(fs: &SftpFs, relative: &str) -> VPath {
        if relative.is_empty() {
            VPath::Posix(fs.home.clone())
        } else {
            VPath::Posix(join(&fs.home, relative.as_bytes()))
        }
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    async fn read_all(fs: &SftpFs, path: &VPath, offset: u64) -> Vec<u8> {
        let mut data = Vec::new();
        fs.open_read(path, offset)
            .await
            .unwrap()
            .read_to_end(&mut data)
            .await
            .unwrap();
        data
    }

    async fn write_all(fs: &SftpFs, path: &VPath, mode: WriteMode, data: &[u8]) {
        let mut writer = fs.open_write(path, mode).await.unwrap();
        // Odd-sized writes, so chunks never line up with the caller's buffers.
        for piece in data.chunks(10_007) {
            writer.write_all(piece).await.unwrap();
        }
        writer.shutdown().await.unwrap();
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        let mut names: Vec<_> = entries.iter().map(|entry| entry.name.display()).collect();
        names.sort();
        names
    }

    fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
        entries
            .iter()
            .find(|entry| entry.name.display() == name)
            .unwrap()
    }

    #[tokio::test]
    async fn lists_reads_and_writes_files_through_a_real_sftp_server() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("hello.txt"), b"hello").unwrap();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };

        let listed = sftp.list(&at(&sftp, "")).await.unwrap();
        assert_eq!(names(&listed), [".hidden", "docs", "hello.txt"]);
        let hello = find(&listed, "hello.txt");
        assert_eq!(hello.meta.kind, EntryKind::File);
        assert_eq!(hello.meta.size, Some(5));
        assert!(hello.meta.modified.is_some());
        let docs = find(&listed, "docs");
        assert!(docs.meta.is_directory && docs.meta.size.is_none());
        assert!(find(&listed, ".hidden").meta.hidden);

        // Several pipelined chunks' worth, plus a ragged end.
        let data = pattern(3 * 1024 * 1024 + 12_345);
        let big = at(&sftp, "big.bin");
        write_all(&sftp, &big, WriteMode::CreateNew, &data).await;
        assert_eq!(fs::read(dir.path().join("big.bin")).unwrap(), data);
        assert_eq!(read_all(&sftp, &big, 0).await, data);
        assert_eq!(read_all(&sftp, &big, 1_000_000).await, &data[1_000_000..]);
        assert_eq!(sftp.stat(&big).await.unwrap().size, Some(data.len() as u64));

        // CreateNew never touches an existing file.
        let error = sftp
            .open_write(&big, WriteMode::CreateNew)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(dir.path().join("big.bin")).unwrap().len(),
            data.len()
        );

        // Append continues at the end; Truncate starts over.
        let hello = at(&sftp, "hello.txt");
        write_all(&sftp, &hello, WriteMode::Append, b", world").await;
        assert_eq!(
            fs::read(dir.path().join("hello.txt")).unwrap(),
            b"hello, world"
        );
        write_all(&sftp, &hello, WriteMode::Truncate, b"hi").await;
        assert_eq!(fs::read(dir.path().join("hello.txt")).unwrap(), b"hi");

        // An empty file reads as empty.
        write_all(&sftp, &at(&sftp, "empty"), WriteMode::CreateNew, b"").await;
        assert!(read_all(&sftp, &at(&sftp, "empty"), 0).await.is_empty());

        let missing = sftp.stat(&at(&sftp, "nope")).await.err().unwrap();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(sftp.list(&at(&sftp, "nope")).await.is_err());
    }

    #[tokio::test]
    async fn renames_replace_safely_and_removal_is_recursive() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), b"new").unwrap();
        fs::write(root.join("b.txt"), b"old").unwrap();
        fs::create_dir_all(root.join("tree/inner/deeper")).unwrap();
        fs::write(root.join("tree/inner/deeper/x"), b"x").unwrap();
        fs::write(root.join("tree/top"), b"t").unwrap();
        fs::create_dir(root.join("old-folder")).unwrap();
        fs::write(root.join("old-folder/stale"), b"s").unwrap();
        let Some((sftp, _server)) = local_sftp(root).await else {
            return;
        };

        let error = sftp
            .rename(&at(&sftp, "a.txt"), &at(&sftp, "b.txt"), false)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"old");

        sftp.rename(&at(&sftp, "a.txt"), &at(&sftp, "b.txt"), true)
            .await
            .unwrap();
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"new");
        assert!(!root.join("a.txt").exists());

        // A folder in the way is moved aside, replaced, then deleted.
        sftp.rename(&at(&sftp, "tree"), &at(&sftp, "old-folder"), true)
            .await
            .unwrap();
        assert!(root.join("old-folder/inner/deeper/x").is_file());
        assert!(!root.join("old-folder/stale").exists());
        let leftovers: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("corepty"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        sftp.mkdir(&at(&sftp, "made")).await.unwrap();
        assert!(root.join("made").is_dir());
        let again = sftp.mkdir(&at(&sftp, "made")).await.err().unwrap();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);

        assert!(sftp.remove(&at(&sftp, "old-folder"), false).await.is_err());
        sftp.remove(&at(&sftp, "old-folder"), true).await.unwrap();
        assert!(!root.join("old-folder").exists());
        sftp.remove(&at(&sftp, "b.txt"), false).await.unwrap();
        assert!(!root.join("b.txt").exists());
    }

    #[tokio::test]
    async fn resolves_typed_paths_on_the_server() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        fs::write(dir.path().join("file.txt"), b"").unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };
        let home = at(&sftp, "");
        assert_eq!(sftp.resolve("~", None).await.unwrap(), home);
        assert_eq!(sftp.resolve("~/sub", None).await.unwrap(), at(&sftp, "sub"));
        assert_eq!(
            sftp.resolve("sub/deeper/..", None).await.unwrap(),
            at(&sftp, "sub")
        );
        assert_eq!(
            sftp.resolve("deeper", Some(&at(&sftp, "sub")))
                .await
                .unwrap(),
            at(&sftp, "sub/deeper")
        );
        let absolute = at(&sftp, "sub").display();
        assert_eq!(
            sftp.resolve(&absolute, None).await.unwrap(),
            at(&sftp, "sub")
        );
        let file = sftp.resolve("file.txt", None).await.err().unwrap();
        assert_eq!(file.kind(), io::ErrorKind::NotADirectory);
        assert!(sftp.resolve("missing", None).await.is_err());
        assert_eq!(sftp.roots().await.unwrap()[0].path, home);
    }

    #[tokio::test]
    async fn sets_times_and_permissions() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("f"), b"data").unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };
        let file = at(&sftp, "f");
        let when = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        sftp.set_times(&file, when, None).await.unwrap();
        assert_eq!(sftp.stat(&file).await.unwrap().modified, Some(when));
        assert_eq!(
            fs::metadata(dir.path().join("f"))
                .unwrap()
                .modified()
                .unwrap(),
            when
        );

        sftp.set_mode(&file, 0o444).await.unwrap();
        assert!(sftp.stat(&file).await.unwrap().readonly);
        sftp.set_mode(&file, 0o640).await.unwrap();
        let meta = sftp.stat(&file).await.unwrap();
        assert!(!meta.readonly);
        #[cfg(unix)]
        assert_eq!(meta.mode, Some(0o640));
    }

    fn sftp_side(sftp: SftpFs) -> (Side, Arc<SftpFs>) {
        let sftp = Arc::new(sftp);
        let side = Side {
            id: FsId("sftp:test".to_string()),
            fs: sftp.clone(),
        };
        (side, sftp)
    }

    #[tokio::test]
    async fn the_transfer_engine_uploads_downloads_and_moves_on_the_server() {
        let (local, remote) = (tempdir().unwrap(), tempdir().unwrap());
        let project = local.path().join("project");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("src/main.rs"), pattern(200_000)).unwrap();
        fs::write(project.join("README"), b"read me").unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        fs::File::options()
            .write(true)
            .open(project.join("README"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        // A folder's time too (set after its contents were written).
        let older = UNIX_EPOCH + Duration::from_secs(1_400_000_000);
        crate::vfs::local::LocalFs
            .set_times(&VPath::native(project.join("src")), older, None)
            .await
            .unwrap();
        let Some((sftp, _server)) = local_sftp(remote.path()).await else {
            return;
        };
        let (side, sftp) = sftp_side(sftp);

        let upload = engine::run(
            TransferKind::Copy,
            engine::local(),
            side.clone(),
            &[VPath::native(&project)],
            &at(&sftp, ""),
            engine::no_conflicts(),
        )
        .await;
        assert_eq!(upload.result, Ok(()));
        assert_eq!(
            fs::read(remote.path().join("project/src/main.rs")).unwrap(),
            pattern(200_000)
        );
        // Timestamps survive the trip.
        let uploaded = fs::metadata(remote.path().join("project/README")).unwrap();
        assert_eq!(uploaded.modified().unwrap(), old);
        let folder = fs::metadata(remote.path().join("project/src")).unwrap();
        assert_eq!(folder.modified().unwrap(), older);

        let back = local.path().join("back");
        fs::create_dir(&back).unwrap();
        let download = engine::run(
            TransferKind::Copy,
            side.clone(),
            engine::local(),
            &[at(&sftp, "project")],
            &VPath::native(&back),
            engine::no_conflicts(),
        )
        .await;
        assert_eq!(download.result, Ok(()));
        assert_eq!(fs::read(back.join("project/README")).unwrap(), b"read me");
        assert_eq!(
            fs::read(back.join("project/src/main.rs")).unwrap(),
            pattern(200_000)
        );
        let downloaded = fs::metadata(back.join("project/README")).unwrap();
        assert_eq!(downloaded.modified().unwrap(), old);
        let folder = fs::metadata(back.join("project/src")).unwrap();
        assert_eq!(folder.modified().unwrap(), older);

        // A move within the server is a rename there.
        sftp.mkdir(&at(&sftp, "archive")).await.unwrap();
        let moved = engine::run(
            TransferKind::Move,
            side.clone(),
            side,
            &[at(&sftp, "project")],
            &at(&sftp, "archive"),
            engine::no_conflicts(),
        )
        .await;
        assert_eq!(moved.result, Ok(()));
        assert!(remote.path().join("archive/project/src/main.rs").is_file());
        assert!(!remote.path().join("project").exists());
    }

    // macOS (APFS) refuses names that aren't valid UTF-8, so only Linux can host one.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn names_that_are_not_utf8_survive_listing_renaming_and_reading() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempdir().unwrap();
        let latin1 = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        fs::write(dir.path().join(latin1), b"bytes").unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };
        let listed = sftp.list(&at(&sftp, "")).await.unwrap();
        let entry = &listed[0];
        assert_eq!(entry.name, VName::Posix(b"caf\xe9.txt".to_vec()));
        assert_eq!(read_all(&sftp, &entry.path, 0).await, b"bytes");
        let renamed = VPath::Posix(join(&sftp.home, b"\xff\xfe"));
        sftp.rename(&entry.path, &renamed, false).await.unwrap();
        let on_disk = dir.path().join(std::ffi::OsStr::from_bytes(b"\xff\xfe"));
        assert!(on_disk.is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_chmod_on_the_server_keeps_folders_openable_and_skips_links() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("site/css")).unwrap();
        fs::write(dir.path().join("site/css/app.css"), b"*{}").unwrap();
        fs::write(dir.path().join("outside.txt"), b"not mine").unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside.txt"), dir.path().join("site/link"))
            .unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };
        let mode = |path: &str| {
            fs::metadata(dir.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        let before = mode("outside.txt");
        crate::vfs::commands::chmod(&sftp, &at(&sftp, "site"), 0o640, true)
            .await
            .unwrap();
        assert_eq!(mode("site/css/app.css"), 0o640);
        assert_eq!(mode("site/css"), 0o750);
        assert_eq!(mode("site"), 0o750);
        assert_eq!(
            mode("outside.txt"),
            before,
            "the link's target is outside the tree"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn links_are_listed_with_their_targets_kind() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();
        fs::write(dir.path().join("file"), b"12345").unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };
        for (target, link) in [
            ("folder", "to-folder"),
            ("file", "to-file"),
            ("gone", "dangling"),
        ] {
            sftp.symlink(&at(&sftp, target), &at(&sftp, link), false)
                .await
                .unwrap();
        }
        assert_eq!(
            sftp.read_link(&at(&sftp, "to-file")).await.unwrap(),
            at(&sftp, "file")
        );
        let listed = sftp.list(&at(&sftp, "")).await.unwrap();
        assert_eq!(find(&listed, "to-folder").meta.kind, EntryKind::Symlink);
        assert!(find(&listed, "to-folder").meta.is_directory);
        assert_eq!(find(&listed, "to-file").meta.size, Some(5));
        assert!(!find(&listed, "dangling").meta.is_directory);
        // Removing a link to a folder removes the link only.
        sftp.remove(&at(&sftp, "to-folder"), true).await.unwrap();
        assert!(dir.path().join("folder").is_dir());
        let link = sftp.lstat(&at(&sftp, "to-file")).await.unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
    }

    async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..300 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn sftp_shares_an_ssh_connection_with_a_terminal() {
        use crate::connections::{tests as ssh, ConnectionRegistry};
        use std::sync::atomic::Ordering;
        if crate::sftp::testing::sftp_server().is_none() {
            return;
        }
        let root = tempdir().unwrap();
        fs::write(root.path().join("remote.txt"), b"over ssh").unwrap();
        let server = ssh::start_sftp_server(root.path().to_path_buf()).await;
        let keys = tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let lease = registry
            .connect(
                "conn".into(),
                &ssh::target(&server, "secret"),
                &ssh::temp_host_keys(&keys),
                &|_| {},
            )
            .await
            .unwrap();
        // A terminal tab on the same connection.
        let terminal = registry.attach("conn").unwrap();
        let sftp = SftpFs::over_connection(lease).await.unwrap();

        let home = sftp.home().await.unwrap();
        assert_eq!(names(&sftp.list(&home).await.unwrap()), ["remote.txt"]);
        let data = pattern(1024 * 1024 + 3);
        let uploaded = at(&sftp, "up.bin");
        write_all(&sftp, &uploaded, WriteMode::CreateNew, &data).await;
        assert_eq!(fs::read(root.path().join("up.bin")).unwrap(), data);
        assert_eq!(read_all(&sftp, &uploaded, 0).await, data);
        assert_eq!(server.connections.load(Ordering::SeqCst), 1);
        assert_eq!(registry.list()[0].users, 2);

        // Closing the file panel leaves the terminal's connection up…
        drop(sftp);
        assert!(eventually(|| registry.list().first().is_some_and(|c| c.users == 1)).await);
        assert_eq!(server.closed.load(Ordering::SeqCst), 0);
        // …until the terminal goes too.
        drop(terminal);
        assert!(eventually(|| server.closed.load(Ordering::SeqCst) == 1).await);
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn a_server_without_sftp_says_so() {
        use crate::connections::{tests as ssh, ConnectionRegistry};
        let server = ssh::start_server(ssh::host_key()).await;
        let keys = tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let lease = registry
            .connect(
                "conn".into(),
                &ssh::target(&server, "secret"),
                &ssh::temp_host_keys(&keys),
                &|_| {},
            )
            .await
            .unwrap();
        let error = SftpFs::over_connection(lease).await.err().unwrap();
        assert!(error.contains("refused SFTP"), "{error}");
        // The failed attempt didn't keep the connection alive.
        assert!(eventually(|| registry.list().is_empty()).await);
    }

    #[test]
    fn owner_and_group_come_from_the_long_listing_line() {
        assert_eq!(
            owner_and_group(b"-rw-r--r--    1 alice    staff        1234 Jan  1 00:00 notes.txt"),
            Some(("alice".to_string(), "staff".to_string()))
        );
        assert_eq!(
            owner_and_group(b"drwxr-xr-x   12 1000     1000         4096 Mar  3  2024 src"),
            Some(("1000".to_string(), "1000".to_string()))
        );
        // Servers that send something else (or nothing) give no names.
        assert_eq!(owner_and_group(b"notes.txt"), None);
        assert_eq!(owner_and_group(b""), None);
        assert_eq!(owner_and_group(b"-rw-r--r-- x alice staff"), None);
    }

    #[tokio::test]
    async fn listings_carry_owner_names_and_modes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("mine.txt"), b"x").unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };
        let listed = sftp.list(&at(&sftp, "")).await.unwrap();
        let mine = find(&listed, "mine.txt");
        let owner = mine.meta.owner.as_deref().unwrap_or("");
        assert!(!owner.is_empty(), "{:?}", mine.meta);
        assert!(mine.meta.group.is_some());
        assert!(mine.meta.mode.is_some_and(|mode| mode & 0o400 != 0));
    }

    #[tokio::test]
    async fn an_interrupted_upload_resumes_on_the_server() {
        use crate::transfer::engine::{tests::Scripted, Resolution};
        use crate::vfs::memory::MemFs;
        let remote_dir = tempdir().unwrap();
        let Some((sftp, _server)) = local_sftp(remote_dir.path()).await else {
            return;
        };
        let (side, sftp) = sftp_side(sftp);
        let source = Arc::new(MemFs::new());
        let data = pattern(3 * 1024 * 1024 + 5);
        source.add_dir("/up");
        source.add_file("/up/big.iso", &data);
        let (sources, home) = ([MemFs::path("/up/big.iso")], at(&sftp, ""));
        let upload = |resolver: Arc<Scripted>| {
            engine::run(
                TransferKind::Copy,
                engine::memory(&source, "memory"),
                side.clone(),
                &sources,
                &home,
                resolver,
            )
        };

        source.fail_next_read_midway();
        assert!(upload(engine::no_conflicts()).await.result.is_err());
        let partial = remote_dir.path().join("big.iso.filepart");
        let have = fs::read(&partial).unwrap().len();
        assert!(have > 0 && have < data.len(), "{have}");

        let resumed = upload(Scripted::new(&[(Resolution::Resume, false)])).await;
        assert_eq!(resumed.result, Ok(()));
        assert_eq!(fs::read(remote_dir.path().join("big.iso")).unwrap(), data);
        assert!(!partial.exists());
    }

    #[test]
    fn pipelines_follow_the_server_limits() {
        // No limits given: the size every server must take, many in flight.
        let safe = pipeline_for(0);
        assert_eq!((safe.chunk, safe.depth), (32 * 1024, 256));
        // OpenSSH's 255 KiB: capped there, about 8 MiB in flight.
        let openssh = pipeline_for(261_120);
        assert_eq!((openssh.chunk, openssh.depth), (255 * 1024, 32));
        // A server with a small limit is respected.
        assert_eq!(pipeline_for(16 * 1024).chunk, 16 * 1024);
        assert_eq!(pipeline_for(1 << 30).chunk, 255 * 1024);
    }

    #[tokio::test]
    async fn sftp_server_limits_pick_large_chunks() {
        let dir = tempdir().unwrap();
        let Some((sftp, _server)) = local_sftp(dir.path()).await else {
            return;
        };
        let (read, write) = sftp.pipelines();
        if sftp.client.extensions.limits {
            assert!(read.chunk > 32 * 1024 && write.chunk > 32 * 1024);
        }
        assert!(u64::from(read.chunk) * read.depth as u64 >= 4 * 1024 * 1024);
    }

    #[test]
    fn names_are_the_last_component() {
        assert_eq!(name_of(b"/a/b"), b"b");
        assert_eq!(name_of(b"/a/b/"), b"b");
        assert_eq!(name_of(b"x"), b"x");
    }
}

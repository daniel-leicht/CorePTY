//! An in-memory POSIX filesystem for tests: it stands in for a remote host so
//! transfers can be exercised across two different filesystems, and it counts
//! file reads so tests can prove a move never copied any data.

use super::{
    Capabilities, Entry, EntryKind, Meta, Root, VName, VPath, VRead, VWrite, Vfs, WriteMode,
};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::SystemTime,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug, Clone)]
enum Node {
    Dir {
        modified: SystemTime,
    },
    File {
        data: Vec<u8>,
        modified: SystemTime,
        mode: u32,
    },
}

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct MemFs {
    nodes: Arc<Mutex<BTreeMap<Vec<u8>, Node>>>,
    reads: AtomicU64,
    lstats: AtomicU64,
    first_read_hook: Mutex<Option<Hook>>,
}

impl MemFs {
    pub fn new() -> Self {
        let fs = Self::default();
        fs.nodes.lock().unwrap().insert(
            b"/".to_vec(),
            Node::Dir {
                modified: SystemTime::now(),
            },
        );
        fs
    }

    pub fn path(path: &str) -> VPath {
        VPath::posix(path.as_bytes())
    }

    pub fn add_dir(&self, path: &str) {
        self.nodes.lock().unwrap().insert(
            path.as_bytes().to_vec(),
            Node::Dir {
                modified: SystemTime::now(),
            },
        );
    }

    pub fn add_file(&self, path: &str, data: &[u8]) {
        self.nodes.lock().unwrap().insert(
            path.as_bytes().to_vec(),
            Node::File {
                data: data.to_vec(),
                modified: SystemTime::now(),
                mode: 0o644,
            },
        );
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        match self.nodes.lock().unwrap().get(path.as_bytes()) {
            Some(Node::File { data, .. }) => Some(data.clone()),
            _ => None,
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(path.as_bytes())
    }

    /// Every stored path, for asserting exact tree shapes.
    pub fn paths(&self) -> Vec<String> {
        self.nodes
            .lock()
            .unwrap()
            .keys()
            .map(|key| String::from_utf8_lossy(key).into_owned())
            .collect()
    }

    /// How many times file contents were opened for reading.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    /// How many `lstat` calls were made (one per remote round trip on SFTP).
    pub fn lstats(&self) -> u64 {
        self.lstats.load(Ordering::SeqCst)
    }

    fn meta_at(&self, path: &VPath) -> io::Result<Meta> {
        let key = key(path)?;
        let nodes = self.nodes.lock().unwrap();
        nodes
            .get(&key)
            .map(|node| meta_of(&key, node))
            .ok_or_else(|| not_found(&key))
    }

    /// Run `hook` right after the next reader hands out its first chunk, e.g.
    /// to cancel a transfer deterministically mid-file.
    pub fn on_first_read(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.first_read_hook.lock().unwrap() = Some(Arc::new(hook));
    }
}

struct HookedReader {
    inner: std::io::Cursor<Vec<u8>>,
    hook: Option<Hook>,
}

impl AsyncRead for HookedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) {
            if let Some(hook) = self.hook.take() {
                hook();
            }
        }
        polled
    }
}

fn key(path: &VPath) -> io::Result<Vec<u8>> {
    path.to_posix().map(<[u8]>::to_vec)
}

fn not_found(path: &[u8]) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", String::from_utf8_lossy(path)),
    )
}

fn child_prefix(dir: &[u8]) -> Vec<u8> {
    let mut prefix = dir.to_vec();
    if !prefix.ends_with(b"/") {
        prefix.push(b'/');
    }
    prefix
}

fn meta_of(path: &[u8], node: &Node) -> Meta {
    let hidden = VPath::posix(path)
        .file_name()
        .is_some_and(|name| name.display().starts_with('.'));
    match node {
        Node::Dir { modified } => Meta {
            kind: EntryKind::Directory,
            is_directory: true,
            size: None,
            modified: Some(*modified),
            accessed: None,
            mode: Some(0o755),
            readonly: false,
            hidden,
            owner: None,
            group: None,
        },
        Node::File {
            data,
            modified,
            mode,
        } => Meta {
            kind: EntryKind::File,
            is_directory: false,
            size: Some(data.len() as u64),
            modified: Some(*modified),
            accessed: None,
            mode: Some(*mode),
            readonly: mode & 0o222 == 0,
            hidden,
            owner: None,
            group: None,
        },
    }
}

impl MemFs {
    fn parent_must_be_dir(&self, nodes: &BTreeMap<Vec<u8>, Node>, path: &[u8]) -> io::Result<()> {
        let parent = VPath::posix(path)
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent"))?;
        match nodes.get(parent.to_posix()?) {
            Some(Node::Dir { .. }) => Ok(()),
            _ => Err(not_found(parent.to_posix()?)),
        }
    }

    fn subtree_keys(nodes: &BTreeMap<Vec<u8>, Node>, path: &[u8]) -> Vec<Vec<u8>> {
        let prefix = child_prefix(path);
        nodes
            .keys()
            .filter(|key| key.as_slice() == path || key.starts_with(&prefix))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Vfs for MemFs {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            trash: false,
            open_native: false,
            posix_permissions: true,
            symlinks: false,
        }
    }

    async fn home(&self) -> io::Result<VPath> {
        Ok(Self::path("/"))
    }

    async fn roots(&self) -> io::Result<Vec<Root>> {
        Ok(vec![Root {
            label: "/".to_string(),
            path: Self::path("/"),
        }])
    }

    async fn resolve(&self, input: &str, _base: Option<&VPath>) -> io::Result<VPath> {
        Ok(Self::path(input))
    }

    async fn canonicalize(&self, path: &VPath) -> io::Result<VPath> {
        let key = key(path)?;
        if self.nodes.lock().unwrap().contains_key(&key) {
            Ok(path.clone())
        } else {
            Err(not_found(&key))
        }
    }

    async fn list(&self, dir: &VPath) -> io::Result<Vec<Entry>> {
        let dir = key(dir)?;
        let nodes = self.nodes.lock().unwrap();
        if !matches!(nodes.get(&dir), Some(Node::Dir { .. })) {
            return Err(not_found(&dir));
        }
        let prefix = child_prefix(&dir);
        Ok(nodes
            .iter()
            .filter(|(path, _)| {
                path.starts_with(&prefix)
                    && path.len() > prefix.len()
                    && !path[prefix.len()..].contains(&b'/')
            })
            .map(|(path, node)| Entry {
                name: VName::Posix(path[prefix.len()..].to_vec()),
                path: VPath::posix(path.clone()),
                meta: meta_of(path, node),
                error: None,
            })
            .collect())
    }

    async fn stat(&self, path: &VPath) -> io::Result<Meta> {
        self.meta_at(path)
    }

    async fn lstat(&self, path: &VPath) -> io::Result<Meta> {
        self.lstats.fetch_add(1, Ordering::SeqCst);
        self.meta_at(path)
    }

    async fn mkdir(&self, path: &VPath) -> io::Result<()> {
        let key = key(path)?;
        let mut nodes = self.nodes.lock().unwrap();
        if nodes.contains_key(&key) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists"));
        }
        self.parent_must_be_dir(&nodes, &key)?;
        nodes.insert(
            key,
            Node::Dir {
                modified: SystemTime::now(),
            },
        );
        Ok(())
    }

    async fn rename(&self, from: &VPath, to: &VPath, overwrite: bool) -> io::Result<()> {
        let (from, to) = (key(from)?, key(to)?);
        let mut nodes = self.nodes.lock().unwrap();
        if !nodes.contains_key(&from) {
            return Err(not_found(&from));
        }
        self.parent_must_be_dir(&nodes, &to)?;
        if from == to {
            return Ok(());
        }
        if nodes.contains_key(&to) {
            if !overwrite {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists"));
            }
            for stale in Self::subtree_keys(&nodes, &to) {
                nodes.remove(&stale);
            }
        }
        for old in Self::subtree_keys(&nodes, &from) {
            let node = nodes.remove(&old).unwrap();
            let mut new = to.clone();
            new.extend_from_slice(&old[from.len()..]);
            nodes.insert(new, node);
        }
        Ok(())
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> io::Result<()> {
        let key = key(path)?;
        let mut nodes = self.nodes.lock().unwrap();
        let subtree = Self::subtree_keys(&nodes, &key);
        if subtree.is_empty() {
            return Err(not_found(&key));
        }
        if subtree.len() > 1 && !recursive {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                "not empty",
            ));
        }
        for stale in subtree {
            nodes.remove(&stale);
        }
        Ok(())
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> io::Result<VRead> {
        let key = key(path)?;
        let data = match self.nodes.lock().unwrap().get(&key) {
            Some(Node::File { data, .. }) => data.clone(),
            _ => return Err(not_found(&key)),
        };
        self.reads.fetch_add(1, Ordering::SeqCst);
        let start = (offset as usize).min(data.len());
        Ok(Box::pin(HookedReader {
            inner: std::io::Cursor::new(data[start..].to_vec()),
            hook: self.first_read_hook.lock().unwrap().take(),
        }))
    }

    async fn open_write(&self, path: &VPath, mode: WriteMode) -> io::Result<VWrite> {
        let key = key(path)?;
        let mut nodes = self.nodes.lock().unwrap();
        self.parent_must_be_dir(&nodes, &key)?;
        let existing = match nodes.get(&key) {
            Some(Node::File { data, .. }) => Some(data.clone()),
            Some(Node::Dir { .. }) => {
                return Err(io::Error::new(io::ErrorKind::IsADirectory, "is a folder"))
            }
            None => None,
        };
        let buffer = match (mode, existing) {
            (WriteMode::CreateNew, Some(_)) => {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists"))
            }
            (WriteMode::Append, Some(data)) => data,
            (WriteMode::Append, None) => return Err(not_found(&key)),
            _ => Vec::new(),
        };
        // Like a real file, it exists as soon as it's opened.
        nodes.insert(
            key.clone(),
            Node::File {
                data: buffer.clone(),
                modified: SystemTime::now(),
                mode: 0o644,
            },
        );
        Ok(Box::pin(MemWriter {
            nodes: self.nodes.clone(),
            key,
            buffer,
        }))
    }

    async fn set_times(
        &self,
        path: &VPath,
        modified: SystemTime,
        _accessed: Option<SystemTime>,
    ) -> io::Result<()> {
        let key = key(path)?;
        match self.nodes.lock().unwrap().get_mut(&key) {
            Some(Node::File {
                modified: stamp, ..
            })
            | Some(Node::Dir { modified: stamp }) => {
                *stamp = modified;
                Ok(())
            }
            None => Err(not_found(&key)),
        }
    }

    async fn set_mode(&self, path: &VPath, new_mode: u32) -> io::Result<()> {
        let key = key(path)?;
        if let Some(Node::File { mode, .. }) = self.nodes.lock().unwrap().get_mut(&key) {
            *mode = new_mode;
        }
        Ok(())
    }

    async fn read_link(&self, _path: &VPath) -> io::Result<VPath> {
        Err(super::unsupported("symbolic links"))
    }

    async fn symlink(&self, _target: &VPath, _link: &VPath, _dir: bool) -> io::Result<()> {
        Err(super::unsupported("symbolic links"))
    }
}

/// Buffers writes and stores them on every flush, like an OS file would.
struct MemWriter {
    nodes: Arc<Mutex<BTreeMap<Vec<u8>, Node>>>,
    key: Vec<u8>,
    buffer: Vec<u8>,
}

impl MemWriter {
    fn store(&self) {
        if let Some(Node::File { data, .. }) = self.nodes.lock().unwrap().get_mut(&self.key) {
            data.clone_from(&self.buffer);
        }
    }
}

impl AsyncWrite for MemWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.buffer.extend_from_slice(bytes);
        self.store();
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.store();
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.store();
        Poll::Ready(Ok(()))
    }
}

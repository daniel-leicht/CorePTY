//! Copies and moves between any two filesystems.
//!
//! A job is planned first (items and bytes, for the progress bar), then run
//! item by item. File contents stream into a temporary file next to the target
//! and are renamed into place, so a target is never left half-written. Moves
//! within one filesystem are renames: no file data is read, even when a folder
//! is merged into an existing one. Every name clash, including those deep
//! inside a merged folder, goes to the [`ConflictResolver`].

use crate::vfs::{epoch_millis, EntryKind, FsId, Meta, VName, VPath, Vfs, WriteMode};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    io,
    ops::AddAssign,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
};
use uuid::Uuid;

/// Read/write chunk size. Large enough that per-chunk overhead (a blocking-pool
/// hop locally, a round trip remotely) stays small.
const CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkStats {
    pub items: u64,
    pub bytes: u64,
}

impl AddAssign for WorkStats {
    fn add_assign(&mut self, other: Self) {
        self.items = self.items.saturating_add(other.items);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
}

/// Live progress, updated by the engine and sampled by the queue.
#[derive(Default)]
pub struct Counters {
    items: AtomicU64,
    bytes: AtomicU64,
    skipped: AtomicU64,
    current: Mutex<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Items and bytes processed so far, skipped ones included, so the
    /// progress bar still reaches the end.
    pub done: WorkStats,
    pub skipped: u64,
    pub current: String,
}

impl Counters {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            done: WorkStats {
                items: self.items.load(Ordering::Relaxed),
                bytes: self.bytes.load(Ordering::Relaxed),
            },
            skipped: self.skipped.load(Ordering::Relaxed),
            current: self
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    fn add(&self, stats: WorkStats) {
        self.items.fetch_add(stats.items, Ordering::Relaxed);
        self.bytes.fetch_add(stats.bytes, Ordering::Relaxed);
    }

    fn set_current(&self, name: &VName) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = name.display();
    }
}

/// One side of a name clash, as the conflict dialog shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictSide {
    pub path: String,
    pub is_directory: bool,
    pub size: Option<u64>,
    /// Milliseconds since the Unix epoch.
    pub modified: Option<u64>,
}

impl ConflictSide {
    fn new(path: &VPath, meta: &Meta) -> Self {
        Self {
            path: path.display(),
            is_directory: meta.is_directory,
            size: meta.size,
            modified: meta.modified.and_then(epoch_millis),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub name: String,
    pub source: ConflictSide,
    pub target: ConflictSide,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Resolution {
    Overwrite,
    Skip,
    /// Keep both: use `name`, or an automatic "name (copy N)" when absent.
    Rename {
        #[serde(default)]
        name: Option<String>,
    },
    /// Overwrite only when the source is newer; otherwise skip.
    OverwriteIfNewer,
    /// Overwrite only when the sizes differ; otherwise skip.
    OverwriteIfDifferentSize,
    Cancel,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictAnswer {
    pub resolution: Resolution,
    /// Reuse this answer for every later conflict in the job.
    #[serde(default)]
    pub apply_to_all: bool,
}

#[async_trait]
pub trait ConflictResolver: Send + Sync {
    async fn resolve(&self, conflict: Conflict) -> ConflictAnswer;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    Cancelled,
    Failed(String),
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Operation cancelled"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

fn failed(action: &str, path: &VPath, error: io::Error) -> TransferError {
    TransferError::Failed(format!("could not {action} {}: {error}", path.display()))
}

/// A filesystem taking part in a transfer.
#[derive(Clone)]
pub struct Side {
    pub id: FsId,
    pub fs: Arc<dyn Vfs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    /// Skipped, or (for a folder) only partly moved.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Nothing is at the target.
    Fresh,
    /// Replace what's at the target.
    Overwrite,
    /// The target is a folder; move/copy the source folder's contents into it.
    Merge,
}

enum Decision {
    Overwrite,
    Skip,
    RenameTo(VName),
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct Engine {
    kind: TransferKind,
    source: Side,
    target: Side,
    counters: Arc<Counters>,
    cancel: watch::Receiver<bool>,
    resolver: Arc<dyn ConflictResolver>,
    /// An "apply to all" answer, reused for every later conflict.
    sticky: Option<Resolution>,
    /// Planned totals of every source folder, for counting renamed subtrees.
    folder_stats: HashMap<VPath, WorkStats>,
}

impl Engine {
    pub fn new(
        kind: TransferKind,
        source: Side,
        target: Side,
        counters: Arc<Counters>,
        cancel: watch::Receiver<bool>,
        resolver: Arc<dyn ConflictResolver>,
    ) -> Self {
        Self {
            kind,
            source,
            target,
            counters,
            cancel,
            resolver,
            sticky: None,
            folder_stats: HashMap::new(),
        }
    }

    /// Count everything the job will touch.
    pub async fn plan(&mut self, sources: &[VPath]) -> Result<WorkStats, TransferError> {
        let mut total = WorkStats::default();
        for source in sources {
            total += self.measure(source).await?;
        }
        Ok(total)
    }

    pub async fn run(
        &mut self,
        sources: &[VPath],
        destination: &VPath,
    ) -> Result<(), TransferError> {
        let meta = self
            .target
            .fs
            .stat(destination)
            .await
            .map_err(|e| failed("open", destination, e))?;
        if !meta.is_directory {
            return Err(TransferError::Failed(format!(
                "{} is not a folder",
                destination.display()
            )));
        }
        for source in sources {
            self.transfer(source, destination, None).await?;
        }
        Ok(())
    }

    fn same_fs(&self) -> bool {
        self.source.id == self.target.id
    }

    fn check_cancelled(&self) -> Result<(), TransferError> {
        if *self.cancel.borrow() {
            Err(TransferError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Metadata of a source entry. Links are recreated within one filesystem
    /// but followed (their target copied) across filesystems.
    async fn source_meta(&self, path: &VPath) -> Result<Meta, TransferError> {
        let meta = self
            .source
            .fs
            .lstat(path)
            .await
            .map_err(|e| failed("inspect", path, e))?;
        if meta.kind == EntryKind::Symlink && !self.same_fs() {
            return self
                .source
                .fs
                .stat(path)
                .await
                .map_err(|e| failed("follow the link", path, e));
        }
        Ok(meta)
    }

    /// Whether the transfer walks into this entry rather than copying it whole.
    fn descends(&self, meta: &Meta) -> bool {
        meta.kind == EntryKind::Directory
            || (meta.kind == EntryKind::Symlink && meta.is_directory && !self.same_fs())
    }

    async fn target_meta(&self, path: &VPath) -> Result<Option<Meta>, TransferError> {
        match self.target.fs.lstat(path).await {
            Ok(meta) => Ok(Some(meta)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failed("inspect", path, e)),
        }
    }

    fn measure<'a>(
        &'a mut self,
        path: &'a VPath,
    ) -> BoxFuture<'a, Result<WorkStats, TransferError>> {
        Box::pin(async move {
            self.check_cancelled()?;
            let meta = self.source_meta(path).await?;
            let mut stats = WorkStats {
                items: 1,
                bytes: meta.size.unwrap_or(0),
            };
            if self.descends(&meta) {
                let children = self
                    .source
                    .fs
                    .list(path)
                    .await
                    .map_err(|e| failed("read", path, e))?;
                for child in children {
                    stats += self.measure(&child.path).await?;
                }
                self.folder_stats.insert(path.clone(), stats);
            }
            Ok(stats)
        })
    }

    /// What a finished (or skipped) entry contributes to the progress counters.
    fn stats_of(&self, path: &VPath, meta: &Meta) -> WorkStats {
        self.folder_stats.get(path).copied().unwrap_or(WorkStats {
            items: 1,
            bytes: meta.size.unwrap_or(0),
        })
    }

    fn transfer<'a>(
        &'a mut self,
        source: &'a VPath,
        destination: &'a VPath,
        name: Option<VName>,
    ) -> BoxFuture<'a, Result<Outcome, TransferError>> {
        Box::pin(async move {
            self.check_cancelled()?;
            let name = match name.or_else(|| source.file_name()) {
                Some(name) => name,
                None => {
                    return Err(TransferError::Failed(format!(
                        "{} can't be copied or moved",
                        source.display()
                    )))
                }
            };
            self.counters.set_current(&name);
            let meta = self.source_meta(source).await?;
            if self.same_fs() && self.descends(&meta) && self.is_inside(destination, source).await {
                return Err(TransferError::Failed(format!(
                    "{} can't be copied or moved into itself",
                    source.display()
                )));
            }

            let Some((target, placement)) = self.place(source, &meta, destination, &name).await?
            else {
                self.counters.skipped.fetch_add(1, Ordering::Relaxed);
                self.counters.add(self.stats_of(source, &meta));
                return Ok(Outcome::Skipped);
            };

            if self.kind == TransferKind::Move && self.same_fs() && placement != Placement::Merge {
                let overwrite = placement == Placement::Overwrite;
                // Fails across volumes (and for some type mismatches); copying
                // and then deleting the source covers those.
                if self
                    .source
                    .fs
                    .rename(source, &target, overwrite)
                    .await
                    .is_ok()
                {
                    self.counters.add(self.stats_of(source, &meta));
                    return Ok(Outcome::Done);
                }
            }

            if self.descends(&meta) {
                return self.transfer_folder(source, &target, placement).await;
            }
            if meta.kind == EntryKind::Symlink
                && self.copy_link(source, &meta, &target, placement).await?
            {
                return self.finish_move(source).await;
            }
            self.copy_file(source, &meta, &target, placement == Placement::Overwrite)
                .await?;
            self.finish_move(source).await
        })
    }

    /// Decide where `source` lands, asking the resolver about clashes. `None`
    /// means skip it.
    async fn place(
        &mut self,
        source: &VPath,
        meta: &Meta,
        destination: &VPath,
        name: &VName,
    ) -> Result<Option<(VPath, Placement)>, TransferError> {
        let mut target = destination
            .join(name)
            .map_err(|e| TransferError::Failed(e.to_string()))?;
        loop {
            let Some(existing) = self.target_meta(&target).await? else {
                return Ok(Some((target, Placement::Fresh)));
            };
            if self.same_fs() && self.same_entry(source, &target).await {
                // Copying an item onto itself makes a "(copy)" beside it, as
                // file managers do; moving it onto itself is a no-op.
                if self.kind == TransferKind::Move {
                    return Ok(None);
                }
                target = self.unique_target(destination, name).await?;
                continue;
            }
            if self.descends(meta) && existing.is_real_directory() {
                return Ok(Some((target, Placement::Merge)));
            }
            match self.decide(name, source, meta, &target, &existing).await? {
                Decision::Overwrite => return Ok(Some((target, Placement::Overwrite))),
                Decision::Skip => return Ok(None),
                Decision::RenameTo(new_name) => {
                    target = destination
                        .join(&new_name)
                        .map_err(|e| TransferError::Failed(e.to_string()))?;
                }
            }
        }
    }

    async fn decide(
        &mut self,
        name: &VName,
        source: &VPath,
        meta: &Meta,
        target: &VPath,
        existing: &Meta,
    ) -> Result<Decision, TransferError> {
        let resolution = match &self.sticky {
            Some(resolution) => resolution.clone(),
            None => {
                let answer = self
                    .resolver
                    .resolve(Conflict {
                        name: name.display(),
                        source: ConflictSide::new(source, meta),
                        target: ConflictSide::new(target, existing),
                    })
                    .await;
                if answer.apply_to_all {
                    // A typed name can't apply to other files; keep both with
                    // automatic names instead.
                    self.sticky = Some(match &answer.resolution {
                        Resolution::Rename { .. } => Resolution::Rename { name: None },
                        other => other.clone(),
                    });
                }
                answer.resolution
            }
        };
        let overwrite_if = |condition: bool| {
            if condition {
                Decision::Overwrite
            } else {
                Decision::Skip
            }
        };
        Ok(match resolution {
            Resolution::Overwrite => Decision::Overwrite,
            Resolution::Skip => Decision::Skip,
            Resolution::Cancel => return Err(TransferError::Cancelled),
            Resolution::OverwriteIfNewer => overwrite_if(matches!(
                (meta.modified, existing.modified),
                (Some(source), Some(target)) if source > target
            )),
            Resolution::OverwriteIfDifferentSize => overwrite_if(meta.size != existing.size),
            Resolution::Rename { name: Some(typed) } => {
                let parent = target.parent().unwrap_or_else(|| target.clone());
                Decision::RenameTo(VName::for_path(&parent, &typed))
            }
            Resolution::Rename { name: None } => {
                let parent = target.parent().unwrap_or_else(|| target.clone());
                let unique = self.unique_target(&parent, name).await?;
                Decision::RenameTo(unique.file_name().unwrap_or_else(|| name.clone()))
            }
        })
    }

    async fn unique_target(&self, folder: &VPath, name: &VName) -> Result<VPath, TransferError> {
        for number in 1_u32.. {
            let candidate = folder
                .join(&name.with_copy_suffix(number))
                .map_err(|e| TransferError::Failed(e.to_string()))?;
            if self.target_meta(&candidate).await?.is_none() {
                return Ok(candidate);
            }
        }
        unreachable!("u32 names ran out")
    }

    async fn same_entry(&self, source: &VPath, target: &VPath) -> bool {
        if source == target {
            return true;
        }
        match (
            self.source.fs.canonicalize(source).await,
            self.target.fs.canonicalize(target).await,
        ) {
            (Ok(source), Ok(target)) => source == target,
            _ => false,
        }
    }

    /// Whether `destination` is `folder` or lies inside it (same filesystem).
    async fn is_inside(&self, destination: &VPath, folder: &VPath) -> bool {
        let canonical = |path: &VPath| {
            let fs = self.source.fs.clone();
            let path = path.clone();
            async move { fs.canonicalize(&path).await.unwrap_or(path) }
        };
        canonical(destination)
            .await
            .starts_with(&canonical(folder).await)
    }

    async fn transfer_folder(
        &mut self,
        source: &VPath,
        target: &VPath,
        placement: Placement,
    ) -> Result<Outcome, TransferError> {
        if placement == Placement::Overwrite {
            // A file (or link) is in the way of the folder.
            self.target
                .fs
                .remove(target, true)
                .await
                .map_err(|e| failed("replace", target, e))?;
        }
        if placement != Placement::Merge {
            self.target
                .fs
                .mkdir(target)
                .await
                .map_err(|e| failed("create", target, e))?;
        }
        self.counters.add(WorkStats { items: 1, bytes: 0 });
        let children = self
            .source
            .fs
            .list(source)
            .await
            .map_err(|e| failed("read", source, e))?;
        let mut complete = true;
        for child in children {
            if self.transfer(&child.path, target, Some(child.name)).await? == Outcome::Skipped {
                complete = false;
            }
        }
        if self.kind == TransferKind::Move {
            if !complete {
                // Some children stayed behind; so does their folder.
                return Ok(Outcome::Skipped);
            }
            // Not recursive: only an emptied folder can go, so nothing that
            // wasn't moved is ever deleted.
            self.source
                .fs
                .remove(source, false)
                .await
                .map_err(|e| failed("remove", source, e))?;
        }
        Ok(Outcome::Done)
    }

    /// Recreate a link (same filesystem only). Returns false when the link
    /// can't be created here (e.g. no symlink privilege on Windows) and it's a
    /// file link, so the caller copies its contents instead.
    async fn copy_link(
        &mut self,
        source: &VPath,
        meta: &Meta,
        target: &VPath,
        placement: Placement,
    ) -> Result<bool, TransferError> {
        let link = self
            .source
            .fs
            .read_link(source)
            .await
            .map_err(|e| failed("read the link", source, e))?;
        if placement == Placement::Overwrite {
            self.target
                .fs
                .remove(target, true)
                .await
                .map_err(|e| failed("replace", target, e))?;
        }
        match self
            .target
            .fs
            .symlink(&link, target, meta.is_directory)
            .await
        {
            Ok(()) => {
                self.counters.add(WorkStats { items: 1, bytes: 0 });
                Ok(true)
            }
            Err(_) if !meta.is_directory => Ok(false),
            Err(e) => Err(failed("create the link", target, e)),
        }
    }

    async fn copy_file(
        &mut self,
        source: &VPath,
        meta: &Meta,
        target: &VPath,
        overwrite: bool,
    ) -> Result<(), TransferError> {
        let folder = target.parent().unwrap_or_else(|| target.clone());
        let temporary_name = format!(".corepty-{}.part", Uuid::new_v4().simple());
        let temporary = folder
            .join(&VName::for_path(&folder, &temporary_name))
            .map_err(|e| TransferError::Failed(e.to_string()))?;

        let result = self.stream(source, &temporary).await;
        let result = match result {
            Ok(()) => {
                if let Some(modified) = meta.modified {
                    // Best effort: some servers refuse it, and the copy is fine anyway.
                    let _ = self
                        .target
                        .fs
                        .set_times(&temporary, modified, meta.accessed)
                        .await;
                }
                self.target
                    .fs
                    .rename(&temporary, target, overwrite)
                    .await
                    .map_err(|e| failed("finish", target, e))
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            let _ = self.target.fs.remove(&temporary, false).await;
            return Err(error);
        }

        // Permissions last: a read-only file can't have its times set.
        let target_posix = self.target.fs.capabilities().posix_permissions;
        let mode = match meta.mode {
            Some(mode) if target_posix => Some(mode),
            // Windows' read-only attribute, between local folders.
            None if meta.readonly && self.same_fs() => Some(0o444),
            _ => None,
        };
        if let Some(mode) = mode {
            let _ = self.target.fs.set_mode(target, mode).await;
        }
        self.counters.add(WorkStats { items: 1, bytes: 0 });
        Ok(())
    }

    async fn stream(&self, source: &VPath, temporary: &VPath) -> Result<(), TransferError> {
        let mut reader = self
            .source
            .fs
            .open_read(source, 0)
            .await
            .map_err(|e| failed("open", source, e))?;
        let mut writer = self
            .target
            .fs
            .open_write(temporary, WriteMode::CreateNew)
            .await
            .map_err(|e| failed("create", temporary, e))?;
        let mut buffer = vec![0_u8; CHUNK_SIZE];
        loop {
            self.check_cancelled()?;
            let read = reader
                .read(&mut buffer)
                .await
                .map_err(|e| failed("read", source, e))?;
            if read == 0 {
                break;
            }
            writer
                .write_all(&buffer[..read])
                .await
                .map_err(|e| failed("write", temporary, e))?;
            self.counters.add(WorkStats {
                items: 0,
                bytes: read as u64,
            });
        }
        writer
            .shutdown()
            .await
            .map_err(|e| failed("finish", temporary, e))
    }

    async fn finish_move(&self, source: &VPath) -> Result<Outcome, TransferError> {
        if self.kind == TransferKind::Move {
            self.source
                .fs
                .remove(source, false)
                .await
                .map_err(|e| failed("remove", source, e))?;
        }
        Ok(Outcome::Done)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::vfs::{local::LocalFs, memory::MemFs};
    use std::{collections::VecDeque, fs, path::Path};
    use tempfile::tempdir;

    /// Answers conflicts from a script and records what it was asked.
    #[derive(Default)]
    pub(crate) struct Scripted {
        answers: Mutex<VecDeque<ConflictAnswer>>,
        pub asked: Mutex<Vec<Conflict>>,
    }

    impl Scripted {
        pub(crate) fn new(answers: &[(Resolution, bool)]) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(
                    answers
                        .iter()
                        .map(|(resolution, apply_to_all)| ConflictAnswer {
                            resolution: resolution.clone(),
                            apply_to_all: *apply_to_all,
                        })
                        .collect(),
                ),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked_names(&self) -> Vec<String> {
            self.asked
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.name.clone())
                .collect()
        }
    }

    #[async_trait]
    impl ConflictResolver for Scripted {
        async fn resolve(&self, conflict: Conflict) -> ConflictAnswer {
            self.asked.lock().unwrap().push(conflict);
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("an unexpected conflict was raised")
        }
    }

    fn local() -> Side {
        Side {
            id: FsId::local(),
            fs: Arc::new(LocalFs),
        }
    }

    fn memory(fs: &Arc<MemFs>, id: &str) -> Side {
        Side {
            id: FsId(id.to_string()),
            fs: fs.clone(),
        }
    }

    fn native(path: &Path) -> VPath {
        VPath::native(path)
    }

    struct Run {
        result: Result<(), TransferError>,
        counters: Arc<Counters>,
    }

    async fn run_with(
        kind: TransferKind,
        source: Side,
        target: Side,
        sources: &[VPath],
        destination: &VPath,
        resolver: Arc<dyn ConflictResolver>,
        cancel: watch::Receiver<bool>,
    ) -> Run {
        let counters = Arc::new(Counters::default());
        let mut engine = Engine::new(kind, source, target, counters.clone(), cancel, resolver);
        let result = match engine.plan(sources).await {
            Ok(_) => engine.run(sources, destination).await,
            Err(error) => Err(error),
        };
        Run { result, counters }
    }

    async fn run(
        kind: TransferKind,
        source: Side,
        target: Side,
        sources: &[VPath],
        destination: &VPath,
        resolver: Arc<dyn ConflictResolver>,
    ) -> Run {
        let (_cancel, receiver) = watch::channel(false);
        run_with(
            kind,
            source,
            target,
            sources,
            destination,
            resolver,
            receiver,
        )
        .await
    }

    fn no_conflicts() -> Arc<Scripted> {
        Scripted::new(&[])
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("corepty"))
            .collect()
    }

    #[tokio::test]
    async fn copies_a_file_with_its_contents_and_timestamp() {
        let (from, to) = (tempdir().unwrap(), tempdir().unwrap());
        let source = from.path().join("report.txt");
        fs::write(&source, vec![7_u8; CHUNK_SIZE * 2 + 5]).unwrap();
        let stamp =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(stamp)
            .unwrap();

        let run = run(
            TransferKind::Copy,
            local(),
            local(),
            &[native(&source)],
            &native(to.path()),
            no_conflicts(),
        )
        .await;
        run.result.unwrap();
        let copied = to.path().join("report.txt");
        assert_eq!(fs::read(&copied).unwrap(), fs::read(&source).unwrap());
        assert_eq!(fs::metadata(&copied).unwrap().modified().unwrap(), stamp);
        assert!(source.exists());
        assert!(leftovers(to.path()).is_empty());
        assert_eq!(
            run.counters.snapshot().done.bytes,
            (CHUNK_SIZE * 2 + 5) as u64
        );
    }

    #[tokio::test]
    async fn copies_a_tree_across_filesystems_and_back() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("src").join("main.rs"), b"fn main() {}").unwrap();
        fs::write(project.join("README"), b"hi").unwrap();
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/srv");

        let up = run(
            TransferKind::Copy,
            local(),
            memory(&remote, "ssh-1"),
            &[native(&project)],
            &MemFs::path("/srv"),
            no_conflicts(),
        )
        .await;
        up.result.unwrap();
        assert_eq!(
            remote.read("/srv/project/src/main.rs").unwrap(),
            b"fn main() {}"
        );
        assert_eq!(remote.read("/srv/project/README").unwrap(), b"hi");
        assert_eq!(up.counters.snapshot().done.items, 4);

        let back = tempdir().unwrap();
        let down = run(
            TransferKind::Copy,
            memory(&remote, "ssh-1"),
            local(),
            &[MemFs::path("/srv/project")],
            &native(back.path()),
            no_conflicts(),
        )
        .await;
        down.result.unwrap();
        assert_eq!(
            fs::read(back.path().join("project/src/main.rs")).unwrap(),
            b"fn main() {}"
        );
    }

    #[tokio::test]
    async fn moving_a_folder_onto_an_existing_one_merges_without_copying_data() {
        // B5: a same-filesystem move is renames only, even when merging.
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/a");
        fs.add_file("/a/x.txt", b"x");
        fs.add_dir("/a/sub");
        fs.add_file("/a/sub/y.txt", b"y");
        fs.add_dir("/dest");
        fs.add_dir("/dest/a");
        fs.add_file("/dest/a/z.txt", b"z");

        let run = run(
            TransferKind::Move,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &[MemFs::path("/a")],
            &MemFs::path("/dest"),
            no_conflicts(),
        )
        .await;
        run.result.unwrap();
        assert_eq!(
            fs.reads(),
            0,
            "a same-filesystem move must not read file data"
        );
        assert_eq!(
            fs.paths(),
            [
                "/",
                "/dest",
                "/dest/a",
                "/dest/a/sub",
                "/dest/a/sub/y.txt",
                "/dest/a/x.txt",
                "/dest/a/z.txt"
            ]
        );
        assert_eq!(run.counters.snapshot().done.items, 4);
    }

    #[tokio::test]
    async fn moving_a_local_folder_onto_an_existing_one_merges_it() {
        let temp = tempdir().unwrap();
        let (source, dest) = (temp.path().join("photos"), temp.path().join("backup"));
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("new.jpg"), b"new").unwrap();
        fs::create_dir_all(dest.join("photos")).unwrap();
        fs::write(dest.join("photos").join("old.jpg"), b"old").unwrap();

        let run = run(
            TransferKind::Move,
            local(),
            local(),
            &[native(&source)],
            &native(&dest),
            no_conflicts(),
        )
        .await;
        run.result.unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(dest.join("photos/new.jpg")).unwrap(), b"new");
        assert_eq!(fs::read(dest.join("photos/old.jpg")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn nested_conflicts_in_a_merged_folder_are_each_asked() {
        // B6: nothing inside a merged folder is overwritten silently.
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/src/docs");
        fs.add_file("/src/docs/a.txt", b"new a");
        fs.add_file("/src/docs/b.txt", b"new b");
        fs.add_dir("/dst");
        fs.add_dir("/dst/docs");
        fs.add_file("/dst/docs/a.txt", b"old a");
        fs.add_file("/dst/docs/b.txt", b"old b");
        let resolver = Scripted::new(&[(Resolution::Overwrite, false), (Resolution::Skip, false)]);

        let run = run(
            TransferKind::Copy,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &[MemFs::path("/src/docs")],
            &MemFs::path("/dst"),
            resolver.clone(),
        )
        .await;
        run.result.unwrap();
        assert_eq!(resolver.asked_names(), ["a.txt", "b.txt"]);
        assert_eq!(fs.read("/dst/docs/a.txt").unwrap(), b"new a");
        assert_eq!(fs.read("/dst/docs/b.txt").unwrap(), b"old b");
        assert_eq!(run.counters.snapshot().skipped, 1);
    }

    #[tokio::test]
    async fn apply_to_all_asks_once() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        for name in ["1", "2", "3"] {
            fs.add_file(&format!("/src/{name}"), b"new");
            fs.add_file(&format!("/dst/{name}"), b"old");
        }
        let resolver = Scripted::new(&[(Resolution::Rename { name: None }, true)]);
        let sources: Vec<VPath> = ["1", "2", "3"]
            .iter()
            .map(|n| MemFs::path(&format!("/src/{n}")))
            .collect();

        run(
            TransferKind::Copy,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &sources,
            &MemFs::path("/dst"),
            resolver.clone(),
        )
        .await
        .result
        .unwrap();
        assert_eq!(resolver.asked_names().len(), 1);
        for name in ["1", "2", "3"] {
            assert_eq!(fs.read(&format!("/dst/{name}")).unwrap(), b"old");
            assert_eq!(fs.read(&format!("/dst/{name} (copy)")).unwrap(), b"new");
        }
    }

    #[tokio::test]
    async fn rename_to_a_typed_name_and_conditional_overwrites() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/typed.txt", b"new");
        fs.add_file("/dst/typed.txt", b"old");
        fs.add_file("/dst/same-size.txt", b"old");
        fs.add_file("/src/same-size.txt", b"new"); // newer and same length
        let resolver = Scripted::new(&[
            (
                Resolution::Rename {
                    name: Some("chosen.txt".into()),
                },
                false,
            ),
            (Resolution::OverwriteIfDifferentSize, false),
        ]);
        let sources = [
            MemFs::path("/src/typed.txt"),
            MemFs::path("/src/same-size.txt"),
        ];

        run(
            TransferKind::Copy,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &sources,
            &MemFs::path("/dst"),
            resolver,
        )
        .await
        .result
        .unwrap();
        assert_eq!(fs.read("/dst/chosen.txt").unwrap(), b"new");
        assert_eq!(fs.read("/dst/typed.txt").unwrap(), b"old");
        assert_eq!(fs.read("/dst/same-size.txt").unwrap(), b"old");

        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        fs.set_times(&MemFs::path("/dst/same-size.txt"), old, None)
            .await
            .unwrap();
        let newer = Scripted::new(&[(Resolution::OverwriteIfNewer, false)]);
        run(
            TransferKind::Copy,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &[MemFs::path("/src/same-size.txt")],
            &MemFs::path("/dst"),
            newer,
        )
        .await
        .result
        .unwrap();
        assert_eq!(fs.read("/dst/same-size.txt").unwrap(), b"new");
    }

    #[tokio::test]
    async fn cancel_answer_stops_the_job() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/a", b"new");
        fs.add_file("/dst/a", b"old");
        let resolver = Scripted::new(&[(Resolution::Cancel, false)]);
        let run = run(
            TransferKind::Copy,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &[MemFs::path("/src/a")],
            &MemFs::path("/dst"),
            resolver,
        )
        .await;
        assert_eq!(run.result, Err(TransferError::Cancelled));
        assert_eq!(fs.read("/dst/a").unwrap(), b"old");
    }

    #[tokio::test]
    async fn cancelling_mid_file_leaves_no_partial_target() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/srv");
        remote.add_file("/srv/big.bin", &vec![1_u8; CHUNK_SIZE * 4]);
        let (cancel, receiver) = watch::channel(false);
        let cancel = Arc::new(cancel);
        let trigger = cancel.clone();
        remote.on_first_read(move || {
            let _ = trigger.send(true);
        });
        let to = tempdir().unwrap();

        let run = run_with(
            TransferKind::Copy,
            memory(&remote, "ssh-1"),
            local(),
            &[MemFs::path("/srv/big.bin")],
            &native(to.path()),
            no_conflicts(),
            receiver,
        )
        .await;
        assert_eq!(run.result, Err(TransferError::Cancelled));
        assert_eq!(
            fs::read_dir(to.path()).unwrap().count(),
            0,
            "no target and no temporary file"
        );
    }

    #[tokio::test]
    async fn copying_onto_itself_keeps_both_and_moving_onto_itself_is_a_no_op() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("notes.txt");
        fs::write(&file, b"notes").unwrap();

        run(
            TransferKind::Copy,
            local(),
            local(),
            &[native(&file)],
            &native(temp.path()),
            no_conflicts(),
        )
        .await
        .result
        .unwrap();
        assert_eq!(
            fs::read(temp.path().join("notes (copy).txt")).unwrap(),
            b"notes"
        );

        let moved = run(
            TransferKind::Move,
            local(),
            local(),
            &[native(&file)],
            &native(temp.path()),
            no_conflicts(),
        )
        .await;
        moved.result.unwrap();
        assert_eq!(moved.counters.snapshot().skipped, 1);
        assert_eq!(fs::read(&file).unwrap(), b"notes");
    }

    #[tokio::test]
    async fn a_folder_cannot_go_into_itself() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir_all(folder.join("inner")).unwrap();
        let run = run(
            TransferKind::Copy,
            local(),
            local(),
            &[native(&folder)],
            &native(&folder.join("inner")),
            no_conflicts(),
        )
        .await;
        assert!(
            matches!(run.result, Err(TransferError::Failed(message)) if message.contains("into itself"))
        );
    }

    #[tokio::test]
    async fn a_cross_filesystem_move_keeps_a_folder_whose_children_were_skipped() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("site");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("index.html"), b"new").unwrap();
        fs::write(folder.join("style.css"), b"css").unwrap();
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/www");
        remote.add_dir("/www/site");
        remote.add_file("/www/site/index.html", b"old");
        let resolver = Scripted::new(&[(Resolution::Skip, false)]);

        let run = run(
            TransferKind::Move,
            local(),
            memory(&remote, "ssh-1"),
            &[native(&folder)],
            &MemFs::path("/www"),
            resolver,
        )
        .await;
        run.result.unwrap();
        assert_eq!(remote.read("/www/site/style.css").unwrap(), b"css");
        assert_eq!(remote.read("/www/site/index.html").unwrap(), b"old");
        assert!(
            !folder.join("style.css").exists(),
            "moved files leave the source"
        );
        assert_eq!(
            fs::read(folder.join("index.html")).unwrap(),
            b"new",
            "skipped files stay"
        );
    }

    #[tokio::test]
    async fn replacing_a_read_only_file_keeps_it_read_only() {
        let (from, to) = (tempdir().unwrap(), tempdir().unwrap());
        let source = from.path().join("locked.cfg");
        fs::write(&source, b"new").unwrap();
        let mut permissions = fs::metadata(&source).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&source, permissions).unwrap();
        fs::write(to.path().join("locked.cfg"), b"old").unwrap();
        let resolver = Scripted::new(&[(Resolution::Overwrite, false)]);

        run(
            TransferKind::Copy,
            local(),
            local(),
            &[native(&source)],
            &native(to.path()),
            resolver,
        )
        .await
        .result
        .unwrap();
        let copied = to.path().join("locked.cfg");
        assert_eq!(fs::read(&copied).unwrap(), b"new");
        assert!(fs::metadata(&copied).unwrap().permissions().readonly());
        // Let tempdir clean up on Windows.
        for path in [&source, &copied] {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions).unwrap();
        }
    }
}

//! Copies and moves between any two filesystems.
//!
//! A job is planned first (items and bytes, for the progress bar), then run
//! item by item. File contents stream into a temporary file next to the target
//! and are renamed into place, so a target is never left half-written. Moves
//! within one filesystem are renames: no file data is read, even when a folder
//! is merged into an existing one. Every name clash, including those deep
//! inside a merged folder, goes to the [`ConflictResolver`].

use crate::vfs::{epoch_millis, Entry, EntryKind, FsId, Meta, VName, VPath, Vfs, WriteMode};
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
    /// The target is a partial copy left by an earlier, interrupted transfer
    /// (`<name>.filepart`): the choices are Resume, start over, or skip.
    pub partial: bool,
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
    /// Continue a partial copy from where it stopped.
    Resume,
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

/// How one source entry is handled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Handling {
    /// A real folder: walk into it.
    Folder,
    /// A link to a folder, followed: walk into its target, copying only.
    /// `trail` extends the link-loop guard for the walk below it.
    FollowedFolder { trail: Vec<VPath> },
    /// Recreate the link itself (same filesystem).
    Link,
    /// Copy the contents (files, and file links being followed).
    File,
    /// Leave it out: a broken link, a link loop, an unreadable entry, or a
    /// device / pipe / socket (reading a pipe would hang the transfer).
    Skip,
}

impl Handling {
    fn walks(&self) -> bool {
        matches!(self, Self::Folder | Self::FollowedFolder { .. })
    }
}

/// State carried down the folder walk.
#[derive(Debug, Clone, Default)]
struct Walk {
    /// Below a followed link: copy only. Moving or deleting there would take
    /// the files out of the link's target, which the user never selected.
    copy_only: bool,
    /// Canonical folders where links were followed on the way down. A link
    /// whose target contains one of them (or the folder holding the link)
    /// leads back into the walk: a loop.
    trail: Vec<VPath>,
    /// 0 for the items the user picked.
    depth: usize,
}

impl Walk {
    fn child(&self) -> Self {
        Self {
            copy_only: self.copy_only,
            trail: self.trail.clone(),
            depth: self.depth + 1,
        }
    }

    fn followed(&self, trail: Vec<VPath>) -> Self {
        Self {
            copy_only: true,
            trail,
            depth: self.depth + 1,
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct Engine {
    kind: TransferKind,
    source: Side,
    target: Side,
    counters: Arc<Counters>,
    cancel: watch::Receiver<bool>,
    resolver: Arc<dyn ConflictResolver>,
    /// An "apply to all" answer, reused for later conflicts of the same kind.
    sticky: Option<Resolution>,
    /// The same for partial copies (resume / start over / skip).
    sticky_partial: Option<Resolution>,
    /// The last partial-copy answer was "skip".
    partial_skipped: bool,
    /// Give copies the source's permissions (POSIX mode bits).
    preserve_mode: bool,
    /// Planned totals of every walked folder, for counting renamed subtrees.
    folder_stats: HashMap<VPath, WorkStats>,
    /// Copy buffer, allocated once per job.
    buffer: Vec<u8>,
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
            sticky_partial: None,
            partial_skipped: false,
            preserve_mode: true,
            folder_stats: HashMap::new(),
            buffer: Vec::new(),
        }
    }

    /// Count everything the job will touch.
    /// Whether copies get the source's permissions (on by default).
    pub fn set_preserve_mode(&mut self, preserve: bool) {
        self.preserve_mode = preserve;
    }

    pub async fn plan(&mut self, sources: &[VPath]) -> Result<WorkStats, TransferError> {
        let mut total = WorkStats::default();
        for source in sources {
            total += self.measure(source, None, Walk::default()).await?;
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
            self.transfer(source, destination, None, None, Walk::default())
                .await?;
        }
        Ok(())
    }

    fn same_fs(&self) -> bool {
        self.source.id == self.target.id
    }

    /// Whether this level of the walk moves (rather than copies) its items.
    fn moves(&self, walk: &Walk) -> bool {
        self.kind == TransferKind::Move && !walk.copy_only
    }

    fn check_cancelled(&self) -> Result<(), TransferError> {
        if *self.cancel.borrow() {
            Err(TransferError::Cancelled)
        } else {
            Ok(())
        }
    }

    async fn lstat(&self, path: &VPath) -> Result<Meta, TransferError> {
        self.source
            .fs
            .lstat(path)
            .await
            .map_err(|e| failed("inspect", path, e))
    }

    /// Decide how to handle an entry, given its own (not followed) metadata.
    /// Links are recreated within one filesystem and followed across them.
    async fn classify(
        &self,
        path: &VPath,
        meta: Meta,
        walk: &Walk,
    ) -> Result<(Handling, Meta), TransferError> {
        Ok(match meta.kind {
            EntryKind::Directory => (Handling::Folder, meta),
            EntryKind::File => (Handling::File, meta),
            EntryKind::Other => (Handling::Skip, meta),
            EntryKind::Symlink if self.same_fs() => (Handling::Link, meta),
            EntryKind::Symlink => self.follow(path, meta, walk).await?,
        })
    }

    /// Follow a link: its target's metadata, or Skip for a broken link or a
    /// link back into the walk.
    async fn follow(
        &self,
        path: &VPath,
        link_meta: Meta,
        walk: &Walk,
    ) -> Result<(Handling, Meta), TransferError> {
        let meta = match self.source.fs.stat(path).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok((Handling::Skip, link_meta))
            }
            Err(e) => return Err(failed("follow the link", path, e)),
        };
        if !meta.is_directory {
            return Ok((Handling::File, meta));
        }
        let canonical = |path: VPath| {
            let fs = self.source.fs.clone();
            async move { fs.canonicalize(&path).await }
        };
        let target = canonical(path.clone())
            .await
            .map_err(|e| failed("follow the link", path, e))?;
        let here = match path.parent() {
            Some(parent) => canonical(parent).await.ok(),
            None => None,
        };
        let mut trail = walk.trail.clone();
        trail.extend(here);
        if trail.iter().any(|folder| folder.starts_with(&target)) {
            return Ok((Handling::Skip, meta));
        }
        Ok((Handling::FollowedFolder { trail }, meta))
    }

    async fn target_meta(&self, path: &VPath) -> Result<Option<Meta>, TransferError> {
        match self.target.fs.lstat(path).await {
            Ok(meta) => Ok(Some(meta)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failed("inspect", path, e)),
        }
    }

    /// Plan one entry. `known` is metadata the folder listing already returned.
    fn measure<'a>(
        &'a mut self,
        path: &'a VPath,
        known: Option<Meta>,
        walk: Walk,
    ) -> BoxFuture<'a, Result<WorkStats, TransferError>> {
        Box::pin(async move {
            self.check_cancelled()?;
            let meta = match known {
                Some(meta) => meta,
                None => self.lstat(path).await?,
            };
            let (handling, meta) = self.classify(path, meta, &walk).await?;
            let mut stats = WorkStats {
                items: 1,
                bytes: if handling == Handling::File {
                    meta.size.unwrap_or(0)
                } else {
                    0
                },
            };
            let below = match handling {
                Handling::Folder => walk.child(),
                Handling::FollowedFolder { trail } => walk.followed(trail),
                _ => return Ok(stats),
            };
            let children = self
                .source
                .fs
                .list(path)
                .await
                .map_err(|e| failed("read", path, e))?;
            for child in children {
                stats += self
                    .measure(&child.path, Some(listed_meta(&child)), below.clone())
                    .await?;
            }
            self.folder_stats.insert(path.clone(), stats);
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

    fn skip(&self, path: &VPath, meta: &Meta) -> Outcome {
        self.counters.skipped.fetch_add(1, Ordering::Relaxed);
        self.counters.add(self.stats_of(path, meta));
        Outcome::Skipped
    }

    fn transfer<'a>(
        &'a mut self,
        source: &'a VPath,
        destination: &'a VPath,
        name: Option<VName>,
        known: Option<Meta>,
        walk: Walk,
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
            let meta = match known {
                Some(meta) => meta,
                None => self.lstat(source).await?,
            };
            let (mut handling, mut meta) = self.classify(source, meta, &walk).await?;
            if handling == Handling::Skip {
                return Ok(self.skip(source, &meta));
            }
            // Only the picked items can contain the destination: below them,
            // the walk never leaves the folder being transferred.
            if walk.depth == 0
                && self.same_fs()
                && handling.walks()
                && self.is_inside(destination, source).await
            {
                return Err(TransferError::Failed(format!(
                    "{} can't be copied or moved into itself",
                    source.display()
                )));
            }

            let Some((target, placement)) = self
                .place(source, &meta, handling.walks(), destination, &name)
                .await?
            else {
                return Ok(self.skip(source, &meta));
            };

            if self.moves(&walk)
                && self.same_fs()
                && placement != Placement::Merge
                && !matches!(handling, Handling::FollowedFolder { .. })
            {
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

            if handling == Handling::Link {
                if self.copy_link(source, &target, &meta, placement).await? {
                    return self.finish_move(source, &walk).await;
                }
                // This filesystem can't create the link (e.g. no symlink
                // privilege on Windows): copy what it points to instead, as
                // Explorer does for junctions.
                (handling, meta) = self.follow(source, meta, &walk).await?;
                if handling == Handling::Skip {
                    return Ok(self.skip(source, &meta));
                }
            }

            match handling {
                Handling::Folder => {
                    let below = walk.child();
                    self.transfer_folder(
                        source,
                        &meta,
                        &target,
                        placement,
                        below,
                        self.moves(&walk),
                    )
                    .await
                }
                Handling::FollowedFolder { trail } => {
                    // The contents are only copied; a move then removes just
                    // the link (transfer_folder's non-recursive remove).
                    let below = walk.followed(trail);
                    self.transfer_folder(
                        source,
                        &meta,
                        &target,
                        placement,
                        below,
                        self.moves(&walk),
                    )
                    .await
                }
                Handling::File => {
                    let overwrite = placement == Placement::Overwrite;
                    let copied = self
                        .copy_file(&name, source, &meta, &target, overwrite)
                        .await?;
                    if copied == Outcome::Skipped {
                        return Ok(self.skip(source, &meta));
                    }
                    self.finish_move(source, &walk).await
                }
                Handling::Link | Handling::Skip => unreachable!("handled above"),
            }
        })
    }

    /// Decide where `source` lands, asking the resolver about clashes. `None`
    /// means skip it.
    async fn place(
        &mut self,
        source: &VPath,
        meta: &Meta,
        walks: bool,
        destination: &VPath,
        name: &VName,
    ) -> Result<Option<(VPath, Placement)>, TransferError> {
        // A remote name this machine can't hold (`a:b`, `CON` on Windows)
        // lands under a safe spelling instead of failing the job.
        let storable = match destination {
            VPath::Native(_) => name.storable_here(),
            VPath::Posix(_) => None,
        };
        let name = storable.as_ref().unwrap_or(name);
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
            if walks && existing.is_real_directory() {
                return Ok(Some((target, Placement::Merge)));
            }
            match self.decide(name, source, meta, &target, &existing).await? {
                Decision::Overwrite => return Ok(Some((target, Placement::Overwrite))),
                Decision::Skip => return Ok(None),
                // A typed name that isn't valid here: ask again.
                Decision::RenameTo(new_name) => {
                    if let Ok(renamed) = destination.join(&new_name) {
                        target = renamed;
                    }
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
        // "Apply to all" covers clashes of the same kind only: an answer given
        // for files must not silently replace a whole folder.
        let same_kind = meta.is_directory == existing.is_directory;
        let resolution = match &self.sticky {
            Some(resolution) if same_kind => resolution.clone(),
            _ => {
                let answer = self
                    .resolver
                    .resolve(Conflict {
                        name: name.display(),
                        source: ConflictSide::new(source, meta),
                        target: ConflictSide::new(target, existing),
                        partial: false,
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
            // Resuming applies to partial copies only; for a name clash it
            // means replacing what's there.
            Resolution::Overwrite | Resolution::Resume => Decision::Overwrite,
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

    /// Copy (or move) a folder's contents into `target`. With `remove_source`,
    /// the emptied source folder (or the followed link) is removed afterwards.
    async fn transfer_folder(
        &mut self,
        source: &VPath,
        meta: &Meta,
        target: &VPath,
        placement: Placement,
        below: Walk,
        remove_source: bool,
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
            let meta = listed_meta(&child);
            let outcome = self
                .transfer(
                    &child.path,
                    target,
                    Some(child.name),
                    Some(meta),
                    below.clone(),
                )
                .await?;
            if outcome == Outcome::Skipped {
                complete = false;
            }
        }
        if placement != Placement::Merge {
            // A folder we created gets its times (and mode) last: writing
            // its contents changed them.
            if let Some(modified) = meta.modified {
                let _ = self
                    .target
                    .fs
                    .set_times(target, modified, meta.accessed)
                    .await;
            }
            if let Some(mode) = self.mode_to_keep(meta) {
                let _ = self.target.fs.set_mode(target, mode).await;
            }
        }
        if remove_source {
            if !complete {
                // Some children stayed behind; so does their folder.
                return Ok(Outcome::Skipped);
            }
            // Not recursive: only an emptied folder (or the link itself) can
            // go, so nothing that wasn't moved is ever deleted.
            self.source
                .fs
                .remove(source, false)
                .await
                .map_err(|e| failed("remove", source, e))?;
        }
        Ok(Outcome::Done)
    }

    /// Recreate a link (same filesystem only), under a temporary name first so
    /// whatever is at the target survives a failure. Returns false when links
    /// can't be created here at all; the caller then copies the contents.
    async fn copy_link(
        &mut self,
        source: &VPath,
        target: &VPath,
        meta: &Meta,
        placement: Placement,
    ) -> Result<bool, TransferError> {
        let link = self
            .source
            .fs
            .read_link(source)
            .await
            .map_err(|e| failed("read the link", source, e))?;
        let temporary = temporary_beside(target, "link")?;
        if self
            .target
            .fs
            .symlink(&link, &temporary, meta.is_directory)
            .await
            .is_err()
        {
            return Ok(false);
        }
        if let Err(e) = self
            .target
            .fs
            .rename(&temporary, target, placement == Placement::Overwrite)
            .await
        {
            let _ = self.target.fs.remove(&temporary, false).await;
            return Err(failed("finish", target, e));
        }
        self.counters.add(WorkStats { items: 1, bytes: 0 });
        Ok(true)
    }

    /// Where a file is written before it takes the target's name, and from
    /// which offset. Big files go to `<name>.filepart` (WinSCP's convention),
    /// so a copy cut short by a dropped connection can resume; small ones to
    /// a hidden temporary. `None`: skip the file.
    async fn staging(
        &mut self,
        name: &VName,
        source: &VPath,
        meta: &Meta,
        target: &VPath,
    ) -> Result<Option<(VPath, u64)>, TransferError> {
        let size = meta.size.unwrap_or(0);
        if size < RESUMABLE_SIZE {
            return Ok(Some((temporary_beside(target, "part")?, 0)));
        }
        let partial = partial_beside(target)?;
        let Some(existing) = self.target_meta(&partial).await? else {
            return Ok(Some((partial, 0)));
        };
        if existing.kind != EntryKind::File {
            // Not ours to touch (a folder by that name, say).
            return Ok(Some((temporary_beside(target, "part")?, 0)));
        }
        let have = existing.size.unwrap_or(0);
        let resumable = have > 0 && have < size;
        let resume = resumable
            && self
                .ask_resume(name, source, meta, &partial, &existing)
                .await?;
        if resumable && !resume && self.partial_skipped {
            return Ok(None);
        }
        if resume {
            return Ok(Some((partial, have)));
        }
        // Start over: an empty or complete leftover, or the user's choice.
        self.target
            .fs
            .remove(&partial, false)
            .await
            .map_err(|e| failed("replace", &partial, e))?;
        Ok(Some((partial, 0)))
    }

    /// Resume the partial copy at `partial`? Sets `partial_skipped` when the
    /// answer was to skip the file altogether.
    async fn ask_resume(
        &mut self,
        name: &VName,
        source: &VPath,
        meta: &Meta,
        partial: &VPath,
        existing: &Meta,
    ) -> Result<bool, TransferError> {
        self.partial_skipped = false;
        let resolution = match &self.sticky_partial {
            Some(resolution) => resolution.clone(),
            None => {
                let answer = self
                    .resolver
                    .resolve(Conflict {
                        name: name.display(),
                        source: ConflictSide::new(source, meta),
                        target: ConflictSide::new(partial, existing),
                        partial: true,
                    })
                    .await;
                if answer.apply_to_all {
                    self.sticky_partial = Some(answer.resolution.clone());
                }
                answer.resolution
            }
        };
        match resolution {
            Resolution::Resume => Ok(true),
            Resolution::Skip => {
                self.partial_skipped = true;
                Ok(false)
            }
            Resolution::Cancel => Err(TransferError::Cancelled),
            // Anything else starts the file over.
            _ => Ok(false),
        }
    }

    /// Copy one file; `Skipped` when the user chose to leave it.
    async fn copy_file(
        &mut self,
        name: &VName,
        source: &VPath,
        meta: &Meta,
        target: &VPath,
        overwrite: bool,
    ) -> Result<Outcome, TransferError> {
        let Some((temporary, offset)) = self.staging(name, source, meta, target).await? else {
            return Ok(Outcome::Skipped);
        };
        let keeps_partial = offset > 0 || is_partial(&temporary);
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.resize(CHUNK_SIZE, 0);
        let streamed = self.stream(source, &temporary, offset, &mut buffer).await;
        self.buffer = buffer;
        if let Err(error) = streamed {
            // A copy cut short keeps its .filepart to resume; a cancelled one
            // (and any hidden temporary) is cleaned up.
            if !keeps_partial || error == TransferError::Cancelled {
                let _ = self.target.fs.remove(&temporary, false).await;
            }
            return Err(error);
        }
        if let Some(modified) = meta.modified {
            // Best effort: some servers refuse it, and the copy is fine anyway.
            let _ = self
                .target
                .fs
                .set_times(&temporary, modified, meta.accessed)
                .await;
        }
        if let Err(e) = self.target.fs.rename(&temporary, target, overwrite).await {
            let _ = self.target.fs.remove(&temporary, false).await;
            return Err(failed("finish", target, e));
        }

        // Permissions last: a read-only file can't have its times set.
        if let Some(mode) = self.mode_to_keep(meta) {
            let _ = self.target.fs.set_mode(target, mode).await;
        }
        self.counters.add(WorkStats { items: 1, bytes: 0 });
        Ok(Outcome::Done)
    }

    /// The permissions to give a copy of an entry with `meta`, if any.
    fn mode_to_keep(&self, meta: &Meta) -> Option<u32> {
        if !self.preserve_mode {
            return None;
        }
        let target_posix = self.target.fs.capabilities().posix_permissions;
        match meta.mode {
            Some(mode) if target_posix => Some(mode),
            // Windows' read-only attribute, between local folders.
            None if meta.readonly && self.same_fs() && !meta.is_directory => Some(0o444),
            _ => None,
        }
    }

    /// Copy `source` into `temporary`, from `offset` on (resuming: the bytes
    /// before it are already there).
    async fn stream(
        &self,
        source: &VPath,
        temporary: &VPath,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), TransferError> {
        let mut reader = self
            .source
            .fs
            .open_read(source, offset)
            .await
            .map_err(|e| failed("open", source, e))?;
        let mode = if offset > 0 {
            WriteMode::Append
        } else {
            WriteMode::CreateNew
        };
        let mut writer = self
            .target
            .fs
            .open_write(temporary, mode)
            .await
            .map_err(|e| failed("create", temporary, e))?;
        if offset > 0 {
            self.counters.add(WorkStats {
                items: 0,
                bytes: offset,
            });
        }
        loop {
            self.check_cancelled()?;
            let read = match reader.read(buffer).await {
                Ok(read) => read,
                Err(e) => {
                    // The source went away (a dropped connection): keep
                    // everything that arrived, which is where a resume
                    // continues. (Writes land in order, so it's a prefix.)
                    let _ = writer.shutdown().await;
                    return Err(failed("read", source, e));
                }
            };
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

    async fn finish_move(&self, source: &VPath, walk: &Walk) -> Result<Outcome, TransferError> {
        if self.moves(walk) {
            self.source
                .fs
                .remove(source, false)
                .await
                .map_err(|e| failed("remove", source, e))?;
        }
        Ok(Outcome::Done)
    }
}

/// A listing entry's own metadata; entries that couldn't be inspected are
/// skipped (their kind is Other).
fn listed_meta(entry: &Entry) -> Meta {
    entry.meta.clone()
}

/// Files from this size on are copied via `<name>.filepart` (resumable).
const RESUMABLE_SIZE: u64 = 1024 * 1024;
const PARTIAL_SUFFIX: &str = ".filepart";

/// `<name>.filepart` beside the target.
fn partial_beside(target: &VPath) -> Result<VPath, TransferError> {
    let folder = target.parent().unwrap_or_else(|| target.clone());
    let name = target
        .file_name()
        .ok_or_else(|| TransferError::Failed(format!("{} has no name", target.display())))?;
    folder
        .join(&name.with_suffix(PARTIAL_SUFFIX))
        .map_err(|e| TransferError::Failed(e.to_string()))
}

fn is_partial(path: &VPath) -> bool {
    path.file_name()
        .is_some_and(|name| name.display().ends_with(PARTIAL_SUFFIX))
}

/// `.corepty-<uuid>.<what>` in the target's folder (same volume, so the final
/// rename is a single step).
fn temporary_beside(target: &VPath, what: &str) -> Result<VPath, TransferError> {
    let folder = target.parent().unwrap_or_else(|| target.clone());
    let name = format!(".corepty-{}.{what}", Uuid::new_v4().simple());
    folder
        .join(&VName::for_path(&folder, &name))
        .map_err(|e| TransferError::Failed(e.to_string()))
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

    pub(crate) fn local() -> Side {
        Side {
            id: FsId::local(),
            fs: Arc::new(LocalFs),
        }
    }

    pub(crate) fn memory(fs: &Arc<MemFs>, id: &str) -> Side {
        Side {
            id: FsId(id.to_string()),
            fs: fs.clone(),
        }
    }

    fn native(path: &Path) -> VPath {
        VPath::native(path)
    }

    pub(crate) struct Run {
        pub result: Result<(), TransferError>,
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

    pub(crate) async fn run(
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

    pub(crate) fn no_conflicts() -> Arc<Scripted> {
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

    /// A link to a folder: a junction on Windows (no privilege needed), a
    /// symlink elsewhere.
    fn dir_link(link: &Path, target: &Path) {
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(made.status.success(), "mklink /J failed");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[tokio::test]
    async fn moving_a_folder_link_across_filesystems_keeps_the_linked_files() {
        // Review finding: the move used to delete the files *inside* the
        // link's target while "moving" them through the link.
        let temp = tempdir().unwrap();
        let important = temp.path().join("important");
        fs::create_dir(&important).unwrap();
        fs::write(important.join("keep.txt"), b"keep").unwrap();
        let link = temp.path().join("link");
        dir_link(&link, &important);
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/srv");

        let run = run(
            TransferKind::Move,
            local(),
            memory(&remote, "ssh-1"),
            &[native(&link)],
            &MemFs::path("/srv"),
            no_conflicts(),
        )
        .await;
        run.result.unwrap();
        assert_eq!(remote.read("/srv/link/keep.txt").unwrap(), b"keep");
        assert_eq!(
            fs::read(important.join("keep.txt")).unwrap(),
            b"keep",
            "the target is untouched"
        );
        assert!(
            fs::symlink_metadata(&link).is_err(),
            "the moved link itself is gone"
        );
    }

    #[tokio::test]
    async fn a_link_loop_is_skipped_instead_of_failing_the_transfer() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("a");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("file.txt"), b"x").unwrap();
        dir_link(&folder.join("loop"), &folder);
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/srv");

        let run = run(
            TransferKind::Copy,
            local(),
            memory(&remote, "ssh-1"),
            &[native(&folder)],
            &MemFs::path("/srv"),
            no_conflicts(),
        )
        .await;
        run.result.unwrap();
        assert_eq!(remote.read("/srv/a/file.txt").unwrap(), b"x");
        assert!(!remote.exists("/srv/a/loop"));
        assert_eq!(run.counters.snapshot().skipped, 1);
    }

    #[tokio::test]
    async fn a_broken_link_is_skipped_instead_of_failing_the_transfer() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("a");
        let gone = temp.path().join("gone");
        fs::create_dir(&folder).unwrap();
        fs::create_dir(&gone).unwrap();
        fs::write(folder.join("file.txt"), b"x").unwrap();
        dir_link(&folder.join("dangling"), &gone);
        fs::remove_dir(&gone).unwrap();
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/srv");

        let run = run(
            TransferKind::Copy,
            local(),
            memory(&remote, "ssh-1"),
            &[native(&folder)],
            &MemFs::path("/srv"),
            no_conflicts(),
        )
        .await;
        run.result.unwrap();
        assert_eq!(remote.read("/srv/a/file.txt").unwrap(), b"x");
        assert_eq!(run.counters.snapshot().skipped, 1);
    }

    #[tokio::test]
    async fn a_folder_link_is_copied_within_one_filesystem() {
        // Recreated where the OS allows it; otherwise (Windows without symlink
        // privilege) its contents are copied, as Explorer does for junctions.
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("data.txt"), b"data").unwrap();
        let source = temp.path().join("src");
        fs::create_dir(&source).unwrap();
        dir_link(&source.join("j"), &real);
        let destination = temp.path().join("dst");
        fs::create_dir(&destination).unwrap();

        let run = run(
            TransferKind::Copy,
            local(),
            local(),
            &[native(&source)],
            &native(&destination),
            no_conflicts(),
        )
        .await;
        run.result.unwrap();
        assert_eq!(
            fs::read(destination.join("src/j/data.txt")).unwrap(),
            b"data"
        );
        assert!(
            leftovers(&destination.join("src")).is_empty(),
            "no temporary link left behind"
        );
    }

    #[tokio::test]
    async fn apply_to_all_does_not_cross_file_and_folder_clashes() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/x", b"new");
        fs.add_dir("/src/y");
        fs.add_file("/src/y/inner", b"i");
        fs.add_file("/dst/x", b"old");
        fs.add_file("/dst/y", b"a file where a folder arrives");
        let resolver = Scripted::new(&[(Resolution::Overwrite, true), (Resolution::Skip, false)]);
        let sources = [MemFs::path("/src/x"), MemFs::path("/src/y")];

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
        assert_eq!(
            resolver.asked_names(),
            ["x", "y"],
            "the folder clash is asked separately"
        );
        assert_eq!(fs.read("/dst/x").unwrap(), b"new");
        assert_eq!(fs.read("/dst/y").unwrap(), b"a file where a folder arrives");
    }

    #[tokio::test]
    async fn an_invalid_typed_name_asks_again() {
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/dst");
        fs.add_file("/src/a", b"new");
        fs.add_file("/dst/a", b"old");
        let resolver = Scripted::new(&[
            (
                Resolution::Rename {
                    name: Some("bad/name".into()),
                },
                false,
            ),
            (Resolution::Skip, false),
        ]);
        run(
            TransferKind::Copy,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &[MemFs::path("/src/a")],
            &MemFs::path("/dst"),
            resolver.clone(),
        )
        .await
        .result
        .unwrap();
        assert_eq!(resolver.asked_names().len(), 2);
        assert_eq!(fs.read("/dst/a").unwrap(), b"old");
    }

    #[tokio::test]
    async fn children_reuse_the_listing_metadata() {
        // One lstat per entry would be one SFTP round trip each; the folder
        // listing already carries what the engine needs.
        let fs = Arc::new(MemFs::new());
        fs.add_dir("/src");
        fs.add_dir("/src/d");
        for index in 0..10 {
            fs.add_file(&format!("/src/d/{index}"), b"x");
        }
        fs.add_dir("/dst");
        run(
            TransferKind::Copy,
            memory(&fs, "m"),
            memory(&fs, "m"),
            &[MemFs::path("/src/d")],
            &MemFs::path("/dst"),
            no_conflicts(),
        )
        .await
        .result
        .unwrap();
        // Planning and running each look at the picked folder once; then one
        // target check per copied entry (1 folder + 10 files).
        assert!(fs.lstats() <= 13, "{} lstat calls", fs.lstats());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_an_entry_with_a_link_leaves_no_temporary_files() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink("real.txt", source.join("link.txt")).unwrap();
        fs::write(destination.join("link.txt"), b"old").unwrap();
        let resolver = Scripted::new(&[(Resolution::Overwrite, false)]);

        run(
            TransferKind::Copy,
            local(),
            local(),
            &[native(&source.join("link.txt"))],
            &native(&destination),
            resolver,
        )
        .await
        .result
        .unwrap();
        assert_eq!(
            fs::read_link(destination.join("link.txt")).unwrap(),
            Path::new("real.txt")
        );
        assert!(leftovers(&destination).is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn remote_names_windows_cannot_store_arrive_under_safe_names() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/src");
        for name in ["a:b.txt", "CON", "notes.", r"back\slash"] {
            remote.add_file(&format!("/src/{name}"), name.as_bytes());
        }
        let local_dir = tempdir().unwrap();
        let outcome = run(
            TransferKind::Copy,
            memory(&remote, "sftp:test"),
            local(),
            &[MemFs::path("/src")],
            &native(local_dir.path()),
            no_conflicts(),
        )
        .await;
        assert_eq!(outcome.result, Ok(()));
        let copied = local_dir.path().join("src");
        assert_eq!(fs::read(copied.join("a_b.txt")).unwrap(), b"a:b.txt");
        assert_eq!(fs::read(copied.join("CON_")).unwrap(), b"CON");
        assert_eq!(fs::read(copied.join("notes._")).unwrap(), b"notes.");
        assert_eq!(fs::read(copied.join("back_slash")).unwrap(), br"back\slash");
        // Nothing escaped the folder as a path.
        assert_eq!(fs::read_dir(&copied).unwrap().count(), 4);
    }
    fn movie(length: usize) -> Vec<u8> {
        (0..length).map(|i| (i % 251) as u8 + 1).collect()
    }

    /// A remote folder with one big file; returns its bytes.
    fn remote_movie(remote: &MemFs) -> Vec<u8> {
        let data = movie(3 * CHUNK_SIZE + 17);
        remote.add_dir("/src");
        remote.add_file("/src/movie.mkv", &data);
        data
    }

    async fn download_movie(remote: &Arc<MemFs>, to: &Path, resolver: Arc<Scripted>) -> Run {
        run(
            TransferKind::Copy,
            memory(remote, "sftp:test"),
            local(),
            &[MemFs::path("/src/movie.mkv")],
            &native(to),
            resolver,
        )
        .await
    }

    #[tokio::test]
    async fn an_interrupted_big_copy_keeps_its_filepart_and_resumes_from_there() {
        let remote = Arc::new(MemFs::new());
        let data = remote_movie(&remote);
        let local_dir = tempdir().unwrap();
        // The connection drops after the first chunk.
        remote.fail_next_read_midway();
        let first = download_movie(&remote, local_dir.path(), no_conflicts()).await;
        assert!(first.result.is_err());
        let partial = local_dir.path().join("movie.mkv.filepart");
        let have = fs::read(&partial).expect("the partial copy is kept").len();
        assert_eq!(have, CHUNK_SIZE);
        assert!(!local_dir.path().join("movie.mkv").exists());
        // Blank the partial: after resuming, these bytes show it was kept and
        // appended to, not rewritten.
        fs::write(&partial, vec![0; have]).unwrap();

        let resolver = Scripted::new(&[(Resolution::Resume, false)]);
        let second = download_movie(&remote, local_dir.path(), resolver.clone()).await;
        assert_eq!(second.result, Ok(()));
        let copied = fs::read(local_dir.path().join("movie.mkv")).unwrap();
        assert_eq!(copied.len(), data.len());
        assert!(copied[..have].iter().all(|&byte| byte == 0));
        assert_eq!(&copied[have..], &data[have..]);
        assert!(!partial.exists());
        let asked = resolver.asked.lock().unwrap();
        assert!(asked[0].partial);
        assert_eq!(asked[0].target.size, Some(have as u64));
        // Progress counts the whole file, the resumed part included.
        assert_eq!(second.counters.snapshot().done.bytes, data.len() as u64);
    }

    #[tokio::test]
    async fn starting_over_replaces_a_partial_copy_and_skipping_leaves_it() {
        let remote = Arc::new(MemFs::new());
        let data = remote_movie(&remote);
        let local_dir = tempdir().unwrap();
        let partial = local_dir.path().join("movie.mkv.filepart");

        fs::write(&partial, vec![0; 1000]).unwrap();
        let skipped = download_movie(
            &remote,
            local_dir.path(),
            Scripted::new(&[(Resolution::Skip, false)]),
        )
        .await;
        assert_eq!(skipped.result, Ok(()));
        assert_eq!(skipped.counters.snapshot().skipped, 1);
        assert_eq!(fs::read(&partial).unwrap().len(), 1000);
        assert!(!local_dir.path().join("movie.mkv").exists());

        let restarted = download_movie(
            &remote,
            local_dir.path(),
            Scripted::new(&[(Resolution::Overwrite, false)]),
        )
        .await;
        assert_eq!(restarted.result, Ok(()));
        assert_eq!(fs::read(local_dir.path().join("movie.mkv")).unwrap(), data);
        assert!(!partial.exists());
    }

    #[tokio::test]
    async fn a_leftover_that_cannot_be_resumed_is_replaced_without_asking() {
        let remote = Arc::new(MemFs::new());
        let data = remote_movie(&remote);
        let local_dir = tempdir().unwrap();
        // As big as the source already: nothing left to resume.
        fs::write(local_dir.path().join("movie.mkv.filepart"), &data).unwrap();
        let resolver = no_conflicts();
        let outcome = download_movie(&remote, local_dir.path(), resolver.clone()).await;
        assert_eq!(outcome.result, Ok(()));
        assert!(resolver.asked.lock().unwrap().is_empty());
        assert_eq!(fs::read(local_dir.path().join("movie.mkv")).unwrap(), data);
    }

    #[tokio::test]
    async fn cancelling_a_big_copy_removes_its_filepart() {
        let remote = Arc::new(MemFs::new());
        remote_movie(&remote);
        let local_dir = tempdir().unwrap();
        let (cancel, receiver) = watch::channel(false);
        remote.on_first_read(move || {
            cancel.send_replace(true);
        });
        let outcome = run_with(
            TransferKind::Copy,
            memory(&remote, "sftp:test"),
            local(),
            &[MemFs::path("/src/movie.mkv")],
            &native(local_dir.path()),
            no_conflicts(),
            receiver,
        )
        .await;
        assert_eq!(outcome.result, Err(TransferError::Cancelled));
        assert_eq!(fs::read_dir(local_dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_small_file_cut_short_leaves_nothing_behind() {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/src");
        remote.add_file("/src/note.txt", b"short");
        let local_dir = tempdir().unwrap();
        remote.fail_next_read_midway();
        let outcome = run(
            TransferKind::Copy,
            memory(&remote, "sftp:test"),
            local(),
            &[MemFs::path("/src/note.txt")],
            &native(local_dir.path()),
            no_conflicts(),
        )
        .await;
        assert!(outcome.result.is_err());
        assert_eq!(fs::read_dir(local_dir.path()).unwrap().count(), 0);
    }
    #[tokio::test]
    async fn copied_folders_keep_their_times() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("project");
        fs::create_dir_all(source.join("docs")).unwrap();
        fs::write(source.join("docs/guide.md"), b"read me").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
        // After the contents: writing them would bump the folder's time.
        LocalFs
            .set_times(&native(&source.join("docs")), old, None)
            .await
            .unwrap();
        let destination = temp.path().join("copy");
        fs::create_dir(&destination).unwrap();
        let outcome = run(
            TransferKind::Copy,
            local(),
            local(),
            &[native(&source)],
            &native(&destination),
            no_conflicts(),
        )
        .await;
        assert_eq!(outcome.result, Ok(()));
        let copied = fs::metadata(destination.join("project/docs")).unwrap();
        assert_eq!(copied.modified().unwrap(), old);
    }

    #[tokio::test]
    async fn permissions_are_copied_unless_the_request_says_not_to() {
        for preserve in [true, false] {
            let fs = Arc::new(MemFs::new());
            fs.add_dir("/src");
            fs.add_dir("/dst");
            fs.add_file("/src/run.sh", b"#!/bin/sh\n");
            fs.set_mode(&MemFs::path("/src/run.sh"), 0o755)
                .await
                .unwrap();
            let (_cancel, receiver) = watch::channel(false);
            let mut engine = Engine::new(
                TransferKind::Copy,
                memory(&fs, "a"),
                memory(&fs, "b"),
                Arc::new(Counters::default()),
                receiver,
                no_conflicts(),
            );
            engine.set_preserve_mode(preserve);
            let sources = [MemFs::path("/src/run.sh")];
            engine.plan(&sources).await.unwrap();
            engine.run(&sources, &MemFs::path("/dst")).await.unwrap();
            let mode = fs.stat(&MemFs::path("/dst/run.sh")).await.unwrap().mode;
            assert_eq!(
                mode,
                Some(if preserve { 0o755 } else { 0o644 }),
                "preserve={preserve}"
            );
        }
    }
}

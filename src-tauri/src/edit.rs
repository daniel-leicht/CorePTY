//! Opening remote files in local apps (WinSCP style). A copy is downloaded
//! into its own temp folder and opened there. When editing, the copy is
//! watched: each save is uploaded back, after checking that the file on the
//! server hasn't changed since (if it has, the user decides). A save that
//! can't be uploaded is tried again (at once when the connection comes back).
//! When the tab closes, a copy is deleted only if the server has everything
//! in it; otherwise it's kept, and the user told where.

use crate::sftp::commands::{SessionUse, SftpSessions};
use crate::vfs::{
    epoch_millis, EntryKind, FsRegistry, Location, Meta, VName, VPath, Vfs, WriteMode,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tauri::{AppHandle, Emitter, State};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
};

/// How often an edited copy is checked for saves.
const POLL: Duration = Duration::from_millis(if cfg!(test) { 40 } else { 1000 });
/// How often a save that couldn't be uploaded is tried again (and at once
/// when the connection comes back).
const RETRY: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 15_000 });
/// How long closing a tab waits for a last upload.
const CLOSE_WAIT: Duration = Duration::from_secs(if cfg!(test) { 5 } else { 30 });

/// The server's file: size and modified time to the second, as SFTP keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: Option<u64>,
    seconds: Option<u64>,
}

impl Stamp {
    fn of(meta: &Meta) -> Self {
        Self {
            size: meta.size,
            seconds: meta
                .modified
                .and_then(epoch_millis)
                .map(|millis| millis / 1000),
        }
    }
}

/// The copy: size and modified time at full precision, so two saves within
/// one second (with the same size) still count as two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalStamp {
    size: u64,
    modified: Option<SystemTime>,
}

fn local_stamp(path: &Path) -> Option<LocalStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(LocalStamp {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// Sent as `fs://edit` while an edited copy is synced.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EditEvent {
    Uploading {
        id: String,
        name: String,
    },
    Uploaded {
        id: String,
        name: String,
    },
    /// The file changed on the server since it was opened: answer with
    /// `fs_edit_resolve(id, overwrite)`.
    Conflict {
        id: String,
        name: String,
    },
    /// A save couldn't be uploaded; it's tried again.
    Failed {
        id: String,
        name: String,
        message: String,
    },
    /// No more uploads from this copy (the user kept the server's version, or
    /// the server's file isn't a file any more). The copy stays at `path`.
    Stopped {
        id: String,
        name: String,
        path: String,
        reason: String,
    },
}

pub type EditSink = Arc<dyn Fn(EditEvent) + Send + Sync>;

/// A copy that was kept when its tab closed: the server doesn't have all of it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeptCopy {
    pub name: String,
    pub path: String,
    pub reason: String,
}

/// An edited copy's state, shared with the exit prompt (read at the moment
/// it asks, not as of the last poll).
#[derive(Debug, Default)]
struct Shared {
    /// The copy as last uploaded (or downloaded).
    synced: Option<LocalStamp>,
    /// An upload is running.
    syncing: bool,
    /// A save failed to upload, or a conflict went unanswered.
    behind: bool,
}

/// A downloaded copy of a remote file.
struct Copy {
    dir: PathBuf,
    local: PathBuf,
    name: String,
    /// The copy as downloaded: one opened with Enter and then changed is kept
    /// on close (its changes go nowhere else).
    downloaded: Option<LocalStamp>,
    /// Set while it's being edited.
    watcher: Option<Watcher>,
}

struct Watcher {
    stop: watch::Sender<bool>,
    answers: mpsc::UnboundedSender<bool>,
    /// What became of the copy once the watcher stopped.
    done: oneshot::Receiver<Option<KeptCopy>>,
    state: Arc<Mutex<Shared>>,
}

/// An edited copy with changes the server doesn't have yet.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingEdit {
    pub name: String,
    pub path: String,
}

/// Tauri-managed state: the copies opened this session, by id.
#[derive(Default)]
pub struct OpenedCopies {
    copies: Mutex<HashMap<String, Copy>>,
    events: RwLock<Option<EditSink>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl OpenedCopies {
    fn copies(&self) -> MutexGuard<'_, HashMap<String, Copy>> {
        lock(&self.copies)
    }

    pub fn set_events(&self, sink: EditSink) {
        *self.events.write().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    fn sink(&self) -> EditSink {
        self.events
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| Arc::new(|_| {}))
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
        let shown = name.display();
        let name = name.storable_here().unwrap_or(name);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = root.join(&id);
        private_dir(&dir)?;
        let local = dir.join(&*name.to_native());
        match download(fs, path, &local).await {
            Ok(meta) => {
                self.copies().insert(
                    id.clone(),
                    Copy {
                        dir,
                        local: local.clone(),
                        name: shown,
                        downloaded: local_stamp(&local),
                        watcher: None,
                    },
                );
                Ok((id, local, meta))
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                Err(e)
            }
        }
    }

    /// Download `path` and keep uploading the copy's saves back to it.
    /// `keep_open` (the server's session) lives as long as the watcher.
    async fn edit(
        &self,
        fs: Arc<dyn Vfs>,
        path: &VPath,
        root: &Path,
        keep_open: Option<SessionUse>,
    ) -> io::Result<(String, PathBuf)> {
        let (id, local, meta) = self.download(fs.as_ref(), path, root).await?;
        // Saves go to the file itself: through a link, to what it points at
        // (replacing the link would leave the real file unchanged).
        let remote = fs.canonicalize(path).await.unwrap_or_else(|_| path.clone());
        let (stop, stopped) = watch::channel(false);
        let (answers, answered) = mpsc::unbounded_channel();
        let (done_tx, done) = oneshot::channel();
        let synced = local_stamp(&local);
        let state = Arc::new(Mutex::new(Shared {
            synced,
            ..Shared::default()
        }));
        let session = Session {
            id: id.clone(),
            name: path
                .file_name()
                .map(|name| name.display())
                .unwrap_or_default(),
            fs,
            remote,
            local: local.clone(),
            original: Original::of(&meta),
            remote_stamp: Some(Stamp::of(&meta)),
            synced,
            attempted: None,
            pending: None,
            conflicted: false,
            stopped: None,
            leftover: None,
            sink: self.sink(),
            state: state.clone(),
            _keep_open: keep_open,
        };
        let dir = match self.copies().get_mut(&id) {
            Some(copy) => {
                copy.watcher = Some(Watcher {
                    stop,
                    answers,
                    done,
                    state,
                });
                copy.dir.clone()
            }
            None => return Err(io::Error::other("the copy went away")),
        };
        tokio::spawn(session.watch(dir, stopped, answered, done_tx));
        Ok((id, local))
    }

    /// Answer a conflict: upload the copy over the server's file, or keep it.
    fn resolve(&self, id: &str, overwrite: bool) {
        if let Some(Copy {
            watcher: Some(watcher),
            ..
        }) = self.copies().get(id)
        {
            let _ = watcher.answers.send(overwrite);
        }
    }

    /// Edited copies with changes the server doesn't have yet, as of now: a
    /// save the watcher hasn't seen, an upload running, or one that failed.
    fn unsynced(&self) -> Vec<PendingEdit> {
        self.copies()
            .values()
            .filter(|copy| {
                copy.watcher.as_ref().is_some_and(|watcher| {
                    let state = lock(&watcher.state);
                    state.syncing || state.behind || local_stamp(&copy.local) != state.synced
                })
            })
            .map(|copy| PendingEdit {
                name: copy.name.clone(),
                path: copy.local.to_string_lossy().into_owned(),
            })
            .collect()
    }

    /// Delete copies (their apps may still have them open: best effort). An
    /// edited one uploads a last change first; one the server doesn't have
    /// all of is kept instead, and returned. So is one opened with Enter and
    /// changed since (nothing uploads those).
    async fn close(&self, ids: &[String]) -> Vec<KeptCopy> {
        let removed: Vec<Copy> = {
            let mut copies = self.copies();
            ids.iter().filter_map(|id| copies.remove(id)).collect()
        };
        let mut kept = Vec::new();
        for copy in removed {
            match copy.watcher {
                Some(watcher) => {
                    watcher.stop.send_replace(true);
                    match tokio::time::timeout(CLOSE_WAIT, watcher.done).await {
                        Ok(Ok(Some(copy))) => kept.push(copy),
                        Ok(_) => {}
                        Err(_) => kept.push(KeptCopy {
                            name: copy.name,
                            path: copy.local.to_string_lossy().into_owned(),
                            reason: "its upload was still running; the copy goes once it's \
                                     done, and stays if it fails"
                                .to_string(),
                        }),
                    }
                }
                None if local_stamp(&copy.local).is_some()
                    && local_stamp(&copy.local) != copy.downloaded =>
                {
                    kept.push(KeptCopy {
                        name: copy.name,
                        path: copy.local.to_string_lossy().into_owned(),
                        reason: "it was changed, and copies opened with Enter aren't \
                                 uploaded (F4 edits a remote file)"
                            .to_string(),
                    });
                }
                None => {
                    let _ = tokio::fs::remove_dir_all(&copy.dir).await;
                }
            }
        }
        kept
    }
}

/// What the server's file was like, to keep it so on upload.
#[derive(Debug, Clone, Default)]
struct Original {
    mode: Option<u32>,
    owner: Option<String>,
    group: Option<String>,
}

impl Original {
    fn of(meta: &Meta) -> Self {
        Self {
            mode: meta.mode,
            owner: meta.owner.clone(),
            group: meta.group.clone(),
        }
    }
}

/// One edited copy, watched for saves.
struct Session {
    id: String,
    name: String,
    fs: Arc<dyn Vfs>,
    /// Where saves go (links resolved).
    remote: VPath,
    local: PathBuf,
    original: Original,
    /// The server's file as last downloaded or uploaded (`None`: not known —
    /// an upload landed but couldn't be checked, or was cut off half-way:
    /// what's there is ours, so the next upload doesn't ask).
    remote_stamp: Option<Stamp>,
    /// The copy as last uploaded (or downloaded).
    synced: Option<LocalStamp>,
    /// The copy as last tried: a failed upload isn't retried on every poll,
    /// only on the retry timer, a reconnect, or a new save.
    attempted: Option<LocalStamp>,
    /// A save that couldn't be uploaded, and why.
    pending: Option<String>,
    /// The tab closed while the user was asked about a conflict.
    conflicted: bool,
    /// No more uploads, and why.
    stopped: Option<String>,
    /// A temporary file a failed upload couldn't remove from the server.
    leftover: Option<VPath>,
    sink: EditSink,
    state: Arc<Mutex<Shared>>,
    _keep_open: Option<SessionUse>,
}

/// Resolves when the connection comes back (never, for one that can't drop).
async fn came_back(connection: &mut Option<watch::Receiver<bool>>) {
    loop {
        let Some(receiver) = connection.as_mut() else {
            return std::future::pending().await;
        };
        if receiver.changed().await.is_err() {
            *connection = None;
            continue;
        }
        if *receiver.borrow_and_update() {
            return;
        }
    }
}

impl Session {
    async fn watch(
        mut self,
        dir: PathBuf,
        mut stop: watch::Receiver<bool>,
        mut answers: mpsc::UnboundedReceiver<bool>,
        done: oneshot::Sender<Option<KeptCopy>>,
    ) {
        let mut connection = self.fs.connection();
        let mut retry_at: Option<Instant> = None;
        loop {
            let stopping = tokio::select! {
                _ = tokio::time::sleep(POLL) => false,
                _ = stop.wait_for(|stop| *stop) => true,
                _ = came_back(&mut connection) => {
                    retry_at = Some(Instant::now());
                    false
                }
            };
            if self.stopped.is_none() {
                let mut due =
                    self.pending.is_some() && retry_at.is_some_and(|at| Instant::now() >= at);
                if let Some(now) = self.new_save() {
                    // Wait for the editor to finish writing before uploading.
                    tokio::time::sleep(POLL / 2).await;
                    due |= local_stamp(&self.local) == Some(now);
                }
                if due || (stopping && self.has_changes()) {
                    self.publish(true);
                    self.sync(&mut answers, &mut stop).await;
                    retry_at = self.pending.as_ref().map(|_| Instant::now() + RETRY);
                }
            }
            self.publish(false);
            if stopping || *stop.borrow() {
                let kept = self.kept();
                if kept.is_none() {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                }
                let _ = done.send(kept);
                return;
            }
        }
    }

    fn publish(&self, syncing: bool) {
        let mut state = lock(&self.state);
        state.synced = self.synced;
        state.syncing = syncing;
        state.behind = self.pending.is_some() || self.conflicted;
    }

    /// The copy's stamp, if it changed since the last upload.
    fn saved(&self) -> Option<LocalStamp> {
        let now = local_stamp(&self.local)?;
        (Some(now) != self.synced).then_some(now)
    }

    /// The copy's stamp, if it was saved since the last upload or attempt.
    fn new_save(&self) -> Option<LocalStamp> {
        self.saved().filter(|now| Some(*now) != self.attempted)
    }

    /// Changes the server doesn't have.
    fn has_changes(&self) -> bool {
        self.pending.is_some() || self.saved().is_some()
    }

    /// Kept on close, and why (`None`: the server has it all, it can go).
    fn kept(&self) -> Option<KeptCopy> {
        let reason = if self.conflicted {
            "it changed on the server too, and the question wasn't answered".to_string()
        } else if let Some(reason) = &self.stopped {
            // Nothing new since it stopped: it can go.
            self.saved()?;
            reason.clone()
        } else if let Some(error) = &self.pending {
            format!("it couldn't be uploaded ({error})")
        } else if self.saved().is_some() {
            "its last save couldn't be uploaded".to_string()
        } else {
            return None;
        };
        Some(KeptCopy {
            name: self.name.clone(),
            path: self.local.to_string_lossy().into_owned(),
            reason,
        })
    }

    fn failed(&mut self, message: String) {
        // Told once per run of failures, not on every retry.
        if self.pending.is_none() {
            (self.sink)(EditEvent::Failed {
                id: self.id.clone(),
                name: self.name.clone(),
                message: format!("{message}. It's tried again, and when the connection is back."),
            });
        }
        self.pending = Some(message);
    }

    fn stop_syncing(&mut self, reason: &str) {
        (self.sink)(EditEvent::Stopped {
            id: self.id.clone(),
            name: self.name.clone(),
            path: self.local.to_string_lossy().into_owned(),
            reason: reason.to_string(),
        });
        self.stopped = Some(reason.to_string());
        self.pending = None;
    }

    async fn sync(
        &mut self,
        answers: &mut mpsc::UnboundedReceiver<bool>,
        stop: &mut watch::Receiver<bool>,
    ) {
        let Some(now) = local_stamp(&self.local) else {
            return; // the copy is gone (an editor saved elsewhere): nothing to send
        };
        let (id, name) = (self.id.clone(), self.name.clone());
        let on_server = match self.fs.stat(&self.remote).await {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return self.failed(format!("couldn't check the file on the server: {e}")),
        };
        if on_server.as_ref().is_some_and(|meta| meta.is_directory) {
            // A folder by that name now: never put a file over it.
            return self.stop_syncing("on the server it's a folder now");
        }
        let changed = match (&on_server, self.remote_stamp) {
            (None, _) => true, // deleted on the server
            (Some(meta), Some(known)) => Stamp::of(meta) != known,
            (Some(_), None) => false,
        };
        if changed {
            (self.sink)(EditEvent::Conflict {
                id: id.clone(),
                name: name.clone(),
            });
            while answers.try_recv().is_ok() {}
            let overwrite = tokio::select! {
                answer = answers.recv() => answer,
                // The tab closed while asked: nothing is uploaded, the copy stays.
                _ = stop.wait_for(|stop| *stop) => None,
            };
            match overwrite {
                None => {
                    self.conflicted = true;
                    return;
                }
                Some(false) => {
                    return self.stop_syncing("you kept the server's version");
                }
                Some(true) => {}
            }
        }
        if let Some(meta) = &on_server {
            self.original = Original::of(meta);
        }
        (self.sink)(EditEvent::Uploading {
            id: id.clone(),
            name: name.clone(),
        });
        self.attempted = Some(now);
        match upload(self.fs.as_ref(), &self.local, &self.remote, &self.original).await {
            Ok(stamp) => {
                self.remote_stamp = stamp;
                self.synced = Some(now);
                self.pending = None;
                self.conflicted = false;
                if let Some(leftover) = self.leftover.take() {
                    let _ = self.fs.remove(&leftover, false).await;
                }
                (self.sink)(EditEvent::Uploaded { id, name });
            }
            Err(e) => {
                if let Some(leftover) = e.leftover {
                    self.leftover = Some(leftover);
                }
                if e.error.kind() == io::ErrorKind::IsADirectory {
                    return self.stop_syncing("on the server it's a folder now");
                }
                if e.touched {
                    // What's on the server now is this upload, cut off: the
                    // retry goes over it without asking.
                    self.remote_stamp = None;
                    self.failed(format!(
                        "{}; the file on the server may be incomplete until it goes up",
                        e.error
                    ));
                } else {
                    self.failed(e.error.to_string());
                }
            }
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
    // Before the time: writing the mark counts as changing the file.
    mark_as_downloaded(local);
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

async fn send_file(local: &Path, writer: &mut crate::vfs::VWrite) -> io::Result<()> {
    let mut source = tokio::fs::File::open(local).await?;
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let read = source.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read]).await?;
    }
    writer.shutdown().await
}

/// Why an upload failed, and what it left behind.
struct UploadError {
    error: io::Error,
    /// The server's file may have changed (written in place part-way, or a
    /// rename whose answer was lost).
    touched: bool,
    /// A temporary file that couldn't be removed.
    leftover: Option<VPath>,
}

impl From<io::Error> for UploadError {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            touched: false,
            leftover: None,
        }
    }
}

/// Remove a temporary file; `Some` (it) when that failed and it's left behind.
async fn left_behind(fs: &dyn Vfs, temporary: &VPath) -> Option<VPath> {
    fs.remove(temporary, false)
        .await
        .err()
        .map(|_| temporary.clone())
}

/// Upload the copy over the server's file. Usually in one step, as editors
/// save: written beside it under a temporary name that gets the file's mode
/// before any of its contents (so a private file is never readable on the
/// way), then renamed over it (a failure leaves the old one). In place
/// instead when that would change the file's mode or owner (a server that
/// refuses setting it, a file someone else owns), the folder takes no new
/// files, or the file is reached through a link: the file itself is kept
/// (owner, links to it, attributes), at the cost of a half-written file if
/// the connection drops mid-way. Returns the new stamp, `None` if the file
/// couldn't be checked afterwards.
async fn upload(
    fs: &dyn Vfs,
    local: &Path,
    remote: &VPath,
    original: &Original,
) -> Result<Option<Stamp>, UploadError> {
    // A link the server didn't resolve: writing through it reaches the file.
    let through_link =
        matches!(fs.lstat(remote).await, Ok(meta) if meta.kind == EntryKind::Symlink);
    let folder = remote.parent().unwrap_or_else(|| remote.clone());
    let temporary = folder.join(&VName::for_path(
        &folder,
        &format!(".corepty-{}.part", uuid::Uuid::new_v4().simple()),
    ))?;
    let mut staged = false;
    if !through_link {
        match fs.open_write(&temporary, WriteMode::CreateNew).await {
            Ok(mut writer) => {
                let fits = async {
                    if let Some(mode) = original.mode {
                        fs.set_mode(&temporary, mode).await?;
                    }
                    // Renaming a file someone else owns would make it ours.
                    let made = fs.stat(&temporary).await?;
                    Ok::<bool, io::Error>(
                        (original.owner.is_none() || made.owner == original.owner)
                            && (original.group.is_none() || made.group == original.group),
                    )
                }
                .await
                .unwrap_or(false);
                if fits {
                    if let Err(error) = send_file(local, &mut writer).await {
                        drop(writer);
                        return Err(UploadError {
                            error,
                            touched: false,
                            leftover: left_behind(fs, &temporary).await,
                        });
                    }
                    if let Ok(modified) = std::fs::metadata(local).and_then(|m| m.modified()) {
                        let _ = fs.set_times(&temporary, modified, None).await;
                    }
                    // A folder that appeared meanwhile is never replaced.
                    if matches!(fs.lstat(remote).await, Ok(meta) if meta.is_real_directory()) {
                        return Err(UploadError {
                            error: io::Error::new(io::ErrorKind::IsADirectory, "it's a folder now"),
                            touched: false,
                            leftover: left_behind(fs, &temporary).await,
                        });
                    }
                    if let Err(error) = fs.rename(&temporary, remote, true).await {
                        return Err(UploadError {
                            error,
                            touched: true, // it may have landed before the answer was lost
                            leftover: left_behind(fs, &temporary).await,
                        });
                    }
                    staged = true;
                } else {
                    drop(writer);
                    let _ = fs.remove(&temporary, false).await;
                }
            }
            // No new files in that folder (`/etc` for a user who may write
            // `/etc/hosts`): write the file in place.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
            Err(e) => return Err(e.into()),
        }
    }
    if !staged {
        let mut writer = fs.open_write(remote, WriteMode::Truncate).await?;
        // From here the server's file is this upload, half-written if it fails.
        if let Err(error) = send_file(local, &mut writer).await {
            return Err(UploadError {
                error,
                touched: true,
                leftover: None,
            });
        }
    }
    Ok(fs.stat(remote).await.ok().map(|meta| Stamp::of(&meta)))
}

/// Split an editor command line (`code --wait`, `"C:\Program Files\x\x.exe" -n`)
/// into program and arguments; `{file}` marks where the file goes (else last).
fn editor_command(line: &str, file: &Path) -> Option<(String, Vec<String>)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    parts.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        parts.push(current);
    }
    if parts.is_empty() {
        return None;
    }
    let file = file.to_string_lossy();
    let mut placed = false;
    for part in &mut parts {
        if part.contains("{file}") {
            *part = part.replace("{file}", &file);
            placed = true;
        }
    }
    if !placed {
        parts.push(file.into_owned());
    }
    let mut parts = parts.into_iter();
    let program = parts.next()?;
    Some((program, parts.collect()))
}

/// A bare program name as Windows' shell finds it: every `PATH` folder, with
/// each `PATHEXT` extension (`code` is `code.cmd`). Names with a folder or an
/// extension are left alone.
#[cfg_attr(not(windows), allow(dead_code))]
fn find_program(name: &str, path: &std::ffi::OsStr, extensions: &str) -> Option<PathBuf> {
    let given = Path::new(name);
    if given.components().count() != 1 || given.extension().is_some() {
        return None;
    }
    for folder in std::env::split_paths(path) {
        for extension in extensions.split(';').filter(|ext| !ext.is_empty()) {
            let candidate = folder.join(format!("{name}{}", extension.to_ascii_lowercase()));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Start `program` detached from us; on Unix a thread reaps it when it exits.
fn spawn_editor(program: &str, args: &[String]) -> io::Result<()> {
    #[cfg(windows)]
    let program = std::env::var_os("PATH")
        .and_then(|path| {
            let extensions =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            find_program(program, &path, &extensions)
        })
        .map(|found| found.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    #[cfg(not(windows))]
    let program = program.to_string();
    let mut command = std::process::Command::new(&program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // A batch-file launcher (`code.cmd`) would keep a console window open.
        use std::os::windows::process::CommandExt;
        let lower = program.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
    }
    let child = command
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("couldn't start {program}: {e}")))?;
    #[cfg(unix)]
    {
        let mut child = child;
        std::thread::spawn(move || child.wait());
    }
    #[cfg(not(unix))]
    drop(child);
    Ok(())
}

/// Open `file` for editing with no editor set: never the type's "open" (that
/// runs scripts and programs). Windows: for a type known to be safe, its
/// "edit" verb (Paint for an image, Word for a document), otherwise Notepad;
/// macOS: the default text editor; elsewhere: the desktop's text editor.
fn edit_with_default(file: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        if name.as_deref().is_some_and(|name| !runs_code(name)) && shell_edit(file) {
            return Ok(());
        }
        spawn_editor("notepad.exe", &[file.to_string_lossy().into_owned()])
    }
    #[cfg(target_os = "macos")]
    {
        spawn_editor(
            "open",
            &["-t".to_string(), file.to_string_lossy().into_owned()],
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // The app set for plain text, never the file type's own (a `.jar` or
        // `.desktop` one would run it).
        let editor = std::process::Command::new("xdg-mime")
            .args(["query", "default", "text/plain"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|id| !id.is_empty());
        let no_editor = || {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no text editor is set up for this desktop: choose one in Settings › File manager",
            )
        };
        let editor = editor.ok_or_else(no_editor)?;
        spawn_editor("gtk-launch", &[editor, file.to_string_lossy().into_owned()])
            .map_err(|_| no_editor())
    }
}

/// The shell's "edit" verb for `file`; false when its type has none.
#[cfg(windows)]
fn shell_edit(file: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::{
            System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
            UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        },
    };
    let wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
    let verb: Vec<u16> = "edit".encode_utf16().chain(Some(0)).collect();
    // ShellExecute can hand off to shell extensions: COM first.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let started = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 means it started.
    started.0 as isize > 32
}

/// Open `file`: with the default app, or (`editing`) in `editor`, a command
/// line, or the default editor when that's blank.
fn launch(editing: bool, editor: Option<&str>, file: &Path) -> io::Result<()> {
    match editor.map(str::trim).filter(|line| !line.is_empty()) {
        Some(line) if editing => {
            let (program, args) = editor_command(line, file)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no editor given"))?;
            spawn_editor(&program, &args)
        }
        _ if editing => edit_with_default(file),
        _ => open::that_detached(file),
    }
}

/// Types that are only ever shown, played or edited when opened: text, data,
/// documents without macros, pictures, sound, video, archives. Any other type
/// may run code (programs, scripts, shortcuts, installers, and types handled
/// by an interpreter or a remote-desktop client), so opening it asks first.
const SAFE_TO_OPEN: &[&str] = &[
    // text and data
    "txt", "text", "log", "md", "markdown", "rst", "adoc", "json", "jsonl", "yaml", "yml", "toml",
    "ini", "cfg", "conf", "csv", "tsv", "xml", "diff", "patch", "srt", "vtt",
    // documents (none that hold macros: doc, docm, xls, xlsm, ppt, pptm ask)
    "pdf", "rtf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "odg", "epub", "html", "htm",
    // pictures
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "tif", "tiff", "heic", "heif", "avif", "svg",
    "psd", // sound and video
    "mp3", "wav", "flac", "ogg", "oga", "opus", "m4a", "aac", "wma", "mp4", "m4v", "mkv", "avi",
    "mov", "webm", "wmv", "mpg", "mpeg", "ogv",
    // archives (they open in an archive tool, which shows them)
    "zip", "tar", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar",
];

/// Unicode controls that can make a name read differently: `invoice` + U+202E +
/// `fdp.exe` shows as `invoiceexe.pdf`.
fn has_bidi_controls(name: &str) -> bool {
    name.chars().any(|c| {
        matches!(
            c,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
    })
}

/// Whether opening a file of this name may run something on this computer:
/// its type isn't one known to be safe to open (no extension included), or
/// its name hides what it is.
pub fn runs_code(name: &str) -> bool {
    if has_bidi_controls(name) {
        return true;
    }
    let extension = name
        .trim_end_matches(['.', ' '])
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    !extension.is_some_and(|extension| SAFE_TO_OPEN.contains(&extension.as_str()))
}

/// Tag a downloaded copy as coming from elsewhere (Windows' Mark of the Web),
/// so SmartScreen and Office's Protected View apply to it as to any download.
fn mark_as_downloaded(local: &Path) {
    #[cfg(windows)]
    {
        let mut stream = local.as_os_str().to_os_string();
        stream.push(":Zone.Identifier");
        let _ = std::fs::write(PathBuf::from(stream), "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
    #[cfg(not(windows))]
    let _ = local;
}

/// A folder only this user can enter.
fn private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Where copies go. Windows and macOS temp folders are per user already; on
/// Linux `/tmp` is shared, so each run makes a fresh folder only this user can
/// enter (one someone else made first can't be used to swap a copy).
fn copies_root() -> io::Result<PathBuf> {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    if let Some(root) = ROOT.get() {
        return Ok(root.clone());
    }
    #[cfg(unix)]
    let root = {
        let root = std::env::temp_dir().join(format!("corepty-{}", uuid::Uuid::new_v4().simple()));
        private_dir(&root)?;
        root.join("open")
    };
    #[cfg(not(unix))]
    let root = std::env::temp_dir().join("CorePTY").join("open");
    std::fs::create_dir_all(&root)?;
    Ok(ROOT.get_or_init(|| root).clone())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyView {
    pub id: String,
    /// Where the copy is on this machine.
    pub path: String,
}

async fn open_local(editing: bool, editor: Option<String>, local: PathBuf) -> Result<(), String> {
    let shown = local.clone();
    tokio::task::spawn_blocking(move || launch(editing, editor.as_deref(), &local))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not open {}: {e}", shown.to_string_lossy()))
}

/// Whether opening a file of this name may run code here: the file manager
/// asks first.
#[tauri::command]
pub fn fs_runs_code(name: String) -> bool {
    runs_code(&name)
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
    let root = copies_root().map_err(|e| format!("could not make a folder for the copy: {e}"))?;
    let (id, local, _) = copies
        .download(fs.as_ref(), &location.path, &root)
        .await
        .map_err(|e| format!("could not download {}: {e}", location.path.display()))?;
    if let Err(e) = open_local(false, None, local.clone()).await {
        copies.close(std::slice::from_ref(&id)).await;
        return Err(e);
    }
    Ok(CopyView {
        id,
        path: local.to_string_lossy().into_owned(),
    })
}

/// Edit a remote file (F4): download it, open it in `editor` (blank: the
/// default editor), and upload every save (`fs://edit` events report it).
#[tauri::command]
pub async fn fs_edit(
    registry: State<'_, FsRegistry>,
    sessions: State<'_, SftpSessions>,
    copies: State<'_, OpenedCopies>,
    location: Location,
    editor: Option<String>,
) -> Result<CopyView, String> {
    let fs = registry.get(&location.fs)?;
    let root = copies_root().map_err(|e| format!("could not make a folder for the copy: {e}"))?;
    let keep_open = sessions.use_of(&registry, &location.fs);
    let (id, local) = copies
        .edit(fs, &location.path, &root, keep_open)
        .await
        .map_err(|e| format!("could not download {}: {e}", location.path.display()))?;
    if let Err(e) = open_local(true, editor, local.clone()).await {
        copies.close(std::slice::from_ref(&id)).await;
        return Err(e);
    }
    Ok(CopyView {
        id,
        path: local.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
pub fn fs_edit_resolve(copies: State<'_, OpenedCopies>, id: String, overwrite: bool) {
    copies.resolve(&id, overwrite);
}

/// Close a tab's copies; returns those kept because the server doesn't have
/// all of them (the user is told where they are).
#[tauri::command]
pub async fn fs_close_copies(
    copies: State<'_, OpenedCopies>,
    ids: Vec<String>,
) -> Result<Vec<KeptCopy>, String> {
    Ok(copies.close(&ids).await)
}

/// Edited files whose latest changes aren't on the server yet (asked on exit).
#[tauri::command]
pub fn fs_edits_pending(copies: State<'_, OpenedCopies>) -> Vec<PendingEdit> {
    copies.unsynced()
}

/// Route `fs://edit` events to the window.
pub fn forward_events(app: AppHandle, copies: &OpenedCopies) {
    copies.set_events(Arc::new(move |event| {
        let _ = app.emit("fs://edit", event);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::memory::MemFs;
    use std::time::UNIX_EPOCH;

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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(local.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "only this user can enter a copy's folder"
            );
        }
        #[cfg(windows)]
        {
            let mut stream = local.as_os_str().to_os_string();
            stream.push(":Zone.Identifier");
            let mark = std::fs::read_to_string(PathBuf::from(stream)).unwrap();
            assert!(mark.contains("ZoneId=3"), "{mark}");
        }

        assert!(copies.close(&[id, second]).await.is_empty());
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

    struct Editing {
        remote: Arc<MemFs>,
        copies: OpenedCopies,
        events: Arc<Mutex<Vec<EditEvent>>>,
        root: tempfile::TempDir,
    }

    fn editing() -> Editing {
        let remote = Arc::new(MemFs::new());
        remote.add_dir("/srv");
        remote.add_file("/srv/app.conf", b"port=80\n");
        let copies = OpenedCopies::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        copies.set_events(Arc::new(move |event| seen.lock().unwrap().push(event)));
        Editing {
            remote,
            copies,
            events,
            root: tempfile::tempdir().unwrap(),
        }
    }

    impl Editing {
        fn kinds(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| match event {
                    EditEvent::Uploading { .. } => "uploading",
                    EditEvent::Uploaded { .. } => "uploaded",
                    EditEvent::Conflict { .. } => "conflict",
                    EditEvent::Failed { .. } => "failed",
                    EditEvent::Stopped { .. } => "stopped",
                })
                .collect()
        }

        fn count(&self, kind: &str) -> usize {
            self.kinds().iter().filter(|seen| **seen == kind).count()
        }

        async fn until(&self, kind: &str, count: usize) -> bool {
            for _ in 0..250 {
                if self.count(kind) >= count {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        }

        async fn edit(&self, path: &str) -> (String, PathBuf) {
            self.copies
                .edit(
                    self.remote.clone(),
                    &MemFs::path(path),
                    self.root.path(),
                    None,
                )
                .await
                .unwrap()
        }
    }

    /// Save the copy as an editor would: new contents, a later time.
    fn save(local: &Path, contents: &[u8], seconds_later: u64) {
        save_at(
            local,
            contents,
            SystemTime::now() + Duration::from_secs(seconds_later),
        );
    }

    fn save_at(local: &Path, contents: &[u8], when: SystemTime) {
        std::fs::write(local, contents).unwrap();
        std::fs::File::options()
            .write(true)
            .open(local)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[tokio::test]
    async fn each_save_is_uploaded_over_the_servers_file() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        save(&local, b"port=8080\n", 5);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert_eq!(e.remote.read("/srv/app.conf").unwrap(), b"port=8080\n");
        save(&local, b"port=9090\n", 10);
        assert!(e.until("uploaded", 2).await, "{:?}", e.kinds());
        assert_eq!(e.remote.read("/srv/app.conf").unwrap(), b"port=9090\n");
        assert_eq!(
            e.kinds(),
            ["uploading", "uploaded", "uploading", "uploaded"]
        );
        // No temporary upload files are left on the server.
        assert!(!e.remote.paths().iter().any(|path| path.contains("corepty")));
        assert!(e.copies.close(&[id]).await.is_empty());
        assert!(!local.exists());
    }

    #[tokio::test]
    async fn two_saves_in_one_second_with_the_same_size_are_both_uploaded() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        let second = UNIX_EPOCH
            + Duration::from_secs(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 30,
            );
        save_at(&local, b"port=81\n", second + Duration::from_millis(100));
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        save_at(&local, b"port=82\n", second + Duration::from_millis(600));
        assert!(e.until("uploaded", 2).await, "{:?}", e.kinds());
        assert_eq!(e.remote.read("/srv/app.conf").unwrap(), b"port=82\n");
        e.copies.close(&[id]).await;
    }

    #[tokio::test]
    async fn uploads_keep_the_files_permissions() {
        let e = editing();
        e.remote
            .set_mode(&MemFs::path("/srv/app.conf"), 0o600)
            .await
            .unwrap();
        let (id, local) = e.edit("/srv/app.conf").await;
        save(&local, b"secret=2\n", 5);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        let meta = e.remote.stat(&MemFs::path("/srv/app.conf")).await.unwrap();
        assert_eq!(meta.mode, Some(0o600), "a private file stays private");
        e.copies.close(&[id]).await;
    }

    #[tokio::test]
    async fn a_file_changed_on_the_server_asks_before_it_is_overwritten() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        // Someone else changes it on the server.
        e.remote.add_file("/srv/app.conf", b"port=81 # theirs\n");
        save(&local, b"port=8080 # mine\n", 5);
        assert!(e.until("conflict", 1).await, "{:?}", e.kinds());
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"port=81 # theirs\n"
        );
        // Upload mine.
        e.copies.resolve(&id, true);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"port=8080 # mine\n"
        );
        e.copies.close(&[id]).await;
    }

    #[tokio::test]
    async fn keeping_the_servers_version_stops_uploads_and_keeps_the_copy() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        e.remote.add_file("/srv/app.conf", b"theirs\n");
        save(&local, b"mine\n", 5);
        assert!(e.until("conflict", 1).await, "{:?}", e.kinds());
        e.copies.resolve(&id, false);
        assert!(e.until("stopped", 1).await, "{:?}", e.kinds());
        // A later save (an autosave, say) must not overwrite theirs either.
        save(&local, b"mine, saved again\n", 10);
        tokio::time::sleep(POLL * 5).await;
        assert_eq!(e.remote.read("/srv/app.conf").unwrap(), b"theirs\n");
        assert_eq!(e.count("uploaded"), 0);
        // Closing keeps the copy: the user's edits exist nowhere else.
        let kept = e.copies.close(&[id]).await;
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(std::fs::read(&local).unwrap(), b"mine, saved again\n");
    }

    #[tokio::test]
    async fn closing_while_asked_about_a_conflict_keeps_the_copy() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        e.remote.add_file("/srv/app.conf", b"theirs\n");
        save(&local, b"mine\n", 5);
        assert!(e.until("conflict", 1).await, "{:?}", e.kinds());
        let kept = e.copies.close(&[id]).await;
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(std::fs::read(&local).unwrap(), b"mine\n");
        assert_eq!(e.remote.read("/srv/app.conf").unwrap(), b"theirs\n");
    }

    #[tokio::test]
    async fn a_file_that_became_a_folder_on_the_server_is_never_replaced() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        e.remote
            .remove(&MemFs::path("/srv/app.conf"), false)
            .await
            .unwrap();
        e.remote.add_dir("/srv/app.conf");
        e.remote.add_file("/srv/app.conf/index.html", b"<html>");
        save(&local, b"mine\n", 5);
        assert!(e.until("stopped", 1).await, "{:?}", e.kinds());
        assert_eq!(
            e.remote.read("/srv/app.conf/index.html").unwrap(),
            b"<html>"
        );
        assert_eq!(e.copies.close(&[id]).await.len(), 1);
    }

    #[tokio::test]
    async fn closing_uploads_a_last_save_then_deletes_the_copy() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        save(&local, b"saved just before closing\n", 5);
        let kept = e.copies.close(&[id]).await;
        assert!(kept.is_empty(), "{kept:?}");
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"saved just before closing\n"
        );
        assert!(!local.exists());
    }

    /// A real sftp-server behind a reconnectable mount, as the app has it.
    async fn mounted(
        dir: &Path,
    ) -> Option<(Arc<crate::vfs::mount::SftpMount>, tokio::process::Child)> {
        let (sftp, server) = crate::sftp::testing::local_sftp(dir).await?;
        Some((crate::vfs::mount::SftpMount::new(sftp), server))
    }

    async fn site(mount: &Arc<crate::vfs::mount::SftpMount>) -> VPath {
        mount
            .home()
            .await
            .unwrap()
            .join(&VName::Posix(b"site.conf".to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn a_save_that_could_not_be_uploaded_goes_up_after_a_reconnect() {
        let server_dir = tempfile::tempdir().unwrap();
        std::fs::write(server_dir.path().join("site.conf"), b"listen 80;\n").unwrap();
        let Some((mount, mut server)) = mounted(server_dir.path()).await else {
            return;
        };
        let remote = site(&mount).await;
        let e = editing();
        let (id, local) = e
            .copies
            .edit(mount.clone(), &remote, e.root.path(), None)
            .await
            .unwrap();
        // The connection drops; the user saves anyway.
        server.kill().await.unwrap();
        for _ in 0..100 {
            if !mount.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        save(&local, b"listen 8080;\n", 5);
        assert!(e.until("failed", 1).await, "{:?}", e.kinds());
        // Retried quietly while down: one message, not one per try.
        tokio::time::sleep(RETRY * 3).await;
        assert_eq!(e.count("failed"), 1, "{:?}", e.kinds());
        // Back: the save goes up without the user saving again.
        let (second, _server) = crate::sftp::testing::local_sftp(server_dir.path())
            .await
            .unwrap();
        mount.replace(second);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert_eq!(
            std::fs::read(server_dir.path().join("site.conf")).unwrap(),
            b"listen 8080;\n"
        );
        assert!(e.copies.close(&[id]).await.is_empty());
    }

    #[tokio::test]
    async fn closing_keeps_a_copy_whose_save_is_not_on_the_server() {
        let server_dir = tempfile::tempdir().unwrap();
        std::fs::write(server_dir.path().join("site.conf"), b"listen 80;\n").unwrap();
        let Some((mount, mut server)) = mounted(server_dir.path()).await else {
            return;
        };
        let remote = site(&mount).await;
        let e = editing();
        let (id, local) = e
            .copies
            .edit(mount.clone(), &remote, e.root.path(), None)
            .await
            .unwrap();
        server.kill().await.unwrap();
        save(&local, b"listen 8080;\n", 5);
        assert!(e.until("failed", 1).await, "{:?}", e.kinds());
        let kept = e.copies.close(&[id]).await;
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].path, local.to_string_lossy());
        assert_eq!(std::fs::read(&local).unwrap(), b"listen 8080;\n");
    }

    #[tokio::test]
    async fn editing_over_a_real_sftp_server_uploads_saves() {
        let server_dir = tempfile::tempdir().unwrap();
        std::fs::write(server_dir.path().join("site.conf"), b"listen 80;\n").unwrap();
        let Some((sftp, _server)) = crate::sftp::testing::local_sftp(server_dir.path()).await
        else {
            return;
        };
        let home = sftp.home().await.unwrap();
        let remote = home.join(&VName::Posix(b"site.conf".to_vec())).unwrap();
        let e = editing();
        let (id, local) = e
            .copies
            .edit(Arc::new(sftp), &remote, e.root.path(), None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&local).unwrap(), b"listen 80;\n");
        save(&local, b"listen 8080;\n", 5);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert_eq!(
            std::fs::read(server_dir.path().join("site.conf")).unwrap(),
            b"listen 8080;\n"
        );
        let leftovers = std::fs::read_dir(server_dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "only the file itself");
        e.copies.close(&[id]).await;
    }

    /// Saves through a link go to the file it points at; the link stays.
    #[cfg(unix)]
    #[tokio::test]
    async fn editing_through_a_link_updates_the_linked_file() {
        let server_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(server_dir.path().join("available")).unwrap();
        std::fs::write(server_dir.path().join("available/site"), b"listen 80;\n").unwrap();
        std::os::unix::fs::symlink("available/site", server_dir.path().join("enabled")).unwrap();
        let Some((sftp, _server)) = crate::sftp::testing::local_sftp(server_dir.path()).await
        else {
            return;
        };
        let home = sftp.home().await.unwrap();
        let link = home.join(&VName::Posix(b"enabled".to_vec())).unwrap();
        let e = editing();
        let (id, local) = e
            .copies
            .edit(Arc::new(sftp), &link, e.root.path(), None)
            .await
            .unwrap();
        save(&local, b"listen 8080;\n", 5);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert!(std::fs::symlink_metadata(server_dir.path().join("enabled"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read(server_dir.path().join("available/site")).unwrap(),
            b"listen 8080;\n"
        );
        e.copies.close(&[id]).await;
    }

    #[test]
    fn only_types_known_to_be_safe_open_without_asking() {
        for name in [
            "deploy.bat",
            "SETUP.EXE",
            "x.ps1",
            "app.hta",
            "link.lnk",
            "install.sh",
            "start.desktop",
            "x.jar",
            "notes.txt.exe",
            "trailing.exe.",
            // Types an interpreter or a client runs, found missing from a deny list.
            "module.pyc",
            "fix.diagcab",
            "office.rdp",
            "macros.docm",
            "old.doc",
            // No extension: whatever the system makes of it.
            "Makefile",
            // Names that read as something else.
            "invoice\u{202E}fdp.exe",
            "report\u{202E}.txt",
            "arabic\u{061C}mark.txt",
        ] {
            assert!(runs_code(name), "{name}");
        }
        for name in [
            "notes.txt",
            "photo.JPG",
            "config.yml",
            "archive.tar.gz",
            "report.pdf",
            "sheet.xlsx",
            "trailing.txt. ",
        ] {
            assert!(!runs_code(name), "{name}");
        }
    }

    #[tokio::test]
    async fn an_upload_cut_off_in_place_is_redone_without_asking() {
        let e = editing();
        // The server refuses setting modes: uploads write the file in place
        // (which keeps its mode).
        e.remote.refuse_set_mode(true);
        let (id, local) = e.edit("/srv/app.conf").await;
        // The connection drops half-way through writing the server's file.
        e.remote.fail_next_write_midway();
        save(&local, b"port=8080 # all of it\n", 5);
        assert!(e.until("failed", 1).await, "{:?}", e.kinds());
        // The retry puts the whole file there; the half-written one isn't
        // taken for someone else's change.
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert_eq!(e.count("conflict"), 0, "{:?}", e.kinds());
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"port=8080 # all of it\n"
        );
        assert!(e.copies.close(&[id]).await.is_empty());
    }

    #[tokio::test]
    async fn a_failing_upload_is_retried_on_the_timer_not_on_every_check() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        e.remote.refuse_writes(true);
        save(&local, b"port=8080\n", 5);
        assert!(e.until("failed", 1).await, "{:?}", e.kinds());
        let window = RETRY * 4;
        tokio::time::sleep(window).await;
        // The first try and about one per RETRY; checks come every POLL.
        let tries = e.count("uploading");
        assert!(tries <= 6, "{tries} uploads in {window:?}: {:?}", e.kinds());
        assert_eq!(e.count("failed"), 1);
        e.remote.refuse_writes(false);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        e.copies.close(&[id]).await;
    }

    #[tokio::test]
    async fn a_save_counts_as_pending_the_moment_it_lands() {
        let e = editing();
        let (id, local) = e.edit("/srv/app.conf").await;
        assert!(e.copies.unsynced().is_empty());
        save(&local, b"port=8080\n", 5);
        // Asked right away (quitting, say), before the watcher's next check.
        let pending = e.copies.unsynced();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].name, "app.conf");
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        for _ in 0..50 {
            if e.copies.unsynced().is_empty() {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        assert!(e.copies.unsynced().is_empty());
        e.copies.close(&[id]).await;
    }

    #[tokio::test]
    async fn a_copy_opened_with_enter_and_changed_is_kept() {
        let remote = MemFs::new();
        remote.add_dir("/srv");
        remote.add_file("/srv/report.docx", b"original");
        let root = tempfile::tempdir().unwrap();
        let copies = OpenedCopies::default();
        let (id, local, _) = copies
            .download(&remote, &MemFs::path("/srv/report.docx"), root.path())
            .await
            .unwrap();
        save(&local, b"edited in the app", 5);
        let kept = copies.close(&[id]).await;
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(std::fs::read(&local).unwrap(), b"edited in the app");
    }

    #[test]
    fn bare_program_names_are_found_with_windows_extensions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.cmd"), b"@echo off").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            find_program("code", &path, ".COM;.EXE;.BAT;.CMD"),
            Some(dir.path().join("code.cmd"))
        );
        assert_eq!(find_program("missing", &path, ".EXE;.CMD"), None);
        // With a folder or an extension, the name is used as given.
        assert_eq!(find_program("code.cmd", &path, ".CMD"), None);
        assert_eq!(find_program("bin/code", &path, ".CMD"), None);
    }

    #[test]
    fn editor_command_lines_split_and_place_the_file() {
        let file = Path::new("/tmp/x/app.conf");
        assert_eq!(
            editor_command("code --wait", file),
            Some((
                "code".into(),
                vec!["--wait".into(), "/tmp/x/app.conf".into()]
            ))
        );
        assert_eq!(
            editor_command(
                r#""C:\Program Files\Notepad++\notepad++.exe" -multiInst"#,
                file
            ),
            Some((
                r"C:\Program Files\Notepad++\notepad++.exe".into(),
                vec!["-multiInst".into(), "/tmp/x/app.conf".into()]
            ))
        );
        assert_eq!(
            editor_command("subl {file}:1", file),
            Some(("subl".into(), vec!["/tmp/x/app.conf:1".into()]))
        );
        assert_eq!(editor_command("   ", file), None);
    }
}

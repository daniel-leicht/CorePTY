//! Opening remote files in local apps (WinSCP style). A copy is downloaded
//! into its own temp folder and opened there. When editing, the copy is
//! watched: each save is uploaded back, after checking that the file on the
//! server hasn't changed since (if it has, the user decides). The folder goes
//! when the file manager tab that opened it closes, after a last upload.

use crate::vfs::{epoch_millis, FsRegistry, Location, Meta, VName, VPath, Vfs, WriteMode};
use serde::Serialize;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::{Duration, SystemTime},
};
use tauri::{AppHandle, Emitter, State};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, watch},
};

/// How often an edited copy is checked for saves.
const POLL: Duration = Duration::from_millis(if cfg!(test) { 40 } else { 1000 });

/// Size and modified time (to the second, as SFTP keeps it): what tells a
/// changed file from an unchanged one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: Option<u64>,
    seconds: Option<u64>,
}

impl Stamp {
    fn of(meta: &Meta) -> Self {
        Self {
            size: meta.size,
            seconds: meta.modified.and_then(seconds),
        }
    }

    fn of_local(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            size: Some(metadata.len()),
            seconds: metadata.modified().ok().and_then(seconds),
        })
    }
}

fn seconds(time: SystemTime) -> Option<u64> {
    epoch_millis(time).map(|millis| millis / 1000)
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
    Failed {
        id: String,
        name: String,
        message: String,
    },
}

pub type EditSink = Arc<dyn Fn(EditEvent) + Send + Sync>;

/// A downloaded copy of a remote file.
struct Copy {
    dir: PathBuf,
    /// Set while it's being edited.
    watcher: Option<Watcher>,
}

struct Watcher {
    stop: watch::Sender<bool>,
    answers: mpsc::UnboundedSender<bool>,
}

/// Tauri-managed state: the copies opened this session, by id.
#[derive(Default)]
pub struct OpenedCopies {
    copies: Mutex<HashMap<String, Copy>>,
    events: RwLock<Option<EditSink>>,
}

impl OpenedCopies {
    fn copies(&self) -> MutexGuard<'_, HashMap<String, Copy>> {
        self.copies.lock().unwrap_or_else(|e| e.into_inner())
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
        let name = name.storable_here().unwrap_or(name);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = root.join(&id);
        tokio::fs::create_dir_all(&dir).await?;
        let local = dir.join(&*name.to_native());
        match download(fs, path, &local).await {
            Ok(meta) => {
                self.copies()
                    .insert(id.clone(), Copy { dir, watcher: None });
                Ok((id, local, meta))
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                Err(e)
            }
        }
    }

    /// Download `path` and keep uploading the copy's saves back to it.
    async fn edit(
        &self,
        fs: Arc<dyn Vfs>,
        path: &VPath,
        root: &Path,
    ) -> io::Result<(String, PathBuf)> {
        let (id, local, meta) = self.download(fs.as_ref(), path, root).await?;
        let (stop, stopped) = watch::channel(false);
        let (answers, answered) = mpsc::unbounded_channel();
        let session = Session {
            id: id.clone(),
            name: path
                .file_name()
                .map(|name| name.display())
                .unwrap_or_default(),
            fs,
            remote: path.clone(),
            local: local.clone(),
            remote_stamp: Stamp::of(&meta),
            local_stamp: Stamp::of_local(&local),
            sink: self.sink(),
        };
        let dir = match self.copies().get_mut(&id) {
            Some(copy) => {
                copy.watcher = Some(Watcher { stop, answers });
                copy.dir.clone()
            }
            None => return Err(io::Error::other("the copy went away")),
        };
        tokio::spawn(session.watch(dir, stopped, answered));
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

    /// Delete copies (their apps may still have them open: best effort). An
    /// edited one uploads a last unsaved change first, then goes.
    fn close(&self, ids: &[String]) {
        let removed: Vec<Copy> = {
            let mut copies = self.copies();
            ids.iter().filter_map(|id| copies.remove(id)).collect()
        };
        for copy in removed {
            match copy.watcher {
                Some(watcher) => {
                    watcher.stop.send_replace(true);
                }
                None => {
                    let _ = std::fs::remove_dir_all(&copy.dir);
                }
            }
        }
    }
}

/// One edited copy, watched for saves.
struct Session {
    id: String,
    name: String,
    fs: Arc<dyn Vfs>,
    remote: VPath,
    local: PathBuf,
    /// The server's file as last downloaded or uploaded.
    remote_stamp: Stamp,
    /// The copy as last synced.
    local_stamp: Option<Stamp>,
    sink: EditSink,
}

impl Session {
    async fn watch(
        mut self,
        dir: PathBuf,
        mut stop: watch::Receiver<bool>,
        mut answers: mpsc::UnboundedReceiver<bool>,
    ) {
        loop {
            let stopping = tokio::select! {
                _ = tokio::time::sleep(POLL) => false,
                _ = stop.wait_for(|stop| *stop) => true,
            };
            if let Some(now) = self.saved() {
                // Wait for the editor to finish writing before uploading.
                tokio::time::sleep(POLL / 2).await;
                if Stamp::of_local(&self.local) == Some(now) {
                    self.sync(now, &mut answers, &mut stop).await;
                }
            }
            if stopping || *stop.borrow() {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return;
            }
        }
    }

    /// The copy's stamp, if it changed since the last sync.
    fn saved(&self) -> Option<Stamp> {
        let now = Stamp::of_local(&self.local)?;
        (Some(now) != self.local_stamp).then_some(now)
    }

    async fn sync(
        &mut self,
        now: Stamp,
        answers: &mut mpsc::UnboundedReceiver<bool>,
        stop: &mut watch::Receiver<bool>,
    ) {
        let (id, name) = (self.id.clone(), self.name.clone());
        let on_server = match self.fs.stat(&self.remote).await {
            Ok(meta) => Some(Stamp::of(&meta)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                self.local_stamp = Some(now);
                (self.sink)(EditEvent::Failed {
                    id,
                    name,
                    message: format!("couldn't check the file on the server: {e}"),
                });
                return;
            }
        };
        if on_server != Some(self.remote_stamp) {
            // Changed (or deleted) on the server since it was opened.
            (self.sink)(EditEvent::Conflict {
                id: id.clone(),
                name: name.clone(),
            });
            while answers.try_recv().is_ok() {}
            let overwrite = tokio::select! {
                answer = answers.recv() => answer.unwrap_or(false),
                // Closing the tab while asked: keep the server's file.
                _ = stop.wait_for(|stop| *stop) => false,
            };
            if !overwrite {
                self.local_stamp = Some(now);
                if let Some(stamp) = on_server {
                    self.remote_stamp = stamp;
                }
                return;
            }
        }
        (self.sink)(EditEvent::Uploading {
            id: id.clone(),
            name: name.clone(),
        });
        self.local_stamp = Some(now);
        match upload(self.fs.as_ref(), &self.local, &self.remote).await {
            Ok(stamp) => {
                self.remote_stamp = stamp;
                (self.sink)(EditEvent::Uploaded { id, name });
            }
            Err(e) => (self.sink)(EditEvent::Failed {
                id,
                name,
                message: e.to_string(),
            }),
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

/// Upload the copy over the server's file in one step: written beside it
/// under a temporary name, then renamed over it (a failure leaves the old one).
async fn upload(fs: &dyn Vfs, local: &Path, remote: &VPath) -> io::Result<Stamp> {
    let folder = remote.parent().unwrap_or_else(|| remote.clone());
    let temporary = folder.join(&VName::for_path(
        &folder,
        &format!(".corepty-{}.part", uuid::Uuid::new_v4().simple()),
    ))?;
    let written = async {
        let mut source = tokio::fs::File::open(local).await?;
        let mut writer = fs.open_write(&temporary, WriteMode::CreateNew).await?;
        let mut buffer = vec![0; 256 * 1024];
        loop {
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read]).await?;
        }
        writer.shutdown().await?;
        if let Ok(modified) = std::fs::metadata(local).and_then(|m| m.modified()) {
            let _ = fs.set_times(&temporary, modified, None).await;
        }
        fs.rename(&temporary, remote, true).await
    }
    .await;
    if let Err(e) = written {
        let _ = fs.remove(&temporary, false).await;
        return Err(e);
    }
    Ok(Stamp::of(&fs.stat(remote).await?))
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

/// Open `file` in `editor` (a command line), or with the default app.
fn launch(editor: Option<&str>, file: &Path) -> io::Result<()> {
    match editor.map(str::trim).filter(|line| !line.is_empty()) {
        None => open::that_detached(file),
        Some(line) => {
            let (program, args) = editor_command(line, file)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no editor given"))?;
            std::process::Command::new(&program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map(drop)
                .map_err(|e| io::Error::new(e.kind(), format!("couldn't start {program}: {e}")))
        }
    }
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

async fn open_local(editor: Option<String>, local: PathBuf) -> Result<(), String> {
    let shown = local.clone();
    tokio::task::spawn_blocking(move || launch(editor.as_deref(), &local))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not open {}: {e}", shown.to_string_lossy()))
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
    open_local(None, local.clone()).await?;
    Ok(CopyView {
        id,
        path: local.to_string_lossy().into_owned(),
    })
}

/// Edit a remote file (F4): download it, open it in `editor` (blank: the
/// default app), and upload every save (`fs://edit` events report it).
#[tauri::command]
pub async fn fs_edit(
    registry: State<'_, FsRegistry>,
    copies: State<'_, OpenedCopies>,
    location: Location,
    editor: Option<String>,
) -> Result<CopyView, String> {
    let fs = registry.get(&location.fs)?;
    let (id, local) = copies
        .edit(fs, &location.path, &copies_root())
        .await
        .map_err(|e| format!("could not download {}: {e}", location.path.display()))?;
    if let Err(e) = open_local(editor, local.clone()).await {
        copies.close(std::slice::from_ref(&id));
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

#[tauri::command]
pub fn fs_close_copies(copies: State<'_, OpenedCopies>, ids: Vec<String>) {
    copies.close(&ids);
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
                })
                .collect()
        }

        async fn until(&self, kind: &str, count: usize) -> bool {
            for _ in 0..200 {
                if self.kinds().iter().filter(|seen| **seen == kind).count() >= count {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        }
    }

    /// Save the copy as an editor would: new contents, a later time.
    fn save(local: &Path, contents: &[u8], seconds_later: u64) {
        std::fs::write(local, contents).unwrap();
        let later = SystemTime::now() + Duration::from_secs(seconds_later);
        std::fs::File::options()
            .write(true)
            .open(local)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }

    #[tokio::test]
    async fn each_save_is_uploaded_over_the_servers_file() {
        let e = editing();
        let (id, local) = e
            .copies
            .edit(
                e.remote.clone(),
                &MemFs::path("/srv/app.conf"),
                e.root.path(),
            )
            .await
            .unwrap();
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
        e.copies.close(&[id]);
        tokio::time::sleep(POLL * 4).await;
        assert!(!local.exists());
    }

    #[tokio::test]
    async fn a_file_changed_on_the_server_asks_before_it_is_overwritten() {
        let e = editing();
        let (id, local) = e
            .copies
            .edit(
                e.remote.clone(),
                &MemFs::path("/srv/app.conf"),
                e.root.path(),
            )
            .await
            .unwrap();
        // Someone else changes it on the server.
        e.remote.add_file("/srv/app.conf", b"port=81 # theirs\n");
        save(&local, b"port=8080 # mine\n", 5);
        assert!(e.until("conflict", 1).await, "{:?}", e.kinds());
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"port=81 # theirs\n"
        );
        // Keep theirs: nothing is uploaded.
        e.copies.resolve(&id, false);
        tokio::time::sleep(POLL * 4).await;
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"port=81 # theirs\n"
        );
        // The next save no longer clashes with theirs (it was seen): it goes up.
        save(&local, b"port=8080 # mine, again\n", 10);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"port=8080 # mine, again\n"
        );

        // Another outside change, this time overwritten.
        e.remote.add_file("/srv/app.conf", b"theirs again\n");
        save(&local, b"mine wins\n", 15);
        assert!(e.until("conflict", 2).await, "{:?}", e.kinds());
        e.copies.resolve(&id, true);
        assert!(e.until("uploaded", 2).await, "{:?}", e.kinds());
        assert_eq!(e.remote.read("/srv/app.conf").unwrap(), b"mine wins\n");
        e.copies.close(&[id]);
    }

    #[tokio::test]
    async fn closing_uploads_a_last_save_then_deletes_the_copy() {
        let e = editing();
        let (id, local) = e
            .copies
            .edit(
                e.remote.clone(),
                &MemFs::path("/srv/app.conf"),
                e.root.path(),
            )
            .await
            .unwrap();
        save(&local, b"saved just before closing\n", 5);
        e.copies.close(&[id]);
        assert!(e.until("uploaded", 1).await, "{:?}", e.kinds());
        assert_eq!(
            e.remote.read("/srv/app.conf").unwrap(),
            b"saved just before closing\n"
        );
        for _ in 0..50 {
            if !local.exists() {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        assert!(!local.exists());
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
            .edit(Arc::new(sftp), &remote, e.root.path())
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
        e.copies.close(&[id]);
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

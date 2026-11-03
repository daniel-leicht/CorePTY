//! Integration tests against a real sshd, run by hand (they're `#[ignore]`):
//!
//! ```text
//! COREPTY_SSHD=127.0.0.1:22 COREPTY_SSHD_USER=me COREPTY_SSHD_KEY=~/.ssh/id_ed25519 \
//! COREPTY_SSHD_DIR=/tmp/corepty-tests cargo test --lib sftp::real_server -- --ignored
//! ```
//!
//! Each test works in its own new folder under `COREPTY_SSHD_DIR` and removes
//! it afterwards. What a server can't do (links on a Windows host, names that
//! aren't UTF-8 on one that only stores Unicode) is reported and skipped.

use super::testing::{real_sshd, RealServer};
use crate::connections::{ConnectionRegistry, HostKeys, SshAuth, SshTarget};
use crate::transfer::engine::{
    tests::{self as engine, Scripted},
    Resolution, Side, TransferKind,
};
use crate::vfs::{memory::MemFs, sftp::SftpFs, EntryKind, FsId, VName, VPath, Vfs, WriteMode};
use std::{
    fs,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Remote {
    _registry: ConnectionRegistry,
    _keys: tempfile::TempDir,
    fs: Arc<SftpFs>,
    /// This test's own folder on the server.
    dir: Vec<u8>,
}

impl Remote {
    fn at(&self, name: &str) -> VPath {
        let mut path = self.dir.clone();
        path.push(b'/');
        path.extend_from_slice(name.as_bytes());
        VPath::Posix(path)
    }

    fn root(&self) -> VPath {
        VPath::Posix(self.dir.clone())
    }

    fn side(&self) -> Side {
        Side {
            id: FsId("sftp:real".to_string()),
            fs: self.fs.clone(),
        }
    }

    async fn clean_up(self) {
        let _ = self.fs.remove(&self.root(), true).await;
    }
}

/// Log in and make a fresh folder for one test (`None`: no server configured).
async fn remote(test: &str) -> Option<Remote> {
    let Some(server) = real_sshd() else {
        eprintln!(
            "skipped: set COREPTY_SSHD, COREPTY_SSHD_USER, COREPTY_SSHD_KEY, COREPTY_SSHD_DIR"
        );
        return None;
    };
    let RealServer {
        host,
        port,
        user,
        key,
        dir,
    } = server;
    let keys = tempfile::tempdir().unwrap();
    let registry = ConnectionRegistry::default();
    let target = SshTarget {
        host,
        port,
        username: user,
        auth: SshAuth::Key {
            path: key,
            passphrase: None,
        },
    };
    let host_keys = HostKeys {
        path: Some(keys.path().join("known_hosts")),
    };
    let lease = registry
        .connect(test.to_string(), &target, &host_keys, &|_| {})
        .await
        .expect("log in to the test server");
    let fs = Arc::new(SftpFs::over_connection(lease).await.expect("start SFTP"));
    let mut folder = dir;
    folder
        .extend_from_slice(format!("/corepty-{test}-{}", uuid::Uuid::new_v4().simple()).as_bytes());
    fs.mkdir(&VPath::Posix(folder.clone()))
        .await
        .expect("create the test folder (does COREPTY_SSHD_DIR exist?)");
    Some(Remote {
        _registry: registry,
        _keys: keys,
        fs,
        dir: folder,
    })
}

fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|i| (i * 13 % 251) as u8).collect()
}

async fn read_remote(fs: &SftpFs, path: &VPath) -> Vec<u8> {
    let mut data = Vec::new();
    fs.open_read(path, 0)
        .await
        .unwrap()
        .read_to_end(&mut data)
        .await
        .unwrap();
    data
}

#[tokio::test]
#[ignore = "needs a real sshd (COREPTY_SSHD…)"]
async fn real_list_upload_and_download_keep_contents_and_times() {
    let Some(remote) = remote("updown").await else {
        return;
    };
    let local = tempfile::tempdir().unwrap();
    let project = local.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/main.rs"), pattern(3 * 1024 * 1024 + 7)).unwrap();
    fs::write(project.join("notes.txt"), b"notes").unwrap();
    let old = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::options()
        .write(true)
        .open(project.join("notes.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();

    let up = engine::run(
        TransferKind::Copy,
        engine::local(),
        remote.side(),
        &[VPath::native(&project)],
        &remote.root(),
        engine::no_conflicts(),
    )
    .await;
    assert_eq!(up.result, Ok(()));
    let listed = remote.fs.list(&remote.at("project")).await.unwrap();
    let mut names: Vec<_> = listed.iter().map(|e| e.name.display()).collect();
    names.sort();
    assert_eq!(names, ["notes.txt", "src"]);
    let notes = listed
        .iter()
        .find(|e| e.name.display() == "notes.txt")
        .unwrap();
    assert_eq!(notes.meta.size, Some(5));
    assert_eq!(notes.meta.modified, Some(old));
    assert!(notes.meta.owner.is_some(), "owner names from the listing");

    let back = local.path().join("back");
    fs::create_dir(&back).unwrap();
    let down = engine::run(
        TransferKind::Copy,
        remote.side(),
        engine::local(),
        &[remote.at("project")],
        &VPath::native(&back),
        engine::no_conflicts(),
    )
    .await;
    assert_eq!(down.result, Ok(()));
    assert_eq!(
        fs::read(back.join("project/src/main.rs")).unwrap(),
        pattern(3 * 1024 * 1024 + 7)
    );
    assert_eq!(
        fs::metadata(back.join("project/notes.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        old
    );
    remote.clean_up().await;
}

#[tokio::test]
#[ignore = "needs a real sshd (COREPTY_SSHD…)"]
async fn real_an_interrupted_upload_resumes() {
    let Some(remote) = remote("resume").await else {
        return;
    };
    let source = Arc::new(MemFs::new());
    let data = pattern(4 * 1024 * 1024 + 3);
    source.add_dir("/up");
    source.add_file("/up/disk.img", &data);
    let sources = [MemFs::path("/up/disk.img")];
    let root = remote.root();
    let upload = |resolver: Arc<Scripted>| {
        engine::run(
            TransferKind::Copy,
            engine::memory(&source, "memory"),
            remote.side(),
            &sources,
            &root,
            resolver,
        )
    };
    source.fail_next_read_midway();
    assert!(upload(engine::no_conflicts()).await.result.is_err());
    let partial = remote
        .fs
        .stat(&remote.at("disk.img.filepart"))
        .await
        .unwrap();
    let have = partial.size.unwrap();
    assert!(have > 0 && have < data.len() as u64, "{have}");

    let resolver = Scripted::new(&[(Resolution::Resume, false)]);
    let resumed = upload(resolver.clone()).await;
    assert_eq!(resumed.result, Ok(()));
    assert!(resolver.asked.lock().unwrap()[0].partial);
    assert_eq!(read_remote(&remote.fs, &remote.at("disk.img")).await, data);
    assert!(remote
        .fs
        .stat(&remote.at("disk.img.filepart"))
        .await
        .is_err());
    remote.clean_up().await;
}

#[tokio::test]
#[ignore = "needs a real sshd (COREPTY_SSHD…)"]
async fn real_conflicts_are_asked_and_answered() {
    let Some(remote) = remote("conflicts").await else {
        return;
    };
    let local = tempfile::tempdir().unwrap();
    let file = local.path().join("report.txt");
    fs::write(&file, b"first").unwrap();
    let (sources, root) = ([VPath::native(&file)], remote.root());
    let copy = |resolver: Arc<Scripted>| {
        engine::run(
            TransferKind::Copy,
            engine::local(),
            remote.side(),
            &sources,
            &root,
            resolver,
        )
    };
    assert_eq!(copy(engine::no_conflicts()).await.result, Ok(()));
    fs::write(&file, b"second").unwrap();
    // Keep both, then overwrite.
    let keep_both = Scripted::new(&[(Resolution::Rename { name: None }, false)]);
    assert_eq!(copy(keep_both.clone()).await.result, Ok(()));
    assert_eq!(keep_both.asked.lock().unwrap().len(), 1);
    assert_eq!(
        read_remote(&remote.fs, &remote.at("report.txt")).await,
        b"first"
    );
    assert_eq!(
        read_remote(&remote.fs, &remote.at("report (copy).txt")).await,
        b"second"
    );
    let overwrite = Scripted::new(&[(Resolution::Overwrite, false)]);
    assert_eq!(copy(overwrite).await.result, Ok(()));
    assert_eq!(
        read_remote(&remote.fs, &remote.at("report.txt")).await,
        b"second"
    );
    remote.clean_up().await;
}

#[tokio::test]
#[ignore = "needs a real sshd (COREPTY_SSHD…)"]
async fn real_chmod_rename_and_recursive_delete() {
    let Some(remote) = remote("chmod").await else {
        return;
    };
    let file = remote.at("tool.sh");
    let mut writer = remote
        .fs
        .open_write(&file, WriteMode::CreateNew)
        .await
        .unwrap();
    writer.write_all(b"#!/bin/sh\n").await.unwrap();
    writer.shutdown().await.unwrap();

    remote.fs.set_mode(&file, 0o444).await.unwrap();
    assert!(remote.fs.stat(&file).await.unwrap().readonly);
    let changed = crate::vfs::commands::chmod(remote.fs.as_ref(), &file, 0o750, false)
        .await
        .unwrap();
    assert_eq!(changed, 1);
    let mode = remote.fs.stat(&file).await.unwrap().mode.unwrap();
    assert_ne!(mode & 0o200, 0, "writable again: {mode:o}");

    remote.fs.mkdir(&remote.at("tree")).await.unwrap();
    remote.fs.mkdir(&remote.at("tree/deeper")).await.unwrap();
    remote
        .fs
        .rename(&file, &remote.at("tree/deeper/tool.sh"), false)
        .await
        .unwrap();
    assert!(remote.fs.stat(&file).await.is_err());
    remote.fs.remove(&remote.at("tree"), true).await.unwrap();
    assert!(remote.fs.stat(&remote.at("tree")).await.is_err());
    remote.clean_up().await;
}

#[tokio::test]
#[ignore = "needs a real sshd (COREPTY_SSHD…)"]
async fn real_symlinks_are_listed_by_what_they_point_at() {
    let Some(remote) = remote("links").await else {
        return;
    };
    remote.fs.mkdir(&remote.at("folder")).await.unwrap();
    if let Err(e) = remote
        .fs
        .symlink(&remote.at("folder"), &remote.at("to-folder"), true)
        .await
    {
        eprintln!("skipped: this server can't create links ({e})");
        remote.clean_up().await;
        return;
    }
    let listed = remote.fs.list(&remote.root()).await.unwrap();
    let link = listed
        .iter()
        .find(|e| e.name.display() == "to-folder")
        .unwrap();
    if link.meta.kind != EntryKind::Symlink {
        // Git for Windows' msys sshd copies the folder when Windows won't
        // let it make a link.
        eprintln!(
            "skipped: this server made a {:?} instead of a link",
            link.meta.kind
        );
        remote.clean_up().await;
        return;
    }
    assert!(link.meta.is_directory);
    assert_eq!(
        remote.fs.read_link(&remote.at("to-folder")).await.unwrap(),
        remote.at("folder")
    );
    // Removing the link leaves the folder.
    remote
        .fs
        .remove(&remote.at("to-folder"), true)
        .await
        .unwrap();
    assert!(
        remote
            .fs
            .stat(&remote.at("folder"))
            .await
            .unwrap()
            .is_directory
    );
    remote.clean_up().await;
}

#[tokio::test]
#[ignore = "needs a real sshd (COREPTY_SSHD…)"]
async fn real_names_that_are_not_utf8_round_trip() {
    let Some(remote) = remote("bytes").await else {
        return;
    };
    let mut name = remote.dir.clone();
    name.extend_from_slice(b"/caf\xe9.txt");
    let path = VPath::Posix(name);
    let created = match remote.fs.open_write(&path, WriteMode::CreateNew).await {
        Ok(mut writer) => {
            writer.write_all(b"latin-1").await.is_ok() && writer.shutdown().await.is_ok()
        }
        Err(_) => false,
    };
    let listed = remote.fs.list(&remote.root()).await.unwrap();
    let kept = listed
        .iter()
        .any(|e| e.name == VName::Posix(b"caf\xe9.txt".to_vec()));
    if !created || !kept {
        eprintln!(
            "skipped: this server doesn't keep names that aren't UTF-8 (a Unicode-only filesystem)"
        );
        remote.clean_up().await;
        return;
    }
    assert_eq!(read_remote(&remote.fs, &path).await, b"latin-1");
    let mut renamed = remote.dir.clone();
    renamed.extend_from_slice(b"/\xff\xfe");
    remote
        .fs
        .rename(&path, &VPath::Posix(renamed.clone()), false)
        .await
        .unwrap();
    assert!(remote.fs.stat(&VPath::Posix(renamed)).await.is_ok());
    remote.clean_up().await;
}

#[tokio::test]
#[ignore = "needs a real sshd (COREPTY_SSHD…)"]
async fn real_set_times_reach_the_server() {
    let Some(remote) = remote("times").await else {
        return;
    };
    remote.fs.mkdir(&remote.at("dated")).await.unwrap();
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567_890);
    remote
        .fs
        .set_times(&remote.at("dated"), when, None)
        .await
        .unwrap();
    assert_eq!(
        remote.fs.stat(&remote.at("dated")).await.unwrap().modified,
        Some(when)
    );
    remote.clean_up().await;
}

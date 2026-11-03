//! Test support: a real OpenSSH `sftp-server`, spoken to over its stdio.
//!
//! Every CI runner has one (Git for Windows ships it; Linux and macOS have
//! OpenSSH), so the client is tested against the implementation nearly every
//! real server runs. Set `COREPTY_SFTP_SERVER` to point at another binary.
//! Without one, the tests skip locally but fail on CI.

use crate::vfs::sftp::SftpFs;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};

pub fn sftp_server() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("COREPTY_SFTP_SERVER") {
        return Some(PathBuf::from(path));
    }
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files\Git\usr\lib\ssh\sftp-server.exe",
            r"C:\Program Files (x86)\Git\usr\lib\ssh\sftp-server.exe",
        ]
    } else {
        &[
            "/usr/lib/openssh/sftp-server",
            "/usr/libexec/openssh/sftp-server",
            "/usr/libexec/sftp-server",
            "/usr/lib/ssh/sftp-server",
            "/usr/local/libexec/sftp-server",
        ]
    };
    let found = candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file());
    if found.is_none() {
        assert!(
            std::env::var_os("CI").is_none(),
            "no OpenSSH sftp-server found; set COREPTY_SFTP_SERVER"
        );
        eprintln!("skipped: no OpenSSH sftp-server found (set COREPTY_SFTP_SERVER)");
    }
    found
}

/// Start `sftp-server` in `dir` (its home, as far as SFTP is concerned).
pub fn spawn_server(binary: &Path, dir: &Path) -> Child {
    let mut command = Command::new(binary);
    command
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Git's msys build needs its runtime DLLs from `usr/bin`.
    if let Some(bin) = binary
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|usr| usr.join("bin"))
        .filter(|bin| bin.join("msys-2.0.dll").is_file())
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&path));
        command.env("PATH", std::env::join_paths(paths).unwrap());
    }
    command.spawn().expect("start sftp-server")
}

/// The server's pipes as one bidirectional stream.
pub fn stdio(child: &mut Child) -> impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send {
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    tokio::io::join(stdout, stdin)
}

/// An [`SftpFs`] over a local `sftp-server` rooted in `dir`, or `None` (skip)
/// where there is no server.
pub async fn local_sftp(dir: &Path) -> Option<(SftpFs, Child)> {
    let binary = sftp_server()?;
    let mut child = spawn_server(&binary, dir);
    let fs = SftpFs::start(stdio(&mut child), None)
        .await
        .expect("SFTP handshake with sftp-server");
    Some((fs, child))
}

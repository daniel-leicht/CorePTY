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

/// A real sshd to test against, from the environment (for `#[ignore]` tests
/// run by hand): `COREPTY_SSHD=host:port`, `COREPTY_SSHD_USER`,
/// `COREPTY_SSHD_KEY` (an Ed25519/ECDSA private key file) and
/// `COREPTY_SSHD_DIR` (a scratch folder on the server; tests stay inside it).
pub struct RealServer {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key: String,
    pub dir: Vec<u8>,
}

pub fn real_sshd() -> Option<RealServer> {
    let address = std::env::var("COREPTY_SSHD").ok()?;
    let (host, port) = address.rsplit_once(':')?;
    Some(RealServer {
        host: host.to_string(),
        port: port.parse().ok()?,
        user: std::env::var("COREPTY_SSHD_USER").ok()?,
        key: std::env::var("COREPTY_SSHD_KEY").ok()?,
        dir: std::env::var("COREPTY_SSHD_DIR").ok()?.into_bytes(),
    })
}

/// A TCP relay to `upstream` that delays every chunk by `delay` each way: a
/// long-distance link (latency without a bandwidth cap).
pub async fn delayed_relay(
    upstream: std::net::SocketAddr,
    delay: std::time::Duration,
) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect(upstream).await else {
                continue;
            };
            let _ = client.set_nodelay(true);
            let _ = server.set_nodelay(true);
            let (client_read, client_write) = client.into_split();
            let (server_read, server_write) = server.into_split();
            for (mut from, mut to) in [
                (
                    Box::new(client_read) as Box<dyn tokio::io::AsyncRead + Send + Unpin>,
                    Box::new(server_write) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
                ),
                (Box::new(server_read), Box::new(client_write)),
            ] {
                let (queue, mut due) =
                    tokio::sync::mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
                tokio::spawn(async move {
                    let mut buffer = vec![0; 256 * 1024];
                    while let Ok(read) = from.read(&mut buffer).await {
                        if read == 0 {
                            break;
                        }
                        let at = tokio::time::Instant::now() + delay;
                        if queue.send((at, buffer[..read].to_vec())).is_err() {
                            break;
                        }
                    }
                });
                tokio::spawn(async move {
                    while let Some((at, data)) = due.recv().await {
                        tokio::time::sleep_until(at).await;
                        if to.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown().await;
                });
            }
        }
    });
    address
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

//! Throughput measurements against a real sshd (run by hand, in release):
//!
//! ```text
//! COREPTY_SSHD=127.0.0.1:22 COREPTY_SSHD_USER=me COREPTY_SSHD_KEY=~/.ssh/id_ed25519 \
//! COREPTY_SSHD_DIR=/tmp/corepty-bench COREPTY_BENCH_MB=1024 COREPTY_BENCH_DELAY_MS=20 \
//!   cargo test --release --lib sftp::bench -- --ignored --nocapture
//! ```
//!
//! It uploads and downloads a file of `COREPTY_BENCH_MB` through SftpFs for
//! several chunk sizes and pipeline depths (optionally through a relay that
//! adds `COREPTY_BENCH_DELAY_MS` each way), and times `scp` the same way.

use super::testing::{delayed_relay, real_sshd, RealServer};
use crate::connections::{ConnectionRegistry, HostKeys, SshAuth, SshTarget};
use crate::sftp::file::Pipeline;
use crate::vfs::{sftp::SftpFs, VPath, Vfs, WriteMode};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

async fn connect(
    server: &RealServer,
    port: u16,
    keys: &tempfile::TempDir,
    id: &str,
) -> (ConnectionRegistry, SftpFs) {
    let registry = ConnectionRegistry::default();
    let target = SshTarget {
        host: "127.0.0.1".to_string(),
        port,
        username: server.user.clone(),
        auth: SshAuth::Key {
            path: server.key.clone(),
            passphrase: None,
        },
    };
    let host_keys = HostKeys {
        path: Some(keys.path().join("known_hosts")),
    };
    let lease = registry
        .connect(id.to_string(), &target, &host_keys, &|_| {})
        .await
        .expect("log in to the benchmark server");
    let fs = SftpFs::over_connection(lease)
        .await
        .expect("start SFTP on the benchmark server");
    (registry, fs)
}

fn megabytes_per_second(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64()
}

async fn upload(fs: &SftpFs, path: &VPath, bytes: u64, block: &[u8]) -> Duration {
    let _ = fs.remove(path, false).await;
    let started = Instant::now();
    let mut writer = fs.open_write(path, WriteMode::CreateNew).await.unwrap();
    let mut left = bytes;
    while left > 0 {
        let take = left.min(block.len() as u64) as usize;
        writer.write_all(&block[..take]).await.unwrap();
        left -= take as u64;
    }
    writer.shutdown().await.unwrap();
    started.elapsed()
}

async fn download(fs: &SftpFs, path: &VPath, bytes: u64) -> Duration {
    let started = Instant::now();
    let mut reader = fs.open_read(path, 0).await.unwrap();
    let mut buffer = vec![0; 1024 * 1024];
    let mut total = 0;
    loop {
        let read = reader.read(&mut buffer).await.unwrap();
        if read == 0 {
            break;
        }
        total += read as u64;
    }
    assert_eq!(total, bytes, "downloaded size");
    started.elapsed()
}

/// `scp` up and down through the same relay, for comparison.
fn scp(
    server: &RealServer,
    port: u16,
    local: &std::path::Path,
    remote: &str,
    keys: &std::path::Path,
) -> (Duration, Duration) {
    let binary = if cfg!(windows) {
        r"C:\Program Files\Git\usr\bin\scp.exe"
    } else {
        "scp"
    };
    let run = |from: &str, to: &str| {
        let started = Instant::now();
        let status = std::process::Command::new(binary)
            // Git's msys scp would rewrite `host:/c/...` into `host:C:/...`.
            .env("MSYS_NO_PATHCONV", "1")
            .env("MSYS2_ARG_CONV_EXCL", "*")
            .args(["-q", "-B", "-P", &port.to_string(), "-i", &server.key])
            .args(["-o", &format!("UserKnownHostsFile={}", keys.display())])
            .args(["-o", "StrictHostKeyChecking=no", from, to])
            .status()
            .expect("run scp");
        assert!(status.success(), "scp {from} {to}");
        started.elapsed()
    };
    let remote = format!("{}@127.0.0.1:{remote}", server.user);
    let local_display = local.display().to_string();
    let up = run(&local_display, &remote);
    let back = format!("{local_display}.back");
    let down = run(&remote, &back);
    let _ = std::fs::remove_file(&back);
    (up, down)
}

// Multi-threaded: the latency relay must keep running while scp blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real sshd (COREPTY_SSHD…); run in release by hand"]
async fn sftp_throughput() {
    let Some(server) = real_sshd() else {
        eprintln!(
            "skipped: set COREPTY_SSHD, COREPTY_SSHD_USER, COREPTY_SSHD_KEY, COREPTY_SSHD_DIR"
        );
        return;
    };
    let bytes = env_number("COREPTY_BENCH_MB", 256) * 1024 * 1024;
    let delay = Duration::from_millis(env_number("COREPTY_BENCH_DELAY_MS", 0));
    let upstream = tokio::net::lookup_host((server.host.as_str(), server.port))
        .await
        .unwrap()
        .next()
        .unwrap();
    let port = if delay.is_zero() {
        server.port
    } else {
        delayed_relay(upstream, delay).await.port()
    };
    let keys = tempfile::tempdir().unwrap();
    let (_registry, mut fs) = connect(&server, port, &keys, "bench").await;
    let mut remote = server.dir.clone();
    remote.extend_from_slice(b"/corepty-bench.bin");
    let path = VPath::Posix(remote.clone());
    let block: Vec<u8> = (0..1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();

    println!(
        "\n{} MiB, {} ms each way, server {}:{}",
        bytes / (1024 * 1024),
        delay.as_millis(),
        server.host,
        server.port
    );
    println!(
        "{:>8} {:>6} {:>12} {:>12}",
        "chunk", "depth", "upload MB/s", "down MB/s"
    );
    let configs: Vec<(u32, usize)> = std::env::var("COREPTY_BENCH_CONFIGS")
        .ok()
        .map(|list| {
            list.split(',')
                .filter_map(|pair| {
                    let (chunk, depth) = pair.split_once('x')?;
                    Some((chunk.parse::<u32>().ok()? * 1024, depth.parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                (32 * 1024, 1),
                (32 * 1024, 16),
                (32 * 1024, 64),
                (64 * 1024, 32),
                (128 * 1024, 16),
                (255 * 1024, 8),
                (255 * 1024, 16),
                (255 * 1024, 32),
            ]
        });
    for (chunk, depth) in configs {
        let pipeline = Pipeline { chunk, depth };
        fs.set_pipelines(pipeline, pipeline);
        let up = upload(&fs, &path, bytes, &block).await;
        let down = download(&fs, &path, bytes).await;
        println!(
            "{:>7}K {:>6} {:>12.1} {:>12.1}",
            chunk / 1024,
            depth,
            megabytes_per_second(bytes, up),
            megabytes_per_second(bytes, down)
        );
    }
    let defaults = SftpFs::over_connection(
        _registry
            .attach("bench")
            .expect("the benchmark connection is still up"),
    )
    .await
    .unwrap();
    let (read, write) = defaults.pipelines();
    let up = upload(&defaults, &path, bytes, &block).await;
    let down = download(&defaults, &path, bytes).await;
    println!(
        "{:>8} {:>6} {:>12.1} {:>12.1}   (defaults: read {}K×{}, write {}K×{})",
        "default",
        "",
        megabytes_per_second(bytes, up),
        megabytes_per_second(bytes, down),
        read.chunk / 1024,
        read.depth,
        write.chunk / 1024,
        write.depth
    );
    let _ = fs.remove(&path, false).await;

    // scp, through the same relay.
    let local = keys.path().join("scp-source.bin");
    {
        let mut file = std::fs::File::create(&local).unwrap();
        let mut left = bytes;
        while left > 0 {
            let take = left.min(block.len() as u64) as usize;
            std::io::Write::write_all(&mut file, &block[..take]).unwrap();
            left -= take as u64;
        }
    }
    let remote_display = String::from_utf8_lossy(&remote).into_owned();
    let known = keys.path().join("scp_known_hosts");
    let (up, down) = tokio::task::spawn_blocking({
        let server = RealServer {
            dir: server.dir.clone(),
            host: server.host.clone(),
            user: server.user.clone(),
            key: server.key.clone(),
            port: server.port,
        };
        move || scp(&server, port, &local, &remote_display, &known)
    })
    .await
    .unwrap();
    println!(
        "{:>8} {:>6} {:>12.1} {:>12.1}   (OpenSSH scp, includes its login)",
        "scp",
        "",
        megabytes_per_second(bytes, up),
        megabytes_per_second(bytes, down)
    );
    let _ = fs.remove(&path, false).await;
}

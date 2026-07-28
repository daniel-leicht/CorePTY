//! The elevated broker process.
//!
//! `corepty.exe` re-launches itself with `--broker …` via `ShellExecute "runas"`
//! (which raises the UAC prompt). The elevated instance runs *this* code instead
//! of the Tauri app: it connects back to the non-elevated app over **two**
//! one-directional named pipes (output + input), spawns a ConPTY running the
//! requested shell **elevated**, and relays terminal I/O + resize.
//!
//! Two pipes (rather than one duplex pipe) are deliberate: Windows serializes
//! synchronous I/O per handle, so concurrent read+write on a single handle would
//! deadlock. Each pipe here is used in exactly one direction per side.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use windows::Win32::System::Console::FreeConsole;

use crate::session::local::resolve_program;
use crate::session::proto::{self, TAG_CLOSE, TAG_DATA, TAG_EXIT, TAG_RESIZE};

/// Entry point for a `--broker` invocation. Blocks until the elevated shell ends.
pub fn run(args: &[String]) {
    let (Some(pipe_out), Some(pipe_in)) = (arg(args, "--pipe-out"), arg(args, "--pipe-in")) else {
        return;
    };
    let shell = arg(args, "--shell").unwrap_or_else(|| "powershell".to_string());
    let cols: u16 = arg(args, "--cols")
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);
    let rows: u16 = arg(args, "--rows")
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    log(&format!("start: shell={shell} cols={cols} rows={rows}"));
    // Detach any console handed to us by the elevation service.
    let _ = unsafe { FreeConsole() };
    match bridge(&pipe_out, &pipe_in, &shell, cols.max(1), rows.max(1)) {
        Ok(()) => log("finished"),
        Err(e) => log(&format!("ERROR: {e}")),
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1).cloned())
}

fn bridge(pipe_out: &str, pipe_in: &str, shell: &str, cols: u16, rows: u16) -> Result<(), String> {
    let fout = Arc::new(open_pipe(pipe_out).ok_or("could not open the output pipe")?);
    let fin = open_pipe(pipe_in).ok_or("could not open the input pipe")?;
    log("pipes connected");

    let (program, pargs) =
        resolve_program(shell).ok_or_else(|| format!("unknown shell '{shell}'"))?;
    log(&format!("program: {program}"));

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new(&program);
    for a in &pargs {
        cmd.arg(a);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "CorePTY");
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.is_empty() {
            cmd.cwd(home);
        }
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn: {e}"))?;
    log("shell spawned; streaming");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let mut writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let master = pair.master;
    let mut killer = child.clone_killer();

    // PTY output -> output pipe (write-only on this handle).
    let out = {
        let fout = fout.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            let mut total = 0usize;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if total == 0 {
                            log(&format!("pty first output: {n} bytes"));
                        }
                        total += n;
                        let mut w: &File = &fout;
                        if proto::write_frame(&mut w, TAG_DATA, &buf[..n]).is_err() {
                            log("output pipe write failed");
                            break;
                        }
                    }
                }
            }
            log(&format!("pty output ended after {total} bytes"));
        })
    };

    // App input / resize -> PTY (read-only on the input handle).
    let _inp = thread::spawn(move || {
        let mut r: &File = &fin;
        while let Ok(frame) = proto::read_frame(&mut r) {
            match frame.tag {
                TAG_DATA => {
                    let _ = writer.write_all(&frame.payload);
                    let _ = writer.flush();
                }
                TAG_RESIZE if frame.payload.len() >= 4 => {
                    let c = u16::from_le_bytes([frame.payload[0], frame.payload[1]]);
                    let r = u16::from_le_bytes([frame.payload[2], frame.payload[3]]);
                    let _ = master.resize(PtySize {
                        rows: r.max(1),
                        cols: c.max(1),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                TAG_CLOSE => break,
                _ => {}
            }
        }
        let _ = killer.kill();
    });

    // Wait for the shell, drain its output, then tell the app it exited.
    let code = child
        .wait()
        .ok()
        .map(|s| s.exit_code() as i32)
        .unwrap_or(-1);
    log(&format!("shell exited: code={code}"));
    let _ = out.join();
    {
        let mut w: &File = &fout;
        let _ = proto::write_frame(&mut w, TAG_EXIT, &code.to_le_bytes());
    }
    thread::sleep(Duration::from_millis(120));
    Ok(())
}

/// Append a diagnostic line to `%TEMP%\corepty-broker.log` — only when
/// `COREPTY_DEBUG` is set (off by default; see the app-side `log` for why).
fn log(msg: &str) {
    use std::io::Write as _;
    if std::env::var_os("COREPTY_DEBUG").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("corepty-broker.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Open one of the app's named pipes as a client, retrying while it spins up.
fn open_pipe(name: &str) -> Option<File> {
    for _ in 0..100 {
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(name) {
            return Some(f);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

// Local shell sessions backed by a real PTY (ConPTY on Windows, openpty
// elsewhere) via `portable-pty`. The available shells are OS-specific — Windows:
// Command Prompt / PowerShell / PowerShell 7 / Git Bash; Unix: bash / zsh / fish
// / sh / pwsh — plus an arbitrary custom command. See `available_shells`.

use std::io::{Read, Write};
use std::path::Path;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc::unbounded_channel;
use uuid::Uuid;

use super::{
    emit_data, emit_exit, emit_status, SessionInfo, SessionInput, SessionKind, SessionManager,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalOptions {
    /// Optional caller-supplied session id (lets the UI wire up event routing
    /// before output starts). Generated if absent.
    #[serde(default)]
    pub id: Option<String>,
    /// A shell id from `available_shells` (OS-specific), or "custom" to run an
    /// arbitrary `command`.
    pub shell: String,
    /// Program to run when `shell == "custom"`.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Optional display title override.
    #[serde(default)]
    pub title: Option<String>,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

pub fn spawn(
    app: AppHandle,
    manager: &SessionManager,
    opts: LocalOptions,
) -> Result<SessionInfo, String> {
    let (program, args, default_title) = resolve_shell(&opts)?;

    let id = opts
        .id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if manager.exists(&id) {
        return Err(format!("session id already in use: {id}"));
    }

    let mut cmd = CommandBuilder::new(&program);
    for a in &args {
        cmd.arg(a);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Make it easy for shells/programs to detect us.
    cmd.env("TERM_PROGRAM", "CorePTY");
    if let Some(dir) = opts.cwd.clone().or_else(home_dir) {
        if !dir.is_empty() && Path::new(&dir).is_dir() {
            cmd.cwd(dir);
        }
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: opts.rows.max(1),
            cols: opts.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("failed to open pty: {e}"))?;

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| {
        // If we never resolved the shell to a real path (still a bare name), the
        // OS "file not found" is just "it isn't installed / not on PATH" — say so
        // plainly instead of dumping the raw CreateProcessW command line.
        if !program.contains('\\') && !program.contains('/') {
            format!("could not find '{program}' — is it installed and on your PATH?")
        } else {
            format!("failed to launch {program}: {e}")
        }
    })?;
    // The slave handle is no longer needed once the child owns it.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("failed to read from pty: {e}"))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("failed to write to pty: {e}"))?;
    let master = pair.master;
    let mut killer = child.clone_killer();

    let title = opts.title.clone().unwrap_or(default_title);
    let info = SessionInfo {
        id: id.clone(),
        kind: SessionKind::Local,
        title,
    };
    let (tx, mut rx) = unbounded_channel::<SessionInput>();
    manager.register(info.clone(), tx);

    // Reader: PTY output -> UI.
    {
        let app = app.clone();
        let id = id.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => emit_data(&app, &id, &buf[..n]),
                    Err(_) => break,
                }
            }
        });
    }

    // Waiter: child exit -> emit exit + deregister.
    {
        let app = app.clone();
        let id = id.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            app.state::<SessionManager>().remove(&id);
            emit_exit(&app, &id, code, None);
        });
    }

    // Control: UI input/resize/close -> PTY. Owns the master (for resize).
    {
        std::thread::spawn(move || {
            let master = master;
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    SessionInput::Data(bytes) => {
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    SessionInput::Resize { cols, rows } => {
                        let _ = master.resize(PtySize {
                            rows: rows.max(1),
                            cols: cols.max(1),
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    SessionInput::Close => {
                        let _ = killer.kill();
                        break;
                    }
                }
            }
            let _ = killer.kill();
            drop(writer);
            drop(master);
        });
    }

    emit_status(&app, &id, "connected", None);
    Ok(info)
}

/// Resolves `(program, args, default_title)` for the requested shell. Delegates
/// the program/args lookup to `resolve_program` so the mapping lives in one place.
fn resolve_shell(opts: &LocalOptions) -> Result<(String, Vec<String>, String), String> {
    if opts.shell == "custom" {
        let program = opts
            .command
            .clone()
            .filter(|c| !c.is_empty())
            .ok_or("custom shell requires a command")?;
        let title = opts.title.clone().unwrap_or_else(|| file_stem(&program));
        return Ok((program, opts.args.clone().unwrap_or_default(), title));
    }
    let (program, args) =
        resolve_program(&opts.shell).ok_or_else(|| format!("unknown shell '{}'", opts.shell))?;
    Ok((program, args, shell_title(&opts.shell).to_string()))
}

/// Human-friendly default tab title for a known shell name.
fn shell_title(shell: &str) -> &'static str {
    match shell {
        "cmd" => "Command Prompt",
        "powershell" => "PowerShell",
        "pwsh" => "PowerShell 7",
        "bash" => "Bash",
        "zsh" => "Zsh",
        "fish" => "Fish",
        "sh" => "Shell",
        _ => "Shell",
    }
}

/// Resolve `(program, args)` for a known shell name — shared with the elevated
/// broker, which spawns the same shells under an elevated ConPTY. Which shells
/// are valid depends on the OS (no cmd/PowerShell on Unix; no zsh/fish/sh on
/// Windows); an unknown/unavailable shell yields `None`.
pub(crate) fn resolve_program(shell: &str) -> Option<(String, Vec<String>)> {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    Some(match shell {
        #[cfg(windows)]
        "cmd" => (resolve_cmd(), args(&[])),
        #[cfg(windows)]
        "powershell" => (resolve_powershell(), args(&["-NoLogo"])),
        "pwsh" => (resolve_pwsh(), args(&["-NoLogo"])),
        "bash" => (resolve_bash(), args(&["-l", "-i"])),
        #[cfg(not(windows))]
        "zsh" => (resolve_unix("zsh")?, args(&["-l"])),
        #[cfg(not(windows))]
        "fish" => (resolve_unix("fish")?, args(&["-l", "-i"])),
        #[cfg(not(windows))]
        "sh" => (
            resolve_unix("sh").unwrap_or_else(|| "/bin/sh".to_string()),
            args(&[]),
        ),
        _ => return None,
    })
}

/// Display metadata for a selectable local shell, sent to the UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShellInfo {
    pub id: String,
    pub label: String,
    pub hint: String,
    pub icon: String,
}

/// The shells offered on this OS, filtered to those actually installed. Windows
/// always has Command Prompt + PowerShell; Unix always has `sh`; optional shells
/// (PowerShell 7, Git Bash, fish, …) appear only when present. The UI renders
/// whatever this returns.
pub fn available_shells() -> Vec<ShellInfo> {
    // (id, label, hint, icon, always_present)
    #[cfg(windows)]
    let candidates: &[(&str, &str, &str, &str, bool)] = &[
        ("powershell", "PowerShell", "Windows PowerShell 5.1", "powershell", true),
        ("pwsh", "PowerShell 7", "Cross-platform pwsh", "pwsh", false),
        ("cmd", "Command Prompt", "cmd.exe", "cmd", true),
        ("bash", "Bash", "Git Bash / WSL", "bash", false),
    ];
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &str, &str, &str, bool)] = &[
        ("zsh", "Zsh", "Default macOS shell", "terminal", true),
        ("bash", "Bash", "GNU Bash", "bash", true),
        ("fish", "Fish", "Friendly shell", "terminal", false),
        ("sh", "sh", "POSIX shell", "terminal", true),
        ("pwsh", "PowerShell 7", "Cross-platform pwsh", "pwsh", false),
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[(&str, &str, &str, &str, bool)] = &[
        ("bash", "Bash", "GNU Bash", "bash", false),
        ("zsh", "Zsh", "Z shell", "terminal", false),
        ("fish", "Fish", "Friendly shell", "terminal", false),
        ("sh", "sh", "POSIX shell", "terminal", true),
        ("pwsh", "PowerShell 7", "Cross-platform pwsh", "pwsh", false),
    ];

    let mut out: Vec<ShellInfo> = Vec::new();
    for &(id, label, hint, icon, always) in candidates {
        if always || shell_available(id) {
            out.push(ShellInfo {
                id: id.to_string(),
                label: label.to_string(),
                hint: hint.to_string(),
                icon: icon.to_string(),
            });
        }
    }
    if out.is_empty() {
        out.push(ShellInfo {
            id: "sh".to_string(),
            label: "Shell".to_string(),
            hint: String::new(),
            icon: "terminal".to_string(),
        });
    }
    out
}

/// Whether an (optional) shell resolves to a real, launchable program.
fn shell_available(id: &str) -> bool {
    match resolve_program(id) {
        Some((prog, _)) => {
            let p = Path::new(&prog);
            if p.is_absolute() {
                p.symlink_metadata().is_ok()
            } else {
                which(&prog).is_some()
            }
        }
        None => false,
    }
}

fn file_stem(program: &str) -> String {
    Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shell")
        .to_string()
}

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
}

#[cfg(windows)]
fn win_root() -> String {
    std::env::var("SystemRoot")
        .or_else(|_| std::env::var("windir"))
        .unwrap_or_else(|_| r"C:\Windows".to_string())
}

/// Find an executable by name on `PATH`, returning a full launchable path — used
/// both to resolve a shell to a real path (Windows ConPTY won't search `PATH`
/// for a bare name) and to test whether an optional shell is installed. Uses
/// `symlink_metadata` so 0-byte reparse-point aliases (e.g. the Windows Store
/// WindowsApps entry) still match.
fn which(exe: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(exe);
        if candidate.symlink_metadata().map(|m| !m.is_dir()).unwrap_or(false) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(windows)]
fn resolve_cmd() -> String {
    let p = format!(r"{}\System32\cmd.exe", win_root());
    if Path::new(&p).exists() {
        p
    } else {
        "cmd.exe".to_string()
    }
}

#[cfg(windows)]
fn resolve_powershell() -> String {
    let p = format!(
        r"{}\System32\WindowsPowerShell\v1.0\powershell.exe",
        win_root()
    );
    if Path::new(&p).exists() {
        p
    } else {
        "powershell.exe".to_string()
    }
}

#[cfg(windows)]
fn resolve_pwsh() -> String {
    // MSI / winget install — honor a non-C: Program Files via the env var.
    for base in [
        std::env::var_os("ProgramFiles"),
        std::env::var_os("ProgramW6432"),
    ]
    .into_iter()
    .flatten()
    {
        for sub in [r"PowerShell\7\pwsh.exe", r"PowerShell\7-preview\pwsh.exe"] {
            let c = Path::new(&base).join(sub);
            if c.exists() {
                return c.to_string_lossy().into_owned();
            }
        }
    }
    // Microsoft Store install — the WindowsApps execution alias, a 0-byte reparse
    // point a plain exists()/metadata() can't always stat.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let alias = Path::new(&local).join(r"Microsoft\WindowsApps\pwsh.exe");
        if alias.symlink_metadata().is_ok() {
            return alias.to_string_lossy().into_owned();
        }
    }
    // scoop / custom install — anything named pwsh.exe on PATH.
    which("pwsh.exe").unwrap_or_else(|| "pwsh.exe".to_string())
}

#[cfg(windows)]
fn resolve_bash() -> String {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(pf) = std::env::var("ProgramFiles") {
        candidates.push(format!(r"{pf}\Git\bin\bash.exe"));
        candidates.push(format!(r"{pf}\Git\usr\bin\bash.exe"));
    }
    candidates.push(r"C:\Program Files\Git\bin\bash.exe".to_string());
    candidates.push(r"C:\Windows\System32\bash.exe".to_string()); // WSL
    for c in &candidates {
        if Path::new(c).exists() {
            return c.clone();
        }
    }
    which("bash.exe").unwrap_or_else(|| "bash.exe".to_string())
}

/// Resolve a Unix program by name: common bin dirs first, then `PATH`.
#[cfg(not(windows))]
fn resolve_unix(name: &str) -> Option<String> {
    for base in ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        let p = format!("{base}/{name}");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    which(name)
}

#[cfg(not(windows))]
fn resolve_pwsh() -> String {
    resolve_unix("pwsh").unwrap_or_else(|| "pwsh".to_string())
}

#[cfg(not(windows))]
fn resolve_bash() -> String {
    resolve_unix("bash").unwrap_or_else(|| "bash".to_string())
}

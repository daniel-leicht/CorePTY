// What local tabs are doing, for "restore my tabs": each shell's working
// folder, and the Claude Code session running in it (its id, its folder and
// the command-line options to resume it with).
//
// Claude Code writes `<config>/sessions/<pid>.json` for each interactive
// session (config: `$CLAUDE_CONFIG_DIR`, else `~/.claude`), so a process under
// a tab's shell that has such a file is Claude. The file names the session and
// its folder; the options come from the process's command line, keeping those
// that say how Claude runs and leaving out those that pick a session or make
// it a one-shot run (see `resume_args`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tauri::State;

use crate::session::SessionManager;

/// How far below a shell to look for Claude (shell → cmd /c claude.cmd →
/// node is three levels).
const MAX_DEPTH: usize = 4;
/// A session file is a few hundred bytes; anything much bigger isn't one.
const MAX_SESSION_FILE: u64 = 64 * 1024;

/// The process list, kept between looks (refreshing one is cheaper than
/// building it again).
#[derive(Default, Clone)]
pub struct Processes(Arc<Mutex<Option<System>>>);

impl Processes {
    fn with<T>(&self, f: impl FnOnce(&mut System) -> T) -> T {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(System::new))
    }
}

/// What one tab is doing.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabContext {
    pub id: String,
    /// The shell process's working folder. Command Prompt and bash keep it
    /// current; PowerShell's stays where it started (its location is its own),
    /// so the frontend prefers the folder the prompt reports (OSC 7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeSession>,
}

/// A Claude Code session running in a tab.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSession {
    pub session_id: String,
    /// The folder Claude runs in (its sessions are kept per folder).
    pub cwd: String,
    /// The options to start it with again, after `--resume <id>`.
    pub args: Vec<String>,
    /// The options left out, as typed (for the restore dialog).
    pub dropped: Vec<String>,
    /// Whether the conversation has been saved (a new session isn't until
    /// its first message, and can't be resumed before that).
    pub resumable: bool,
}

/// The parts of `sessions/<pid>.json` used here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    pid: u32,
    session_id: String,
    cwd: String,
    /// When the session started (ms since 1970).
    #[serde(default)]
    started_at: Option<u64>,
    /// The process's creation time (on Windows a FILETIME, as a string).
    #[serde(default)]
    proc_start: Option<String>,
    /// "interactive" for a session in a terminal.
    #[serde(default)]
    kind: Option<String>,
}

/// Where Claude Code keeps its state.
fn claude_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => dirs::home_dir().map(|home| home.join(".claude")),
    }
}

/// The processes that have a session file (from its name; read later).
fn session_pids(dir: &Path) -> HashSet<u32> {
    let Ok(entries) = fs::read_dir(dir.join("sessions")) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            name.to_str()?.strip_suffix(".json")?.parse().ok()
        })
        .collect()
}

fn read_session(dir: &Path, pid: u32) -> Option<SessionFile> {
    let path = dir.join("sessions").join(format!("{pid}.json"));
    if fs::metadata(&path).ok()?.len() > MAX_SESSION_FILE {
        return None;
    }
    let file: SessionFile = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let interactive = file
        .kind
        .as_deref()
        .is_none_or(|kind| kind == "interactive");
    (file.pid == pid && interactive && is_uuid(&file.session_id) && !file.cwd.is_empty())
        .then_some(file)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Seconds between 1601 (FILETIME's epoch) and 1970.
const FILETIME_TO_UNIX: u64 = 11_644_473_600;

/// Whether the session file was written by this process (started at
/// `started`, seconds since 1970) and not by an earlier one that had its id.
fn written_by(file: &SessionFile, started: u64) -> bool {
    let filetime = file
        .proc_start
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ticks| ticks / 10_000_000)
        .filter(|s| *s > FILETIME_TO_UNIX)
        .map(|s| s - FILETIME_TO_UNIX)
        // A plausible date (2001–2286): it is a FILETIME, not some other clock.
        .filter(|s| (1_000_000_000..10_000_000_000).contains(s));
    if let Some(created) = filetime {
        return created.abs_diff(started) <= 1;
    }
    // A process that is older than the file and has its id is the one that
    // wrote it: ids aren't shared by two live processes.
    file.started_at.is_some_and(|ms| started <= ms / 1000 + 1)
}

/// The children of each process, oldest first.
fn children(sys: &System) -> HashMap<Pid, Vec<Pid>> {
    let mut map: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in sys.processes() {
        let Some(parent) = process.parent() else {
            continue;
        };
        // Windows keeps a dead parent's id, which a newer process may get:
        // a child is never older than its parent.
        match sys.process(parent) {
            Some(p) if p.start_time() <= process.start_time() => {}
            _ => continue,
        }
        map.entry(parent).or_default().push(*pid);
    }
    for list in map.values_mut() {
        list.sort_by_key(|pid| (sys.process(*pid).map(|p| p.start_time()), *pid));
    }
    map
}

/// The nearest process under `shell` that is a Claude session.
fn find_claude(
    sys: &System,
    children: &HashMap<Pid, Vec<Pid>>,
    dir: &Path,
    sessions: &HashSet<u32>,
    shell: Pid,
) -> Option<(Pid, SessionFile)> {
    let mut queue = VecDeque::from([(shell, 0)]);
    let mut seen = HashSet::new();
    while let Some((pid, depth)) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if pid != shell && sessions.contains(&pid.as_u32()) {
            let started = sys.process(pid).map(|p| p.start_time());
            if let (Some(file), Some(started)) = (read_session(dir, pid.as_u32()), started) {
                if written_by(&file, started) {
                    return Some((pid, file));
                }
            }
        }
        if depth < MAX_DEPTH {
            for child in children.get(&pid).into_iter().flatten() {
                queue.push_back((*child, depth + 1));
            }
        }
    }
    None
}

/// Claude's own arguments: after the program, or after the script when a
/// runtime (node, bun) runs it.
fn claude_args(argv: &[String]) -> &[String] {
    let Some(program) = argv.first() else {
        return argv;
    };
    let stem = Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(stem.as_str(), "node" | "bun" | "deno") {
        // Runtime options come first, then the script.
        if let Some(script) = argv.iter().skip(1).position(|a| !a.starts_with('-')) {
            return &argv[script + 2..];
        }
        return &[];
    }
    &argv[1..]
}

/// How many values an option takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Takes {
    Nothing,
    /// One, which may start with `-`.
    Value,
    /// One if the next argument isn't an option.
    MaybeValue,
    /// One, then every following argument that isn't an option.
    Values,
}

use Takes::*;

/// Options kept on resume: how Claude runs (permissions, model, tools,
/// prompts, plugins, settings, debugging).
const KEEP: &[(&str, Takes)] = &[
    ("--add-dir", Values),
    ("--agent", Value),
    ("--agents", Value),
    ("--allow-dangerously-skip-permissions", Nothing),
    ("--allowed-tools", Values),
    ("--allowedTools", Values),
    ("--append-system-prompt", Value),
    ("--append-system-prompt-file", Value),
    ("--autocompact", Value),
    ("--ax-screen-reader", Nothing),
    ("--bare", Nothing),
    ("--betas", Values),
    ("--brief", Nothing),
    ("--chrome", Nothing),
    ("--dangerously-skip-permissions", Nothing),
    ("--debug", MaybeValue),
    ("--debug-file", Value),
    ("--disable-slash-commands", Nothing),
    ("--disallowed-tools", Values),
    ("--disallowedTools", Values),
    ("--effort", Value),
    ("--exclude-dynamic-system-prompt-sections", Nothing),
    ("--fallback-model", Value),
    ("--ide", Nothing),
    ("--mcp-config", Values),
    ("--model", Value),
    ("--name", Value),
    ("--no-chrome", Nothing),
    ("--permission-mode", Value),
    ("--permission-prompts", Value),
    ("--plugin-dir", Value),
    ("--plugin-url", Value),
    ("--restricted", Nothing),
    ("--safe-mode", Nothing),
    ("--setting-sources", Value),
    ("--settings", Value),
    ("--strict-mcp-config", Nothing),
    ("--system-prompt", Value),
    ("--system-prompt-file", Value),
    ("--system-prompt-snapshot", Value),
    ("--tools", Values),
    ("--verbose", Nothing),
    ("-d", MaybeValue),
    ("-n", Value),
];

/// Options left out: which session to open (resuming names it), and one-shot,
/// background or scripted runs. Listed so their values are left out with them.
const DROP: &[(&str, Takes)] = &[
    ("--background", Nothing),
    ("--bg", Nothing),
    ("--cloud", MaybeValue),
    ("--continue", Nothing),
    ("--environment", Value),
    ("--file", Values),
    ("--fork-session", Nothing),
    ("--forward-subagent-text", Nothing),
    ("--from-pr", MaybeValue),
    ("--help", Nothing),
    ("--include-hook-events", Nothing),
    ("--include-partial-messages", Nothing),
    ("--input-format", Value),
    ("--json-schema", Value),
    ("--max-budget-usd", Value),
    ("--no-session-persistence", Nothing),
    ("--output-format", Value),
    ("--permission-prompt-tool", Value),
    ("--print", Nothing),
    ("--prompt-suggestions", MaybeValue),
    ("--remote-control", MaybeValue),
    ("--remote-control-session-name-prefix", Value),
    ("--replay-user-messages", Nothing),
    ("--resume", MaybeValue),
    ("--session-id", Value),
    ("--teleport", MaybeValue),
    ("--tmux", Nothing),
    ("--version", Nothing),
    ("--worktree", MaybeValue),
    ("-c", Nothing),
    ("-h", Nothing),
    ("-p", Nothing),
    ("-r", MaybeValue),
    ("-v", Nothing),
    ("-w", MaybeValue),
];

fn takes(table: &[(&str, Takes)], name: &str) -> Option<Takes> {
    table.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// A value that can be typed back into a shell: one line, no double quotes
/// (Command Prompt and Windows PowerShell can't pass those on reliably).
fn typeable(value: &str) -> bool {
    !value.chars().any(|c| c.is_control() || c == '"')
}

/// Split Claude's arguments into those to resume with and those left out
/// (grouped per option, as typed). Mirrors how Claude reads its command line:
/// an option's value may be attached (`--model=opus`, `-nwork`) or follow it,
/// and a list option takes arguments until the next option. Anything else (the
/// opening prompt, unknown options) is left out.
pub fn resume_args(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut keep = Vec::new();
    let mut dropped = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            // Everything after it is the prompt.
            dropped.push(args[i - 1..].join(" "));
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            dropped.push(arg.clone());
            continue;
        }
        // The option's name and an attached value.
        let (name, attached) =
            if let Some((name, value)) = arg.split_once('=').filter(|_| arg.starts_with("--")) {
                (name.to_string(), Some(value.to_string()))
            } else if !arg.starts_with("--") && arg.len() > 2 && arg.is_char_boundary(2) {
                (arg[..2].to_string(), Some(arg[2..].to_string()))
            } else {
                (arg.clone(), None)
            };
        let (kept, takes) = match (takes(KEEP, &name), takes(DROP, &name)) {
            (Some(t), _) => (true, t),
            (None, Some(t)) => (false, t),
            (None, None) => {
                dropped.push(arg.clone());
                continue;
            }
        };
        let mut values = Vec::new();
        match (takes, attached) {
            (Nothing, Some(_)) if kept => {
                // `--verbose=x` isn't something Claude accepted.
                dropped.push(arg.clone());
                continue;
            }
            (Nothing, _) => {}
            (_, Some(value)) => values.push(value),
            (Value | Values, None) => match args.get(i) {
                Some(value) => {
                    values.push(value.clone());
                    i += 1;
                }
                None => {
                    dropped.push(arg.clone());
                    continue;
                }
            },
            (MaybeValue, None) => {
                if let Some(value) = args.get(i).filter(|a| !a.starts_with('-')) {
                    values.push(value.clone());
                    i += 1;
                }
            }
        }
        if takes == Values {
            while let Some(value) = args.get(i).filter(|a| !a.starts_with('-')) {
                values.push(value.clone());
                i += 1;
            }
        }
        if kept && values.iter().all(|v| typeable(v)) {
            keep.push(name);
            keep.extend(values);
        } else {
            let mut typed = vec![arg.clone()];
            // The attached value is part of `arg` already.
            let skip = usize::from(arg.len() > name.len());
            typed.extend(values.into_iter().skip(skip));
            dropped.push(typed.join(" "));
        }
    }
    (keep, dropped)
}

/// Claude keeps a conversation in `projects/<folder>/<id>.jsonl`, the folder
/// named after the one it ran in. Looks there first, then in every folder
/// (the naming may change).
fn has_transcript(dir: &Path, cwd: &str, session_id: &str) -> bool {
    let projects = dir.join("projects");
    let name = format!("{session_id}.jsonl");
    let folder: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if projects.join(folder).join(&name).is_file() {
        return true;
    }
    fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.path().join(&name).is_file())
}

/// A folder as the user would type it: no trailing separator (`C:\Users\`),
/// except for a root (`C:\`, `/`).
fn tidy_folder(path: &Path) -> Option<String> {
    let text = dunce::simplified(path).to_string_lossy().into_owned();
    if text.is_empty() {
        return None;
    }
    let trimmed = text.trim_end_matches(['\\', '/']);
    let root = trimmed.is_empty() || (trimmed.len() == 2 && trimmed.ends_with(':'));
    Some(if root { text } else { trimmed.to_string() })
}

fn lossy(args: &[std::ffi::OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// A shell found under a tab.
struct Found {
    id: String,
    /// The tab's process.
    shell: Pid,
    /// The shell that has the folder (see `inner_shell`).
    inner: Pid,
    claude: Option<(Pid, SessionFile)>,
}

/// The shell the user is typing into: Git Bash's `bin\bash.exe` only starts
/// `usr\bin\bash.exe` and waits, and a shell started in a shell (`cmd` in
/// cmd) is the one in front; either way, a child with the same name.
fn inner_shell(sys: &System, tree: &HashMap<Pid, Vec<Pid>>, shell: Pid) -> Pid {
    let mut current = shell;
    for _ in 0..MAX_DEPTH {
        let Some(name) = sys.process(current).map(|p| p.name()) else {
            break;
        };
        let same = tree
            .get(&current)
            .into_iter()
            .flatten()
            .rev()
            .find(|child| {
                sys.process(**child)
                    .is_some_and(|p| p.name().eq_ignore_ascii_case(name))
            });
        match same {
            Some(child) => current = *child,
            None => break,
        }
    }
    current
}

/// Look at the shells `(tab, pid)`.
fn inspect(sys: &mut System, shells: &[(String, u32)]) -> Vec<TabContext> {
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let tree = children(sys);
    let dir = claude_dir();
    let sessions = dir.as_deref().map(session_pids).unwrap_or_default();
    let found: Vec<Found> = shells
        .iter()
        .map(|(id, pid)| {
            let shell = Pid::from_u32(*pid);
            let claude = match &dir {
                Some(dir) if !sessions.is_empty() => find_claude(sys, &tree, dir, &sessions, shell),
                _ => None,
            };
            Found {
                id: id.clone(),
                shell,
                inner: inner_shell(sys, &tree, shell),
                claude,
            }
        })
        .collect();

    // Only now read the folders and command lines, of these processes only.
    let wanted: Vec<Pid> = found
        .iter()
        .flat_map(|f| [f.inner].into_iter().chain(f.claude.as_ref().map(|c| c.0)))
        .collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&wanted),
        false,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );

    found
        .into_iter()
        .filter(|f| sys.process(f.shell).is_some())
        .map(
            |Found {
                 id, inner, claude, ..
             }| {
                let cwd = sys
                    .process(inner)
                    .and_then(|p| p.cwd())
                    .and_then(tidy_folder);
                let claude = claude.and_then(|(pid, file)| {
                    let argv = lossy(sys.process(pid)?.cmd());
                    let (args, dropped) = resume_args(claude_args(&argv));
                    let resumable = dir
                        .as_deref()
                        .is_some_and(|dir| has_transcript(dir, &file.cwd, &file.session_id));
                    Some(ClaudeSession {
                        session_id: file.session_id,
                        cwd: file.cwd,
                        args,
                        dropped,
                        resumable,
                    })
                });
                TabContext { id, cwd, claude }
            },
        )
        .collect()
}

/// The Claude sessions running anywhere on this machine (in any terminal).
fn running_sessions(sys: &mut System) -> Vec<String> {
    let Some(dir) = claude_dir() else {
        return Vec::new();
    };
    let pids = session_pids(&dir);
    if pids.is_empty() {
        return Vec::new();
    }
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut ids: Vec<String> = pids
        .into_iter()
        .filter_map(|pid| {
            let started = sys.process(Pid::from_u32(pid))?.start_time();
            let file = read_session(&dir, pid)?;
            written_by(&file, started).then_some(file.session_id)
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// What these local tabs are doing (tabs without a shell of ours, or whose
/// shell is gone, are left out).
#[tauri::command]
pub async fn tabs_context(
    manager: State<'_, SessionManager>,
    processes: State<'_, Processes>,
    ids: Vec<String>,
) -> Result<Vec<TabContext>, String> {
    let shells = manager.pids(&ids);
    if shells.is_empty() {
        return Ok(Vec::new());
    }
    let processes = processes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || processes.with(|sys| inspect(sys, &shells)))
        .await
        .map_err(|e| e.to_string())
}

/// The ids of the Claude sessions running now, so a restore doesn't open one
/// a second time.
#[tauri::command]
pub async fn claude_running(processes: State<'_, Processes>) -> Result<Vec<String>, String> {
    let processes = processes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || processes.with(running_sessions))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn split(line: &str) -> (String, Vec<String>) {
        let (keep, dropped) = resume_args(&args(line));
        (keep.join(" "), dropped)
    }

    #[test]
    fn keeps_how_claude_runs() {
        assert_eq!(
            split("--dangerously-skip-permissions"),
            ("--dangerously-skip-permissions".into(), vec![])
        );
        assert_eq!(
            split("--model opus --effort high --permission-mode acceptEdits --verbose"),
            (
                "--model opus --effort high --permission-mode acceptEdits --verbose".into(),
                vec![]
            )
        );
        // Attached values come apart; a short name keeps its form.
        assert_eq!(
            split("--model=sonnet -nwork -d api"),
            ("--model sonnet -n work -d api".into(), vec![])
        );
        // A required value may look like an option.
        assert_eq!(
            split("--append-system-prompt -x"),
            ("--append-system-prompt -x".into(), vec![])
        );
    }

    #[test]
    fn leaves_out_the_session_and_one_shot_options() {
        let (keep, dropped) =
            split("-c --resume 1234 --fork-session --session-id abc -p --output-format json");
        assert_eq!(keep, "");
        assert_eq!(
            dropped,
            vec![
                "-c",
                "--resume 1234",
                "--fork-session",
                "--session-id abc",
                "-p",
                "--output-format json"
            ]
        );
        // `--resume` without a value leaves the next option alone.
        assert_eq!(
            split("-r --dangerously-skip-permissions"),
            (
                "--dangerously-skip-permissions".into(),
                vec!["-r".to_string()]
            )
        );
        assert_eq!(
            split("--worktree=feature --verbose"),
            ("--verbose".into(), vec!["--worktree=feature".to_string()])
        );
    }

    #[test]
    fn leaves_out_the_prompt_and_unknown_options() {
        assert_eq!(
            split("--model opus fix the bug"),
            (
                "--model opus".into(),
                vec!["fix".to_string(), "the".into(), "bug".into()]
            )
        );
        assert_eq!(
            split("--frobnicate 3 --verbose -- --model x"),
            (
                "--verbose".into(),
                vec![
                    "--frobnicate".to_string(),
                    "3".into(),
                    "-- --model x".into()
                ]
            )
        );
        // A flag given a value isn't one Claude took.
        assert_eq!(
            split("--verbose=yes"),
            ("".into(), vec!["--verbose=yes".to_string()])
        );
        // A required value that's missing.
        assert_eq!(split("--model"), ("".into(), vec!["--model".to_string()]));
    }

    #[test]
    fn list_options_take_arguments_up_to_the_next_option() {
        assert_eq!(
            split("--add-dir ../a ../b --allowedTools Bash Edit --model opus"),
            (
                "--add-dir ../a ../b --allowedTools Bash Edit --model opus".into(),
                vec![]
            )
        );
        // Like Claude, a list swallows a prompt after it.
        assert_eq!(
            split("--tools Read hello"),
            ("--tools Read hello".into(), vec![])
        );
        assert_eq!(
            split("--file a:b c:d --verbose"),
            ("--verbose".into(), vec!["--file a:b c:d".to_string()])
        );
        assert_eq!(split("--add-dir=x y"), ("--add-dir x y".into(), vec![]));
    }

    #[test]
    fn values_that_cannot_be_typed_back_are_left_out() {
        let argv = vec![
            "--settings".to_string(),
            r#"{"model":"opus"}"#.to_string(),
            "--append-system-prompt".to_string(),
            "line one\nline two".to_string(),
            "--system-prompt=a\tb".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        let (keep, dropped) = resume_args(&argv);
        assert_eq!(keep, vec!["--model", "opus"]);
        assert_eq!(
            dropped,
            vec![
                r#"--settings {"model":"opus"}"#.to_string(),
                "--append-system-prompt line one\nline two".to_string(),
                "--system-prompt=a\tb".to_string(),
            ]
        );
        // Spaces are fine: they're quoted when typed.
        let argv = vec!["--append-system-prompt".to_string(), "be brief".to_string()];
        assert_eq!(resume_args(&argv).0, argv);
    }

    #[test]
    fn claude_args_skip_the_program_or_the_runtime_and_script() {
        let v = |s: &[&str]| s.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            claude_args(&v(&[r"C:\Users\u\.local\bin\claude.exe", "--verbose"])),
            v(&["--verbose"])
        );
        assert_eq!(
            claude_args(&v(&[
                r"C:\Program Files\nodejs\node.exe",
                "--no-warnings",
                r"C:\npm\node_modules\@anthropic-ai\claude-code\cli.js",
                "--model",
                "opus"
            ])),
            v(&["--model", "opus"])
        );
        assert_eq!(claude_args(&v(&["node"])), v(&[]));
        assert_eq!(claude_args(&[]), v(&[]));
    }

    #[test]
    fn a_session_file_belongs_to_the_process_that_wrote_it() {
        let file = |proc_start: Option<&str>, started_at: Option<u64>| SessionFile {
            pid: 1,
            session_id: "766ac475-7d95-4f55-96ac-e885e0a6a577".into(),
            cwd: "C:\\x".into(),
            started_at,
            proc_start: proc_start.map(str::to_string),
            kind: None,
        };
        // Windows: the creation time, as a FILETIME (from a real file).
        let real = file(Some("134381648921920572"), Some(1_793_691_293_454));
        assert!(written_by(&real, 1_793_691_292));
        assert!(
            !written_by(&real, 1_793_691_400),
            "a later process with the id"
        );
        // Elsewhere: no older than the session.
        let unix = file(Some("12345"), Some(1_793_691_293_454));
        assert!(written_by(&unix, 1_793_691_290));
        assert!(written_by(&unix, 1_793_691_294));
        assert!(!written_by(&unix, 1_793_691_300));
        assert!(!written_by(&file(None, None), 1));
    }

    #[test]
    fn session_files_are_read_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sessions")).unwrap();
        let write = |pid: u32, body: &str| {
            fs::write(
                dir.path().join("sessions").join(format!("{pid}.json")),
                body,
            )
            .unwrap();
        };
        write(
            10,
            r#"{"pid":10,"sessionId":"766ac475-7d95-4f55-96ac-e885e0a6a577","cwd":"C:\\p","startedAt":5,"kind":"interactive","extra":1}"#,
        );
        write(
            11,
            r#"{"pid":12,"sessionId":"766ac475-7d95-4f55-96ac-e885e0a6a577","cwd":"C:\\p"}"#,
        );
        write(13, r#"{"pid":13,"sessionId":"../../etc","cwd":"C:\\p"}"#);
        write(
            14,
            r#"{"pid":14,"sessionId":"766ac475-7d95-4f55-96ac-e885e0a6a577","cwd":"C:\\p","kind":"print"}"#,
        );
        fs::write(dir.path().join("sessions").join("10.abc.key"), "x").unwrap();
        assert_eq!(session_pids(dir.path()), HashSet::from([10, 11, 13, 14]));
        let file = read_session(dir.path(), 10).unwrap();
        assert_eq!(file.session_id, "766ac475-7d95-4f55-96ac-e885e0a6a577");
        assert_eq!(file.cwd, "C:\\p");
        assert!(read_session(dir.path(), 11).is_none(), "another process's");
        assert!(read_session(dir.path(), 13).is_none(), "not a session id");
        assert!(read_session(dir.path(), 14).is_none(), "not interactive");
        assert!(read_session(dir.path(), 15).is_none(), "no file");
    }

    #[test]
    fn transcripts_are_found_by_folder() {
        let dir = tempfile::tempdir().unwrap();
        let id = "766ac475-7d95-4f55-96ac-e885e0a6a577";
        let project = dir.path().join("projects").join("C--Projects-CorePty-Net");
        fs::create_dir_all(&project).unwrap();
        assert!(!has_transcript(dir.path(), r"C:\Projects\CorePty.Net", id));
        fs::write(project.join(format!("{id}.jsonl")), "{}").unwrap();
        assert!(has_transcript(dir.path(), r"C:\Projects\CorePty.Net", id));
        // Under another folder name too.
        assert!(has_transcript(dir.path(), r"D:\elsewhere", id));
    }

    #[test]
    fn folders_are_tidied() {
        assert_eq!(
            tidy_folder(Path::new(r"C:\Users\u\")).unwrap(),
            r"C:\Users\u"
        );
        assert_eq!(tidy_folder(Path::new(r"C:\")).unwrap(), r"C:\");
        assert_eq!(tidy_folder(Path::new("/home/u/")).unwrap(), "/home/u");
        assert_eq!(tidy_folder(Path::new("/")).unwrap(), "/");
        assert!(tidy_folder(Path::new("")).is_none());
    }

    #[test]
    fn uuids() {
        assert!(is_uuid("766ac475-7d95-4f55-96ac-e885e0a6a577"));
        assert!(!is_uuid("766ac475-7d95-4f55-96ac-e885e0a6a57"));
        assert!(!is_uuid("766ac475x7d95-4f55-96ac-e885e0a6a577"));
        assert!(!is_uuid("766ac475-7d95-4f55-96ac-e885e0a6a57g"));
    }

    /// This process's own tree: a child started by a test has no session file.
    #[test]
    fn inspecting_a_live_shell() {
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                ["/c", "ping -n 3 127.0.0.1 > nul"]
            } else {
                ["-c", "sleep 2"]
            })
            .current_dir(std::env::temp_dir())
            .spawn()
            .unwrap();
        let mut sys = System::new();
        let found = inspect(&mut sys, &[("t".to_string(), child.id())]);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "t");
        assert!(found[0].claude.is_none());
        let cwd = found[0].cwd.as_deref().expect("a folder");
        let expected = tidy_folder(&std::env::temp_dir()).unwrap();
        assert_eq!(
            dunce::canonicalize(cwd).unwrap(),
            dunce::canonicalize(expected).unwrap()
        );
        // A process that's gone is left out.
        assert!(inspect(&mut sys, &[("t".to_string(), child.id())]).is_empty());
    }

    /// A shell in a shell (like Git Bash's launcher and bash): the inner one's
    /// folder is the tab's.
    #[test]
    fn the_folder_is_the_inner_shells() {
        let mut outer = if cfg!(windows) {
            let mut cmd = std::process::Command::new("cmd");
            #[cfg(windows)]
            std::os::windows::process::CommandExt::raw_arg(
                &mut cmd,
                r#"/c cmd /c "cd /d C:\Windows && ping -n 4 127.0.0.1 > nul""#,
            );
            cmd
        } else {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "sh -c 'cd / && sleep 3; true'; true"]);
            cmd
        };
        let mut outer = outer.current_dir(std::env::temp_dir()).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let found = inspect(&mut System::new(), &[("t".to_string(), outer.id())]);
        let _ = outer.kill();
        let _ = outer.wait();
        let expected = if cfg!(windows) { r"C:\Windows" } else { "/" };
        assert_eq!(found[0].cwd.as_deref(), Some(expected));
    }
}

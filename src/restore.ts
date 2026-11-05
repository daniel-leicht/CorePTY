// "Restore my tabs": a snapshot of the open tabs (what each runs, its folder,
// the Claude Code session in it) and the commands that bring them back. Pure
// functions; the app takes the snapshots and opens the tabs (restore-ui.ts).

import type { FsLocationView, LocalShell, TabContext } from "./ipc";
import type { LaunchSpec } from "./spec";

export const SNAPSHOT_VERSION = 1;

/** A Claude Code session to start again (see `ClaudeContext`). */
export interface SnapshotClaude {
  sessionId: string;
  cwd: string;
  args: string[][];
  dropped: string[];
  resumable: boolean;
}

/** What each tab was, enough to open it again. `label` is the title it showed. */
export type SnapshotTab =
  | {
      kind: "local";
      shell: LocalShell;
      label: string;
      pinned?: string;
      cwd?: string;
      claude?: SnapshotClaude;
    }
  | { kind: "ssh"; savedId: string; label: string; pinned?: string }
  | {
      kind: "telnet";
      savedId?: string;
      host: string;
      port: number;
      name: string;
      label: string;
      pinned?: string;
    }
  | {
      kind: "files";
      label: string;
      pinned?: string;
      left?: FsLocationView;
      right?: FsLocationView;
    }
  /** Shown in the restore dialog, never reopened. */
  | { kind: "skipped"; label: string; pinned?: string; reason: string };

export interface Snapshot {
  version: typeof SNAPSHOT_VERSION;
  /** When it was taken (ms since 1970). */
  savedAt: number;
  /** The active tab's index (-1: none). */
  active: number;
  tabs: SnapshotTab[];
}

/** The facts about an open tab that a snapshot needs (gathered by the app). */
export type TabFacts =
  | {
      kind: "terminal";
      spec: LaunchSpec | null;
      /** The backend session, while it runs. */
      sessionId: string | null;
      label: string;
      pinned: string | null;
      /** Its OSC 7 folder, if the shell reports one. */
      osc7: { path: string; host: string } | null;
      /** A Claude session a restore is bringing back here, until it's seen running. */
      pendingClaude?: SnapshotClaude | null;
    }
  | {
      kind: "files";
      label: string;
      pinned: string | null;
      locations: [FsLocationView | undefined, FsLocationView | undefined];
    };

/** What the machine is: to read OSC 7 folders and quote commands. */
export interface HostFacts {
  windows: boolean;
  /** Its names, lowercase (`host_names`). */
  names: string[];
}

// ---- taking a snapshot --------------------------------------------------------

/**
 * A local path from an OSC 7 folder (`/C:/Users/me` → `C:\Users\me` on
 * Windows), or null if a shell on another machine reported it (an `ssh` in
 * the tab) or it isn't a path here.
 */
export function localOsc7Path(path: string, host: string, machine: HostFacts): string | null {
  if (host && !machine.names.includes(host.toLowerCase())) return null;
  if (!machine.windows) return path.startsWith("/") ? path : null;
  const drive = /^\/([A-Za-z]:)(\/.*)?$/.exec(path);
  if (drive) return drive[1].toUpperCase() + (drive[2] ?? "/").replace(/\//g, "\\");
  const unc = /^\/\/([^/]+)\/([^/]+)(\/.*)?$/.exec(path);
  if (unc) return `\\\\${unc[1]}\\${unc[2]}${(unc[3] ?? "").replace(/\//g, "\\")}`;
  return null;
}

/**
 * The folder to reopen a local shell in. PowerShell reports its own (its
 * process's folder doesn't follow `cd`); other shells' process folder is
 * exact, and what their prompt reports may be in another form (Git Bash's
 * `/c/Users`).
 */
export function localFolder(
  shell: string,
  reported: string | null,
  processCwd: string | undefined
): string | undefined {
  const reports = shell === "powershell" || shell === "pwsh";
  return (reports ? reported ?? processCwd : processCwd ?? reported) ?? undefined;
}

/** The snapshot of these tabs (`contexts`: what the local ones are doing, by session id). */
export function buildSnapshot(
  tabs: TabFacts[],
  active: number,
  contexts: Map<string, TabContext>,
  machine: HostFacts,
  now = Date.now()
): Snapshot {
  return {
    version: SNAPSHOT_VERSION,
    savedAt: now,
    active,
    tabs: tabs.map((tab) => snapshotTab(tab, contexts, machine)),
  };
}

/**
 * A tab title without the spinner a busy program animates at its start
 * (Claude Code's stars, braille dots): it isn't a change worth saving.
 */
export function steadyLabel(title: string): string {
  return title.replace(/^[\u2800-\u28ff\u2722-\u273d\u00b7*\s]+/u, "") || title;
}

function snapshotTab(tab: TabFacts, contexts: Map<string, TabContext>, machine: HostFacts): SnapshotTab {
  const named = { label: steadyLabel(tab.label), ...(tab.pinned !== null ? { pinned: tab.pinned } : {}) };
  if (tab.kind === "files") {
    const local = (location: FsLocationView | undefined) =>
      location?.fs === "local" ? { fs: location.fs, path: location.path, display: location.display } : undefined;
    const [left, right] = tab.locations.map(local);
    return { kind: "files", ...named, ...(left ? { left } : {}), ...(right ? { right } : {}) };
  }
  const spec = tab.spec;
  if (!spec) return { kind: "skipped", label: named.label, reason: "can't be reopened" };
  if (spec.kind === "local") {
    if (spec.elevated) return { kind: "skipped", label: named.label, reason: "runs as Administrator" };
    const context = tab.sessionId ? contexts.get(tab.sessionId) : undefined;
    const reported = tab.osc7 ? localOsc7Path(tab.osc7.path, tab.osc7.host, machine) : null;
    const cwd = localFolder(spec.shell, reported, context?.cwd);
    const claude = context?.claude ?? tab.pendingClaude ?? undefined;
    return {
      kind: "local",
      shell: spec.shell,
      ...named,
      ...(cwd ? { cwd } : {}),
      ...(claude
        ? {
            claude: {
              sessionId: claude.sessionId,
              cwd: claude.cwd,
              args: claude.args,
              dropped: claude.dropped,
              resumable: claude.resumable,
            },
          }
        : {}),
    };
  }
  const form = spec.form;
  if (spec.kind === "ssh") {
    // Its login is in the saved connection (and the keychain), never here.
    if (!form.id) return { kind: "skipped", label: named.label, reason: "not a saved connection" };
    return { kind: "ssh", savedId: form.id, ...named };
  }
  return {
    kind: "telnet",
    ...(form.id ? { savedId: form.id } : {}),
    host: form.host,
    port: form.port,
    name: form.name,
    ...named,
  };
}

/** What a snapshot says, without when it was taken: equal keys, nothing to save. */
export function snapshotKey(snapshot: Snapshot): string {
  return JSON.stringify({ ...snapshot, savedAt: 0 });
}

// ---- reading one back -------------------------------------------------------

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const isString = (value: unknown): value is string => typeof value === "string";
const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/** A saved snapshot, checked (the file may be old, edited or damaged): null if unusable. */
export function parseSnapshot(raw: unknown): Snapshot | null {
  if (!isObject(raw) || raw.version !== SNAPSHOT_VERSION || !Array.isArray(raw.tabs)) return null;
  const tabs = raw.tabs.map(parseTab).filter((tab): tab is SnapshotTab => tab !== null);
  const active = typeof raw.active === "number" && Number.isInteger(raw.active) ? raw.active : -1;
  return {
    version: SNAPSHOT_VERSION,
    savedAt: typeof raw.savedAt === "number" ? raw.savedAt : 0,
    active: active >= 0 && active < tabs.length ? active : -1,
    tabs,
  };
}

function parseTab(raw: unknown): SnapshotTab | null {
  if (!isObject(raw) || !isString(raw.label)) return null;
  const named = { label: raw.label, ...(isString(raw.pinned) ? { pinned: raw.pinned } : {}) };
  switch (raw.kind) {
    case "local": {
      if (!isString(raw.shell) || !raw.shell) return null;
      const claude = parseClaude(raw.claude);
      return {
        kind: "local",
        shell: raw.shell,
        ...named,
        ...(isString(raw.cwd) && raw.cwd ? { cwd: raw.cwd } : {}),
        ...(claude ? { claude } : {}),
      };
    }
    case "ssh":
      return isString(raw.savedId) && raw.savedId ? { kind: "ssh", savedId: raw.savedId, ...named } : null;
    case "telnet": {
      const port = raw.port;
      if (!isString(raw.host) || !raw.host || typeof port !== "number" || !Number.isInteger(port)) return null;
      if (port < 1 || port > 65535) return null;
      return {
        kind: "telnet",
        ...(isString(raw.savedId) && raw.savedId ? { savedId: raw.savedId } : {}),
        host: raw.host,
        port,
        name: isString(raw.name) ? raw.name : "",
        ...named,
      };
    }
    case "files": {
      const left = parseLocation(raw.left);
      const right = parseLocation(raw.right);
      return { kind: "files", ...named, ...(left ? { left } : {}), ...(right ? { right } : {}) };
    }
    case "skipped":
      return isString(raw.reason) ? { kind: "skipped", label: raw.label, reason: raw.reason } : null;
    default:
      return null;
  }
}

function parseClaude(raw: unknown): SnapshotClaude | undefined {
  if (!isObject(raw) || !isString(raw.sessionId) || !UUID.test(raw.sessionId)) return undefined;
  if (!isString(raw.cwd) || !raw.cwd) return undefined;
  const args = Array.isArray(raw.args)
    ? raw.args.filter(
        (option): option is string[] =>
          Array.isArray(option) && option.length > 0 && option.every(isString) && option[0].startsWith("-")
      )
    : [];
  const dropped = Array.isArray(raw.dropped) ? raw.dropped.filter(isString) : [];
  return { sessionId: raw.sessionId, cwd: raw.cwd, args, dropped, resumable: raw.resumable === true };
}

function parseLocation(raw: unknown): FsLocationView | undefined {
  if (!isObject(raw) || raw.fs !== "local" || !isString(raw.path) || !isString(raw.display)) return undefined;
  return { fs: "local", path: raw.path, display: raw.display };
}

// ---- starting Claude again ------------------------------------------------------

export type Quoting = "cmd" | "powershell" | "posix";

export function quotingFor(shell: string): Quoting {
  if (shell === "cmd") return "cmd";
  if (shell === "powershell" || shell === "pwsh") return "powershell";
  return "posix";
}

/** `word` as typed in this shell, or null if it can't be typed there as is. */
export function quote(word: string, quoting: Quoting, windows: boolean): string | null {
  // One line, and no double quotes: Command Prompt and Windows PowerShell
  // can't hand those to a program reliably.
  if (/[\u0000-\u001f\u007f"]/.test(word)) return null;
  switch (quoting) {
    case "cmd":
      // %NAME% is expanded even inside quotes.
      if (word.includes("%")) return null;
      if (/^[A-Za-z0-9_\-.,:=/\\+@]+$/.test(word)) return word;
      // A closing `\"` would be read as a quote in the value.
      if (word === "" || word.endsWith("\\")) return word === "" ? '""' : null;
      return `"${word}"`;
    case "powershell":
      if (/^[A-Za-z0-9_\-.:=/\\]+$/.test(word)) return word;
      // Windows PowerShell drops an empty argument, and passes `a b\` on as
      // "a b\" (an escaped quote).
      if (windows && (word === "" || (/\s/.test(word) && word.endsWith("\\")))) return null;
      // PowerShell takes typographic single quotes for quotes too.
      return `'${word.replace(/['\u2018\u2019\u201a\u201b]/g, "$&$&")}'`;
    case "posix":
      if (/^[A-Za-z0-9_\-.,:/+@][A-Za-z0-9_\-.,:=/+@%]*$/.test(word)) return word;
      return `'${word.replace(/'/g, "'\\''")}'`;
  }
}

export interface ClaudeLine {
  /** What to type (without the Enter). */
  line: string;
  /** Options left out because this shell can't take them, as typed. */
  left: string[];
}

/**
 * The line that starts this Claude session again in `shell`:
 * `claude --resume <id> <options>`, or just `claude <options>` if the
 * conversation was never saved (it had no messages yet).
 */
export function claudeLine(claude: SnapshotClaude, shell: string, windows: boolean): ClaudeLine {
  const quoting = quotingFor(shell);
  const words = ["claude"];
  if (claude.resumable) words.push("--resume", claude.sessionId);
  const left: string[] = [];
  for (const option of claude.args) {
    const quoted = option.map((word) => quote(word, quoting, windows));
    if (quoted.every((word): word is string => word !== null)) words.push(...quoted);
    else left.push(option.join(" "));
  }
  return { line: words.join(" "), left };
}

/** The folder a local tab reopens in: Claude's, if it ran there. */
export function startFolder(tab: Extract<SnapshotTab, { kind: "local" }>): string | undefined {
  return tab.claude?.cwd ?? tab.cwd;
}

// Deterministic README screenshot scene. This runs only from
// scripts/showcase.html and talks to the real UI through Tauri's supported IPC
// mocks, so screenshots exercise the same app shell and xterm renderer as the
// desktop build without launching a native session or exposing local data.
import "./fonts.css";
import "./styles.css";
import "@xterm/xterm/css/xterm.css";
import "../scripts/showcase.css";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type { ConnForm } from "./dialog";
import { REMOTE_CONNECTION, ShowcaseFs } from "./showcase-fs";

const markCaptureStage = (stage: string): void => {
  document.documentElement.dataset.captureStage = stage;
};
markCaptureStage("module-loaded");

const params = new URLSearchParams(location.search);
const theme = params.get("theme") ?? "corepty-dark";
const scene = params.get("scene") ?? "terminal";
const sessionIds: string[] = [];
// `?big=N` adds C:\big with N files; `?step=ms` paces simulated transfers;
// `?partial=1` presents clashes as partial copies (the Resume dialog).
// The SFTP scene holds its upload part-way, for a still frame.
const showcaseFs = new ShowcaseFs({
  bigFolder: Number(params.get("big")) || 0,
  stepMs: Number(params.get("step")) || undefined,
  partialConflicts: params.get("partial") === "1",
  holdAt: scene === "sftp" ? 0.5 : undefined,
});

const folders = [
  { id: "production", name: "Production", parentId: null },
  { id: "homelab", name: "Homelab", parentId: null },
];

const sessions = [
  {
    id: "edge-01",
    name: "edge-01",
    kind: "ssh",
    host: "edge-01.example.net",
    port: 22,
    username: "deploy",
    authType: "key",
    keyPath: null,
    saveSecret: false,
    folderId: "production",
    order: 0,
  },
  {
    id: "database",
    name: "database",
    kind: "ssh",
    host: "10.24.0.12",
    port: 22,
    username: "postgres",
    authType: "key",
    keyPath: null,
    saveSecret: false,
    folderId: "production",
    order: 1,
  },
  {
    id: "nas",
    name: "nas",
    kind: "ssh",
    host: "nas.home.arpa",
    port: 22,
    username: "admin",
    authType: "password",
    keyPath: null,
    saveSecret: true,
    folderId: "homelab",
    order: 0,
  },
  {
    id: "core-switch",
    name: "core-switch",
    kind: "telnet",
    host: "192.168.1.23",
    port: 23,
    username: null,
    authType: null,
    keyPath: null,
    saveSecret: false,
    folderId: "homelab",
    order: 1,
  },
] as const;

/** The restore scene's last session: Claude tabs, a shell, an admin tab, SSH, files. */
const RESUMED = "766ac475-7d95-4f55-96ac-e885e0a6a577";
const OPEN_ELSEWHERE = "f83afb2c-17e2-4435-8a61-8fd18d89793a";
const restoreSnapshot = {
  version: 1,
  savedAt: Date.now() - 7 * 60_000,
  active: 0,
  tabs: [
    {
      kind: "local",
      shell: "cmd",
      label: "✳ Refactor the transfer queue",
      cwd: "C:\\Projects\\CorePTY",
      claude: {
        sessionId: RESUMED,
        cwd: "C:\\Projects\\CorePTY",
        args: [["--dangerously-skip-permissions"], ["--model", "opus"]],
        dropped: ["--continue"],
        resumable: true,
      },
    },
    { kind: "local", shell: "powershell", label: "PowerShell", pinned: "builds", cwd: "C:\\Users\\dev\\Downloads" },
    {
      kind: "local",
      shell: "cmd",
      label: "✳ Claude Code",
      cwd: "C:\\Projects\\site",
      claude: { sessionId: OPEN_ELSEWHERE, cwd: "C:\\Projects\\site", args: [], dropped: [], resumable: true },
    },
    { kind: "skipped", label: "PowerShell (Admin)", reason: "runs as Administrator" },
    { kind: "ssh", savedId: "edge-01", label: "edge-01" },
    // A password to type: asked for after the others are open.
    { kind: "ssh", savedId: "nas", label: "admin@nas", pinned: "backups" },
    {
      kind: "files",
      label: "Files",
      left: { fs: "local", path: "m:C:\\Projects\\CorePTY", display: "C:\\Projects\\CorePTY" },
    },
  ],
};
/** What the app sent: checked by scripted runs. */
const ipcLog: { command: string; args: unknown }[] = [];
(window as unknown as { __ipcLog?: unknown }).__ipcLog = ipcLog;

mockWindows("main");
mockIPC(
  (command, args) => {
    const filesystem = showcaseFs.handle(command, args as Record<string, unknown> | undefined);
    if (filesystem !== undefined) return filesystem;
    if (/^(session_create|session_write|tabs_)/.test(command)) ipcLog.push({ command, args });
    switch (command) {
      case "host_names":
        return ["devbox", "localhost"];
      case "claude_running":
        return scene === "restore" ? [OPEN_ELSEWHERE] : [];
      case "tabs_context":
        return [];
      case "tabs_snapshot_load":
        return scene === "restore" ? { snapshot: restoreSnapshot, live: false } : null;
      case "tabs_snapshot_save":
        return null;
      case "settings_load":
        return { theme, fontSize: 14, lineHeight: 1.18, cursorBlink: false };
      case "settings_save":
      case "session_write":
      case "session_resize":
      case "session_close":
        return null;
      case "list_local_shells":
        return [
          { id: "powershell", label: "PowerShell", hint: "Windows PowerShell 5.1", icon: "powershell" },
          { id: "pwsh", label: "PowerShell 7", hint: "Cross-platform pwsh", icon: "pwsh" },
          { id: "cmd", label: "Command Prompt", hint: "cmd.exe", icon: "cmd" },
          { id: "bash", label: "Bash", hint: "Git Bash / WSL", icon: "bash" },
        ];
      case "host_os":
        return "windows";
      case "folders_load":
        return folders;
      case "sessions_load":
        return sessions;
      case "session_create_local":
      case "session_create_ssh": {
        const options = (args as unknown as Record<string, unknown> | undefined)?.options as
          | { id?: string; title?: string }
          | undefined;
        const id = options?.id ?? `showcase-${sessionIds.length + 1}`;
        sessionIds.push(id);
        return {
          id,
          kind: command === "session_create_ssh" ? "ssh" : "local",
          title: options?.title ?? (command === "session_create_ssh" ? "edge-01" : "PowerShell 7"),
          // The SSH tab shares its connection with SFTP panels.
          connectionId: command === "session_create_ssh" ? REMOTE_CONNECTION : undefined,
        };
      }
      default:
        return null;
    }
  },
  { shouldMockEvents: true }
);

try {
  localStorage.clear();
} catch {
  // Storage can be unavailable under unusually strict browser policies.
}

const { App } = await import("./app");
const root = document.querySelector<HTMLDivElement>("#app")!;
const app = new App(root);
// Scripted checks reach the app (its tabs and terminals) through this.
(window as unknown as { __showcaseApp?: unknown }).__showcaseApp = app;
await app.mount();
markCaptureStage("app-mounted");

if (scene === "files") await renderFileManagerShowcase();
else if (scene === "sftp") await renderSftpShowcase();
else if (scene === "restore") await renderRestoreShowcase();
else await renderTerminalShowcase();

/** The restore dialog CorePTY shows at startup, over the empty window. */
async function renderRestoreShowcase(): Promise<void> {
  for (let attempt = 0; attempt < 80 && !document.querySelector(".restore-dialog"); attempt += 1) {
    await new Promise<void>((resolve) => setTimeout(resolve, 25));
  }
  markCaptureStage("restore-ready");
  document.documentElement.dataset.showcaseReady = "true";
}

/** Local downloads on the left, the server's releases on the right, an upload running. */
async function renderSftpShowcase(): Promise<void> {
  // "Open SFTP" comes from an SSH tab: the panel shares its connection.
  await app.newSsh({
    name: "edge-01",
    kind: "ssh",
    host: "edge-01.example.net",
    port: 22,
    username: "deploy",
    authType: "key",
    password: "",
    keyPath: "C:\Users\dev\.ssh\id_ed25519",
    passphrase: "",
    saveSecret: false,
    folderId: "production",
  });
  for (const session of (app as unknown as { tabs: Array<{ setStatus?: (status: "connected") => void }> }).tabs) {
    session.setStatus?.("connected");
  }
  const local = { fs: "local", path: "m:C:\\Users\\dev\\Downloads", display: "C:\\Users\\dev\\Downloads" };
  app.newFileManager([local, undefined], { side: 1, target: { kind: "live", connectionId: REMOTE_CONNECTION } });
  markCaptureStage("sftp-opened");
  const panel = (side: number) =>
    document.querySelector<HTMLElement>(`.file-manager.is-active .fm-panel[data-panel="${side}"]`);
  const rows = (side: number) => [...(panel(side)?.querySelectorAll<HTMLElement>(".fm-row") ?? [])];
  const named = (side: number, name: string) =>
    rows(side).find((row) => row.querySelector(".fm-row__name")?.textContent === name);
  const waitUntil = async (test: () => boolean) => {
    for (let attempt = 0; attempt < 120 && !test(); attempt += 1) {
      await new Promise<void>((resolve) => setTimeout(resolve, 25));
    }
  };
  const need = <T>(value: T | null | undefined, what: string): T => {
    if (value === null || value === undefined) {
      const panels = [...document.querySelectorAll<HTMLElement>(".fm-panel")].map(
        (each) => `${each.dataset.panel}:${each.querySelector<HTMLInputElement>(".fm-path")?.value}:${each.querySelectorAll(".fm-row").length}`
      );
      throw new Error(`SFTP scene: no ${what} (panels ${panels.join(", ")}; focus ${document.activeElement?.className})`);
    }
    return value;
  };
  await waitUntil(() => rows(0).length >= 5 && rows(1).length >= 3);
  // The right panel goes to the releases folder.
  const path = need(panel(1)?.querySelector<HTMLInputElement>(".fm-path"), "right path field");
  path.value = "/home/deploy/app/releases";
  path.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  await waitUntil(() => !!named(1, "checksums.txt"));
  // Mark the two packages and send them up (F5, then Enter in the prompt).
  for (const name of ["corepty-0.4.0-x64-setup.exe", "corepty-portable.zip"]) {
    named(0, name)?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, ctrlKey: true }));
  }
  const grid = need(panel(0)?.querySelector<HTMLElement>(".fm-grid"), "left grid");
  grid.focus();
  const press = (target: Element, key: string) =>
    target.dispatchEvent(new KeyboardEvent("keydown", { key, code: key, bubbles: true, cancelable: true }));
  press(grid, "F5");
  await waitUntil(() => !!document.querySelector(".fm-prompt input"));
  const prompt = need(document.querySelector<HTMLInputElement>(".fm-prompt input"), "copy prompt");
  prompt.focus();
  press(prompt, "Enter");
  await waitUntil(() => /50%|of/.test(document.querySelector(".fm-job .fm-job__detail")?.textContent ?? ""));
  await waitUntil(() => !!document.querySelector(".fm-job.is-running"));
  // The cursor rests on a release folder, the left panel active.
  named(1, "0.3.8")?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
  need(panel(0)?.querySelector<HTMLElement>(".fm-grid"), "left grid").focus();
  await new Promise<void>((resolve) => setTimeout(resolve, 150));
  await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
  markCaptureStage("sftp-ready");
  document.documentElement.dataset.showcaseReady = "true";
}

async function renderFileManagerShowcase(): Promise<void> {
  const at = (display: string) => ({ fs: "local", path: `m:${display}`, display });
  app.newFileManager([at("C:\\Projects\\CorePTY"), at("C:\\Users\\dev\\Downloads")]);
  markCaptureStage("file-manager-opened");
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (document.querySelectorAll(".fm-row").length >= 14) break;
    await new Promise<void>((resolve) => setTimeout(resolve, 25));
  }
  // Show the Commander look: the cursor on src-tauri with two files marked.
  const left = '.fm-panel[data-panel="0"] .fm-row';
  for (const name of ["package.json", "README.md"]) {
    const row = [...document.querySelectorAll<HTMLElement>(left)].find(
      (each) => each.querySelector(".fm-row__name")?.textContent === name
    );
    row?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, ctrlKey: true }));
  }
  const cursor = [...document.querySelectorAll<HTMLElement>(left)].find(
    (each) => each.querySelector(".fm-row__name")?.textContent === "src-tauri"
  );
  cursor?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, ctrlKey: true }));
  cursor?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, ctrlKey: true }));
  await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
  markCaptureStage("file-manager-ready");
  document.documentElement.dataset.showcaseReady = "true";
}

async function renderTerminalShowcase(): Promise<void> {

// A background local tab makes the tab strip visibly useful; the active SSH
// tab matches the saved connection highlighted by the terminal content.
await app.newLocal("pwsh");
const sshForm: ConnForm = {
  name: "edge-01",
  kind: "ssh",
  host: "edge-01.example.net",
  port: 22,
  username: "deploy",
  authType: "key",
  password: "",
  keyPath: "C:\\Users\\dev\\.ssh\\id_ed25519",
  passphrase: "",
  saveSecret: false,
  folderId: "production",
};
await app.newSsh(sshForm);
markCaptureStage("sessions-opened");

type ShowcaseSession = {
  element: HTMLDivElement;
  fit: () => void;
  setStatus: (status: "connected") => void;
  term: {
    buffer: {
      active: {
        length: number;
        getLine: (index: number) => { translateToString: (trimRight?: boolean) => string } | undefined;
      };
    };
    rows: number;
    refresh: (start: number, end: number) => void;
    write: (data: string) => void;
  };
};

// The capture fixture writes through the real xterm instance after its box has
// completed layout. Backend PTY events can legitimately arrive before that in
// production, but a screenshot must not depend on font/render timing.
const showcaseApp = app as unknown as {
  tabs: ShowcaseSession[];
  active: ShowcaseSession | null;
};
for (const session of showcaseApp.tabs) session.setStatus("connected");
const active = showcaseApp.active!;
for (let frame = 0; frame < 3; frame += 1) {
  await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
}
active.fit();
active.term.write(showcaseOutput());
markCaptureStage("output-enqueued");
let parsed = false;
for (let attempt = 0; attempt < 40; attempt += 1) {
  const buffer = active.term.buffer.active;
  for (let line = 0; line < Math.min(8, buffer.length); line += 1) {
    if (buffer.getLine(line)?.translateToString(true).includes("COREPTY")) parsed = true;
  }
  if (parsed) break;
  await new Promise<void>((resolve) => setTimeout(resolve, 25));
}
if (!parsed) throw new Error("xterm did not parse the showcase output");
markCaptureStage("output-parsed");
active.term.refresh(0, active.term.rows - 1);

// Let xterm parse the stream, load WebGL when available, and settle the final
// fit/repaint before the headless browser takes its screenshot.
await new Promise<void>((resolve) => setTimeout(resolve, 350));
document.documentElement.dataset.showcaseReady = "true";
}

function showcaseOutput(): string {
  const e = "\x1b[";
  const reset = `${e}0m`;
  const dim = `${e}2m`;
  const bold = `${e}1m`;
  const red = `${e}31m`;
  const green = `${e}32m`;
  const yellow = `${e}33m`;
  const blue = `${e}34m`;
  const magenta = `${e}35m`;
  const cyan = `${e}36m`;
  const white = `${e}97m`;
  const swatches = [40, 41, 42, 43, 44, 45, 46, 47, 100, 101, 102, 103, 104, 105, 106, 107]
    .map((code) => `${e}${code}m  `)
    .join("");

  return [
    `${e}2J${e}H${e}?25l`,
    `  ${cyan}${bold}╭─ COREPTY / REMOTE WORKSPACE ─────────────────────────────────────────────╮${reset}`,
    `  ${cyan}│${reset}  ${white}${bold}One window. Every shell. Anywhere.${reset}                                      ${cyan}│${reset}`,
    `  ${cyan}${bold}╰──────────────────────────────────────────────────────────────────────────╯${reset}`,
    "",
    `        ${blue}╭────────────╮${reset}         ${white}${bold}dev@edge-01${reset}`,
    `        ${blue}│${reset}            ${blue}│${reset}         ${dim}────────────────────────────${reset}`,
    `        ${blue}│${reset}    ${magenta}${bold}>${yellow}_${reset}      ${blue}│${reset}         ${cyan}OS${reset}        Ubuntu 24.04 LTS`,
    `        ${blue}│${reset}            ${blue}│${reset}         ${cyan}Host${reset}      edge-01 · production`,
    `        ${blue}╰────────────╯${reset}         ${cyan}Shell${reset}     zsh 5.9`,
    `                               ${cyan}Terminal${reset}  CorePTY · xterm.js`,
    `                               ${cyan}Link${reset}      SSH · Ed25519`,
    "",
    `        ${dim}ANSI palette${reset}   ${swatches}${reset}`,
    `  ${blue}╭─${reset} ${green}dev@edge-01${reset}  ${cyan}~/services/corepty${reset}  ${magenta}git:main${reset}`,
    `  ${blue}╰─${reset}${yellow}❯${reset} npm run check`,
    `       ${green}✓${reset} ${bold}typecheck${reset}   ${dim}0 errors${reset}`,
    `       ${green}✓${reset} ${bold}tests${reset}       ${green}12 passed${reset}`,
    `       ${green}✓${reset} ${bold}bundle${reset}      ${cyan}production ready${reset}`,
    "",
    `  ${blue}╭─${reset} ${green}dev@edge-01${reset}  ${cyan}~/services/corepty${reset}  ${magenta}git:main${reset}`,
    `  ${blue}╰─${reset}${yellow}❯${reset} git status --short --branch`,
    `       ${cyan}## main...origin/main${reset}`,
    `       ${yellow}M${reset}  src/terminal.ts`,
    `       ${red}M${reset}  docs/theme-*.png`,
    "",
    `  ${blue}╭─${reset} ${green}dev@edge-01${reset}  ${cyan}~/services/corepty${reset}`,
    `  ${blue}╰─${reset}${yellow}❯${reset} ${e}?25h`,
  ].join("\r\n");
}

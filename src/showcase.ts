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

const markCaptureStage = (stage: string): void => {
  document.documentElement.dataset.captureStage = stage;
};
markCaptureStage("module-loaded");

const params = new URLSearchParams(location.search);
const theme = params.get("theme") ?? "corepty-dark";
const scene = params.get("scene") ?? "terminal";
const sessionIds: string[] = [];

const leftLocation = {
  provider: "local",
  path: "C:\\Projects\\CorePTY",
  token: "demo:left",
};
const rightLocation = {
  provider: "local",
  path: "C:\\Users\\dev\\Downloads",
  token: "demo:right",
};
const demoEntries = {
  "demo:left": [
    demoEntry(".github", "directory", true, null, 1785517260000),
    demoEntry("docs", "directory", true, null, 1785516840000),
    demoEntry("src", "directory", true, null, 1785517320000),
    demoEntry("src-tauri", "directory", true, null, 1785517140000),
    demoEntry("package.json", "file", false, 1426, 1785517200000),
    demoEntry("README.md", "file", false, 12184, 1785517100000),
    demoEntry("LICENSE", "file", false, 35150, 1784090400000),
    demoEntry("vite.config.ts", "file", false, 1173, 1785517020000),
  ],
  "demo:right": [
    demoEntry("Screenshots", "directory", true, null, 1785516500000, "demo:right"),
    demoEntry("Releases", "directory", true, null, 1785516200000, "demo:right"),
    demoEntry("corepty-0.3.8-x64-setup.exe", "file", false, 8_734_912, 1785517300000, "demo:right"),
    demoEntry("corepty-portable.zip", "file", false, 7_218_432, 1785517240000, "demo:right"),
    demoEntry("terminal-themes.png", "file", false, 1_842_205, 1785516800000, "demo:right"),
    demoEntry("release-notes.txt", "file", false, 4821, 1785516600000, "demo:right"),
  ],
} as const;

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

mockWindows("main");
mockIPC(
  (command, args) => {
    switch (command) {
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
      case "files_home":
        return leftLocation;
      case "files_roots":
        return [
          { label: "Home", location: leftLocation },
          { label: "C:", location: { provider: "local", path: "C:\\", token: "demo:root" } },
        ];
      case "files_resolve": {
        const path = (args as { path?: string } | undefined)?.path ?? "";
        return path.includes("Downloads") ? rightLocation : leftLocation;
      }
      case "files_list": {
        const token = (args as { token?: string } | undefined)?.token ?? "demo:left";
        const location = token === "demo:right" ? rightLocation : leftLocation;
        return { location, parent: null, entries: demoEntries[token as keyof typeof demoEntries] ?? [] };
      }
      case "files_conflicts":
        return [];
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
await app.mount();
markCaptureStage("app-mounted");

if (scene === "files") await renderFileManagerShowcase();
else await renderTerminalShowcase();

async function renderFileManagerShowcase(): Promise<void> {
  app.newFileManager([leftLocation.path, rightLocation.path]);
  markCaptureStage("file-manager-opened");
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (document.querySelectorAll(".fm-row").length >= 12) break;
    await new Promise<void>((resolve) => setTimeout(resolve, 25));
  }
  const firstSource = document.querySelector<HTMLElement>('.fm-panel[data-panel="0"] .fm-row:nth-child(3)');
  firstSource?.click();
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

function demoEntry(
  name: string,
  kind: "directory" | "file",
  isDirectory: boolean,
  size: number | null,
  modified: number,
  parent = "demo:left"
) {
  return {
    name,
    path: `${parent === "demo:left" ? leftLocation.path : rightLocation.path}\\${name}`,
    token: `${parent}:${name}`,
    kind,
    isDirectory,
    size,
    modified,
    hidden: name.startsWith("."),
    readonly: false,
  };
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

import { describe, expect, it } from "vitest";
import type { ConnForm } from "./dialog";
import type { TabContext } from "./ipc";
import {
  buildSnapshot,
  claudeLine,
  localFolder,
  localOsc7Path,
  parseSnapshot,
  quote,
  snapshotKey,
  startFolder,
  type SnapshotClaude,
  type TabFacts,
} from "./restore";

const windows = { windows: true, names: ["desktop-a2e84k2", "localhost"] };
const linux = { windows: false, names: ["box", "box.lan", "localhost"] };
const id = "766ac475-7d95-4f55-96ac-e885e0a6a577";

const claude = (args: string[][], resumable = true): SnapshotClaude => ({
  sessionId: id,
  cwd: "C:\\Projects\\CorePty.Net",
  args,
  dropped: [],
  resumable,
});

const form = (extra: Partial<ConnForm>): ConnForm => ({
  name: "",
  kind: "ssh",
  host: "example.com",
  port: 22,
  username: "me",
  authType: "key",
  password: "",
  keyPath: "",
  passphrase: "",
  saveSecret: false,
  ...extra,
});

describe("OSC 7 folders", () => {
  it("become Windows paths", () => {
    expect(localOsc7Path("/C:/Users/me", "desktop-a2e84k2", windows)).toBe("C:\\Users\\me");
    expect(localOsc7Path("/c:/", "", windows)).toBe("C:\\");
    expect(localOsc7Path("/C:", "", windows)).toBe("C:\\");
    expect(localOsc7Path("//server/share/a b", "localhost", windows)).toBe("\\\\server\\share\\a b");
    // Git Bash's own form isn't one.
    expect(localOsc7Path("/c/Users/me", "", windows)).toBeNull();
  });

  it("from another machine are ignored", () => {
    expect(localOsc7Path("/C:/Users/me", "buildbox", windows)).toBeNull();
    expect(localOsc7Path("/home/me", "server", linux)).toBeNull();
    expect(localOsc7Path("/home/me", "BOX", linux)).toBe("/home/me");
    expect(localOsc7Path("/home/me", "", linux)).toBe("/home/me");
  });

  it("win for PowerShell; the process folder wins for other shells", () => {
    expect(localFolder("powershell", "C:\\a", "C:\\start")).toBe("C:\\a");
    expect(localFolder("pwsh", null, "C:\\start")).toBe("C:\\start");
    expect(localFolder("cmd", "C:\\a", "C:\\b")).toBe("C:\\b");
    expect(localFolder("bash", "C:\\a", undefined)).toBe("C:\\a");
    expect(localFolder("cmd", null, undefined)).toBeUndefined();
  });
});

describe("snapshots", () => {
  const contexts = new Map<string, TabContext>([
    ["s1", { id: "s1", cwd: "C:\\Projects\\CorePty.Net", claude: { ...claude([["--dangerously-skip-permissions"]]) } }],
    ["s2", { id: "s2", cwd: "C:\\Windows" }],
    ["s3", { id: "s3", cwd: "C:\\started-here" }],
  ]);
  const tabs: TabFacts[] = [
    { kind: "terminal", spec: { kind: "local", shell: "cmd" }, sessionId: "s1", label: "✳ Claude Code", pinned: null, osc7: null },
    { kind: "terminal", spec: { kind: "local", shell: "cmd" }, sessionId: "s2", label: "Command Prompt", pinned: "build", osc7: null },
    {
      kind: "terminal",
      spec: { kind: "local", shell: "powershell" },
      sessionId: "s3",
      label: "PowerShell",
      pinned: null,
      osc7: { path: "/C:/Users/me", host: "desktop-a2e84k2" },
    },
    { kind: "terminal", spec: { kind: "local", shell: "powershell", elevated: true }, sessionId: "s4", label: "PowerShell (Admin)", pinned: null, osc7: null },
    { kind: "terminal", spec: { kind: "ssh", form: form({ id: "saved-1" }) }, sessionId: "s5", label: "me@example.com", pinned: null, osc7: null },
    { kind: "terminal", spec: { kind: "ssh", form: form({}) }, sessionId: "s6", label: "quick", pinned: null, osc7: null },
    {
      kind: "terminal",
      spec: { kind: "telnet", form: form({ kind: "telnet", host: "bbs.example", port: 2323, name: "BBS" }) },
      sessionId: "s7",
      label: "BBS",
      pinned: null,
      osc7: null,
    },
    {
      kind: "files",
      label: "Files",
      pinned: null,
      locations: [
        { fs: "local", path: "C:\\x", display: "C:\\x" },
        { fs: "conn-1", path: "/srv", display: "/srv" },
      ],
    },
    // A shell that exited keeps the folder it reported.
    {
      kind: "terminal",
      spec: { kind: "local", shell: "powershell" },
      sessionId: null,
      label: "PowerShell",
      pinned: null,
      osc7: { path: "/D:/work", host: "" },
    },
  ];
  const snapshot = buildSnapshot(tabs, 2, contexts, windows, 1000);

  it("records what each tab runs, where, and the Claude session in it", () => {
    expect(snapshot.active).toBe(2);
    expect(snapshot.tabs).toEqual([
      {
        kind: "local",
        shell: "cmd",
        label: "✳ Claude Code",
        cwd: "C:\\Projects\\CorePty.Net",
        claude: claude([["--dangerously-skip-permissions"]]),
      },
      { kind: "local", shell: "cmd", label: "Command Prompt", pinned: "build", cwd: "C:\\Windows" },
      { kind: "local", shell: "powershell", label: "PowerShell", cwd: "C:\\Users\\me" },
      { kind: "skipped", label: "PowerShell (Admin)", reason: "runs as Administrator" },
      { kind: "ssh", savedId: "saved-1", label: "me@example.com" },
      { kind: "skipped", label: "quick", reason: "not a saved connection" },
      { kind: "telnet", host: "bbs.example", port: 2323, name: "BBS", label: "BBS" },
      { kind: "files", label: "Files", left: { fs: "local", path: "C:\\x", display: "C:\\x" } },
      { kind: "local", shell: "powershell", label: "PowerShell", cwd: "D:\\work" },
    ]);
  });

  it("never holds a password", () => {
    const secret = buildSnapshot(
      [{ kind: "terminal", spec: { kind: "ssh", form: form({ id: "x", password: "hunter2", passphrase: "p" }) }, sessionId: null, label: "x", pinned: null, osc7: null }],
      0,
      new Map(),
      windows
    );
    expect(JSON.stringify(secret)).not.toMatch(/hunter2|passphrase|password/);
  });

  it("read back as written", () => {
    expect(parseSnapshot(JSON.parse(JSON.stringify(snapshot)))).toEqual(snapshot);
  });

  it("compare without the time they were taken", () => {
    expect(snapshotKey(snapshot)).toBe(snapshotKey({ ...snapshot, savedAt: 5000 }));
    expect(snapshotKey(snapshot)).not.toBe(snapshotKey({ ...snapshot, active: 0 }));
  });

  it("that are damaged are cleaned up or refused", () => {
    expect(parseSnapshot(null)).toBeNull();
    expect(parseSnapshot({ version: 2, tabs: [] })).toBeNull();
    expect(parseSnapshot({ version: 1, tabs: "x" })).toBeNull();
    const parsed = parseSnapshot({
      version: 1,
      active: 7,
      tabs: [
        { kind: "local", label: "a" }, // no shell
        { kind: "local", shell: "cmd", label: "b", claude: { sessionId: "../x", cwd: "C:\\" } },
        {
          kind: "local",
          shell: "cmd",
          label: "c",
          claude: { sessionId: id, cwd: "C:\\p", args: [["--verbose"], ["oops"], [], "x", [1]], resumable: "yes" },
        },
        { kind: "telnet", label: "d", host: "h", port: 70000 },
        { kind: "files", label: "e", left: { fs: "conn", path: "/", display: "/" } },
        { kind: "rocket", label: "f" },
        { kind: "ssh", label: "g" },
      ],
    });
    expect(parsed).toEqual({
      version: 1,
      savedAt: 0,
      active: -1,
      tabs: [
        { kind: "local", shell: "cmd", label: "b" },
        {
          kind: "local",
          shell: "cmd",
          label: "c",
          claude: { sessionId: id, cwd: "C:\\p", args: [["--verbose"]], dropped: [], resumable: false },
        },
        { kind: "files", label: "e" },
      ],
    });
  });

  it("reopen a Claude tab in Claude's folder", () => {
    expect(startFolder({ kind: "local", shell: "cmd", label: "", cwd: "C:\\a", claude: claude([]) })).toBe(
      "C:\\Projects\\CorePty.Net"
    );
    expect(startFolder({ kind: "local", shell: "cmd", label: "", cwd: "C:\\a" })).toBe("C:\\a");
  });
});

describe("quoting", () => {
  it("leaves plain words alone", () => {
    for (const quoting of ["cmd", "powershell", "posix"] as const) {
      expect(quote("--dangerously-skip-permissions", quoting, true)).toBe("--dangerously-skip-permissions");
      expect(quote("claude-opus-5-5", quoting, true)).toBe("claude-opus-5-5");
      expect(quote("C:/x/y.json", quoting, true)).toBe("C:/x/y.json");
    }
  });

  it("quotes for Command Prompt", () => {
    expect(quote("be brief & kind", "cmd", true)).toBe('"be brief & kind"');
    expect(quote("C:\\My Dir\\", "cmd", true)).toBeNull();
    expect(quote("100%", "cmd", true)).toBeNull();
    expect(quote("", "cmd", true)).toBe('""');
  });

  it("quotes for PowerShell", () => {
    expect(quote("it's $HOME", "powershell", true)).toBe("'it''s $HOME'");
    expect(quote("a\u2019b", "powershell", true)).toBe("'a\u2019\u2019b'");
    expect(quote("a,b", "powershell", true)).toBe("'a,b'");
    expect(quote("@file", "powershell", true)).toBe("'@file'");
    expect(quote("C:\\My Dir\\", "powershell", true)).toBeNull();
    expect(quote("C:\\My Dir\\", "powershell", false)).toBe("'C:\\My Dir\\'");
    expect(quote("", "powershell", true)).toBeNull();
  });

  it("quotes for POSIX shells", () => {
    expect(quote("it's $HOME", "posix", false)).toBe("'it'\\''s $HOME'");
    expect(quote("=cmd", "posix", false)).toBe("'=cmd'");
    expect(quote("~/x", "posix", false)).toBe("'~/x'");
    expect(quote("*.ts", "posix", false)).toBe("'*.ts'");
  });

  it("refuses what can't be typed on one line", () => {
    for (const quoting of ["cmd", "powershell", "posix"] as const) {
      expect(quote("a\nb", quoting, false)).toBeNull();
      expect(quote('{"a":1}', quoting, false)).toBeNull();
    }
  });
});

describe("the claude line", () => {
  it("resumes the session with its options", () => {
    expect(claudeLine(claude([["--dangerously-skip-permissions"]]), "cmd", true)).toEqual({
      line: `claude --resume ${id} --dangerously-skip-permissions`,
      left: [],
    });
    expect(
      claudeLine(claude([["--model", "opus"], ["--append-system-prompt", "be brief"]]), "powershell", true).line
    ).toBe(`claude --resume ${id} --model opus --append-system-prompt 'be brief'`);
  });

  it("starts afresh with the same options if there was nothing to resume", () => {
    expect(claudeLine(claude([["--verbose"]], false), "bash", false).line).toBe("claude --verbose");
  });

  it("leaves out an option this shell can't take, whole", () => {
    expect(claudeLine(claude([["--add-dir", "ok", "100%"], ["--verbose"]]), "cmd", true)).toEqual({
      line: `claude --resume ${id} --verbose`,
      left: ["--add-dir ok 100%"],
    });
  });
});

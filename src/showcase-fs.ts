// An in-memory stand-in for the fs_* and transfer_* commands, used by the
// showcase page (README screenshots and scripted UI tests). The real file
// manager runs against it unchanged: listings, renames, deletes, the viewer,
// and transfers with progress, conflict questions and cancellation.

type Answer = { resolution: { action: string; name?: string }; applyToAll: boolean };
type Emit = (event: Record<string, unknown>) => void;

interface Node {
  dir: boolean;
  size: number;
  modified: number;
  content: string;
}

interface Job {
  id: string;
  cancelled: boolean;
  pending: { id: number; resolve: (answer: Answer | null) => void } | null;
  sticky: Answer["resolution"] | null;
  conflicts: number;
}

const HOME = "C:\\Users\\dev";
/** A remote host the showcase pretends to have an SFTP session on. */
export const REMOTE_FS = "sftp:showcase-edge";
export const REMOTE_CONNECTION = "showcase-edge";
export const REMOTE_LABEL = "deploy@edge-01.example.net";
const REMOTE_HOME = "/home/deploy";
const T0 = 1785516000000; // 31 Jul 2026, the demo's "today"

export class ShowcaseFs {
  private readonly nodes = new Map<string, Node>();
  private readonly jobs = new Map<string, Job>();
  private nextJob = 1;
  private readonly stepMs: number;
  /** Present every clash as a partial copy (the Resume dialog). */
  private readonly partialConflicts: boolean;

  constructor(options: { bigFolder?: number; stepMs?: number; partialConflicts?: boolean } = {}) {
    this.stepMs = options.stepMs ?? 40;
    this.partialConflicts = options.partialConflicts ?? false;
    this.dir("C:\\");
    this.dir("C:\\Users");
    this.dir(HOME);
    this.dir("C:\\Projects");
    const project = "C:\\Projects\\CorePTY";
    this.dir(project, 1260);
    for (const [name, at] of [[".github", 1260], ["docs", 840], ["src", 1320], ["src-tauri", 1140]] as const) {
      this.dir(`${project}\\${name}`, at);
    }
    this.file(`${project}\\package.json`, 1426, 1200, '{\n  "name": "corepty",\n  "version": "0.3.8"\n}\n');
    this.file(`${project}\\README.md`, 12184, 1100, "# CorePTY\n\nA modern terminal workspace with SSH, Telnet, and dual-pane file management.\n");
    this.file(`${project}\\LICENSE`, 35150, -1425600, "GNU GENERAL PUBLIC LICENSE\nVersion 3, 29 June 2007\n");
    this.file(`${project}\\vite.config.ts`, 1173, 1020, 'import { defineConfig } from "vite";\n');
    this.file(`${project}\\src\\main.ts`, 412, 1300, 'import { App } from "./app";\n');
    this.file(`${project}\\src\\app.ts`, 48210, 1310, "export class App {}\n");
    const downloads = `${HOME}\\Downloads`;
    this.dir(downloads, 600);
    this.dir(`${downloads}\\Screenshots`, 500);
    this.dir(`${downloads}\\Releases`, 200);
    this.file(`${downloads}\\corepty-0.3.8-x64-setup.exe`, 8734912, 1300);
    this.file(`${downloads}\\corepty-portable.zip`, 7218432, 1240);
    this.file(`${downloads}\\terminal-themes.png`, 1842205, 800);
    this.file(`${downloads}\\release-notes.txt`, 4821, 600, "CorePTY 0.3.8\n\n- Stabilized terminal UI\n- Installer upgrades\n");
    // The remote host: POSIX paths.
    for (const dir of ["/", "/home", REMOTE_HOME, `${REMOTE_HOME}/app`, `${REMOTE_HOME}/app/releases`, "/var", "/var/www", "/var/log"]) {
      this.dir(dir, 900);
    }
    this.file(`${REMOTE_HOME}/.bashrc`, 3771, -2000, "# ~/.bashrc\n");
    this.file(`${REMOTE_HOME}/app/config.yml`, 1820, 1200, "port: 8080\nworkers: 4\n");
    this.file(`${REMOTE_HOME}/app/server.log`, 2_480_220, 1310, "started\n");
    this.file(`${REMOTE_HOME}/deploy.sh`, 940, 1100, "#!/bin/sh\nset -e\n");
    this.file("/var/www/index.html", 5120, 900, "<!doctype html>\n");
    if (options.bigFolder) {
      const big = "C:\\big";
      this.dir(big);
      for (let index = 0; index < options.bigFolder; index += 1) {
        const name = `log-${String(index).padStart(6, "0")}.txt`;
        this.file(`${big}\\${name}`, 1000 + ((index * 7919) % 90000), (index % 1440) * 60);
      }
    }
  }


  /** Answer a mocked IPC command, or undefined when it isn't a filesystem one. */
  handle(command: string, args: Record<string, unknown> | undefined): unknown {
    const a = (args ?? {}) as Record<string, any>;
    switch (command) {
      case "sftp_connect":
      case "sftp_reconnect":
        return {
          fs: REMOTE_FS,
          connectionId: REMOTE_CONNECTION,
          label: REMOTE_LABEL,
          home: this.view(REMOTE_HOME),
          notice: null,
        };
      case "fs_chmod":
        return 1;
      case "sftp_close":
      case "fs_edit_resolve":
      case "fs_close_copies":
        return null;
      case "fs_edit":
      case "fs_open_copy":
        return { id: "copy-1", path: `C:\Users\dev\AppData\Local\Temp\CorePTY\open\copy-1\${nameOf(untoken(a.location.path))}` };
      case "sftp_sessions":
        return [{ fs: REMOTE_FS, connectionId: REMOTE_CONNECTION, label: REMOTE_LABEL, users: 1, connected: true }];
      case "ssh_connections":
        return [
          { id: REMOTE_CONNECTION, label: REMOTE_LABEL, host: "edge-01.example.net", port: 22, username: "deploy", users: 1 },
        ];
      case "fs_capabilities":
        if (a.fs === REMOTE_FS) {
          return { trash: false, openNative: false, posixPermissions: true, symlinks: true };
        }
        return { trash: true, openNative: true, posixPermissions: false, symlinks: true };
      case "fs_home":
        if (a.fs === REMOTE_FS) return this.view(REMOTE_HOME);
        return this.view(HOME);
      case "fs_roots":
        if (a.fs === REMOTE_FS) {
          return [
            { label: "Home", location: this.view(REMOTE_HOME) },
            { label: "Filesystem", location: this.view("/") },
          ];
        }
        return [
          { label: "Home", location: this.view(HOME) },
          { label: "C:", location: this.view("C:\\") },
        ];
      case "fs_resolve":
        return this.view(this.resolve(a.input, a.base ? untoken(a.base.path) : null));
      case "fs_list": {
        const path = this.existingDir(untoken(a.location.path));
        const parent = parentOf(path);
        return {
          location: this.view(path),
          parent: parent ? this.view(parent) : null,
          entries: this.children(path).map((child) => this.entry(child)),
        };
      }
      case "fs_stat":
        return this.entry(this.existing(untoken(a.location.path)));
      case "fs_mkdir": {
        const path = join(this.existingDir(untoken(a.parent.path)), validName(a.name));
        if (this.nodes.has(path)) fail(`${a.name} already exists`);
        this.dir(path, null);
        return this.view(path);
      }
      case "fs_rename": {
        const from = this.existing(untoken(a.location.path));
        const to = join(parentOf(from) ?? fail("the root can't be renamed"), validName(a.name));
        if (this.nodes.has(to)) fail(`${a.name} already exists`);
        this.move(from, to);
        return this.view(to);
      }
      case "fs_delete":
      case "fs_trash":
        for (const location of a.locations) this.removeTree(this.existing(untoken(location.path)));
        return null;
      case "fs_dir_size": {
        const path = this.existingDir(untoken(a.location.path));
        let items = 0;
        let bytes = 0;
        for (const [key, node] of this.nodes) {
          if (key !== path && isInside(key, path)) {
            items += 1;
            bytes += node.dir ? 0 : node.size;
          }
        }
        return { items, bytes };
      }
      case "fs_read": {
        const node = this.nodes.get(this.existing(untoken(a.location.path)))!;
        const bytes = new TextEncoder().encode(node.content);
        return bytes.slice(a.offset, a.offset + a.length).buffer;
      }
      case "fs_open_native":
        return null;
      case "transfer_start":
        return this.startJob(a.request, (event) => a.onEvent.onmessage(event));
      case "transfer_cancel": {
        const job = this.jobs.get(a.id);
        if (!job) return false;
        job.cancelled = true;
        job.pending?.resolve(null);
        return true;
      }
      case "transfer_resolve": {
        const job = this.jobs.get(a.id);
        if (!job?.pending || job.pending.id !== a.conflictId) fail("that question was already answered");
        job!.pending!.resolve(a.answer as Answer);
        return null;
      }
      case "transfer_list":
        return [];
      default:
        return undefined;
    }
  }

  private dir(path: string, minutesAfterT0: number | null = 0): void {
    this.nodes.set(path, {
      dir: true,
      size: 0,
      modified: minutesAfterT0 === null ? Date.now() : T0 + minutesAfterT0 * 60000,
      content: "",
    });
  }

  private file(path: string, size: number, minutesAfterT0: number, content = ""): void {
    this.nodes.set(path, { dir: false, size, modified: T0 + minutesAfterT0 * 60000, content });
  }

  private view(path: string) {
    return { fs: isPosix(path) ? REMOTE_FS : "local", path: token(path), display: path };
  }

  private entry(path: string) {
    const node = this.nodes.get(path)!;
    const name = nameOf(path);
    return {
      name,
      path: token(path),
      display: path,
      kind: node.dir ? "directory" : "file",
      isDirectory: node.dir,
      size: node.dir ? null : node.size,
      modified: node.modified,
      mode: isPosix(path) ? (node.dir ? 0o755 : name.endsWith(".sh") ? 0o750 : 0o644) : null,
      readonly: false,
      hidden: name.startsWith("."),
      owner: isPosix(path) ? (path.startsWith("/var") ? "www-data" : "deploy") : null,
      group: isPosix(path) ? "deploy" : null,
      error: null,
    };
  }

  private children(dir: string): string[] {
    return [...this.nodes.keys()].filter((key) => parentOf(key) === dir);
  }

  private existing(path: string): string {
    if (!this.nodes.has(path)) fail(`${path} not found`);
    return path;
  }

  private existingDir(path: string): string {
    if (!this.nodes.get(path)?.dir) fail(`${path} is not a folder`);
    return path;
  }

  private resolve(input: string, base: string | null): string {
    if (input.startsWith("/") || (base && isPosix(base))) {
      const start = input.startsWith("/") ? input : `${base}/${input}`;
      const parts: string[] = [];
      for (const part of start.split("/")) {
        if (part === "" || part === ".") continue;
        if (part === "..") parts.pop();
        else parts.push(part);
      }
      return this.existingDir(`/${parts.join("/")}`);
    }
    let path = /^[A-Za-z]:/.test(input) ? input : join(base ?? HOME, input);
    path = path.replace(/\//g, "\\");
    const parts: string[] = [];
    for (const part of path.split("\\")) {
      if (part === "" || part === ".") continue;
      if (part === "..") parts.length > 1 && parts.pop();
      else parts.push(part);
    }
    const resolved = parts.length === 1 ? `${parts[0]}\\` : parts.join("\\");
    return this.existingDir(resolved);
  }

  private move(from: string, to: string): void {
    for (const key of [...this.nodes.keys()]) {
      if (key === from || isInside(key, from)) {
        const node = this.nodes.get(key)!;
        this.nodes.delete(key);
        this.nodes.set(to + key.slice(from.length), node);
      }
    }
  }

  private removeTree(path: string): void {
    for (const key of [...this.nodes.keys()]) {
      if (key === path || isInside(key, path)) this.nodes.delete(key);
    }
  }

  private startJob(request: any, emit: Emit) {
    const job: Job = { id: `job-${this.nextJob++}`, cancelled: false, pending: null, sticky: null, conflicts: 0 };
    this.jobs.set(job.id, job);
    const sources: string[] = request.sources.map((source: any) => untoken(source.path));
    const destination = untoken(request.destination.path);
    void this.runJob(job, request.kind, sources, destination, emit);
    return {
      id: job.id,
      kind: request.kind,
      state: "queued",
      message: null,
      source: this.view(sources[0]),
      sourceCount: sources.length,
      destination: this.view(destination),
      totalItems: 0,
      totalBytes: 0,
      completedItems: 0,
      completedBytes: 0,
      skippedItems: 0,
    };
  }

  private async runJob(job: Job, kind: "copy" | "move", sources: string[], destination: string, emit: Emit) {
    const id = job.id;
    const totals = { items: 0, bytes: 0 };
    const done = { items: 0, bytes: 0, skipped: 0 };
    const pause = () => new Promise((resolve) => setTimeout(resolve, this.stepMs));
    const progress = (current: string) =>
      emit({
        event: "progress",
        id,
        current,
        completedItems: done.items,
        completedBytes: done.bytes,
        totalItems: totals.items,
        totalBytes: totals.bytes,
        skippedItems: done.skipped,
        bytesPerSecond: 4_200_000,
        etaSeconds: Math.max(0, Math.ceil((totals.bytes - done.bytes) / 4_200_000)),
      });
    try {
      emit({ event: "queued", id });
      await pause();
      emit({ event: "planning", id });
      for (const source of sources) {
        for (const [key, node] of this.nodes) {
          if (key === source || isInside(key, source)) {
            totals.items += 1;
            totals.bytes += node.dir ? 0 : node.size;
          }
        }
      }
      emit({ event: "planned", id, totalItems: totals.items, totalBytes: totals.bytes });

      const transfer = async (source: string, folder: string): Promise<boolean> => {
        if (job.cancelled) throw "cancelled";
        const node = this.nodes.get(source)!;
        let target = join(folder, nameOf(source));
        const existing = this.nodes.get(target);
        let merge = false;
        if (existing && target === source) {
          if (kind === "move") return countSkip(source);
          target = this.unique(folder, nameOf(source));
        } else if (existing && existing.dir && node.dir) {
          merge = true;
        } else if (existing) {
          const answer = await this.ask(job, emit, source, target);
          const action = answer.action;
          if (action === "cancel") throw "cancelled";
          const newer = node.modified > existing.modified;
          if (action === "skip" || (action === "overwrite_if_newer" && !newer) ||
              (action === "overwrite_if_different_size" && node.size === existing.size)) {
            return countSkip(source);
          }
          if (action === "rename") target = answer.name ? join(folder, answer.name) : this.unique(folder, nameOf(source));
          else this.removeTree(target);
        }
        await pause();
        if (node.dir) {
          if (!merge) this.nodes.set(target, { ...node });
          done.items += 1;
          progress(nameOf(source));
          let complete = true;
          for (const child of this.children(source)) {
            if (!(await transfer(child, target))) complete = false;
          }
          if (kind === "move" && complete) this.nodes.delete(source);
          return complete;
        }
        this.nodes.set(target, { ...node });
        if (kind === "move") this.nodes.delete(source);
        done.items += 1;
        done.bytes += node.size;
        progress(nameOf(source));
        return true;
      };
      const countSkip = (source: string) => {
        done.skipped += 1;
        for (const [key, node] of this.nodes) {
          if (key === source || isInside(key, source)) {
            done.items += 1;
            done.bytes += node.dir ? 0 : node.size;
          }
        }
        return false;
      };

      for (const source of sources) await transfer(source, destination);
      emit({ event: "finished", id, completedItems: done.items, completedBytes: done.bytes, skippedItems: done.skipped });
    } catch (error) {
      if (error === "cancelled") emit({ event: "cancelled", id });
      else emit({ event: "failed", id, message: String(error) });
    } finally {
      this.jobs.delete(id);
    }
  }

  private async ask(job: Job, emit: Emit, source: string, target: string): Promise<Answer["resolution"]> {
    if (job.sticky) return job.sticky;
    const conflictId = ++job.conflicts;
    const side = (path: string) => {
      const node = this.nodes.get(path)!;
      return { path, isDirectory: node.dir, size: node.dir ? null : node.size, modified: node.modified };
    };
    const answer = await new Promise<Answer | null>((resolve) => {
      job.pending = { id: conflictId, resolve };
      emit({
        event: "conflict",
        id: job.id,
        conflictId,
        conflict: { name: nameOf(source), source: side(source), target: side(target), partial: this.partialConflicts },
      });
    });
    job.pending = null;
    if (!answer) return { action: "cancel" };
    if (answer.applyToAll) {
      job.sticky = answer.resolution.action === "rename" ? { action: "rename" } : answer.resolution;
    }
    return answer.resolution;
  }

  private unique(folder: string, name: string): string {
    const dot = name.lastIndexOf(".");
    for (let number = 1; ; number += 1) {
      const suffix = number === 1 ? " (copy)" : ` (copy ${number})`;
      const candidate = dot > 0 ? `${name.slice(0, dot)}${suffix}${name.slice(dot)}` : `${name}${suffix}`;
      const path = join(folder, candidate);
      if (!this.nodes.has(path)) return path;
    }
  }
}

function token(path: string): string {
  return `m:${path}`;
}

function untoken(value: string): string {
  if (!value.startsWith("m:")) fail("invalid path token");
  return value.slice(2);
}

/** Remote (POSIX) paths start with "/"; the rest are Windows paths. */
function isPosix(path: string): boolean {
  return path.startsWith("/");
}

function parentOf(path: string): string | null {
  if (isPosix(path)) {
    if (path === "/") return null;
    const index = path.lastIndexOf("/");
    return index === 0 ? "/" : path.slice(0, index);
  }
  if (/^[A-Za-z]:\\$/.test(path)) return null;
  const index = path.lastIndexOf("\\");
  const parent = path.slice(0, index);
  return /^[A-Za-z]:$/.test(parent) ? `${parent}\\` : parent;
}

function nameOf(path: string): string {
  return path.slice(path.lastIndexOf(isPosix(path) ? "/" : "\\") + 1);
}

function join(folder: string, name: string): string {
  const separator = isPosix(folder) ? "/" : "\\";
  return folder.endsWith(separator) ? `${folder}${name}` : `${folder}${separator}${name}`;
}

function isInside(path: string, folder: string): boolean {
  const separator = isPosix(folder) ? "/" : "\\";
  return path.startsWith(folder.endsWith(separator) ? folder : `${folder}${separator}`);
}

function validName(name: string): string {
  if (!name || name === "." || name === ".." || /[\\/]/.test(name)) fail(`"${name}" is not a valid name`);
  return name;
}

function fail(message: string): never {
  // Tauri rejects commands with the error string itself.
  throw message;
}

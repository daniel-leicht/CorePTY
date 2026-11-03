// Thin, typed wrappers over the Tauri IPC bridge.
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export type SessionKind = "local" | "ssh" | "telnet";

export interface SessionInfo {
  id: string;
  kind: SessionKind;
  title: string;
  /** SSH: the shared connection the shell runs on (valid once connected). */
  connectionId?: string;
}

/** A local shell id — dynamic per OS (see `api.listShells`); "custom" runs an arbitrary command. */
export type LocalShell = string;

/** A selectable local shell offered on this OS. */
export interface ShellInfo {
  id: string;
  label: string;
  hint: string;
  icon: string;
}

export interface LocalOptions {
  id?: string;
  shell: LocalShell;
  command?: string;
  args?: string[];
  cwd?: string;
  cols: number;
  rows: number;
  title?: string;
}

export type SshAuth =
  | { type: "password"; password: string }
  | { type: "key"; keyPath: string; passphrase?: string };

export interface SshConnectOptions {
  id?: string;
  host: string;
  port: number;
  username: string;
  auth: SshAuth;
  cols: number;
  rows: number;
  title?: string;
  /** Open the shell on this connection (no new login) while it is still up. */
  connectionId?: string;
}

/** A live SSH connection, shared by terminal tabs and file panels. */
export interface SshConnectionView {
  id: string;
  /** `user@host` (with `:port` when not 22). */
  label: string;
  host: string;
  port: number;
  username: string;
  /** Shells and file panels currently using it. */
  users: number;
}

export interface TelnetConnectOptions {
  id?: string;
  host: string;
  port: number;
  cols: number;
  rows: number;
  title?: string;
}

export interface SavedSession {
  id: string;
  name: string;
  kind: "ssh" | "telnet";
  host: string;
  port?: number | null;
  username?: string | null;
  authType?: "password" | "key" | null;
  keyPath?: string | null;
  saveSecret: boolean;
  folderId?: string | null;
  color?: string | null;
  /** Sort index within its folder (drag-to-reorder); absent = sort by name. */
  order?: number | null;
}

export interface Folder {
  id: string;
  name: string;
  parentId?: string | null;
}

// ---- filesystems (src-tauri/src/vfs) + transfers (src-tauri/src/transfer) ----

/** A path on a filesystem. `path` is an opaque token: never build or edit it. */
export interface FsLocation {
  /** "local", or a remote connection id. */
  fs: string;
  path: string;
}

/** A location plus its human-readable path. Can be passed wherever an FsLocation goes. */
export interface FsLocationView extends FsLocation {
  display: string;
}

export type FsEntryKind = "directory" | "file" | "symlink" | "other";

export interface FsEntry {
  name: string;
  /** Opaque token of this entry's path (pair with the listing's `fs`). */
  path: string;
  display: string;
  kind: FsEntryKind;
  /** True for folders and for links that resolve to a folder. */
  isDirectory: boolean;
  size: number | null;
  /** Milliseconds since the Unix epoch. */
  modified: number | null;
  /** POSIX permission bits, where the filesystem has them. */
  mode: number | null;
  readonly: boolean;
  hidden: boolean;
  owner: string | null;
  group: string | null;
  /** Set when the entry was listed but could not be inspected. */
  error: string | null;
}

export interface FsListing {
  location: FsLocationView;
  parent: FsLocationView | null;
  entries: FsEntry[];
}

export interface FsRoot {
  label: string;
  location: FsLocationView;
}

export interface FsCapabilities {
  trash: boolean;
  openNative: boolean;
  posixPermissions: boolean;
  symlinks: boolean;
}

export interface FsFolderSize {
  items: number;
  bytes: number;
}

export type TransferKind = "copy" | "move";

export interface TransferRequest {
  kind: TransferKind;
  sources: FsLocation[];
  /** The folder the sources go into. */
  destination: FsLocation;
  /** Give copies the source's permissions (default: yes). */
  preserveMode?: boolean;
}

export type TransferState =
  | "queued"
  | "planning"
  | "running"
  | "waiting_for_user"
  /** A filesystem it uses is disconnected; it carries on after a reconnect. */
  | "paused"
  | "done"
  | "failed"
  | "cancelled";

export interface TransferJob {
  id: string;
  kind: TransferKind;
  state: TransferState;
  message: string | null;
  source: FsLocationView;
  sourceCount: number;
  destination: FsLocationView;
  totalItems: number;
  totalBytes: number;
  completedItems: number;
  completedBytes: number;
  skippedItems: number;
}

export interface ConflictSide {
  path: string;
  isDirectory: boolean;
  size: number | null;
  modified: number | null;
}

export interface TransferConflict {
  name: string;
  source: ConflictSide;
  target: ConflictSide;
  /** The target is a partial copy (`<name>.filepart`) from an interrupted transfer. */
  partial: boolean;
}

export type ConflictResolution =
  | { action: "overwrite" }
  | { action: "skip" }
  | { action: "rename"; name?: string }
  | { action: "overwrite_if_newer" }
  | { action: "overwrite_if_different_size" }
  | { action: "resume" }
  | { action: "cancel" };

export interface ConflictAnswer {
  resolution: ConflictResolution;
  applyToAll: boolean;
}

export type TransferEvent =
  | { event: "queued"; id: string }
  | { event: "planning"; id: string }
  | { event: "planned"; id: string; totalItems: number; totalBytes: number }
  | {
      event: "progress";
      id: string;
      current: string;
      completedItems: number;
      completedBytes: number;
      totalItems: number;
      totalBytes: number;
      skippedItems: number;
      bytesPerSecond: number;
      etaSeconds: number | null;
    }
  | { event: "conflict"; id: string; conflictId: number; conflict: TransferConflict }
  | {
      event: "finished";
      id: string;
      completedItems: number;
      completedBytes: number;
      skippedItems: number;
    }
  | { event: "failed"; id: string; message: string }
  | { event: "cancelled"; id: string }
  | { event: "paused"; id: string; fs: string; message: string };

export const fsApi = {
  capabilities: (fs: string) => invoke<FsCapabilities>("fs_capabilities", { fs }),
  home: (fs: string) => invoke<FsLocationView>("fs_home", { fs }),
  roots: (fs: string) => invoke<FsRoot[]>("fs_roots", { fs }),
  /** Typed input (absolute, `~`, or relative to `base`) → an existing folder. */
  resolve: (fs: string, input: string, base?: FsLocation) =>
    invoke<FsLocationView>("fs_resolve", { fs, input, base: base ?? null }),
  list: (location: FsLocation) => invoke<FsListing>("fs_list", { location: plain(location) }),
  stat: (location: FsLocation) => invoke<FsEntry>("fs_stat", { location: plain(location) }),
  mkdir: (parent: FsLocation, name: string) =>
    invoke<FsLocationView>("fs_mkdir", { parent: plain(parent), name }),
  /** Rename in place; never replaces an existing entry. */
  rename: (location: FsLocation, name: string) =>
    invoke<FsLocationView>("fs_rename", { location: plain(location), name }),
  /** Permanently delete, folders included. */
  remove: (locations: FsLocation[]) =>
    invoke<void>("fs_delete", { locations: locations.map(plain) }),
  trash: (locations: FsLocation[]) => invoke<void>("fs_trash", { locations: locations.map(plain) }),
  openNative: (location: FsLocation) => invoke<void>("fs_open_native", { location: plain(location) }),
  /** Raw bytes `[offset, offset + length)` (at most 4 MiB per call). */
  read: (location: FsLocation, offset: number, length: number) =>
    invoke<ArrayBuffer>("fs_read", { location: plain(location), offset, length }),
  folderSize: (location: FsLocation) =>
    invoke<FsFolderSize>("fs_dir_size", { location: plain(location) }),
  /** Set POSIX permissions; `recursive` also inside a folder. Returns how many changed. */
  chmod: (location: FsLocation, mode: number, recursive = false) =>
    invoke<number>("fs_chmod", { location: plain(location), mode, recursive }),
};

export const transferApi = {
  /** Queue a copy/move; returns at once. Progress and questions arrive on `onEvent`. */
  start: (request: TransferRequest, onEvent: (event: TransferEvent) => void) => {
    const channel = new Channel<TransferEvent>();
    channel.onmessage = onEvent;
    return invoke<TransferJob>("transfer_start", {
      request: {
        kind: request.kind,
        sources: request.sources.map(plain),
        destination: plain(request.destination),
        preserveMode: request.preserveMode ?? true,
      },
      onEvent: channel,
    });
  },
  cancel: (id: string) => invoke<boolean>("transfer_cancel", { id }),
  list: () => invoke<TransferJob[]>("transfer_list"),
  resolve: (id: string, conflictId: number, answer: ConflictAnswer) =>
    invoke<void>("transfer_resolve", { id, conflictId, answer }),
};

/** Open SFTP on a live SSH connection (`connectionId`), or log in first. */
export type SftpConnectOptions =
  | { connectionId: string }
  | { connectionId?: string; host: string; port: number; username: string; auth: SshAuth };

export interface SftpView {
  /** The filesystem id to browse (`sftp:<connectionId>`). */
  fs: string;
  connectionId: string;
  label: string;
  home: FsLocationView;
  /** e.g. a newly trusted host key. */
  notice: string | null;
}

export interface SftpSessionView {
  fs: string;
  connectionId: string;
  label: string;
  users: number;
  connected: boolean;
}

/** Sent when an SFTP filesystem drops or comes back. */
export interface SftpStatus {
  fs: string;
  connected: boolean;
  message: string | null;
}

export const sftpApi = {
  /** Call `close` once for every successful `connect`. */
  connect: (options: SftpConnectOptions) => invoke<SftpView>("sftp_connect", { options }),
  /** Carry on with `fs` over a new session: panels and paused jobs continue. */
  reconnect: (fs: string, options: SftpConnectOptions) =>
    invoke<SftpView>("sftp_reconnect", { fs, options }),
  onStatus: (cb: (status: SftpStatus) => void): Promise<UnlistenFn> =>
    listen<SftpStatus>("sftp://status", (e) => cb(e.payload)),
  close: (fs: string) => invoke<void>("sftp_close", { fs }),
  sessions: () => invoke<SftpSessionView[]>("sftp_sessions"),
};

/** Just the addressing fields — views and entries carry extra display data. */
function plain(location: FsLocation): FsLocation {
  return { fs: location.fs, path: location.path };
}

export const api = {
  ping: () => invoke<string>("ping"),

  createLocal: (options: LocalOptions) =>
    invoke<SessionInfo>("session_create_local", { options }),
  createLocalElevated: (options: LocalOptions) =>
    invoke<SessionInfo>("session_create_local_elevated", { options }),
  createSsh: (options: SshConnectOptions) =>
    invoke<SessionInfo>("session_create_ssh", { options }),
  createTelnet: (options: TelnetConnectOptions) =>
    invoke<SessionInfo>("session_create_telnet", { options }),

  write: (id: string, data: string) => invoke<void>("session_write", { id, data }),
  resize: (id: string, cols: number, rows: number) =>
    invoke<void>("session_resize", { id, cols, rows }),
  close: (id: string) => invoke<void>("session_close", { id }),
  list: () => invoke<SessionInfo[]>("session_list"),

  /** Local shells available on this OS. */
  listShells: () => invoke<ShellInfo[]>("list_local_shells"),
  /** Live SSH connections (shared by terminal tabs and file panels). */
  sshConnections: () => invoke<SshConnectionView[]>("ssh_connections"),
  /** Host OS: "windows" | "macos" | "linux" | … */
  hostOs: () => invoke<string>("host_os"),

  secretSet: (id: string, secret: string) => invoke<void>("secret_set", { id, secret }),
  secretGet: (id: string) => invoke<string | null>("secret_get", { id }),
  secretDelete: (id: string) => invoke<void>("secret_delete", { id }),

  sessionsLoad: () => invoke<SavedSession[]>("sessions_load"),
  sessionsUpsert: (session: SavedSession) => invoke<void>("sessions_upsert", { session }),
  /** Set the folder + order of `ids` in one write (drag-to-reorder). */
  sessionsReorder: (folder: string | null, ids: string[]) =>
    invoke<void>("sessions_reorder", { folder, ids }),
  sessionsDelete: (id: string) => invoke<void>("sessions_delete", { id }),

  foldersLoad: () => invoke<Folder[]>("folders_load"),
  folderUpsert: (folder: Folder) => invoke<void>("folder_upsert", { folder }),
  folderDelete: (id: string) => invoke<void>("folder_delete", { id }),

  settingsLoad: () => invoke<Record<string, unknown>>("settings_load"),
  settingsSave: (settings: Record<string, unknown>) => invoke<void>("settings_save", { settings }),
};

/** Native file picker for an SSH private key. Returns an absolute path or null. */
export async function pickKeyFile(): Promise<string | null> {
  const selected = await open({
    multiple: false,
    directory: false,
    title: "Select SSH private key",
  });
  return typeof selected === "string" ? selected : null;
}

/** Native cross-platform folder picker used by either file-manager panel. */
export async function pickDirectory(defaultPath?: string): Promise<string | null> {
  const selected = await open({
    multiple: false,
    directory: true,
    title: "Choose folder",
    defaultPath,
  });
  return typeof selected === "string" ? selected : null;
}

// ---- streamed events ----

export interface DataPayload {
  id: string;
  data: string; // base64
}
export interface ExitPayload {
  id: string;
  code: number | null;
  message: string | null;
}
export interface StatusPayload {
  id: string;
  status: string;
  detail: string | null;
}

export const onData = (cb: (p: DataPayload) => void): Promise<UnlistenFn> =>
  listen<DataPayload>("pty://data", (e) => cb(e.payload));
export const onExit = (cb: (p: ExitPayload) => void): Promise<UnlistenFn> =>
  listen<ExitPayload>("pty://exit", (e) => cb(e.payload));
export const onStatus = (cb: (p: StatusPayload) => void): Promise<UnlistenFn> =>
  listen<StatusPayload>("pty://status", (e) => cb(e.payload));

/** Decode a base64 payload into raw bytes for xterm. */
export function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

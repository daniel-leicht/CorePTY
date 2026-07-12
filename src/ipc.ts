// Thin, typed wrappers over the Tauri IPC bridge.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export type SessionKind = "local" | "ssh" | "telnet";

export interface SessionInfo {
  id: string;
  kind: SessionKind;
  title: string;
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
}

export interface Folder {
  id: string;
  name: string;
  parentId?: string | null;
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
  /** Host OS: "windows" | "macos" | "linux" | … */
  hostOs: () => invoke<string>("host_os"),

  secretSet: (id: string, secret: string) => invoke<void>("secret_set", { id, secret }),
  secretGet: (id: string) => invoke<string | null>("secret_get", { id }),
  secretDelete: (id: string) => invoke<void>("secret_delete", { id }),

  sessionsLoad: () => invoke<SavedSession[]>("sessions_load"),
  sessionsUpsert: (session: SavedSession) => invoke<void>("sessions_upsert", { session }),
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

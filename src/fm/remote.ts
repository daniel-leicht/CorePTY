// Remote filesystems in the file manager: what the app can open (saved SSH
// connections, the live connections of SSH tabs), how to log in to them, and
// what the file manager learns about each one it opened (its label for the
// panel badge, and how to log in again after the connection drops).

import type { SftpConnectOptions, SftpView } from "../ipc";

/** A saved SSH connection. */
export interface SavedRemote {
  id: string;
  name: string;
  /** Its folder in the connections tree (`Homelab / Lab`), if any. */
  folder: string | null;
  /** `user@host`. */
  label: string;
}

/** The live connection of an open SSH tab (SFTP on it needs no login). */
export interface LiveRemote {
  connectionId: string;
  /** `user@host`. */
  label: string;
  /** The tab(s) using it. */
  title: string;
}

/** What the app offers the file manager. */
export interface RemoteSource {
  saved(): Promise<SavedRemote[]>;
  live(): Promise<LiveRemote[]>;
  /** Log in to a saved connection (keychain, else ask): null if cancelled. */
  login(savedId: string): Promise<SftpConnectOptions | null>;
  /** How to log in again to the host of a live connection (from its tab), if known. */
  relogin(connectionId: string): Promise<SftpConnectOptions | null>;
}

/** Where to open a remote panel. */
export type RemoteTarget =
  | { kind: "live"; connectionId: string }
  | { kind: "saved"; id: string };

interface Known {
  label: string;
  relogin: () => Promise<SftpConnectOptions | null>;
}

/** Every remote filesystem opened in this window, by fs id. */
const known = new Map<string, Known>();

export function isRemote(fs: string): boolean {
  return fs !== "local";
}

/** Remember an opened filesystem's label and how to log in to it again. */
export function learn(view: SftpView, relogin: Known["relogin"]): void {
  known.set(view.fs, { label: view.label, relogin });
}

/** `user@host` for a remote filesystem, or null when unknown. */
export function remoteLabel(fs: string): string | null {
  return known.get(fs)?.label ?? null;
}

/** How to log in again to `fs`, if it was opened here. */
export function reloginFor(fs: string): Known["relogin"] | null {
  return known.get(fs)?.relogin ?? null;
}

/** `LOCAL`, or `SFTP user@host` (the status bar, the badge's tooltip). */
export function providerLabel(fs: string): string {
  if (!isRemote(fs)) return "LOCAL";
  const label = remoteLabel(fs);
  return label ? `SFTP ${label}` : "SFTP";
}

/** The badge itself, kept short: `SFTP deploy@edge-01` for `deploy@edge-01.example.net`. */
export function providerBadge(fs: string): string {
  if (!isRemote(fs)) return "LOCAL";
  const label = remoteLabel(fs);
  if (!label) return "SFTP";
  // `user@host`, `user@host:2222`, `user@[::1]:2222`
  const match = /^(.*@)?(\[[^\]]+\]|[^:]+)(:\d+)?$/.exec(label);
  const host = match?.[2] ?? label;
  // Names shorten to their first part; addresses stay whole.
  const short = /^[\d.]+$/.test(host) || host.startsWith("[") ? host : host.split(".")[0];
  return `SFTP ${match?.[1] ?? ""}${short}`;
}

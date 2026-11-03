// The connections the app knows, offered to the file manager: saved SSH
// sessions (logged in with the keychain secret, or a password prompt) and the
// live connections of open SSH tabs (SFTP on them needs no login).

import type { ConnForm } from "./dialog";
import { promptText } from "./fm/dialogs";
import type { LiveRemote, RemoteSource, SavedRemote } from "./fm/remote";
import { api, type Folder, type SavedSession, type SftpConnectOptions, type SshAuth } from "./ipc";

/** An SSH tab with a live connection. */
export interface SshTab {
  connectionId: string;
  title: string;
  form: ConnForm;
  /** Its shell's working folder (OSC 7), if reported. */
  cwd: string | null;
  watchCwd: (listener: (cwd: string) => void) => () => void;
}

function hostLabel(username: string, host: string, port: number): string {
  const at = host.includes(":") ? `[${host}]` : host;
  return port === 22 ? `${username}@${at}` : `${username}@${at}:${port}`;
}

function folderPath(id: string | null | undefined, folders: Folder[]): string | null {
  const names: string[] = [];
  const seen = new Set<string>();
  let current = folders.find((folder) => folder.id === id);
  while (current && !seen.has(current.id)) {
    seen.add(current.id);
    names.unshift(current.name);
    const parent = current.parentId;
    current = folders.find((folder) => folder.id === parent);
  }
  return names.length ? names.join(" / ") : null;
}

/** SFTP login options from a tab's (or a saved session's) form. */
export function loginFromForm(form: ConnForm): SftpConnectOptions {
  const auth: SshAuth =
    form.authType === "key"
      ? { type: "key", keyPath: form.keyPath, passphrase: form.passphrase || undefined }
      : { type: "password", password: form.password };
  return { host: form.host, port: form.port, username: form.username, auth };
}

async function savedSsh(): Promise<SavedSession[]> {
  return (await api.sessionsLoad()).filter((session) => session.kind === "ssh");
}

/** Log in to a saved connection: its keychain secret, else ask for the password. */
export async function loginToSaved(session: SavedSession): Promise<SftpConnectOptions | null> {
  let secret = "";
  if (session.saveSecret) {
    secret = (await api.secretGet(session.id).catch(() => null)) ?? "";
  }
  const port = session.port ?? 22;
  const username = session.username ?? "";
  if (session.authType !== "key" && !secret) {
    const typed = await promptText({
      title: `Log in to ${session.name}`,
      label: `Password for ${hostLabel(username, session.host, port)}`,
      value: "",
      action: "Connect",
      secret: true,
    });
    if (!typed) return null;
    secret = typed;
  }
  const auth: SshAuth =
    session.authType === "key"
      ? { type: "key", keyPath: session.keyPath ?? "", passphrase: secret || undefined }
      : { type: "password", password: secret };
  return { host: session.host, port, username, auth };
}

export function appRemoteSource(sshTabs: () => SshTab[]): RemoteSource {
  return {
    async saved(): Promise<SavedRemote[]> {
      const [sessions, folders] = await Promise.all([savedSsh(), api.foldersLoad()]);
      return sessions
        .map((session) => ({
          id: session.id,
          name: session.name || session.host,
          folder: folderPath(session.folderId, folders),
          label: hostLabel(session.username ?? "", session.host, session.port ?? 22),
          order: session.order ?? 0,
        }))
        .sort(
          (a, b) =>
            (a.folder ?? "").localeCompare(b.folder ?? "") ||
            a.order - b.order ||
            a.name.localeCompare(b.name)
        )
        .map(({ order: _order, ...remote }) => remote);
    },

    async live(): Promise<LiveRemote[]> {
      const connections = (await api.sshConnections()) ?? [];
      const tabs = sshTabs();
      return connections.map((connection) => ({
        connectionId: connection.id,
        label: connection.label,
        title: tabs
          .filter((tab) => tab.connectionId === connection.id)
          .map((tab) => tab.title)
          .join(", "),
        folder: tabs.find((tab) => tab.connectionId === connection.id && tab.cwd)?.cwd ?? null,
      }));
    },

    async login(savedId: string): Promise<SftpConnectOptions | null> {
      const session = (await savedSsh()).find((each) => each.id === savedId);
      return session ? loginToSaved(session) : null;
    },

    async relogin(connectionId: string): Promise<SftpConnectOptions | null> {
      const tab = sshTabs().find((each) => each.connectionId === connectionId);
      return tab ? loginFromForm(tab.form) : null;
    },

    watchFolder(connectionId, onFolder) {
      const tab = sshTabs().find((each) => each.connectionId === connectionId);
      return tab ? tab.watchCwd(onFolder) : null;
    },
  };
}

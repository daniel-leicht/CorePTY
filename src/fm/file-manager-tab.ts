// The dual-pane file manager tab (WinSCP / Norton Commander style): two
// panels, a transfer strip and an F-key bar. Everything goes through the
// provider-neutral fs_* / transfer_* commands, so a panel can show any
// registered filesystem.

import {
  fsApi,
  pickDirectory,
  sftpApi,
  type FsEntry,
  type FsLocation,
  type FsLocationView,
  type SftpConnectOptions,
  type SftpStatus,
  type SftpView,
  type EditEvent,
  type TransferKind,
  type TransferRequest,
} from "../ipc";
import { contextMenu, type MenuItem } from "../menu";
import type { AppTab, TabStatus, TabStatusInfo } from "../tab";
import { uuid } from "../util";
import {
  confirmAction,
  confirmChoice,
  copyMoveDialog,
  isBrowserAccelerator,
  maskDialog,
  modalOpen,
  promptText,
  type DialogContext,
} from "./dialogs";
import { formatFileSize, visibleName } from "./format";
import { current as settings } from "../settings";
import { commandFor, FUNCTION_KEYS, isQuickSearchKey, type FmCommand } from "./keymap";
import { DRAG_TYPE, PanelView, type PanelHost } from "./panel-view";
import {
  isRemote,
  learn,
  providerLabel,
  reloginFor,
  remoteLabel,
  type RemoteSource,
  type RemoteTarget,
} from "./remote";
import { TransferQueue } from "./transfer-queue";
import { openViewer } from "./viewer";
import { propertiesDialog } from "./properties";
import { Claims } from "./claims";

const LOCATION_KEY = "corepty.fm.location.";
/** Remote files bigger than this ask before downloading to open. */
const LARGE_DOWNLOAD = 100 * 1024 * 1024;
const SPLIT_KEY = "corepty.files.split";

export interface FileManagerOptions {
  toast: (message: string, kind?: "info" | "warn" | "error") => void;
  /** Start here (duplicating a tab) instead of the remembered folders. */
  initialLocations?: [FsLocationView | undefined, FsLocationView | undefined];
  /** The connections the app knows (for the location menu and logins). */
  remote?: RemoteSource;
  /** Open this remote filesystem in a panel on start ("Open SFTP"). */
  openRemote?: { side: 0 | 1; target: RemoteTarget };
}

interface Clipboard {
  kind: TransferKind;
  fs: string;
  entries: Array<Pick<FsEntry, "name" | "path">>;
  /** The folder they came from, refreshed after a cut-and-paste. */
  from: FsLocationView | null;
}

interface DragPayload {
  tab: string;
  side: 0 | 1;
  fs: string;
  entries: Array<Pick<FsEntry, "name" | "path">>;
  from: FsLocationView | null;
}

/** Shared by every file manager tab, like the OS clipboard. */
let clipboard: Clipboard | null = null;


export class FileManagerTab implements AppTab, PanelHost {
  readonly tabType = "file-manager" as const;
  readonly uid = uuid();
  readonly element: HTMLDivElement;
  readonly iconName = "files";
  readonly elevated = false;

  onTitleUpdate?: () => void;
  onStatusChange?: () => void;
  onClose?: () => void;

  status: TabStatus = "connecting";
  readonly toast: FileManagerOptions["toast"];
  private customTitle: string | null = null;
  private opened = false;
  private disposed = false;
  /** Filesystems being reconnected. */
  private readonly reconnecting = new Set<string>();
  private activeSide: 0 | 1 = 0;
  private readonly panels: [PanelView, PanelView];
  private readonly queue: TransferQueue;
  private readonly initialLocations?: FileManagerOptions["initialLocations"];
  private readonly remote?: RemoteSource;
  private readonly openOnStart?: FileManagerOptions["openRemote"];
  /** Downloaded copies of remote files this tab opened (deleted when it closes). */
  private readonly copies: string[] = [];
  /** The filesystem each copy came from. */
  private readonly copyFs = new Map<string, string>();
  /** Remote filesystems this tab holds (a panel shows one, or is on its way). */
  private readonly claims = new Claims(
    {
      retain: (fs) => sftpApi.retain(fs).catch(() => false),
      close: (fs) => void sftpApi.close(fs).catch(() => undefined),
    },
    isRemote
  );
  /** A panel following its SSH tab's working folder (OSC 7), on the host first seen. */
  private following: {
    side: 0 | 1;
    fs: string;
    connectionId: string;
    host: string | null;
    stop: () => void;
  } | null = null;
  /** Edit conflicts waiting until this tab is in front to be asked. */
  private readonly editQuestions: Array<Extract<EditEvent, { event: "conflict" }>> = [];
  private askingEdit = false;
  /** A question was put off (dismissed): asked again when the tab is focused. */
  private editQuestionsDeferred = false;
  private editQuestionTimer: number | null = null;
  /** Source folder of each queued transfer, refreshed when it ends. */
  private readonly transferSources = new WeakMap<TransferRequest, FsLocationView | null>();
  /** Aborted on dispose: dismisses this tab's dialogs. */
  private readonly lifetime = new AbortController();
  /** Stops listening for remote filesystems dropping and coming back. */
  private readonly statusListener: Promise<(() => void) | null> = sftpApi
    .onStatus((status) => this.onRemoteStatus(status))
    .catch(() => null);
  /** Stops listening for edited remote files being uploaded. */
  private readonly editListener: Promise<(() => void) | null> = fsApi
    .onEdit((event) => this.onEdit(event))
    .catch(() => null);
  private readonly dialogs: DialogContext = {
    signal: this.lifetime.signal,
    fallbackFocus: () => this.focus(),
  };
  /** Window listeners to remove on dispose (they would keep the tab alive). */
  private readonly windowListeners: Array<[string, () => void]> = [];
  /** Last persisted folder per side, so moving the cursor doesn't rewrite it. */
  private readonly savedLocations: [string | null, string | null] = [null, null];

  constructor(options: FileManagerOptions) {
    this.toast = options.toast;
    this.initialLocations = options.initialLocations;
    this.remote = options.remote;
    this.openOnStart = options.openRemote;
    this.panels = [new PanelView(this, 0, this.uid), new PanelView(this, 1, this.uid)];
    this.queue = new TransferQueue({
      toast: (message, kind) => this.toast(message, kind),
      finished: (request) => this.afterTransfer(request),
      changed: () => this.onStatusChange?.(),
      isActive: () => !this.disposed && this.element.classList.contains("is-active"),
      dialogContext: () => this.dialogs,
      focus: () => this.focus(),
      reconnect: (fs) => this.reconnectFs(fs),
    });
    this.element = document.createElement("div");
    this.element.className = "file-manager";
    this.element.innerHTML = `
      <div class="fm-workspace">
        <div class="fm-panels"><div class="fm-splitter" role="separator" aria-label="Resize file panels" aria-orientation="vertical" tabindex="0"></div></div>
        <div class="fm-keys" role="toolbar" aria-label="File commands"></div>
      </div>`;
    const panelsEl = this.element.querySelector(".fm-panels")!;
    panelsEl.prepend(this.panels[0].element);
    panelsEl.append(this.panels[1].element);
    this.element.querySelector(".fm-workspace")!.insertBefore(
      this.queue.element,
      this.element.querySelector(".fm-keys")
    );
    this.panels[0].setActive(true);
    this.buildKeyBar();
    this.wireSplitter();
    this.element.addEventListener("keydown", (event) => this.onKeyDown(event), true);
  }

  // ---- AppTab --------------------------------------------------------------

  get title(): string {
    return this.customTitle ?? "File Manager";
  }

  get pinned(): boolean {
    return this.customTitle !== null;
  }

  setCustomTitle(name: string | null): void {
    const trimmed = name?.trim() ?? "";
    this.customTitle = trimmed || null;
    this.onTitleUpdate?.();
  }

  statusInfo(): TabStatusInfo {
    const panel = this.active;
    const fs = providerLabel(panel.fs);
    const marked = panel.model.markedSummary();
    const { folders, files } = panel.model.totals;
    const items = folders + files;
    const parts = [
      marked.count ? `${marked.count} marked · ${formatFileSize(marked.bytes)}` : `${items} items`,
      panel.offline ? "disconnected" : null,
      this.queue.summary(),
    ].filter(Boolean);
    return { kind: `FILES · ${fs}`, context: panel.location?.display, metrics: parts.join(" · ") };
  }

  open(): void {
    if (this.opened) return;
    this.opened = true;
    void this.initialize();
  }

  fit(): void {
    for (const panel of this.panels) panel.renderRows();
  }

  focus(): void {
    if (this.disposed) return;
    this.active.focus();
    this.queue.resume(); // a question that came in while another tab was in front
    this.editQuestionsDeferred = false;
    this.askEditQuestion();
  }

  applySettings(): void {
    // Colors and fonts follow the app theme through CSS variables.
  }

  dispose(): void {
    this.disposed = true;
    void this.statusListener.then((unlisten) => unlisten?.());
    void this.editListener.then((unlisten) => unlisten?.());
    this.lifetime.abort();
    this.queue.cancelAll();
    for (const [type, listener] of this.windowListeners) window.removeEventListener(type, listener);
    this.windowListeners.length = 0;
    for (const panel of this.panels) panel.dispose();
    this.stopFollowing();
    // Panels let go of theirs above; anything left was on its way to one.
    this.claims.dispose();
    if (this.copies.length) {
      const toast = this.toast;
      void fsApi
        .closeCopies(this.copies.splice(0))
        .then((kept) => {
          for (const copy of kept) {
            toast(`Kept your copy of ${copy.name} (${copy.reason}): ${copy.path}`, "warn");
          }
        })
        .catch(() => undefined);
    }
    this.element.remove();
  }

  /** Both panels' folders, for duplicating this tab. */
  currentLocations(): [FsLocationView | undefined, FsLocationView | undefined] {
    return [this.panels[0].location ?? undefined, this.panels[1].location ?? undefined];
  }

  /** Ctrl+Shift+F from the app: filter the active panel. */
  openSearch(): void {
    this.active.openFilter();
  }

  // ---- PanelHost -----------------------------------------------------------

  activate(panel: PanelView): void {
    if (this.active === panel) return;
    this.activeSide = panel.side;
    for (const each of this.panels) each.setActive(each === panel);
    this.onStatusChange?.();
  }

  openRow(panel: PanelView): void {
    void this.openCursor(panel);
  }

  changed(panel: PanelView): void {
    const location = panel.location;
    if (location) {
      // Persist only when the folder changes, not on every cursor move.
      const saved = JSON.stringify(location);
      if (this.savedLocations[panel.side] !== saved) {
        this.savedLocations[panel.side] = saved;
        writeStorage(`${LOCATION_KEY}${panel.side}`, saved);
        writeStorage(`${LOCATION_KEY}${panel.side}.${location.fs}`, saved);
      }
    }
    if (this.following && this.panels[this.following.side].fs !== this.following.fs) {
      this.stopFollowing();
    }
    this.onStatusChange?.();
  }

  contextMenu(panel: PanelView, x: number, y: number): void {
    const targets = panel.model.targets();
    const one = targets.length === 1 ? targets[0] : null;
    const items: MenuItem[] = [];
    if (one) {
      // Act on the entry the menu was opened for, which may be a marked row
      // rather than the one under the cursor.
      items.push({
        label: one.isDirectory ? "Open" : "Open with default app",
        icon: one.isDirectory ? "folder" : "file",
        action: () => void this.openEntry(panel, one),
      });
      if (!one.isDirectory) {
        items.push({ label: "View (F3)", icon: "eye", action: () => void this.view(panel, one) });
      }
    }
    if (targets.length) {
      items.push(
        "sep",
        { label: "Copy to other panel (F5)", icon: "copy", action: () => void this.run("copy") },
        { label: "Move to other panel (F6)", icon: "move", action: () => void this.run("move") },
        { label: "Copy (Ctrl+C)", icon: "copy", action: () => void this.run("clipCopy") },
        { label: "Cut (Ctrl+X)", icon: "move", action: () => void this.run("clipCut") }
      );
    }
    if (clipboard) items.push({ label: "Paste (Ctrl+V)", icon: "copy", action: () => void this.run("clipPaste") });
    if (one) items.push({ label: "Rename (F2)", icon: "pencil", action: () => void this.rename(panel, one) });
    if (targets.some((entry) => entry.isDirectory)) {
      items.push({ label: "Calculate size", icon: "refresh", action: () => void this.measure(panel, targets) });
    }
    if (targets.length) {
      items.push(
        "sep",
        {
          label: panel.capabilities.trash ? "Move to Trash (F8)" : "Delete (F8)",
          icon: "trash",
          danger: true,
          action: () => void this.run("delete"),
        },
        {
          label: "Delete permanently (Shift+F8)",
          icon: "trash",
          danger: true,
          action: () => void this.run("deletePermanent"),
        }
      );
    }
    if (targets.length) {
      items.push({ label: "Properties… (Alt+Enter)", icon: "more", action: () => void this.properties(panel) });
    }
    items.push(
      "sep",
      { label: "New folder (F7)", icon: "folderPlus", action: () => void this.run("mkdir") },
      { label: "Refresh (Ctrl+R)", icon: "refresh", action: () => void this.run("refresh") }
    );
    contextMenu(x, y, items);
  }

  locationMenu(panel: PanelView, anchor: HTMLElement): void {
    void (async () => {
      const go = (location: FsLocationView) => () =>
        void panel.navigate(location).then(() => panel.focus());
      const items: MenuItem[] = [];
      // This machine.
      try {
        const roots = await fsApi.roots("local");
        items.push({ heading: "This PC" });
        for (const root of roots) {
          items.push({
            label: root.label,
            icon: root.label === "Home" ? "home" : "server",
            action: go(root.location),
          });
        }
        items.push({ label: "Choose folder…", icon: "folder", action: () => void this.chooseFolder(panel) });
      } catch (error) {
        this.toast(`Couldn't list this PC's drives: ${error}`, "error");
      }
      // The remote filesystem this panel shows.
      if (isRemote(panel.fs)) {
        try {
          const roots = await fsApi.roots(panel.fs);
          items.push({ heading: remoteLabel(panel.fs) ?? "This server" });
          for (const root of roots) {
            items.push({ label: root.label, icon: root.label === "Home" ? "home" : "server", action: go(root.location) });
          }
          const tab = this.liveConnectionOf(panel.fs);
          if (tab) {
            const on = this.following?.side === panel.side;
            items.push({
              label: on ? "Stop following the terminal's folder" : "Follow the terminal's folder",
              icon: "terminal",
              action: () => (on ? this.stopFollowing() : this.follow(panel, tab)),
            });
          }
        } catch {
          // Disconnected: the banner says so.
        }
      }
      if (this.remote) {
        const [live, saved] = await Promise.all([
          this.remote.live().catch(() => []),
          this.remote.saved().catch(() => []),
        ]);
        if (live.length) {
          items.push({ heading: "SSH tabs" });
          for (const each of live) {
            items.push({
              label: each.label,
              hint: each.folder ? `${each.title} · ${each.folder}` : each.title,
              icon: "terminal",
              action: () => void this.openRemote(panel, { kind: "live", connectionId: each.connectionId }),
            });
          }
        }
        if (saved.length) {
          items.push({ heading: "Saved connections" });
          for (const each of saved) {
            items.push({
              label: each.folder ? `${each.folder} / ${each.name}` : each.name,
              hint: each.label,
              icon: "server",
              action: () => void this.openRemote(panel, { kind: "saved", id: each.id }),
            });
          }
        }
      }
      if (this.disposed || !items.length) return;
      const bounds = anchor.getBoundingClientRect();
      contextMenu(bounds.left, bounds.bottom + 4, items);
    })();
  }

  dragStart(panel: PanelView, event: DragEvent): void {
    const index = panel.rowIndexAt(event.target);
    if (index === null || panel.model.isParentRow(index) || !event.dataTransfer) {
      event.preventDefault();
      return;
    }
    if (!panel.model.isMarked(index)) {
      panel.model.clickRow(index);
      panel.renderStates();
    }
    const entries = panel.model.targets();
    const payload: DragPayload = {
      tab: this.uid,
      side: panel.side,
      fs: panel.fs,
      entries: entries.map(({ name, path }) => ({ name, path })),
      from: panel.location,
    };
    event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(payload));
    event.dataTransfer.setData("text/plain", entries.map((entry) => entry.display).join("\n"));
    event.dataTransfer.effectAllowed = "copyMove";
    panel.element.classList.add("is-drag-source");
    event.target?.addEventListener(
      "dragend",
      () => {
        for (const each of this.panels) {
          each.element.classList.remove("is-drag-source");
          each.setDropTarget(null);
        }
      },
      { once: true }
    );
  }

  reconnect(panel: PanelView): void {
    this.reconnectFs(panel.fs);
  }

  private reconnectFs(fs: string): void {
    // Both panels (and a paused job) may ask: one reconnect at a time.
    if (this.reconnecting.has(fs)) return;
    const relogin = reloginFor(fs);
    if (!relogin) {
      this.toast(
        `No login is known for ${remoteLabel(fs) ?? "this server"}: open it again from the locations (Alt+F1 / Alt+F2)`,
        "warn"
      );
      return;
    }
    this.reconnecting.add(fs);
    void (async () => {
      try {
        const login = await relogin();
        if (!login || this.disposed) return;
        const view = await sftpApi.reconnect(fs, login);
        if (view.notice) this.toast(view.notice, "info");
      } catch (error) {
        this.toast(`Couldn't reconnect: ${error}`, "error");
      } finally {
        this.reconnecting.delete(fs);
      }
    })();
  }

  /** Show a remote filesystem in `panel` (logging in first if it needs to). */
  private async openRemote(
    panel: PanelView,
    target: RemoteTarget,
    follow = false
  ): Promise<boolean> {
    const remote = this.remote;
    if (!remote) return false;
    // A saved connection with an SSH tab open uses the tab's connection.
    if (target.kind === "saved") {
      const live = remote.liveFor(target.id);
      if (live) target = { kind: "live", connectionId: live };
    }
    let options: SftpConnectOptions | null;
    let relogin: (() => Promise<SftpConnectOptions | null>) | null;
    if (target.kind === "live") {
      options = { connectionId: target.connectionId };
      relogin = remote.loginLater(target.connectionId);
    } else {
      try {
        options = await remote.login(target.id);
      } catch (error) {
        this.toast(`Couldn't log in: ${error}`, "error");
        return false;
      }
      const id = target.id;
      relogin = () => remote.login(id);
    }
    if (!options || this.disposed) return false;
    let view: SftpView;
    try {
      view = await sftpApi.connect(options);
    } catch (error) {
      this.toast(`Couldn't open SFTP: ${error}`, "error");
      return false;
    }
    if (view.notice) this.toast(view.notice, "info");
    learn(view, relogin);
    // The reference connect took: held from here until a panel shows it.
    const claim = this.adopt(view.fs);
    if (this.disposed) return false;
    const connectionId = target.kind === "live" ? target.connectionId : null;
    if (connectionId) this.liveConnections.set(view.fs, connectionId);
    // An SSH tab's connection opens where its terminal is (when the shell says).
    let start = view.home;
    if (connectionId) {
      const folder = (await remote.live().catch(() => [])).find(
        (each) => each.connectionId === connectionId
      )?.folder;
      if (folder) start = (await fsApi.resolve(view.fs, folder).catch(() => null)) ?? view.home;
    }
    const shown = await panel.navigate(start, { claim });
    if (shown && connectionId && follow) this.follow(panel, connectionId);
    if (shown && panel === this.active) panel.focus();
    return shown;
  }

  /** Remote filesystems opened on an SSH tab's connection (fs → connection id). */
  private readonly liveConnections = new Map<string, string>();

  private liveConnectionOf(fs: string): string | null {
    return this.liveConnections.get(fs) ?? null;
  }

  /** Keep `panel` in the SSH tab's working folder as its shell reports it. */
  private follow(panel: PanelView, connectionId: string): void {
    this.stopFollowing();
    const fs = panel.fs;
    const watched = this.remote?.watchFolder(connectionId, (folder, host) => {
      const following = this.following;
      if (this.disposed || !following || panel.fs !== fs) return this.stopFollowing();
      // The first host the shell names is this server; another one means an
      // `ssh` inside the terminal: its folders aren't on this server.
      following.host ??= host;
      if (host !== following.host) {
        this.stopFollowing();
        this.toast(`Stopped following: the terminal is on ${host || "another host"} now`, "info");
        return;
      }
      void fsApi
        .resolve(fs, folder)
        .then((location) => panel.navigate(location, { quiet: true }))
        .catch(() => undefined);
    });
    if (!watched) {
      this.toast("No SSH tab uses this connection any more", "warn");
      return;
    }
    this.following = { side: panel.side, fs, connectionId, host: watched.host, stop: watched.stop };
  }

  private stopFollowing(): void {
    this.following?.stop();
    this.following = null;
  }

  /** PanelHost: keep `fs` open for a panel going there (back/forward, a
   * remembered folder, swapping panels). Null when it isn't open any more. */
  claim(fs: string): Promise<(() => void) | null> {
    return this.claims.claim(fs);
  }

  /** The reference `sftpApi.connect` just took, with a claim on it. */
  private adopt(fs: string): () => void {
    return this.claims.adopt(fs);
  }

  /** An edited remote file's save: uploaded, failed, or clashing with the server's. */
  private onEdit(event: EditEvent): void {
    if (this.disposed || !this.copies.includes(event.id)) return;
    switch (event.event) {
      case "uploaded": {
        this.toast(`Uploaded ${event.name}`, "info");
        const edited = this.copyFs.get(event.id);
        for (const panel of this.panels) if (panel.fs === edited) void panel.refresh();
        break;
      }
      case "failed":
        this.toast(`Couldn't upload ${event.name}: ${event.message}`, "error");
        break;
      case "stopped":
        this.toast(
          `No longer uploading ${event.name} (${event.reason}). Your copy is at ${event.path}`,
          "warn"
        );
        break;
      case "conflict":
        // Asked when this tab is in front, one at a time.
        this.editQuestions.push(event);
        this.askEditQuestion();
        break;
      case "uploading":
        break;
    }
  }

  private askEditQuestion(): void {
    if (this.askingEdit || this.disposed || this.editQuestionsDeferred) return;
    if (!this.element.classList.contains("is-active") || !this.editQuestions.length) return;
    if (modalOpen()) {
      // Never on top of another question (Enter there must not answer this).
      this.editQuestionTimer ??= window.setTimeout(() => {
        this.editQuestionTimer = null;
        this.askEditQuestion();
      }, 400);
      return;
    }
    const event = this.editQuestions.shift()!;
    this.askingEdit = true;
    void (async () => {
      const overwrite = await confirmChoice(
        {
          title: "Changed on the server",
          message: `“${event.name}” changed on the server since you opened it. Upload your copy over it?`,
          detail:
            "Keeping the server's version stops uploading your copy (it stays where it is), so nothing of theirs is overwritten later.",
          action: "Upload mine",
          cancel: "Keep the server's",
          danger: true,
          safeDefault: true,
        },
        this.dialogs
      );
      this.askingEdit = false;
      if (this.disposed) return;
      if (overwrite === null) {
        // Put off (Esc): asked again later, not taken as an answer.
        this.editQuestions.unshift(event);
        this.editQuestionsDeferred = true;
        return;
      }
      void fsApi.editResolve(event.id, overwrite).catch(() => undefined);
      this.askEditQuestion();
    })();
  }

  /** A remote filesystem dropped or came back: show it on its panels. */
  private onRemoteStatus(status: SftpStatus): void {
    if (this.disposed) return;
    for (const panel of this.panels) {
      if (panel.fs !== status.fs) continue;
      panel.setOffline(status.connected ? null : (status.message ?? "The connection was lost."));
      if (status.connected) {
        void panel.refresh();
        if (panel === this.active) panel.focus();
      }
    }
    this.onStatusChange?.();
  }

  drop(panel: PanelView, folder: FsEntry | null, event: DragEvent): void {
    let payload: DragPayload;
    try {
      payload = JSON.parse(event.dataTransfer?.getData(DRAG_TYPE) ?? "") as DragPayload;
    } catch {
      return;
    }
    if (!payload.entries?.length) return;
    const location = panel.location;
    if (!location) return;
    const destination: FsLocationView = folder
      ? { fs: panel.fs, path: folder.path, display: folder.display }
      : location;
    const sameFolder =
      payload.from?.fs === destination.fs && payload.from?.path === destination.path;
    if (sameFolder) return; // dropped back where it came from
    if (payload.fs === destination.fs && payload.entries.some((entry) => entry.path === destination.path)) {
      return; // onto one of the dragged folders
    }
    const kind: TransferKind = event.shiftKey ? "move" : "copy";
    void this.startTransfer(kind, payload.fs, payload.entries, payload.from, destination);
  }

  // ---- commands ------------------------------------------------------------

  private get active(): PanelView {
    return this.panels[this.activeSide];
  }

  private get other(): PanelView {
    return this.panels[this.activeSide === 0 ? 1 : 0];
  }

  private async initialize(): Promise<void> {
    let home: Promise<FsLocationView> | null = null;
    const homeFor = () => (home ??= fsApi.home("local"));
    // Both panels at once: an offline remembered share mustn't hold up the other.
    const results = await Promise.all(
      this.panels.map(async (panel) => {
        if (this.openOnStart?.side === panel.side) {
          if (await this.openRemote(panel, this.openOnStart.target, true)) return true;
          if (this.disposed) return false;
        }
        // A remembered remote folder works while its server is still open.
        const wanted = this.initialLocations?.[panel.side] ?? readLocation(panel.side);
        if (wanted && (await panel.navigate(wanted, { quiet: true }))) return true;
        if (this.disposed) return false;
        try {
          return await panel.navigate(await homeFor());
        } catch (error) {
          if (!this.disposed) this.toast(`Couldn't open the file manager: ${error}`, "error");
          return false;
        }
      })
    );
    if (this.disposed) return;
    this.status = results.some(Boolean) ? "connected" : "error";
    this.onStatusChange?.();
    this.focus();
  }

  private onKeyDown(event: KeyboardEvent): void {
    // A context menu handles its own keys; the panel underneath stays put.
    if (event.defaultPrevented || modalOpen() || document.querySelector(".ctx-menu")) return;
    const target = event.target as HTMLElement;
    // Inputs and the Disconnected banner's button (Enter reconnects) keep
    // their keys, but never let F-keys/Ctrl+R reach the webview (F5 / Ctrl+R
    // would reload the whole app, F7 toggles caret browsing).
    if (target.matches("input, textarea") || target.closest(".fm-offline")) {
      const refresh = (event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "r";
      if (/^F\d{1,2}$/.test(event.key) || refresh) event.preventDefault();
      if (refresh) void this.run("refresh");
      return;
    }
    // Keys act on the panel that has keyboard focus, whatever was last clicked.
    const focused = this.panels.find((panel) => panel.element.contains(target));
    if (focused) this.activate(focused);
    const plain = !event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey;
    if (event.key === "Backspace" && plain && this.active.quickSearchActive) {
      // While typing a quick search, Backspace edits it instead of going up.
      event.preventDefault();
      this.active.quickSearchBackspace();
      return;
    }
    const command = commandFor(event);
    if (command) {
      event.preventDefault();
      // Any command key ends a quick search, as in Commander (Esc ends just it).
      if (command !== "clear") this.active.clearQuickSearch();
      void this.run(command);
    } else if (isQuickSearchKey(event)) {
      event.preventDefault();
      this.active.quickSearch(event.key);
    } else if (isBrowserAccelerator(event)) {
      event.preventDefault(); // e.g. Ctrl+F5: never reload the app
    }
  }

  private async run(command: FmCommand): Promise<void> {
    const panel = this.active;
    const { model } = panel;
    const page = panel.pageSize();
    let moved = true;
    switch (command) {
      case "cursorUp":
        model.moveCursor(-1);
        break;
      case "cursorDown":
        model.moveCursor(1);
        break;
      case "pageUp":
        model.moveCursor(-page);
        break;
      case "pageDown":
        model.moveCursor(page);
        break;
      case "home":
        model.setCursor(0);
        break;
      case "end":
        model.setCursor(model.rows.length - 1);
        break;
      case "extendUp":
        model.extendTo(model.cursor - 1);
        break;
      case "extendDown":
        model.extendTo(model.cursor + 1);
        break;
      case "extendPageUp":
        model.extendTo(model.cursor - page);
        break;
      case "extendPageDown":
        model.extendTo(model.cursor + page);
        break;
      case "extendHome":
        model.extendTo(0);
        break;
      case "extendEnd":
        model.extendTo(model.rows.length - 1);
        break;
      case "toggleMarkAdvance":
        model.toggleMarkAndAdvance();
        break;
      case "toggleMark": {
        const entry = model.toggleMarkAtCursor();
        if (entry?.isDirectory && model.marks.has(entry.path)) void this.measure(panel, [entry]);
        break;
      }
      case "markAll":
        model.markAll();
        break;
      case "invertMarks":
        model.invertMarks();
        break;
      case "markMask":
      case "unmarkMask": {
        const mark = command === "markMask";
        const mask = await maskDialog(mark, this.dialogs);
        if (this.disposed) return;
        if (mask) model.markByMask(mask, mark);
        break;
      }
      case "clear":
        if (!panel.clearQuickSearch()) {
          if (panel.filterOpen) panel.closeFilter();
          else model.clearMarks();
        }
        break;
      default:
        moved = false;
    }
    if (moved) {
      panel.renderStates();
      panel.ensureCursorVisible();
      this.changed(panel);
      return;
    }

    switch (command) {
      case "open":
        return this.openCursor(panel);
      case "parent":
        return panel.goUp();
      case "back":
        return panel.goHistory(-1);
      case "forward":
        return panel.goHistory(1);
      case "switchPanel":
        this.activate(this.other);
        this.active.focus();
        return;
      case "swapPanels":
        return this.swapPanels();
      case "root":
        return this.goRoot(panel);
      case "refresh":
        await panel.refresh();
        return;
      case "filter":
        return panel.openFilter();
      case "locationLeft":
      case "locationRight": {
        const target = this.panels[command === "locationLeft" ? 0 : 1];
        this.activate(target);
        target.focus(); // where the keyboard returns if the menu is dismissed
        this.locationMenu(target, target.element.querySelector<HTMLElement>('.fm-tool[data-act="roots"]')!);
        return;
      }
      case "view":
        return this.view(panel);
      case "edit":
        return this.edit(panel, model.cursorEntry(), true);
      case "properties":
        return this.properties(panel);
      case "copy":
      case "move":
        return this.transferToOther(command);
      case "rename":
        return this.rename(panel, model.cursorEntry());
      case "mkdir":
        return this.mkdir(panel);
      case "delete":
      case "deletePermanent":
        return this.remove(panel, command === "deletePermanent");
      case "clipCopy":
      case "clipCut":
        return this.toClipboard(panel, command === "clipCopy" ? "copy" : "move");
      case "clipPaste":
        return this.paste(panel);
    }
  }

  private async openCursor(panel: PanelView): Promise<void> {
    const { model } = panel;
    if (model.isParentRow(model.cursor)) return panel.goUp();
    const entry = model.cursorEntry();
    if (entry) await this.openEntry(panel, entry);
  }

  private async openEntry(panel: PanelView, entry: FsEntry): Promise<void> {
    if (entry.isDirectory) {
      await panel.navigate({ fs: panel.fs, path: entry.path, display: entry.display });
      return;
    }
    return this.edit(panel, entry);
  }

  private async view(panel: PanelView, entry = panel.model.cursorEntry()): Promise<void> {
    if (!entry || entry.isDirectory) {
      this.toast("Put the cursor on a file to view it", "warn");
      return;
    }
    await openViewer(panel.fs, entry, this.dialogs);
  }

  /** Enter (or F4, `editing`) on a file: open it with the OS; a remote one
   * as a downloaded copy, which F4 keeps uploading as it's saved. */
  private async edit(
    panel: PanelView,
    entry = panel.model.cursorEntry(),
    editing = false
  ): Promise<void> {
    if (!entry || entry.isDirectory) return;
    if (!panel.capabilities.openNative) {
      await this.openCopy(panel, entry, editing);
      return;
    }
    try {
      await fsApi.openNative({ fs: panel.fs, path: entry.path });
    } catch (error) {
      this.toast(`Couldn't open ${entry.name}: ${error}`, "error");
    }
  }

  /** Alt+Enter: size, dates, owner, and (POSIX) permissions of the targets. */
  private async properties(panel: PanelView): Promise<void> {
    const entries = panel.model.targets();
    if (!entries.length || !panel.location) return;
    try {
      const changed = await propertiesDialog(
        {
          fs: panel.fs,
          entries,
          folder: panel.location.display,
          posix: panel.capabilities.posixPermissions,
        },
        this.dialogs
      );
      if (changed && !this.disposed) await this.refreshShowing(panel);
    } catch (error) {
      this.toast(`Couldn't change the permissions: ${error}`, "error");
      if (!this.disposed) await this.refreshShowing(panel);
    }
  }

  /** A remote file: download a copy to a temp folder and open it there. */
  private async openCopy(panel: PanelView, entry: FsEntry, editing = false): Promise<void> {
    // Opening a program or script runs it here, with your rights.
    if (!editing && (await fsApi.runsCode(entry.name).catch(() => true))) {
      const run = await confirmAction(
        {
          title: "Open a file from the server?",
          message: `Opening “${visibleName(entry.name)}” may run it on this computer, with your rights: its type isn't one CorePTY knows is safe to open. Only do this if you trust where it came from.`,
          detail: "F3 shows it, F4 opens it in an editor; neither runs it.",
          action: "Open it",
          danger: true,
          safeDefault: true,
        },
        this.dialogs
      );
      if (!run || this.disposed) return;
    }
    const big = (entry.size ?? 0) > LARGE_DOWNLOAD;
    if (big) {
      const go = await confirmAction(
        {
          title: "Download to open?",
          message: `“${entry.name}” is ${formatFileSize(entry.size ?? 0)}. It is downloaded to a temporary folder first.`,
          action: "Download and open",
        },
        this.dialogs
      );
      if (!go || this.disposed) return;
    }
    try {
      const location = { fs: panel.fs, path: entry.path };
      const copy = editing
        ? await fsApi.edit(location, settings.externalEditor)
        : await fsApi.openCopy(location);
      if (this.disposed) {
        void fsApi.closeCopies([copy.id]);
        return;
      }
      this.copies.push(copy.id);
      this.copyFs.set(copy.id, panel.fs);
      if (editing) this.toast(`Editing ${entry.name}: each save is uploaded`, "info");
    } catch (error) {
      this.toast(`Couldn't open ${entry.name}: ${error}`, "error");
    }
  }

  private async transferToOther(kind: TransferKind): Promise<void> {
    const panel = this.active;
    const entries = panel.model.targets();
    const destination = this.other.location;
    if (!entries.length) {
      this.toast("Mark files or put the cursor on one first", "warn");
      return;
    }
    if (!destination) return;
    // Modes are kept only from a POSIX side to a POSIX side.
    const permissions =
      panel.capabilities.posixPermissions && this.other.capabilities.posixPermissions;
    const answer = await copyMoveDialog(
      { kind, what: describe(entries), destination: destination.display, permissions },
      this.dialogs
    );
    if (answer === null || this.disposed) return;
    const typed = answer.destination;
    let target = destination;
    if (typed !== destination.display) {
      try {
        target = await fsApi.resolve(destination.fs, typed, destination);
      } catch (error) {
        this.toast(`Couldn't use ${typed}: ${error}`, "error");
        return;
      }
      if (this.disposed) return;
    }
    await this.startTransfer(kind, panel.fs, entries, panel.location, target, answer.preserveMode);
    panel.model.clearMarks();
    panel.renderStates();
    this.changed(panel);
  }

  private async startTransfer(
    kind: TransferKind,
    fs: string,
    entries: Array<Pick<FsEntry, "name" | "path">>,
    from: FsLocationView | null,
    destination: FsLocationView,
    preserveMode = true
  ): Promise<void> {
    const request: TransferRequest = {
      kind,
      sources: entries.map((entry) => ({ fs, path: entry.path })),
      destination,
      preserveMode,
    };
    this.transferSources.set(request, from);
    const verb = kind === "copy" ? "Copy" : "Move";
    await this.queue.start(request, `${verb} ${describe(entries)} → ${destination.display}`);
  }

  /** Refresh whichever panels show the folders a finished transfer touched. */
  private afterTransfer(request: TransferRequest): void {
    if (this.disposed) return;
    const touched: FsLocation[] = [request.destination];
    const from = this.transferSources.get(request);
    if (from) touched.push(from);
    for (const panel of this.panels) {
      const location = panel.location;
      if (location && touched.some((each) => each.fs === location.fs && each.path === location.path)) {
        void panel.refresh();
      }
    }
  }

  /** F2 / Shift+F6: rename the entry under the cursor (marks don't matter, as in Far / TC). */
  private async rename(panel: PanelView, entry: FsEntry | null): Promise<void> {
    if (!entry) {
      this.toast("Put the cursor on the item to rename", "warn");
      return;
    }
    const name = await promptText(
      {
        title: "Rename",
        label: "New name",
        value: entry.name,
        action: "Rename",
        selectStem: !entry.isDirectory,
      },
      this.dialogs
    );
    if (!name || name === entry.name || this.disposed) return;
    try {
      await fsApi.rename({ fs: panel.fs, path: entry.path }, name);
      await this.refreshShowing(panel, name);
    } catch (error) {
      this.toast(`Couldn't rename ${entry.name}: ${error}`, "error");
    }
  }

  private async mkdir(panel: PanelView): Promise<void> {
    const location = panel.location;
    if (!location) return;
    const name = await promptText(
      { title: "New folder", label: "Name", value: "", action: "Create" },
      this.dialogs
    );
    if (!name || this.disposed) return;
    try {
      await fsApi.mkdir(location, name);
      await this.refreshShowing(panel, name);
    } catch (error) {
      this.toast(`Couldn't create ${name}: ${error}`, "error");
    }
  }

  private async remove(panel: PanelView, permanent: boolean): Promise<void> {
    const targets = panel.model.targets();
    if (!targets.length) {
      this.toast("Mark files or put the cursor on one first", "warn");
      return;
    }
    const toTrash = !permanent && panel.capabilities.trash;
    const folders = targets.filter((entry) => entry.isDirectory);
    const bytes = targets.reduce(
      (sum, entry) =>
        sum + (entry.isDirectory ? (panel.model.folderSizes.get(entry.path) ?? 0) : (entry.size ?? 0)),
      0
    );
    const unmeasured = folders.some((entry) => !panel.model.folderSizes.has(entry.path));
    const confirmed = await confirmAction(
      {
        title: toTrash ? "Move to Trash?" : "Delete permanently?",
        message: `${describe(targets)} (${formatFileSize(bytes)}${unmeasured ? " plus folder contents" : ""}) will be ${
          toTrash ? "moved to the Trash" : "deleted. This can't be undone"
        }.`,
        detail: folders.length ? `Includes ${folders.length} ${folders.length === 1 ? "folder" : "folders"} and everything in them.` : undefined,
        action: toTrash ? "Move to Trash" : "Delete",
        danger: !toTrash,
      },
      this.dialogs
    );
    if (!confirmed || this.disposed) return;
    const locations = targets.map((entry) => ({ fs: panel.fs, path: entry.path }));
    try {
      if (toTrash) await fsApi.trash(locations);
      else await fsApi.remove(locations);
    } catch (error) {
      this.toast(`${toTrash ? "Trash" : "Delete"} failed: ${error}`, "error");
    }
    if (!this.disposed) await this.refreshShowing(panel);
  }

  /** Refresh `panel` (optionally landing on `focusName`) and the other panel if it shows the same folder. */
  private async refreshShowing(panel: PanelView, focusName?: string): Promise<void> {
    const location = panel.location;
    if (!location) return;
    await panel.navigate(location, { keepState: true, history: "none", focusName });
    const other = panel === this.panels[0] ? this.panels[1] : this.panels[0];
    if (other.location?.fs === location.fs && other.location.path === location.path) {
      await other.refresh();
    }
  }

  private async measure(panel: PanelView, entries: FsEntry[]): Promise<void> {
    for (const entry of entries.filter((each) => each.isDirectory)) {
      try {
        const size = await fsApi.folderSize({ fs: panel.fs, path: entry.path });
        if (this.disposed) return;
        panel.model.folderSizes.set(entry.path, size.bytes);
        panel.renderStates();
        this.onStatusChange?.();
      } catch (error) {
        this.toast(`Couldn't measure ${entry.name}: ${error}`, "error");
      }
    }
  }

  private toClipboard(panel: PanelView, kind: TransferKind): void {
    const entries = panel.model.targets();
    if (!entries.length) return;
    clipboard = {
      kind,
      fs: panel.fs,
      entries: entries.map(({ name, path }) => ({ name, path })),
      from: panel.location,
    };
    this.toast(`${kind === "copy" ? "Copied" : "Cut"} ${describe(entries)}`, "info");
  }

  private async paste(panel: PanelView): Promise<void> {
    const location = panel.location;
    if (!clipboard || !location) return;
    const { kind, fs, entries, from } = clipboard;
    if (kind === "move") clipboard = null; // a cut pastes once
    await this.startTransfer(kind, fs, entries, from, location);
  }

  private async chooseFolder(panel: PanelView): Promise<void> {
    const picked = await pickDirectory(panel.location?.display);
    if (!picked) return;
    try {
      await panel.navigate(await fsApi.resolve("local", picked));
      panel.focus();
    } catch (error) {
      this.toast(`Couldn't open ${picked}: ${error}`, "error");
    }
  }

  private async goRoot(panel: PanelView): Promise<void> {
    const location = panel.location;
    if (!location) return;
    try {
      await panel.navigate(await fsApi.resolve(location.fs, rootOf(location.display), location));
    } catch (error) {
      this.toast(`Couldn't open the root folder: ${error}`, "error");
    }
  }

  private async swapPanels(): Promise<void> {
    const [left, right] = this.currentLocations();
    if (!left || !right) return;
    await Promise.all([this.panels[0].navigate(right), this.panels[1].navigate(left)]);
  }

  // ---- chrome ----------------------------------------------------------------

  private buildKeyBar(): void {
    const bar = this.element.querySelector<HTMLElement>(".fm-keys")!;
    for (const { key, label, command } of FUNCTION_KEYS) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = `fm-key${command === "delete" ? " fm-key--danger" : ""}`;
      button.dataset.command = command;
      button.innerHTML = `<kbd>${key}</kbd><span class="fm-key__label"></span>`;
      button.querySelector(".fm-key__label")!.textContent = label;
      // Holding Shift shows (and runs) the Shift variants, as in Commander.
      if (command === "move") button.dataset.shift = "Rename";
      if (command === "delete") button.dataset.shift = "Delete!";
      button.dataset.label = label;
      button.addEventListener("mousedown", (event) => event.preventDefault()); // keep panel focus
      button.addEventListener("click", (event) => {
        const shifted =
          event.shiftKey && command === "move" ? "rename" : event.shiftKey && command === "delete" ? "deletePermanent" : command;
        void this.run(shifted);
      });
      bar.appendChild(button);
    }
    const setShift = (shift: boolean) => {
      bar.querySelectorAll<HTMLElement>(".fm-key").forEach((button) => {
        const label = button.querySelector(".fm-key__label")!;
        label.textContent = shift && button.dataset.shift ? button.dataset.shift : button.dataset.label!;
      });
    };
    this.element.addEventListener("keydown", (event) => {
      if (event.key === "Shift") setShift(true);
    });
    this.element.addEventListener("keyup", (event) => {
      if (event.key === "Shift") setShift(false);
    });
    const onBlur = () => setShift(false);
    window.addEventListener("blur", onBlur);
    this.windowListeners.push(["blur", onBlur]);
  }

  private wireSplitter(): void {
    const splitter = this.element.querySelector<HTMLElement>(".fm-splitter")!;
    const panels = this.element.querySelector<HTMLElement>(".fm-panels")!;
    const setSplit = (percent: number) => {
      const clamped = Math.max(25, Math.min(75, percent));
      panels.style.setProperty("--fm-split", `${clamped}%`);
      splitter.setAttribute("aria-valuenow", String(Math.round(clamped)));
      return clamped;
    };
    const stored = Number(readStorage(SPLIT_KEY));
    let current = setSplit(Number.isFinite(stored) && stored >= 25 && stored <= 75 ? stored : 50);
    let moved = false;
    splitter.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      splitter.setPointerCapture(event.pointerId);
      splitter.classList.add("is-dragging");
      moved = false;
    });
    splitter.addEventListener("pointermove", (event) => {
      if (!splitter.hasPointerCapture(event.pointerId)) return;
      const bounds = panels.getBoundingClientRect();
      current = setSplit(((event.clientX - bounds.left) / bounds.width) * 100);
      moved = true;
    });
    const finish = (event: PointerEvent) => {
      if (!splitter.hasPointerCapture(event.pointerId)) return;
      splitter.releasePointerCapture(event.pointerId);
      splitter.classList.remove("is-dragging");
      if (moved) writeStorage(SPLIT_KEY, String(current)); // a plain click changes nothing
    };
    splitter.addEventListener("pointerup", finish);
    splitter.addEventListener("pointercancel", finish);
    splitter.addEventListener("dblclick", () => writeStorage(SPLIT_KEY, String((current = setSplit(50)))));
    splitter.addEventListener("keydown", (event) => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault();
      event.stopPropagation();
      current = setSplit(current + (event.key === "ArrowRight" ? 2 : -2));
      writeStorage(SPLIT_KEY, String(current));
    });
  }
}

/** “report.txt” or “3 items”. */
function describe(entries: ReadonlyArray<Pick<FsEntry, "name">>): string {
  return entries.length === 1 ? `“${entries[0].name}”` : `${entries.length} items`;
}

/** The root of a displayed path: `C:\`, `\\server\share\`, or `/`. */
export function rootOf(display: string): string {
  const drive = /^[A-Za-z]:[\\/]/.exec(display);
  if (drive) return `${display.slice(0, 2)}\\`;
  const unc = /^\\\\[^\\]+\\[^\\]+/.exec(display);
  if (unc) return `${unc[0]}\\`;
  return "/";
}

function readLocation(side: 0 | 1): FsLocationView | null {
  try {
    const parsed = JSON.parse(readStorage(`${LOCATION_KEY}${side}`) ?? "null") as FsLocationView | null;
    return parsed && typeof parsed.fs === "string" && typeof parsed.path === "string" ? parsed : null;
  } catch {
    return null;
  }
}

function readStorage(key: string): string | undefined {
  try {
    return window.localStorage.getItem(key) ?? undefined;
  } catch {
    return undefined;
  }
}

function writeStorage(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Persistence is a convenience; private webviews may disable localStorage.
  }
}


// Application shell: sidebar, tab bar, terminal stage, status bar, settings,
// reconnect, and the connections tree.
import {
  api,
  b64ToBytes,
  onData,
  onExit,
  onStatus,
  type LocalShell,
  type SavedSession,
  type SessionKind,
  type SshConnectOptions,
  type TelnetConnectOptions,
} from "./ipc";
import { icon } from "./icons";
import { TerminalSession } from "./terminal";
import { ConnectionDialog, type ConnForm, type DialogPrefill } from "./dialog";
import { ConnectionsTree } from "./connections";
import { contextMenu, type MenuItem } from "./menu";
import { SettingsDialog } from "./settings-dialog";
import { current as settings, loadSettings } from "./settings";
import { activeTheme, preloadThemeFonts } from "./themes";
import { escapeHtml, uuid } from "./util";
import { winClose, winMinimize, winSetDecorations, winStartResize, winToggleMaximize } from "./window";

/** App version, injected at build time by Vite (see vite.config.ts). */
declare const __APP_VERSION__: string;

interface ShellDef {
  shell: LocalShell;
  label: string;
  iconName: string;
  hint: string;
}

const SHELLS: ShellDef[] = [
  { shell: "powershell", label: "PowerShell", iconName: "powershell", hint: "Windows PowerShell 5.1" },
  { shell: "pwsh", label: "PowerShell 7", iconName: "pwsh", hint: "Cross-platform pwsh" },
  { shell: "cmd", label: "Command Prompt", iconName: "cmd", hint: "cmd.exe" },
  { shell: "bash", label: "Bash", iconName: "bash", hint: "Git Bash / WSL" },
];

export class App {
  private readonly root: HTMLElement;
  private readonly dialog = new ConnectionDialog();
  private readonly settingsDialog = new SettingsDialog(() => this.applySettingsToAll());
  private tree!: ConnectionsTree;
  private tabs: TerminalSession[] = [];
  private byId = new Map<string, TerminalSession>();
  private active: TerminalSession | null = null;
  /** uid of the tab whose inline-rename input is currently open (if any). */
  private renamingUid: string | null = null;
  /** Last tab mousedown, for manual double-click (survives tab re-render). */
  private lastTabClick: { uid: string; at: number } | null = null;
  /** Shared theme-styled tooltip element for tab hover, and its show timer. */
  private tabTipEl: HTMLElement | null = null;
  private tabTipTimer: number | null = null;

  private stageEl!: HTMLElement;
  private tabsListEl!: HTMLElement;
  private connectionsEl!: HTMLElement;
  private statusEls!: {
    dot: HTMLElement;
    title: HTMLElement;
    kind: HTMLElement;
    os: HTMLElement;
    dims: HTMLElement;
  };
  private toastEl!: HTMLElement;
  private searchBar!: HTMLElement;
  private searchInput!: HTMLInputElement;
  private resizeRaf = 0;

  constructor(root: HTMLElement) {
    this.root = root;
  }

  async mount(): Promise<void> {
    await loadSettings();
    this.render();
    this.syncWindowFrame();
    this.tree = new ConnectionsTree(this.connectionsEl, {
      onConnect: (s) => void this.connectSaved(s),
      onEdit: (s) => void this.openDialog(s),
      onNewConnection: (folderId) => void this.openDialog(undefined, { folderId }),
      toast: (m, k) => this.toast(m, k),
    });
    this.wireGlobalEvents();
    // Web fonts load lazily; wait for the theme fonts before the first terminal
    // measures its glyph cell, else selection columns and row count come out wrong.
    await preloadThemeFonts();
    await this.newLocal(this.defaultShell());
    void this.tree.refresh();
  }

  private defaultShell(): LocalShell {
    return settings.defaultShell;
  }

  // ---- layout -------------------------------------------------------------

  private render(): void {
    this.root.innerHTML = `
      <div class="win">
        <div class="win-titlebar" data-tauri-drag-region>
          <span class="win-titlebar__label" data-tauri-drag-region>CorePTY</span>
          <div class="win-controls">
            <span class="win-controls__cap" data-tauri-drag-region aria-hidden="true"></span>
            <button class="win-btn" data-win="min" title="Minimize">${icon("winMin")}</button>
            <button class="win-btn" data-win="max" title="Maximize">${icon("winMax")}</button>
            <button class="win-btn win-btn--close" data-win="close" title="Close">${icon("close")}</button>
          </div>
        </div>
        <div class="app">
        <aside class="sidebar">
          <div class="brand">
            <span class="brand__mark">${icon("terminal", "brand__icon")}</span>
            <span class="brand__name">Core<span>PTY</span></span>
          </div>

          <div class="side-scroll">
            <div class="side-group">
              <div class="side-group__label">Local Shells</div>
              <div class="quick" id="quick"></div>
            </div>

            <div class="side-group">
              <div class="side-group__label">
                Connections
                <span class="side-group__actions">
                  <button class="side-add" id="new-folder" title="New folder">${icon("folderPlus")}</button>
                  <button class="side-add" id="new-connection" title="New connection">${icon("plus")}</button>
                </span>
              </div>
              <div class="connections" id="connections"></div>
            </div>
          </div>

          <div class="side-footer">
            <button class="side-footer__btn" id="open-settings">${icon("settings")}<span>Settings</span></button>
            <span class="side-footer__ver">v${__APP_VERSION__}</span>
          </div>
        </aside>

        <main class="main">
          <div class="tabbar">
            <div class="tabs" id="tabs"></div>
            <button class="tab-add" id="tab-add" title="New tab (Ctrl+Shift+T)">${icon("plus")}</button>
          </div>

          <div class="stage" id="stage">
            <div class="search-bar" id="search-bar" hidden>
              ${icon("search", "search-bar__icon")}
              <input id="search-input" class="search-bar__input" placeholder="Search buffer…" spellcheck="false" />
              <button class="search-bar__btn" data-act="prev" title="Previous (Shift+Enter)">${icon("chevronDown", "flip")}</button>
              <button class="search-bar__btn" data-act="next" title="Next (Enter)">${icon("chevronDown")}</button>
              <button class="search-bar__btn" data-act="close" title="Close (Esc)">${icon("close")}</button>
            </div>
          </div>

          <div class="statusbar">
            <div class="statusbar__left">
              <span class="status-dot" id="st-dot"></span>
              <span class="status-title" id="st-title">—</span>
              <span class="status-kind" id="st-kind"></span>
              <span class="status-os" id="st-os"></span>
            </div>
            <div class="statusbar__right">
              <span id="st-dims" class="status-dims"></span>
            </div>
          </div>
        </main>

        <div class="toasts" id="toasts"></div>
        </div>
        <div class="win-resizers">
          <div class="wr wr-n" data-dir="North"></div>
          <div class="wr wr-e" data-dir="East"></div>
          <div class="wr wr-s" data-dir="South"></div>
          <div class="wr wr-w" data-dir="West"></div>
          <div class="wr wr-nw" data-dir="NorthWest"></div>
          <div class="wr wr-ne" data-dir="NorthEast"></div>
          <div class="wr wr-se" data-dir="SouthEast"></div>
          <div class="wr wr-sw" data-dir="SouthWest"></div>
        </div>
      </div>
    `;

    this.stageEl = this.root.querySelector("#stage")!;
    this.tabsListEl = this.root.querySelector("#tabs")!;
    this.connectionsEl = this.root.querySelector("#connections")!;
    this.toastEl = this.root.querySelector("#toasts")!;
    this.searchBar = this.root.querySelector("#search-bar")!;
    this.searchInput = this.root.querySelector("#search-input")!;
    this.statusEls = {
      dot: this.root.querySelector("#st-dot")!,
      title: this.root.querySelector("#st-title")!,
      kind: this.root.querySelector("#st-kind")!,
      os: this.root.querySelector("#st-os")!,
      dims: this.root.querySelector("#st-dims")!,
    };

    const quick = this.root.querySelector("#quick")!;
    quick.innerHTML = SHELLS.map(
      (s) => `
      <button class="quick__btn" data-shell="${s.shell}">
        <span class="quick__icon">${icon(s.iconName)}</span>
        <span class="quick__text"><span class="quick__label">${s.label}</span><span class="quick__hint">${s.hint}</span></span>
        <span class="quick__go">${icon("plus")}</span>
      </button>`
    ).join("");
    quick.querySelectorAll<HTMLButtonElement>(".quick__btn").forEach((btn) => {
      const shell = () => btn.dataset.shell as LocalShell;
      btn.addEventListener("click", () => this.newLocal(shell()));
      btn.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        contextMenu(e.clientX, e.clientY, [
          { label: "New tab", icon: "plus", action: () => void this.newLocal(shell()) },
          {
            label: "Run as Administrator",
            icon: "shield",
            action: () => void this.newLocalElevated(shell()),
          },
        ]);
      });
    });

    this.root
      .querySelector("#tab-add")!
      .addEventListener("click", (e) => this.openLauncher(e.currentTarget as HTMLElement));
    this.root
      .querySelector("#new-connection")!
      .addEventListener("click", () => void this.openDialog());
    this.root
      .querySelector("#new-folder")!
      .addEventListener("click", () => void this.tree.newFolder(null));
    this.root
      .querySelector("#open-settings")!
      .addEventListener("click", () => this.settingsDialog.open());

    this.setupStageInteractions();
    this.setupSearch();
    this.wireWindowChrome();
  }

  // ---- saved connections --------------------------------------------------

  private async openDialog(existing?: SavedSession, prefill: DialogPrefill = {}): Promise<void> {
    const result = await this.dialog.open(existing, prefill);
    if (!result) return;
    const form = result.form;
    if (existing) form.id = existing.id;
    if (result.action === "save") {
      await this.saveProfile(form);
    } else {
      if (form.name || existing) await this.saveProfile(form, true);
      this.connectForm(form);
    }
  }

  private async saveProfile(form: ConnForm, silent = false): Promise<string> {
    const id = form.id ?? uuid();
    const saved: SavedSession = {
      id,
      name: form.name || form.host,
      kind: form.kind,
      host: form.host,
      port: form.port,
      username: form.kind === "ssh" ? form.username : null,
      authType: form.kind === "ssh" ? form.authType : null,
      keyPath: form.authType === "key" ? form.keyPath : null,
      saveSecret: form.kind === "ssh" ? form.saveSecret : false,
      folderId: form.folderId ?? null,
    };
    await api.sessionsUpsert(saved);
    if (saved.saveSecret) {
      const secret = form.authType === "key" ? form.passphrase : form.password;
      if (secret) await api.secretSet(id, secret);
      else await api.secretDelete(id);
    } else {
      await api.secretDelete(id);
    }
    await this.tree.refresh();
    if (!silent) this.toast(`Saved "${saved.name}"`, "info");
    return id;
  }

  private async connectSaved(s: SavedSession): Promise<void> {
    let secret = "";
    if (s.saveSecret) {
      try {
        secret = (await api.secretGet(s.id)) ?? "";
      } catch {
        secret = "";
      }
    }
    const form: ConnForm = {
      id: s.id,
      name: s.name,
      kind: s.kind,
      host: s.host,
      port: s.port ?? (s.kind === "ssh" ? 22 : 23),
      username: s.username ?? "",
      authType: s.authType ?? "password",
      password: s.authType !== "key" ? secret : "",
      keyPath: s.keyPath ?? "",
      passphrase: s.authType === "key" ? secret : "",
      saveSecret: s.saveSecret,
      folderId: s.folderId ?? null,
    };
    if (s.kind === "ssh" && form.authType === "password" && !form.password) {
      void this.openDialog(s);
      return;
    }
    this.connectForm(form);
  }

  private connectForm(form: ConnForm): void {
    if (form.kind === "ssh") void this.newSsh(form);
    else void this.newTelnet(form);
  }

  // ---- sessions -----------------------------------------------------------

  private beginSession(session: TerminalSession): { id: string; cols: number; rows: number } {
    this.addTab(session);
    session.open();
    session.fit();
    const id = uuid();
    this.byId.set(id, session);
    const { cols, rows } = session.dims();
    return { id, cols, rows };
  }

  async newLocal(shell: LocalShell): Promise<void> {
    const def = SHELLS.find((s) => s.shell === shell);
    const session = new TerminalSession("local", def?.label ?? "Shell", def?.iconName ?? "terminal");
    session.spec = { kind: "local", shell };
    const { id, cols, rows } = this.beginSession(session);
    try {
      const info = await api.createLocal({ id, shell, cols, rows });
      session.attach(info);
      session.setStatus("connected");
    } catch (err) {
      this.byId.delete(id);
      session.markError(String(err));
      this.toast(`Couldn't start ${def?.label ?? shell}: ${err}`, "error");
    }
    this.renderTabs();
    this.updateStatus();
  }

  /** Open a local shell elevated (Administrator) — raises a UAC prompt. */
  async newLocalElevated(shell: LocalShell): Promise<void> {
    const def = SHELLS.find((s) => s.shell === shell);
    const label = def?.label ?? "Shell";
    const title = `${label} (Admin)`;
    const session = new TerminalSession("local", title, def?.iconName ?? "terminal");
    session.elevated = true;
    session.spec = { kind: "local", shell, elevated: true };
    const { id, cols, rows } = this.beginSession(session);
    try {
      const info = await api.createLocalElevated({ id, shell, cols, rows, title });
      session.attach(info);
      // Status is driven by pty://status events: connecting → connected once the
      // broker attaches after the UAC prompt.
    } catch (err) {
      this.byId.delete(id);
      session.markError(String(err));
      this.toast(`Couldn't start ${label} as admin: ${err}`, "error");
    }
    this.renderTabs();
    this.updateStatus();
  }

  async newSsh(form: ConnForm): Promise<void> {
    const title = form.name || `${form.username}@${form.host}`;
    const session = new TerminalSession("ssh", title, "ssh");
    session.spec = { kind: "ssh", form };
    const { id, cols, rows } = this.beginSession(session);
    try {
      const info = await api.createSsh(this.sshOptions(form, id, cols, rows));
      session.attach(info);
    } catch (err) {
      this.byId.delete(id);
      session.markError(String(err));
    }
    this.renderTabs();
    this.updateStatus();
  }

  async newTelnet(form: ConnForm): Promise<void> {
    const title = form.name || `telnet ${form.host}`;
    const session = new TerminalSession("telnet", title, "telnet");
    session.spec = { kind: "telnet", form };
    const { id, cols, rows } = this.beginSession(session);
    try {
      const info = await api.createTelnet(this.telnetOptions(form, id, cols, rows));
      session.attach(info);
    } catch (err) {
      this.byId.delete(id);
      session.markError(String(err));
    }
    this.renderTabs();
    this.updateStatus();
  }

  private sshOptions(form: ConnForm, id: string, cols: number, rows: number): SshConnectOptions {
    const auth =
      form.authType === "key"
        ? { type: "key" as const, keyPath: form.keyPath, passphrase: form.passphrase || undefined }
        : { type: "password" as const, password: form.password };
    return {
      id,
      host: form.host,
      port: form.port,
      username: form.username,
      auth,
      cols,
      rows,
      title: form.name || `${form.username}@${form.host}`,
    };
  }

  private telnetOptions(form: ConnForm, id: string, cols: number, rows: number): TelnetConnectOptions {
    return { id, host: form.host, port: form.port, cols, rows, title: form.name || `telnet ${form.host}` };
  }

  private async relaunch(session: TerminalSession): Promise<void> {
    const spec = session.spec;
    if (!spec) return;
    if (session.info) {
      void api.close(session.info.id).catch(() => {});
      this.byId.delete(session.info.id);
    }
    session.beginReconnect();
    session.fit();
    const { cols, rows } = session.dims();
    const id = uuid();
    this.byId.set(id, session);
    try {
      if (spec.kind === "local") {
        if (spec.elevated) {
          session.attach(await api.createLocalElevated({ id, shell: spec.shell, cols, rows }));
        } else {
          session.attach(await api.createLocal({ id, shell: spec.shell, cols, rows }));
          session.setStatus("connected");
        }
      } else if (spec.kind === "ssh") {
        session.attach(await api.createSsh(this.sshOptions(spec.form, id, cols, rows)));
      } else {
        session.attach(await api.createTelnet(this.telnetOptions(spec.form, id, cols, rows)));
      }
    } catch (err) {
      this.byId.delete(id);
      session.markError(String(err));
    }
    this.renderTabs();
    this.updateStatus();
    session.focus();
  }

  private addTab(session: TerminalSession): void {
    session.onTitleUpdate = () => {
      this.renderTabs();
      if (session === this.active) this.updateStatus();
    };
    session.onStatusChange = () => {
      this.renderTabs();
      if (session === this.active) this.updateStatus();
    };
    session.onExit = () => {
      this.renderTabs();
      if (session === this.active) this.updateStatus();
    };
    session.onReconnect = () => void this.relaunch(session);
    session.onClose = () => void this.closeTab(session);
    this.tabs.push(session);
    this.stageEl.appendChild(session.element);
    this.activate(session);
  }

  private activate(session: TerminalSession): void {
    this.active = session;
    for (const t of this.tabs) t.element.classList.toggle("is-active", t === session);
    this.renderTabs();
    this.updateStatus();
    requestAnimationFrame(() => {
      session.open();
      session.fit();
      session.focus();
    });
  }

  private async closeTab(session: TerminalSession): Promise<void> {
    const idx = this.tabs.indexOf(session);
    if (idx < 0) return;
    if (session.info) {
      void api.close(session.info.id).catch(() => {});
      this.byId.delete(session.info.id);
    }
    session.dispose();
    this.tabs.splice(idx, 1);

    if (this.active === session) {
      const next = this.tabs[idx] ?? this.tabs[idx - 1] ?? null;
      this.active = null;
      if (next) this.activate(next);
      else {
        this.renderTabs();
        this.updateStatus();
        void this.newLocal(this.defaultShell());
      }
    } else {
      this.renderTabs();
    }
  }

  // ---- tab bar ------------------------------------------------------------

  private renderTabs(): void {
    this.hideTabTip();
    // Never rebuild the strip while an inline-rename input is open (it would
    // destroy the field mid-edit, e.g. if the program fires an OSC title).
    if (this.renamingUid) return;
    this.tabsListEl.innerHTML = "";
    for (const t of this.tabs) {
      const el = document.createElement("div");
      el.className = "tab" + (t === this.active ? " is-active" : "");
      el.dataset.key = t.uid;
      el.setAttribute("aria-label", t.title);
      el.innerHTML = `
        <span class="tab__icon">${icon(t.iconName)}</span>
        <span class="tab__title">${escapeHtml(t.title)}</span>
        ${t.elevated ? `<span class="tab__admin" title="Administrator">${icon("shield")}</span>` : ""}
        <span class="tab__dot ${t.status}"></span>
        <button class="tab__close" title="Close (Ctrl+Shift+W)">${icon("close")}</button>
      `;
      el.addEventListener("mouseenter", () => this.showTabTip(el, t));
      el.addEventListener("mouseleave", () => this.hideTabTip());
      el.addEventListener("mousedown", (e) => {
        this.hideTabTip();
        if ((e.target as HTMLElement).closest(".tab__close, .tab__rename")) return;
        if (e.button === 1) {
          e.preventDefault();
          void this.closeTab(t);
          return;
        }
        if (e.button !== 0) return;
        // Double-click → rename, detected manually and keyed on the tab uid so it
        // survives the strip re-rendering on the first click's activate().
        const now = Date.now();
        const prev = this.lastTabClick;
        this.lastTabClick = { uid: t.uid, at: now };
        if (prev && prev.uid === t.uid && now - prev.at < 400) {
          this.lastTabClick = null;
          e.preventDefault();
          this.startTabRename(t);
          return;
        }
        this.activate(t);
      });
      el.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        this.openTabMenu(t, e.clientX, e.clientY);
      });
      el.querySelector(".tab__close")!.addEventListener("click", (e) => {
        e.stopPropagation();
        void this.closeTab(t);
      });
      this.tabsListEl.appendChild(el);
    }
  }

  /** Right-click menu on a tab: duplicate, rename, reset the name, or close. */
  private openTabMenu(session: TerminalSession, x: number, y: number): void {
    this.hideTabTip();
    const items: MenuItem[] = [
      { label: "Duplicate session", icon: "copy", action: () => this.duplicateSession(session) },
      { label: "Rename…", icon: "pencil", action: () => this.startTabRename(session) },
    ];
    if (session.pinned) {
      items.push({
        label: "Reset name",
        icon: "refresh",
        action: () => session.setCustomTitle(null),
      });
    }
    items.push("sep", {
      label: "Close tab",
      icon: "close",
      danger: true,
      action: () => void this.closeTab(session),
    });
    contextMenu(x, y, items);
  }

  /** Open a new tab launching the same thing as this tab (reusing its spec). */
  private duplicateSession(session: TerminalSession): void {
    const spec = session.spec;
    if (!spec) {
      this.toast("This tab can't be duplicated", "warn");
      return;
    }
    if (spec.kind === "local") {
      if (spec.elevated) void this.newLocalElevated(spec.shell);
      else void this.newLocal(spec.shell);
    } else if (spec.kind === "ssh") void this.newSsh(spec.form);
    else void this.newTelnet(spec.form);
  }

  // ---- tab hover tooltip (theme-styled) -----------------------------------

  private ensureTabTip(): HTMLElement {
    if (!this.tabTipEl) {
      this.tabTipEl = document.createElement("div");
      this.tabTipEl.className = "tab-tip";
      document.body.appendChild(this.tabTipEl);
    }
    return this.tabTipEl;
  }

  /** Show the full tab title in a themed tooltip under the tab, after a beat. */
  private showTabTip(anchor: HTMLElement, session: TerminalSession): void {
    if (this.tabTipTimer) clearTimeout(this.tabTipTimer);
    this.tabTipTimer = window.setTimeout(() => {
      const tip = this.ensureTabTip();
      tip.textContent = session.title;
      const r = anchor.getBoundingClientRect();
      tip.style.top = `${Math.round(r.bottom + 6)}px`;
      tip.style.left = `${Math.round(r.left)}px`;
      tip.classList.add("show");
      // clamp within the viewport now that the tip has a measured width
      const left = Math.max(8, Math.min(r.left, window.innerWidth - tip.offsetWidth - 8));
      tip.style.left = `${Math.round(left)}px`;
    }, 300);
  }

  private hideTabTip(): void {
    if (this.tabTipTimer) {
      clearTimeout(this.tabTipTimer);
      this.tabTipTimer = null;
    }
    this.tabTipEl?.classList.remove("show");
  }

  /**
   * Inline-rename a tab. The typed name is *pinned*: from then on it overrides
   * any OSC title the program or remote host sets (now and in the future) until
   * it's reset. Submitting an empty value clears the pin, handing control back
   * to the OSC / default title.
   */
  private startTabRename(session: TerminalSession): void {
    const tabEl = this.tabsListEl.querySelector<HTMLElement>(`.tab[data-key="${session.uid}"]`);
    const titleEl = tabEl?.querySelector<HTMLElement>(".tab__title");
    if (!tabEl || !titleEl) return;

    const input = document.createElement("input");
    input.className = "tab__rename";
    input.value = session.title;
    input.spellcheck = false;
    titleEl.replaceWith(input);
    this.renamingUid = session.uid;
    input.focus();
    input.select();

    let done = false;
    const finish = (save: boolean): void => {
      if (done) return;
      done = true;
      this.renamingUid = null;
      if (save) session.setCustomTitle(input.value);
      this.renderTabs();
      if (session === this.active) this.updateStatus();
    };
    input.addEventListener("mousedown", (e) => e.stopPropagation());
    input.addEventListener("dblclick", (e) => e.stopPropagation());
    input.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Enter") {
        e.preventDefault();
        finish(true);
      } else if (e.key === "Escape") {
        e.preventDefault();
        finish(false);
      }
    });
    input.addEventListener("blur", () => finish(true));
  }

  // ---- status bar ---------------------------------------------------------

  private updateStatus(): void {
    const s = this.active;
    const { dot, title, kind, os, dims } = this.statusEls;
    if (!s) {
      title.textContent = "—";
      kind.textContent = "";
      os.textContent = "";
      dims.textContent = "";
      dot.className = "status-dot";
      return;
    }
    title.textContent = s.title;
    kind.textContent = kindLabel(s.kind);
    os.textContent = s.osTitle && s.osTitle !== s.title ? s.osTitle : "";
    dims.textContent = s.alive ? `${s.term.cols} × ${s.term.rows}` : "";
    dot.className = `status-dot ${s.status}`;
  }

  // ---- launcher popover ---------------------------------------------------

  private openLauncher(anchor: HTMLElement): void {
    this.closeLauncher();
    const menu = document.createElement("div");
    menu.className = "launcher pop";
    menu.innerHTML = `
      <div class="pop__label">Local shell</div>
      ${SHELLS.map(
        (s) => `<button class="pop__item" data-shell="${s.shell}">
          <span class="pop__icon">${icon(s.iconName)}</span>
          <span class="pop__name">${s.label}</span>
          <span class="pop__hint">${s.hint}</span>
        </button>`
      ).join("")}
      <div class="pop__sep"></div>
      <div class="pop__label">Remote</div>
      <button class="pop__item" data-conn="ssh"><span class="pop__icon">${icon("ssh")}</span><span class="pop__name">SSH connection</span></button>
      <button class="pop__item" data-conn="telnet"><span class="pop__icon">${icon("telnet")}</span><span class="pop__name">Telnet connection</span></button>
    `;
    document.body.appendChild(menu);
    const r = anchor.getBoundingClientRect();
    menu.style.top = `${r.bottom + 6}px`;
    menu.style.left = `${Math.max(8, r.right - menu.offsetWidth)}px`;

    menu.querySelectorAll<HTMLButtonElement>(".pop__item").forEach((item) => {
      item.addEventListener("click", () => {
        this.closeLauncher();
        if (item.dataset.shell) void this.newLocal(item.dataset.shell as LocalShell);
        else if (item.dataset.conn)
          void this.openDialog(undefined, { presetKind: item.dataset.conn as "ssh" | "telnet" });
      });
    });

    setTimeout(() => {
      document.addEventListener(
        "mousedown",
        (e) => {
          if (!menu.contains(e.target as Node)) this.closeLauncher();
        },
        { once: true }
      );
    });
  }

  private closeLauncher(): void {
    document.querySelectorAll(".launcher").forEach((m) => m.remove());
  }

  // ---- terminal stage interactions (clipboard, focus) ---------------------

  private setupStageInteractions(): void {
    this.stageEl.addEventListener("mouseup", () => {
      if (!settings.copyOnSelect) return;
      const s = this.active;
      if (s && s.hasSelection()) {
        const text = s.getSelection();
        if (text) navigator.clipboard.writeText(text).catch(() => {});
      }
    });

    this.stageEl.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      const s = this.active;
      if (!s) return;
      if (settings.rightClick === "menu") {
        const items = [];
        if (s.hasSelection()) items.push({ label: "Copy", icon: "copy", action: () => this.copy() });
        items.push({ label: "Paste", icon: "clipboard", action: () => void this.paste() });
        items.push({ label: "Clear buffer", action: () => s.term.clear() });
        contextMenu(e.clientX, e.clientY, items);
        return;
      }
      if (s.hasSelection()) {
        navigator.clipboard.writeText(s.getSelection()).catch(() => {});
        s.term.clearSelection();
      } else {
        void this.paste();
      }
    });

    this.stageEl.addEventListener("mousedown", (e) => {
      if (e.target === this.stageEl) this.active?.focus();
    });

    const ro = new ResizeObserver(() => {
      if (this.resizeRaf) cancelAnimationFrame(this.resizeRaf);
      this.resizeRaf = requestAnimationFrame(() => this.active?.fit());
    });
    ro.observe(this.stageEl);
  }

  private async paste(): Promise<void> {
    const s = this.active;
    if (!s?.info || !s.alive) return;
    try {
      const text = await navigator.clipboard.readText();
      if (text) await api.write(s.info.id, text);
    } catch {
      this.toast("Clipboard access was blocked", "warn");
    }
  }

  private copy(): void {
    const s = this.active;
    if (s?.hasSelection()) navigator.clipboard.writeText(s.getSelection()).catch(() => {});
  }

  // ---- settings -----------------------------------------------------------

  private applySettingsToAll(): void {
    this.syncWindowFrame();
    for (const t of this.tabs) t.applySettings();
    requestAnimationFrame(() => this.active?.fit());
    // The frameless transition resizes the window asynchronously; re-fit once it
    // settles so the terminal's row count matches the new layout.
    setTimeout(() => this.active?.fit(), 200);
  }

  // ---- window chrome (frameless per theme) --------------------------------

  /** Match the native window frame to the active theme (LCARS is frameless). */
  private syncWindowFrame(): void {
    void winSetDecorations(!activeTheme().frameless).then(() => {
      requestAnimationFrame(() => this.active?.fit());
    });
  }

  private wireWindowChrome(): void {
    this.root.querySelectorAll<HTMLButtonElement>(".win-btn[data-win]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const act = btn.dataset.win;
        if (act === "min") void winMinimize();
        else if (act === "max") void winToggleMaximize();
        else if (act === "close") void winClose();
      });
    });
    this.root.querySelectorAll<HTMLElement>(".wr[data-dir]").forEach((h) => {
      h.addEventListener("mousedown", (e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        void winStartResize(h.dataset.dir!);
      });
    });
  }

  // ---- search overlay -----------------------------------------------------

  private setupSearch(): void {
    const run = (dir: "next" | "prev") => {
      const q = this.searchInput.value;
      if (!q || !this.active) return;
      if (dir === "next") this.active.search.findNext(q);
      else this.active.search.findPrevious(q);
    };
    this.searchInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        run(e.shiftKey ? "prev" : "next");
      } else if (e.key === "Escape") {
        this.toggleSearch(false);
      }
    });
    this.searchBar.querySelectorAll<HTMLButtonElement>(".search-bar__btn").forEach((b) => {
      b.addEventListener("click", () => {
        const act = b.dataset.act;
        if (act === "next") run("next");
        else if (act === "prev") run("prev");
        else this.toggleSearch(false);
      });
    });
  }

  private toggleSearch(show?: boolean): void {
    const next = show ?? this.searchBar.hasAttribute("hidden");
    if (next) {
      this.searchBar.removeAttribute("hidden");
      this.searchInput.focus();
      this.searchInput.select();
    } else {
      this.searchBar.setAttribute("hidden", "");
      this.active?.search.clearDecorations();
      this.active?.focus();
    }
  }

  // ---- global events & shortcuts ------------------------------------------

  private wireGlobalEvents(): void {
    void onData(({ id, data }) => this.byId.get(id)?.writeBytes(b64ToBytes(data)));
    void onExit(({ id, code, message }) => {
      const s = this.byId.get(id);
      if (!s) return;
      if (message) s.markError(message);
      else s.markExited(code);
    });
    void onStatus(({ id, status }) => {
      const s = this.byId.get(id);
      if (!s) return;
      if (status === "connected") s.setStatus("connected");
      else if (status === "connecting") s.setStatus("connecting");
      else if (status === "error") s.setStatus("error");
    });

    document.addEventListener(
      "keydown",
      (e) => {
        const ctrl = e.ctrlKey || e.metaKey;
        if (ctrl && e.shiftKey && (e.key === "T" || e.key === "t")) {
          e.preventDefault();
          void this.newLocal(this.defaultShell());
        } else if (ctrl && e.shiftKey && (e.key === "N" || e.key === "n")) {
          e.preventDefault();
          void this.openDialog();
        } else if (ctrl && e.shiftKey && (e.key === "W" || e.key === "w")) {
          e.preventDefault();
          if (this.active) void this.closeTab(this.active);
        } else if (ctrl && e.shiftKey && (e.key === "R" || e.key === "r")) {
          e.preventDefault();
          if (this.active) void this.relaunch(this.active);
        } else if (ctrl && e.shiftKey && (e.key === "F" || e.key === "f")) {
          e.preventDefault();
          this.toggleSearch();
        } else if (ctrl && e.key === ",") {
          e.preventDefault();
          this.settingsDialog.open();
        } else if (ctrl && e.shiftKey && (e.key === "C" || e.key === "c")) {
          if (this.active?.hasSelection()) {
            e.preventDefault();
            this.copy();
          }
        } else if (ctrl && e.shiftKey && (e.key === "V" || e.key === "v")) {
          e.preventDefault();
          void this.paste();
        } else if (ctrl && e.key === "Tab") {
          e.preventDefault();
          this.cycleTab(e.shiftKey ? -1 : 1);
        } else if (ctrl && (e.key === "PageDown" || e.key === "PageUp")) {
          e.preventDefault();
          this.cycleTab(e.key === "PageDown" ? 1 : -1);
        }
      },
      true
    );
  }

  private cycleTab(dir: number): void {
    if (this.tabs.length < 2 || !this.active) return;
    const i = this.tabs.indexOf(this.active);
    const next = this.tabs[(i + dir + this.tabs.length) % this.tabs.length];
    this.activate(next);
  }

  // ---- toasts -------------------------------------------------------------

  toast(message: string, kind: "info" | "warn" | "error" = "info"): void {
    const el = document.createElement("div");
    el.className = `toast toast--${kind}`;
    el.textContent = message;
    this.toastEl.appendChild(el);
    requestAnimationFrame(() => el.classList.add("show"));
    setTimeout(() => {
      el.classList.remove("show");
      setTimeout(() => el.remove(), 250);
    }, 4000);
  }
}

function kindLabel(kind: SessionKind): string {
  return kind === "local" ? "local" : kind.toUpperCase();
}

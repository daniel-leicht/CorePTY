import { icon } from "./icons";
import {
  api,
  pickDirectory,
  type ConflictPolicy,
  type DirectoryListing,
  type FileEntry,
  type FileLocation,
  type FileOperationEvent,
  type FileOperationKind,
} from "./ipc";
import { contextMenu, type MenuItem } from "./menu";
import {
  formatFileSize,
  formatModified,
  rangeTokens,
  visibleEntries,
  type FileSort,
  type FileSortKey,
} from "./file-manager-model";
import type { AppTab, TabStatus, TabStatusInfo } from "./tab";
import { uuid } from "./util";

const PATH_KEYS = ["corepty.files.left", "corepty.files.right"] as const;
const SPLIT_KEY = "corepty.files.split";
const DRAG_TYPE = "application/x-corepty-files";

interface PanelState {
  location: FileLocation | null;
  parent: FileLocation | null;
  entries: FileEntry[];
  visible: FileEntry[];
  selected: Set<string>;
  cursor: string | null;
  anchor: string | null;
  history: FileLocation[];
  historyIndex: number;
  sort: FileSort;
  showHidden: boolean;
  query: string;
  loadSequence: number;
}

interface DragPayload {
  sourcePanel: number;
  tokens: string[];
}

interface FileClipboard {
  kind: FileOperationKind;
  tokens: string[];
}

export interface FileManagerOptions {
  toast: (message: string, kind?: "info" | "warn" | "error") => void;
  initialPaths?: [string | undefined, string | undefined];
}

let fileClipboard: FileClipboard | null = null;

/** A modern, keyboard-first two-pane local file manager. */
export class FileManagerTab implements AppTab {
  readonly tabType = "file-manager" as const;
  readonly uid = uuid();
  readonly element: HTMLDivElement;
  readonly iconName = "files";
  readonly elevated = false;

  onTitleUpdate?: () => void;
  onStatusChange?: () => void;
  onClose?: () => void;

  status: TabStatus = "connecting";
  private customTitle: string | null = null;
  private opened = false;
  private disposed = false;
  private activePanel = 0;
  private operationId: string | null = null;
  private modalEl: HTMLElement | null = null;
  private typeahead = "";
  private typeaheadTimer: number | null = null;
  private readonly panels: [PanelState, PanelState] = [newPanel(), newPanel()];
  private readonly toast: FileManagerOptions["toast"];
  private readonly initialPaths?: FileManagerOptions["initialPaths"];

  constructor(options: FileManagerOptions) {
    this.toast = options.toast;
    this.initialPaths = options.initialPaths;
    this.element = document.createElement("div");
    this.element.className = "file-manager";
    this.element.innerHTML = this.template();
    this.wire();
  }

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
    const panel = this.panels[this.activePanel];
    const count = panel.selected.size;
    return {
      kind: "FILES · LOCAL",
      context: panel.location?.path,
      metrics: `${count ? `${count} selected · ` : ""}${panel.visible.length} items`,
    };
  }

  open(): void {
    if (this.opened) return;
    this.opened = true;
    void this.initialize();
  }

  fit(): void {
    // CSS grid owns sizing; keep the active cursor visible after a tab/window resize.
    this.cursorRow(this.activePanel)?.scrollIntoView({ block: "nearest" });
  }

  focus(): void {
    this.gridElement(this.activePanel)?.focus({ preventScroll: true });
  }

  applySettings(): void {
    // File-manager colors and fonts are inherited live from the active app theme.
  }

  dispose(): void {
    this.disposed = true;
    if (this.operationId) void api.filesCancel(this.operationId).catch(() => false);
    if (this.typeaheadTimer !== null) window.clearTimeout(this.typeaheadTimer);
    this.modalEl
      ?.querySelector<HTMLButtonElement>("[data-value=cancel], .modal__close")
      ?.click();
    this.closeModal();
    this.element.remove();
  }

  /** Paths used when duplicating this tab. */
  currentPaths(): [string | undefined, string | undefined] {
    return [this.panels[0].location?.path, this.panels[1].location?.path];
  }

  openSearch(): void {
    this.openFilter(this.activePanel);
  }

  private template(): string {
    return `
      <div class="fm-workspace">
        <div class="fm-panels">
          ${this.panelTemplate(0, "Left file panel")}
          <div class="fm-splitter" role="separator" aria-label="Resize file panels" aria-orientation="vertical" tabindex="0"></div>
          ${this.panelTemplate(1, "Right file panel")}
        </div>
        <div class="fm-operation" hidden>
          <div class="fm-operation__copy">
            <span class="fm-operation__title">Preparing…</span>
            <span class="fm-operation__detail"></span>
          </div>
          <div class="fm-progress" aria-label="File operation progress"><span></span></div>
          <button class="fm-operation__cancel btn ghost" type="button">Cancel</button>
        </div>
        <div class="fm-actions" aria-label="File actions">
          ${this.actionButton("F3", "Preview", "eye", "preview")}
          ${this.actionButton("F5", "Copy", "copy", "copy")}
          ${this.actionButton("F6", "Move", "move", "move")}
          ${this.actionButton("F7", "New folder", "folderPlus", "mkdir")}
          ${this.actionButton("F8", "Trash", "trash", "trash", true)}
        </div>
      </div>`;
  }

  private panelTemplate(index: number, label: string): string {
    return `
      <section class="fm-panel ${index === 0 ? "is-active" : ""}" data-panel="${index}" aria-label="${label}">
        <div class="fm-toolbar">
          <div class="fm-nav">
            ${toolButton("back", "Back (Alt+Left)", "back")}
            ${toolButton("forward", "Forward (Alt+Right)", "forward")}
            ${toolButton("up", "Parent folder (Alt+Up)", "up")}
            ${toolButton("home", "Home", "home")}
          </div>
          <span class="fm-provider">LOCAL</span>
          <input class="fm-path" aria-label="Folder path" spellcheck="false" autocomplete="off" />
          ${toolButton("roots", "Locations", "server")}
          ${toolButton("browse", "Choose folder…", "folder")}
          ${toolButton("filter", "Filter files (Ctrl+F)", "search")}
          ${toolButton("hidden", "Show hidden files", "eye")}
          ${toolButton("refresh", "Refresh (Ctrl+R)", "refresh")}
        </div>
        <div class="fm-filter" hidden>
          ${icon("search")}
          <input aria-label="Filter files" placeholder="Filter this folder…" spellcheck="false" />
          <button type="button" data-act="close-filter" title="Clear filter">${icon("close")}</button>
        </div>
        <div class="fm-grid" role="grid" aria-label="Files" aria-multiselectable="true" tabindex="0">
          <div class="fm-header" role="row" aria-rowindex="1">
            <button role="columnheader" aria-colindex="1" data-sort="name" class="fm-col-name is-sorted">Name <span>↑</span></button>
            <button role="columnheader" aria-colindex="2" data-sort="size" class="fm-col-size">Size <span></span></button>
            <button role="columnheader" aria-colindex="3" data-sort="modified" class="fm-col-modified">Modified <span></span></button>
          </div>
          <div class="fm-list" role="rowgroup">
            <div class="fm-loading">Loading folder…</div>
          </div>
        </div>
        <div class="fm-panel-status">
          <span class="fm-panel-status__count">Loading…</span>
          <span class="fm-panel-status__selection"></span>
          <span class="fm-panel-status__hint">Drop here to copy · hold Shift to move</span>
        </div>
      </section>`;
  }

  private actionButton(
    key: string,
    label: string,
    iconName: string,
    command: string,
    danger = false
  ): string {
    return `<button type="button" class="fm-action ${danger ? "fm-action--danger" : ""}" data-command="${command}">
      <kbd>${key}</kbd>${icon(iconName)}<span>${label}</span>
    </button>`;
  }

  private wire(): void {
    for (let index = 0; index < 2; index += 1) this.wirePanel(index);
    this.wireSplitter();
    this.element.querySelectorAll<HTMLButtonElement>(".fm-action").forEach((button) => {
      button.addEventListener("click", () => this.runCommand(button.dataset.command ?? ""));
    });
    this.element
      .querySelector<HTMLButtonElement>(".fm-operation__cancel")!
      .addEventListener("click", () => {
        if (this.operationId) void api.filesCancel(this.operationId);
      });
    this.element.addEventListener("keydown", (event) => this.onKeyDown(event), true);
  }

  private wirePanel(index: number): void {
    const panel = this.panelElement(index)!;
    panel.addEventListener("pointerdown", () => this.activatePanel(index));

    panel.querySelectorAll<HTMLButtonElement>(".fm-tool[data-act]").forEach((button) => {
      button.addEventListener("click", (event) => {
        event.stopPropagation();
        this.activatePanel(index);
        void this.runToolbarAction(index, button.dataset.act ?? "", button);
      });
    });

    const pathInput = panel.querySelector<HTMLInputElement>(".fm-path")!;
    pathInput.addEventListener("focus", () => this.activatePanel(index));
    pathInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        void this.navigateEnteredPath(index, pathInput.value);
      } else if (event.key === "Escape") {
        event.preventDefault();
        pathInput.value = this.panels[index].location?.path ?? "";
        this.focus();
      }
    });

    panel.querySelectorAll<HTMLButtonElement>(".fm-header [data-sort]").forEach((button) => {
      button.addEventListener("click", () => {
        this.activatePanel(index);
        this.changeSort(index, button.dataset.sort as FileSortKey);
      });
    });

    const filter = panel.querySelector<HTMLElement>(".fm-filter")!;
    const filterInput = filter.querySelector<HTMLInputElement>("input")!;
    filterInput.addEventListener("input", () => {
      this.panels[index].query = filterInput.value;
      this.rebuildVisible(index);
    });
    filterInput.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        this.closeFilter(index);
      } else if (event.key === "Enter") {
        event.preventDefault();
        this.gridElement(index)?.focus();
      }
    });
    filter.querySelector<HTMLButtonElement>("[data-act=close-filter]")!.addEventListener("click", () =>
      this.closeFilter(index)
    );

    const list = this.listElement(index)!;
    this.gridElement(index)!.addEventListener("focus", () => this.activatePanel(index));
    list.addEventListener("contextmenu", (event) => this.openPanelMenu(index, event));
    list.addEventListener("dragover", (event) => this.onDragOver(index, event));
    list.addEventListener("dragleave", (event) => this.onDragLeave(event));
    list.addEventListener("drop", (event) => void this.onDrop(index, event));
  }

  private wireSplitter(): void {
    const splitter = this.element.querySelector<HTMLElement>(".fm-splitter")!;
    const panels = this.element.querySelector<HTMLElement>(".fm-panels")!;
    const stored = Number(readStorage(SPLIT_KEY));
    this.setSplit(Number.isFinite(stored) && stored >= 25 && stored <= 75 ? stored : 50);

    splitter.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      splitter.setPointerCapture(event.pointerId);
      splitter.classList.add("is-dragging");
    });
    splitter.addEventListener("pointermove", (event) => {
      if (!splitter.hasPointerCapture(event.pointerId)) return;
      const bounds = panels.getBoundingClientRect();
      this.setSplit(((event.clientX - bounds.left) / bounds.width) * 100);
    });
    const finish = (event: PointerEvent) => {
      if (!splitter.hasPointerCapture(event.pointerId)) return;
      splitter.releasePointerCapture(event.pointerId);
      splitter.classList.remove("is-dragging");
      writeStorage(SPLIT_KEY, panels.style.getPropertyValue("--fm-split").replace("%", ""));
    };
    splitter.addEventListener("pointerup", finish);
    splitter.addEventListener("pointercancel", finish);
    splitter.addEventListener("dblclick", () => {
      this.setSplit(50);
      writeStorage(SPLIT_KEY, "50");
    });
    splitter.addEventListener("keydown", (event) => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault();
      const current = Number(panels.style.getPropertyValue("--fm-split").replace("%", "")) || 50;
      const next = current + (event.key === "ArrowRight" ? 2 : -2);
      this.setSplit(next);
      writeStorage(SPLIT_KEY, String(next));
    });
  }

  private setSplit(percent: number): void {
    const clamped = Math.max(25, Math.min(75, percent));
    this.element
      .querySelector<HTMLElement>(".fm-panels")!
      .style.setProperty("--fm-split", `${clamped}%`);
    this.element
      .querySelector<HTMLElement>(".fm-splitter")!
      .setAttribute("aria-valuenow", String(Math.round(clamped)));
  }

  private async initialize(): Promise<void> {
    try {
      const home = await api.filesHome();
      const locations = await Promise.all(
        [0, 1].map(async (index) => {
          const saved = this.initialPaths?.[index] ?? readStorage(PATH_KEYS[index]);
          if (!saved) return home;
          return api.filesResolve(saved).catch(() => home);
        })
      );
      await Promise.all([
        this.navigate(0, locations[0], "replace"),
        this.navigate(1, locations[1], "replace"),
      ]);
      this.setStatus("connected");
      this.focus();
    } catch (error) {
      this.setStatus("error");
      this.toast(`Couldn't open the file manager: ${error}`, "error");
    }
  }

  private async navigate(
    index: number,
    location: FileLocation,
    history: "push" | "replace" | "none" = "push"
  ): Promise<boolean> {
    const panel = this.panels[index];
    const sequence = ++panel.loadSequence;
    this.setPanelLoading(index, true);
    try {
      const listing = await api.filesList(location.token);
      if (this.disposed || sequence !== panel.loadSequence) return false;
      this.applyListing(index, listing);
      this.setPanelLoading(index, false);
      if (history === "replace") {
        panel.history = [listing.location];
        panel.historyIndex = 0;
      } else if (history === "push") {
        panel.history = panel.history.slice(0, panel.historyIndex + 1);
        if (panel.history[panel.history.length - 1]?.token !== listing.location.token) {
          panel.history.push(listing.location);
        }
        panel.historyIndex = panel.history.length - 1;
      }
      writeStorage(PATH_KEYS[index], listing.location.path);
      this.renderPanel(index);
      this.onStatusChange?.();
      return true;
    } catch (error) {
      if (sequence === panel.loadSequence) {
        this.setPanelLoading(index, false);
        this.toast(`Couldn't open folder: ${error}`, "error");
      }
      return false;
    }
  }

  private applyListing(index: number, listing: DirectoryListing): void {
    const panel = this.panels[index];
    panel.location = listing.location;
    panel.parent = listing.parent;
    panel.entries = listing.entries;
    panel.selected.clear();
    panel.cursor = null;
    panel.anchor = null;
    panel.visible = visibleEntries(panel.entries, panel.showHidden, panel.query, panel.sort);
  }

  private renderPanel(index: number): void {
    const state = this.panels[index];
    const panel = this.panelElement(index)!;
    const path = panel.querySelector<HTMLInputElement>(".fm-path")!;
    path.value = state.location?.path ?? "";
    path.title = state.location?.path ?? "";
    this.updateToolbar(index);
    this.updateSortHeader(index);
    this.renderRows(index);
  }

  private renderRows(index: number): void {
    const panel = this.panels[index];
    const list = this.listElement(index)!;
    this.gridElement(index)!.setAttribute("aria-rowcount", String(panel.visible.length + 1));
    const scrollTop = list.scrollTop;
    list.replaceChildren();
    if (panel.visible.length === 0) {
      const empty = document.createElement("div");
      empty.className = "fm-empty";
      empty.innerHTML = panel.query
        ? `${icon("search")}<span>No files match “${escapeText(panel.query)}”</span>`
        : `${icon("folder")}<span>This folder is empty</span>`;
      list.appendChild(empty);
    } else {
      panel.visible.forEach((entry, rowIndex) => list.appendChild(this.createRow(index, entry, rowIndex)));
    }
    list.scrollTop = scrollTop;
    this.updateSelectionUI(index);
    this.updatePanelStatus(index);
  }

  private createRow(index: number, entry: FileEntry, rowIndex: number): HTMLElement {
    const row = document.createElement("div");
    row.className = "fm-row";
    row.id = `fm-${this.uid}-${index}-${rowIndex}`;
    row.role = "row";
    row.setAttribute("aria-rowindex", String(rowIndex + 2));
    row.dataset.token = entry.token;
    row.draggable = true;
    row.title = entry.path;
    row.innerHTML = `
      <span class="fm-cell fm-col-name" role="gridcell">
        <span class="fm-row__mark">${icon("check")}</span>
        <span class="fm-row__icon">${icon(entryIcon(entry))}</span>
        <span class="fm-row__name"></span>
        ${entry.kind === "symlink" ? `<span class="fm-row__badge">LINK</span>` : ""}
        ${entry.readonly ? `<span class="fm-row__badge">READ ONLY</span>` : ""}
      </span>
      <span class="fm-cell fm-col-size" role="gridcell">${entry.isDirectory ? "—" : formatFileSize(entry.size)}</span>
      <span class="fm-cell fm-col-modified" role="gridcell">${formatModified(entry.modified)}</span>`;
    row.querySelector<HTMLElement>(".fm-row__name")!.textContent = entry.name;
    row.addEventListener("click", (event) => this.onRowClick(index, entry, event));
    row.addEventListener("dblclick", (event) => {
      event.preventDefault();
      void this.openEntry(index, entry);
    });
    row.addEventListener("dragstart", (event) => this.onDragStart(index, entry, event));
    row.addEventListener("dragend", () => this.clearDropTargets());
    return row;
  }

  private onRowClick(index: number, entry: FileEntry, event: MouseEvent): void {
    this.activatePanel(index);
    const panel = this.panels[index];
    if (event.shiftKey && panel.anchor) {
      panel.selected = new Set(rangeTokens(panel.visible, panel.anchor, entry.token));
    } else if (event.ctrlKey || event.metaKey) {
      if (panel.selected.has(entry.token)) panel.selected.delete(entry.token);
      else panel.selected.add(entry.token);
      panel.anchor = entry.token;
    } else {
      panel.selected = new Set([entry.token]);
      panel.anchor = entry.token;
    }
    panel.cursor = entry.token;
    this.updateSelectionUI(index);
    this.gridElement(index)?.focus({ preventScroll: true });
  }

  private activatePanel(index: number): void {
    if (this.activePanel === index) return;
    this.activePanel = index;
    this.element.querySelectorAll<HTMLElement>(".fm-panel").forEach((panel) => {
      panel.classList.toggle("is-active", Number(panel.dataset.panel) === index);
    });
    this.onStatusChange?.();
  }

  private updateSelectionUI(index: number): void {
    const panel = this.panels[index];
    const list = this.listElement(index)!;
    const grid = this.gridElement(index)!;
    let activeId = "";
    list.querySelectorAll<HTMLElement>(".fm-row").forEach((row) => {
      const token = row.dataset.token ?? "";
      const selected = panel.selected.has(token);
      const cursor = panel.cursor === token;
      row.classList.toggle("is-selected", selected);
      row.classList.toggle("is-cursor", cursor);
      row.setAttribute("aria-selected", String(selected));
      if (cursor) activeId = row.id;
    });
    if (activeId) grid.setAttribute("aria-activedescendant", activeId);
    else grid.removeAttribute("aria-activedescendant");
    this.updatePanelStatus(index);
    this.onStatusChange?.();
  }

  private updatePanelStatus(index: number): void {
    const panel = this.panels[index];
    const root = this.panelElement(index)!;
    const selected = this.selectedEntries(index);
    const selectedBytes = selected.reduce((sum, entry) => sum + (entry.size ?? 0), 0);
    root.querySelector<HTMLElement>(".fm-panel-status__count")!.textContent =
      `${panel.visible.length} ${panel.visible.length === 1 ? "item" : "items"}`;
    root.querySelector<HTMLElement>(".fm-panel-status__selection")!.textContent = selected.length
      ? `${selected.length} selected${selectedBytes ? ` · ${formatFileSize(selectedBytes)}` : ""}`
      : "";
  }

  private updateToolbar(index: number): void {
    const panel = this.panels[index];
    const root = this.panelElement(index)!;
    buttonFor(root, "back").disabled = panel.historyIndex <= 0;
    buttonFor(root, "forward").disabled = panel.historyIndex >= panel.history.length - 1;
    buttonFor(root, "up").disabled = !panel.parent;
    buttonFor(root, "hidden").classList.toggle("is-active", panel.showHidden);
    buttonFor(root, "hidden").title = panel.showHidden ? "Hide hidden files" : "Show hidden files";
  }

  private updateSortHeader(index: number): void {
    const panel = this.panels[index];
    this.panelElement(index)!
      .querySelectorAll<HTMLButtonElement>(".fm-header [data-sort]")
      .forEach((button) => {
        const active = button.dataset.sort === panel.sort.key;
        button.classList.toggle("is-sorted", active);
        button.querySelector("span")!.textContent = active
          ? panel.sort.direction === "asc"
            ? "↑"
            : "↓"
          : "";
        button.setAttribute(
          "aria-sort",
          active ? (panel.sort.direction === "asc" ? "ascending" : "descending") : "none"
        );
      });
  }

  private rebuildVisible(index: number): void {
    const panel = this.panels[index];
    panel.visible = visibleEntries(panel.entries, panel.showHidden, panel.query, panel.sort);
    const visibleTokens = new Set(panel.visible.map((entry) => entry.token));
    panel.selected = new Set([...panel.selected].filter((token) => visibleTokens.has(token)));
    if (panel.cursor && !visibleTokens.has(panel.cursor)) panel.cursor = panel.visible[0]?.token ?? null;
    this.renderPanel(index);
  }

  private changeSort(index: number, key: FileSortKey): void {
    const panel = this.panels[index];
    if (panel.sort.key === key) {
      panel.sort.direction = panel.sort.direction === "asc" ? "desc" : "asc";
    } else {
      panel.sort = { key, direction: "asc" };
    }
    this.rebuildVisible(index);
  }

  private async runToolbarAction(index: number, action: string, anchor: HTMLElement): Promise<void> {
    const panel = this.panels[index];
    if (action === "refresh") await this.refreshPanel(index);
    else if (action === "up" && panel.parent) await this.navigate(index, panel.parent);
    else if (action === "home") await this.navigate(index, await api.filesHome());
    else if (action === "back") await this.goHistory(index, -1);
    else if (action === "forward") await this.goHistory(index, 1);
    else if (action === "browse") {
      const picked = await pickDirectory(panel.location?.path);
      if (picked) await this.navigate(index, await api.filesResolve(picked));
    } else if (action === "roots") await this.openRoots(index, anchor);
    else if (action === "hidden") {
      panel.showHidden = !panel.showHidden;
      this.rebuildVisible(index);
    } else if (action === "filter") this.openFilter(index);
  }

  private async openRoots(index: number, anchor: HTMLElement): Promise<void> {
    try {
      const roots = await api.filesRoots();
      const items: MenuItem[] = roots.map((root) => ({
        label: root.label,
        icon: root.label === "Home" ? "home" : "server",
        action: () => void this.navigate(index, root.location),
      }));
      const bounds = anchor.getBoundingClientRect();
      contextMenu(bounds.left, bounds.bottom + 4, items);
    } catch (error) {
      this.toast(`Couldn't list locations: ${error}`, "error");
    }
  }

  private async goHistory(index: number, direction: number): Promise<void> {
    const panel = this.panels[index];
    const next = panel.historyIndex + direction;
    const location = panel.history[next];
    if (!location) return;
    if (await this.navigate(index, location, "none")) {
      panel.historyIndex = next;
      this.updateToolbar(index);
    }
  }

  private async navigateEnteredPath(index: number, entered: string): Promise<void> {
    const panel = this.panels[index];
    try {
      const location = await api.filesResolve(entered, panel.location?.token);
      await this.navigate(index, location);
    } catch (error) {
      this.toast(`Couldn't open folder: ${error}`, "error");
      this.panelElement(index)!.querySelector<HTMLInputElement>(".fm-path")!.value =
        panel.location?.path ?? "";
    }
  }

  private async refreshPanel(index: number): Promise<void> {
    const location = this.panels[index].location;
    if (location) await this.navigate(index, location, "none");
  }

  private setPanelLoading(index: number, loading: boolean): void {
    const root = this.panelElement(index);
    if (!root) return;
    root.classList.toggle("is-loading", loading);
    if (loading) {
      const list = this.listElement(index)!;
      if (!this.panels[index].entries.length) {
        list.innerHTML = '<div class="fm-loading">Loading folder…</div>';
      }
    }
  }

  private openFilter(index: number): void {
    this.activatePanel(index);
    const filter = this.panelElement(index)!.querySelector<HTMLElement>(".fm-filter")!;
    filter.hidden = false;
    const input = filter.querySelector<HTMLInputElement>("input")!;
    input.value = this.panels[index].query;
    input.focus();
    input.select();
  }

  private closeFilter(index: number): void {
    const panel = this.panels[index];
    panel.query = "";
    const filter = this.panelElement(index)!.querySelector<HTMLElement>(".fm-filter")!;
    filter.hidden = true;
    filter.querySelector<HTMLInputElement>("input")!.value = "";
    this.rebuildVisible(index);
    this.gridElement(index)?.focus();
  }

  private async openEntry(index: number, entry: FileEntry): Promise<void> {
    if (entry.isDirectory) {
      await this.navigate(index, {
        provider: this.panels[index].location?.provider ?? "local",
        path: entry.path,
        token: entry.token,
      });
      return;
    }
    try {
      await api.filesOpen(entry.token);
    } catch (error) {
      this.toast(`Couldn't open ${entry.name}: ${error}`, "error");
    }
  }

  private onKeyDown(event: KeyboardEvent): void {
    if (this.modalEl || event.defaultPrevented) return;
    const target = event.target as HTMLElement;
    const ctrl = event.ctrlKey || event.metaKey;
    if (ctrl && event.key.toLowerCase() === "r") {
      event.preventDefault();
      void this.refreshPanel(this.activePanel);
      return;
    }
    if (target.matches("input")) return;
    const panel = this.panels[this.activePanel];

    if (event.key === "Tab") {
      event.preventDefault();
      this.activatePanel(this.activePanel === 0 ? 1 : 0);
      this.focus();
    } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      this.moveCursor(event.key === "ArrowDown" ? 1 : -1, event.shiftKey);
    } else if (event.key === "PageDown" || event.key === "PageUp") {
      event.preventDefault();
      this.moveCursor(event.key === "PageDown" ? 12 : -12, event.shiftKey);
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      this.setCursor(event.key === "Home" ? 0 : panel.visible.length - 1, event.shiftKey);
    } else if (event.key === "Enter") {
      event.preventDefault();
      const entry = this.cursorEntry(this.activePanel);
      if (entry) void this.openEntry(this.activePanel, entry);
    } else if (event.key === "Backspace" || (event.altKey && event.key === "ArrowUp")) {
      event.preventDefault();
      if (panel.parent) void this.navigate(this.activePanel, panel.parent);
    } else if (event.altKey && (event.key === "ArrowLeft" || event.key === "ArrowRight")) {
      event.preventDefault();
      void this.goHistory(this.activePanel, event.key === "ArrowLeft" ? -1 : 1);
    } else if (event.key === "Insert" || event.key === " ") {
      event.preventDefault();
      this.toggleCursorSelection(event.key === "Insert");
    } else if (ctrl && event.key.toLowerCase() === "a") {
      event.preventDefault();
      panel.selected = new Set(panel.visible.map((entry) => entry.token));
      this.updateSelectionUI(this.activePanel);
    } else if (ctrl && event.key.toLowerCase() === "f") {
      event.preventDefault();
      this.openFilter(this.activePanel);
    } else if (ctrl && event.key.toLowerCase() === "c") {
      event.preventDefault();
      this.storeClipboard("copy");
    } else if (ctrl && event.key.toLowerCase() === "x") {
      event.preventDefault();
      this.storeClipboard("move");
    } else if (ctrl && event.key.toLowerCase() === "v") {
      event.preventDefault();
      void this.pasteClipboard();
    } else if (event.key === "F2") {
      event.preventDefault();
      void this.renameSelected();
    } else if (event.key === "F3") {
      event.preventDefault();
      void this.previewSelected();
    } else if (event.key === "F5") {
      event.preventDefault();
      void this.transferToOther("copy");
    } else if (event.key === "F6") {
      event.preventDefault();
      void this.transferToOther("move");
    } else if (event.key === "F7") {
      event.preventDefault();
      void this.createFolder();
    } else if (event.key === "F8" || event.key === "Delete") {
      event.preventDefault();
      void this.deleteSelected(event.shiftKey);
    } else if (event.key === "Escape") {
      event.preventDefault();
      if (this.operationId) void api.filesCancel(this.operationId);
      else {
        panel.selected.clear();
        this.updateSelectionUI(this.activePanel);
      }
    } else if (!ctrl && !event.altKey && !event.metaKey && event.key.length === 1) {
      this.typeToJump(event.key);
    }
  }

  private moveCursor(delta: number, extend: boolean): void {
    const panel = this.panels[this.activePanel];
    if (!panel.visible.length) return;
    const current = panel.visible.findIndex((entry) => entry.token === panel.cursor);
    const requested = current < 0 ? (delta > 0 ? 0 : panel.visible.length - 1) : current + delta;
    const next = Math.max(0, Math.min(panel.visible.length - 1, requested));
    this.setCursor(next, extend);
  }

  private setCursor(index: number, extend: boolean): void {
    const panel = this.panels[this.activePanel];
    const entry = panel.visible[index];
    if (!entry) return;
    const previousAnchor = panel.anchor;
    panel.cursor = entry.token;
    if (extend && previousAnchor) {
      panel.selected = new Set(rangeTokens(panel.visible, previousAnchor, entry.token));
    } else {
      panel.selected = new Set([entry.token]);
      panel.anchor = entry.token;
    }
    this.updateSelectionUI(this.activePanel);
    this.cursorRow(this.activePanel)?.scrollIntoView({ block: "nearest" });
  }

  private toggleCursorSelection(advance: boolean): void {
    const panel = this.panels[this.activePanel];
    let index = panel.visible.findIndex((entry) => entry.token === panel.cursor);
    if (index < 0) index = 0;
    const entry = panel.visible[index];
    if (!entry) return;
    if (panel.selected.has(entry.token)) panel.selected.delete(entry.token);
    else panel.selected.add(entry.token);
    panel.anchor = entry.token;
    if (advance && index < panel.visible.length - 1) panel.cursor = panel.visible[index + 1].token;
    else panel.cursor = entry.token;
    this.updateSelectionUI(this.activePanel);
    this.cursorRow(this.activePanel)?.scrollIntoView({ block: "nearest" });
  }

  private typeToJump(key: string): void {
    if (this.typeaheadTimer !== null) window.clearTimeout(this.typeaheadTimer);
    this.typeahead += key.toLocaleLowerCase();
    const panel = this.panels[this.activePanel];
    const entry = panel.visible.find((item) => item.name.toLocaleLowerCase().startsWith(this.typeahead));
    if (entry) this.setCursor(panel.visible.indexOf(entry), false);
    this.typeaheadTimer = window.setTimeout(() => {
      this.typeahead = "";
      this.typeaheadTimer = null;
    }, 700);
  }

  private runCommand(command: string): void {
    if (command === "preview") void this.previewSelected();
    else if (command === "copy") void this.transferToOther("copy");
    else if (command === "move") void this.transferToOther("move");
    else if (command === "mkdir") void this.createFolder();
    else if (command === "trash") void this.deleteSelected(false);
  }

  private selectedEntries(index: number): FileEntry[] {
    const panel = this.panels[index];
    return panel.visible.filter((entry) => panel.selected.has(entry.token));
  }

  private actionableEntries(index: number): FileEntry[] {
    const selected = this.selectedEntries(index);
    if (selected.length) return selected;
    const cursor = this.cursorEntry(index);
    return cursor ? [cursor] : [];
  }

  private cursorEntry(index: number): FileEntry | undefined {
    const panel = this.panels[index];
    return panel.visible.find((entry) => entry.token === panel.cursor);
  }

  private storeClipboard(kind: FileOperationKind): void {
    const entries = this.actionableEntries(this.activePanel);
    if (!entries.length) return;
    fileClipboard = { kind, tokens: entries.map((entry) => entry.token) };
    this.toast(`${kind === "copy" ? "Copied" : "Cut"} ${entries.length} item${entries.length === 1 ? "" : "s"}`, "info");
  }

  private async pasteClipboard(): Promise<void> {
    const destination = this.panels[this.activePanel].location;
    if (!fileClipboard || !destination) return;
    await this.transfer(fileClipboard.kind, fileClipboard.tokens, destination.token);
    if (fileClipboard.kind === "move") fileClipboard = null;
  }

  private async transferToOther(kind: FileOperationKind): Promise<void> {
    const entries = this.actionableEntries(this.activePanel);
    const destination = this.panels[this.activePanel === 0 ? 1 : 0].location;
    if (!entries.length) {
      this.toast("Select at least one file or folder", "warn");
      return;
    }
    if (!destination) return;
    await this.transfer(kind, entries.map((entry) => entry.token), destination.token);
  }

  private async transfer(
    kind: FileOperationKind,
    tokens: string[],
    destination: string
  ): Promise<void> {
    if (this.operationId) {
      this.toast("Wait for the current file operation to finish", "warn");
      return;
    }
    let policy: ConflictPolicy = "error";
    try {
      const conflicts = await api.filesConflicts(tokens, destination);
      if (conflicts.length) {
        if (kind === "move" && conflicts.some((conflict) => conflict.sameSource)) {
          this.toast("The selected item is already in the destination folder", "warn");
          return;
        }
        const choice = await this.askConflict(
          conflicts.map((conflict) => conflict.name),
          !conflicts.some((conflict) => conflict.sameSource)
        );
        if (!choice) return;
        policy = choice;
      }
      const id = uuid();
      this.operationId = id;
      this.showOperation(kind, tokens.length);
      this.setStatus("connecting");
      const result = await api.filesOperate(
        { id, kind, sources: tokens, destination, conflictPolicy: policy },
        (event) => this.onOperationEvent(event)
      );
      const skipped = result.skippedItems ? ` · ${result.skippedItems} skipped` : "";
      this.toast(`${kind === "copy" ? "Copy" : "Move"} complete${skipped}`, "info");
    } catch (error) {
      const message = String(error);
      if (/cancelled/i.test(message)) this.toast("File operation cancelled", "warn");
      else this.toast(`${kind === "copy" ? "Copy" : "Move"} failed: ${message}`, "error");
    } finally {
      this.operationId = null;
      if (!this.disposed) {
        this.hideOperation();
        this.setStatus("connected");
        await Promise.all([this.refreshPanel(0), this.refreshPanel(1)]);
        this.focus();
      }
    }
  }

  private onOperationEvent(event: FileOperationEvent): void {
    if (event.id !== this.operationId) return;
    const operation = this.element.querySelector<HTMLElement>(".fm-operation")!;
    const title = operation.querySelector<HTMLElement>(".fm-operation__title")!;
    const detail = operation.querySelector<HTMLElement>(".fm-operation__detail")!;
    const progress = operation.querySelector<HTMLElement>(".fm-progress span")!;
    if (event.event === "started") {
      detail.textContent = "Scanning files…";
      progress.style.width = "4%";
    } else if (event.event === "planned") {
      detail.textContent = `${event.totalItems} item${event.totalItems === 1 ? "" : "s"} · ${formatFileSize(event.totalBytes)}`;
    } else if (event.event === "progress") {
      title.textContent = event.current;
      const ratio = event.totalBytes
        ? event.completedBytes / event.totalBytes
        : event.totalItems
          ? event.completedItems / event.totalItems
          : 0;
      progress.style.width = `${Math.max(4, Math.min(100, ratio * 100))}%`;
      detail.textContent = `${event.completedItems} / ${event.totalItems} · ${formatFileSize(event.completedBytes)} / ${formatFileSize(event.totalBytes)}`;
    } else if (event.event === "finished") {
      progress.style.width = "100%";
      detail.textContent = `${event.completedItems} completed`;
    } else if (event.event === "failed") {
      detail.textContent = event.message;
    }
  }

  private showOperation(kind: FileOperationKind, count: number): void {
    const operation = this.element.querySelector<HTMLElement>(".fm-operation")!;
    operation.hidden = false;
    operation.querySelector<HTMLElement>(".fm-operation__title")!.textContent =
      `${kind === "copy" ? "Copying" : "Moving"} ${count} item${count === 1 ? "" : "s"}`;
    operation.querySelector<HTMLElement>(".fm-operation__detail")!.textContent = "Preparing…";
    operation.querySelector<HTMLElement>(".fm-progress span")!.style.width = "0%";
  }

  private hideOperation(): void {
    this.element.querySelector<HTMLElement>(".fm-operation")!.hidden = true;
  }

  private async createFolder(): Promise<void> {
    const panel = this.panels[this.activePanel];
    if (!panel.location) return;
    const name = await this.askName("New folder", "", "Create folder");
    if (!name) return;
    try {
      const created = await api.filesCreateDirectory(panel.location.token, name);
      await this.refreshPanel(this.activePanel);
      panel.selected = new Set([created.token]);
      panel.cursor = created.token;
      panel.anchor = created.token;
      this.updateSelectionUI(this.activePanel);
    } catch (error) {
      this.toast(`Couldn't create folder: ${error}`, "error");
    }
  }

  private async renameSelected(): Promise<void> {
    const entries = this.actionableEntries(this.activePanel);
    if (entries.length !== 1) {
      this.toast("Select one file or folder to rename", "warn");
      return;
    }
    const entry = entries[0];
    const name = await this.askName("Rename", entry.name, "Rename");
    if (!name || name === entry.name) return;
    try {
      const renamed = await api.filesRename(entry.token, name);
      await this.refreshPanel(this.activePanel);
      const panel = this.panels[this.activePanel];
      panel.selected = new Set([renamed.token]);
      panel.cursor = renamed.token;
      panel.anchor = renamed.token;
      this.updateSelectionUI(this.activePanel);
    } catch (error) {
      this.toast(`Couldn't rename ${entry.name}: ${error}`, "error");
    }
  }

  private async deleteSelected(permanent: boolean): Promise<void> {
    const entries = this.actionableEntries(this.activePanel);
    if (!entries.length) {
      this.toast("Select at least one file or folder", "warn");
      return;
    }
    const count = entries.length;
    const confirmed = await this.askConfirm(
      permanent ? "Delete permanently?" : "Move to Trash?",
      permanent
        ? `${count} selected item${count === 1 ? "" : "s"} will be permanently deleted. This cannot be undone.`
        : `${count} selected item${count === 1 ? "" : "s"} will be moved to the operating system Trash or Recycle Bin.`,
      permanent ? "Delete permanently" : "Move to Trash",
      true
    );
    if (!confirmed) return;
    try {
      const tokens = entries.map((entry) => entry.token);
      if (permanent) await api.filesDelete(tokens);
      else await api.filesTrash(tokens);
      await Promise.all([this.refreshPanel(0), this.refreshPanel(1)]);
      this.toast(`${count} item${count === 1 ? "" : "s"} ${permanent ? "deleted" : "moved to Trash"}`, "info");
    } catch (error) {
      this.toast(`${permanent ? "Delete" : "Trash"} failed: ${error}`, "error");
    }
  }

  private async previewSelected(): Promise<void> {
    const entries = this.actionableEntries(this.activePanel);
    if (entries.length !== 1 || entries[0].isDirectory) {
      this.toast("Select one file to preview", "warn");
      return;
    }
    const entry = entries[0];
    try {
      const preview = await api.filesPreview(entry.token);
      const backdrop = this.createModal("fm-preview-backdrop");
      const modal = document.createElement("div");
      modal.className = "modal modal--wide fm-preview-dialog";
      modal.setAttribute("role", "dialog");
      modal.setAttribute("aria-modal", "true");
      modal.innerHTML = `
        <div class="modal__head">
          <div><div class="modal__title"></div><div class="fm-preview__path"></div></div>
          <button class="modal__close" type="button" title="Close">${icon("close")}</button>
        </div>
        <div class="fm-preview__body"></div>
        <div class="fm-preview__foot">${formatFileSize(preview.size)}${preview.truncated ? " · Preview truncated" : ""}</div>`;
      modal.querySelector<HTMLElement>(".modal__title")!.textContent = preview.name;
      modal.querySelector<HTMLElement>(".fm-preview__path")!.textContent = preview.path;
      const body = modal.querySelector<HTMLElement>(".fm-preview__body")!;
      if (preview.kind === "image" && preview.mime && preview.content) {
        const image = document.createElement("img");
        image.src = `data:${preview.mime};base64,${preview.content}`;
        image.alt = preview.name;
        body.appendChild(image);
      } else if (preview.kind === "text" && preview.content !== null) {
        const pre = document.createElement("pre");
        pre.textContent = preview.content;
        body.appendChild(pre);
      } else {
        body.classList.add("fm-preview__unavailable");
        body.innerHTML = `${icon("file")}<span></span>`;
        body.querySelector("span")!.textContent = preview.message ?? "Preview is not available";
      }
      backdrop.appendChild(modal);
      this.wireModalDismiss(backdrop);
      modal.querySelector<HTMLButtonElement>(".modal__close")!.focus();
    } catch (error) {
      this.toast(`Couldn't preview ${entry.name}: ${error}`, "error");
    }
  }

  private openPanelMenu(index: number, event: MouseEvent): void {
    event.preventDefault();
    this.activatePanel(index);
    const row = (event.target as HTMLElement).closest<HTMLElement>(".fm-row");
    if (row?.dataset.token && !this.panels[index].selected.has(row.dataset.token)) {
      this.panels[index].selected = new Set([row.dataset.token]);
      this.panels[index].cursor = row.dataset.token;
      this.panels[index].anchor = row.dataset.token;
      this.updateSelectionUI(index);
    }
    const entries = this.actionableEntries(index);
    const one = entries.length === 1 ? entries[0] : null;
    const items: MenuItem[] = [];
    if (one) {
      items.push({
        label: one.isDirectory ? "Open folder" : "Open",
        icon: one.isDirectory ? "folder" : "file",
        action: () => void this.openEntry(index, one),
      });
      if (!one.isDirectory) {
        items.push({ label: "Preview", icon: "eye", action: () => void this.previewSelected() });
      }
    }
    if (entries.length) {
      items.push(
        "sep",
        { label: "Copy to other panel", icon: "copy", action: () => void this.transferToOther("copy") },
        { label: "Move to other panel", icon: "move", action: () => void this.transferToOther("move") },
        { label: "Copy", icon: "copy", action: () => this.storeClipboard("copy") },
        { label: "Cut", icon: "move", action: () => this.storeClipboard("move") }
      );
      if (one) items.push({ label: "Rename…", icon: "pencil", action: () => void this.renameSelected() });
      items.push(
        "sep",
        {
          label: "Move to Trash",
          icon: "trash",
          danger: true,
          action: () => void this.deleteSelected(false),
        },
        {
          label: "Delete permanently…",
          icon: "trash",
          danger: true,
          action: () => void this.deleteSelected(true),
        }
      );
    }
    if (items.length) items.push("sep");
    if (fileClipboard) {
      items.push({ label: "Paste", icon: "copy", action: () => void this.pasteClipboard() });
    }
    items.push(
      { label: "New folder…", icon: "folderPlus", action: () => void this.createFolder() },
      { label: "Refresh", icon: "refresh", action: () => void this.refreshPanel(index) }
    );
    contextMenu(event.clientX, event.clientY, items);
  }

  private onDragStart(index: number, entry: FileEntry, event: DragEvent): void {
    this.activatePanel(index);
    const panel = this.panels[index];
    if (!panel.selected.has(entry.token)) {
      panel.selected = new Set([entry.token]);
      panel.cursor = entry.token;
      panel.anchor = entry.token;
      this.updateSelectionUI(index);
    }
    const payload: DragPayload = { sourcePanel: index, tokens: [...panel.selected] };
    event.dataTransfer?.setData(DRAG_TYPE, JSON.stringify(payload));
    event.dataTransfer?.setData("text/plain", entry.path);
    if (event.dataTransfer) event.dataTransfer.effectAllowed = "copyMove";
    requestAnimationFrame(() => {
      this.panelElement(index)?.classList.add("is-drag-source");
    });
  }

  private onDragOver(index: number, event: DragEvent): void {
    if (!event.dataTransfer?.types.includes(DRAG_TYPE)) return;
    event.preventDefault();
    event.stopPropagation();
    event.dataTransfer.dropEffect = event.shiftKey ? "move" : "copy";
    this.clearDropTargets();
    const row = (event.target as HTMLElement).closest<HTMLElement>(".fm-row");
    const entry = row?.dataset.token ? this.entryByToken(index, row.dataset.token) : undefined;
    (entry?.isDirectory ? row : this.panelElement(index))?.classList.add("is-drop-target");
  }

  private onDragLeave(event: DragEvent): void {
    const related = event.relatedTarget as Node | null;
    const current = event.currentTarget as HTMLElement;
    if (!related || !current.contains(related)) this.clearDropTargets();
  }

  private async onDrop(index: number, event: DragEvent): Promise<void> {
    const encoded = event.dataTransfer?.getData(DRAG_TYPE);
    if (!encoded) return;
    event.preventDefault();
    event.stopPropagation();
    this.clearDropTargets();
    this.activatePanel(index);
    try {
      const payload = JSON.parse(encoded) as DragPayload;
      const row = (event.target as HTMLElement).closest<HTMLElement>(".fm-row");
      const entry = row?.dataset.token ? this.entryByToken(index, row.dataset.token) : undefined;
      const destination = entry?.isDirectory ? entry.token : this.panels[index].location?.token;
      if (!destination || !Array.isArray(payload.tokens)) return;
      await this.transfer(event.shiftKey ? "move" : "copy", payload.tokens, destination);
    } catch {
      this.toast("That drag operation could not be read", "error");
    }
  }

  private clearDropTargets(): void {
    this.element
      .querySelectorAll(".is-drop-target, .is-drag-source")
      .forEach((element) => element.classList.remove("is-drop-target", "is-drag-source"));
  }

  private entryByToken(index: number, token: string): FileEntry | undefined {
    return this.panels[index].entries.find((entry) => entry.token === token);
  }

  private async askName(title: string, value: string, action: string): Promise<string | null> {
    const backdrop = this.createModal();
    const modal = document.createElement("div");
    modal.className = "modal fm-prompt";
    modal.setAttribute("role", "dialog");
    modal.setAttribute("aria-modal", "true");
    modal.innerHTML = `
      <div class="modal__head"><div class="modal__title"></div><button class="modal__close" data-value="cancel" title="Close">${icon("close")}</button></div>
      <div class="modal__body"><label class="field"><span class="field__label">Name</span><input autocomplete="off" spellcheck="false" /></label></div>
      <div class="modal__foot"><span class="spacer"></span><button class="btn ghost" data-value="cancel">Cancel</button><button class="btn primary" data-value="accept"></button></div>`;
    modal.querySelector<HTMLElement>(".modal__title")!.textContent = title;
    modal.querySelector<HTMLButtonElement>("[data-value=accept]")!.textContent = action;
    const input = modal.querySelector<HTMLInputElement>("input")!;
    input.value = value;
    backdrop.appendChild(modal);
    return new Promise((resolve) => {
      const finish = (result: string | null) => {
        this.closeModal();
        resolve(result);
      };
      modal.querySelectorAll<HTMLButtonElement>("[data-value=cancel]").forEach((button) =>
        button.addEventListener("click", () => finish(null))
      );
      modal.querySelector<HTMLButtonElement>("[data-value=accept]")!.addEventListener("click", () =>
        finish(input.value.trim() || null)
      );
      backdrop.addEventListener("mousedown", (event) => {
        if (event.target === backdrop) finish(null);
      });
      backdrop.addEventListener("keydown", (event) => {
        if (event.key === "Escape") finish(null);
        else if (event.key === "Enter") finish(input.value.trim() || null);
      });
      setTimeout(() => {
        input.focus();
        input.select();
      });
    });
  }

  private async askConfirm(
    title: string,
    message: string,
    action: string,
    danger = false
  ): Promise<boolean> {
    const backdrop = this.createModal();
    const modal = document.createElement("div");
    modal.className = "modal fm-confirm";
    modal.setAttribute("role", "alertdialog");
    modal.setAttribute("aria-modal", "true");
    modal.innerHTML = `
      <div class="modal__head"><div class="modal__title"></div><button class="modal__close" data-value="cancel" title="Close">${icon("close")}</button></div>
      <div class="modal__body"><p class="fm-confirm__message"></p></div>
      <div class="modal__foot"><span class="spacer"></span><button class="btn ghost" data-value="cancel">Cancel</button><button class="btn ${danger ? "danger" : "primary"}" data-value="accept"></button></div>`;
    modal.querySelector<HTMLElement>(".modal__title")!.textContent = title;
    modal.querySelector<HTMLElement>(".fm-confirm__message")!.textContent = message;
    modal.querySelector<HTMLButtonElement>("[data-value=accept]")!.textContent = action;
    backdrop.appendChild(modal);
    return new Promise((resolve) => {
      const finish = (result: boolean) => {
        this.closeModal();
        resolve(result);
      };
      modal.querySelectorAll<HTMLButtonElement>("[data-value=cancel]").forEach((button) =>
        button.addEventListener("click", () => finish(false))
      );
      modal.querySelector<HTMLButtonElement>("[data-value=accept]")!.addEventListener("click", () =>
        finish(true)
      );
      backdrop.addEventListener("mousedown", (event) => {
        if (event.target === backdrop) finish(false);
      });
      backdrop.addEventListener("keydown", (event) => {
        if (event.key === "Escape") finish(false);
      });
      modal.querySelector<HTMLButtonElement>("[data-value=cancel]")!.focus();
    });
  }

  private async askConflict(
    names: string[],
    allowReplace: boolean
  ): Promise<ConflictPolicy | null> {
    const backdrop = this.createModal();
    const modal = document.createElement("div");
    modal.className = "modal fm-confirm";
    modal.setAttribute("role", "dialog");
    modal.setAttribute("aria-modal", "true");
    modal.innerHTML = `
      <div class="modal__head"><div class="modal__title">Files already exist</div><button class="modal__close" data-value="cancel" title="Close">${icon("close")}</button></div>
      <div class="modal__body"><p class="fm-confirm__message"></p><div class="fm-conflicts"></div><p class="fm-confirm__note">Apply this choice to every conflict in the operation.</p></div>
      <div class="modal__foot fm-conflict-actions"><button class="btn ghost" data-value="cancel">Cancel</button><span class="spacer"></span><button class="btn ghost" data-value="skip">Skip</button><button class="btn ghost" data-value="keep_both">Keep both</button>${allowReplace ? '<button class="btn primary" data-value="replace">Replace</button>' : ""}</div>`;
    modal.querySelector<HTMLElement>(".fm-confirm__message")!.textContent =
      `${names.length} destination item${names.length === 1 ? " has" : "s have"} the same name.`;
    if (!allowReplace) {
      modal.querySelector<HTMLElement>(".fm-confirm__note")!.textContent =
        "The source and destination are the same. Keep a renamed copy or skip the item.";
    }
    const list = modal.querySelector<HTMLElement>(".fm-conflicts")!;
    names.slice(0, 5).forEach((name) => {
      const item = document.createElement("div");
      item.textContent = name;
      list.appendChild(item);
    });
    if (names.length > 5) {
      const more = document.createElement("div");
      more.textContent = `…and ${names.length - 5} more`;
      list.appendChild(more);
    }
    backdrop.appendChild(modal);
    return new Promise((resolve) => {
      const finish = (result: ConflictPolicy | null) => {
        this.closeModal();
        resolve(result);
      };
      modal.querySelectorAll<HTMLButtonElement>("[data-value]").forEach((button) => {
        button.addEventListener("click", () => {
          const value = button.dataset.value;
          finish(value === "replace" || value === "skip" || value === "keep_both" ? value : null);
        });
      });
      backdrop.addEventListener("mousedown", (event) => {
        if (event.target === backdrop) finish(null);
      });
      backdrop.addEventListener("keydown", (event) => {
        if (event.key === "Escape") finish(null);
      });
      modal.querySelector<HTMLButtonElement>("[data-value=keep_both]")!.focus();
    });
  }

  private createModal(extraClass = ""): HTMLElement {
    this.closeModal();
    const backdrop = document.createElement("div");
    backdrop.className = `modal-backdrop ${extraClass}`;
    document.body.appendChild(backdrop);
    this.modalEl = backdrop;
    return backdrop;
  }

  private wireModalDismiss(backdrop: HTMLElement): void {
    const close = () => this.closeModal();
    backdrop.querySelector(".modal__close")?.addEventListener("click", close);
    backdrop.addEventListener("mousedown", (event) => {
      if (event.target === backdrop) close();
    });
    backdrop.addEventListener("keydown", (event) => {
      if (event.key === "Escape") close();
    });
  }

  private closeModal(): void {
    this.modalEl?.remove();
    this.modalEl = null;
  }

  private setStatus(status: TabStatus): void {
    this.status = status;
    this.onStatusChange?.();
  }

  private panelElement(index: number): HTMLElement | null {
    return this.element.querySelector<HTMLElement>(`.fm-panel[data-panel="${index}"]`);
  }

  private listElement(index: number): HTMLElement | null {
    return this.panelElement(index)?.querySelector<HTMLElement>(".fm-list") ?? null;
  }

  private gridElement(index: number): HTMLElement | null {
    return this.panelElement(index)?.querySelector<HTMLElement>(".fm-grid") ?? null;
  }

  private cursorRow(index: number): HTMLElement | null {
    return (
      Array.from(this.listElement(index)?.querySelectorAll<HTMLElement>(".fm-row") ?? []).find(
        (row) => row.dataset.token === this.panels[index].cursor
      ) ?? null
    );
  }
}

function newPanel(): PanelState {
  return {
    location: null,
    parent: null,
    entries: [],
    visible: [],
    selected: new Set(),
    cursor: null,
    anchor: null,
    history: [],
    historyIndex: -1,
    sort: { key: "name", direction: "asc" },
    showHidden: false,
    query: "",
    loadSequence: 0,
  };
}

function toolButton(action: string, title: string, iconName: string): string {
  return `<button type="button" class="fm-tool" data-act="${action}" title="${title}">${icon(iconName)}</button>`;
}

function buttonFor(root: HTMLElement, action: string): HTMLButtonElement {
  return root.querySelector<HTMLButtonElement>(`.fm-tool[data-act="${action}"]`)!;
}

function entryIcon(entry: FileEntry): string {
  if (entry.kind === "symlink") return "link";
  if (entry.isDirectory) return "folder";
  if (/\.(png|jpe?g|gif|webp|bmp)$/i.test(entry.name)) return "image";
  return "file";
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

function escapeText(value: string): string {
  return value.replace(/[&<>"']/g, (character) => {
    const escaped: Record<string, string> = {
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    };
    return escaped[character];
  });
}

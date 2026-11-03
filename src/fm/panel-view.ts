// One file panel on screen: toolbar, sortable header, a virtualized row list
// and a status line. State lives in PanelModel; this class draws it and turns
// mouse input into model calls. Only the rows in view (plus a little overscan)
// exist in the DOM, so a 20k-entry folder costs the same as a small one.

import { icon } from "../icons";
import { fsApi, type FsCapabilities, type FsEntry, type FsLocationView } from "../ipc";
import { formatFileSize, formatMode, formatModified, modeType } from "./format";
import { isRemote, providerBadge, providerLabel } from "./remote";
import { History, PanelModel, type SortKey } from "./panel-model";

export const ROW_HEIGHT = 28;
const OVERSCAN = 8;
const QUICK_SEARCH_RESET_MS = 1200;

/** What a panel needs from its tab. */
export interface PanelHost {
  activate(panel: PanelView): void;
  /** Enter / double-click on a row. */
  openRow(panel: PanelView): void;
  contextMenu(panel: PanelView, x: number, y: number): void;
  locationMenu(panel: PanelView, anchor: HTMLElement): void;
  dragStart(panel: PanelView, event: DragEvent): void;
  /** Something was dropped on this panel (onto `folder`, or its current folder). */
  drop(panel: PanelView, folder: FsEntry | null, event: DragEvent): void;
  /** The cursor, marks or location changed (status bar, key bar). */
  changed(panel: PanelView): void;
  toast(message: string, kind?: "info" | "warn" | "error"): void;
  /** The panel's remote filesystem dropped; the user asked to reconnect. */
  reconnect(panel: PanelView): void;
}

export interface NavigateOptions {
  focusName?: string;
  /** Put the cursor on the entry with this path token (preferred over the name). */
  focusPath?: string;
  /** "push" records history (default), "none" is for back/forward/refresh. */
  history?: "push" | "none";
  /** Refresh: keep cursor, marks and scroll position. */
  keepState?: boolean;
  /** Fail silently (e.g. a remembered folder that no longer exists). */
  quiet?: boolean;
}

export class PanelView {
  readonly element: HTMLElement;
  readonly model = new PanelModel();
  readonly history = new History();
  capabilities: FsCapabilities = {
    trash: false,
    openNative: false,
    posixPermissions: false,
    symlinks: false,
  };

  private readonly list: HTMLElement;
  private readonly spacer: HTMLElement;
  private readonly grid: HTMLElement;
  private readonly pathInput: HTMLInputElement;
  private readonly filterBar: HTMLElement;
  private readonly filterInput: HTMLInputElement;
  private readonly status: HTMLElement;
  private readonly quick: HTMLElement;
  /** Rendered rows by index, and the rows array they were drawn from. */
  private readonly rendered = new Map<number, HTMLElement>();
  private drawnRows: unknown = null;
  private loadSequence = 0;
  /** A navigation the user started is in flight (a refresh must not cancel it). */
  private navigating = false;
  private quickText = "";
  private quickTimer: number | null = null;

  constructor(
    private readonly host: PanelHost,
    readonly side: 0 | 1,
    private readonly uid: string
  ) {
    this.element = document.createElement("section");
    this.element.className = "fm-panel";
    this.element.dataset.panel = String(side);
    this.element.setAttribute("aria-label", side === 0 ? "Left file panel" : "Right file panel");
    this.element.innerHTML = `
      <div class="fm-toolbar">
        <div class="fm-nav">
          ${tool("back", "Back (Alt+Left)", "back")}
          ${tool("forward", "Forward (Alt+Right)", "forward")}
          ${tool("up", "Parent folder (Backspace)", "up")}
          ${tool("home", "Home folder", "home")}
        </div>
        <span class="fm-provider"></span>
        <input class="fm-path" aria-label="Folder path" spellcheck="false" autocomplete="off" />
        ${tool("roots", `Locations (Alt+F${side + 1})`, "server")}
        ${tool("filter", "Filter (Ctrl+F)", "search")}
        ${tool("hidden", "Show hidden files", "eye")}
        ${tool("refresh", "Refresh (Ctrl+R)", "refresh")}
      </div>
      <div class="fm-filter" hidden>
        ${icon("search")}
        <input aria-label="Filter files" placeholder="Filter this folder…" spellcheck="false" />
        <button type="button" data-act="close-filter" title="Clear filter (Esc)">${icon("close")}</button>
      </div>
      <div class="fm-grid" role="grid" aria-multiselectable="true" tabindex="0">
        <div class="fm-header" role="row">
          <span role="columnheader" class="fm-col-name"><button type="button" data-sort="name">Name <span></span></button></span>
          <span role="columnheader" class="fm-col-size"><button type="button" data-sort="size">Size <span></span></button></span>
          <span role="columnheader" class="fm-col-modified"><button type="button" data-sort="modified">Modified <span></span></button></span>
          <span role="columnheader" class="fm-col-perms">Rights</span>
          <span role="columnheader" class="fm-col-owner">Owner</span>
        </div>
        <div class="fm-list" role="rowgroup"><div class="fm-spacer"></div></div>
        <div class="fm-quicksearch" hidden></div>
        <div class="fm-offline" role="alert" hidden>
          <span class="fm-offline__icon">${icon("server")}</span>
          <div class="fm-offline__text"><strong>Disconnected</strong><span></span></div>
          <button type="button" class="btn primary" data-act="reconnect">Reconnect</button>
        </div>
      </div>
      <div class="fm-panel-status"></div>`;

    this.list = this.element.querySelector(".fm-list")!;
    this.spacer = this.element.querySelector(".fm-spacer")!;
    this.grid = this.element.querySelector(".fm-grid")!;
    this.pathInput = this.element.querySelector(".fm-path")!;
    this.filterBar = this.element.querySelector(".fm-filter")!;
    this.filterInput = this.filterBar.querySelector("input")!;
    this.status = this.element.querySelector(".fm-panel-status")!;
    this.quick = this.element.querySelector(".fm-quicksearch")!;
    this.wire();
    this.element
      .querySelector(".fm-offline button")!
      .addEventListener("click", () => this.host.reconnect(this));
  }

  get location(): FsLocationView | null {
    return this.model.location;
  }

  /** The provider badge: `LOCAL`, or the server with a connection dot. */
  private renderProvider(): void {
    const badge = this.element.querySelector<HTMLElement>(".fm-provider")!;
    const remote = isRemote(this.fs);
    badge.classList.toggle("is-remote", remote);
    badge.classList.toggle("is-offline", remote && this.offline);
    badge.textContent = providerBadge(this.fs);
    badge.title = remote
      ? `${providerLabel(this.fs)} — ${this.offline ? "disconnected" : "connected"}`
      : "This PC";
  }

  /** Show (with why) or hide the "Disconnected — Reconnect" banner. */
  setOffline(message: string | null): void {
    const banner = this.element.querySelector<HTMLElement>(".fm-offline")!;
    const wasHidden = banner.hidden;
    banner.hidden = message === null;
    this.element.classList.toggle("is-offline", message !== null);
    banner.querySelector(".fm-offline__text span")!.textContent = message ?? "";
    this.renderProvider();
    if (message !== null && wasHidden && this.element.classList.contains("is-active")) {
      // Keyboard users reach the button at once; Enter reconnects.
      banner.querySelector<HTMLButtonElement>("button")!.focus({ preventScroll: true });
    }
  }

  get offline(): boolean {
    return this.element.classList.contains("is-offline");
  }

  get fs(): string {
    return this.model.location?.fs ?? "local";
  }

  focus(): void {
    this.grid.focus({ preventScroll: true });
  }

  setActive(active: boolean): void {
    this.element.classList.toggle("is-active", active);
  }

  /** List `location` and show it. Resolves false (after a toast) on failure. */
  async navigate(location: FsLocationView, options: NavigateOptions = {}): Promise<boolean> {
    const sequence = ++this.loadSequence;
    this.navigating = !options.keepState;
    this.element.classList.add("is-loading");
    try {
      const [listing, capabilities] = await Promise.all([
        fsApi.list(location),
        location.fs === this.model.location?.fs
          ? Promise.resolve(this.capabilities)
          : fsApi.capabilities(location.fs),
      ]);
      if (sequence !== this.loadSequence) return false;
      const scrollTop = this.list.scrollTop;
      this.capabilities = capabilities;
      const moved = listing.location.path !== this.model.location?.path;
      if (moved && this.model.filter) {
        // A filter belongs to the folder it was typed in.
        this.model.filter = "";
        this.filterBar.hidden = true;
        this.filterInput.value = "";
      }
      this.model.applyListing(listing, {
        keepState: options.keepState,
        focusName: options.focusName,
        focusPath: options.focusPath,
      });
      if ((options.history ?? "push") === "push") this.history.visit(listing.location);
      this.render();
      if (options.keepState) this.list.scrollTop = scrollTop;
      else this.list.scrollTop = 0;
      this.ensureCursorVisible();
      this.host.changed(this);
      return true;
    } catch (error) {
      if (sequence === this.loadSequence && !options.quiet) {
        this.host.toast(`Couldn't open folder: ${error}`, "error");
      }
      return false;
    } finally {
      if (sequence === this.loadSequence) {
        this.navigating = false;
        this.element.classList.remove("is-loading");
      }
    }
  }

  /** Re-list the folder shown, keeping cursor, marks and scroll. Skipped while
   *  the user is navigating elsewhere (the new folder is fresh anyway). */
  refresh(): Promise<boolean> {
    const location = this.model.location;
    if (!location || this.navigating) return Promise.resolve(false);
    return this.navigate(location, { keepState: true, history: "none" });
  }

  /** Up one level, landing on the folder we came from. */
  async goUp(): Promise<void> {
    const { parent, location } = this.model;
    if (!parent || !location) return;
    // The path token finds the folder; the name is a fallback. POSIX names
    // may contain `\`, so only Windows-style paths split on it.
    const windows = /^([A-Za-z]:)?\\/.test(location.display);
    const separators = windows ? /[\\/]/ : /\//;
    const from = location.display.replace(/[\\/]+$/, "").split(separators).pop();
    await this.navigate(parent, { focusPath: location.path, focusName: from });
  }

  async goHistory(step: -1 | 1): Promise<void> {
    const target = this.history.peek(step);
    if (target && (await this.navigate(target, { history: "none" }))) {
      this.history.go(step);
      this.updateToolbar();
    }
  }

  /** Full redraw after the rows changed (listing, sort, filter). */
  render(): void {
    const { location } = this.model;
    this.pathInput.value = location?.display ?? "";
    this.pathInput.title = location?.display ?? "";
    this.renderProvider();
    this.element.classList.toggle("has-perms", this.capabilities.posixPermissions);
    // Owner names come with remote listings; local ones don't have them.
    this.element.classList.toggle(
      "has-owner",
      this.model.visibleEntries().some((entry) => entry.owner !== null)
    );
    this.updateToolbar();
    this.updateHeader();
    this.renderRows();
  }

  /** Draw the rows in view; reuses rows already drawn from the same data. */
  renderRows(): void {
    const { rows } = this.model;
    if (this.drawnRows !== rows) {
      for (const row of this.rendered.values()) row.remove();
      this.rendered.clear();
      this.drawnRows = rows;
      this.spacer.style.height = `${rows.length * ROW_HEIGHT}px`;
    }
    this.element.querySelector(".fm-empty")?.remove();
    const entryRows = rows.length - (this.model.parent ? 1 : 0);
    this.grid.setAttribute("aria-rowcount", String(rows.length + 1));
    if (!entryRows) {
      const empty = document.createElement("div");
      empty.className = "fm-empty";
      // Below the ".." row, which stays usable.
      empty.style.top = `${(rows.length - entryRows) * ROW_HEIGHT}px`;
      empty.innerHTML = `${icon(this.model.filter ? "search" : "folder")}<span></span>`;
      empty.querySelector("span")!.textContent = this.model.filter
        ? `No names contain “${this.model.filter}”`
        : "This folder is empty";
      this.list.appendChild(empty);
    }
    const top = this.list.scrollTop;
    const height = this.list.clientHeight || ROW_HEIGHT * 20;
    const first = Math.max(0, Math.floor(top / ROW_HEIGHT) - OVERSCAN);
    const last = Math.min(rows.length - 1, Math.ceil((top + height) / ROW_HEIGHT) + OVERSCAN);
    for (const [index, row] of this.rendered) {
      if (index < first || index > last) {
        row.remove();
        this.rendered.delete(index);
      }
    }
    for (let index = first; index <= last; index += 1) {
      if (!this.rendered.has(index)) {
        const row = this.createRow(index);
        this.rendered.set(index, row);
        this.spacer.appendChild(row);
      }
    }
    this.renderStates();
  }

  /** Cursor/mark classes on the drawn rows only, plus the status line. */
  renderStates(): void {
    const { cursor } = this.model;
    for (const [index, row] of this.rendered) {
      const marked = this.model.isMarked(index);
      row.classList.toggle("is-marked", marked);
      row.classList.toggle("is-cursor", index === cursor);
      row.setAttribute("aria-selected", String(marked));
      if (row.dataset.dirSize !== undefined) this.fillSize(row, index);
    }
    const cursorRow = this.rendered.get(cursor);
    if (cursorRow) this.grid.setAttribute("aria-activedescendant", cursorRow.id);
    else this.grid.removeAttribute("aria-activedescendant");
    this.renderStatus();
  }

  ensureCursorVisible(): void {
    // Rows start below the list's top padding and must clear the bottom one.
    const style = getComputedStyle(this.list);
    const padTop = parseFloat(style.paddingTop) || 0;
    const padBottom = parseFloat(style.paddingBottom) || 0;
    const top = this.model.cursor * ROW_HEIGHT;
    const view = this.list.clientHeight;
    if (top < this.list.scrollTop) this.list.scrollTop = top;
    else if (top + ROW_HEIGHT + padTop + padBottom > this.list.scrollTop + view) {
      this.list.scrollTop = top + ROW_HEIGHT + padTop + padBottom - view;
    }
    this.renderRows();
  }

  /** Rows that fit in the list, for PgUp/PgDn. */
  pageSize(): number {
    return Math.max(1, Math.floor(this.list.clientHeight / ROW_HEIGHT) - 1);
  }

  openFilter(): void {
    this.filterBar.hidden = false;
    this.filterInput.value = this.model.filter;
    this.filterInput.focus();
    this.filterInput.select();
  }

  closeFilter(): void {
    this.filterBar.hidden = true;
    this.filterInput.value = "";
    if (this.model.filter) {
      this.model.setFilter("");
      this.render();
      this.ensureCursorVisible();
      this.host.changed(this);
    }
    this.focus();
  }

  get filterOpen(): boolean {
    return !this.filterBar.hidden;
  }

  /** Type-to-jump: each key extends the prefix; a pause starts over. */
  quickSearch(char: string): void {
    if (this.quickTimer !== null) window.clearTimeout(this.quickTimer);
    const next = this.quickText + char;
    const index = this.model.findPrefix(next);
    if (index >= 0) {
      this.quickText = next;
      this.model.setCursor(index);
      this.ensureCursorVisible();
      this.host.changed(this);
    }
    this.quick.hidden = !this.quickText;
    this.quick.textContent = this.quickText;
    this.quickTimer = window.setTimeout(() => this.clearQuickSearch(), QUICK_SEARCH_RESET_MS);
  }

  get quickSearchActive(): boolean {
    return this.quickText !== "";
  }

  /** Backspace during a quick search: drop the last character. */
  quickSearchBackspace(): void {
    const text = this.quickText.slice(0, -1);
    this.clearQuickSearch();
    for (const char of text) this.quickSearch(char);
  }

  /** Returns whether there was a quick search to clear. */
  clearQuickSearch(): boolean {
    const had = this.quickText !== "";
    if (this.quickTimer !== null) window.clearTimeout(this.quickTimer);
    this.quickTimer = null;
    this.quickText = "";
    this.quick.hidden = true;
    return had;
  }

  /** The row element under a pointer event, as a row index. */
  rowIndexAt(target: EventTarget | null): number | null {
    const row = (target as HTMLElement | null)?.closest<HTMLElement>(".fm-row");
    return row?.dataset.index ? Number(row.dataset.index) : null;
  }

  setDropTarget(index: number | null | "panel"): void {
    this.element.classList.toggle("is-drop-target", index === "panel");
    for (const [rowIndex, row] of this.rendered) {
      row.classList.toggle("is-drop-target", rowIndex === index);
    }
  }

  dispose(): void {
    this.clearQuickSearch();
  }

  private createRow(index: number): HTMLElement {
    const row = document.createElement("div");
    row.className = "fm-row";
    row.id = `fm-${this.uid}-${this.side}-${index}`;
    row.role = "row";
    row.dataset.index = String(index);
    row.setAttribute("aria-rowindex", String(index + 2));
    row.style.top = `${index * ROW_HEIGHT}px`;
    const data = this.model.rows[index];
    if (data.kind === "parent") {
      row.classList.add("is-parent");
      row.innerHTML = `
        <span class="fm-cell fm-col-name" role="gridcell"><span class="fm-row__mark"></span><span class="fm-row__icon">${icon("up")}</span><span class="fm-row__name">..</span></span>
        <span class="fm-cell fm-col-size" role="gridcell"></span>
        <span class="fm-cell fm-col-modified" role="gridcell"></span>
        <span class="fm-cell fm-col-perms" role="gridcell"></span>
        <span class="fm-cell fm-col-owner" role="gridcell"></span>`;
      return row;
    }
    const entry = data.entry;
    row.draggable = true;
    row.title = entry.error ? `${entry.display}\n${entry.error}` : entry.display;
    row.classList.toggle("is-dir", entry.isDirectory);
    row.classList.toggle("is-hidden-entry", entry.hidden);
    row.innerHTML = `
      <span class="fm-cell fm-col-name" role="gridcell">
        <span class="fm-row__mark">${icon("check")}</span>
        <span class="fm-row__icon">${icon(entryIcon(entry))}</span>
        <span class="fm-row__name"></span>
        ${entry.kind === "symlink" ? `<span class="fm-row__badge">LINK</span>` : ""}
        ${entry.error ? `<span class="fm-row__badge">UNREADABLE</span>` : ""}
      </span>
      <span class="fm-cell fm-col-size" role="gridcell"></span>
      <span class="fm-cell fm-col-modified" role="gridcell">${formatModified(entry.modified)}</span>
      <span class="fm-cell fm-col-perms" role="gridcell">${formatMode(entry.mode, modeType(entry))}</span>
      <span class="fm-cell fm-col-owner" role="gridcell"></span>`;
    row.querySelector<HTMLElement>(".fm-row__name")!.textContent = entry.name;
    const owner = row.querySelector<HTMLElement>(".fm-col-owner")!;
    owner.textContent = entry.owner ?? "";
    if (entry.owner !== null || entry.group !== null) {
      owner.title = `${entry.owner ?? "?"}:${entry.group ?? "?"}`;
    }
    if (entry.isDirectory) row.dataset.dirSize = "";
    this.fillSize(row, index);
    return row;
  }

  private fillSize(row: HTMLElement, index: number): void {
    const data = this.model.rows[index];
    if (data?.kind !== "entry") return;
    const cell = row.querySelector<HTMLElement>(".fm-col-size")!;
    const { entry } = data;
    const known = entry.isDirectory ? this.model.folderSizes.get(entry.path) : entry.size;
    const text = known === undefined || known === null ? (entry.isDirectory ? "<DIR>" : "") : formatFileSize(known);
    if (cell.textContent !== text) cell.textContent = text;
  }

  private renderStatus(): void {
    const { model } = this;
    const { folders, files, bytes } = model.totals;
    const marked = model.markedSummary();
    this.status.innerHTML = "";
    const totals = document.createElement("span");
    totals.className = "fm-panel-status__count";
    totals.textContent = `${folders} ${folders === 1 ? "folder" : "folders"} · ${files} ${files === 1 ? "file" : "files"} · ${formatFileSize(bytes)}`;
    this.status.appendChild(totals);
    if (marked.count) {
      const selection = document.createElement("span");
      selection.className = "fm-panel-status__selection";
      selection.textContent = `${marked.count} marked · ${formatFileSize(marked.bytes)}`;
      this.status.appendChild(selection);
    }
    if (model.filter) {
      const filter = document.createElement("span");
      filter.className = "fm-panel-status__hint";
      filter.textContent = `filter: ${model.filter}`;
      this.status.appendChild(filter);
    }
  }

  private updateToolbar(): void {
    const button = (act: string) =>
      this.element.querySelector<HTMLButtonElement>(`.fm-tool[data-act="${act}"]`)!;
    button("back").disabled = !this.history.canGo(-1);
    button("forward").disabled = !this.history.canGo(1);
    button("up").disabled = !this.model.parent;
    button("hidden").classList.toggle("is-active", this.model.showHidden);
    button("hidden").title = this.model.showHidden ? "Hide hidden files" : "Show hidden files";
    button("filter").classList.toggle("is-active", this.model.filter !== "");
  }

  private updateHeader(): void {
    const { sort } = this.model;
    this.element.querySelectorAll<HTMLButtonElement>(".fm-header [data-sort]").forEach((button) => {
      const active = button.dataset.sort === sort.key;
      button.classList.toggle("is-sorted", active);
      button.querySelector("span")!.textContent = active ? (sort.direction === "asc" ? "↑" : "↓") : "";
      button.parentElement!.setAttribute(
        "aria-sort",
        active ? (sort.direction === "asc" ? "ascending" : "descending") : "none"
      );
    });
  }

  private wire(): void {
    this.element.addEventListener("pointerdown", (event) => {
      this.host.activate(this);
      // Clicks on non-focusable chrome (status line, badge, padding) would
      // leave keyboard focus on <body>, where no key reaches the panel.
      const target = event.target as Element;
      if (!target.closest("input, button, select, textarea, [tabindex]")) {
        requestAnimationFrame(() => this.focus());
      }
    });
    this.grid.addEventListener("focus", () => this.host.activate(this));
    this.list.addEventListener("scroll", () => this.renderRows(), { passive: true });
    // A resize (window, splitter, the transfer strip appearing) keeps the cursor in view.
    new ResizeObserver(() => this.ensureCursorVisible()).observe(this.list);

    this.element.querySelectorAll<HTMLButtonElement>(".fm-tool").forEach((button) => {
      button.addEventListener("click", (event) => {
        event.stopPropagation();
        this.host.activate(this);
        void this.toolbar(button.dataset.act ?? "", button);
      });
    });

    this.pathInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        void this.goToTyped(this.pathInput.value);
      } else if (event.key === "Escape") {
        event.preventDefault();
        this.pathInput.value = this.model.location?.display ?? "";
        this.focus();
      }
    });

    this.filterInput.addEventListener("input", () => {
      this.model.setFilter(this.filterInput.value);
      this.render();
      this.ensureCursorVisible();
      this.host.changed(this);
    });
    this.filterInput.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        this.closeFilter();
      } else if (event.key === "Enter" || event.key === "ArrowDown") {
        event.preventDefault();
        this.focus();
      }
    });
    this.filterBar
      .querySelector("[data-act=close-filter]")!
      .addEventListener("click", () => this.closeFilter());

    this.element.querySelectorAll<HTMLButtonElement>(".fm-header [data-sort]").forEach((button) => {
      button.addEventListener("click", () => {
        this.model.setSort(button.dataset.sort as SortKey);
        this.render();
        this.ensureCursorVisible();
      });
    });

    this.list.addEventListener("mousedown", (event) => {
      const index = this.rowIndexAt(event.target);
      if (index === null || event.button !== 0) return;
      if (event.shiftKey) {
        event.preventDefault(); // no text selection
        this.model.extendTo(index);
      } else if (event.ctrlKey || event.metaKey) {
        this.model.toggleRow(index);
      } else if (!this.model.isMarked(index)) {
        // A plain click on a marked row keeps the marks, so they can be dragged.
        this.model.clickRow(index);
      } else {
        this.model.setCursor(index);
      }
      this.renderStates();
      this.host.changed(this);
      this.focus();
    });
    this.list.addEventListener("click", (event) => {
      // Releasing a plain click on a marked row (no drag): select just that row.
      const index = this.rowIndexAt(event.target);
      if (index === null || event.shiftKey || event.ctrlKey || event.metaKey) return;
      if (this.model.isMarked(index) && this.model.marks.size > 1) {
        this.model.clickRow(index);
        this.renderStates();
        this.host.changed(this);
      }
    });
    this.list.addEventListener("dblclick", (event) => {
      const index = this.rowIndexAt(event.target);
      if (index === null) return;
      event.preventDefault();
      this.model.setCursor(index);
      this.host.openRow(this);
    });
    this.list.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      this.host.activate(this);
      const index = this.rowIndexAt(event.target);
      if (index !== null) {
        // The menu acts on what was right-clicked: move the cursor there, and
        // keep the marks only if that row is one of them.
        if (this.model.isMarked(index)) this.model.setCursor(index);
        else this.model.clickRow(index);
        this.renderStates();
      }
      this.host.contextMenu(this, event.clientX, event.clientY);
    });
    this.list.addEventListener("dragstart", (event) => this.host.dragStart(this, event));
    this.list.addEventListener("dragover", (event) => {
      if (!event.dataTransfer?.types.includes(DRAG_TYPE)) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = event.shiftKey ? "move" : "copy";
      const index = this.rowIndexAt(event.target);
      const folder = index !== null ? this.folderAt(index) : null;
      this.setDropTarget(folder ? index : "panel");
    });
    this.list.addEventListener("dragleave", (event) => {
      if (!this.list.contains(event.relatedTarget as Node | null)) this.setDropTarget(null);
    });
    this.list.addEventListener("drop", (event) => {
      if (!event.dataTransfer?.types.includes(DRAG_TYPE)) return;
      event.preventDefault();
      this.setDropTarget(null);
      const index = this.rowIndexAt(event.target);
      this.host.drop(this, index !== null ? this.folderAt(index) : null, event);
    });
  }

  /** The folder entry at a row, if it is one (drops onto files go to the folder shown). */
  folderAt(index: number): FsEntry | null {
    const row = this.model.rows[index];
    return row?.kind === "entry" && row.entry.isDirectory ? row.entry : null;
  }

  private async toolbar(action: string, button: HTMLElement): Promise<void> {
    const location = this.model.location;
    if (action === "back") await this.goHistory(-1);
    else if (action === "forward") await this.goHistory(1);
    else if (action === "up") await this.goUp();
    else if (action === "home") {
      try {
        await this.navigate(await fsApi.home(this.fs));
      } catch (error) {
        this.host.toast(`Couldn't find the home folder: ${error}`, "error");
      }
    } else if (action === "roots") this.host.locationMenu(this, button);
    else if (action === "filter") this.openFilter();
    else if (action === "hidden") {
      this.model.setShowHidden(!this.model.showHidden);
      this.render();
      this.ensureCursorVisible();
      this.host.changed(this);
    } else if (action === "refresh" && location) await this.refresh();
  }

  private async goToTyped(input: string): Promise<void> {
    try {
      const location = await fsApi.resolve(this.fs, input, this.model.location ?? undefined);
      if (await this.navigate(location)) this.focus();
    } catch (error) {
      this.host.toast(`Couldn't open ${input}: ${error}`, "error");
      this.pathInput.value = this.model.location?.display ?? "";
    }
  }
}

export const DRAG_TYPE = "application/x-corepty-files";

function tool(action: string, title: string, iconName: string): string {
  return `<button type="button" class="fm-tool" data-act="${action}" title="${title}">${icon(iconName)}</button>`;
}

function entryIcon(entry: FsEntry): string {
  if (entry.kind === "symlink" && !entry.isDirectory) return "link";
  if (entry.isDirectory) return "folder";
  if (/\.(png|jpe?g|gif|webp|bmp|svg|ico)$/i.test(entry.name)) return "image";
  return "file";
}

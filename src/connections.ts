// Connections sidebar: a folder/subfolder tree of saved SSH/Telnet sessions
// with context menus, drag-and-drop, and inline rename.
import { ask } from "@tauri-apps/plugin-dialog";
import { api, type Folder, type SavedSession } from "./ipc";
import { icon } from "./icons";
import { contextMenu } from "./menu";
import { escapeHtml as esc, uuid as genId } from "./util";

export interface TreeDeps {
  onConnect: (s: SavedSession) => void;
  onEdit: (s: SavedSession) => void;
  onNewConnection: (folderId: string | null) => void;
  toast: (msg: string, kind?: "info" | "warn" | "error") => void;
}

type DragItem = { type: "folder" | "session"; id: string };

const LS_COLLAPSED = "corepty.collapsedFolders";

export class ConnectionsTree {
  private folders: Folder[] = [];
  private sessions: SavedSession[] = [];
  private collapsed = new Set<string>();
  private drag: DragItem | null = null;

  constructor(private el: HTMLElement, private deps: TreeDeps) {
    try {
      this.collapsed = new Set(JSON.parse(localStorage.getItem(LS_COLLAPSED) ?? "[]"));
    } catch {
      /* ignore */
    }
  }

  async refresh(): Promise<void> {
    try {
      const [folders, sessions] = await Promise.all([api.foldersLoad(), api.sessionsLoad()]);
      this.folders = folders;
      this.sessions = sessions;
    } catch {
      this.folders = [];
      this.sessions = [];
    }
    this.render();
  }

  async newFolder(parentId: string | null): Promise<void> {
    const id = genId();
    try {
      await api.folderUpsert({ id, name: "New folder", parentId });
    } catch (e) {
      this.deps.toast(`Couldn't create folder: ${e}`, "error");
      return;
    }
    if (parentId) this.collapsed.delete(parentId);
    await this.refresh();
    this.startRename(id);
  }

  // ---- rendering ----------------------------------------------------------

  render(): void {
    this.el.innerHTML = "";
    this.el.classList.add("tree-host");
    if (this.folders.length === 0 && this.sessions.length === 0) {
      this.renderEmpty();
      return;
    }
    const tree = document.createElement("div");
    tree.className = "tree";
    this.renderLevel(tree, null, 0);
    this.el.appendChild(tree);

    // the whole panel is a drop zone for "move to root"
    this.el.ondragover = (e) => {
      if (this.drag) {
        e.preventDefault();
        this.el.classList.add("root-drop");
      }
    };
    this.el.ondragleave = (e) => {
      if (e.target === this.el) this.el.classList.remove("root-drop");
    };
    this.el.ondrop = (e) => {
      e.preventDefault();
      this.el.classList.remove("root-drop");
      void this.handleDrop(null);
    };
  }

  private renderEmpty(): void {
    this.el.innerHTML = `
      <div class="empty-hint">
        ${icon("server", "empty-hint__icon")}
        <p>No saved connections yet.</p>
        <div class="empty-actions">
          <button class="ghost-btn" data-act="new-conn">${icon("plus")} Connection</button>
          <button class="ghost-btn" data-act="new-folder">${icon("folderPlus")} Folder</button>
        </div>
      </div>`;
    this.el
      .querySelector('[data-act="new-conn"]')
      ?.addEventListener("click", () => this.deps.onNewConnection(null));
    this.el
      .querySelector('[data-act="new-folder"]')
      ?.addEventListener("click", () => void this.newFolder(null));
  }

  private childFolders(parentId: string | null): Folder[] {
    return this.folders.filter((f) => (f.parentId ?? null) === parentId).sort(byName);
  }
  private childSessions(folderId: string | null): SavedSession[] {
    return this.sessions.filter((s) => (s.folderId ?? null) === folderId).sort(byOrder);
  }

  private renderLevel(container: HTMLElement, parentId: string | null, depth: number): void {
    for (const f of this.childFolders(parentId)) this.renderFolder(container, f, depth);
    for (const s of this.childSessions(parentId)) this.renderSession(container, s, depth);
  }

  private renderFolder(container: HTMLElement, folder: Folder, depth: number): void {
    const collapsed = this.collapsed.has(folder.id);
    const row = document.createElement("div");
    row.className = "tree-folder" + (collapsed ? "" : " is-open");
    row.style.setProperty("--depth", String(depth));
    row.dataset.folderId = folder.id;
    row.draggable = true;
    const count = this.descendantCount(folder.id);
    row.innerHTML = `
      <span class="tree-twisty">${icon("chevronDown")}</span>
      <span class="tree-folder__icon">${icon("folder")}</span>
      <span class="tree-folder__name">${esc(folder.name)}</span>
      <span class="tree-folder__count">${count || ""}</span>
      <button class="tree-act" data-act="add" title="Add here">${icon("plus")}</button>`;

    row.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest('[data-act="add"]')) return;
      this.toggle(folder.id);
    });
    row.querySelector('[data-act="add"]')?.addEventListener("click", (e) => {
      e.stopPropagation();
      this.openAddMenu(e as MouseEvent, folder.id);
    });
    row.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      this.folderMenu(e, folder);
    });
    row.addEventListener("dblclick", (e) => {
      if (!(e.target as HTMLElement).closest(".tree-act")) this.startRename(folder.id);
    });
    this.wireDrag(row, { type: "folder", id: folder.id }, folder.id);
    container.appendChild(row);

    if (!collapsed) this.renderLevel(container, folder.id, depth + 1);
  }

  private renderSession(container: HTMLElement, s: SavedSession, depth: number): void {
    const row = document.createElement("div");
    row.className = "tree-conn";
    row.style.setProperty("--depth", String(depth));
    row.dataset.sessionId = s.id;
    row.draggable = true;
    const sub = s.kind === "ssh" ? `${s.username ?? ""}@${s.host}` : `${s.host}:${s.port ?? 23}`;
    row.innerHTML = `
      <span class="tree-conn__icon">${icon(s.kind === "ssh" ? "ssh" : "telnet")}</span>
      <div class="tree-conn__text">
        <span class="tree-conn__name">${esc(s.name)}</span>
        <span class="tree-conn__sub">${esc(sub)}</span>
      </div>
      <div class="tree-conn__actions">
        <button class="tree-act" data-act="edit" title="Edit">${icon("pencil")}</button>
        <button class="tree-act" data-act="delete" title="Delete">${icon("trash")}</button>
      </div>`;

    row.addEventListener("click", (e) => {
      const act = (e.target as HTMLElement).closest("[data-act]") as HTMLElement | null;
      if (act?.dataset.act === "edit") this.deps.onEdit(s);
      else if (act?.dataset.act === "delete") void this.deleteSession(s);
      else this.deps.onConnect(s);
    });
    row.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      this.sessionMenu(e, s);
    });
    this.wireDrag(row, { type: "session", id: s.id }, null);
    this.wireReorder(row, s);
    container.appendChild(row);
  }

  // ---- interactions -------------------------------------------------------

  private toggle(id: string): void {
    if (this.collapsed.has(id)) this.collapsed.delete(id);
    else this.collapsed.add(id);
    try {
      localStorage.setItem(LS_COLLAPSED, JSON.stringify([...this.collapsed]));
    } catch {
      /* ignore */
    }
    this.render();
  }

  private descendantCount(folderId: string): number {
    let n = this.childSessions(folderId).length;
    for (const f of this.childFolders(folderId)) n += this.descendantCount(f.id);
    return n;
  }

  private startRename(folderId: string): void {
    const row = this.el.querySelector(`.tree-folder[data-folder-id="${CSS.escape(folderId)}"]`);
    const nameEl = row?.querySelector(".tree-folder__name") as HTMLElement | null;
    const folder = this.folders.find((f) => f.id === folderId);
    if (!row || !nameEl || !folder) return;

    const input = document.createElement("input");
    input.className = "tree-rename";
    input.value = folder.name;
    nameEl.replaceWith(input);
    input.focus();
    input.select();

    let done = false;
    const commit = async (save: boolean) => {
      if (done) return;
      done = true;
      const val = input.value.trim();
      if (save && val && val !== folder.name) {
        try {
          await api.folderUpsert({ ...folder, name: val });
        } catch (e) {
          this.deps.toast(`Rename failed: ${e}`, "error");
        }
      }
      await this.refresh();
    };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void commit(true);
      } else if (e.key === "Escape") {
        void commit(false);
      }
    });
    input.addEventListener("blur", () => void commit(true));
  }

  private async deleteSession(s: SavedSession): Promise<void> {
    const ok = await ask(`Delete "${s.name}"?`, { title: "Delete connection", kind: "warning" });
    if (!ok) return;
    await api.sessionsDelete(s.id);
    await this.refresh();
    this.deps.toast(`Deleted "${s.name}"`, "info");
  }

  private async deleteFolder(f: Folder): Promise<void> {
    const ok = await ask(`Delete folder "${f.name}"? Its contents move up one level.`, {
      title: "Delete folder",
      kind: "warning",
    });
    if (!ok) return;
    await api.folderDelete(f.id);
    await this.refresh();
  }

  private async duplicateSession(s: SavedSession): Promise<void> {
    await api.sessionsUpsert({ ...s, id: genId(), name: `${s.name} copy`, saveSecret: false });
    await this.refresh();
    this.deps.toast(`Duplicated "${s.name}"`, "info");
  }

  // ---- menus --------------------------------------------------------------

  private openAddMenu(e: MouseEvent, folderId: string): void {
    contextMenu(e.clientX, e.clientY, [
      { label: "New connection", icon: "plus", action: () => this.deps.onNewConnection(folderId) },
      { label: "New subfolder", icon: "folderPlus", action: () => void this.newFolder(folderId) },
    ]);
  }
  private folderMenu(e: MouseEvent, folder: Folder): void {
    contextMenu(e.clientX, e.clientY, [
      { label: "New connection here", icon: "plus", action: () => this.deps.onNewConnection(folder.id) },
      { label: "New subfolder", icon: "folderPlus", action: () => void this.newFolder(folder.id) },
      "sep",
      { label: "Rename", icon: "pencil", action: () => this.startRename(folder.id) },
      { label: "Delete folder", icon: "trash", danger: true, action: () => void this.deleteFolder(folder) },
    ]);
  }
  private sessionMenu(e: MouseEvent, s: SavedSession): void {
    contextMenu(e.clientX, e.clientY, [
      { label: "Connect", icon: "bolt", action: () => this.deps.onConnect(s) },
      { label: "Edit", icon: "pencil", action: () => this.deps.onEdit(s) },
      { label: "Duplicate", icon: "copy", action: () => void this.duplicateSession(s) },
      "sep",
      { label: "Delete", icon: "trash", danger: true, action: () => void this.deleteSession(s) },
    ]);
  }

  // ---- drag & drop --------------------------------------------------------

  private wireDrag(row: HTMLElement, item: DragItem, dropFolderId: string | null): void {
    row.addEventListener("dragstart", (e) => {
      this.drag = item;
      e.stopPropagation();
      e.dataTransfer!.effectAllowed = "move";
      row.classList.add("dragging");
    });
    row.addEventListener("dragend", () => {
      this.drag = null;
      row.classList.remove("dragging");
      this.el
        .querySelectorAll(".drop-into, .drop-above, .drop-below")
        .forEach((el) => el.classList.remove("drop-into", "drop-above", "drop-below"));
    });
    if (dropFolderId !== null) {
      row.addEventListener("dragover", (e) => {
        if (!this.drag) return;
        e.preventDefault();
        e.stopPropagation();
        row.classList.add("drop-into");
      });
      row.addEventListener("dragleave", () => row.classList.remove("drop-into"));
      row.addEventListener("drop", (e) => {
        e.preventDefault();
        e.stopPropagation();
        row.classList.remove("drop-into");
        void this.handleDrop(dropFolderId);
      });
    }
  }

  private async handleDrop(targetFolderId: string | null): Promise<void> {
    const d = this.drag;
    this.drag = null;
    if (!d) return;
    if (d.type === "session") {
      if (!this.sessions.some((x) => x.id === d.id)) return;
      // Move into the target folder, appended to the end (via the reorder model).
      const ids = this.childSessions(targetFolderId)
        .filter((x) => x.id !== d.id)
        .map((x) => x.id);
      ids.push(d.id);
      await api.sessionsReorder(targetFolderId, ids);
    } else {
      if (d.id === targetFolderId) return;
      if (targetFolderId && this.isDescendant(targetFolderId, d.id)) {
        this.deps.toast("Can't move a folder into its own subfolder", "warn");
        return;
      }
      const f = this.folders.find((x) => x.id === d.id);
      if (!f || (f.parentId ?? null) === targetFolderId) return;
      await api.folderUpsert({ ...f, parentId: targetFolderId });
    }
    if (targetFolderId) this.collapsed.delete(targetFolderId);
    await this.refresh();
  }

  /** Session rows are reorder targets: drop above/below to reposition (dragging
   *  a session; folders still drop *into* via wireDrag). */
  private wireReorder(row: HTMLElement, s: SavedSession): void {
    const mark = (e: DragEvent): boolean => {
      const d = this.drag;
      if (d?.type !== "session" || d.id === s.id) return false;
      const r = row.getBoundingClientRect();
      const below = e.clientY > r.top + r.height / 2;
      row.classList.toggle("drop-below", below);
      row.classList.toggle("drop-above", !below);
      return below;
    };
    row.addEventListener("dragover", (e) => {
      if (this.drag?.type !== "session" || this.drag.id === s.id) return;
      e.preventDefault();
      e.stopPropagation();
      mark(e);
    });
    row.addEventListener("dragleave", () => row.classList.remove("drop-above", "drop-below"));
    row.addEventListener("drop", (e) => {
      const d = this.drag;
      if (d?.type !== "session" || d.id === s.id) return;
      e.preventDefault();
      e.stopPropagation();
      const below = mark(e);
      row.classList.remove("drop-above", "drop-below");
      this.drag = null;
      void this.reorderSession(d.id, s, below);
    });
  }

  /** Move a dragged session into `target`'s folder, positioned before/after it. */
  private async reorderSession(
    draggedId: string,
    target: SavedSession,
    after: boolean
  ): Promise<void> {
    if (draggedId === target.id) return;
    const folderId = target.folderId ?? null;
    const ids = this.childSessions(folderId)
      .filter((x) => x.id !== draggedId)
      .map((x) => x.id);
    const ti = ids.indexOf(target.id);
    if (ti < 0) return;
    ids.splice(after ? ti + 1 : ti, 0, draggedId);
    await api.sessionsReorder(folderId, ids);
    await this.refresh();
  }

  private isDescendant(candidateId: string, ancestorId: string): boolean {
    let cur = this.folders.find((f) => f.id === candidateId);
    const seen = new Set<string>();
    while (cur?.parentId && !seen.has(cur.id)) {
      if (cur.parentId === ancestorId) return true;
      seen.add(cur.id);
      cur = this.folders.find((f) => f.id === cur!.parentId);
    }
    return false;
  }
}

// ---- shared helpers -------------------------------------------------------

function byName(a: { name: string }, b: { name: string }): number {
  return a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
}

/** Sort by explicit `order` (drag-to-reorder), falling back to name. */
function byOrder(
  a: { order?: number | null; name: string },
  b: { order?: number | null; name: string }
): number {
  return (a.order ?? 0) - (b.order ?? 0) || byName(a, b);
}

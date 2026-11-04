// One file panel's state, with Commander semantics: the cursor and the marks
// are separate. Moving the cursor never changes the marks; operations act on
// the marked entries, or on the entry under the cursor when nothing is marked.
// Pure (no DOM), so every rule is unit-tested.

import type { FsEntry, FsListing, FsLocationView } from "../ipc";
import { maskToRegExp } from "./format";

export type SortKey = "name" | "size" | "modified";

export interface SortOrder {
  key: SortKey;
  direction: "asc" | "desc";
}

/** A panel row: the ".." link to the parent folder, or an entry. */
export type Row = { kind: "parent" } | { kind: "entry"; entry: FsEntry };

export interface ApplyOptions {
  /** Refresh: keep the cursor (by name) and marks (by path) instead of starting fresh. */
  keepState?: boolean;
  /** Put the cursor on this name, e.g. the folder we just came up from. */
  focusName?: string;
  /** Put the cursor on the entry with this path token (more exact than a name). */
  focusPath?: string;
}

const PARENT_NAME = "..";

const nameCollator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

/** Folders first, then by the chosen key; names break ties. */
export function sortEntries(entries: readonly FsEntry[], sort: SortOrder): FsEntry[] {
  const sign = sort.direction === "asc" ? 1 : -1;
  return entries.slice().sort((left, right) => {
    if (left.isDirectory !== right.isDirectory) return left.isDirectory ? -1 : 1;
    let compared = 0;
    if (sort.key === "size") compared = (left.size ?? -1) - (right.size ?? -1);
    else if (sort.key === "modified") compared = (left.modified ?? -1) - (right.modified ?? -1);
    if (compared === 0) compared = nameCollator.compare(left.name, right.name);
    return compared * sign;
  });
}

export class PanelModel {
  location: FsLocationView | null = null;
  parent: FsLocationView | null = null;
  entries: FsEntry[] = [];
  rows: Row[] = [];
  cursor = 0;
  /** Marked entries, by path token. */
  readonly marks = new Set<string>();
  /** Folder sizes computed on demand (Space), by path token. */
  readonly folderSizes = new Map<string, number>();
  sort: SortOrder = { key: "name", direction: "asc" };
  showHidden = false;
  filter = "";

  private visible: FsEntry[] = [];
  private byPath = new Map<string, FsEntry>();
  private counts = { folders: 0, files: 0, bytes: 0 };

  /** Where a Shift-extended range started, and the marks it started from. */
  private anchor: number | null = null;
  private rangeBase: Set<string> = new Set();

  applyListing(listing: FsListing, options: ApplyOptions = {}): void {
    const keep = options.keepState === true;
    const previousName = keep ? this.cursorName() : null;
    const previousIndex = this.cursor;
    const previousMarks = keep ? new Set(this.marks) : new Set<string>();

    this.location = listing.location;
    this.parent = listing.parent;
    this.entries = listing.entries;
    if (!keep) this.folderSizes.clear();
    this.marks.clear();
    const present = new Set(listing.entries.map((entry) => entry.path));
    for (const path of previousMarks) if (present.has(path)) this.marks.add(path);
    this.rebuild();

    const byPath = options.focusPath ? this.indexOfPath(options.focusPath) : -1;
    const focus = options.focusName ?? previousName;
    const found = byPath >= 0 ? byPath : focus === null ? -1 : this.indexOfName(focus);
    this.cursor = this.clamp(found >= 0 ? found : keep ? previousIndex : 0);
    this.resetRange();
  }

  /** Re-sort / re-filter, keeping the cursor on the same name where possible. */
  rebuildKeepingCursor(): void {
    const name = this.cursorName();
    this.rebuild();
    const found = name === null ? -1 : this.indexOfName(name);
    this.cursor = this.clamp(found >= 0 ? found : this.cursor);
    this.resetRange();
  }

  setSort(key: SortKey): void {
    this.sort =
      this.sort.key === key
        ? { key, direction: this.sort.direction === "asc" ? "desc" : "asc" }
        : { key, direction: "asc" };
    this.rebuildKeepingCursor();
  }

  setFilter(query: string): void {
    this.filter = query;
    this.rebuildKeepingCursor();
  }

  setShowHidden(show: boolean): void {
    this.showHidden = show;
    this.rebuildKeepingCursor();
  }

  rowName(index: number): string | null {
    const row = this.rows[index];
    if (!row) return null;
    return row.kind === "parent" ? PARENT_NAME : row.entry.name;
  }

  cursorName(): string | null {
    return this.rowName(this.cursor);
  }

  /** The entry under the cursor (null on ".." or an empty panel). */
  cursorEntry(): FsEntry | null {
    const row = this.rows[this.cursor];
    return row?.kind === "entry" ? row.entry : null;
  }

  isParentRow(index: number): boolean {
    return this.rows[index]?.kind === "parent";
  }

  isMarked(index: number): boolean {
    const row = this.rows[index];
    return row?.kind === "entry" && this.marks.has(row.entry.path);
  }

  /** Move the cursor without touching the marks (arrows, PgUp/PgDn, Home/End). */
  moveCursor(delta: number): void {
    this.setCursor(this.cursor + delta);
  }

  setCursor(index: number): void {
    this.cursor = this.clamp(index);
    this.resetRange();
  }

  /** Space: toggle the mark under the cursor. Returns the entry, if any. */
  toggleMarkAtCursor(): FsEntry | null {
    this.toggle(this.cursor);
    this.resetRange();
    return this.cursorEntry();
  }

  /** Insert: toggle the mark under the cursor and move down. */
  toggleMarkAndAdvance(): void {
    this.toggle(this.cursor);
    this.cursor = this.clamp(this.cursor + 1);
    this.resetRange();
  }

  /**
   * Shift+movement / Shift+click: mark everything between where the range
   * started and `index`, on top of the marks that existed before. Moving back
   * shrinks the range again.
   */
  extendTo(index: number): void {
    if (this.anchor === null) {
      this.anchor = this.cursor;
      this.rangeBase = new Set(this.marks);
    }
    const target = this.clamp(index);
    this.marks.clear();
    for (const path of this.rangeBase) this.marks.add(path);
    const [from, to] = this.anchor <= target ? [this.anchor, target] : [target, this.anchor];
    for (let row = from; row <= to; row += 1) {
      const entry = this.entryAt(row);
      if (entry) this.marks.add(entry.path);
    }
    this.cursor = target;
  }

  /** A plain click: the cursor moves there and the marks are cleared. */
  clickRow(index: number): void {
    this.marks.clear();
    this.setCursor(index);
  }

  /** Ctrl+click: toggle that row's mark and put the cursor on it. */
  toggleRow(index: number): void {
    this.cursor = this.clamp(index);
    this.toggle(this.cursor);
    this.resetRange();
  }

  markAll(): void {
    for (const entry of this.visibleEntries()) this.marks.add(entry.path);
    this.resetRange();
  }

  clearMarks(): void {
    this.marks.clear();
    this.resetRange();
  }

  /**
   * Numpad +/-: mark or unmark files whose names match `mask`. Folders are
   * left alone, as in Far and Norton Commander. Returns how many changed.
   */
  markByMask(mask: string, mark: boolean): number {
    const pattern = maskToRegExp(mask);
    if (!pattern) return 0;
    let changed = 0;
    for (const entry of this.visibleEntries()) {
      if (entry.isDirectory || !pattern.test(entry.name)) continue;
      if (mark !== this.marks.has(entry.path)) changed += 1;
      if (mark) this.marks.add(entry.path);
      else this.marks.delete(entry.path);
    }
    this.resetRange();
    return changed;
  }

  /** Numpad *: invert the marks on files (folders are left alone). */
  invertMarks(): void {
    for (const entry of this.visibleEntries()) {
      if (entry.isDirectory) continue;
      if (this.marks.has(entry.path)) this.marks.delete(entry.path);
      else this.marks.add(entry.path);
    }
    this.resetRange();
  }

  /** What an operation acts on: the marked entries, else the one under the cursor. */
  targets(): FsEntry[] {
    const marked = this.visibleEntries().filter((entry) => this.marks.has(entry.path));
    if (marked.length) return marked;
    const entry = this.cursorEntry();
    return entry ? [entry] : [];
  }

  /** Count and total size of the marked entries (folders count once sized). */
  markedSummary(): { count: number; bytes: number } {
    let count = 0;
    let bytes = 0;
    for (const path of this.marks) {
      const entry = this.byPath.get(path);
      if (!entry) continue;
      count += 1;
      bytes += entry.isDirectory ? (this.folderSizes.get(entry.path) ?? 0) : (entry.size ?? 0);
    }
    return { count, bytes };
  }

  /** Folders, files and file bytes shown (computed once per rebuild). */
  get totals(): { folders: number; files: number; bytes: number } {
    return this.counts;
  }

  indexOfPath(path: string): number {
    return this.rows.findIndex((row) => row.kind === "entry" && row.entry.path === path);
  }

  /** Index of the first row whose name starts with `prefix` (case-insensitive), or -1. */
  findPrefix(prefix: string): number {
    const needle = prefix.toLocaleLowerCase();
    return this.rows.findIndex(
      (row) => row.kind === "entry" && row.entry.name.toLocaleLowerCase().startsWith(needle)
    );
  }

  indexOfName(name: string): number {
    const exact = this.rows.findIndex((_, index) => this.rowName(index) === name);
    if (exact >= 0) return exact;
    const lower = name.toLocaleLowerCase();
    return this.rows.findIndex((_, index) => this.rowName(index)?.toLocaleLowerCase() === lower);
  }

  /** The entries shown, in row order. Cached per rebuild: don't modify it. */
  visibleEntries(): readonly FsEntry[] {
    return this.visible;
  }

  private entryAt(index: number): FsEntry | null {
    const row = this.rows[index];
    return row?.kind === "entry" ? row.entry : null;
  }

  private toggle(index: number): void {
    const entry = this.entryAt(index);
    if (!entry) return;
    if (this.marks.has(entry.path)) this.marks.delete(entry.path);
    else this.marks.add(entry.path);
  }

  private rebuild(): void {
    const needle = this.filter.trim().toLocaleLowerCase();
    const visible = this.entries.filter(
      (entry) =>
        (this.showHidden || !entry.hidden) &&
        (!needle || entry.name.toLocaleLowerCase().includes(needle))
    );
    const rows: Row[] = this.parent ? [{ kind: "parent" }] : [];
    const sorted = sortEntries(visible, this.sort);
    for (const entry of sorted) rows.push({ kind: "entry", entry });
    this.rows = rows;
    // Cached so per-keypress work (status line, marked totals) stays O(marks).
    this.visible = sorted;
    this.byPath = new Map(sorted.map((entry) => [entry.path, entry]));
    const counts = { folders: 0, files: 0, bytes: 0 };
    for (const entry of sorted) {
      if (entry.isDirectory) counts.folders += 1;
      else {
        counts.files += 1;
        counts.bytes += entry.size ?? 0;
      }
    }
    this.counts = counts;
    // Marks on entries that are no longer shown would be acted on invisibly.
    const shown = new Set(visible.map((entry) => entry.path));
    for (const path of [...this.marks]) if (!shown.has(path)) this.marks.delete(path);
  }

  private resetRange(): void {
    this.anchor = null;
    this.rangeBase = new Set();
  }

  private clamp(index: number): number {
    return Math.max(0, Math.min(this.rows.length - 1, index));
  }
}

/** Back/forward history of visited folders. */
export class History {
  private stack: FsLocationView[] = [];
  private index = -1;

  /** Record a visit; going somewhere new drops any "forward" entries. */
  visit(location: FsLocationView): void {
    if (this.current()?.path === location.path && this.current()?.fs === location.fs) return;
    this.stack = this.stack.slice(0, this.index + 1);
    this.stack.push(location);
    this.index = this.stack.length - 1;
  }

  current(): FsLocationView | null {
    return this.stack[this.index] ?? null;
  }

  canGo(step: -1 | 1): boolean {
    return this.stack[this.index + step] !== undefined;
  }

  /** The location `step` away, without moving there yet. */
  peek(step: -1 | 1): FsLocationView | null {
    return this.stack[this.index + step] ?? null;
  }

  /** Move after a successful navigation to `peek(step)`. */
  go(step: -1 | 1): void {
    if (this.canGo(step)) this.index += step;
  }
}

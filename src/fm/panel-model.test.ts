import { describe, expect, it } from "vitest";
import type { FsEntry, FsListing, FsLocationView } from "../ipc";
import { History, PanelModel, sortEntries } from "./panel-model";
import { formatDuration, formatFileSize, formatMode, maskToRegExp } from "./format";

const at = (display: string): FsLocationView => ({ fs: "local", path: `tok:${display}`, display });

function entry(name: string, options: Partial<FsEntry> = {}): FsEntry {
  return {
    name,
    path: `tok:${name}`,
    display: `/x/${name}`,
    kind: options.isDirectory ? "directory" : "file",
    isDirectory: false,
    size: 10,
    modified: 0,
    mode: null,
    readonly: false,
    hidden: false,
    owner: null,
    group: null,
    error: null,
    ...options,
  };
}

const dir = (name: string) => entry(name, { isDirectory: true, kind: "directory", size: null });

function listing(entries: FsEntry[], withParent = true): FsListing {
  return { location: at("/x"), parent: withParent ? at("/") : null, entries };
}

/** A panel showing ".." + folders a, b + files c.txt, d.md, e.txt. */
function panel(): PanelModel {
  const model = new PanelModel();
  model.applyListing(
    listing([entry("e.txt"), dir("b"), entry("c.txt"), dir("a"), entry("d.md")])
  );
  return model;
}

const names = (model: PanelModel) => model.rows.map((_, i) => model.rowName(i));
const marked = (model: PanelModel) => model.targets().map((e) => e.name);

describe("rows", () => {
  it("puts .. first, then folders, then files, naturally sorted", () => {
    expect(names(panel())).toEqual(["..", "a", "b", "c.txt", "d.md", "e.txt"]);
    const files = sortEntries([entry("file10"), entry("file2")], { key: "name", direction: "asc" });
    expect(files.map((e) => e.name)).toEqual(["file2", "file10"]);
  });

  it("has no .. row at a filesystem root", () => {
    const model = new PanelModel();
    model.applyListing(listing([entry("a")], false));
    expect(names(model)).toEqual(["a"]);
  });

  it("keeps folders first when sorting descending", () => {
    const model = panel();
    model.setSort("name");
    expect(model.sort.direction).toBe("desc");
    expect(names(model)).toEqual(["..", "b", "a", "e.txt", "d.md", "c.txt"]);
  });
});

describe("cursor and marks are separate (B3)", () => {
  it("cursor movement never changes the marks", () => {
    const model = panel();
    model.setCursor(3);
    model.toggleMarkAndAdvance(); // marks c.txt, cursor → d.md
    model.moveCursor(1);
    model.moveCursor(-3);
    model.setCursor(0);
    model.setCursor(99);
    expect(marked(model)).toEqual(["c.txt"]);
    expect(model.cursor).toBe(5);
  });

  it("Insert toggles and advances; Space toggles in place", () => {
    const model = panel();
    model.setCursor(1);
    model.toggleMarkAndAdvance();
    model.toggleMarkAndAdvance();
    expect(marked(model)).toEqual(["a", "b"]);
    expect(model.cursor).toBe(3);
    model.setCursor(1);
    expect(model.toggleMarkAtCursor()?.name).toBe("a");
    expect(model.cursor).toBe(1);
    expect(marked(model)).toEqual(["b"]);
  });

  it(".. can hold the cursor but is never marked or acted on", () => {
    const model = panel();
    model.setCursor(0);
    model.toggleMarkAtCursor();
    expect(model.marks.size).toBe(0);
    expect(model.targets()).toEqual([]);
    model.markAll();
    expect(marked(model)).toEqual(["a", "b", "c.txt", "d.md", "e.txt"]);
  });

  it("operations fall back to the cursor entry when nothing is marked", () => {
    const model = panel();
    model.setCursor(4);
    expect(marked(model)).toEqual(["d.md"]);
  });

  it("Shift+movement marks a range on top of earlier marks, and shrinks back", () => {
    const model = panel();
    model.setCursor(5);
    model.toggleMarkAtCursor(); // e.txt
    model.setCursor(1);
    model.extendTo(2);
    model.extendTo(3);
    expect(marked(model)).toEqual(["a", "b", "c.txt", "e.txt"]);
    model.extendTo(2); // back up: c.txt leaves the range
    expect(marked(model)).toEqual(["a", "b", "e.txt"]);
    model.moveCursor(1); // a plain move ends the range, keeping its marks
    model.extendTo(4);
    expect(marked(model)).toEqual(["a", "b", "c.txt", "d.md", "e.txt"]);
  });

  it("mouse: click clears marks, Ctrl+click toggles, Shift+click extends", () => {
    const model = panel();
    model.toggleRow(2);
    model.toggleRow(4);
    expect(marked(model)).toEqual(["b", "d.md"]);
    model.toggleRow(2);
    expect(marked(model)).toEqual(["d.md"]);
    model.clickRow(1);
    expect(model.marks.size).toBe(0);
    model.extendTo(3);
    expect(marked(model)).toEqual(["a", "b", "c.txt"]);
  });

  it("numpad masks mark/unmark files only, and * inverts files only", () => {
    const model = panel();
    expect(model.markByMask("*.txt", true)).toBe(2);
    expect(marked(model)).toEqual(["c.txt", "e.txt"]);
    model.markByMask("e*", false);
    expect(marked(model)).toEqual(["c.txt"]);
    model.markByMask("*", true);
    expect(marked(model)).toEqual(["c.txt", "d.md", "e.txt"]);
    model.invertMarks();
    expect(model.marks.size).toBe(0);
    model.toggleRow(1); // a folder
    model.invertMarks();
    expect(marked(model)).toEqual(["a", "c.txt", "d.md", "e.txt"]);
  });

  it("summarizes marked sizes, counting folders once measured", () => {
    const model = panel();
    model.markAll();
    expect(model.markedSummary()).toEqual({ count: 5, bytes: 30 });
    model.folderSizes.set("tok:a", 1000);
    expect(model.markedSummary().bytes).toBe(1030);
  });
});

describe("refresh and navigation (B9)", () => {
  it("refresh keeps the cursor by name and the marks by path", () => {
    const model = panel();
    model.setCursor(4); // d.md
    model.toggleRow(3); // c.txt marked, cursor c.txt
    model.setCursor(4);
    model.applyListing(listing([entry("0-new.txt"), dir("a"), dir("b"), entry("c.txt"), entry("d.md")]), {
      keepState: true,
    });
    expect(model.cursorName()).toBe("d.md");
    expect(marked(model)).toEqual(["c.txt"]);
  });

  it("refresh falls back to the nearest index when the cursor entry is gone", () => {
    const model = panel();
    model.setCursor(5); // e.txt, deleted below
    model.applyListing(listing([dir("a"), dir("b"), entry("c.txt")]), { keepState: true });
    expect(model.cursorName()).toBe("c.txt");
  });

  it("navigation starts fresh, and focusName lands on the folder we came from", () => {
    const model = panel();
    model.markAll();
    model.applyListing(listing([dir("docs"), dir("src"), entry("x")]), { focusName: "src" });
    expect(model.marks.size).toBe(0);
    expect(model.cursorName()).toBe("src");
  });

  it("filters hide entries and drop their marks; the cursor stays put by name", () => {
    const model = panel();
    model.markAll();
    model.setCursor(3); // c.txt
    model.setFilter("txt");
    expect(names(model)).toEqual(["..", "c.txt", "e.txt"]);
    expect(marked(model)).toEqual(["c.txt", "e.txt"]);
    expect(model.cursorName()).toBe("c.txt");
    model.setCursor(2); // e.txt
    model.setFilter("");
    expect(model.cursorName()).toBe("e.txt");
    // A filtered-out cursor entry falls back to the nearest row.
    model.setCursor(4); // d.md
    model.setFilter("txt");
    expect(model.cursorName()).toBe("e.txt");
  });

  it("hidden entries appear only on request", () => {
    const model = new PanelModel();
    model.applyListing(listing([entry(".env", { hidden: true }), entry("a")]));
    expect(names(model)).toEqual(["..", "a"]);
    model.setShowHidden(true);
    expect(names(model)).toEqual(["..", ".env", "a"]);
  });

  it("quick search jumps by prefix, case-insensitively", () => {
    const model = panel();
    expect(model.findPrefix("D")).toBe(4);
    expect(model.findPrefix("zz")).toBe(-1);
  });

  it("history goes back and forward, and a new visit drops forward entries", () => {
    const history = new History();
    history.visit(at("/a"));
    history.visit(at("/b"));
    history.visit(at("/c"));
    expect(history.peek(-1)?.display).toBe("/b");
    history.go(-1);
    history.go(-1);
    expect(history.current()?.display).toBe("/a");
    expect(history.canGo(-1)).toBe(false);
    history.visit(at("/d"));
    expect(history.canGo(1)).toBe(false);
    history.go(-1);
    expect(history.current()?.display).toBe("/a");
  });
});

describe("format", () => {
  it("formats sizes, durations and modes", () => {
    expect(formatFileSize(999)).toBe("999 B");
    expect(formatFileSize(1536)).toBe("1.50 KB");
    expect(formatDuration(75)).toBe("1:15");
    expect(formatDuration(3725)).toBe("1:02:05");
    expect(formatMode(0o755, true)).toBe("drwxr-xr-x");
    expect(formatMode(0o640, false)).toBe("-rw-r-----");
  });

  it("turns masks into whole-name, case-insensitive patterns", () => {
    const mask = maskToRegExp("*.TXT; report?.doc")!;
    expect(mask.test("notes.txt")).toBe(true);
    expect(mask.test("report1.doc")).toBe(true);
    expect(mask.test("report10.doc")).toBe(false);
    expect(mask.test("notes.txt.bak")).toBe(false);
    expect(maskToRegExp("a+b(1).txt")!.test("a+b(1).txt")).toBe(true);
    expect(maskToRegExp(" ; ")).toBeNull();
  });
});

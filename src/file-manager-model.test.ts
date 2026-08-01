import { describe, expect, it } from "vitest";
import type { FileEntry } from "./ipc";
import { formatFileSize, rangeTokens, visibleEntries } from "./file-manager-model";

const entry = (name: string, options: Partial<FileEntry> = {}): FileEntry => ({
  name,
  path: name,
  token: name,
  kind: "file",
  isDirectory: false,
  size: 0,
  modified: 0,
  hidden: false,
  readonly: false,
  ...options,
});

describe("file manager model", () => {
  it("sorts directories first and names naturally", () => {
    const result = visibleEntries(
      [entry("file10"), entry("folder", { kind: "directory", isDirectory: true }), entry("file2")],
      true,
      "",
      { key: "name", direction: "asc" }
    );
    expect(result.map((item) => item.name)).toEqual(["folder", "file2", "file10"]);
  });

  it("filters hidden items and matches case-insensitively", () => {
    const items = [entry("README.md"), entry(".secret", { hidden: true })];
    expect(visibleEntries(items, false, "read", { key: "name", direction: "asc" })).toHaveLength(1);
    expect(visibleEntries(items, false, "secret", { key: "name", direction: "asc" })).toHaveLength(0);
  });

  it("builds a selection range in either direction", () => {
    const items = [entry("a"), entry("b"), entry("c")];
    expect(rangeTokens(items, "c", "a")).toEqual(["a", "b", "c"]);
  });

  it("formats compact file sizes", () => {
    expect(formatFileSize(999)).toBe("999 B");
    expect(formatFileSize(1536)).toBe("1.50 KB");
  });
});

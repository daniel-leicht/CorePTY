import type { FileEntry } from "./ipc";

export type FileSortKey = "name" | "size" | "modified";
export type SortDirection = "asc" | "desc";

export interface FileSort {
  key: FileSortKey;
  direction: SortDirection;
}

const nameCollator = new Intl.Collator(undefined, {
  numeric: true,
  sensitivity: "base",
});
const modifiedFormatter = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short",
});

/** Filter and sort a panel while always keeping directories above files. */
export function visibleEntries(
  entries: readonly FileEntry[],
  showHidden: boolean,
  query: string,
  sort: FileSort
): FileEntry[] {
  const needle = query.trim().toLocaleLowerCase();
  return entries
    .filter((entry) => showHidden || !entry.hidden)
    .filter((entry) => !needle || entry.name.toLocaleLowerCase().includes(needle))
    .slice()
    .sort((left, right) => {
      if (left.isDirectory !== right.isDirectory) return left.isDirectory ? -1 : 1;
      let compared = 0;
      if (sort.key === "size") compared = (left.size ?? -1) - (right.size ?? -1);
      else if (sort.key === "modified")
        compared = (left.modified ?? -1) - (right.modified ?? -1);
      else compared = nameCollator.compare(left.name, right.name);
      if (compared === 0) compared = nameCollator.compare(left.name, right.name);
      return sort.direction === "asc" ? compared : -compared;
    });
}

export function formatFileSize(size: number | null): string {
  if (size === null) return "";
  if (size < 1024) return `${size} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let value = size / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${units[unit]}`;
}

export function formatModified(value: number | null): string {
  if (value === null) return "";
  return modifiedFormatter.format(new Date(value));
}

export function rangeTokens(entries: readonly FileEntry[], from: string, to: string): string[] {
  const first = entries.findIndex((entry) => entry.token === from);
  const last = entries.findIndex((entry) => entry.token === to);
  if (first < 0 || last < 0) return [to];
  const start = Math.min(first, last);
  const end = Math.max(first, last);
  return entries.slice(start, end + 1).map((entry) => entry.token);
}

// Display formatting shared by the file manager's panels, dialogs and queue.

const modifiedFormatter = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short",
});

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

export function formatSpeed(bytesPerSecond: number): string {
  return bytesPerSecond > 0 ? `${formatFileSize(Math.round(bytesPerSecond))}/s` : "";
}

/** 75 → "1:15", 3725 → "1:02:05". */
export function formatDuration(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds)) return "";
  const total = Math.max(0, Math.round(seconds));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/**
 * POSIX bits as `drwxr-xr-x`, the way `ls -l` and WinSCP show them, with
 * setuid/setgid (`s`/`S`) and sticky (`t`/`T`). `type` is the leading letter:
 * `d` folder, `l` link, `-` anything else.
 */
export function formatMode(mode: number | null, type: "-" | "d" | "l" = "-"): string {
  if (mode === null) return "";
  const triad = (shift: number, special: number, letter: string) => {
    const read = mode & (4 << shift) ? "r" : "-";
    const write = mode & (2 << shift) ? "w" : "-";
    const execute = (mode & (1 << shift)) !== 0;
    const last =
      mode & special ? (execute ? letter : letter.toUpperCase()) : execute ? "x" : "-";
    return read + write + last;
  };
  return type + triad(6, 0o4000, "s") + triad(3, 0o2000, "s") + triad(0, 0o1000, "t");
}

/** The `ls -l` type letter for an entry. */
export function modeType(entry: { kind: string; isDirectory: boolean }): "-" | "d" | "l" {
  if (entry.kind === "symlink") return "l";
  return entry.isDirectory ? "d" : "-";
}

/**
 * A file mask like `*.txt` or `report?.doc;*.md` (`;` or `,` separate several)
 * as a case-insensitive, whole-name RegExp. Empty masks match nothing.
 */
export function maskToRegExp(mask: string): RegExp | null {
  const parts = mask
    .split(/[;,]/)
    .map((part) => part.trim())
    .filter(Boolean);
  if (!parts.length) return null;
  const pattern = parts
    .map((part) =>
      part
        .split("")
        .map((char) =>
          char === "*" ? ".*" : char === "?" ? "." : char.replace(/[\\^$.|+()[\]{}]/g, "\\$&")
        )
        .join("")
    )
    .join("|");
  return new RegExp(`^(?:${pattern})$`, "i");
}

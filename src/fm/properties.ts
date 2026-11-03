// Alt+Enter: an entry's properties — size (folders counted up), dates, owner —
// and, on POSIX filesystems, its permissions as a checkbox grid tied to an
// octal field. Applying runs chmod (recursively into folders, if asked).

import { fsApi, type FsEntry } from "../ipc";
import { modal, type DialogContext } from "./dialogs";
import { formatFileSize, formatMode, formatModified, modeType } from "./format";

const WHO = [
  { label: "Owner", shift: 6 },
  { label: "Group", shift: 3 },
  { label: "Others", shift: 0 },
] as const;
const WHAT = [
  { label: "Read", bit: 4 },
  { label: "Write", bit: 2 },
  { label: "Execute", bit: 1 },
] as const;
const SPECIAL = [
  { label: "Set user ID", bit: 0o4000 },
  { label: "Set group ID", bit: 0o2000 },
  { label: "Sticky", bit: 0o1000 },
] as const;

/** `755`, `4750` (four digits only with special bits). */
export function octal(mode: number): string {
  const text = (mode & 0o7777).toString(8);
  return mode & 0o7000 ? text.padStart(4, "0") : text.padStart(3, "0");
}

/** An octal field's value, or null when it isn't one (`75`, `0755`, `rwx`). */
export function parseOctal(text: string): number | null {
  const trimmed = text.trim();
  if (!/^[0-7]{3,4}$/.test(trimmed)) return null;
  return parseInt(trimmed, 8);
}

export interface PropertiesOptions {
  fs: string;
  entries: FsEntry[];
  /** The folder they're in, as shown. */
  folder: string;
  posix: boolean;
}

/** Show the dialog; resolves true when permissions were changed. */
export async function propertiesDialog(
  options: PropertiesOptions,
  context: DialogContext = {}
): Promise<boolean> {
  const { entries, posix } = options;
  const one = entries.length === 1 ? entries[0] : null;
  const folders = entries.filter((entry) => entry.isDirectory);
  const body = document.createElement("div");
  body.className = "fm-props";
  const rows: Array<[string, string]> = [
    ["Name", one ? one.name : `${entries.length} items`],
    ["Where", options.folder],
  ];
  if (one) {
    rows.push(["Type", one.kind === "symlink" ? "Link" : one.isDirectory ? "Folder" : "File"]);
    rows.push(["Modified", formatModified(one.modified) || "—"]);
    if (one.owner !== null || one.group !== null) {
      rows.push(["Owner", `${one.owner ?? "?"}${one.group ? ` : ${one.group}` : ""}`]);
    }
  }
  const fileBytes = entries.reduce((sum, entry) => sum + (entry.isDirectory ? 0 : (entry.size ?? 0)), 0);
  rows.push(["Size", folders.length ? "Counting…" : formatFileSize(fileBytes)]);
  body.innerHTML = `<dl class="fm-props__facts"></dl>`;
  const facts = body.querySelector("dl")!;
  for (const [label, value] of rows) {
    const term = document.createElement("dt");
    term.textContent = label;
    const detail = document.createElement("dd");
    detail.textContent = value;
    if (label === "Size") detail.dataset.size = "";
    facts.append(term, detail);
  }

  // Folder sizes are counted while the dialog is open.
  if (folders.length) {
    void (async () => {
      let items = 0;
      let bytes = fileBytes;
      for (const folder of folders) {
        try {
          const size = await fsApi.folderSize({ fs: options.fs, path: folder.path });
          items += size.items;
          bytes += size.bytes;
        } catch {
          // Unreadable parts: what could be counted is shown.
        }
      }
      const cell = facts.querySelector<HTMLElement>("[data-size]");
      if (cell) cell.textContent = `${formatFileSize(bytes)} · ${items.toLocaleString()} items inside`;
    })();
  }

  let original: number | null = null;
  let current = 0;
  let recursive: HTMLInputElement | null = null;
  if (posix) {
    original = one?.mode ?? entries.find((entry) => entry.mode !== null)?.mode ?? null;
    current = original ?? 0o644;
    const perms = document.createElement("div");
    perms.className = "fm-perms";
    perms.innerHTML = `
      <div class="fm-perms__grid" role="group" aria-label="Permissions">
        <span></span>${WHAT.map((what) => `<span class="fm-perms__head">${what.label}</span>`).join("")}
        ${WHO.map(
          (who) =>
            `<span class="fm-perms__who">${who.label}</span>${WHAT.map(
              (what) =>
                `<label class="check fm-perms__cell"><input type="checkbox" data-bit="${what.bit << who.shift}" aria-label="${who.label} ${what.label.toLowerCase()}" /></label>`
            ).join("")}`
        ).join("")}
      </div>
      <div class="fm-perms__special">${SPECIAL.map(
        (special) => `<label class="check"><input type="checkbox" data-bit="${special.bit}" /> ${special.label}</label>`
      ).join("")}</div>
      <label class="field fm-perms__octal"><span class="field__label">Octal</span><input inputmode="numeric" spellcheck="false" maxlength="4" /><span class="fm-perms__text"></span></label>
      ${folders.length ? `<label class="check fm-perms__recursive"><input type="checkbox" /> Also everything inside ${folders.length === 1 ? "the folder" : "the folders"} (folders keep x where r is set)</label>` : ""}`;
    body.appendChild(perms);
    const boxes = Array.from(perms.querySelectorAll<HTMLInputElement>("input[data-bit]"));
    const field = perms.querySelector<HTMLInputElement>(".fm-perms__octal input")!;
    const text = perms.querySelector<HTMLElement>(".fm-perms__text")!;
    recursive = perms.querySelector<HTMLInputElement>(".fm-perms__recursive input");
    const type = one ? modeType(one) : "-";
    const show = (from: "boxes" | "field" | "start") => {
      for (const box of boxes) box.checked = (current & Number(box.dataset.bit)) !== 0;
      if (from !== "field") field.value = octal(current);
      field.classList.remove("is-invalid");
      text.textContent = formatMode(current, type);
    };
    for (const box of boxes) {
      box.addEventListener("change", () => {
        const bit = Number(box.dataset.bit);
        current = box.checked ? current | bit : current & ~bit;
        show("boxes");
      });
    }
    field.addEventListener("input", () => {
      const parsed = parseOctal(field.value);
      if (parsed === null) {
        field.classList.add("is-invalid");
        return;
      }
      current = parsed;
      show("field");
    });
    show("start");
    if (original === null) text.textContent = "(not known: shown as 644)";
  }

  const choice = await modal({
    ...context,
    title: one ? `${one.name} — properties` : "Properties",
    className: "fm-props-dialog",
    body,
    buttons: posix
      ? [
          { value: "cancel", label: "Cancel" },
          { value: "apply", label: "Apply", kind: "primary" },
        ]
      : [{ value: "cancel", label: "Close", kind: "primary" }],
    enterValue: posix ? "apply" : "cancel",
  });
  if (choice !== "apply" || !posix) return false;
  const deep = recursive?.checked ?? false;
  if (current === original && !deep) return false;
  for (const entry of entries) {
    await fsApi.chmod({ fs: options.fs, path: entry.path }, current, deep && entry.isDirectory);
  }
  return true;
}

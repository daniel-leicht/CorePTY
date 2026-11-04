// Alt+Enter: an entry's properties — size (folders counted up), dates, owner —
// and, on POSIX filesystems, its permissions as a checkbox grid tied to an
// octal field. For several entries a box is mixed until changed; applying
// sets just the bits changed (each entry keeps the rest of its own mode), so
// one dialog can't flatten different modes into one. Links are left alone.

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

/** What each permission bit is across entries: all on, all off, or mixed
 * (a mode not known counts as mixed). */
export function commonBits(modes: Array<number | null>): { on: number; off: number } {
  let on = 0o7777;
  let off = 0o7777;
  for (const mode of modes) {
    if (mode === null) return { on: 0, off: 0 };
    on &= mode;
    off &= ~mode;
  }
  return modes.length ? { on: on & 0o7777, off: off & 0o7777 } : { on: 0, off: 0 };
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

  // Links are left alone: changing one changes what it points at.
  const targets = entries.filter((entry) => entry.kind !== "symlink");
  const common = commonBits(targets.map((entry) => entry.mode));
  /** Bits the user turned on and off. */
  let set = 0;
  let clear = 0;
  let valid = true;
  let recursive: HTMLInputElement | null = null;
  let apply: () => void = () => undefined;
  if (posix && targets.length) {
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
      ${folders.length ? `<label class="check fm-perms__recursive"><input type="checkbox" /> Also everything inside ${folders.length === 1 ? "the folder" : "the folders"} (folders keep x where r is set)</label>` : ""}
      ${targets.length < entries.length ? `<p class="fm-confirm__note">Links are left as they are.</p>` : ""}`;
    body.appendChild(perms);
    const boxes = Array.from(perms.querySelectorAll<HTMLInputElement>("input[data-bit]"));
    const field = perms.querySelector<HTMLInputElement>(".fm-perms__octal input")!;
    const text = perms.querySelector<HTMLElement>(".fm-perms__text")!;
    recursive = perms.querySelector<HTMLInputElement>(".fm-perms__recursive input");
    const type = one ? modeType(one) : "-";
    /** The mode every entry gets, when that's one mode for all; else null. */
    const resulting = (): number | null => {
      const on = (common.on & ~clear) | set;
      const off = (common.off & ~set) | clear;
      return (on | off) === 0o7777 ? on : null;
    };
    const show = (from: "boxes" | "field") => {
      for (const box of boxes) {
        const bit = Number(box.dataset.bit);
        const on = ((common.on & ~clear) | set) & bit;
        const off = ((common.off & ~set) | clear) & bit;
        box.checked = on !== 0;
        box.indeterminate = !on && !off;
      }
      const mode = resulting();
      if (from !== "field") field.value = mode === null ? "" : octal(mode);
      field.placeholder = mode === null ? "mixed" : "";
      field.classList.remove("is-invalid");
      valid = true;
      text.textContent =
        mode !== null
          ? formatMode(mode, type)
          : targets.some((entry) => entry.mode === null)
            ? "(not known: only what you change is set)"
            : "(different on each: only what you change is set)";
      apply();
    };
    for (const box of boxes) {
      box.addEventListener("change", () => {
        const bit = Number(box.dataset.bit);
        if (box.checked) {
          set |= bit;
          clear &= ~bit;
        } else {
          clear |= bit;
          set &= ~bit;
        }
        show("boxes");
      });
    }
    field.addEventListener("input", () => {
      const parsed = parseOctal(field.value);
      if (parsed === null) {
        // Nothing is applied while the field holds something that isn't a mode.
        field.classList.toggle("is-invalid", field.value.trim() !== "");
        valid = field.value.trim() === "";
        apply();
        return;
      }
      set = parsed;
      clear = ~parsed & 0o7777;
      show("field");
    });
    show("boxes");
  }

  const pending = modal({
    ...context,
    title: one ? `${one.name} — properties` : "Properties",
    className: "fm-props-dialog",
    body,
    buttons:
      posix && targets.length
        ? [
            { value: "cancel", label: "Cancel" },
            { value: "apply", label: "Apply", kind: "primary" },
          ]
        : [{ value: "cancel", label: "Close", kind: "primary" }],
    enterValue: posix && targets.length ? "apply" : "cancel",
  });
  // Apply only once something changed (and the octal field holds a mode).
  const applyButton = document.querySelector<HTMLButtonElement>('.fm-props-dialog [data-value="apply"]');
  apply = () => {
    if (applyButton) applyButton.disabled = !valid || (set === 0 && clear === 0);
  };
  apply();
  const choice = await pending;
  if (choice !== "apply" || !posix || !valid || (set === 0 && clear === 0)) return false;
  const deep = recursive?.checked ?? false;
  for (const entry of targets) {
    await fsApi.chmod({ fs: options.fs, path: entry.path }, set, clear, deep && entry.isDirectory);
  }
  return true;
}

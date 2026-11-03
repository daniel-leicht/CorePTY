// File manager dialogs on the shared .modal styles: prompts, confirmations,
// the Copy/Move dialog, and the per-file conflict question (B6).

import { icon } from "../icons";
import type { ConflictAnswer, ConflictResolution, ConflictSide, TransferConflict } from "../ipc";
import { formatFileSize, formatModified } from "./format";

export interface ModalButton {
  value: string;
  label: string;
  kind?: "primary" | "ghost" | "danger";
}

/** Ties a dialog to the tab that opened it. */
export interface DialogContext {
  /** Aborted when the tab closes: the dialog is dismissed. */
  signal?: AbortSignal;
  /** Where keyboard focus goes when the element that opened the dialog is gone. */
  fallbackFocus?: () => void;
}

interface ModalOptions extends DialogContext {
  title: string;
  className?: string;
  body: Node;
  buttons: ModalButton[];
  /** Element to focus first; defaults to the primary button. */
  focus?: HTMLElement | null;
  /** Value Enter submits (unless a button or textarea has focus). */
  enterValue?: string;
  role?: "dialog" | "alertdialog";
  /** Extra keys that dismiss the modal (the viewer closes on F3/F10 too). */
  closeKeys?: string[];
}

/** Whether a file manager modal is open (keyboard handlers stand aside). */
export function modalOpen(): boolean {
  return document.querySelector(".modal-backdrop") !== null;
}

/** Browser accelerators that must never fire inside the app (reload, find, caret browsing). */
export function isBrowserAccelerator(event: KeyboardEvent): boolean {
  if (event.altKey && event.key === "F4") return false; // close window: leave it to the OS
  const ctrl = event.ctrlKey || event.metaKey;
  return ["F3", "F5", "F7"].includes(event.key) || (ctrl && event.key.toLowerCase() === "r");
}

/** Show a modal; resolves with the chosen button's value, or null if dismissed. */
export function modal(options: ModalOptions): Promise<string | null> {
  const previous = document.activeElement as HTMLElement | null;
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop fm-modal";
  const dialog = document.createElement("div");
  dialog.className = `modal ${options.className ?? ""}`;
  dialog.setAttribute("role", options.role ?? "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-label", options.title);
  dialog.innerHTML = `
    <div class="modal__head"><div class="modal__title"></div><button class="modal__close" type="button" title="Close (Esc)">${icon("close")}</button></div>
    <div class="modal__body"></div>
    <div class="modal__foot"><span class="spacer"></span></div>`;
  dialog.querySelector(".modal__title")!.textContent = options.title;
  dialog.querySelector(".modal__body")!.appendChild(options.body);
  const foot = dialog.querySelector(".modal__foot")!;
  const buttons = options.buttons.map((spec) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `btn ${spec.kind ?? "ghost"}`;
    button.dataset.value = spec.value;
    button.textContent = spec.label;
    foot.appendChild(button);
    return button;
  });
  backdrop.appendChild(dialog);
  document.body.appendChild(backdrop);

  return new Promise((resolve) => {
    let done = false;
    const finish = (value: string | null) => {
      if (done) return;
      done = true;
      options.signal?.removeEventListener("abort", onAbort);
      backdrop.remove();
      // The opener may be gone (a context-menu item, a dismissed job row).
      if (previous?.isConnected && previous !== document.body) previous.focus({ preventScroll: true });
      else options.fallbackFocus?.();
      resolve(value);
    };
    const onAbort = () => finish(null);
    if (options.signal?.aborted) {
      finish(null);
      return;
    }
    options.signal?.addEventListener("abort", onAbort);
    for (const button of buttons) button.addEventListener("click", () => finish(button.dataset.value!));
    dialog.querySelector(".modal__close")!.addEventListener("click", () => finish(null));
    backdrop.addEventListener("mousedown", (event) => {
      if (event.target === backdrop) finish(null);
    });
    backdrop.addEventListener("keydown", (event) => {
      event.stopPropagation(); // keep keys away from the panels underneath
      if (event.key === "Escape" || options.closeKeys?.includes(event.key)) {
        event.preventDefault();
        finish(null);
      } else if (event.key === "Enter") {
        const target = event.target as HTMLElement;
        if (target instanceof HTMLButtonElement) {
          // Activate the focused button ourselves (exactly once) rather than
          // relying on the browser's synthesized click.
          event.preventDefault();
          target.click();
        } else if (!target.matches("textarea") && options.enterValue !== undefined) {
          event.preventDefault();
          finish(options.enterValue);
        }
      } else if (event.key === "Tab") {
        trapFocus(dialog, event);
      } else if (isBrowserAccelerator(event)) {
        event.preventDefault();
      }
    });
    // The action is the default (Enter confirms, as in Commander), danger included.
    const primary = buttons.find((button) => button.matches(".primary, .danger"));
    (options.focus ?? primary ?? buttons[0])?.focus();
  });
}

function trapFocus(dialog: HTMLElement, event: KeyboardEvent): void {
  const focusable = Array.from(
    dialog.querySelectorAll<HTMLElement>("button, input, select, textarea, [tabindex]:not([tabindex='-1'])")
  ).filter((element) => !element.hasAttribute("disabled") && element.offsetParent !== null);
  if (!focusable.length) return;
  const first = focusable[0];
  const last = focusable[focusable.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

function field(label: string, value: string, hint?: string): { root: HTMLElement; input: HTMLInputElement } {
  const root = document.createElement("label");
  root.className = "field";
  root.innerHTML = `<span class="field__label"></span><input autocomplete="off" spellcheck="false" />`;
  root.querySelector(".field__label")!.textContent = label;
  const input = root.querySelector("input")!;
  input.value = value;
  if (hint) {
    const note = document.createElement("span");
    note.className = "fm-dialog__hint";
    note.textContent = hint;
    root.appendChild(note);
  }
  return { root, input };
}

export async function promptText(
  options: {
    title: string;
    label: string;
    value: string;
    action: string;
    hint?: string;
    /** Select the name without its extension, as file managers do on rename. */
    selectStem?: boolean;
  },
  context: DialogContext = {}
): Promise<string | null> {
  const { root, input } = field(options.label, options.value, options.hint);
  const body = document.createElement("div");
  body.appendChild(root);
  setTimeout(() => {
    const dot = options.value.lastIndexOf(".");
    if (options.selectStem && dot > 0) input.setSelectionRange(0, dot);
    else input.select();
  });
  const result = await modal({
    ...context,
    title: options.title,
    className: "fm-prompt",
    body,
    buttons: [
      { value: "cancel", label: "Cancel" },
      { value: "ok", label: options.action, kind: "primary" },
    ],
    focus: input,
    enterValue: "ok",
  });
  const value = input.value.trim();
  return result === "ok" && value ? value : null;
}

export async function confirmAction(
  options: {
    title: string;
    message: string;
    detail?: string;
    action: string;
    danger?: boolean;
  },
  context: DialogContext = {}
): Promise<boolean> {
  const body = document.createElement("div");
  body.innerHTML = `<p class="fm-confirm__message"></p>`;
  body.querySelector("p")!.textContent = options.message;
  if (options.detail) {
    const detail = document.createElement("p");
    detail.className = "fm-confirm__note";
    detail.textContent = options.detail;
    body.appendChild(detail);
  }
  const result = await modal({
    ...context,
    title: options.title,
    className: "fm-confirm",
    role: "alertdialog",
    body,
    buttons: [
      { value: "cancel", label: "Cancel" },
      { value: "ok", label: options.action, kind: options.danger ? "danger" : "primary" },
    ],
    enterValue: "ok",
  });
  return result === "ok";
}

/** F5/F6: confirm the destination, which can be edited (NC/WinSCP style). */
export function copyMoveDialog(
  options: { kind: "copy" | "move"; what: string; destination: string },
  context: DialogContext = {}
): Promise<string | null> {
  const verb = options.kind === "copy" ? "Copy" : "Move";
  return promptText(
    {
      title: `${verb} ${options.what}`,
      label: "To folder",
      value: options.destination,
      action: verb,
      hint: "Edit the path to choose another folder.",
    },
    context
  );
}

export function maskDialog(mark: boolean, context: DialogContext = {}): Promise<string | null> {
  return promptText(
    {
      title: mark ? "Mark files" : "Unmark files",
      label: "File mask",
      value: "*",
      action: mark ? "Mark" : "Unmark",
      hint: "Wildcards * and ?, several masks separated by ; — folders are not affected.",
    },
    context
  );
}

/**
 * B6: ask what to do about one name clash. The question can pop up while the
 * user is busy with something else, so it starts on the harmless choice (Skip)
 * and dismissing it (Esc, click outside) answers nothing: the transfer keeps
 * waiting and can be answered later. Returns null when dismissed.
 */
export async function conflictDialog(
  conflict: TransferConflict,
  context: DialogContext = {}
): Promise<ConflictAnswer | null> {
  if (conflict.partial) return partialDialog(conflict, context);
  const body = document.createElement("div");
  body.className = "fm-conflict";
  const newer = isNewer(conflict.source, conflict.target);
  body.innerHTML = `
    <p class="fm-confirm__message"></p>
    <div class="fm-conflict__sides">
      ${side("Copying", conflict.source, newer === "source")}
      ${side("Already there", conflict.target, newer === "target")}
    </div>
    <label class="check"><input type="checkbox" class="fm-conflict__all" /> Do this for all remaining conflicts</label>`;
  body.querySelector(".fm-confirm__message")!.textContent =
    `“${conflict.name}” already exists in the destination.`;
  body.querySelectorAll<HTMLElement>(".fm-conflict__path").forEach((element, index) => {
    element.textContent = index === 0 ? conflict.source.path : conflict.target.path;
  });
  const applyToAll = body.querySelector<HTMLInputElement>(".fm-conflict__all")!;

  const pending = modal({
    ...context,
    title: "Replace or skip?",
    className: "fm-confirm fm-conflict-dialog",
    body,
    buttons: [
      { value: "cancel", label: "Cancel transfer" },
      { value: "skip", label: "Skip" },
      { value: "rename", label: "Keep both" },
      { value: "newer", label: "Newer only" },
      { value: "overwrite", label: "Overwrite", kind: "primary" },
    ],
    focus: null,
  });
  // Focus the harmless choice, not the primary action.
  document.querySelector<HTMLButtonElement>('.fm-conflict-dialog [data-value="skip"]')?.focus();
  const choice = await pending;
  let resolution: ConflictResolution;
  switch (choice) {
    case "overwrite":
      resolution = { action: "overwrite" };
      break;
    case "skip":
      resolution = { action: "skip" };
      break;
    case "newer":
      resolution = { action: "overwrite_if_newer" };
      break;
    case "rename":
      // Keep both: the backend picks a free "name (copy N)".
      resolution = { action: "rename" };
      break;
    case "cancel":
      resolution = { action: "cancel" };
      break;
    default:
      return null;
  }
  return { resolution, applyToAll: applyToAll.checked && resolution.action !== "cancel" };
}

/**
 * A partial copy (`<name>.filepart`) is there from a transfer that was cut
 * short: resume it (the default — nothing is lost), start over, or skip.
 */
async function partialDialog(
  conflict: TransferConflict,
  context: DialogContext
): Promise<ConflictAnswer | null> {
  const have = conflict.target.size ?? 0;
  const total = conflict.source.size ?? 0;
  const body = document.createElement("div");
  body.className = "fm-conflict";
  body.innerHTML = `
    <p class="fm-confirm__message"></p>
    <div class="fm-progress fm-conflict__partial" role="progressbar" aria-valuemin="0" aria-valuemax="100"><span></span></div>
    <p class="fm-confirm__note"></p>
    <label class="check"><input type="checkbox" class="fm-conflict__all" /> Do this for all remaining partial copies</label>`;
  body.querySelector(".fm-confirm__message")!.textContent =
    `“${conflict.name}” was partly copied before: ${formatFileSize(have)} of ${formatFileSize(total)}.`;
  body.querySelector(".fm-confirm__note")!.textContent = conflict.target.path;
  const percent = total ? Math.min(100, Math.round((have / total) * 100)) : 0;
  body.querySelector<HTMLElement>(".fm-conflict__partial span")!.style.width = `${percent}%`;
  body.querySelector(".fm-conflict__partial")!.setAttribute("aria-valuenow", String(percent));
  const applyToAll = body.querySelector<HTMLInputElement>(".fm-conflict__all")!;

  const choice = await modal({
    ...context,
    title: "Resume the copy?",
    className: "fm-confirm fm-conflict-dialog",
    body,
    buttons: [
      { value: "cancel", label: "Cancel transfer" },
      { value: "skip", label: "Skip" },
      { value: "overwrite", label: "Start over" },
      { value: "resume", label: "Resume", kind: "primary" },
    ],
  });
  let resolution: ConflictResolution;
  switch (choice) {
    case "resume":
      resolution = { action: "resume" };
      break;
    case "overwrite":
      resolution = { action: "overwrite" };
      break;
    case "skip":
      resolution = { action: "skip" };
      break;
    case "cancel":
      resolution = { action: "cancel" };
      break;
    default:
      return null;
  }
  return { resolution, applyToAll: applyToAll.checked && resolution.action !== "cancel" };
}

function side(label: string, info: ConflictSide, newer: boolean): string {
  const size = info.isDirectory ? "Folder" : formatFileSize(info.size ?? 0);
  return `
    <div class="fm-conflict__side${newer ? " is-newer" : ""}">
      <div class="fm-conflict__label">${label}${newer ? '<span class="fm-row__badge">NEWER</span>' : ""}</div>
      <div class="fm-conflict__meta">${size} · ${formatModified(info.modified) || "unknown date"}</div>
      <div class="fm-conflict__path"></div>
    </div>`;
}

function isNewer(source: ConflictSide, target: ConflictSide): "source" | "target" | null {
  if (source.modified === null || target.modified === null || source.modified === target.modified) {
    return null;
  }
  return source.modified > target.modified ? "source" : "target";
}

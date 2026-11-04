// Commander-style key bindings for the file manager. A pure lookup, so the
// whole table is unit-tested. It deliberately leaves alone what the app binds
// globally (app.ts): Ctrl+Shift+<letter>, Ctrl+Tab, Ctrl+PgUp/PgDn, Ctrl+,.

export type FmCommand =
  | "cursorUp"
  | "cursorDown"
  | "pageUp"
  | "pageDown"
  | "home"
  | "end"
  | "extendUp"
  | "extendDown"
  | "extendPageUp"
  | "extendPageDown"
  | "extendHome"
  | "extendEnd"
  | "open"
  | "parent"
  | "back"
  | "forward"
  | "toggleMarkAdvance"
  | "toggleMark"
  | "markAll"
  | "markMask"
  | "unmarkMask"
  | "invertMarks"
  | "switchPanel"
  | "swapPanels"
  | "root"
  | "refresh"
  | "filter"
  | "clipCopy"
  | "clipCut"
  | "clipPaste"
  | "view"
  | "edit"
  | "copy"
  | "move"
  | "rename"
  | "mkdir"
  | "delete"
  | "deletePermanent"
  | "locationLeft"
  | "locationRight"
  | "properties"
  | "clear";

export interface KeyLike {
  key: string;
  code: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

export function commandFor(event: KeyLike): FmCommand | null {
  const ctrl = event.ctrlKey || event.metaKey;
  const { shiftKey: shift, altKey: alt } = event;
  // Ctrl+Shift combinations belong to the app (new tab, close tab, …).
  if (ctrl && shift) return null;

  switch (event.code) {
    case "NumpadAdd":
      return ctrl || alt ? null : "markMask";
    case "NumpadSubtract":
      return ctrl || alt ? null : "unmarkMask";
    case "NumpadMultiply":
      return ctrl || alt ? null : "invertMarks";
  }

  if (alt && !ctrl) {
    if (event.key === "ArrowUp") return "parent";
    if (event.key === "ArrowLeft") return "back";
    if (event.key === "ArrowRight") return "forward";
    if (event.key === "F1") return "locationLeft";
    if (event.key === "F2") return "locationRight";
    if (event.key === "Enter") return "properties";
    return null;
  }

  if (ctrl) {
    if (event.code === "Backslash") return "root";
    switch (event.key.toLowerCase()) {
      case "a":
        return "markAll";
      case "u":
        return "swapPanels";
      case "r":
        return "refresh";
      case "f":
        return "filter";
      case "c":
        return "clipCopy";
      case "x":
        return "clipCut";
      case "v":
        return "clipPaste";
    }
    return null;
  }

  switch (event.key) {
    case "ArrowUp":
      return shift ? "extendUp" : "cursorUp";
    case "ArrowDown":
      return shift ? "extendDown" : "cursorDown";
    case "PageUp":
      return shift ? "extendPageUp" : "pageUp";
    case "PageDown":
      return shift ? "extendPageDown" : "pageDown";
    case "Home":
      return shift ? "extendHome" : "home";
    case "End":
      return shift ? "extendEnd" : "end";
    case "Enter":
      return "open";
    case "Backspace":
      return "parent";
    case "Insert":
      return "toggleMarkAdvance";
    case " ":
      return "toggleMark";
    case "Tab":
      return "switchPanel";
    case "Escape":
      return "clear";
    case "F2":
      return "rename";
    case "F3":
      return "view";
    case "F4":
      return "edit";
    case "F5":
      return "copy";
    case "F6":
      return shift ? "rename" : "move";
    case "F7":
      return "mkdir";
    case "F8":
    case "Delete":
      return shift ? "deletePermanent" : "delete";
  }
  return null;
}

/** A printable key that should feed quick search (type a name to jump to it). */
export function isQuickSearchKey(event: KeyLike): boolean {
  return (
    event.key.length === 1 &&
    event.key !== " " &&
    !event.ctrlKey &&
    !event.altKey &&
    !event.metaKey &&
    !event.code.startsWith("Numpad")
  );
}

/** The bottom key bar: F-key, label, command. */
export const FUNCTION_KEYS: ReadonlyArray<{ key: string; label: string; command: FmCommand }> = [
  { key: "F3", label: "View", command: "view" },
  { key: "F4", label: "Edit", command: "edit" },
  { key: "F5", label: "Copy", command: "copy" },
  { key: "F6", label: "Move", command: "move" },
  { key: "F7", label: "New folder", command: "mkdir" },
  { key: "F8", label: "Delete", command: "delete" },
];

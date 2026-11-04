import { describe, expect, it } from "vitest";
import { commandFor, isQuickSearchKey, type KeyLike } from "./keymap";

function press(spec: string, code?: string): KeyLike {
  const parts = spec.split("+");
  const key = parts.pop()!;
  return {
    key: key === "Space" ? " " : key,
    code: code ?? (key.length === 1 ? `Key${key.toUpperCase()}` : key),
    ctrlKey: parts.includes("Ctrl"),
    shiftKey: parts.includes("Shift"),
    altKey: parts.includes("Alt"),
    metaKey: false,
  };
}

describe("keymap", () => {
  it.each([
    ["ArrowDown", "cursorDown"],
    ["Shift+ArrowDown", "extendDown"],
    ["PageUp", "pageUp"],
    ["Shift+End", "extendEnd"],
    ["Enter", "open"],
    ["Backspace", "parent"],
    ["Alt+ArrowUp", "parent"],
    ["Alt+ArrowLeft", "back"],
    ["Alt+ArrowRight", "forward"],
    ["Insert", "toggleMarkAdvance"],
    ["Space", "toggleMark"],
    ["Tab", "switchPanel"],
    ["Escape", "clear"],
    ["F2", "rename"],
    ["F3", "view"],
    ["F4", "edit"],
    ["F5", "copy"],
    ["F6", "move"],
    ["Shift+F6", "rename"],
    ["F7", "mkdir"],
    ["F8", "delete"],
    ["Delete", "delete"],
    ["Shift+F8", "deletePermanent"],
    ["Shift+Delete", "deletePermanent"],
    ["Alt+F1", "locationLeft"],
    ["Alt+Enter", "properties"],
    ["Alt+F2", "locationRight"],
    ["Ctrl+a", "markAll"],
    ["Ctrl+u", "swapPanels"],
    ["Ctrl+r", "refresh"],
    ["Ctrl+f", "filter"],
    ["Ctrl+c", "clipCopy"],
    ["Ctrl+x", "clipCut"],
    ["Ctrl+v", "clipPaste"],
  ])("%s → %s", (spec, command) => {
    expect(commandFor(press(spec))).toBe(command);
  });

  it("maps the numpad operators to mask marking", () => {
    expect(commandFor(press("+", "NumpadAdd"))).toBe("markMask");
    expect(commandFor(press("-", "NumpadSubtract"))).toBe("unmarkMask");
    expect(commandFor(press("*", "NumpadMultiply"))).toBe("invertMarks");
    expect(commandFor(press("+", "Equal"))).toBeNull();
  });

  it("maps Ctrl+\\ to the root folder", () => {
    expect(commandFor(press("Ctrl+\\", "Backslash"))).toBe("root");
  });

  it.each(["Ctrl+Shift+t", "Ctrl+Shift+w", "Ctrl+Shift+f", "Ctrl+Tab", "Ctrl+PageUp", "Ctrl+PageDown"])(
    "leaves the app's global shortcut %s alone",
    (spec) => {
      expect(commandFor(press(spec))).toBeNull();
    }
  );

  it("feeds printable keys, but not Space or the numpad, to quick search", () => {
    expect(isQuickSearchKey(press("d"))).toBe(true);
    expect(isQuickSearchKey(press("Shift+D"))).toBe(true);
    expect(isQuickSearchKey(press("Space"))).toBe(false);
    expect(isQuickSearchKey(press("Ctrl+d"))).toBe(false);
    expect(isQuickSearchKey(press("+", "NumpadAdd"))).toBe(false);
    expect(isQuickSearchKey(press("ArrowDown"))).toBe(false);
  });
});

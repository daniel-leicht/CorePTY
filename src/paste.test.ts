import { expect, it, vi } from "vitest";
import { pasteIntoTerminal } from "./paste";

it("routes clipboard text through the terminal paste API", () => {
  const paste = vi.fn();
  pasteIntoTerminal({ paste }, "first\nsecond");
  expect(paste).toHaveBeenCalledOnce();
  expect(paste).toHaveBeenCalledWith("first\nsecond");
});

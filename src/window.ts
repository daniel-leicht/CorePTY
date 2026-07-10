// Thin wrappers over the Tauri window API for the custom (frameless) title bar.
import { getCurrentWindow } from "@tauri-apps/api/window";

// Mirrors the (non-exported) ResizeDirection union from @tauri-apps/api/window.
type ResizeDir =
  | "East"
  | "North"
  | "NorthEast"
  | "NorthWest"
  | "South"
  | "SouthEast"
  | "SouthWest"
  | "West";

const win = () => getCurrentWindow();

export const winMinimize = () => win().minimize();
export const winToggleMaximize = () => win().toggleMaximize();
export const winClose = () => win().close();

export const winStartResize = (dir: string) => win().startResizeDragging(dir as ResizeDir);

/** Toggle the native OS window frame (used per-theme: LCARS is frameless). */
export async function winSetDecorations(on: boolean): Promise<void> {
  try {
    await win().setDecorations(on);
  } catch {
    /* not in a Tauri context */
  }
}

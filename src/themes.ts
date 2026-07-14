// Theme system. A Theme fully defines the look: UI design tokens, the xterm.js
// terminal palette, fonts, and optional CRT-style effects. `applyTheme` writes
// CSS custom properties on <html> and toggles effect classes.
import type { ITheme } from "@xterm/xterm";

export interface ThemeColors {
  bg0: string;
  bg1: string;
  bg2: string;
  bg3: string;
  bg4: string;
  line: string;
  lineSoft: string;
  text0: string;
  text1: string;
  text2: string;
  text3: string;
  accent: string;
  accent2: string;
  ok: string;
  warn: string;
  danger: string;
  /** Corner radius in px (default 10). Retro themes use ~2 for sharp edges. */
  radius?: number;
}

export interface ThemeEffects {
  scanlines?: boolean;
  grid?: boolean;
  glow?: boolean;
}

export interface Theme {
  id: string;
  name: string;
  group: "Modern" | "Classic" | "Retro";
  colors: ThemeColors;
  terminal: ITheme;
  fontUi?: string;
  fontMono?: string;
  /** Multiplier for the terminal font size (default 1). Retro bitmap fonts like
   *  VT323 render small, so a theme can nudge the terminal text up a little. */
  termFontScale?: number;
  /** Preferred terminal cursor style; overrides the user setting when set. */
  cursor?: "bar" | "block" | "underline";
  effects?: ThemeEffects;
  gridColor?: string;
  glowColor?: string;
  /** Remove the native window frame and use a themed custom title bar. */
  frameless?: boolean;
}

export const DEFAULT_UI_FONT = '"Segoe UI", system-ui, -apple-system, "Inter", sans-serif';
export const DEFAULT_MONO_FONT =
  '"Cascadia Code", "Cascadia Mono", "JetBrains Mono", "Consolas", ui-monospace, monospace';

export const THEMES: Theme[] = [
  {
    id: "corepty-dark",
    name: "CorePTY Dark",
    group: "Modern",
    colors: {
      bg0: "#0b0c10", bg1: "#0e0f13", bg2: "#14161d", bg3: "#1b1e27", bg4: "#232734",
      line: "#232634", lineSoft: "#1a1d27",
      text0: "#f2f4fa", text1: "#cdd2df", text2: "#99a1b3", text3: "#6b7385",
      accent: "#7c5cff", accent2: "#22d3ee", ok: "#34d399", warn: "#fbbf24", danger: "#f87171",
    },
    terminal: {
      background: "#0e0f13", foreground: "#cdd2df", cursor: "#7c5cff", cursorAccent: "#0e0f13",
      selectionBackground: "#2b3350", selectionForeground: "#f2f4fa",
      black: "#12141c", red: "#ff6b6b", green: "#5ef2a0", yellow: "#ffd166",
      blue: "#5aa2ff", magenta: "#c792ff", cyan: "#22d3ee", white: "#cdd2df",
      brightBlack: "#5a6274", brightRed: "#ff8b8b", brightGreen: "#8ef7bd", brightYellow: "#ffe08a",
      brightBlue: "#89c2ff", brightMagenta: "#dcb4ff", brightCyan: "#7ee8f7", brightWhite: "#f2f4fa",
    },
  },
  {
    id: "corepty-light",
    name: "CorePTY Light",
    group: "Modern",
    colors: {
      bg0: "#ffffff", bg1: "#f6f7f9", bg2: "#eef0f4", bg3: "#e6e9ef", bg4: "#dbe0ea",
      line: "#d5dae3", lineSoft: "#e6e9ef",
      text0: "#10131a", text1: "#2b3140", text2: "#5b6472", text3: "#8b93a3",
      accent: "#6d5efc", accent2: "#0e9bb0", ok: "#0f9d58", warn: "#c77800", danger: "#dc2626",
    },
    terminal: {
      background: "#f6f7f9", foreground: "#24292e", cursor: "#6d5efc", cursorAccent: "#f6f7f9",
      selectionBackground: "#cdd6ff", selectionForeground: "#10131a",
      black: "#24292e", red: "#d1425b", green: "#22863a", yellow: "#b08800",
      blue: "#2f6feb", magenta: "#8b5cf6", cyan: "#0e7490", white: "#6a737d",
      brightBlack: "#959da5", brightRed: "#cb2431", brightGreen: "#28a745", brightYellow: "#b08800",
      brightBlue: "#2188ff", brightMagenta: "#8a63d2", brightCyan: "#1b7c83", brightWhite: "#2b3140",
    },
  },
  {
    id: "dracula",
    name: "Dracula",
    group: "Classic",
    colors: {
      bg0: "#21222c", bg1: "#282a36", bg2: "#2d2f3d", bg3: "#343746", bg4: "#414458",
      line: "#383a4a", lineSoft: "#2f313f",
      text0: "#f8f8f2", text1: "#dfe0e6", text2: "#a8abbd", text3: "#6272a4",
      accent: "#bd93f9", accent2: "#ff79c6", ok: "#50fa7b", warn: "#f1fa8c", danger: "#ff5555",
    },
    terminal: {
      background: "#282a36", foreground: "#f8f8f2", cursor: "#f8f8f2", cursorAccent: "#282a36",
      selectionBackground: "#44475a", selectionForeground: "#f8f8f2",
      black: "#21222c", red: "#ff5555", green: "#50fa7b", yellow: "#f1fa8c",
      blue: "#bd93f9", magenta: "#ff79c6", cyan: "#8be9fd", white: "#f8f8f2",
      brightBlack: "#6272a4", brightRed: "#ff6e6e", brightGreen: "#69ff94", brightYellow: "#ffffa5",
      brightBlue: "#d6acff", brightMagenta: "#ff92df", brightCyan: "#a4ffff", brightWhite: "#ffffff",
    },
  },
  {
    id: "nord",
    name: "Nord",
    group: "Classic",
    colors: {
      bg0: "#2b303b", bg1: "#2e3440", bg2: "#343b48", bg3: "#3b4252", bg4: "#434c5e",
      line: "#3b4252", lineSoft: "#343b48",
      text0: "#eceff4", text1: "#d8dee9", text2: "#9aa5b8", text3: "#6a7488",
      accent: "#88c0d0", accent2: "#81a1c1", ok: "#a3be8c", warn: "#ebcb8b", danger: "#bf616a",
    },
    terminal: {
      background: "#2e3440", foreground: "#d8dee9", cursor: "#d8dee9", cursorAccent: "#2e3440",
      selectionBackground: "#434c5e", selectionForeground: "#eceff4",
      black: "#3b4252", red: "#bf616a", green: "#a3be8c", yellow: "#ebcb8b",
      blue: "#81a1c1", magenta: "#b48ead", cyan: "#88c0d0", white: "#e5e9f0",
      brightBlack: "#4c566a", brightRed: "#bf616a", brightGreen: "#a3be8c", brightYellow: "#ebcb8b",
      brightBlue: "#81a1c1", brightMagenta: "#b48ead", brightCyan: "#8fbcbb", brightWhite: "#eceff4",
    },
  },
  {
    id: "solarized-dark",
    name: "Solarized Dark",
    group: "Classic",
    colors: {
      bg0: "#00212b", bg1: "#002b36", bg2: "#073642", bg3: "#0d4451", bg4: "#14505e",
      line: "#0d4b5a", lineSoft: "#073642",
      text0: "#eee8d5", text1: "#93a1a1", text2: "#839496", text3: "#586e75",
      accent: "#2aa198", accent2: "#268bd2", ok: "#859900", warn: "#b58900", danger: "#dc322f",
    },
    terminal: {
      background: "#002b36", foreground: "#839496", cursor: "#93a1a1", cursorAccent: "#002b36",
      selectionBackground: "#073642", selectionForeground: "#93a1a1",
      black: "#073642", red: "#dc322f", green: "#859900", yellow: "#b58900",
      blue: "#268bd2", magenta: "#d33682", cyan: "#2aa198", white: "#eee8d5",
      brightBlack: "#586e75", brightRed: "#cb4b16", brightGreen: "#657b83", brightYellow: "#839496",
      brightBlue: "#839496", brightMagenta: "#6c71c4", brightCyan: "#93a1a1", brightWhite: "#fdf6e3",
    },
  },
  {
    // Ported from esper-theme/bbs — 1990s ANSI bulletin board on a scanlined CRT.
    id: "bbs",
    name: "BBS",
    group: "Retro",
    colors: {
      bg0: "#030408", bg1: "#05060a", bg2: "#080a12", bg3: "#0d1220", bg4: "#131a2c",
      line: "#1f7e96", lineSoft: "#123c47",
      text0: "#eaf2f6", text1: "#9fb0bd", text2: "#7f909d", text3: "#586573",
      accent: "#3fe0ff", accent2: "#5b76ff", ok: "#4dff84", warn: "#ffe14d", danger: "#ff5d5d",
      radius: 0,
    },
    fontUi: '"VT323", "Cascadia Mono", "Consolas", ui-monospace, monospace',
    fontMono: '"VT323", "Cascadia Mono", "Consolas", ui-monospace, monospace',
    termFontScale: 1.2,
    cursor: "block",
    effects: { scanlines: true, glow: true },
    glowColor: "rgba(120, 200, 220, 0.5)",
    terminal: {
      background: "#080a12", foreground: "#9fb0bd", cursor: "#3fe0ff", cursorAccent: "#03121a",
      selectionBackground: "#0f4a5a", selectionForeground: "#eaf2f6",
      black: "#05060a", red: "#ff5d5d", green: "#4dff84", yellow: "#ffe14d",
      blue: "#5b76ff", magenta: "#ff67d4", cyan: "#3fe0ff", white: "#9fb0bd",
      brightBlack: "#586573", brightRed: "#ff8a8a", brightGreen: "#7dffab", brightYellow: "#fff08a",
      brightBlue: "#8a9dff", brightMagenta: "#ff9ae2", brightCyan: "#7debff", brightWhite: "#eaf2f6",
    },
  },
  {
    // Ported from esper-theme/synapse — 80s synthwave "NEURAL_OS", neon on violet.
    id: "synapse",
    name: "Synapse",
    group: "Retro",
    colors: {
      bg0: "#140727", bg1: "#190c2d", bg2: "#211536", bg3: "#26193a", bg4: "#302445",
      line: "#564052", lineSoft: "#3c2e50",
      text0: "#ecdcff", text1: "#dcbed4", text2: "#a4899d", text3: "#7a6b86",
      accent: "#ff00ff", accent2: "#00fbfb", ok: "#00ff41", warn: "#cdcd00", danger: "#ff6b8b",
      radius: 0,
    },
    // Body voice is Space Mono (source: --font-body); Space Grotesk is reserved
    // for display type (brand / modal titles) via CSS overrides.
    fontUi: '"Space Mono", "JetBrains Mono", ui-monospace, "Cascadia Mono", monospace',
    fontMono: '"JetBrains Mono", "Space Mono", ui-monospace, "Cascadia Mono", monospace',
    cursor: "underline",
    effects: { scanlines: true, grid: true, glow: true },
    gridColor: "rgba(0, 251, 251, 0.045)",
    glowColor: "rgba(255, 0, 255, 0.45)",
    terminal: {
      background: "#190c2d", foreground: "#ecdcff", cursor: "#ff00ff", cursorAccent: "#190c2d",
      selectionBackground: "#54006e", selectionForeground: "#ffffff",
      black: "#211536", red: "#ff5d8f", green: "#00ff41", yellow: "#cdcd00",
      blue: "#8b5cff", magenta: "#ff00ff", cyan: "#00fbfb", white: "#ecdcff",
      brightBlack: "#a4899d", brightRed: "#ffabab", brightGreen: "#6bff9a", brightYellow: "#ffff66",
      brightBlue: "#b0a0ff", brightMagenta: "#ffabf3", brightCyan: "#7ffcfc", brightWhite: "#ffffff",
    },
  },
  {
    // Ported from esper-theme/starbase — an LCARS-style starship console.
    id: "starbase",
    name: "Starbase",
    group: "Retro",
    colors: {
      // Warm near-blacks (the LCARS "#0b0805" field family), sand/muted text.
      bg0: "#000000", bg1: "#000000", bg2: "#0b0805", bg3: "#14100a", bg4: "#211a12",
      line: "#2a1c10", lineSoft: "#1a1206",
      text0: "#ffe8d6", text1: "#ffcc99", text2: "#cc8866", text3: "#8a6a54",
      accent: "#ff9966", accent2: "#cc99cc", ok: "#66cc66", warn: "#ffcc00", danger: "#cc4444",
      radius: 18,
    },
    fontUi: '"Antonio", "Oswald", "Arial Narrow", system-ui, sans-serif',
    frameless: true,
    terminal: {
      background: "#000000", foreground: "#ff9966", cursor: "#ffcc00", cursorAccent: "#000000",
      selectionBackground: "#5a3a1a", selectionForeground: "#ffe8d6",
      black: "#000000", red: "#cc4444", green: "#66cc66", yellow: "#ffcc00",
      blue: "#6699cc", magenta: "#cc6699", cyan: "#99ccff", white: "#ffcc99",
      brightBlack: "#8a6a54", brightRed: "#ff6a6a", brightGreen: "#99ff99", brightYellow: "#ffdd44",
      brightBlue: "#8fbfff", brightMagenta: "#cc99cc", brightCyan: "#bfe4ff", brightWhite: "#ffe8d6",
    },
  },
];

let active: Theme = THEMES[0];

export function activeTheme(): Theme {
  return active;
}

export function themeById(id: string): Theme {
  return THEMES.find((t) => t.id === id) ?? THEMES[0];
}

/** Convert `#rrggbb` + alpha to an rgba() string. */
export function rgba(hex: string, a: number): string {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return `rgba(124,92,255,${a})`;
  const n = parseInt(m[1], 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}

/** Apply a theme: write CSS vars on <html> + toggle effect classes. */
export function applyTheme(id: string): Theme {
  const t = themeById(id);
  const root = document.documentElement;
  const s = root.style;
  const c = t.colors;

  s.setProperty("--bg-0", c.bg0);
  s.setProperty("--bg-1", c.bg1);
  s.setProperty("--bg-2", c.bg2);
  s.setProperty("--bg-3", c.bg3);
  s.setProperty("--bg-4", c.bg4);
  s.setProperty("--line", c.line);
  s.setProperty("--line-soft", c.lineSoft);
  s.setProperty("--text-0", c.text0);
  s.setProperty("--text-1", c.text1);
  s.setProperty("--text-2", c.text2);
  s.setProperty("--text-3", c.text3);
  s.setProperty("--accent", c.accent);
  s.setProperty("--accent-2", c.accent2);
  s.setProperty("--accent-soft", rgba(c.accent, 0.16));
  s.setProperty("--accent-ring", rgba(c.accent, 0.45));
  s.setProperty("--grad-accent", `linear-gradient(135deg, ${c.accent}, ${c.accent2})`);
  s.setProperty("--ok", c.ok);
  s.setProperty("--warn", c.warn);
  s.setProperty("--danger", c.danger);

  const r = c.radius ?? 10;
  s.setProperty("--radius-sm", `${Math.max(0, r - 4)}px`);
  s.setProperty("--radius", `${r}px`);
  s.setProperty("--radius-lg", `${r <= 1 ? r : r + 4}px`);

  s.setProperty("--font-ui", t.fontUi ?? DEFAULT_UI_FONT);
  s.setProperty("--font-mono", t.fontMono ?? DEFAULT_MONO_FONT);

  s.setProperty("--grid-line", t.gridColor ?? rgba(c.accent2, 0.05));
  s.setProperty("--glow-tint", t.glowColor ?? rgba(c.accent, 0.35));
  s.setProperty("--glow", `0 0 14px ${rgba(c.accent, 0.45)}`);
  s.setProperty("--glow-2", `0 0 14px ${rgba(c.accent2, 0.4)}`);

  root.classList.remove(`theme-${active.id}`);
  root.classList.add(`theme-${t.id}`);
  root.classList.toggle("fx-scanlines", !!t.effects?.scanlines);
  root.classList.toggle("fx-grid", !!t.effects?.grid);
  root.classList.toggle("fx-glow", !!t.effects?.glow);
  root.classList.toggle("frameless", !!t.frameless);

  active = t;
  return t;
}

/**
 * Preload every font the themes use and resolve once they're available.
 *
 * `@font-face` fonts load lazily, but xterm measures the glyph cell from the
 * *live* font — measuring before the font loads sizes the cell to a fallback
 * face, which then mis-maps mouse selection to the wrong column and mis-counts
 * rows (the last one spills under the status bar). Call this before opening the
 * first terminal so its measurement is correct from the start; later theme
 * switches re-measure against these now-loaded fonts.
 */
export async function preloadThemeFonts(): Promise<void> {
  const set = typeof document !== "undefined" ? document.fonts : undefined;
  if (!set?.load) return;
  const families = new Set<string>();
  const add = (stack?: string): void => {
    const first = stack?.split(",")[0]?.replace(/["']/g, "").trim();
    if (first) families.add(first);
  };
  add(DEFAULT_MONO_FONT);
  add(DEFAULT_UI_FONT);
  for (const t of THEMES) {
    add(t.fontMono);
    add(t.fontUi);
  }
  await Promise.all(
    [...families].map((f) => set.load(`16px "${f}"`).catch(() => undefined))
  );
}

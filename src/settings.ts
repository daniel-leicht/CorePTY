// User settings: schema, persistence (settings.json in config dir), and the
// terminal option derivation. Appearance (colors/fonts/effects) comes from the
// selected theme; see themes.ts.
import type { ITerminalOptions } from "@xterm/xterm";
import { api, type LocalShell } from "./ipc";
import { activeTheme, applyTheme, DEFAULT_MONO_FONT, THEMES } from "./themes";

export interface Settings {
  theme: string;
  /** Terminal font override; blank = use the theme's mono font. */
  fontFamily: string;
  fontSize: number;
  lineHeight: number;
  cursorStyle: "bar" | "block" | "underline";
  cursorBlink: boolean;
  scrollback: number;
  copyOnSelect: boolean;
  rightClick: "paste" | "menu";
  defaultShell: LocalShell;
  bell: "none" | "visual" | "sound";
  /** Auto-adjust unreadable text to a minimum contrast against its cell background. */
  minContrast: "off" | "standard" | "high";
}

export const DEFAULTS: Settings = {
  theme: "corepty-dark",
  fontFamily: "",
  fontSize: 13.5,
  lineHeight: 1.2,
  cursorStyle: "bar",
  cursorBlink: true,
  scrollback: 10000,
  copyOnSelect: true,
  rightClick: "paste",
  defaultShell: "powershell",
  bell: "visual",
  minContrast: "standard",
};

/** Live settings object. Mutated in place so importers see updates. */
export const current: Settings = { ...DEFAULTS };

export function termOptions(): ITerminalOptions {
  const t = activeTheme();
  return {
    fontFamily: current.fontFamily.trim() || t.fontMono || DEFAULT_MONO_FONT,
    // Some themes render small (e.g. BBS's VT323), so allow a per-theme nudge —
    // relative to the user's chosen size, rounded to the nearest half-pixel.
    fontSize: Math.round(current.fontSize * (t.termFontScale ?? 1) * 2) / 2,
    lineHeight: current.lineHeight,
    cursorStyle: t.cursor ?? current.cursorStyle,
    cursorBlink: current.cursorBlink,
    cursorWidth: 2,
    scrollback: current.scrollback,
    fontWeight: 400,
    fontWeightBold: 600,
    allowProposedApi: true,
    drawBoldTextInBrightColors: true,
    // Force a floor on foreground/background contrast per cell, so low-contrast
    // color pairs (e.g. nano's white-on-beige footer keys) stay readable.
    minimumContrastRatio: contrastRatio(current.minContrast),
    macOptionIsMeta: true,
    theme: t.terminal,
  };
}

export async function loadSettings(): Promise<void> {
  try {
    const raw = (await api.settingsLoad()) ?? {};
    Object.assign(current, DEFAULTS, sanitize(raw));
  } catch {
    Object.assign(current, DEFAULTS);
  }
  applyTheme(current.theme);
}

export async function persistSettings(): Promise<void> {
  try {
    await api.settingsSave(current as unknown as Record<string, unknown>);
  } catch {
    /* non-fatal */
  }
}

/** Switch theme, apply it, and persist. */
export function setTheme(id: string): void {
  current.theme = id;
  applyTheme(id);
  void persistSettings();
}

function sanitize(raw: Record<string, unknown>): Partial<Settings> {
  const out: Partial<Settings> = {};
  const num = (v: unknown) => (typeof v === "number" && isFinite(v) ? v : undefined);
  const bool = (v: unknown) => (typeof v === "boolean" ? v : undefined);
  if (typeof raw.theme === "string" && THEMES.some((t) => t.id === raw.theme))
    out.theme = raw.theme;
  if (typeof raw.fontFamily === "string") out.fontFamily = raw.fontFamily;
  if (num(raw.fontSize)) out.fontSize = clamp(raw.fontSize as number, 8, 32);
  if (num(raw.lineHeight)) out.lineHeight = clamp(raw.lineHeight as number, 1, 2.5);
  if (raw.cursorStyle === "bar" || raw.cursorStyle === "block" || raw.cursorStyle === "underline")
    out.cursorStyle = raw.cursorStyle;
  if (bool(raw.cursorBlink) !== undefined) out.cursorBlink = raw.cursorBlink as boolean;
  if (num(raw.scrollback)) out.scrollback = clamp(raw.scrollback as number, 100, 200000);
  if (bool(raw.copyOnSelect) !== undefined) out.copyOnSelect = raw.copyOnSelect as boolean;
  if (raw.rightClick === "paste" || raw.rightClick === "menu") out.rightClick = raw.rightClick;
  if (["cmd", "powershell", "pwsh", "bash"].includes(raw.defaultShell as string))
    out.defaultShell = raw.defaultShell as LocalShell;
  if (raw.bell === "none" || raw.bell === "visual" || raw.bell === "sound") out.bell = raw.bell;
  if (raw.minContrast === "off" || raw.minContrast === "standard" || raw.minContrast === "high")
    out.minContrast = raw.minContrast;
  return out;
}

function clamp(n: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, n));
}

/** Map the friendly contrast level to an xterm minimumContrastRatio (1 = off). */
function contrastRatio(level: Settings["minContrast"]): number {
  return level === "high" ? 7 : level === "off" ? 1 : 4.5;
}

let audioCtx: AudioContext | null = null;

/** Handle a terminal bell according to the current setting. */
export function ringBell(el: HTMLElement): void {
  if (current.bell === "visual") {
    el.classList.remove("bell");
    void el.offsetWidth;
    el.classList.add("bell");
  } else if (current.bell === "sound") {
    try {
      audioCtx ??= new AudioContext();
      const osc = audioCtx.createOscillator();
      const gain = audioCtx.createGain();
      osc.frequency.value = 880;
      gain.gain.value = 0.05;
      osc.connect(gain).connect(audioCtx.destination);
      osc.start();
      osc.stop(audioCtx.currentTime + 0.08);
    } catch {
      /* ignore */
    }
  }
}

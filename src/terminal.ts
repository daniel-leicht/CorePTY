// A single terminal instance: xterm.js + addons wired to a backend session.
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebglAddon } from "@xterm/addon-webgl";
import { api, type SessionInfo, type SessionKind } from "./ipc";
import { icon } from "./icons";
import { ringBell, termOptions } from "./settings";
import type { LaunchSpec } from "./spec";
import { SgrDimFilter } from "./dimfix";
import { escapeHtml, uuid } from "./util";

export type SessionStatus = "connecting" | "connected" | "exited" | "error";

export class TerminalSession {
  info: SessionInfo | null = null;
  status: SessionStatus = "connecting";
  alive = true;
  readonly element: HTMLDivElement;
  readonly term: Terminal;
  private readonly fitAddon: FitAddon;
  readonly search: SearchAddon;
  private opened = false;
  private overlayEl: HTMLDivElement | null = null;
  private repaintTimer: number | null = null;
  /** Rewrites SGR dim → grey (WebGL doesn't render the faint attribute). */
  private readonly dimFilter = new SgrDimFilter();
  /** Re-fits when the element gains/changes size (e.g. becomes visible). */
  private sizeObserver: ResizeObserver | null = null;
  private fitRaf = 0;

  kind: SessionKind;
  iconName: string;
  /** Stable client-side tab identity, independent of the backend session id. */
  readonly uid = uuid();
  /** How to relaunch this tab (set by the app). */
  spec: LaunchSpec | null = null;
  /** Whether this tab runs elevated (Administrator). */
  elevated = false;
  /** Which xterm renderer is live: "webgl" once the addon loads, else "dom". */
  renderer: "webgl" | "dom" = "dom";

  /** Default label (shell name / user@host), set at creation and on attach. */
  private baseTitle: string;
  /** Latest OSC 0/2 title set by the program, shell prompt, or remote host. */
  osTitle = "";
  /** A manually pinned name. Once set it overrides everything, permanently. */
  private customTitle: string | null = null;

  onTitleUpdate?: () => void;
  onExit?: (code: number | null) => void;
  onStatusChange?: () => void;
  onReconnect?: () => void;
  onClose?: () => void;

  constructor(kind: SessionKind, title: string, iconName = "terminal") {
    this.kind = kind;
    this.baseTitle = title;
    this.iconName = iconName;

    this.element = document.createElement("div");
    this.element.className = "term";

    this.term = new Terminal(termOptions(kind));
    this.fitAddon = new FitAddon();
    this.search = new SearchAddon();
    this.term.loadAddon(this.fitAddon);
    this.term.loadAddon(new WebLinksAddon());
    this.term.loadAddon(this.search);
    // Use Unicode 11 character widths so xterm agrees with what modern CLIs
    // (Node/Ink — e.g. Claude Code) assume; a width mismatch desyncs the cursor
    // and leaves stray blank cells while editing.
    this.term.loadAddon(new Unicode11Addon());
    this.term.unicode.activeVersion = "11";

    this.term.onData((d) => {
      if (this.info && this.alive) void api.write(this.info.id, d);
    });
    this.term.onResize(({ cols, rows }) => {
      if (this.info && this.alive) void api.resize(this.info.id, cols, rows);
    });
    // OSC 0/2 title from the program (e.g. Claude Code, a shell prompt) or, over
    // SSH / Telnet, from the remote shell. This drives the tab name — unless the
    // user has pinned a manual one (see `title` / `setCustomTitle`).
    this.term.onTitleChange((t) => {
      this.osTitle = t || "";
      this.onTitleUpdate?.();
    });
    this.term.onBell(() => ringBell(this.element));
    // Scrolling the scrollback (mouse wheel) exposes many rows at once; the WebGL
    // renderer can surface stale/partial cells from earlier in-place redraws
    // ("broken parts" of the session). Repaint once the scroll settles.
    this.term.onScroll(() => this.scheduleRepaint());
  }

  get id(): string | null {
    return this.info?.id ?? null;
  }

  /**
   * Effective tab label. A user-pinned name wins over everything; otherwise the
   * program-set OSC title wins over the default label.
   */
  get title(): string {
    return this.customTitle ?? (this.osTitle || this.baseTitle);
  }

  /** Whether the user pinned a manual name (which locks out OSC/default titles). */
  get pinned(): boolean {
    return this.customTitle !== null;
  }

  /** Pin a manual tab name; empty / null clears the pin (back to OSC/default). */
  setCustomTitle(name: string | null): void {
    const trimmed = name?.trim() ?? "";
    this.customTitle = trimmed || null;
    this.onTitleUpdate?.();
  }

  open(): void {
    if (this.opened) return;
    this.term.open(this.element);
    this.opened = true;
    // Load the WebGL renderer *after* open() (it needs the screen element). It
    // draws every cell on a fixed integer grid, which eliminates the sub-pixel
    // column drift the DOM renderer suffers with webfonts whose glyph advance
    // isn't a whole number of pixels — most visible with VT323 (BBS theme) and
    // heavy syntax highlighting. Falls back to the DOM renderer if WebGL is
    // unavailable or its GPU context is lost.
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => {
        try {
          webgl.dispose();
        } catch {
          /* ignore — xterm reverts to the DOM renderer */
        }
        this.renderer = "dom";
        this.onStatusChange?.();
      });
      this.term.loadAddon(webgl);
      this.renderer = "webgl";
    } catch {
      /* WebGL unavailable — keep the DOM renderer. */
    }
    console.info(`[corepty] terminal renderer: ${this.renderer}`);
    // Re-fit whenever this terminal's box changes size — most importantly when
    // it first gains a size (a tab opened right after launch, before the stage
    // had laid out, would otherwise fit to 0 and come up blank). Debounced to a
    // frame so a burst of layout changes collapses to one fit.
    this.sizeObserver = new ResizeObserver(() => {
      if (this.fitRaf) cancelAnimationFrame(this.fitRaf);
      this.fitRaf = requestAnimationFrame(() => this.fit());
    });
    this.sizeObserver.observe(this.element);
    this.fit();
  }

  attach(info: SessionInfo): void {
    this.info = info;
    this.baseTitle = info.title || this.baseTitle;
  }

  setStatus(status: SessionStatus): void {
    this.status = status;
    this.onStatusChange?.();
  }

  /** Re-apply the current user settings to this terminal. */
  applySettings(): void {
    const o = termOptions();
    const t = this.term.options;
    t.fontFamily = o.fontFamily;
    t.fontSize = o.fontSize;
    t.letterSpacing = o.letterSpacing;
    t.lineHeight = o.lineHeight;
    t.cursorStyle = o.cursorStyle;
    t.cursorBlink = o.cursorBlink;
    t.scrollback = o.scrollback;
    t.minimumContrastRatio = o.minimumContrastRatio;
    t.theme = o.theme;
    this.fit();
  }

  fit(): void {
    if (!this.opened) return;
    // Don't fit a hidden / not-yet-laid-out terminal: fitAddon would compute 0
    // rows/cols and the session would come up blank until the next resize.
    if (this.element.clientWidth === 0 || this.element.clientHeight === 0) return;
    try {
      this.fitAddon.fit();
    } catch {
      /* not measurable yet */
    }
  }

  dims(): { cols: number; rows: number } {
    return { cols: this.term.cols, rows: this.term.rows };
  }

  writeBytes(bytes: Uint8Array): void {
    this.term.write(this.dimFilter.feed(bytes));
    this.scheduleRepaint();
  }

  /**
   * Belt-and-braces full repaint once output settles. During a burst of
   * cursor-relative redraws (e.g. Claude Code's input box on ConPTY) the WebGL
   * renderer can leave a single cell mispainted — a swallowed or doubled space —
   * that only corrects itself when something later marks that row dirty. That's
   * the "spaces that fix themselves after a few seconds". Forcing a refresh a
   * beat after the last write collapses that window to ~120 ms. Trailing-edge
   * debounced, so a continuous stream (which repaints every cell anyway) doesn't
   * pay for it until it pauses.
   */
  private scheduleRepaint(): void {
    if (this.repaintTimer !== null) clearTimeout(this.repaintTimer);
    this.repaintTimer = window.setTimeout(() => {
      this.repaintTimer = null;
      try {
        this.term.refresh(0, this.term.rows - 1);
      } catch {
        /* terminal disposed — nothing to repaint */
      }
    }, 120);
  }

  focus(): void {
    this.term.focus();
  }

  hasSelection(): boolean {
    return this.term.hasSelection();
  }

  getSelection(): string {
    return this.term.getSelection();
  }

  markExited(code: number | null): void {
    this.alive = false;
    this.setStatus("exited");
    const label =
      code === 0 || code === null ? "Session ended" : `Session ended · exit ${code}`;
    this.term.write(`\r\n\x1b[38;2;107;115;133m╶╴ ${label} ╶╴\x1b[0m\r\n`);
    this.showOverlay("exit", label);
    this.onExit?.(code);
  }

  markError(message: string): void {
    this.alive = false;
    this.setStatus("error");
    this.term.write(`\r\n\x1b[38;2;248;113;113m✖ ${message}\x1b[0m\r\n`);
    this.showOverlay("error", message);
  }

  /** Reset for a reconnect: clear overlay, revive, mark connecting. */
  beginReconnect(): void {
    this.hideOverlay();
    this.info = null;
    this.alive = true;
    this.setStatus("connecting");
    this.term.write("\r\n\x1b[38;2;124;92;255m↻ reconnecting…\x1b[0m\r\n");
  }

  private showOverlay(kind: "exit" | "error", message: string): void {
    this.hideOverlay();
    const canReconnect = !!this.spec;
    const o = document.createElement("div");
    o.className = `term-overlay term-overlay--${kind}`;
    o.innerHTML = `
      <div class="term-overlay__card">
        <div class="term-overlay__msg">${escapeHtml(message)}</div>
        <div class="term-overlay__actions">
          ${
            canReconnect
              ? `<button class="btn primary" data-act="reconnect">${icon("refresh")} Reconnect</button>`
              : ""
          }
          <button class="btn ghost" data-act="close">Close tab</button>
        </div>
      </div>`;
    o.querySelector('[data-act="reconnect"]')?.addEventListener("click", () => this.onReconnect?.());
    o.querySelector('[data-act="close"]')?.addEventListener("click", () => this.onClose?.());
    this.element.appendChild(o);
    this.overlayEl = o;
  }

  private hideOverlay(): void {
    this.overlayEl?.remove();
    this.overlayEl = null;
  }

  dispose(): void {
    if (this.repaintTimer !== null) {
      clearTimeout(this.repaintTimer);
      this.repaintTimer = null;
    }
    if (this.fitRaf) cancelAnimationFrame(this.fitRaf);
    this.sizeObserver?.disconnect();
    this.sizeObserver = null;
    try {
      this.term.dispose();
    } catch {
      /* ignore */
    }
    this.element.remove();
  }
}

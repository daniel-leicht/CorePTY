// Settings modal. Changes apply live and persist immediately.
import { icon } from "./icons";
import {
  current,
  DEFAULTS,
  effectiveDefaultShell,
  localShells,
  persistSettings,
  setTheme,
  type Settings,
} from "./settings";
import { activeTheme, THEMES } from "./themes";

export class SettingsDialog {
  private el: HTMLElement | null = null;
  constructor(private onChange: () => void) {}

  open(): void {
    this.close();
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";
    backdrop.innerHTML = this.template();
    document.body.appendChild(backdrop);
    this.el = backdrop;
    this.wire(backdrop);
    backdrop.addEventListener("mousedown", (e) => {
      if (e.target === backdrop) this.close();
    });
    document.addEventListener("keydown", this.escHandler, true);
  }

  private escHandler = (e: KeyboardEvent) => {
    if (e.key === "Escape") this.close();
  };

  private close(): void {
    this.el?.remove();
    this.el = null;
    document.removeEventListener("keydown", this.escHandler, true);
  }

  private template(): string {
    const s = current;
    const themeCursor = activeTheme().cursor;
    const effCursor = themeCursor ?? s.cursorStyle;
    const cursorHint = themeCursor ? ` <span class="muted">· ${themeCursor} (theme)</span>` : "";
    return `
      <div class="modal modal--wide" role="dialog" aria-modal="true">
        <div class="modal__head">
          <div class="modal__title">${icon("settings")} Settings</div>
          <button class="modal__close" data-act="close">${icon("close")}</button>
        </div>
        <div class="modal__body">
          <div class="settings-section">Theme</div>
          ${themeGrid(s.theme)}

          <div class="settings-section">Appearance</div>
          <label class="field">
            <span class="field__label">Terminal font <span class="muted">(blank = theme default)</span></span>
            <input data-role="fontFamily" value="${escAttr(s.fontFamily)}" placeholder="e.g. Cascadia Code" spellcheck="false" />
          </label>
          <div class="grid2">
            <label class="field">
              <span class="field__label">Font size <span data-fslabel>${s.fontSize}px</span></span>
              <input type="range" min="9" max="24" step="0.5" data-role="fontSize" value="${s.fontSize}" />
            </label>
            <label class="field">
              <span class="field__label">Line height <span data-lhlabel>${s.lineHeight}</span></span>
              <input type="range" min="1" max="2" step="0.05" data-role="lineHeight" value="${s.lineHeight}" />
            </label>
          </div>
          <div class="grid2">
            <div class="field">
              <span class="field__label">Cursor${cursorHint}</span>
              <div class="seg ${themeCursor ? "is-locked" : ""}" data-role="cursorStyle">${seg(["bar", "block", "underline"], effCursor)}</div>
            </div>
            <label class="check cursor-blink">
              <input type="checkbox" data-role="cursorBlink" ${s.cursorBlink ? "checked" : ""} />
              <span>Blink cursor</span>
            </label>
          </div>

          <div class="settings-section">Terminal</div>
          <div class="grid2">
            <label class="field">
              <span class="field__label">Scrollback (lines)</span>
              <input type="number" min="100" max="200000" step="100" data-role="scrollback" value="${s.scrollback}" />
            </label>
            <label class="field">
              <span class="field__label">Default shell</span>
              <select data-role="defaultShell" class="select">${shellOpts(effectiveDefaultShell())}</select>
            </label>
          </div>
          <div class="grid2">
            <div class="field">
              <span class="field__label">Bell</span>
              <div class="seg" data-role="bell">${seg(["none", "visual", "sound"], s.bell)}</div>
            </div>
            <div class="field">
              <span class="field__label">Right-click</span>
              <div class="seg" data-role="rightClick">${seg(["paste", "menu"], s.rightClick)}</div>
            </div>
          </div>
          <div class="field">
            <span class="field__label">Minimum contrast <span class="muted">· auto-adjust hard-to-read text</span></span>
            <div class="seg" data-role="minContrast">${seg(["off", "standard", "high"], s.minContrast)}</div>
          </div>
          <label class="check">
            <input type="checkbox" data-role="copyOnSelect" ${s.copyOnSelect ? "checked" : ""} />
            <span>Copy on selection (PuTTY-style)</span>
          </label>
        </div>
        <div class="modal__foot">
          <button type="button" class="btn ghost" data-act="reset">Reset to defaults</button>
          <div class="spacer"></div>
          <button type="button" class="btn primary" data-act="close">Done</button>
        </div>
      </div>`;
  }

  private wire(root: HTMLElement): void {
    const apply = () => {
      void persistSettings();
      this.onChange();
    };
    const set = <K extends keyof Settings>(k: K, v: Settings[K]) => {
      current[k] = v;
      apply();
    };

    root.querySelectorAll<HTMLElement>("[data-role]").forEach((elm) => {
      const role = elm.dataset.role as keyof Settings;
      if (elm instanceof HTMLInputElement && elm.type === "range") {
        elm.addEventListener("input", () => {
          const num = parseFloat(elm.value);
          set(role, num as Settings[typeof role]);
          if (role === "fontSize") root.querySelector("[data-fslabel]")!.textContent = `${num}px`;
          if (role === "lineHeight") root.querySelector("[data-lhlabel]")!.textContent = `${num}`;
        });
      } else if (elm instanceof HTMLInputElement && elm.type === "number") {
        elm.addEventListener("change", () =>
          set(role, (parseInt(elm.value, 10) || 100) as Settings[typeof role])
        );
      } else if (elm instanceof HTMLInputElement && elm.type === "checkbox") {
        elm.addEventListener("change", () => set(role, elm.checked as Settings[typeof role]));
      } else if (elm instanceof HTMLInputElement || elm instanceof HTMLSelectElement) {
        elm.addEventListener("change", () => set(role, elm.value as Settings[typeof role]));
      } else if (elm.classList.contains("seg")) {
        elm.querySelectorAll<HTMLButtonElement>("button").forEach((btn) => {
          btn.addEventListener("click", () => {
            elm.querySelectorAll("button").forEach((b) => b.classList.remove("is-active"));
            btn.classList.add("is-active");
            set(role, btn.dataset.val as Settings[typeof role]);
          });
        });
      }
    });

    // theme cards
    root.querySelectorAll<HTMLButtonElement>(".theme-card").forEach((card) => {
      card.addEventListener("click", () => {
        setTheme(card.dataset.theme!);
        this.onChange();
        this.open(); // re-open so the modal restyles + the cursor lock re-syncs
      });
    });

    root.querySelectorAll<HTMLElement>('[data-act="close"]').forEach((b) =>
      b.addEventListener("click", () => this.close())
    );
    root.querySelector('[data-act="reset"]')?.addEventListener("click", () => {
      Object.assign(current, DEFAULTS);
      setTheme(current.theme);
      this.onChange();
      this.open();
    });
  }
}

function themeGrid(activeId: string): string {
  const groups = ["Modern", "Classic", "Retro"] as const;
  return groups
    .map((g) => {
      const items = THEMES.filter((t) => t.group === g);
      if (!items.length) return "";
      return `<div class="theme-group">${g}</div>
      <div class="theme-grid">
        ${items
          .map((t) => {
            const c = t.colors;
            const tm = t.terminal;
            return `<button type="button" class="theme-card ${t.id === activeId ? "is-active" : ""}" data-theme="${t.id}">
            <span class="theme-card__pv" style="background:${c.bg1};border-color:${c.line}">
              <span class="theme-card__txt" style="color:${c.text1}">Aa</span>
              <span class="theme-card__dots">
                <i style="background:${c.accent}"></i><i style="background:${tm.red}"></i><i style="background:${tm.green}"></i><i style="background:${tm.yellow}"></i><i style="background:${tm.cyan}"></i><i style="background:${c.accent2}"></i>
              </span>
            </span>
            <span class="theme-card__name">${t.name}</span>
          </button>`;
          })
          .join("")}
      </div>`;
    })
    .join("");
}

function seg(values: string[], active: string): string {
  return values
    .map(
      (v) =>
        `<button type="button" data-val="${v}" class="${v === active ? "is-active" : ""}">${cap(v)}</button>`
    )
    .join("");
}

function shellOpts(active: string): string {
  return localShells
    .map(
      (s) => `<option value="${s.id}" ${s.id === active ? "selected" : ""}>${escAttr(s.label)}</option>`
    )
    .join("");
}

function cap(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function escAttr(s: string): string {
  return s.replace(/[&<>"]/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : "&quot;"
  );
}

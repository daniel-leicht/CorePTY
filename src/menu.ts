// A lightweight floating context menu, positioned at a point and dismissed on
// outside click / Escape. Keyboard: it takes focus, arrows / Home / End move,
// Enter picks, Esc closes (and only closes: the key goes no further).
import { icon } from "./icons";
import { escapeHtml } from "./util";

export type MenuItem =
  | "sep"
  /** A section title (not an item: arrows skip it). */
  | { heading: string }
  | { label: string; icon?: string; action: () => void; danger?: boolean; hint?: string };

type Action = Extract<MenuItem, { label: string }>;

/** Closes the open menu, listeners included. */
let closeOpen: (() => void) | null = null;

export function contextMenu(x: number, y: number, items: MenuItem[]): void {
  closeOpen?.();
  const previous = document.activeElement as HTMLElement | null;
  const menu = document.createElement("div");
  menu.className = "ctx-menu pop";
  menu.setAttribute("role", "menu");
  menu.innerHTML = items
    .map((it) => {
      if (it === "sep") return '<div class="pop__sep" role="separator"></div>';
      if ("heading" in it) {
        return `<div class="ctx-heading" role="presentation">${escapeHtml(it.heading)}</div>`;
      }
      const hint = it.hint ? `<span class="ctx-item__hint">${escapeHtml(it.hint)}</span>` : "";
      return `<button class="ctx-item ${it.danger ? "danger" : ""}" role="menuitem">${
        it.icon ? icon(it.icon) : ""
      }<span>${escapeHtml(it.label)}</span>${hint}</button>`;
    })
    .join("");
  document.body.appendChild(menu);
  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(x, window.innerWidth - rect.width - 8)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - rect.height - 8)}px`;

  const buttons = Array.from(menu.querySelectorAll<HTMLButtonElement>(".ctx-item"));
  let bi = 0;
  for (const it of items.filter((item): item is Action => item !== "sep" && "label" in item)) {
    buttons[bi++].addEventListener("click", () => {
      close(true); // the opener gets focus back first; the action may move it on
      it.action();
    });
  }
  let closed = false;
  /** `restore`: hand focus back to where it was (not after a click elsewhere). */
  const close = (restore: boolean) => {
    if (closed) return;
    closed = true;
    closeOpen = null;
    menu.remove();
    document.removeEventListener("mousedown", onOut, true);
    document.removeEventListener("keydown", onKey, true);
    if (restore && previous?.isConnected && previous !== document.body) {
      previous.focus({ preventScroll: true });
    }
  };
  const onOut = (e: MouseEvent) => {
    if (!menu.contains(e.target as Node)) close(false);
  };
  const onKey = (e: KeyboardEvent) => {
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next =
      e.key === "ArrowDown"
        ? buttons[(at + 1) % buttons.length]
        : e.key === "ArrowUp"
          ? buttons[(at - 1 + buttons.length) % buttons.length]
          : e.key === "Home"
            ? buttons[0]
            : e.key === "End"
              ? buttons[buttons.length - 1]
              : null;
    if (next) {
      e.preventDefault();
      e.stopPropagation();
      next.focus();
    } else if ((e.key === "Enter" || e.key === " ") && at >= 0) {
      // Pick it ourselves (exactly once) rather than rely on a synthesized click.
      e.preventDefault();
      e.stopPropagation();
      buttons[at].click();
    } else if (e.key === "Escape" || e.key === "Tab") {
      e.preventDefault();
      e.stopPropagation();
      close(true);
    }
  };
  closeOpen = () => close(false);
  document.addEventListener("keydown", onKey, true);
  // From the next tick, so the click that opened the menu doesn't close it;
  // the menu may be gone by then (picked at once), and must not leave it behind.
  setTimeout(() => {
    if (!closed) document.addEventListener("mousedown", onOut, true);
  });
  buttons[0]?.focus({ preventScroll: true });
}

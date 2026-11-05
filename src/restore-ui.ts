// "Restore my tabs" in the app: taking snapshots of the open tabs as they
// change, and the dialog that offers to reopen the last one.

import { modal } from "./fm/dialogs";
import { icon } from "./icons";
import { api, type SavedSession, type TabContext } from "./ipc";
import {
  buildSnapshot,
  claudeLine,
  snapshotKey,
  startFolder,
  type HostFacts,
  type Snapshot,
  type SnapshotTab,
  type TabFacts,
} from "./restore";
import { current as settings, localShells } from "./settings";

/** How often the open tabs are looked at, besides after each change. */
const SWEEP_MS = 30_000;
/** Changes coming together (opening several tabs) make one snapshot. */
const SETTLE_MS = 2_000;

/**
 * Keeps `tabs.json` up to date: a look every 30 s (a shell's folder, a Claude
 * session started or ended in a tab), one shortly after each change (a tab
 * opened, closed, moved or renamed, a folder changed), and one as the window
 * closes. It's written only when it says something new.
 */
export class TabSnapshots {
  private sweep = 0;
  private settle = 0;
  private lastKey: string | null = null;
  /** A window that never had a tab leaves the last snapshot alone: it may yet restore it. */
  private hadTabs = false;
  private taking: Promise<void> | null = null;

  constructor(
    private readonly facts: () => { tabs: TabFacts[]; active: number },
    private readonly machine: () => HostFacts
  ) {}

  /** Start or stop with the setting. */
  sync(): void {
    if (settings.restoreTabs && !this.sweep) {
      this.sweep = window.setInterval(() => void this.take(), SWEEP_MS);
      this.changed();
    } else if (!settings.restoreTabs && this.sweep) {
      window.clearInterval(this.sweep);
      window.clearTimeout(this.settle);
      this.sweep = this.settle = 0;
    }
  }

  /** Something changed: take a snapshot in a moment. */
  changed(): void {
    if (!settings.restoreTabs || this.settle) return;
    this.settle = window.setTimeout(() => {
      this.settle = 0;
      void this.take();
    }, SETTLE_MS);
  }

  /** Take one now (after the one being taken, if any). */
  async take(): Promise<void> {
    while (this.taking) await this.taking;
    if (!settings.restoreTabs) return;
    this.taking = this.takeOne().finally(() => (this.taking = null));
    return this.taking;
  }

  private async takeOne(): Promise<void> {
    const { tabs, active } = this.facts();
    if (tabs.length) this.hadTabs = true;
    if (!this.hadTabs) return;
    const local = tabs.flatMap((tab) =>
      tab.kind === "terminal" && tab.spec?.kind === "local" && !tab.spec.elevated && tab.sessionId
        ? [tab.sessionId]
        : []
    );
    const contexts = new Map<string, TabContext>();
    try {
      if (local.length) for (const context of await api.tabsContext(local)) contexts.set(context.id, context);
    } catch {
      return; // better the last good snapshot than one without folders and sessions
    }
    const snapshot = buildSnapshot(tabs, active, contexts, this.machine());
    const key = snapshotKey(snapshot);
    if (key === this.lastKey) return;
    try {
      await api.tabsSnapshotSave(snapshot);
      this.lastKey = key;
    } catch {
      // Tried again at the next change or sweep.
    }
  }
}

/** What the dialog needs to say about each tab. */
export interface RestoreFacts {
  machine: HostFacts;
  /** Claude sessions running now (anywhere): not resumed a second time. */
  running: Set<string>;
  saved: SavedSession[];
}

/** Whether a snapshot tab can be reopened, and if not, why. */
export function unavailable(tab: SnapshotTab, facts: RestoreFacts): string | null {
  if (tab.kind === "skipped") return tab.reason;
  if (tab.kind === "local" && !localShells.some((shell) => shell.id === tab.shell)) {
    return `${tab.shell} isn't available`;
  }
  const savedId = tab.kind === "ssh" || tab.kind === "telnet" ? tab.savedId : undefined;
  if (savedId && !facts.saved.some((saved) => saved.id === savedId)) return "the saved connection was deleted";
  return null;
}

/**
 * Ask which of the snapshot's tabs to reopen: they're listed with their
 * folders, and the Claude sessions with exactly what will run. Resolves with
 * the chosen tabs, or null for "not now".
 */
export async function chooseTabs(snapshot: Snapshot, facts: RestoreFacts): Promise<SnapshotTab[] | null> {
  const body = document.createElement("div");
  body.className = "restore";
  const when = document.createElement("div");
  when.className = "restore__when";
  when.textContent = snapshot.savedAt ? `Open when CorePTY last ran (${savedWhen(snapshot.savedAt)}).` : "";
  body.appendChild(when);

  const list = document.createElement("div");
  list.className = "restore__list";
  const boxes: { tab: SnapshotTab; box: HTMLInputElement }[] = [];
  for (const tab of snapshot.tabs) {
    const reason = unavailable(tab, facts);
    const row = document.createElement("label");
    row.className = `restore__tab${reason ? " is-off" : ""}`;
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = !reason;
    box.disabled = !!reason;
    row.appendChild(box);
    const glyph = document.createElement("span");
    glyph.className = "restore__icon";
    glyph.innerHTML = icon(tabIcon(tab));
    row.appendChild(glyph);
    const text = document.createElement("span");
    text.className = "restore__text";
    const label = document.createElement("span");
    label.className = "restore__label";
    label.textContent = tab.pinned ?? tab.label;
    text.appendChild(label);
    for (const line of reason ? [`Not reopened: ${reason}`] : describe(tab, facts)) {
      const detail = document.createElement("span");
      const kind = line.startsWith("claude ") ? " is-command" : line.includes("Claude Code") ? " is-claude" : "";
      detail.className = `restore__detail${kind}`;
      detail.textContent = line;
      text.appendChild(detail);
    }
    row.appendChild(text);
    list.appendChild(row);
    if (!reason) boxes.push({ tab, box });
  }
  body.appendChild(list);

  const restoreButton = () => document.querySelector<HTMLButtonElement>('.restore-dialog [data-value="restore"]');
  list.addEventListener("change", () => {
    const button = restoreButton();
    if (button) button.disabled = !boxes.some(({ box }) => box.checked);
  });

  const choice = await modal({
    title: "Restore your tabs?",
    className: "restore-dialog",
    body,
    buttons: [
      { value: "later", label: "Not now" },
      { value: "restore", label: "Restore", kind: "primary" },
    ],
    enterValue: "restore",
  });
  if (choice !== "restore") return null;
  const chosen = boxes.filter(({ box }) => box.checked).map(({ tab }) => tab);
  return chosen.length ? chosen : null;
}

function tabIcon(tab: SnapshotTab): string {
  switch (tab.kind) {
    case "local":
      return localShells.find((shell) => shell.id === tab.shell)?.icon ?? "terminal";
    case "ssh":
      return "ssh";
    case "telnet":
      return "telnet";
    case "files":
      return "files";
    default:
      return "terminal";
  }
}

/** The lines under a tab's name: what it opens, where, and what it runs. */
export function describe(tab: SnapshotTab, facts: RestoreFacts): string[] {
  switch (tab.kind) {
    case "local": {
      const shell = localShells.find((each) => each.id === tab.shell)?.label ?? tab.shell;
      const folder = startFolder(tab);
      const lines = [folder ? `${shell} in ${folder}` : shell];
      const claude = tab.claude;
      if (!claude) return lines;
      if (facts.running.has(claude.sessionId)) {
        lines.push("Its Claude Code session is open elsewhere, so it isn't resumed here");
        return lines;
      }
      const { line, left } = claudeLine(claude, tab.shell, facts.machine.windows);
      lines.push(
        claude.resumable
          ? "Resumes its Claude Code session:"
          : "Starts Claude Code again (its session had no messages to resume):",
        line
      );
      const out = [...claude.dropped, ...left];
      if (out.length) lines.push(`Left out: ${out.join(", ")}`);
      return lines;
    }
    case "ssh": {
      const saved = facts.saved.find((each) => each.id === tab.savedId);
      return [saved ? `SSH · ${saved.name || saved.host}` : "SSH"];
    }
    case "telnet":
      return [`Telnet · ${tab.host}:${tab.port}`];
    case "files": {
      const sides = [tab.left?.display, tab.right?.display].filter(Boolean);
      return [sides.length ? `File manager · ${sides.join("  |  ")}` : "File manager"];
    }
    default:
      return [];
  }
}

function savedWhen(at: number): string {
  const minutes = Math.round((Date.now() - at) / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const date = new Date(at);
  const today = new Date();
  const time = date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return date.toDateString() === today.toDateString() ? `today ${time}` : `${date.toLocaleDateString()} ${time}`;
}

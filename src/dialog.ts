// Connection dialog for creating / editing SSH and Telnet connections.
import { icon } from "./icons";
import { api, pickKeyFile, type Folder, type SavedSession } from "./ipc";
import { escapeHtml as esc } from "./util";

export interface ConnForm {
  id?: string;
  name: string;
  kind: "ssh" | "telnet";
  host: string;
  port: number;
  username: string;
  authType: "password" | "key";
  password: string;
  keyPath: string;
  passphrase: string;
  saveSecret: boolean;
  folderId?: string | null;
}

export type DialogResult = { action: "connect" | "save"; form: ConnForm } | null;

export interface DialogPrefill {
  presetKind?: "ssh" | "telnet";
  folderId?: string | null;
  password?: string;
  passphrase?: string;
}

export class ConnectionDialog {
  private el: HTMLElement | null = null;
  private resolve: ((r: DialogResult) => void) | null = null;

  async open(existing?: SavedSession, prefill: DialogPrefill = {}): Promise<DialogResult> {
    this.close();
    const kind = existing?.kind ?? prefill.presetKind ?? "ssh";
    const editing = !!existing;
    let folders: Folder[] = [];
    try {
      folders = await api.foldersLoad();
    } catch {
      folders = [];
    }
    const selectedFolder = existing?.folderId ?? prefill.folderId ?? null;

    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";
    backdrop.innerHTML = this.template(existing, prefill, kind, editing, folders, selectedFolder);
    document.body.appendChild(backdrop);
    this.el = backdrop;

    this.wire(backdrop, kind);

    // focus first empty field
    const firstEmpty =
      backdrop.querySelector<HTMLInputElement>("input[name=host]") ??
      backdrop.querySelector<HTMLInputElement>("input");
    setTimeout(() => firstEmpty?.focus(), 30);

    return new Promise<DialogResult>((res) => {
      this.resolve = res;
    });
  }

  private template(
    existing: SavedSession | undefined,
    prefill: DialogPrefill,
    kind: "ssh" | "telnet",
    editing: boolean,
    folders: Folder[],
    selectedFolder: string | null
  ): string {
    const port = existing?.port ?? (kind === "ssh" ? 22 : 23);
    const authType = existing?.authType ?? "password";
    const name = existing?.name ?? "";
    const host = existing?.host ?? "";
    const username = existing?.username ?? "";
    const keyPath = existing?.keyPath ?? "";
    const saveSecret = existing?.saveSecret ?? true;

    return `
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modal__head">
          <div class="modal__title">${editing ? "Edit connection" : "New connection"}</div>
          <button class="modal__close" data-act="cancel" title="Close">${icon("close")}</button>
        </div>
        <div class="modal__body">
          <div class="seg" data-group="kind">
            <button type="button" data-val="ssh" class="${kind === "ssh" ? "is-active" : ""}">${icon("ssh")} SSH</button>
            <button type="button" data-val="telnet" class="${kind === "telnet" ? "is-active" : ""}">${icon("telnet")} Telnet</button>
          </div>

          <div class="grid-name">
            <label class="field">
              <span class="field__label">Name <span class="muted">(optional)</span></span>
              <input name="name" value="${esc(name)}" placeholder="My server" autocomplete="off" />
            </label>
            <label class="field">
              <span class="field__label">Folder</span>
              <select name="folderId" class="select">${folderOptions(folders, selectedFolder)}</select>
            </label>
          </div>

          <div class="grid-host">
            <label class="field">
              <span class="field__label">Host</span>
              <input name="host" value="${esc(host)}" placeholder="example.com or 10.0.0.5" autocomplete="off" spellcheck="false" />
            </label>
            <label class="field field--port">
              <span class="field__label">Port</span>
              <input name="port" type="number" min="1" max="65535" value="${port}" />
            </label>
          </div>

          <label class="field ssh-only">
            <span class="field__label">Username</span>
            <input name="username" value="${esc(username)}" placeholder="root" autocomplete="off" spellcheck="false" />
          </label>

          <div class="field ssh-only">
            <span class="field__label">Authentication</span>
            <div class="seg" data-group="auth">
              <button type="button" data-val="password" class="${authType === "password" ? "is-active" : ""}">Password</button>
              <button type="button" data-val="key" class="${authType === "key" ? "is-active" : ""}">SSH Key</button>
            </div>
          </div>

          <label class="field ssh-only auth-password ${authType === "password" ? "" : "hidden"}">
            <span class="field__label">Password</span>
            <input name="password" type="password" value="${esc(prefill.password ?? "")}" placeholder="${editing && saveSecret ? "Leave blank to keep the stored password" : "Password"}" autocomplete="off" />
          </label>

          <div class="ssh-only auth-key ${authType === "key" ? "" : "hidden"}">
            <label class="field">
              <span class="field__label">Private key file</span>
              <div class="file-row">
                <input name="keyPath" value="${esc(keyPath)}" placeholder="C:\\Users\\you\\.ssh\\id_ed25519" spellcheck="false" />
                <button type="button" class="btn ghost" data-act="browse">Browse…</button>
              </div>
            </label>
            <label class="field">
              <span class="field__label">Passphrase <span class="muted">(if encrypted)</span></span>
              <input name="passphrase" type="password" value="${esc(prefill.passphrase ?? "")}" placeholder="${editing && saveSecret ? "Leave blank to keep the stored passphrase" : "Optional passphrase"}" autocomplete="off" />
            </label>
          </div>

          <label class="check ssh-only">
            <input type="checkbox" name="saveSecret" ${saveSecret ? "checked" : ""} />
            <span>Save secret in the operating system keychain</span>
          </label>
        </div>
        <div class="modal__foot">
          <button type="button" class="btn ghost" data-act="cancel">Cancel</button>
          <div class="spacer"></div>
          <button type="button" class="btn ghost" data-act="save">Save</button>
          <button type="button" class="btn primary" data-act="connect">${icon("bolt")} Connect</button>
        </div>
      </div>`;
  }

  private wire(root: HTMLElement, initialKind: "ssh" | "telnet"): void {
    let kind = initialKind;

    const applyKind = () => {
      root.classList.toggle("is-telnet", kind === "telnet");
      const portInput = root.querySelector<HTMLInputElement>("input[name=port]")!;
      // reset the port to the protocol default only if it still holds the other default
      if (portInput.value === "22" || portInput.value === "23" || portInput.value === "") {
        portInput.value = kind === "ssh" ? "22" : "23";
      }
    };
    applyKind();

    // segmented controls
    root.querySelectorAll<HTMLElement>(".seg").forEach((seg) => {
      seg.querySelectorAll<HTMLButtonElement>("button").forEach((btn) => {
        btn.addEventListener("click", () => {
          seg.querySelectorAll("button").forEach((b) => b.classList.remove("is-active"));
          btn.classList.add("is-active");
          if (seg.dataset.group === "kind") {
            kind = btn.dataset.val as "ssh" | "telnet";
            applyKind();
          } else if (seg.dataset.group === "auth") {
            const isKey = btn.dataset.val === "key";
            root.querySelector(".auth-password")?.classList.toggle("hidden", isKey);
            root.querySelector(".auth-key")?.classList.toggle("hidden", !isKey);
          }
        });
      });
    });

    root.querySelector('[data-act=browse]')?.addEventListener("click", async () => {
      const path = await pickKeyFile();
      if (path) {
        const input = root.querySelector<HTMLInputElement>("input[name=keyPath]")!;
        input.value = path;
      }
    });

    root.querySelectorAll<HTMLElement>("[data-act]").forEach((btn) => {
      const act = btn.dataset.act!;
      if (act === "browse") return;
      btn.addEventListener("click", () => this.finish(act, root, kind));
    });

    root.addEventListener("mousedown", (e) => {
      if (e.target === root) this.finish("cancel", root, kind);
    });
    root.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.finish("cancel", root, kind);
      else if (e.key === "Enter" && !e.shiftKey) this.finish("connect", root, kind);
    });
  }

  private finish(action: string, root: HTMLElement, kind: "ssh" | "telnet"): void {
    if (action === "cancel") {
      this.settle(null);
      return;
    }
    const val = (n: string) =>
      root.querySelector<HTMLInputElement>(`[name=${n}]`)?.value.trim() ?? "";
    const host = val("host");
    if (!host) {
      this.shake(root, "input[name=host]");
      return;
    }
    const authSeg = root.querySelector('.seg[data-group=auth] .is-active') as HTMLElement | null;
    const authType = (authSeg?.dataset.val as "password" | "key") ?? "password";
    if (kind === "ssh" && !val("username")) {
      this.shake(root, "input[name=username]");
      return;
    }

    const form: ConnForm = {
      id: undefined,
      name: val("name"),
      kind,
      host,
      port: clampPort(parseInt(val("port"), 10), kind),
      username: val("username"),
      authType,
      password: root.querySelector<HTMLInputElement>("[name=password]")?.value ?? "",
      keyPath: val("keyPath"),
      passphrase: root.querySelector<HTMLInputElement>("[name=passphrase]")?.value ?? "",
      saveSecret: root.querySelector<HTMLInputElement>("[name=saveSecret]")?.checked ?? false,
      folderId: root.querySelector<HTMLSelectElement>('select[name="folderId"]')?.value || null,
    };
    this.settle({ action: action as "connect" | "save", form });
  }

  private shake(root: HTMLElement, sel: string): void {
    const input = root.querySelector<HTMLElement>(sel);
    input?.classList.remove("shake");
    void input?.offsetWidth;
    input?.classList.add("shake");
    input?.focus();
  }

  private settle(result: DialogResult): void {
    const r = this.resolve;
    this.resolve = null;
    this.close();
    r?.(result);
  }

  private close(): void {
    this.el?.remove();
    this.el = null;
  }
}

/** Clamp a parsed port to a valid 1–65535, falling back to the protocol default. */
function clampPort(p: number, kind: "ssh" | "telnet"): number {
  return Number.isInteger(p) && p >= 1 && p <= 65535 ? p : kind === "ssh" ? 22 : 23;
}

function folderOptions(folders: Folder[], selected: string | null): string {
  const children = (pid: string | null) =>
    folders
      .filter((f) => (f.parentId ?? null) === pid)
      .sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }));
  let html = `<option value="" ${!selected ? "selected" : ""}>— No folder (root) —</option>`;
  const walk = (pid: string | null, depth: number) => {
    for (const f of children(pid)) {
      const pad = "  ".repeat(depth);
      html += `<option value="${f.id}" ${f.id === selected ? "selected" : ""}>${pad}${esc(f.name)}</option>`;
      walk(f.id, depth + 1);
    }
  };
  walk(null, 0);
  return html;
}

// F3 quick viewer: the head of a text file, or an image, read through the
// filesystem layer in raw chunks (works the same for local and remote files).

import { fsApi, type FsEntry, type FsLocation } from "../ipc";
import { modal, type DialogContext } from "./dialogs";
import { formatFileSize } from "./format";

const TEXT_LIMIT = 512 * 1024;
const IMAGE_LIMIT = 32 * 1024 * 1024;
const CHUNK = 4 * 1024 * 1024;
const IMAGE_TYPES: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  bmp: "image/bmp",
  ico: "image/x-icon",
  // Shown through <img>, so any script inside an SVG never runs.
  svg: "image/svg+xml",
};

export function imageType(name: string): string | null {
  const extension = name.split(".").pop()?.toLowerCase() ?? "";
  return name.includes(".") ? (IMAGE_TYPES[extension] ?? null) : null;
}

/** Whether a byte sample looks binary (a NUL in the first 8 KB). */
export function looksBinary(bytes: Uint8Array): boolean {
  return bytes.subarray(0, 8192).includes(0);
}

export async function openViewer(
  fs: string,
  entry: FsEntry,
  context: DialogContext = {}
): Promise<void> {
  const location: FsLocation = { fs, path: entry.path };
  const body = document.createElement("div");
  body.className = "fm-preview__body";
  // Focused, so arrows / PgUp / PgDn / Space scroll the content.
  body.tabIndex = 0;
  body.innerHTML = `<div class="fm-loading">Loading…</div>`;
  const head = document.createElement("div");
  head.className = "fm-preview__path";
  head.textContent = entry.display;
  const wrapper = document.createElement("div");
  wrapper.className = "fm-preview";
  wrapper.append(head, body);
  const foot = document.createElement("div");
  foot.className = "fm-preview__foot";
  wrapper.appendChild(foot);

  let objectUrl: string | null = null;
  let closed = false;
  const closing = modal({
    ...context,
    focus: body,
    title: entry.name,
    className: "modal--wide fm-preview-dialog",
    body: wrapper,
    buttons: [{ value: "close", label: "Close" }],
    closeKeys: ["F3", "F10"],
  });
  void closing.then(() => {
    closed = true;
    if (objectUrl) URL.revokeObjectURL(objectUrl);
  });

  // Links (and some remote entries) carry no size; ask for the target's.
  let size = entry.size;
  if (size === null) {
    try {
      size = (await fsApi.stat(location)).size;
    } catch {
      size = null;
    }
  }
  size ??= 0;
  const type = imageType(entry.name);
  try {
    if (type && size <= IMAGE_LIMIT) {
      const parts: ArrayBuffer[] = [];
      for (let offset = 0; offset < size && !closed; offset += CHUNK) {
        parts.push(await fsApi.read(location, offset, Math.min(CHUNK, size - offset)));
      }
      if (closed) return;
      objectUrl = URL.createObjectURL(new Blob(parts, { type }));
      const image = document.createElement("img");
      image.alt = entry.name;
      image.src = objectUrl;
      body.replaceChildren(image);
      foot.textContent = formatFileSize(size);
    } else if (type) {
      unavailable(body, "This image is too large to preview");
      foot.textContent = formatFileSize(size);
    } else {
      const bytes = new Uint8Array(await fsApi.read(location, 0, TEXT_LIMIT));
      if (closed) return;
      if (looksBinary(bytes)) {
        unavailable(body, "Binary file — no text preview");
      } else {
        const pre = document.createElement("pre");
        pre.textContent = new TextDecoder("utf-8").decode(bytes);
        body.replaceChildren(pre);
      }
      foot.textContent =
        bytes.length >= TEXT_LIMIT && size > bytes.length
          ? `${formatFileSize(size)} · showing the first ${formatFileSize(bytes.length)}`
          : formatFileSize(size);
    }
  } catch (error) {
    if (!closed) unavailable(body, `Couldn't read the file: ${error}`);
  }
  await closing;
}

function unavailable(body: HTMLElement, message: string): void {
  body.classList.add("fm-preview__unavailable");
  const text = document.createElement("span");
  text.textContent = message;
  body.replaceChildren(text);
}

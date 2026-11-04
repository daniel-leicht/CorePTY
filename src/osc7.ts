// OSC 7: shells that report their working folder send
// `ESC ] 7 ; file://host/path ST` after each prompt (fish, zsh with vte.sh,
// bash with a PROMPT_COMMAND). The file manager follows it on SSH tabs.

export interface Osc7Folder {
  /** The machine the shell runs on, as it names itself (often empty). */
  host: string;
  path: string;
}

/** The host and folder in an OSC 7 payload (`file://box/a%20b` → `box`, `/a b`), or null. */
export function parseOsc7(data: string): Osc7Folder | null {
  const match = /^file:\/\/([^/]*)(\/.*)$/i.exec(data.trim());
  if (!match) return null;
  try {
    const host = decodeURIComponent(match[1]).toLowerCase();
    const path = decodeURIComponent(match[2]);
    return path.includes("\0") ? null : { host, path };
  } catch {
    return null;
  }
}

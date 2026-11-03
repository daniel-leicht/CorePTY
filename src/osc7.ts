// OSC 7: shells that report their working folder send
// `ESC ] 7 ; file://host/path ST` after each prompt (fish, zsh with vte.sh,
// bash with a PROMPT_COMMAND). The file manager follows it on SSH tabs.

/** The folder in an OSC 7 payload (`file://host/a%20b` → `/a b`), or null. */
export function parseOsc7(data: string): string | null {
  const match = /^file:\/\/[^/]*(\/.*)$/.exec(data.trim());
  if (!match) return null;
  try {
    const path = decodeURIComponent(match[1]);
    return path.includes("\0") ? null : path;
  } catch {
    return null;
  }
}

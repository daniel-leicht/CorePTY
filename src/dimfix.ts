// Workaround for xterm's WebGL renderer not visually applying the SGR 2
// (dim / faint) attribute. Confirmed on this stack (@xterm/xterm 5.5 +
// addon-webgl 0.18): faint text renders at full brightness under WebGL even
// with minimumContrastRatio off, while colour-based greys (SGR 90 / 38;5 /
// 38;2) render fine — the glyph atlas reuses a cached non-dim glyph for dim
// cells. WebGL is wanted elsewhere (stable glyph grid), so rather than drop it
// we rewrite the dim attribute in the PTY byte stream into an explicit grey
// foreground: a 50% blend of the theme's foreground over its background, which
// is what "dim" is meant to look like (à la Windows Terminal). Most faint text
// — e.g. Claude Code's autosuggest ghost text — is dim-on-default-fg, which
// this reproduces exactly; dim layered on an explicit colour degrades to the
// same grey (rare, acceptable).
import { activeTheme } from "./themes";

const ESC = 0x1b;
const CSI = 0x5b; // '['
const SGR_FINAL = 0x6d; // 'm'
/** Guard against buffering a runaway "incomplete" CSI forever. */
const MAX_CARRY = 64;

let cachedThemeId = "";
let cachedDimParams = "";

/** SGR params that replace a bare `2` (dim): a truecolor grey `38;2;r;g;b`. */
function dimParams(): string {
  const t = activeTheme();
  if (t.id === cachedThemeId && cachedDimParams) return cachedDimParams;
  const fg = hexToRgb(t.terminal.foreground ?? "#cdd2df");
  const bg = hexToRgb(t.terminal.background ?? "#0e0f13");
  const mix = (a: number, b: number) => Math.round(a * 0.5 + b * 0.5);
  cachedThemeId = t.id;
  cachedDimParams = `38;2;${mix(fg[0], bg[0])};${mix(fg[1], bg[1])};${mix(fg[2], bg[2])}`;
  return cachedDimParams;
}

function hexToRgb(hex: string): [number, number, number] {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return [200, 200, 200];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/** Rewrite an SGR parameter list: a bare `2` (dim) → grey fg; `22` (intensity
 *  off) also resets the fg (`39`) so the grey doesn't linger. Everything else is
 *  passed through untouched.
 *
 *  Crucially, the extended-colour introducers `38`/`48`/`58` are followed by
 *  sub-parameters (`;5;N` or `;2;R;G;B`) whose digits — including a `2` selector
 *  or an R/G/B/index that happens to equal `2` — must NOT be read as the dim
 *  attribute. We copy those sub-parameters verbatim. (Colon-packed colours like
 *  `38:2::r:g:b` are a single `;`-element and never equal `"2"`, so they're
 *  already safe.) */
function rewriteSgr(params: string): string {
  // Cheap reject: only a bare `2` / `22` matters; if no `2` digit at all, skip.
  if (params.indexOf("2") === -1) return params;
  const parts = params.split(";");
  const out: string[] = [];
  for (let i = 0; i < parts.length; i++) {
    const p = parts[i];
    if (p === "38" || p === "48" || p === "58") {
      out.push(p);
      const mode = parts[i + 1];
      const take = mode === "5" ? 2 : mode === "2" ? 4 : mode !== undefined ? 1 : 0;
      const consumed = Math.min(take, parts.length - 1 - i);
      for (let k = 1; k <= consumed; k++) out.push(parts[i + k]);
      i += consumed;
      continue;
    }
    if (p === "2") out.push(dimParams());
    else if (p === "22") out.push("22;39");
    else out.push(p);
  }
  return out.join(";");
}

/**
 * Stateful, per-terminal stream filter. `feed()` returns the byte stream with
 * SGR dim rewritten; an incomplete escape sequence at a chunk boundary is
 * carried to the next call so a split `ESC [ … m` is still rewritten as a unit.
 */
export class SgrDimFilter {
  private carry: Uint8Array | null = null;

  feed(input: Uint8Array): Uint8Array {
    // Fast path: the overwhelming majority of chunks contain no ESC.
    if (!this.carry && !input.includes(ESC)) return input;

    let buf = input;
    if (this.carry) {
      buf = new Uint8Array(this.carry.length + input.length);
      buf.set(this.carry, 0);
      buf.set(input, this.carry.length);
      this.carry = null;
    }

    const out: number[] = [];
    const n = buf.length;
    let i = 0;
    while (i < n) {
      const b = buf[i];
      if (b !== ESC) {
        out.push(b);
        i++;
        continue;
      }
      // ESC. Only CSI (ESC [) can carry an SGR; anything else passes through.
      if (i + 1 >= n) {
        this.carry = buf.slice(i); // ESC at very end — wait for more
        break;
      }
      if (buf[i + 1] !== CSI) {
        out.push(b);
        i++;
        continue;
      }
      // Scan to the CSI final byte (0x40–0x7e).
      let j = i + 2;
      while (j < n && !(buf[j] >= 0x40 && buf[j] <= 0x7e)) j++;
      if (j >= n) {
        // Incomplete CSI at the chunk end: buffer it, unless it's absurdly long
        // (malformed) — then flush as-is so we never stall the stream.
        if (n - i > MAX_CARRY) for (let k = i; k < n; k++) out.push(buf[k]);
        else this.carry = buf.slice(i);
        break;
      }
      if (buf[j] === SGR_FINAL) {
        let params = "";
        for (let k = i + 2; k < j; k++) params += String.fromCharCode(buf[k]);
        const rewritten = rewriteSgr(params);
        out.push(ESC, CSI);
        for (let k = 0; k < rewritten.length; k++) out.push(rewritten.charCodeAt(k));
        out.push(SGR_FINAL);
      } else {
        for (let k = i; k <= j; k++) out.push(buf[k]); // non-SGR CSI, verbatim
      }
      i = j + 1;
    }
    return Uint8Array.from(out);
  }
}

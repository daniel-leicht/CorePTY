export interface PasteTarget {
  paste(data: string): void;
}

/** Route clipboard input through xterm so it can normalize and bracket the paste. */
export function pasteIntoTerminal(target: PasteTarget, text: string): void {
  target.paste(text);
}

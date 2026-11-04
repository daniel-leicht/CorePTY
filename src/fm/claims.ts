// A file manager tab's hold on remote filesystems. The backend counts users of
// each open SFTP filesystem and closes it when the last lets go; a tab is one
// user per filesystem, for as long as any claim on it lives. A panel takes a
// claim before it goes to a filesystem and gives back the old one only once
// the new folder is shown, so swapping panels or going back and forth never
// lets go of a server still on screen.

export interface ClaimsBackend {
  /** One more reference to an open filesystem; false when it isn't open. */
  retain(fs: string): Promise<boolean>;
  /** One reference fewer. */
  close(fs: string): void;
}

export class Claims {
  /** Filesystems held (one backend reference each), and how many claims use each. */
  private readonly counts = new Map<string, number>();
  private disposed = false;

  constructor(
    private readonly backend: ClaimsBackend,
    private readonly isRemote: (fs: string) => boolean
  ) {}

  /** A claim on `fs`, taking a reference if there's none yet; null when it
   * isn't open any more. Local filesystems need nothing. */
  async claim(fs: string): Promise<(() => void) | null> {
    if (!this.isRemote(fs)) return () => undefined;
    if (!this.counts.has(fs)) {
      if (!(await this.backend.retain(fs))) return null;
      // Another claim may have taken one meanwhile: one per tab.
      if (this.disposed || this.counts.has(fs)) this.backend.close(fs);
      else this.counts.set(fs, 0);
      if (this.disposed) return null;
    }
    return this.take(fs);
  }

  /** A reference just taken elsewhere (opening a server): the one for `fs`, or
   * one too many (given back), with a claim on it. */
  adopt(fs: string): () => void {
    if (this.disposed || this.counts.has(fs)) this.backend.close(fs);
    else this.counts.set(fs, 0);
    return this.disposed ? () => undefined : this.take(fs);
  }

  /** Let go of everything (the tab closed); claims given back later do nothing. */
  dispose(): void {
    this.disposed = true;
    for (const fs of this.counts.keys()) this.backend.close(fs);
    this.counts.clear();
  }

  /** What's held, with its claim count (tests). */
  held(): Map<string, number> {
    return new Map(this.counts);
  }

  private take(fs: string): () => void {
    this.counts.set(fs, (this.counts.get(fs) ?? 0) + 1);
    let released = false;
    return () => {
      if (released) return;
      released = true;
      const left = (this.counts.get(fs) ?? 0) - 1;
      if (left > 0) this.counts.set(fs, left);
      else if (this.counts.delete(fs)) this.backend.close(fs);
    };
  }
}

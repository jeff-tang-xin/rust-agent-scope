/**
 * Edit locations for Navigate › Last Edit Location (⇧⌘⌫) / Next Edit
 * Location, like IDEA's: every edit records the caret; edits within
 * `MERGE_LINES` lines of the newest place update it instead of adding one.
 * Repeated ⇧⌘⌫ walks back through older places; an edit resets the walk.
 */
import type { NavigationLocation } from "@/editor/api";

export const MERGE_LINES = 3;
const MAX_ENTRIES = 60;

type Place = Omit<NavigationLocation, "time">;

export class EditHistory {
  entries: Place[] = [];
  /** Position of the last ⇧⌘⌫ / Next Edit target; `entries.length` while not walking. */
  index = 0;

  record(loc: Place) {
    const last = this.entries[this.entries.length - 1];
    if (last && last.path === loc.path && Math.abs(last.line - loc.line) <= MERGE_LINES)
      this.entries[this.entries.length - 1] = { ...loc };
    else {
      this.entries = this.entries.filter((e) => e.path !== loc.path || Math.abs(e.line - loc.line) > MERGE_LINES);
      this.entries.push({ ...loc });
      if (this.entries.length > MAX_ENTRIES) this.entries.splice(0, this.entries.length - MAX_ENTRIES);
    }
    this.index = this.entries.length;
  }

  /**
   * The place to go back to from `caret`: the newest edit, or the one before
   * it when the caret already is there (or the walk already started).
   */
  back(caret: Place | null): Place | null {
    let i = Math.min(this.index, this.entries.length) - 1;
    const at = (e: Place | undefined) => !!e && !!caret && e.path === caret.path && e.line === caret.line;
    if (i >= 0 && this.index === this.entries.length && at(this.entries[i])) i--;
    if (i < 0) return null;
    this.index = i;
    return this.entries[i]!;
  }

  forward(): Place | null {
    if (this.index >= this.entries.length - 1) return null;
    this.index++;
    return this.entries[this.index]!;
  }

  clear() {
    this.entries = [];
    this.index = 0;
  }

  /** Drops the places of a deleted file. */
  removePath(path: string) {
    const before = this.entries.length;
    this.entries = this.entries.filter((e) => e.path !== path);
    if (this.entries.length !== before) this.index = this.entries.length;
  }
}

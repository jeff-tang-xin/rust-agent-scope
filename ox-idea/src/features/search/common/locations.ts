/** Recent caret locations (⇧⌘E), recorded locally when the editor exposes no history. */
export interface RecentLocation {
  path: string;
  /** 0-based. */
  line: number;
  col: number;
  /** A few lines around `line`, starting at `snippetStart`. */
  snippet: string[];
  snippetStart: number;
  /** Recorded because of an edit (IDEA "Show changed only"). */
  changed: boolean;
  time: number;
}

export const MAX_LOCATIONS = 60;
/** Caret moves within this many lines merge into one entry. */
export const MERGE_DISTANCE = 5;

export function recordLocation(list: readonly RecentLocation[], entry: RecentLocation): RecentLocation[] {
  const near = (l: RecentLocation) => l.path === entry.path && Math.abs(l.line - entry.line) <= MERGE_DISTANCE;
  const merged = list.find(near);
  const next = { ...entry, changed: entry.changed || (merged?.changed ?? false) };
  return [next, ...list.filter((l) => !near(l))].slice(0, MAX_LOCATIONS);
}

/** Combines editor history and locally recorded locations, most recent first. */
export function mergeLocations(...lists: ReadonlyArray<readonly RecentLocation[]>): RecentLocation[] {
  const all = lists.flat().sort((a, b) => a.time - b.time);
  let out: RecentLocation[] = [];
  for (const l of all) {
    const prev = out.find((x) => x.path === l.path && Math.abs(x.line - l.line) <= MERGE_DISTANCE);
    out = recordLocation(
      out,
      prev && !l.snippet.length ? { ...l, snippet: prev.snippet, snippetStart: prev.snippetStart } : l,
    );
  }
  return out;
}

/** A place removed from Recent Locations (Delete); visits after `time` show it again. */
export interface DismissedLocation {
  path: string;
  line: number;
  time: number;
}

export function isDismissed(l: RecentLocation, dismissed: readonly DismissedLocation[]): boolean {
  return dismissed.some((d) => d.path === l.path && Math.abs(d.line - l.line) <= MERGE_DISTANCE && l.time <= d.time);
}

/** Drops entries pointing at removed files, shifts paths of renamed ones. */
export function renameLocations(list: readonly RecentLocation[], from: string, to: string | null): RecentLocation[] {
  const out: RecentLocation[] = [];
  for (const l of list) {
    if (l.path === from || l.path.startsWith(from + "/")) {
      if (to !== null) out.push({ ...l, path: to + l.path.slice(from.length) });
    } else out.push(l);
  }
  return out;
}

/**
 * Normalises entries returned by the editor's `getNavigationHistory()` (whose
 * exact shape is owned by the editor feature) into `RecentLocation`s.
 */
export function fromNavigationHistory(raw: unknown): RecentLocation[] | null {
  // `{ entries (oldest first), index }` snapshot, or a plain array.
  const entries =
    raw && typeof raw === "object" && Array.isArray((raw as { entries?: unknown }).entries)
      ? (raw as { entries: unknown[] }).entries
      : raw;
  if (!Array.isArray(entries)) return null;
  const out: RecentLocation[] = [];
  for (const e of entries as Array<Record<string, unknown>>) {
    if (!e || typeof e !== "object") continue;
    const path = typeof e.path === "string" ? e.path : null;
    if (!path) continue;
    const pos = (e.selection ?? e.pos ?? e.position ?? e) as Record<string, unknown>;
    const line = typeof pos.line === "number" ? pos.line : 0;
    const col = typeof pos.col === "number" ? pos.col : typeof pos.column === "number" ? pos.column : 0;
    const snippet = Array.isArray(e.snippet)
      ? (e.snippet as unknown[]).map(String)
      : typeof e.snippet === "string"
        ? [e.snippet]
        : typeof e.text === "string"
          ? [e.text]
          : [];
    out.push({
      path,
      line,
      col,
      snippet,
      snippetStart: typeof e.snippetStart === "number" ? e.snippetStart : line,
      changed: e.changed === true || e.edited === true,
      time: typeof e.time === "number" ? e.time : 0,
    });
  }
  return out;
}

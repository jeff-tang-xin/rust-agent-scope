/** IDEA's "New" popup (⌘N): the New › items under a "Search for file type" field. */
import { create } from "zustand";
import { getAction } from "@/platform/actions";
import { fuzzyMatch, resolveMenuItems, type MenuItem } from "@/ui";

export interface NewPopupRow {
  key: string;
  separator?: boolean;
  label: string;
  icon?: string;
  shortcut?: string;
  run?: () => void;
  /** Matched label indices while filtering. */
  matches?: number[] | null;
}

interface NewPopupState {
  request: { at: { x: number; y: number }; items: MenuItem[]; seq: number } | null;
}

export const useNewPopup = create<NewPopupState>(() => ({ request: null }));
let seq = 0;

export function openNewPopup(at: { x: number; y: number }, items: MenuItem[]) {
  useNewPopup.setState({ request: { at, items, seq: ++seq } });
}

export const closeNewPopup = () => useNewPopup.setState({ request: null });

/** Generic action keywords that would match every template. */
const GENERIC = new Set(["new", "create", "template", "file"]);

/**
 * Enabled entries, filtered by `query`: label matches (highlighted), then
 * entries whose action has a matching keyword (`css` finds Stylesheet).
 * Separators are kept only without a query.
 */
export function newPopupRows(items: MenuItem[], query: string): NewPopupRow[] {
  const q = query.trim();
  const rows: NewPopupRow[] = [];
  const byKeyword: NewPopupRow[] = [];
  for (const r of resolveMenuItems(items)) {
    if (r.kind === "separator") {
      if (!q && rows.length && !rows[rows.length - 1]!.separator) rows.push({ key: r.key, separator: true, label: "" });
      continue;
    }
    if (r.kind !== "item" || r.disabled || !r.run) continue;
    const row: NewPopupRow = { key: r.key, label: r.label, icon: r.icon, shortcut: r.shortcut, run: r.run };
    if (!q) {
      rows.push(row);
      continue;
    }
    const matches = fuzzyMatch(r.label, q);
    if (matches) rows.push({ ...row, matches });
    else if (keywordMatch(r.key, q)) byKeyword.push(row);
  }
  while (rows.length && rows[rows.length - 1]!.separator) rows.pop();
  return [...rows, ...byKeyword];
}

function keywordMatch(actionId: string, q: string): boolean {
  const lower = q.toLowerCase().replace(/^\./, "");
  return !!getAction(actionId)?.keywords?.some(
    (k) => !GENERIC.has(k) && k.toLowerCase().replace(/^\./, "").startsWith(lower),
  );
}

/**
 * Local scoring for results that don't come with a backend score (actions,
 * LSP workspace symbols, outline items). Mirrors the index's ordering:
 * exact > prefix > contiguous > camel-hump > subsequence; shorter wins ties.
 */
import { fuzzyMatch } from "@/ui";

export interface LocalMatch {
  score: number;
  indices: number[];
}

export interface MatchOptions {
  /**
   * Reject scattered subsequences: every fragment of the query must be a
   * substring or start at a word / camel-hump boundary (IDEA's matcher for
   * Find Action, where "main" must not match "sMArt step INto").
   */
  wordAligned?: boolean;
}

/** Multi-word aware match: every whitespace-separated token must match. */
export function matchText(text: string, query: string, opts: MatchOptions = {}): LocalMatch | null {
  const q = query.trim();
  if (!q) return { score: 0, indices: [] };
  const fuzzy = (t: string, p: string) => {
    const hit = fuzzyMatch(t, p);
    return hit && (!opts.wordAligned || isWordAligned(t, hit)) ? hit : null;
  };
  const whole = fuzzy(text, q);
  if (whole) return { score: scoreIndices(text, q, whole), indices: whole };
  const tokens = q.split(/\s+/).filter(Boolean);
  if (tokens.length < 2) return null;
  const all = new Set<number>();
  let score = 0;
  for (const t of tokens) {
    const hit = fuzzy(text, t);
    if (!hit) return null;
    hit.forEach((i) => all.add(i));
    score += scoreIndices(text, t, hit);
  }
  return { score: Math.round(score / tokens.length) - 50, indices: [...all].sort((a, b) => a - b) };
}

/** Every run of matched characters after the first starts a word. */
export function isWordAligned(text: string, indices: readonly number[]): boolean {
  if (isContiguous(indices)) return true;
  for (let k = 0; k < indices.length; k++) {
    const i = indices[k]!;
    if (k > 0 && indices[k - 1] === i - 1) continue;
    if (i !== 0 && !/[\s_\-./\\:]/.test(text[i - 1]!) && !isUpper(text, i)) return false;
  }
  return true;
}

function isContiguous(indices: readonly number[]): boolean {
  for (let i = 1; i < indices.length; i++) if (indices[i] !== indices[i - 1]! + 1) return false;
  return true;
}

function scoreIndices(text: string, query: string, indices: readonly number[]): number {
  const lower = text.toLowerCase();
  const q = query.toLowerCase();
  let score: number;
  if (lower === q) score = 1000;
  else if (lower.startsWith(q)) score = 800;
  else if (isContiguous(indices)) score = 600 - Math.min(100, indices[0] ?? 0) * 2;
  else {
    const starts = indices.filter((i) => i === 0 || /[\s_\-./\\:]/.test(text[i - 1]!) || isUpper(text, i)).length;
    score = starts >= Math.min(2, indices.length) ? 400 + starts * 10 : 200 - spread(indices);
  }
  if (text.slice(indices[0] ?? 0).startsWith(query)) score += 5; // case-exact bonus
  return score - Math.min(60, text.length);
}

function isUpper(text: string, i: number) {
  const c = text[i]!;
  return c !== c.toLowerCase() && text[i - 1] === text[i - 1]!.toLowerCase();
}

function spread(indices: readonly number[]): number {
  if (indices.length < 2) return 0;
  return Math.min(150, indices[indices.length - 1]! - indices[0]! - indices.length + 1);
}

/** Stable sort by score descending. */
export function byScore<T extends { score: number }>(items: T[]): T[] {
  return items
    .map((item, i) => ({ item, i }))
    .sort((a, b) => b.item.score - a.item.score || a.i - b.i)
    .map((x) => x.item);
}

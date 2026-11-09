/**
 * Lightweight matcher for speed search and local filtering. Prefers a
 * contiguous substring, then word/camel-hump starts, then a plain
 * subsequence. Returns matched UTF-16 indices or null.
 */
export function fuzzyMatch(text: string, query: string): number[] | null {
  if (!query) return [];
  const t = text.toLowerCase();
  const q = query.toLowerCase();
  const at = t.indexOf(q);
  if (at >= 0) return Array.from({ length: q.length }, (_, i) => at + i);
  return humpMatch(text, q) ?? subsequence(t, q);
}

/** Like `fuzzyMatch` without the loose subsequence fallback: substring or word / camel-hump starts only. */
export function wordMatch(text: string, query: string): number[] | null {
  if (!query) return [];
  const q = query.toLowerCase();
  const at = text.toLowerCase().indexOf(q);
  if (at >= 0) return Array.from({ length: q.length }, (_, i) => at + i);
  return humpMatch(text, q);
}

function isWordStart(text: string, i: number): boolean {
  if (i === 0) return true;
  const prev = text[i - 1]!;
  const c = text[i]!;
  if (/[\s_\-./\\:]/.test(prev)) return true;
  return c !== c.toLowerCase() && prev === prev.toLowerCase();
}

function humpMatch(text: string, q: string): number[] | null {
  const out: number[] = [];
  const t = text.toLowerCase();
  let qi = 0;
  let i = 0;
  while (qi < q.length && i < t.length) {
    if (
      t[i] === q[qi] &&
      (qi === 0 ? isWordStart(text, i) || i === 0 : out[out.length - 1] === i - 1 || isWordStart(text, i))
    ) {
      out.push(i);
      qi++;
    }
    i++;
  }
  return qi === q.length ? out : null;
}

function subsequence(t: string, q: string): number[] | null {
  const out: number[] = [];
  let qi = 0;
  for (let i = 0; i < t.length && qi < q.length; i++) {
    if (t[i] === q[qi]) {
      out.push(i);
      qi++;
    }
  }
  return qi === q.length ? out : null;
}

/** Groups sorted indices into `[start, end)` runs. */
export function indexRuns(indices: readonly number[]): Array<[number, number]> {
  const runs: Array<[number, number]> = [];
  for (const i of indices) {
    const last = runs[runs.length - 1];
    if (last && last[1] === i) last[1] = i + 1;
    else runs.push([i, i + 1]);
  }
  return runs;
}

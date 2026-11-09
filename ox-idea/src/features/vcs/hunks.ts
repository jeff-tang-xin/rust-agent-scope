/**
 * Line hunk helpers (pure). Hunk conventions follow `ide-vcs`: 0-based lines;
 * a hunk covers current lines `[start, start + lines)`; deletions have
 * `lines == 0` and `start` = the line now following the removed block (equal
 * to the line count when removed at the end); `oldText` is the original text
 * of the hunk including line terminators.
 */
import type { LineHunk } from "@/bindings";

/** Offsets where each line starts, plus `text.length` as a sentinel line start. */
export function lineStarts(text: string): number[] {
  const starts = [0];
  for (let i = text.indexOf("\n"); i >= 0; i = text.indexOf("\n", i + 1)) starts.push(i + 1);
  if (starts[starts.length - 1] !== text.length) starts.push(text.length);
  return starts;
}

function startOfLine(starts: number[], line: number): number {
  return starts[Math.min(line, starts.length - 1)]!;
}

/** The edit that restores the hunk's original text (IDEA "Rollback Lines"). */
export function rollbackEdit(
  text: string,
  hunk: Pick<LineHunk, "start" | "lines" | "oldText">,
): { from: number; to: number; insert: string } {
  const starts = lineStarts(text);
  const from = startOfLine(starts, hunk.start);
  const to = hunk.lines === 0 ? from : startOfLine(starts, hunk.start + hunk.lines);
  // Hunks are computed over terminator-inclusive lines, so a plain replacement restores
  // the original exactly (including a missing / present final newline).
  const insert = hunk.oldText;
  return { from, to, insert };
}

/** Applies `rollbackEdit` (for tests and previews). */
export function applyRollback(text: string, hunk: Pick<LineHunk, "start" | "lines" | "oldText">): string {
  const { from, to, insert } = rollbackEdit(text, hunk);
  return text.slice(0, from) + insert + text.slice(to);
}

/** The hunk whose marker is on `line` (deletions mark the following line). */
export function hunkAtLine<H extends Pick<LineHunk, "start" | "lines">>(
  hunks: readonly H[],
  line: number,
  lineCount = Infinity,
): H | undefined {
  return hunks.find((h) => {
    if (h.lines === 0) {
      const marker = Math.min(h.start, Math.max(0, lineCount - 1));
      return marker === line;
    }
    return line >= h.start && line < h.start + h.lines;
  });
}

/** Next hunk starting after `line` (wraps around). */
export function nextHunk<H extends Pick<LineHunk, "start" | "lines">>(
  hunks: readonly H[],
  line: number,
): H | undefined {
  if (!hunks.length) return undefined;
  return hunks.find((h) => h.start > line) ?? hunks[0];
}

/** Previous hunk ending before `line` (wraps around). */
export function prevHunk<H extends Pick<LineHunk, "start" | "lines">>(
  hunks: readonly H[],
  line: number,
): H | undefined {
  if (!hunks.length) return undefined;
  for (let i = hunks.length - 1; i >= 0; i--) {
    const h = hunks[i]!;
    if (h.start + Math.max(h.lines, 1) - 1 < line) return h;
  }
  return hunks[hunks.length - 1];
}

/** Current text of the hunk's lines (for the popup's "new" side). */
export function currentHunkText(text: string, hunk: Pick<LineHunk, "start" | "lines">): string {
  if (hunk.lines === 0) return "";
  const starts = lineStarts(text);
  return text.slice(startOfLine(starts, hunk.start), startOfLine(starts, hunk.start + hunk.lines));
}

/**
 * Backend hunk indices overlapping a diff chunk given as 0-based line ranges
 * `[fromA, toA)` (old side) and `[fromB, toB)` (new side).
 */
export function hunksOverlapping(
  hunks: readonly Pick<LineHunk, "start" | "lines" | "oldStart" | "oldLines">[],
  chunk: { fromA: number; toA: number; fromB: number; toB: number },
): number[] {
  const overlaps = (s1: number, e1: number, s2: number, e2: number) => {
    if (s1 === e1 && s2 === e2) return s1 === s2;
    if (s1 === e1) return s2 <= s1 && s1 <= e2;
    if (s2 === e2) return s1 <= s2 && s2 <= e1;
    return s1 < e2 && s2 < e1;
  };
  const out: number[] = [];
  hunks.forEach((h, i) => {
    const b = overlaps(h.start, h.start + h.lines, chunk.fromB, chunk.toB);
    const a = overlaps(h.oldStart, h.oldStart + h.oldLines, chunk.fromA, chunk.toA);
    if (a || b) out.push(i);
  });
  return out;
}

/**
 * Identity of a changed block that survives edits elsewhere in the file: its old and
 * new text (an FNV-1a hash), plus how often the same block occurs before it.
 */
export function hunkKeys(text: string, hunks: readonly Pick<LineHunk, "start" | "lines" | "oldText">[]): string[] {
  const seen = new Map<string, number>();
  return hunks.map((h) => {
    const body = `${h.oldText}\u0000${currentHunkText(text, h)}`;
    let x = 0x811c9dc5;
    for (let i = 0; i < body.length; i++) x = Math.imul(x ^ body.charCodeAt(i), 0x01000193);
    const base = `${(x >>> 0).toString(36)}:${body.length}`;
    const n = seen.get(base) ?? 0;
    seen.set(base, n + 1);
    return `${base}:${n}`;
  });
}

/**
 * IPC ⇄ CodeMirror coordinate conversion. The backend speaks 0-based lines and
 * UTF-16 columns; CodeMirror offsets are UTF-16 code units too, so a column is
 * simply an index into the line's text.
 */
import type { Text } from "@codemirror/state";
import type { Pos, Range, TextEdit } from "@/bindings";

export function posToOffset(doc: Text, pos: Pos): number {
  if (pos.line < 0) return 0;
  if (pos.line >= doc.lines) return doc.length;
  const line = doc.line(pos.line + 1);
  return line.from + Math.max(0, Math.min(pos.col, line.length));
}

export function offsetToPos(doc: Text, offset: number): Pos {
  const o = Math.max(0, Math.min(offset, doc.length));
  const line = doc.lineAt(o);
  return { line: line.number - 1, col: o - line.from };
}

export function rangeToOffsets(doc: Text, range: Range): { from: number; to: number } {
  const a = posToOffset(doc, range.start);
  const b = posToOffset(doc, range.end);
  return a <= b ? { from: a, to: b } : { from: b, to: a };
}

export function offsetsToRange(doc: Text, from: number, to: number): Range {
  return { start: offsetToPos(doc, from), end: offsetToPos(doc, to) };
}

export function comparePos(a: Pos, b: Pos): number {
  return a.line - b.line || a.col - b.col;
}

/** Whether `pos` lies inside `range` (end inclusive, like a caret at a word end). */
export function rangeContains(range: Range, pos: Pos): boolean {
  return comparePos(range.start, pos) <= 0 && comparePos(pos, range.end) <= 0;
}

/**
 * Converts LSP text edits (all relative to the same document version) into
 * sorted, non-overlapping CodeMirror changes. Overlapping edits are dropped
 * rather than producing a corrupt change set.
 */
export function textEditsToChanges(
  doc: Text,
  edits: readonly TextEdit[],
): Array<{ from: number; to: number; insert: string }> {
  const changes = edits
    .map((e, i) => ({ ...rangeToOffsets(doc, e.range), insert: e.new_text, i }))
    .sort((a, b) => a.from - b.from || a.to - b.to || a.i - b.i);
  const out: Array<{ from: number; to: number; insert: string }> = [];
  let end = -1;
  for (const c of changes) {
    if (c.from < end) continue;
    const current = doc.sliceString(c.from, c.to);
    if (current === c.insert) continue;
    out.push({ from: c.from, to: c.to, insert: c.insert });
    end = c.to;
  }
  return out;
}

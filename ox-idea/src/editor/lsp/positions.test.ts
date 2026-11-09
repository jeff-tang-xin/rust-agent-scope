import { describe, expect, it } from "vitest";
import { Text } from "@codemirror/state";
import {
  offsetToPos,
  offsetsToRange,
  posToOffset,
  rangeContains,
  rangeToOffsets,
  textEditsToChanges,
} from "./positions";

const doc = Text.of(["let a = 1;", "// 😀 émoji", "", "end"]);

describe("positions", () => {
  it("maps 0-based line + UTF-16 column to offsets and back", () => {
    expect(posToOffset(doc, { line: 0, col: 4 })).toBe(4);
    // Line 1 starts at 11; the emoji is two UTF-16 units.
    expect(posToOffset(doc, { line: 1, col: 3 })).toBe(14);
    expect(posToOffset(doc, { line: 1, col: 6 })).toBe(17);
    expect(offsetToPos(doc, 17)).toEqual({ line: 1, col: 6 });
    expect(offsetToPos(doc, doc.length)).toEqual({ line: 3, col: 3 });
  });

  it("clamps out-of-range positions", () => {
    expect(posToOffset(doc, { line: 0, col: 99 })).toBe(10);
    expect(posToOffset(doc, { line: 99, col: 0 })).toBe(doc.length);
    expect(posToOffset(doc, { line: -1, col: 3 })).toBe(0);
    expect(offsetToPos(doc, -5)).toEqual({ line: 0, col: 0 });
  });

  it("converts ranges in both directions and normalises order", () => {
    const r = offsetsToRange(doc, 4, 14);
    expect(r).toEqual({ start: { line: 0, col: 4 }, end: { line: 1, col: 3 } });
    expect(rangeToOffsets(doc, { start: r.end, end: r.start })).toEqual({ from: 4, to: 14 });
  });

  it("checks containment with inclusive ends", () => {
    const r = { start: { line: 1, col: 2 }, end: { line: 1, col: 5 } };
    expect(rangeContains(r, { line: 1, col: 5 })).toBe(true);
    expect(rangeContains(r, { line: 1, col: 6 })).toBe(false);
    expect(rangeContains(r, { line: 0, col: 9 })).toBe(false);
  });

  it("turns text edits into sorted, non-overlapping changes", () => {
    const changes = textEditsToChanges(doc, [
      { range: { start: { line: 3, col: 0 }, end: { line: 3, col: 3 } }, new_text: "END" },
      { range: { start: { line: 0, col: 0 }, end: { line: 0, col: 3 } }, new_text: "const" },
      { range: { start: { line: 0, col: 1 }, end: { line: 0, col: 2 } }, new_text: "overlap" },
      { range: { start: { line: 0, col: 4 }, end: { line: 0, col: 5 } }, new_text: "a" },
    ]);
    expect(changes).toEqual([
      { from: 0, to: 3, insert: "const" },
      { from: doc.length - 3, to: doc.length, insert: "END" },
    ]);
  });
});

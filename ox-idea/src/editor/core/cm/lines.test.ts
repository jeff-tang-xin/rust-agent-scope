import { describe, expect, it } from "vitest";
import { EditorSelection, EditorState } from "@codemirror/state";
import { reverseLinesSpec, sortLinesSpec } from "./lines";

function apply(doc: string, sel: EditorSelection | undefined, fn: typeof sortLinesSpec) {
  const state = EditorState.create({ doc, selection: sel, extensions: EditorState.allowMultipleSelections.of(true) });
  const spec = fn(state);
  return spec ? state.update(spec).state.doc.toString() : null;
}

describe("Sort Lines / Reverse Lines", () => {
  it("sorts the whole file without a selection", () => {
    expect(apply("c\na\nb", undefined, sortLinesSpec)).toBe("a\nb\nc");
    expect(apply("c\na\nb", undefined, reverseLinesSpec)).toBe("b\na\nc");
    expect(apply("c\nb\na\n", undefined, sortLinesSpec)).toBe("a\nb\nc\n");
    expect(apply("c\nb\na\n", undefined, reverseLinesSpec)).toBe("a\nb\nc\n");
  });

  it("sorts only the selected lines (a selection ending at a line start excludes that line)", () => {
    const doc = "z\nc\nb\na\ny";
    const sel = EditorSelection.single(2, 8); // "c\nb\na\n"
    expect(apply(doc, sel, sortLinesSpec)).toBe("z\na\nb\nc\ny");
  });

  it("handles several selections independently", () => {
    const doc = "b\na\n--\nd\nc";
    const sel = EditorSelection.create([EditorSelection.range(0, 3), EditorSelection.range(7, 10)]);
    expect(apply(doc, sel, sortLinesSpec)).toBe("a\nb\n--\nc\nd");
  });

  it("does nothing for a single line or an already sorted block", () => {
    expect(apply("only", undefined, sortLinesSpec)).toBeNull();
    expect(apply("a\nb", undefined, sortLinesSpec)).toBeNull();
  });
});

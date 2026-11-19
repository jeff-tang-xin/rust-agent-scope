import { describe, expect, it } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { resultMarks } from "../mergeGutters";
import { applyTo, fillAuto, mergeField, revertResolved, startMerge, summarize } from "../mergeModel";

// Line 2: a conflict; line 4: left only; after line 6: right inserts a line (a point mark).
const base = "a\nb\nc\nd\ne\nf\ng";
const left = "a\nB-left\nc\nD\ne\nf\ng";
const right = "a\nB-right\nc\nd\ne\nf\nNEW\ng";
const start = startMerge(left, base, right);

const result = () =>
  EditorState.create({ doc: start.text, extensions: [mergeField.init(() => start.changes), resultMarks] });

/** The marks as comparable text: position, class, sides and kind, sorted across the per-change sets. */
const marksOf = (state: EditorState) => {
  const out: string[] = [];
  for (const set of state.facet(EditorView.decorations)) {
    if (typeof set === "function") throw new Error("unexpected view-dependent decoration source");
    const cur = set.iter();
    while (cur.value) {
      const spec = cur.value.spec as { class?: string; widget?: { className?: string } };
      const cls = spec.class ?? spec.widget?.className ?? "?";
      // A widget is the insertion's point mark; a line decoration is a tint. Tag the kind so
      // assertions need not hard-code CodeMirror's internal side constants.
      const kind = spec.widget ? "widget" : "line";
      out.push(`${cur.from}:${cur.to}:${cls}:${cur.value.startSide}:${cur.value.endSide}:${kind}`);
      cur.next();
    }
  }
  return out.sort();
};

/** A full rebuild of the same document and changes: what the incremental field must equal. */
const rebuilt = (state: EditorState) =>
  EditorState.create({
    doc: state.doc,
    extensions: [mergeField.init(() => state.field(mergeField)), resultMarks],
  });

const check = (state: EditorState) => expect(marksOf(state)).toEqual(marksOf(rebuilt(state)));

describe("incremental result marks", () => {
  it("starts with a conflict tint, a change tint and a point for the insertion", () => {
    const marks = marksOf(result());
    expect(marks.some((m) => m.includes("cm-merge-conflict"))).toBe(true);
    expect(marks.some((m) => m.includes("cm-merge-change"))).toBe(true);
    // The insertion is a block-widget point mark, not a line tint.
    expect(marks.some((m) => m.endsWith(":widget"))).toBe(true);
    check(result());
  });

  it("matches a full rebuild after an edit inside a change", () => {
    let state = result();
    state = state.update({ changes: { from: 2, to: 3, insert: "bx" } }).state;
    check(state);
  });

  it("matches a full rebuild after an edit between changes, far from their lines", () => {
    let state = result();
    // Inside line 3 ("c"): outside the conflict's widened span, inside the left change's.
    state = state.update({ changes: { from: 5, insert: "x" } }).state;
    check(state);
  });

  it("matches a full rebuild after a newline joins or splits lines at a change's edge", () => {
    let state = result();
    state = state.update({ changes: { from: 1, to: 2 } }).state; // join lines 1 and 2
    check(state);
    state = state.update({ changes: { from: 0, insert: "top\n" } }).state; // shift everything
    check(state);
    state = state.update({ changes: { from: state.doc.length, insert: "\nend" } }).state;
    check(state);
  });

  it("matches a full rebuild through accept, resolve and revert decisions", () => {
    let state = result();
    state = state.update(applyTo(state, [0], { type: "accept", side: "left", resolve: false })!).state;
    check(state);
    state = state.update(applyTo(state, [0], { type: "accept", side: "right", resolve: true })!).state;
    check(state);
    state = state.update(applyTo(state, [1], { type: "ignore", side: null })!).state;
    check(state);
    state = state.update(revertResolved(state)!).state;
    check(state);
  });

  it("matches a full rebuild when fillAuto settles the model without editing text", () => {
    let state = result();
    const spec = fillAuto(state);
    expect(spec).not.toBeNull();
    state = state.update(spec!).state;
    check(state);
  });
});

describe("summarize cache", () => {
  it("returns the same summary while only the cursor moves, and refreshes it on edits and decisions", () => {
    let state = result();
    const first = summarize(state);
    expect(summarize(state)).toBe(first);
    state = state.update({ selection: { anchor: 1 } }).state;
    expect(summarize(state)).toBe(first);
    const edited = state.update({ changes: { from: 2, to: 3, insert: "bx" } }).state;
    expect(summarize(edited)).not.toBe(first);
    expect(summarize(edited)).toEqual(first); // the edit changed no decision
    const decided = edited.update(applyTo(edited, [1], { type: "ignore", side: null })!).state;
    expect(summarize(decided).pending).toBe(first.pending - 1);
    const filled = decided.update(fillAuto(decided)!).state;
    expect(summarize(filled)).not.toBe(summarize(decided));
  });
});

import { describe, expect, it, beforeEach } from "vitest";
import { ChangeSet, EditorState, Text } from "@codemirror/state";
import { interpolateLog, isConditional, makeBreakpoint, mapLines, toBackend, toggleLine } from "../breakpoints/model";
import {
  addBreakpoint,
  breakpointsOf,
  loadBreakpoints,
  removeBreakpoint,
  setMuted,
  toggleBreakpoint,
  updateBreakpoint,
  useBreakpoints,
} from "../breakpoints/store";

const doc = (lines: number) => Text.of(Array.from({ length: lines }, (_, i) => `line ${i}`));

function edit(text: Text, spec: { from: number; to?: number; insert?: string }) {
  const changes = ChangeSet.of([spec], text.length);
  return { changes, next: changes.apply(text) };
}

describe("breakpoint model", () => {
  it("toggles a line on and off, keeping the list sorted", () => {
    let list = toggleLine([], 5);
    list = toggleLine(list, 2);
    expect(list.map((b) => b.line)).toEqual([2, 5]);
    list = toggleLine(list, 5);
    expect(list.map((b) => b.line)).toEqual([2]);
  });

  it("treats conditions, hit counts and log messages as conditional", () => {
    expect(isConditional(makeBreakpoint(1))).toBe(false);
    expect(isConditional(makeBreakpoint(1, { condition: "x > 1" }))).toBe(true);
    expect(isConditional(makeBreakpoint(1, { condition: "  " }))).toBe(false);
    expect(isConditional(makeBreakpoint(1, { logMessage: "x={x}" }))).toBe(true);
    expect(isConditional(makeBreakpoint(1, { hitCondition: "3" }))).toBe(true);
  });

  it("maps lines down when lines are inserted above", () => {
    const text = doc(10);
    const list = [makeBreakpoint(3), makeBreakpoint(7)];
    // Insert two lines at the start of line 1.
    const { changes, next } = edit(text, { from: text.line(2).from, insert: "a\nb\n" });
    expect(mapLines(list, text, changes, next).map((b) => b.line)).toEqual([5, 9]);
  });

  it("moves a breakpoint with its line when text is inserted at the line start", () => {
    const text = doc(5);
    const list = [makeBreakpoint(2)];
    const { changes, next } = edit(text, { from: text.line(3).from, insert: "\n" });
    expect(mapLines(list, text, changes, next)[0]!.line).toBe(3);
  });

  it("keeps breakpoints above an edit and preserves identity", () => {
    const text = doc(5);
    const bp = makeBreakpoint(1);
    const { changes, next } = edit(text, { from: text.line(4).from, insert: "x\n" });
    const out = mapLines([bp], text, changes, next);
    expect(out[0]).toBe(bp);
  });

  it("maps up on deletion and merges breakpoints that collapse onto one line", () => {
    const text = doc(10);
    const list = [makeBreakpoint(2), makeBreakpoint(3), makeBreakpoint(8)];
    // Delete lines 2..3 (0-based) entirely.
    const { changes, next } = edit(text, { from: text.line(3).from, to: text.line(5).from });
    const out = mapLines(list, text, changes, next);
    expect(out.map((b) => b.line)).toEqual([2, 6]);
  });

  it("works through EditorState transactions", () => {
    const state = EditorState.create({ doc: "a\nb\nc\nd" });
    const tr = state.update({ changes: { from: 0, insert: "zero\n" } });
    const out = mapLines([makeBreakpoint(3)], state.doc, tr.changes, tr.state.doc);
    expect(out[0]!.line).toBe(4);
  });

  it("serialises for the backend, muting disables everything", () => {
    const list = [makeBreakpoint(1, { condition: " x ", enabled: true }), makeBreakpoint(4, { enabled: false })];
    expect(toBackend(list, false)).toEqual([
      { line: 1, condition: "x", hitCondition: null, logMessage: null, enabled: true },
      { line: 4, condition: null, hitCondition: null, logMessage: null, enabled: false },
    ]);
    expect(toBackend(list, true).every((b) => !b.enabled)).toBe(true);
  });
});

describe("suspend and logging (IDEA's independent options)", () => {
  it("sends non-suspending breakpoints as logpoints with the hit message and the evaluated template", () => {
    const list = [
      makeBreakpoint(1, { suspend: false, logHit: true }),
      makeBreakpoint(2, { suspend: false, logHit: true, logMessage: "i={i}" }),
      makeBreakpoint(3, { suspend: false }),
      makeBreakpoint(4, { logHit: true, logMessage: "i={i}" }),
    ];
    expect(toBackend(list, false, "/p/a.py")).toEqual([
      { line: 1, condition: null, hitCondition: null, logMessage: "Breakpoint reached at a.py:2", enabled: true },
      {
        line: 2,
        condition: null,
        hitCondition: null,
        logMessage: "Breakpoint reached at a.py:3\ni={i}",
        enabled: true,
      },
      // Logs nothing and never suspends: no DAP equivalent.
      { line: 3, condition: null, hitCondition: null, logMessage: null, enabled: false },
      // Suspending breakpoints log from the UI on stop.
      { line: 4, condition: null, hitCondition: null, logMessage: null, enabled: true },
    ]);
  });

  it("interpolates {expressions} in log templates", async () => {
    const out = await interpolateLog("a={a} b={ b }", async (e) => {
      if (e === "b") throw new Error("undefined");
      return e.toUpperCase();
    });
    expect(out).toBe("a=A b=<undefined>");
  });

  it("migrates stored log messages to non-suspending breakpoints", () => {
    localStorage.setItem(
      "openidea.debugger.breakpoints:/old",
      JSON.stringify({
        files: {
          "/x": [
            { id: "a", line: 1, logMessage: "v={v}" },
            { id: "b", line: 2 },
          ],
        },
      }),
    );
    loadBreakpoints("/old");
    expect(breakpointsOf("/x")[0]).toMatchObject({ suspend: false, logMessage: "v={v}" });
    expect(breakpointsOf("/x")[1]!.suspend).toBeUndefined();
  });
});

describe("breakpoint store", () => {
  beforeEach(() => {
    localStorage.clear();
    loadBreakpoints("/proj");
  });

  it("toggles, edits and removes per file", () => {
    toggleBreakpoint("/proj/a.py", 4);
    toggleBreakpoint("/proj/a.py", 1);
    toggleBreakpoint("/proj/b.py", 0);
    expect(breakpointsOf("/proj/a.py").map((b) => b.line)).toEqual([1, 4]);
    const bp = breakpointsOf("/proj/a.py")[1]!;
    updateBreakpoint("/proj/a.py", bp.id, { condition: "i == 3", enabled: false });
    expect(breakpointsOf("/proj/a.py")[1]).toMatchObject({ condition: "i == 3", enabled: false, line: 4 });
    removeBreakpoint("/proj/a.py", bp.id);
    expect(breakpointsOf("/proj/a.py").map((b) => b.line)).toEqual([1]);
    toggleBreakpoint("/proj/a.py", 1);
    expect(useBreakpoints.getState().byFile["/proj/a.py"]).toBeUndefined();
  });

  it("adds a conditional breakpoint replacing one on the same line", () => {
    toggleBreakpoint("/proj/a.py", 2);
    addBreakpoint("/proj/a.py", 2, { logMessage: "hit {i}" });
    const list = breakpointsOf("/proj/a.py");
    expect(list).toHaveLength(1);
    expect(list[0]!.logMessage).toBe("hit {i}");
  });

  it("persists per project, including mute", () => {
    toggleBreakpoint("/proj/a.py", 7);
    setMuted(true);
    loadBreakpoints("/other");
    expect(useBreakpoints.getState().byFile).toEqual({});
    loadBreakpoints("/proj");
    expect(breakpointsOf("/proj/a.py").map((b) => b.line)).toEqual([7]);
    expect(useBreakpoints.getState().muted).toBe(true);
  });

  it("ignores corrupt storage", () => {
    localStorage.setItem("openidea.debugger.breakpoints:/bad", "{nope");
    loadBreakpoints("/bad");
    expect(useBreakpoints.getState().byFile).toEqual({});
    localStorage.setItem(
      "openidea.debugger.breakpoints:/bad2",
      JSON.stringify({ files: { "/x": [{ line: "a" }, { line: 3 }] } }),
    );
    loadBreakpoints("/bad2");
    expect(breakpointsOf("/x").map((b) => b.line)).toEqual([3]);
  });
});

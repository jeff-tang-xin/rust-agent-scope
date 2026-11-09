import { beforeEach, describe, expect, it } from "vitest";
import { ChangeSet, Text } from "@codemirror/state";
import { mapLines } from "../breakpoints/model";
import {
  addFieldWatchpoint,
  addMethodBreakpoint,
  loadSpecialBreakpoints,
  removeAllSpecial,
  sanitizeSpecial,
  setSpecialLines,
  specialAt,
  toDataBreakpoints,
  toFunctionBreakpoints,
  toggleMethodAt,
  updateSpecial,
  useSpecialBreakpoints,
} from "../breakpoints/special";
import { consumeTemporary, breakpointsOf, loadBreakpoints, toggleTemporaryBreakpoint } from "../breakpoints/store";
import { toggleBreakpointSmart } from "../breakpoints/toggle";

beforeEach(() => {
  localStorage.clear();
  loadBreakpoints("/p");
  loadSpecialBreakpoints("/p");
  removeAllSpecial();
});

describe("method breakpoints and field watchpoints", () => {
  it("toggles a method breakpoint on its declaration line and persists it", () => {
    const bp = toggleMethodAt("/p/A.java", 4, "a.A#run");
    expect(bp?.name).toBe("a.A#run");
    expect(specialAt("/p/A.java").map((b) => b.line)).toEqual([4]);
    loadSpecialBreakpoints("/p");
    expect(useSpecialBreakpoints.getState().methods.map((m) => m.name)).toEqual(["a.A#run"]);
    expect(toggleMethodAt("/p/A.java", 4, "a.A#run")).toBeNull();
    expect(specialAt("/p/A.java")).toEqual([]);
  });

  it("sends enabled state, conditions and muting to the backend lists", () => {
    const m = addMethodBreakpoint("main.run");
    updateSpecial(m.id, { condition: " n > 1 ", enabled: false });
    const f = addFieldWatchpoint({ dataId: "a.A#count", label: "A.count", accessType: "write" });
    expect(addFieldWatchpoint({ dataId: "a.A#count", label: "dup" }).id).toBe(f.id);
    const { methods, fields } = useSpecialBreakpoints.getState();
    expect(toFunctionBreakpoints(methods, false)).toEqual([
      { name: "main.run", condition: "n > 1", hitCondition: null, enabled: false },
    ]);
    expect(toDataBreakpoints(fields, false)).toEqual([
      { dataId: "a.A#count", accessType: "write", condition: null, hitCondition: null, enabled: true },
    ]);
    expect(toDataBreakpoints(fields, true)[0]!.enabled).toBe(false);
  });

  it("follows edits and drops sanitized junk", () => {
    const m = toggleMethodAt("/p/a.py", 3, "run")!;
    const doc = Text.of(["a", "b", "c", "def run():", "  pass"]);
    const changes = ChangeSet.of([{ from: 0, insert: "x\ny\n" }], doc.length);
    setSpecialLines("/p/a.py", mapLines([{ id: m.id, line: 3 }], doc, changes, changes.apply(doc)));
    expect(specialAt("/p/a.py")[0]!.line).toBe(5);
    expect(
      sanitizeSpecial({ methods: [{ name: "" }, { name: "ok", line: 2 }, 3], fields: [{ label: "no id" }] }),
    ).toEqual({
      methods: [expect.objectContaining({ name: "ok", line: 2, enabled: true, kind: "method" })],
      fields: [],
    });
  });

  it("toggles like IDEA: Java declarations get method / field breakpoints, other lines line breakpoints", () => {
    const src = "package a;\nclass A {\n  int count;\n  void run() {\n    count++;\n  }\n}";
    toggleBreakpointSmart("/p/A.java", 3, "java", src);
    toggleBreakpointSmart("/p/A.java", 2, "java", src);
    toggleBreakpointSmart("/p/A.java", 4, "java", src);
    expect(useSpecialBreakpoints.getState().methods.map((m) => m.name)).toEqual(["a.A#run"]);
    expect(useSpecialBreakpoints.getState().fields.map((f) => f.dataId)).toEqual(["a.A#count"]);
    expect(breakpointsOf("/p/A.java").map((b) => b.line)).toEqual([4]);
    // Again on the declaration: removed.
    toggleBreakpointSmart("/p/A.java", 3, "java", src);
    expect(useSpecialBreakpoints.getState().methods).toEqual([]);
    // Python `def` lines stay line breakpoints.
    toggleBreakpointSmart("/p/a.py", 0, "python", "def run():\n  pass");
    expect(breakpointsOf("/p/a.py").map((b) => b.line)).toEqual([0]);
  });
});

describe("temporary line breakpoints", () => {
  it("are removed after the first hit", () => {
    toggleTemporaryBreakpoint("/p/a.py", 7);
    expect(breakpointsOf("/p/a.py")[0]).toMatchObject({ line: 7, temporary: true });
    expect(consumeTemporary("/p/a.py", 6)).toBe(false);
    expect(consumeTemporary("/p/a.py", 7)).toBe(true);
    expect(breakpointsOf("/p/a.py")).toEqual([]);
  });

  it("survive a reload and toggle off", () => {
    toggleTemporaryBreakpoint("/p/a.py", 2);
    loadBreakpoints("/p");
    expect(breakpointsOf("/p/a.py")[0]!.temporary).toBe(true);
    toggleTemporaryBreakpoint("/p/a.py", 2);
    expect(breakpointsOf("/p/a.py")).toEqual([]);
  });
});

import { describe, expect, it } from "vitest";
import { EditHistory } from "./editHistory";

const at = (path: string, line: number, col = 0) => ({ path, line, col });

describe("EditHistory (Last Edit Location)", () => {
  it("goes to the newest edit, then walks back through older ones", () => {
    const h = new EditHistory();
    h.record(at("/a.ts", 10));
    h.record(at("/b.ts", 3));
    h.record(at("/a.ts", 40, 2));
    expect(h.back(at("/c.ts", 0))).toEqual(at("/a.ts", 40, 2));
    expect(h.back(at("/a.ts", 40))).toEqual(at("/b.ts", 3));
    expect(h.back(at("/b.ts", 3))).toEqual(at("/a.ts", 10));
    expect(h.back(at("/a.ts", 10))).toBeNull();
    expect(h.forward()).toEqual(at("/b.ts", 3));
  });

  it("skips the newest place when the caret is already there", () => {
    const h = new EditHistory();
    h.record(at("/a.ts", 1));
    h.record(at("/a.ts", 20));
    expect(h.back(at("/a.ts", 20))).toEqual(at("/a.ts", 1));
  });

  it("merges nearby edits and resets the walk on a new edit", () => {
    const h = new EditHistory();
    h.record(at("/a.ts", 10));
    h.record(at("/a.ts", 12, 4));
    expect(h.entries).toEqual([at("/a.ts", 12, 4)]);
    h.record(at("/b.ts", 0));
    expect(h.back(null)).toEqual(at("/b.ts", 0));
    h.record(at("/a.ts", 11));
    expect(h.entries.map((e) => e.path)).toEqual(["/b.ts", "/a.ts"]);
    expect(h.back(null)).toEqual(at("/a.ts", 11));
  });

  it("drops places of deleted files", () => {
    const h = new EditHistory();
    h.record(at("/a.ts", 1));
    h.record(at("/b.ts", 1));
    h.removePath("/b.ts");
    expect(h.back(null)).toEqual(at("/a.ts", 1));
  });
});

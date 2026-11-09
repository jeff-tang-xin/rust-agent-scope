import { describe, expect, it } from "vitest";
import type { HierarchyItem, OutlineItem, Pos } from "@/platform/ipc";
import { inheritedMembers, typeKey, typesIn } from "../structure/inherited";
import { keyOutline, symbolPathAt, withInherited } from "../structure/outline";

const item = (name: string, kind: OutlineItem["kind"], line: number, children: OutlineItem[] = []): OutlineItem => ({
  name,
  detail: null,
  kind,
  range: { start: { line, col: 0 }, end: { line: line + 5, col: 1 } },
  selection_range: { start: { line, col: 6 }, end: { line, col: 6 + name.length } },
  children,
});

const hier = (name: string, path: string, line: number): HierarchyItem => ({
  name,
  kind: "class",
  detail: null,
  path,
  range: { start: { line, col: 0 }, end: { line: line + 5, col: 1 } },
  selectionRange: { start: { line, col: 6 }, end: { line, col: 6 + name.length } },
  fromRanges: [],
});

const dog = item("Dog", "class", 0, [item("bark", "method", 1), item("eat()", "method", 2)]);
const outlines: Record<string, OutlineItem[]> = {
  "/p/Dog.java": [dog],
  "/p/Animal.java": [
    item("Animal", "class", 0, [
      item("Animal", "constructor", 1),
      item("eat(String)", "method", 2),
      item("sleep()", "method", 3),
      item("sleep(int)", "method", 4),
    ]),
  ],
  "/p/Living.java": [item("Living", "interface", 0, [item("sleep()", "method", 1), item("age", "field", 2)])],
};
const supers: Record<string, HierarchyItem[]> = {
  "/p/Dog.java:0": [hier("Animal", "/p/Animal.java", 0)],
  "/p/Animal.java:0": [hier("Living", "/p/Living.java", 0)],
};
const deps = {
  supertypes: async (path: string, pos: Pos) => supers[`${path}:${pos.line}`] ?? [],
  outline: async (path: string) => outlines[path] ?? [],
};

describe("Show Inherited", () => {
  it("collects supertype members level by level, without overridden ones or constructors", async () => {
    const members = await inheritedMembers("/p/Dog.java", dog, deps);
    expect(members.map((m) => `${m.owner}.${m.item.name}`)).toEqual([
      "Animal.sleep()",
      "Animal.sleep(int)",
      "Living.age",
    ]);
    expect(members[0]!.path).toBe("/p/Animal.java");
  });

  it("finds nested types and appends inherited members after a type's own ones", async () => {
    const outline = [item("Outer", "class", 0, [item("Inner", "class", 1)])];
    expect(typesIn(outline).map((t) => t.name)).toEqual(["Outer", "Inner"]);

    const members = await inheritedMembers("/p/Dog.java", dog, deps);
    const nodes = withInherited(keyOutline([dog]), new Map([[typeKey(dog), members]]));
    expect(nodes[0]!.children.map((c) => [c.item.name, c.inherited?.owner ?? null])).toEqual([
      ["bark", null],
      ["eat()", null],
      ["sleep()", "Animal"],
      ["sleep(int)", "Animal"],
      ["age", "Living"],
    ]);
    // Inherited members live in other files: the caret never selects them.
    const empty = item("Puppy", "class", 0);
    const bare = withInherited(keyOutline([empty]), new Map([[typeKey(empty), members]]));
    expect(bare[0]!.children).toHaveLength(3);
    expect(symbolPathAt(bare, { line: 3, col: 0 })).toEqual([bare[0]!.key]);
  });
});

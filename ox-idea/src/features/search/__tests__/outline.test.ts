import { describe, expect, it } from "vitest";
import type { OutlineItem } from "@/platform/ipc";
import {
  allKeys,
  applyViewFilter,
  filterOutline,
  hasFields,
  hasVisibility,
  keyOutline,
  outlinePathAt,
  parentKeys,
  sortOutline,
  symbolPathAt,
  visibleCount,
} from "../structure/outline";

const item = (
  name: string,
  kind: OutlineItem["kind"],
  from: number,
  to: number,
  children: OutlineItem[] = [],
): OutlineItem => ({
  name,
  detail: null,
  kind,
  range: { start: { line: from, col: 0 }, end: { line: to, col: 1 } },
  selection_range: { start: { line: from, col: 4 }, end: { line: from, col: 4 + name.length } },
  children,
});

const outline = [
  item("OrderController", "class", 0, 20, [
    item("list", "method", 2, 6),
    item("create", "method", 8, 12),
    item("list", "method", 14, 16),
  ]),
  item("helper", "function", 22, 25),
];

describe("structure view options", () => {
  const vis = (i: OutlineItem, visibility: OutlineItem["visibility"]): OutlineItem => ({ ...i, visibility });
  const cls = item("Repo", "class", 0, 30, [
    vis(item("cache", "field", 1, 1), "private"),
    vis(item("find", "method", 2, 5), "public"),
    vis(item("load", "method", 6, 9), "private"),
    vis(item("check", "method", 10, 12), "protected"),
    item("size", "property", 13, 13),
    item("all", "method", 14, 16),
  ]);
  const names = (nodes: ReturnType<typeof keyOutline>) => nodes[0]!.children.map((n) => n.item.name);

  it("hides fields and non-public symbols", () => {
    const nodes = keyOutline([cls]);
    expect(names(applyViewFilter(nodes, { fields: false, nonPublic: true }))).toEqual(["find", "load", "check", "all"]);
    expect(names(applyViewFilter(nodes, { fields: true, nonPublic: false }))).toEqual(["find", "size", "all"]);
    expect(applyViewFilter(nodes, { fields: true, nonPublic: true })).toBe(nodes);
  });

  it("sorts by visibility, then by name when alphabetical", () => {
    const nodes = keyOutline([cls]);
    expect(names(sortOutline(nodes, false, true))).toEqual(["find", "size", "all", "check", "cache", "load"]);
    expect(names(sortOutline(nodes, true, true))).toEqual(["all", "find", "size", "check", "cache", "load"]);
  });

  it("offers options only when the outline has the data", () => {
    expect(hasVisibility([cls])).toBe(true);
    expect(hasVisibility(outline)).toBe(false);
    expect(hasFields([cls])).toBe(true);
    expect(hasFields(outline)).toBe(false);
  });

  it("finds the symbols around a position", () => {
    expect(outlinePathAt(outline, { line: 9, col: 0 }).map((i) => i.name)).toEqual(["OrderController", "create"]);
    expect(outlinePathAt(outline, { line: 40, col: 0 })).toEqual([]);
  });
});

describe("outline helpers", () => {
  const nodes = keyOutline(outline);

  it("gives stable unique keys to same-named siblings", () => {
    const keys = nodes[0]!.children.map((c) => c.key);
    expect(new Set(keys).size).toBe(3);
    expect(keys[2]).toMatch(/#1$/);
    expect(allKeys(nodes)).toEqual([nodes[0]!.key]);
  });

  it("finds the innermost symbol around a position", () => {
    const path = symbolPathAt(nodes, { line: 9, col: 3 });
    expect(path).toEqual([nodes[0]!.key, nodes[0]!.children[1]!.key]);
    expect(symbolPathAt(nodes, { line: 21, col: 0 })).toEqual([]);
  });

  it("sorts alphabetically, recursively", () => {
    const sorted = sortOutline(nodes, true);
    expect(sorted.map((n) => n.item.name)).toEqual(["helper", "OrderController"]);
    expect(sorted[1]!.children.map((n) => n.item.name)).toEqual(["create", "list", "list"]);
    expect(sortOutline(nodes, false)).toBe(nodes);
  });

  it("narrows to matches and their ancestors", () => {
    const f = filterOutline(nodes, "creat", true);
    expect(f.nodes.map((n) => [n.item.name, n.children.map((c) => c.item.name)])).toEqual([
      ["OrderController", ["create"]],
    ]);
    expect(f.best).toBe(nodes[0]!.children[1]!.key);
    expect(f.matches.has(nodes[0]!.key)).toBe(false);
    expect(filterOutline(nodes, "", true).nodes).toBe(nodes);
  });

  it("matches word and camel-hump starts, not scattered letters", () => {
    const loose = keyOutline([item("items.reduce() callback", "function", 0, 1), item("subtotal", "method", 2, 3)]);
    expect(filterOutline(loose, "sub", true).nodes.map((n) => n.item.name)).toEqual(["subtotal"]);
    expect(filterOutline(nodes, "OC", true).nodes.map((n) => n.item.name)).toEqual(["OrderController"]);
  });

  it("only highlights matches when not narrowing down", () => {
    const f = filterOutline(nodes, "list", false);
    expect(f.nodes).toBe(nodes);
    expect(f.order).toEqual([nodes[0]!.children[0]!.key, nodes[0]!.children[2]!.key]);
  });

  it("counts visible rows and maps parents", () => {
    expect(visibleCount(nodes, new Set())).toBe(5);
    expect(visibleCount(nodes, new Set([nodes[0]!.key]))).toBe(2);
    expect(parentKeys(nodes).get(nodes[0]!.children[1]!.key)).toBe(nodes[0]!.key);
  });
});

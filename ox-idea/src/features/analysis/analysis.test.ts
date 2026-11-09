import { describe, expect, it } from "vitest";
import type { Diagnostic, StructuralFile } from "@/platform/ipc";
import { useCustomScopes } from "@/platform/search";
import { fragmentLines } from "./duplicates";
import { groupInspection, kindOf } from "./inspect";
import { buildInspectionNodes } from "./InspectionView";
import { resolveScope } from "./scope";
import { buildReplaceEdit, matchKey, matchLocations } from "./structural";

const diag = (
  line: number,
  severity: Diagnostic["severity"],
  source: string | null,
  code: string | null = null,
): Diagnostic => ({
  range: { start: { line, col: 2 }, end: { line, col: 5 } },
  severity,
  message: `problem at ${line}`,
  source,
  code,
  unnecessary: false,
  deprecated: false,
});

describe("Inspect Code grouping", () => {
  const files = [
    { path: "/p/b.ts", relPath: "b.ts", diagnostics: [diag(9, "error", "ts", "2304"), diag(1, "error", "ts", "2304")] },
    {
      path: "/p/a.ts",
      relPath: "a.ts",
      diagnostics: [diag(3, "warning", "eslint", "no-unused-vars"), diag(4, "hint", null)],
    },
    { path: "/p/c.ts", relPath: "c.ts", diagnostics: [diag(0, "error", "ts", "2304"), diag(2, "error", "ts", "7006")] },
  ];

  it("groups by severity, then inspection, then file, sorted", () => {
    const groups = groupInspection(files);
    expect(groups.map((g) => [g.severity, g.count])).toEqual([
      ["error", 4],
      ["warning", 1],
      ["info", 1],
    ]);
    const errors = groups[0]!;
    expect(errors.kinds.map((k) => [k.kind, k.count])).toEqual([
      ["ts(2304)", 3],
      ["ts(7006)", 1],
    ]);
    const ts2304 = errors.kinds[0]!;
    expect(ts2304.files.map((f) => f.relPath)).toEqual(["b.ts", "c.ts"]);
    expect(ts2304.files[0]!.problems.map((p) => p.d.range.start.line)).toEqual([1, 9]);
    expect(groups[2]!.kinds[0]!.kind).toBe("Language server");
  });

  it("names inspections by source and code", () => {
    expect(kindOf(diag(0, "error", "eslint", "eqeqeq"))).toBe("eslint(eqeqeq)");
    expect(kindOf(diag(0, "error", "rustc"))).toBe("rustc");
  });

  it("builds a tree with unique keys", () => {
    const nodes = buildInspectionNodes(groupInspection(files));
    const keys: string[] = [];
    const visit = (ns: typeof nodes) => {
      for (const n of ns) {
        keys.push(n.key);
        if (n.kind !== "problem") visit(n.children);
      }
    };
    visit(nodes);
    expect(new Set(keys).size).toBe(keys.length);
    expect(nodes[0]!.kind).toBe("severity");
  });

  it("groups by inspection by default, worst severity first, merging severities", () => {
    const withWarning = [
      ...files,
      { path: "/p/d.ts", relPath: "d.ts", diagnostics: [diag(5, "warning", "ts", "2304")] },
    ];
    const nodes = buildInspectionNodes(groupInspection(withWarning), false);
    expect(nodes.map((n) => (n.kind === "inspection" ? [n.name, n.severity, n.count] : n.kind))).toEqual([
      ["ts(2304)", "error", 4],
      ["ts(7006)", "error", 1],
      ["eslint(no-unused-vars)", "warning", 1],
      ["Language server", "info", 1],
    ]);
    const ts2304 = nodes[0]!;
    expect(ts2304.kind !== "problem" && ts2304.children.map((f) => (f.kind === "file" ? f.path : ""))).toEqual([
      "/p/b.ts",
      "/p/c.ts",
      "/p/d.ts",
    ]);
  });
});

describe("Structural replace", () => {
  const files: StructuralFile[] = [
    {
      path: "/p/a.rs",
      relPath: "a.rs",
      matches: [
        {
          range: { start: { line: 1, col: 4 }, end: { line: 1, col: 14 } },
          text: "x.unwrap()",
          lineText: "",
          replacement: "x?",
        },
        {
          range: { start: { line: 2, col: 4 }, end: { line: 2, col: 14 } },
          text: "y.unwrap()",
          lineText: "",
          replacement: "y?",
        },
      ],
    },
    {
      path: "/p/b.rs",
      relPath: "b.rs",
      matches: [
        {
          range: { start: { line: 0, col: 0 }, end: { line: 0, col: 10 } },
          text: "z.unwrap()",
          lineText: "",
          replacement: "z?",
        },
      ],
    },
  ];

  it("turns included matches into one workspace edit", () => {
    const all = buildReplaceEdit(files, new Set());
    expect(all.files).toHaveLength(2);
    const skipped = buildReplaceEdit(files, new Set([matchKey("/p/a.rs", 0), matchKey("/p/b.rs", 0)]));
    expect(skipped).toEqual({
      files: [{ kind: "edit", path: "/p/a.rs", edits: [{ range: files[0]!.matches[1]!.range, new_text: "y?" }] }],
    });
  });

  it("lists match locations for the usages view", () => {
    expect(matchLocations(files).map((l) => [l.path, l.range.start.line])).toEqual([
      ["/p/a.rs", 1],
      ["/p/a.rs", 2],
      ["/p/b.rs", 0],
    ]);
  });
});

describe("analysis scopes and fragments", () => {
  it("resolves scopes or explains why not", async () => {
    expect(await resolveScope({ kind: "project", directory: "" })).toEqual({ kind: "project" });
    expect(await resolveScope({ kind: "directory", directory: " /p/src " })).toEqual({
      kind: "directory",
      path: "/p/src",
    });
    expect(await resolveScope({ kind: "directory", directory: "" })).toBe("Choose a directory");
    expect(await resolveScope({ kind: "file", directory: "" })).toBe("No file is open");
    expect(await resolveScope({ kind: "changed", directory: "" })).toBe("There are no uncommitted files");
    expect(await resolveScope({ kind: "custom", directory: "" })).toBe("Choose a scope");
    useCustomScopes.setState({ root: null, scopes: [{ name: "Src", pattern: "file:src//*" }] });
    expect(await resolveScope({ kind: "custom", directory: "", custom: "Src" })).toEqual({
      kind: "pattern",
      pattern: "file:src//*",
    });
    useCustomScopes.setState({ root: null, scopes: [] });
  });

  it("numbers fragment lines from the fragment's first line", () => {
    expect(fragmentLines("a\r\nb\nc", 9)).toEqual([
      { no: 10, text: "a" },
      { no: 11, text: "b" },
      { no: 12, text: "c" },
    ]);
  });
});

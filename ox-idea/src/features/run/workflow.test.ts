import { describe, expect, it } from "vitest";
import { ConsoleModel } from "./console";
import { foldRows } from "./ConsoleView";
import { expandMacros, toolRunConfig } from "./externalTools";
import { DEFAULT_EXTERNAL_TOOL } from "@/platform/workflowSettings";

const ctx = {
  projectRoot: "/p",
  projectName: "demo",
  filePath: "/p/src/main.rs",
  line: 3,
  column: 5,
  selectedText: "sel",
};

describe("console settings", () => {
  it("keeps the cycle buffer under the character limit", () => {
    const m = new ConsoleModel({ maxChars: 100 });
    for (let i = 0; i < 50; i++) m.append({ kind: "stdout", text: `line ${i}\n` });
    const chars = m.lines.reduce((n, l) => n + l.text.length + 1, 0);
    expect(chars).toBeLessThanOrEqual(100);
    expect(m.lines.at(-1)?.text).toBe("line 49");
    expect(m.text()).toContain("line 49");
    expect(m.dropped).toBeGreaterThan(0);
  });

  it("folds runs of matching lines onto the previous line, single lines too", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stderr", text: "Error\n  at org.junit.A\n  at org.junit.B\n  at app.Main\n  at org.junit.C\n" });
    const rows = foldRows(m.lines, ["at org.junit."], new Set());
    const show = (rs: typeof rows) =>
      rs.map((r) =>
        r.kind === "fold" ? `fold${r.fold.count}` : m.lines[r.index]!.text.trim() + (r.fold ? ` +${r.fold.count}` : ""),
      );
    expect(show(rows)).toEqual(["Error +2", "at app.Main +1"]);
    const first = rows[0]!.kind === "line" ? rows[0]!.fold! : null;
    const open = foldRows(m.lines, ["at org.junit."], new Set([first!.id]));
    expect(show(open)).toEqual(["Error +2", "at org.junit.A", "at org.junit.B", "at app.Main +1"]);
    expect(open[0]!.kind === "line" && open[0]!.fold?.expanded).toBe(true);
  });

  it("keeps a fold with no line before it as a row of its own", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "  at org.junit.A\nok\n" });
    const rows = foldRows(m.lines, ["at org.junit."], new Set());
    expect(rows.map((r) => r.kind)).toEqual(["fold", "line"]);
  });

  it("exposes the open (prompt) line", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "done\nName: " });
    expect(m.openLine?.text).toBe("Name: ");
    m.append({ kind: "stdout", text: "x\n" });
    expect(m.openLine).toBeNull();
  });
});

describe("external tools", () => {
  it("expands IDEA macros", () => {
    expect(expandMacros("$FileName$ $FileNameWithoutExtension$ $FileExt$ $FileRelativePath$", ctx)).toBe(
      "main.rs main rs src/main.rs",
    );
    expect(expandMacros("$ProjectFileDir$:$LineNumber$:$ColumnNumber$ $Unknown$ $Prompt$", ctx, "x")).toBe(
      "/p:3:5 $Unknown$ x",
    );
  });

  it("builds a run configuration with per-argument expansion", () => {
    const tool = {
      ...DEFAULT_EXTERNAL_TOOL,
      id: "t",
      name: "Fmt",
      program: "rustfmt",
      arguments: `--edition 2021 "$FilePath$"`,
    };
    expect(toolRunConfig(tool, { ...ctx, filePath: "/p/my dir/a.rs" })).toMatchObject({
      id: "external:t",
      command: "rustfmt",
      args: ["--edition", "2021", "/p/my dir/a.rs"],
      cwd: "/p",
    });
  });
});

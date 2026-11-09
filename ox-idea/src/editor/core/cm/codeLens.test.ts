import { describe, expect, it } from "vitest";
import { EditorState } from "@codemirror/state";
import { declarationStart, groupLenses, indentColumns } from "./codeLens";

const doc = (text: string) => EditorState.create({ doc: text });

describe("code lens layout", () => {
  it("places lenses above attributes, annotations and doc comments", () => {
    const s = doc(
      "mod a {\n    /// Docs\n    #[test]\n    #[ignore]\n    fn t() {}\n}\n@Test\nvoid x() {}\n/**\n * doc\n */\nfunction f() {}",
    );
    expect(s.doc.lineAt(declarationStart(s, 5)).number).toBe(2);
    expect(s.doc.lineAt(declarationStart(s, 8)).number).toBe(7);
    expect(s.doc.lineAt(declarationStart(s, 12)).number).toBe(9);
    expect(s.doc.lineAt(declarationStart(s, 1)).number).toBe(1);
  });

  it("groups lenses per line in order", () => {
    const s = doc("fn a() {}\nfn b() {}");
    const g = groupLenses(s, [
      { pos: 12, title: "Run", order: 200 },
      { pos: 3, title: "2 usages", order: 10 },
      { pos: 10, title: "1 usage", order: 10 },
      { pos: 99, title: "out of range" },
    ]);
    expect(g.map((x) => [x.lineFrom, x.lenses.map((l) => l.title)])).toEqual([
      [0, ["2 usages"]],
      [10, ["1 usage", "Run"]],
    ]);
  });

  it("measures indentation with tabs", () => {
    expect(indentColumns("\t  x", 4)).toBe(6);
    expect(indentColumns("    x", 4)).toBe(4);
  });
});

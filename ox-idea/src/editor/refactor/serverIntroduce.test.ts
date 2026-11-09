import { describe, expect, it } from "vitest";
import type { CodeAction } from "@/bindings";
import { introducedName, occurrenceVariants } from "./serverIntroduce";

const spansText = (text: string, spans: { from: number; to: number }[]) => spans.map((s) => text.slice(s.from, s.to));

describe("introducedName", () => {
  it("finds the server's variable: declaration and replaced occurrence", () => {
    const before = "function f() {\n  return price * 2 + 1;\n}\n";
    const after = "function f() {\n  const newLocal = price * 2;\n  return newLocal + 1;\n}\n";
    const found = introducedName(before, after)!;
    expect(found.name).toBe("newLocal");
    expect(found.spans).toHaveLength(2);
    expect(spansText(after, found.spans)).toEqual(["newLocal", "newLocal"]);
  });

  it("ignores names that were already there and keywords / types written once", () => {
    const before = "void f() {\n  g(a.b(), a.b());\n}\n";
    const after = "void f() {\n  String b = a.b();\n  g(b, b);\n}\n";
    const found = introducedName(before, after)!;
    expect(found.name).toBe("b");
    // `b` already existed in `a.b()`; only the three new tokens count.
    expect(found.spans.map((s) => after.slice(s.from - 1, s.to + 1))).toEqual([" b ", "(b,", " b)"]);
  });

  it("handles an extracted function placed after its call", () => {
    const before = "fn main() {\n    let x = 1 + 2;\n}\n";
    const after = "fn main() {\n    let x = fun_name();\n}\n\nfn fun_name() -> i32 {\n    1 + 2\n}\n";
    expect(introducedName(before, after)?.name).toBe("fun_name");
  });

  it("is null without a new name used twice", () => {
    expect(introducedName("a + b", "(a + b)")).toBeNull();
    expect(introducedName("x", "x")).toBeNull();
  });
});

describe("occurrenceVariants", () => {
  const action = (title: string): CodeAction => ({
    title,
    kind: "refactor.extract.variable",
    isPreferred: false,
    disabledReason: null,
    edit: null,
    command: null,
    data: null,
    server: 1,
  });

  it("pairs the single and the all-occurrences action", () => {
    const one = action("Extract to local variable");
    const all = action("Extract to local variable (replace all occurrences)");
    expect(occurrenceVariants([one, all])).toEqual({ one, all });
    expect(occurrenceVariants([one])).toBeNull();
    expect(occurrenceVariants([one, action("Extract to field"), all])).toBeNull();
  });
});

import { describe, expect, it } from "vitest";
import { extractTypeInfo, typeFromSignature } from "./typeInfo";
import { IMPLEMENT_TITLE, OVERRIDE_TITLE } from "./actions";

const md = (value: string) => ({ kind: "markdown", value });

describe("expression type info", () => {
  it("splits the type off variable signatures", () => {
    expect(typeFromSignature("(local var) x: number")).toBe("number");
    expect(typeFromSignature("let mut v: Vec<i32>")).toBe("Vec<i32>");
    expect(typeFromSignature("(property) a.b?: string")).toBe("string");
    expect(typeFromSignature("const f: (a: number) => void")).toBe("(a: number) => void");
    expect(typeFromSignature("x: int = 3")).toBe("int");
    expect(typeFromSignature("var count int")).toBe("int");
    expect(typeFromSignature("fn add(a: i32, b: i32) -> i32")).toBe("fn add(a: i32, b: i32) -> i32");
    expect(typeFromSignature("String name")).toBe("String name");
  });

  it("reads the signature block of a hover, skipping module paths", () => {
    expect(extractTypeInfo(md("```rust\ncrate::util\n```\n\n```rust\nlet n: usize\n```\n---\nDocs"))).toEqual({
      text: "usize",
      language: "rust",
    });
    expect(extractTypeInfo(md("```typescript\nconst a: string[]\n```"))).toEqual({
      text: "string[]",
      language: "typescript",
    });
    expect(extractTypeInfo(md("Just prose with `T` inline"))).toEqual({ text: "T", language: null });
    expect(extractTypeInfo({ kind: "plaintext", value: "\nint x\nmore" })).toEqual({ text: "int x", language: null });
    expect(extractTypeInfo(md("   "))).toBeNull();
    expect(extractTypeInfo(null)).toBeNull();
  });
});

describe("implement / override titles", () => {
  it.each([
    ["Implement missing members", true, false],
    ["Implement interface 'Runnable'", true, false],
    ["Add missing trait items", true, false],
    ["Override/Implement Methods...", true, true],
    ["Override toString()", false, true],
    ["Extract method", false, false],
    ["Add import", false, false],
  ])("%s", (title, implement, override) => {
    expect(IMPLEMENT_TITLE.test(title)).toBe(implement);
    expect(OVERRIDE_TITLE.test(title)).toBe(override);
  });
});

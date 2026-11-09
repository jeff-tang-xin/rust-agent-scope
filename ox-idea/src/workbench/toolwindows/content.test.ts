import { describe, expect, it } from "vitest";
import { closeSelectedContent, contentTabRegistry } from "./content";

describe("tool window content tabs", () => {
  it("closes the selected tab of the first strip that can, else reports false", () => {
    const closed: string[] = [];
    const register = contentTabRegistry("run");
    expect(contentTabRegistry("run")).toBe(register);
    const disposeA = register({ closeSelected: () => false });
    const disposeB = register({ closeSelected: () => (closed.push("b"), true) });
    expect(closeSelectedContent("run")).toBe(true);
    expect(closed).toEqual(["b"]);
    disposeB();
    expect(closeSelectedContent("run")).toBe(false);
    disposeA();
    expect(closeSelectedContent("other")).toBe(false);
  });
});

import { describe, expect, it } from "vitest";
import { ValueChanges } from "../ui/VariablesView";

describe("changed values (IDEA highlights them after a step)", () => {
  it("compares with the previous stop in the same scope only", () => {
    const c = new ValueChanges();
    c.enter("t1:0", 1);
    c.record("t1:0", "local:i", "1");
    c.record("t1:0", "local:s", "'a'");
    expect(c.changed("t1:0", "local:i", "1")).toBe(false);

    c.enter("t1:0", 2);
    expect(c.changed("t1:0", "local:i", "2")).toBe(true);
    expect(c.changed("t1:0", "local:s", "'a'")).toBe(false);
    // New variables aren't "changed".
    expect(c.changed("t1:0", "local:n", "0")).toBe(false);
    // Another frame depth has its own baseline.
    expect(c.changed("t1:1", "local:i", "2")).toBe(false);

    // Re-rendering in the same stop keeps the baseline.
    c.record("t1:0", "local:i", "2");
    c.enter("t1:0", 2);
    expect(c.changed("t1:0", "local:i", "2")).toBe(true);
    c.enter("t1:0", 3);
    expect(c.changed("t1:0", "local:i", "2")).toBe(false);
  });
});

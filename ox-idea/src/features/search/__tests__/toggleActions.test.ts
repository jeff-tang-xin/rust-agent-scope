import { describe, expect, it } from "vitest";
import { actionToggled, executeAction, getAction, registerAction } from "@/platform/actions";

describe("toggle actions", () => {
  it("reports state only for actions with `toggled`", async () => {
    let on = false;
    const dispose = registerAction({ id: "test.toggle", title: "T", toggled: () => on, run: () => void (on = !on) });
    registerAction({ id: "test.plain", title: "P", run: () => undefined });
    expect(actionToggled(getAction("test.toggle"))).toBe(false);
    await executeAction("test.toggle");
    expect(actionToggled(getAction("test.toggle"))).toBe(true);
    expect(actionToggled(getAction("test.plain"))).toBeUndefined();
    dispose();
  });
});

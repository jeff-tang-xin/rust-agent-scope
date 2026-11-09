import { describe, expect, it } from "vitest";
import { assignedSetting } from "../keymap/AssignShortcut";
import { findConflicts } from "../keymap/model";

const actions = [
  { id: "file.save", title: "Save All" },
  { id: "my.action", title: "My Action" },
];

describe("Assign Shortcut (Search Everywhere ⌥↵)", () => {
  it("adds the binding as an override of a bundled keymap", () => {
    const s = { keymap: "intellij-macos", keymapOverrides: {}, customKeymaps: {} };
    expect(assignedSetting(s, "my.action", "Alt+Meta+J", [], false)).toEqual({
      key: "keymapOverrides",
      value: { "my.action": ["Alt+Meta+J"] },
    });
  });

  it("removes conflicting assignments when asked, like the Keymap page", () => {
    const s = { keymap: "intellij-macos", keymapOverrides: {}, customKeymaps: {} };
    const draft = { preset: "intellij-macos", overrides: {} };
    const conflicts = findConflicts(draft, "Meta+S", "my.action", actions);
    expect(conflicts.map((c) => c.actionId)).toEqual(["file.save"]);
    const removed = assignedSetting(s, "my.action", "Meta+S", conflicts, true).value as Record<string, string[]>;
    expect(removed["my.action"]).toEqual(["Meta+S"]);
    expect(removed["file.save"]).toEqual([]);
    const kept = assignedSetting(s, "my.action", "Meta+S", conflicts, false).value as Record<string, string[]>;
    expect(kept["file.save"]).toBeUndefined();
  });

  it("writes into the selected user keymap", () => {
    const s = {
      keymap: "custom.mine",
      keymapOverrides: {},
      customKeymaps: { "custom.mine": { name: "Mine", parent: "intellij-macos", overrides: {} } },
    };
    const out = assignedSetting(s, "my.action", "Alt+Meta+J", [], false);
    expect(out.key).toBe("customKeymaps");
    expect(out.value).toEqual({
      "custom.mine": { name: "Mine", parent: "intellij-macos", overrides: { "my.action": ["Alt+Meta+J"] } },
    });
  });
});

import { describe, expect, it } from "vitest";
import { intellijMacos } from "./intellij-macos";
import { vscodeMacos } from "./vscode-macos";

describe("IDEA macOS bindings for navigation and editor extras", () => {
  it("⇧⌘T is Go to Test only; Reopen Closed Tab has no default (VS Code keeps it)", () => {
    const owners = Object.entries(intellijMacos)
      .filter(([, keys]) => keys.includes("Meta+Shift+T"))
      .map(([id]) => id);
    expect(owners).toEqual(["navigate.gotoTest"]);
    expect(intellijMacos["editor.reopenClosedTab"]).toEqual([]);
    expect(vscodeMacos["editor.reopenClosedTab"]).toEqual(["Meta+Shift+T"]);
  });

  it.each([
    ["navigate.lastEditLocation", "Meta+Shift+Backspace"],
    ["navigate.superMethod", "Meta+U"],
    ["navigate.nextMethod", "Ctrl+Shift+Down"],
    ["navigate.previousMethod", "Ctrl+Shift+Up"],
    ["navigate.navigationBar", "Meta+Up"],
    ["code.pasteFromHistory", "Meta+Shift+V"],
    ["code.unwrap", "Meta+Shift+Delete"],
    ["code.foldSelection", "Meta+."],
    ["editor.toggleColumnSelection", "Meta+Shift+8"],
  ])("%s → %s", (id, key) => {
    expect(intellijMacos[id]).toContain(key);
    expect(vscodeMacos[id]).toBeDefined();
  });
});

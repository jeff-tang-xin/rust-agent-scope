import { describe, expect, it } from "vitest";
import type { TitleItemContribution } from "./contributions";
import { defaultToolbar, editableToolbar, resolveToolbar, SEARCH_ENTRY, toggleWidget, VCS_WIDGET } from "./mainToolbar";

const item = (id: string, align: "left" | "right", priority: number): TitleItemContribution => ({
  id,
  align,
  priority,
  component: () => null,
});
const items = [item("run.widget", "right", 10), item("build.project", "right", 0), item(VCS_WIDGET, "left", 0)];

describe("main toolbar layout", () => {
  it("defaults to the project and VCS widgets, contributed widgets by priority, then Search Everywhere", () => {
    expect(defaultToolbar(items)).toEqual({
      left: ["project", VCS_WIDGET],
      right: ["build.project", "run.widget", SEARCH_ENTRY],
      hidden: [],
    });
    // Without a VCS widget the left side keeps its "Version Control" fallback.
    expect(defaultToolbar([]).left).toEqual(["project", VCS_WIDGET]);
  });

  it("keeps the saved order and appends widgets registered since", () => {
    const saved = { left: ["project"], right: [SEARCH_ENTRY, "run.widget"], hidden: [VCS_WIDGET] };
    expect(resolveToolbar(saved, items)).toEqual({
      left: ["project"],
      right: [SEARCH_ENTRY, "run.widget", "build.project"],
    });
  });

  it("hides and re-adds widgets; removed actions are dropped, not hidden", () => {
    const layout = editableToolbar(null, items);
    const hidden = toggleWidget(layout, "run.widget", "right");
    expect(hidden.right).toEqual(["build.project", SEARCH_ENTRY]);
    expect(hidden.hidden).toEqual(["run.widget"]);
    const shown = toggleWidget(hidden, "run.widget", "right");
    expect(shown.right).toEqual(["build.project", SEARCH_ENTRY, "run.widget"]);
    expect(shown.hidden).toEqual([]);
    expect(toggleWidget(layout, SEARCH_ENTRY, "right").hidden).toEqual([]);
    // A hidden widget stays hidden after reload.
    expect(resolveToolbar(hidden, items).right).not.toContain("run.widget");
  });
});

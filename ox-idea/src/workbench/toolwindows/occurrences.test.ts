import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { registerOccurrenceNavigator, registerToolWindow, useStatusInfo } from "../contributions";
import { goToOccurrence } from "./actions";
import { resetToolWindowLayout, setFocusedToolWindow, showToolWindow } from "./store";

const Empty = () => null;
let disposers: Array<() => void> = [];

beforeEach(() => {
  localStorage.clear();
  resetToolWindowLayout();
  disposers = [
    registerToolWindow({ id: "project", title: "Project", icon: "folder", anchor: "left", component: Empty }),
    registerToolWindow({ id: "find", title: "Find", icon: "search", anchor: "bottom", component: Empty }),
  ];
});

afterEach(() => {
  disposers.reverse().forEach((d) => d());
  useStatusInfo.setState({ text: null });
});

describe("Next / Previous Occurrence", () => {
  it("steps the focused tool window's results, keeping the focus there", () => {
    const go = vi.fn(() => ({ index: 2, total: 5 }));
    disposers.push(registerOccurrenceNavigator("find", { go }));
    showToolWindow("find");
    setFocusedToolWindow("find");
    expect(goToOccurrence(1)).toBe(true);
    expect(go).toHaveBeenCalledWith(1, false);
    expect(useStatusInfo.getState().text).toBe("2 of 5");
  });

  it("from elsewhere (the editor) uses the last active tool window and focuses the editor", () => {
    const go = vi.fn(() => ({ index: 1, total: 1 }));
    disposers.push(registerOccurrenceNavigator("find", { go }));
    showToolWindow("find");
    setFocusedToolWindow("project");
    setFocusedToolWindow(null);
    expect(goToOccurrence(-1)).toBe(true);
    expect(go).toHaveBeenCalledWith(-1, true);
  });

  it("does nothing without a navigator or past the last occurrence", () => {
    showToolWindow("project");
    expect(goToOccurrence(1)).toBe(false);
    const nav = { go: vi.fn(() => null) };
    const dispose = registerOccurrenceNavigator("project", nav);
    expect(goToOccurrence(1)).toBe(false);
    expect(nav.go).toHaveBeenCalled();
    expect(useStatusInfo.getState().text).toBeNull();
    dispose();
  });
});

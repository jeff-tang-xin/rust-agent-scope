import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { emitLocalIdeEvent } from "@/platform/ipc";
import { setFollowAgentEdits, useAgents } from "./store";
import { followAgentEdits } from "./followAgentEdits";

const openFile = vi.hoisted(() => vi.fn(() => Promise.resolve()));

vi.mock("@/workbench/contributions", () => ({
  editorService: () => ({ openFile }),
  toolWindowService: () => ({ show: vi.fn() }),
  notifications: () => ({ notify: vi.fn() }),
}));

const edit = (path: string, line: number | null) => emitLocalIdeEvent({ type: "agentEdit", path, line });

let stop: () => void;

beforeEach(() => {
  vi.useFakeTimers();
  localStorage.clear();
  openFile.mockClear();
  useAgents.setState({ followAgentEdits: true });
  stop = followAgentEdits();
});

afterEach(() => {
  stop();
  vi.useRealTimers();
});

describe("follow agent edits", () => {
  it("opens the changed file in a preview tab without stealing focus", () => {
    edit("/p/a.ts", 7);
    vi.advanceTimersByTime(150);
    expect(openFile).toHaveBeenCalledExactlyOnceWith("/p/a.ts", {
      preview: true,
      focus: false,
      selection: { line: 7, col: 0 },
    });
  });

  it("batches a burst of edits into one reveal of the last file", () => {
    edit("/p/a.ts", 1);
    edit("/p/b.ts", 2);
    vi.advanceTimersByTime(150);
    expect(openFile).toHaveBeenCalledExactlyOnceWith("/p/b.ts", {
      preview: true,
      focus: false,
      selection: { line: 2, col: 0 },
    });
  });

  it("opens without a selection when the changed line is unknown", () => {
    edit("/p/new.ts", null);
    vi.advanceTimersByTime(150);
    expect(openFile).toHaveBeenCalledExactlyOnceWith("/p/new.ts", { preview: true, focus: false });
  });

  it("does nothing while the toggle is off", () => {
    setFollowAgentEdits(false);
    edit("/p/a.ts", 7);
    vi.advanceTimersByTime(150);
    expect(openFile).not.toHaveBeenCalled();
  });

  it("persists the toggle", () => {
    setFollowAgentEdits(false);
    expect(JSON.parse(localStorage.getItem("oxidea.agents.followAgentEdits.v1")!)).toBe(false);
    expect(useAgents.getState().followAgentEdits).toBe(false);
    setFollowAgentEdits(true);
    expect(JSON.parse(localStorage.getItem("oxidea.agents.followAgentEdits.v1")!)).toBe(true);
  });
});

import { afterEach, describe, expect, it, vi } from "vitest";
import type { Location } from "@/bindings";

const fileUsages = vi.fn();
const askUsagesDetected = vi.fn();
const viewUsages = vi.fn();
vi.mock("@/editor/refactor", () => ({ fileUsages, askUsagesDetected, viewUsages }));
const notify = vi.fn();
vi.mock("@/workbench/contributions", async (orig) => ({
  ...(await orig<typeof import("@/workbench/contributions")>()),
  notifications: () => ({ notify }),
}));

const ask = vi.fn();
vi.mock("@/workbench/messages", async (orig) => ({
  ...(await orig<typeof import("@/workbench/messages")>()),
  ask,
}));

const { safeToDelete } = await import("./operations");

const usage: Location = { path: "/p/main.rs", range: { start: { line: 1, col: 0 }, end: { line: 1, col: 1 } } };

afterEach(() => vi.clearAllMocks());

describe("safe delete of files", () => {
  it("deletes without asking again when nothing uses the file or usages can't be searched", async () => {
    fileUsages.mockResolvedValueOnce({ status: "ok", usages: [] });
    expect(await safeToDelete(["/p/a.rs"])).toBe(true);
    fileUsages.mockResolvedValueOnce({ status: "unsupported", reason: "no server" });
    expect(await safeToDelete(["/p/a.md"])).toBe(true);
    expect(askUsagesDetected).not.toHaveBeenCalled();
  });

  it("asks Usages Detected and follows the answer", async () => {
    fileUsages.mockResolvedValue({ status: "ok", usages: [usage] });
    askUsagesDetected.mockResolvedValueOnce("delete");
    expect(await safeToDelete(["/p/a.rs"])).toBe(true);
    expect(askUsagesDetected).toHaveBeenCalledWith("File 'a.rs'", [usage], false);

    askUsagesDetected.mockResolvedValueOnce("view");
    expect(await safeToDelete(["/p/a.rs", "/p/b.rs"])).toBe(false);
    expect(askUsagesDetected).toHaveBeenLastCalledWith("The 2 files", [usage], true);
    expect(viewUsages).toHaveBeenCalledWith("2 files", [usage]);

    askUsagesDetected.mockResolvedValueOnce("cancel");
    expect(await safeToDelete(["/p/a.rs"])).toBe(false);
  });

  it("deletes nothing when the search fails", async () => {
    fileUsages.mockRejectedValueOnce(new Error("cancelled"));
    expect(await safeToDelete(["/p/a.rs"])).toBe(false);
    expect(notify).toHaveBeenCalledWith(expect.objectContaining({ level: "warning" }));
  });

  it("lets a slow search be skipped (delete anyway) or cancelled", async () => {
    vi.useFakeTimers();
    try {
      const pending = (_: string[], signal: AbortSignal) =>
        new Promise((_r, reject) => signal.addEventListener("abort", () => reject(new Error("aborted"))));
      fileUsages.mockImplementation(pending);
      ask.mockResolvedValueOnce({ button: "skip", checked: false });
      const skipped = safeToDelete(["/p/a.rs"]);
      await vi.advanceTimersByTimeAsync(500);
      expect(await skipped).toBe(true);
      expect(ask).toHaveBeenCalledWith(expect.objectContaining({ body: "Looking for usages…" }));

      ask.mockResolvedValueOnce({ button: "cancel", checked: false });
      const cancelled = safeToDelete(["/p/a.rs"]);
      await vi.advanceTimersByTimeAsync(500);
      expect(await cancelled).toBe(false);
      expect(notify).not.toHaveBeenCalled();
    } finally {
      fileUsages.mockReset();
      vi.useRealTimers();
    }
  });
});

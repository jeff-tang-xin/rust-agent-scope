import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";

const schedule = vi.fn();
const restart = vi.fn();

vi.mock("@/platform/ipc", async (importOriginal) => {
  const real = await importOriginal<typeof import("@/platform/ipc")>();
  return {
    ...real,
    isTauri: () => true,
    commands: {
      ...real.commands,
      cachesScheduleInvalidation: (o: unknown) => schedule(o),
      appRestart: () => restart(),
    },
  };
});

import { executeAction } from "@/platform/actions";
import { InvalidateCachesOverlay, registerInvalidateCachesActions } from "../InvalidateCaches";

beforeEach(() => {
  schedule.mockReset().mockResolvedValue({ status: "ok", data: null });
  restart.mockReset().mockResolvedValue(undefined);
});

describe("Invalidate Caches", () => {
  it("schedules the chosen caches and restarts", async () => {
    const dispose = registerInvalidateCachesActions();
    render(<InvalidateCachesOverlay />);
    await executeAction("file.invalidateCaches");
    const dialog = await screen.findByRole("dialog", { name: "Invalidate Caches" });
    const buttons = within(dialog)
      .getAllByRole("button")
      .map((b) => b.textContent);
    expect(buttons).toEqual(expect.arrayContaining(["Invalidate and Restart", "Just Restart", "Cancel"]));
    fireEvent.click(within(dialog).getByRole("checkbox", { name: "Clear file system cache and Local History" }));
    expect(dialog).toHaveTextContent("Local History of all projects will be deleted.");
    fireEvent.click(within(dialog).getByRole("checkbox", { name: /language server workspaces/ }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Invalidate and Restart" }));
    await waitFor(() => expect(restart).toHaveBeenCalled());
    expect(schedule).toHaveBeenCalledWith({ localHistory: true, languageServers: true, buildCaches: false });
    expect(screen.queryByRole("dialog", { name: "Invalidate Caches" })).toBeNull();
    dispose();
  });

  it("Just Restart cancels a pending invalidation; Cancel does nothing", async () => {
    const dispose = registerInvalidateCachesActions();
    render(<InvalidateCachesOverlay />);
    await executeAction("file.invalidateCaches");
    fireEvent.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Just Restart" }));
    await waitFor(() => expect(restart).toHaveBeenCalledTimes(1));
    expect(schedule).toHaveBeenCalledWith(null);
    await executeAction("file.invalidateCaches");
    fireEvent.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Cancel" }));
    expect(restart).toHaveBeenCalledTimes(1);
    dispose();
  });
});

import { afterEach, describe, expect, it, vi } from "vitest";

const ipc = vi.hoisted(() => ({
  runStart: vi.fn((_config: unknown, _channel: unknown) =>
    Promise.resolve({ status: "ok" as const, data: { id: 7, configId: "c", name: "c", pid: 1, running: true } }),
  ),
}));

vi.mock("@/platform/ipc", async (importOriginal) => {
  const real = await importOriginal<typeof import("@/platform/ipc")>();
  return { ...real, isTauri: () => true, commands: { ...real.commands, ...ipc } };
});
vi.mock("@tauri-apps/api/core", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@tauri-apps/api/core")>()),
  Channel: class {
    onmessage: ((m: unknown) => void) | null = null;
  },
}));

import type { RunConfig, RunId } from "@/platform/ipc";
import { registerRunExecutor, runExecutors } from "@/workbench/contributions";
import { runWithExecutor } from "./executors";
import { onRunExited, resetRuns, useRun } from "./store";

const config: RunConfig = {
  id: "cargo:test",
  name: "cargo test",
  kind: "cargo",
  command: "cargo",
  args: ["test"],
  cwd: null,
  env: {},
  detected: true,
  isTest: false,
  debugAdapter: null,
};

describe("run executors", () => {
  afterEach(() => resetRuns());

  it("launches the executor's command in the Run window and reports the exit", async () => {
    const finished = vi.fn();
    const launch = { ...config, args: ["llvm-cov", "test"] };
    const dispose = registerRunExecutor({
      id: "fake",
      label: (n) => `Fake '${n}'`,
      icon: "run",
      action: "fake.run",
      supports: () => true,
      prepare: async () => ({ launch, finished }),
    });
    expect(runExecutors().map((e) => e.id)).toContain("fake");
    await runWithExecutor(config, "fake");
    expect(ipc.runStart.mock.calls[0]![0]).toEqual(launch);
    const session = useRun.getState().sessions[0]!;
    expect(session.config).toEqual(config);
    onRunExited(7 as RunId, 0);
    await vi.waitFor(() => expect(finished).toHaveBeenCalledWith({ exitCode: 0, stopped: false }));
    dispose();
    expect(runExecutors().some((e) => e.id === "fake")).toBe(false);
  });

  it("does nothing when the executor declines", async () => {
    ipc.runStart.mockClear();
    const dispose = registerRunExecutor({
      id: "no",
      label: (n) => n,
      icon: "run",
      action: "no.run",
      supports: () => true,
      prepare: async () => null,
    });
    await runWithExecutor(config, "no");
    expect(ipc.runStart).not.toHaveBeenCalled();
    dispose();
  });
});
